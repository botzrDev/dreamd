//! `IndexHandle` trait + `ProjectIndexMap<H>` (WEG-21 skeleton).
//!
//! `ProjectIndexMap` is keyed on resolved project root path. It opens a
//! per-project handle lazily on first request, evicts least-recently-used
//! entries at a configurable capacity (default 10), and evicts idle entries
//! whose `last_used()` exceeds a configurable timeout (default 30 min).
//!
//! The trait surface is intentionally minimal — exactly the two methods the
//! AC specifies. Production open/commit/close lives in [`crate::server::tantivy_handle`]
//! (WEG-42). This module keeps the map + [`TestIndexHandle`] so eviction and
//! shutdown-drain tests can run without a live Tantivy index.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::server::project_resource_map::{
    ProjectMapResource, ProjectResourceMap, ProjectResourceMapConfig,
};

/// Error surfaced when an index handle fails to open, commit, or close
/// cleanly. Variants separate the failures a caller can sensibly retry
/// (see [`IndexError::is_retryable`]) from terminal ones, and give the
/// schema-incompatibility wipe gate in [`crate::server::tantivy_handle`] a
/// typed source to match on instead of the whole rendered string.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// The indexer task's mpsc receiver is gone — the send never landed.
    #[error("index error: indexer channel closed")]
    ChannelClosed,
    /// The indexer task accepted the message then dropped the oneshot
    /// sender without replying.
    #[error("index error: indexer task dropped response sender")]
    TaskDropped,
    /// A `std::sync::Mutex` guarding a per-project map was poisoned.
    #[error("index error: index map lock poisoned")]
    LockPoisoned,
    /// Write-ahead-log recovery failed while resolving a project.
    #[error("index error: wal recovery: {0}")]
    Wal(String),
    /// A per-project coordinator supervisor failed to start.
    #[error("index error: supervisor start failed: {0}")]
    Supervisor(String),
    /// Filesystem failure.
    #[error("index error: io: {0}")]
    Io(String),
    /// A `tantivy` operation failed. The payload is tantivy's own rendering;
    /// `is_schema_incompatible` matches `"schema error:"` inside it.
    #[error("index error: tantivy: {0}")]
    Tantivy(String),
    /// Opening the tantivy `MmapDirectory` failed. Distinct from
    /// [`IndexError::Tantivy`] because its payload embeds the directory
    /// *path*, which must never be searched for schema keywords.
    #[error("index error: tantivy directory: {0}")]
    TantivyDirectory(String),
    /// Everything else — sidecar/progress/manifest parse and write failures,
    /// task joins, runtime construction.
    #[error("index error: {0}")]
    Other(String),
}

impl IndexError {
    /// Is this failure worth retrying? Only the two channel-shaped failures
    /// are: everything else is a terminal disk, schema, or parse fault that
    /// a retry would reproduce.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::ChannelClosed | Self::TaskDropped)
    }
}

/// Per-project search-index handle managed by [`ProjectIndexMap`].
///
/// The trait surface is exactly two methods, per the WEG-21 AC. Adding
/// methods here is a load-bearing decision — bring it back to PM before
/// extending.
pub trait IndexHandle: Send + 'static {
    /// Release the handle and any underlying resources. Called by
    /// [`ProjectIndexMap`] on eviction or shutdown. Failures bubble up so the
    /// caller can log them; eviction proceeds regardless.
    fn close(self) -> Result<(), IndexError>;

    /// Wall-clock instant of the handle's last touch. Used by the idle-evictor
    /// — handles whose `last_used()` exceeds the map's `idle_timeout` are
    /// closed even if the LRU is under capacity.
    fn last_used(&self) -> Instant;
}

impl<H: IndexHandle> ProjectMapResource for H {
    type ReleaseError = IndexError;

    fn last_used(&self) -> Instant {
        IndexHandle::last_used(self)
    }

    fn release(self) -> Result<(), IndexError> {
        IndexHandle::close(self)
    }
}

/// Lifecycle parameters for a [`ProjectIndexMap`]. Defaults match the WEG-21
/// founder-decision values (cap 10, idle 30 min).
pub type ProjectIndexMapConfig = ProjectResourceMapConfig;

