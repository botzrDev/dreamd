//! Shared lazy-open LRU + idle-eviction container keyed on project root.
//!
//! [`ProjectResourceMap`] holds the eviction policy (default cap 10, 30 min idle)
//! in one place. Per-resource lifecycle — how to read `last_used`, touch on access,
//! and release on eviction — lives in [`ProjectMapResource`] implementations:
//! index handles via [`crate::server::index_map`], coordinators via
//! [`crate::server::supervisor_map`].

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Lifecycle parameters for a [`ProjectResourceMap`]. Defaults match the WEG-21
/// founder-decision values (cap 10, idle 30 min) shared by index and supervisor maps.
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
    /// Error from [`Self::release`]. Use [`Infallible`] when release cannot fail.
    type ReleaseError;
    fn last_used(&self) -> Instant;
    fn release(self) -> Result<(), Self::ReleaseError>;
    /// Called on cache hit after the entry is moved MRU-to-back.
    fn touch(&mut self) {}
}

/// Lazy-opened, LRU + idle-evicting map of per-project resources.
pub struct ProjectResourceMap<R: ProjectMapResource> {
    config: ProjectResourceMapConfig,
    /// Insertion-order list of (root, resource). Most-recently-used is at the
    /// back; we keep it in a `Vec` rather than `LruCache` because we need to
    /// walk for idle eviction anyway and the cap is small (~10).
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
    /// Eviction order on a miss when full: idle entries first (via
    /// [`Self::evict_idle`]), then LRU (front of the entries list). LRU position
    /// is updated on every hit by moving the entry to the back and calling
    /// [`ProjectMapResource::touch`].
    pub fn get_or_insert<F, E>(&mut self, project_root: &Path, open: F) -> Result<&mut R, E>
    where
        F: FnOnce(&Path) -> Result<R, E>,
    {
        self.evict_idle();

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
    /// The hit path is [`Self::get_or_insert`]'s, to the same bookkeeping: idle
    /// entries are evicted first, then a hit is moved MRU-to-back and
    /// [`ProjectMapResource::touch`]ed. A miss returns `None` and leaves the map
    /// untouched — the caller is expected to open the resource with the map's
    /// own lock **released** and publish it via [`Self::insert_or_adopt`]
    /// (AILAB-186). Splitting the two halves is the whole point:
    /// `get_or_insert` cannot express that, because it hands back a borrow of
    /// the map and so must run `open` while that borrow is live.
    pub fn get_mut(&mut self, project_root: &Path) -> Option<&mut R> {
        self.evict_idle();
        let idx = self.position(project_root)?;
        Some(self.promote(idx))
    }

    /// Publish an already-opened `resource` for `project_root`, or adopt the
    /// incumbent if another caller published one first. Returns the winner.
    ///
    /// **The incumbent always wins.** Once a resource is in the map it may
    /// already have handed pieces of itself to live requests — that is exactly
    /// what `AppState::with_index_handle`'s `f` does, and `supervisor_map`
    /// caches coordinators wired to the index handle's indexer channel.
    /// Replacing it would strand every one of those. `resource` has been
    /// observed by nobody, so it is the safe one to discard — and it is
    /// [`released`](ProjectMapResource::release), never dropped on the floor:
    /// for index handles that means two live Tantivy `IndexWriter`s on one
    /// directory otherwise, which is the corruption the writer lock exists to
    /// prevent.
    ///
    /// A miss is the ordinary path and evicts to capacity exactly as
    /// [`Self::get_or_insert`] does.
    pub fn insert_or_adopt(&mut self, project_root: &Path, resource: R) -> &mut R {
        self.evict_idle();

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
        let cutoff = Instant::now().checked_sub(self.config.idle_timeout);
        let Some(cutoff) = cutoff else {
            return 0;
        };
        let mut evicted = 0;
        let mut i = 0;
        while i < self.entries.len() {
            if self.entries[i].1.last_used() < cutoff {
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
