//! Shared lazy-open LRU + idle-eviction container keyed on project root.
//!
//! [`ProjectResourceMap`] holds the eviction policy (default cap 10, 30 min idle)
//! in one place. Per-resource lifecycle — how to read `last_used`, touch on access,
//! and release on eviction — lives in [`ProjectMapResource`] implementations:
//! index handles via [`crate::server::index_map`], a project's whole writer
//! runtime via [`crate::server::project_runtime`].

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Lifecycle parameters for a [`ProjectResourceMap`]. The defaults (cap 10,
/// idle 30 min) are shared by the index and runtime maps (WEG-21).
#[derive(Debug, Clone)]
pub struct ProjectResourceMapConfig {
    pub capacity: usize,
    pub idle_timeout: Duration,
}

impl Default for ProjectResourceMapConfig {
    fn default() -> Self {
        Self {
            capacity: 10,
            idle_timeout: Duration::from_secs(30 * 60),
        }
    }
}

/// Per-entry lifecycle hooks consumed by [`ProjectResourceMap`].
pub trait ProjectMapResource: Send + 'static {
    /// Error from [`Self::release`]. Use [`std::convert::Infallible`] when
    /// release cannot fail.
    type ReleaseError;
    fn last_used(&self) -> Instant;
    fn release(self) -> Result<(), Self::ReleaseError>;
    /// Called on cache hit after the entry is moved MRU-to-back.
    fn touch(&mut self) {}
}

/// Lazy-opened, LRU + idle-evicting map of per-project resources.
///
/// It has no `Drop`: dropping the map drops each resource without calling
/// [`ProjectMapResource::release`]. Call [`Self::close_all`] first when release
/// matters. `ProjectIndexMap` wraps this map and does that in its own `Drop`.
pub struct ProjectResourceMap<R: ProjectMapResource> {
    config: ProjectResourceMapConfig,
    /// Insertion-order list of (root, resource). Most-recently-used is at the
    /// back. A `Vec` rather than an `LruCache`, because idle eviction walks
    /// every entry anyway and the cap is small (~10).
    entries: Vec<(PathBuf, R)>,
}