/// Lazy-opened, LRU + idle-evicting map of per-project index handles.
///
/// Generic on `H: IndexHandle` so tests can drive the same logic against a
/// recording handle (`TestIndexHandle`) without pulling in `tantivy`.
pub struct ProjectIndexMap<H: IndexHandle> {
    inner: ProjectResourceMap<H>,
}

impl<H: IndexHandle> ProjectIndexMap<H> {
    pub fn new(config: ProjectIndexMapConfig) -> Self {
        Self {
            inner: ProjectResourceMap::new(config),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(ProjectIndexMapConfig::default())
    }

    /// Number of currently-open handles.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Look up the handle for `project_root`, or open it via `open` and insert.
    /// Returns a `&mut H` so the caller can mark the handle's `last_used`.
    ///
    /// Eviction order on a miss when full: idle entries first (oldest
    /// `last_used`), then LRU (front of the entries list). LRU position is
    /// updated on every `get_or_open` hit by moving the entry to the back.
    pub fn get_or_open<F>(&mut self, project_root: &Path, open: F) -> Result<&mut H, IndexError>
    where
        F: FnOnce(&Path) -> Result<H, IndexError>,
    {
        self.inner.get_or_insert(project_root, open)
    }

    /// Look up the live handle for `project_root` without opening one.
    ///
    /// A hit is marked most-recently-used, exactly as [`Self::get_or_open`]
    /// does. A miss returns `None` — and the caller must then open the handle
    /// with the surrounding `index_map` mutex **released** and publish it via
    /// [`Self::insert_or_adopt`] (AILAB-186). `TantivyIndexHandle::open` replays
    /// the whole JSONL, commits, and allocates a 50 MB `IndexWriter`; running
    /// that under the map lock stalled every other project's requests.
    ///
    /// [`TantivyIndexHandle::open`]: crate::server::TantivyIndexHandle::open
    pub fn get_handle(&mut self, project_root: &Path) -> Option<&mut H> {
        self.inner.get_mut(project_root)
    }

    /// Publish an already-opened `handle` for `project_root`, or adopt the
    /// incumbent if another thread opened the same root first. Returns whichever
    /// handle is now in the map.
    ///
    /// The incumbent wins and the loser is **closed**, not leaked and not
    /// silently dropped: two live handles on one index directory means two
    /// Tantivy `IndexWriter`s. See
    /// [`ProjectResourceMap::insert_or_adopt`] for why the incumbent is the one
    /// that has to survive.
    pub fn insert_or_adopt(&mut self, project_root: &Path, handle: H) -> &mut H {
        self.inner.insert_or_adopt(project_root, handle)
    }

    /// Walk all entries and close any whose `last_used()` is older than
    /// `idle_timeout`. Returns the number of entries evicted.
    pub fn evict_idle(&mut self) -> usize {
        self.inner.evict_idle()
    }

    /// Close every open handle. Errors from individual closes are swallowed
    /// — the supervisor uses this on shutdown and there is no recovery.
    pub fn close_all(&mut self) {
        self.inner.close_all();
    }
}

impl<H: IndexHandle> Drop for ProjectIndexMap<H> {
    fn drop(&mut self) {
        self.close_all();
    }
}

/// Recording `IndexHandle` used in tests. Each instance carries a shared
/// `EventLog` reference so a test can assert open/close ordering across the
/// whole map.
pub struct TestIndexHandle {
    project_root: PathBuf,
    last_used: Instant,
    log: Arc<TestEventLog>,
}

impl TestIndexHandle {
    pub fn open(project_root: &Path, log: Arc<TestEventLog>) -> Self {
        log.record(TestEvent::Open(project_root.to_path_buf()));
        Self {
            project_root: project_root.to_path_buf(),
            last_used: Instant::now(),
            log,
        }
    }

    /// Override `last_used` for tests that need to simulate an aged handle.
    /// Production handles never need this — they update `last_used` on every
    /// touch through real index operations.
    pub fn set_last_used(&mut self, when: Instant) {
        self.last_used = when;
    }
}

impl IndexHandle for TestIndexHandle {
    fn close(self) -> Result<(), IndexError> {
        self.log.record(TestEvent::Close(self.project_root.clone()));
        Ok(())
    }

