//! `IndexHandle` trait + `ProjectIndexMap<H>` (WEG-21 skeleton).
//!
//! `ProjectIndexMap` is keyed on resolved project root path. It opens a
//! per-project handle lazily on first request, evicts least-recently-used
//! entries at a configurable capacity (default 10), and evicts idle entries
//! whose `last_used()` exceeds a configurable timeout (default 30 min).
//!
//! The trait surface is intentionally minimal — exactly the two methods the
//! AC specifies. Real `IndexWriter` open/commit/close logic lands in WEG-42
//! via `TantivyIndexHandle`. We ship [`TestIndexHandle`] so the eviction and
//! shutdown-drain tests can run without any `tantivy` dep in this ticket.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Error surfaced when an index handle fails to close cleanly. Concrete
/// implementations decide what's "fatal" vs "logged" — the skeleton holds
/// only the string form.
#[derive(Debug, thiserror::Error)]
#[error("index error: {0}")]
pub struct IndexError(pub String);

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

/// Lifecycle parameters for a [`ProjectIndexMap`]. Defaults match the WEG-21
/// founder-decision values (cap 10, idle 30 min).
#[derive(Debug, Clone)]
pub struct ProjectIndexMapConfig {
    pub capacity: usize,
    pub idle_timeout: Duration,
}

impl Default for ProjectIndexMapConfig {
    fn default() -> Self {
        Self {
            capacity: 10,
            idle_timeout: Duration::from_secs(30 * 60),
        }
    }
}

/// Lazy-opened, LRU + idle-evicting map of per-project index handles.
///
/// Generic on `H: IndexHandle` so tests can drive the same logic against a
/// recording handle (`TestIndexHandle`) without pulling in `tantivy`.
pub struct ProjectIndexMap<H: IndexHandle> {
    config: ProjectIndexMapConfig,
    /// Insertion-order list of (root, handle). Most-recently-used is at the
    /// back; we keep it in a `Vec` rather than `LruCache` because we need to
    /// walk for idle eviction anyway and the cap is small (~10).
    entries: Vec<(PathBuf, H)>,
}

impl<H: IndexHandle> ProjectIndexMap<H> {
    pub fn new(config: ProjectIndexMapConfig) -> Self {
        Self {
            config,
            entries: Vec::new(),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(ProjectIndexMapConfig::default())
    }

    /// Number of currently-open handles.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
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
        // First, evict any idle entries up front so a slow caller doesn't
        // keep a stale handle alive just by having been opened recently.
        self.evict_idle();

        if let Some(idx) = self.position(project_root) {
            // LRU touch: move to back.
            let entry = self.entries.remove(idx);
            self.entries.push(entry);
            let last = self.entries.len() - 1;
            return Ok(&mut self.entries[last].1);
        }

        // Miss path: ensure capacity, then open.
        while self.entries.len() >= self.config.capacity {
            // Evict LRU (front).
            let (_, handle) = self.entries.remove(0);
            let _ = handle.close();
        }

        let handle = open(project_root)?;
        self.entries.push((project_root.to_path_buf(), handle));
        let last = self.entries.len() - 1;
        Ok(&mut self.entries[last].1)
    }

    /// Walk all entries and close any whose `last_used()` is older than
    /// `idle_timeout`. Returns the number of entries evicted.
    pub fn evict_idle(&mut self) -> usize {
        let cutoff = Instant::now().checked_sub(self.config.idle_timeout);
        let Some(cutoff) = cutoff else {
            return 0;
        };
        let mut evicted = 0;
        let mut i = 0;
        while i < self.entries.len() {
            if self.entries[i].1.last_used() < cutoff {
                let (_, handle) = self.entries.remove(i);
                let _ = handle.close();
                evicted += 1;
            } else {
                i += 1;
            }
        }
        evicted
    }

    /// Close every open handle. Errors from individual closes are swallowed
    /// — the supervisor uses this on shutdown and there is no recovery.
    pub fn close_all(&mut self) {
        for (_, handle) in self.entries.drain(..) {
            let _ = handle.close();
        }
    }

    fn position(&self, root: &Path) -> Option<usize> {
        self.entries.iter().position(|(p, _)| p == root)
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
}