impl<R: ProjectMapResource> ProjectResourceMap<R> {
    pub fn new(config: ProjectResourceMapConfig) -> Self {
        Self {
            config,
            entries: Vec::new(),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(ProjectResourceMapConfig::default())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up the resource for `project_root`, or open it via `open` and insert.
    ///
    /// Idle entries other than `project_root` are released first (see
    /// `evict_idle_except`). Eviction order on a miss when full: those idle
    /// entries, then LRU (front of the entries list). LRU position is updated
    /// on every hit by moving the entry to the back and calling
    /// [`ProjectMapResource::touch`].
    pub fn get_or_insert<F, E>(&mut self, project_root: &Path, open: F) -> Result<&mut R, E>
    where
        F: FnOnce(&Path) -> Result<R, E>,
    {
        self.evict_idle_except(project_root);

        if let Some(idx) = self.position(project_root) {
            return Ok(self.promote(idx));
        }

        while self.entries.len() >= self.config.capacity {
            let (_, resource) = self.entries.remove(0);
            let _ = resource.release();
        }

        let resource = open(project_root)?;
        self.entries.push((project_root.to_path_buf(), resource));
        let last = self.entries.len() - 1;
        Ok(&mut self.entries[last].1)
    }

    /// Look up the resource for `project_root`, opening nothing.
    ///
    /// The hit path is [`Self::get_or_insert`]'s, to the same bookkeeping: other
    /// idle entries are evicted first, then a hit is moved MRU-to-back and
    /// [`ProjectMapResource::touch`]ed. A miss returns `None` and leaves the map
    /// untouched — the caller is expected to open the resource with the map's
    /// own lock **released** and publish it via [`Self::insert_or_adopt`]
    /// (AILAB-186). Splitting the two halves is the whole point:
    /// `get_or_insert` cannot express that, because it hands back a borrow of
    /// the map and so must run `open` while that borrow is live.
    pub fn get_mut(&mut self, project_root: &Path) -> Option<&mut R> {
        self.evict_idle_except(project_root);
        let idx = self.position(project_root)?;
        Some(self.promote(idx))
    }

    /// Publish an already-opened `resource` for `project_root`, or adopt the
    /// incumbent if another caller published one first. Returns the winner.
    ///
    /// **The incumbent always wins.** Once a resource is in the map it may
    /// already have handed pieces of itself to live requests — that is exactly
    /// what `AppState::with_index_handle`'s `f` does, and a project runtime's
    /// coordinator is wired to its index handle's indexer channel.
    /// Replacing it would strand every one of those. `resource` has been
    /// observed by nobody, so it is the safe one to discard — and it is
    /// [`released`](ProjectMapResource::release), never dropped on the floor:
    /// for index handles that means two live Tantivy `IndexWriter`s on one
    /// directory otherwise, which is the corruption the writer lock exists to
    /// prevent.
    ///
    /// The idle pass spares `project_root`, so an idle incumbent still wins.
    /// A miss is the ordinary path and evicts to capacity exactly as
    /// [`Self::get_or_insert`] does.
    pub fn insert_or_adopt(&mut self, project_root: &Path, resource: R) -> &mut R {
        self.evict_idle_except(project_root);

        if let Some(idx) = self.position(project_root) {
            let _ = resource.release();
            return self.promote(idx);
        }

        while self.entries.len() >= self.config.capacity {
            let (_, evicted) = self.entries.remove(0);
            let _ = evicted.release();
        }

        self.entries.push((project_root.to_path_buf(), resource));
        let last = self.entries.len() - 1;
        &mut self.entries[last].1
    }

    /// Move entry `idx` MRU-to-back, [`touch`](ProjectMapResource::touch) it,
    /// and hand out the borrow. One definition of "cache-hit bookkeeping", so
    /// [`Self::get_mut`] cannot drift from [`Self::get_or_insert`].
    fn promote(&mut self, idx: usize) -> &mut R {
        let entry = self.entries.remove(idx);
        self.entries.push(entry);
        let last = self.entries.len() - 1;
        self.entries[last].1.touch();
        &mut self.entries[last].1
    }

    /// Walk all entries and release any whose `last_used()` is older than
    /// `idle_timeout`. Returns the number of entries evicted.
    pub fn evict_idle(&mut self) -> usize {
        self.evict_idle_after(self.config.idle_timeout)
    }

    /// [`Self::evict_idle`] with an explicit timeout, so a test can run the
    /// pass as if every entry had sat idle past the configured one.
    pub(crate) fn evict_idle_after(&mut self, idle_timeout: Duration) -> usize {
        self.idle_pass(idle_timeout, None)
    }

    /// [`Self::evict_idle`], except that `keep` is never released.
    ///
    /// Every lookup passes the root it was asked for: that entry is about to
    /// be used, so it is not idle. Releasing it would make the caller open it
    /// again at once. A [`ProjectRuntime`](crate::server::ProjectRuntime)'s
    /// tasks still hold the project writer lock when its `release` returns, so
    /// that reopen was refused, naming the daemon's own pid (DRM-49).
    fn evict_idle_except(&mut self, keep: &Path) -> usize {
        self.idle_pass(self.config.idle_timeout, Some(keep))
    }

    /// Release every entry other than `keep` whose `last_used()` is older
    /// than `idle_timeout`. Returns the number of entries released.
    fn idle_pass(&mut self, idle_timeout: Duration, keep: Option<&Path>) -> usize {
        let cutoff = Instant::now().checked_sub(idle_timeout);
        let Some(cutoff) = cutoff else {
            return 0;
        };
        let mut evicted = 0;
        let mut i = 0;
        while i < self.entries.len() {
            let (root, resource) = &self.entries[i];
            if keep != Some(root.as_path()) && resource.last_used() < cutoff {
                let (_, resource) = self.entries.remove(i);
                let _ = resource.release();
                evicted += 1;
            } else {
                i += 1;
            }
        }
        evicted
    }

    /// Release every open resource. Individual release errors are swallowed.
    pub fn close_all(&mut self) {
        for (_, resource) in self.entries.drain(..) {
            let _ = resource.release();
        }
    }

    fn position(&self, root: &Path) -> Option<usize> {
        self.entries.iter().position(|(p, _)| p == root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A resource that records its name when it is released.
    struct Probe {
        name: &'static str,
        last_used: Instant,
        released: Arc<Mutex<Vec<&'static str>>>,
    }

    impl ProjectMapResource for Probe {
        type ReleaseError = std::convert::Infallible;

        fn last_used(&self) -> Instant {
            self.last_used
        }

        fn release(self) -> Result<(), Self::ReleaseError> {
            self.released.lock().unwrap().push(self.name);
            Ok(())
        }
    }

    type Released = Arc<Mutex<Vec<&'static str>>>;

    /// A probe named `name`. An `idle` one was last used 60 s ago.
    fn probe(name: &'static str, idle: bool, released: &Released) -> Probe {
        let now = Instant::now();
        let last_used = if idle {
            now.checked_sub(Duration::from_secs(60))
                .expect("the monotonic clock is past 60 s")
        } else {
            now
        };
        Probe {
            name,
            last_used,
            released: Arc::clone(released),
        }
    }

    /// A map (capacity 10, 10 s idle timeout) holding `/p/a` and `/p/b`, both
    /// idle. The entries are pushed directly: an entry point would run an idle
    /// pass and release the first entry while the second is added.
    fn two_idle_entries() -> (ProjectResourceMap<Probe>, Released) {
        let released: Released = Arc::new(Mutex::new(Vec::new()));
        let mut map = ProjectResourceMap::new(ProjectResourceMapConfig {
            capacity: 10,
            idle_timeout: Duration::from_secs(10),
        });
        map.entries
            .push((PathBuf::from("/p/a"), probe("a", true, &released)));
        map.entries
            .push((PathBuf::from("/p/b"), probe("b", true, &released)));
        (map, released)
    }

    /// DRM-49: a lookup's idle pass spares the root it was asked for.
    #[test]
    fn get_mut_keeps_the_idle_root_it_was_asked_for() {
        let (mut map, released) = two_idle_entries();

        let hit = map.get_mut(Path::new("/p/a")).map(|r| r.name);

        assert_eq!(hit, Some("a"));
        assert_eq!(*released.lock().unwrap(), vec!["b"]);
        assert_eq!(map.len(), 1);
    }

    /// DRM-49: `get_or_insert` hands back the idle entry and opens nothing.
    #[test]
    fn get_or_insert_reuses_the_idle_root_it_was_asked_for() {
        let (mut map, released) = two_idle_entries();

        let got = map
            .get_or_insert(Path::new("/p/a"), |_| {
                Ok::<_, ()>(probe("a-reopened", false, &released))
            })
            .map(|r| r.name);

        assert_eq!(got, Ok("a"));
        assert_eq!(*released.lock().unwrap(), vec!["b"]);
        assert_eq!(map.len(), 1);
    }

    /// DRM-49: an idle incumbent still wins over a newly opened resource.
    #[test]
    fn insert_or_adopt_keeps_an_idle_incumbent() {
        let (mut map, released) = two_idle_entries();

        let winner = map
            .insert_or_adopt(Path::new("/p/a"), probe("a-new", false, &released))
            .name;

        assert_eq!(winner, "a");
        assert_eq!(*released.lock().unwrap(), vec!["b", "a-new"]);
        assert_eq!(map.len(), 1);
    }
}