    fn last_used(&self) -> Instant {
        self.last_used
    }
}

/// Open/close event recorder shared across `TestIndexHandle` instances.
#[derive(Default)]
pub struct TestEventLog {
    events: Mutex<Vec<TestEvent>>,
    open_count: AtomicUsize,
    close_count: AtomicUsize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestEvent {
    Open(PathBuf),
    Close(PathBuf),
}

impl TestEventLog {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn record(&self, ev: TestEvent) {
        match &ev {
            TestEvent::Open(_) => {
                self.open_count.fetch_add(1, Ordering::SeqCst);
            }
            TestEvent::Close(_) => {
                self.close_count.fetch_add(1, Ordering::SeqCst);
            }
        }
        self.events.lock().expect("test log mutex").push(ev);
    }

    pub fn opens(&self) -> usize {
        self.open_count.load(Ordering::SeqCst)
    }

    pub fn closes(&self) -> usize {
        self.close_count.load(Ordering::SeqCst)
    }

    pub fn events(&self) -> Vec<TestEvent> {
        self.events.lock().expect("test log mutex").clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn opener(log: Arc<TestEventLog>) -> impl Fn(&Path) -> Result<TestIndexHandle, IndexError> {
        move |p: &Path| Ok(TestIndexHandle::open(p, log.clone()))
    }

    #[test]
    fn lazy_open_records_one_open_per_unique_root() {
        let log = TestEventLog::new();
        let mut map: ProjectIndexMap<TestIndexHandle> = ProjectIndexMap::with_defaults();
        let open = opener(log.clone());

        let _ = map.get_or_open(Path::new("/p/a"), &open).unwrap();
        let _ = map.get_or_open(Path::new("/p/a"), &open).unwrap();
        let _ = map.get_or_open(Path::new("/p/b"), &open).unwrap();

        assert_eq!(log.opens(), 2);
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn lru_evicts_least_recently_used_at_capacity() {
        let log = TestEventLog::new();
        let mut map = ProjectIndexMap::new(ProjectIndexMapConfig {
            capacity: 3,
            idle_timeout: Duration::from_secs(3600),
        });
        let open = opener(log.clone());

        // Fill capacity in order a, b, c.
        for r in ["/p/a", "/p/b", "/p/c"] {
            let _ = map.get_or_open(Path::new(r), &open).unwrap();
        }
        // Touch a to mark it most-recently-used. Now LRU order is b, c, a.
        let _ = map.get_or_open(Path::new("/p/a"), &open).unwrap();
        // Insert d. LRU (b) must be evicted.
        let _ = map.get_or_open(Path::new("/p/d"), &open).unwrap();

        assert_eq!(map.len(), 3);
        let closes: Vec<_> = log
            .events()
            .into_iter()
            .filter_map(|e| match e {
                TestEvent::Close(p) => Some(p),
                _ => None,
            })
            .collect();
        assert_eq!(
            closes,
            vec![PathBuf::from("/p/b")],
            "LRU eviction must close exactly the LRU entry"
        );
    }

    #[test]
    fn idle_eviction_closes_handles_older_than_timeout() {
        let log = TestEventLog::new();
        let mut map = ProjectIndexMap::new(ProjectIndexMapConfig {
            capacity: 5,
            idle_timeout: Duration::from_millis(50),
        });
        let open = opener(log.clone());

        let h_a = map.get_or_open(Path::new("/p/a"), &open).unwrap();
        // Backdate /p/a's last_used past the idle threshold.
        h_a.set_last_used(Instant::now() - Duration::from_secs(60));

        // get_or_open on a different root triggers idle eviction first.
        let _ = map.get_or_open(Path::new("/p/b"), &open).unwrap();

        assert_eq!(map.len(), 1);
        assert_eq!(log.closes(), 1);
        let events = log.events();
        assert!(
            events.contains(&TestEvent::Close(PathBuf::from("/p/a"))),
            "expected /p/a to be idle-evicted"
        );
    }

    #[test]
    fn close_all_closes_every_open_handle() {
        let log = TestEventLog::new();
        let mut map: ProjectIndexMap<TestIndexHandle> = ProjectIndexMap::with_defaults();
        let open = opener(log.clone());
        for r in ["/p/a", "/p/b", "/p/c"] {
            let _ = map.get_or_open(Path::new(r), &open).unwrap();
        }
        map.close_all();
        assert_eq!(map.len(), 0);
        assert_eq!(log.opens(), 3);
        assert_eq!(log.closes(), 3);
    }

    /// AILAB-186's decisive test: the open must be able to observe that the map
    /// mutex is free while it runs.
    ///
    /// The negative control in the second half is what makes the first half mean
    /// anything — it drives the *old* `get_or_open` shape through the identical
    /// probe and shows it sees the mutex held. (`try_lock`, not `lock`: a std
    /// `Mutex` is not reentrant, so `lock` here would deadlock rather than
    /// report.)
    #[test]
    fn open_runs_without_map_lock_held() {
        let log = TestEventLog::new();
        let map: Arc<Mutex<ProjectIndexMap<TestIndexHandle>>> =
            Arc::new(Mutex::new(ProjectIndexMap::with_defaults()));

        // --- new shape: lookup, release, open, re-lock, publish.
        let root = Path::new("/p/a");
        {
            let mut guard = map.lock().expect("map mutex");
            assert!(guard.get_handle(root).is_none(), "cold map must miss");
        }
        // This closure stands in for `TantivyIndexHandle::open` at exactly the
        // point `with_index_handle` calls it.
        let open_probe = |p: &Path| {
            let map_free = map.try_lock().is_ok();
            (TestIndexHandle::open(p, log.clone()), map_free)
        };
        let (handle, free_during_open) = open_probe(root);
        {
            let mut guard = map.lock().expect("map mutex");
            guard.insert_or_adopt(root, handle);
        }

        assert!(
            free_during_open,
            "the map mutex must be free while the handle is being opened"
        );

        // --- negative control: the same probe under today's `get_or_open`.
        let mut held_under_get_or_open = None;
        {
            let mut guard = map.lock().expect("map mutex");
            let _ = guard.get_or_open(Path::new("/p/b"), |p| {
                held_under_get_or_open = Some(map.try_lock().is_ok());
                Ok(TestIndexHandle::open(p, log.clone()))
            });
        }
        assert_eq!(
            held_under_get_or_open,
            Some(false),
            "control: `get_or_open` runs its opener inside the guard — if this \
             ever reports the map free, the assertion above proves nothing"
        );

        assert_eq!(map.lock().expect("map mutex").len(), 2);
        assert_eq!(log.opens(), 2);
        assert_eq!(log.closes(), 0);
    }

    /// Two threads first-touch the same root. Both open (that is the race the
    /// released lock permits); exactly one handle may live in the map, and the
    /// loser must be **closed**, not leaked — two live handles on one index dir
    /// means two Tantivy `IndexWriter`s.
    #[test]
    fn concurrent_first_touch_keeps_one_handle() {
        let log = TestEventLog::new();
        let map: Arc<Mutex<ProjectIndexMap<TestIndexHandle>>> =
            Arc::new(Mutex::new(ProjectIndexMap::with_defaults()));
        let root = PathBuf::from("/p/a");
        // Both threads complete their lookup before either publishes, so both
        // are guaranteed to miss. No thread short-circuits before the barrier,
        // so it cannot deadlock.
        let barrier = Arc::new(std::sync::Barrier::new(2));

        let threads: Vec<_> = (0..2)
            .map(|_| {
                let map = map.clone();
                let log = log.clone();
                let root = root.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let hit = {
                        let mut guard = map.lock().expect("map mutex");
                        guard.get_handle(&root).is_some()
                    };
                    barrier.wait();
                    let handle = TestIndexHandle::open(&root, log);
                    let mut guard = map.lock().expect("map mutex");
                    guard.insert_or_adopt(&root, handle);
                    hit
                })
            })
            .collect();

        for t in threads {
            assert!(!t.join().expect("thread panicked"), "both must miss");
        }

        assert_eq!(log.opens(), 2, "the barrier forces the both-open race");
        assert_eq!(log.closes(), 1, "the losing handle must be closed");
        assert_eq!(
            map.lock().expect("map mutex").len(),
            1,
            "exactly one handle survives in the map"
        );
        assert_eq!(
            log.events().last(),
            Some(&TestEvent::Close(root)),
            "and the close is of that root, not of some evicted neighbour"
        );
    }

