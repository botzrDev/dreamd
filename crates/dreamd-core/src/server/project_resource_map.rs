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
            let entry = self.entries.remove(idx);
            self.entries.push(entry);
            let last = self.entries.len() - 1;
            self.entries[last].1.touch();
            return Ok(&mut self.entries[last].1);
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