    /// AC #1: a slow open of project A must not block project B. Thread A parks
    /// mid-"open" on a channel with no lock held; B has to get all the way
    /// through its own lookup-open-publish before A is released.
    #[test]
    fn second_project_not_blocked_by_slow_open() {
        let log = TestEventLog::new();
        let map: Arc<Mutex<ProjectIndexMap<TestIndexHandle>>> =
            Arc::new(Mutex::new(ProjectIndexMap::with_defaults()));
        let (release_a_tx, release_a_rx) = std::sync::mpsc::channel::<()>();
        let (a_is_opening_tx, a_is_opening_rx) = std::sync::mpsc::channel::<()>();

        let a = {
            let map = map.clone();
            let log = log.clone();
            std::thread::spawn(move || {
                let root = Path::new("/p/a");
                {
                    let mut guard = map.lock().expect("map mutex");
                    assert!(guard.get_handle(root).is_none());
                }
                a_is_opening_tx.send(()).expect("test receiver alive");
                // The slow open. Lock released above, not re-taken until after.
                release_a_rx.recv().expect("test sender alive");
                let handle = TestIndexHandle::open(root, log);
                let mut guard = map.lock().expect("map mutex");
                guard.insert_or_adopt(root, handle);
            })
        };

        a_is_opening_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("thread A never reached its open");

        // A is parked inside its open right now. B must not be able to tell.
        let (b_done_tx, b_done_rx) = std::sync::mpsc::channel::<()>();
        let b = {
            let map = map.clone();
            let log = log.clone();
            std::thread::spawn(move || {
                let root = Path::new("/p/b");
                {
                    let mut guard = map.lock().expect("map mutex");
                    assert!(guard.get_handle(root).is_none());
                }
                let handle = TestIndexHandle::open(root, log);
                let mut guard = map.lock().expect("map mutex");
                guard.insert_or_adopt(root, handle);
                let _ = b_done_tx.send(());
            })
        };
        // Timed, not a bare `join`: under the old shape B blocks on the map
        // mutex for as long as A is parked, and this must fail rather than hang.
        b_done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("project B blocked behind project A's open");
        b.join().expect("thread B panicked");

        release_a_tx.send(()).expect("thread A alive");
        a.join().expect("thread A panicked");

        assert_eq!(map.lock().expect("map mutex").len(), 2);
        assert_eq!(log.opens(), 2);
        assert_eq!(log.closes(), 0, "distinct roots never race");
    }

    #[test]
    fn drop_closes_open_handles() {
        let log = TestEventLog::new();
        {
            let mut map: ProjectIndexMap<TestIndexHandle> = ProjectIndexMap::with_defaults();
            let open = opener(log.clone());
            let _ = map.get_or_open(Path::new("/p/a"), &open).unwrap();
            let _ = map.get_or_open(Path::new("/p/b"), &open).unwrap();
        }
        assert_eq!(log.closes(), 2, "Drop must close every open handle");
    }

    /// `is_retryable` is the whole point of splitting the enum: only the two
    /// channel-shaped failures are worth a second attempt. A disk, tantivy, or
    /// parse fault would reproduce, so retrying it is a hot loop, not a heal.
    #[test]
    fn is_retryable_covers_channel_failures_only() {
        assert!(
            IndexError::ChannelClosed.is_retryable(),
            "a closed indexer channel is retryable"
        );
        assert!(
            IndexError::TaskDropped.is_retryable(),
            "a dropped response sender is retryable"
        );

        for terminal in [
            IndexError::Tantivy("Schema error: 'x' not found".to_string()),
            IndexError::Io("permission denied".to_string()),
            IndexError::Other("parse index_progress.json: eof".to_string()),
        ] {
            assert!(
                !terminal.is_retryable(),
                "terminal failure must not be retryable: {terminal:?}"
            );
        }
    }
}
