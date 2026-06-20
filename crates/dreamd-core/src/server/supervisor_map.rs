//! Lazy-started, LRU + idle-evicting map of per-project coordinators.
//!
//! Mirrors [`crate::server::ProjectIndexMap`] but for [`Supervisor`]. Two
//! deliberate differences from the index map:
//!   * `last_used` is tracked by the map itself — a [`Supervisor`] carries none.
//!   * eviction reaps a coordinator by **dropping** its `Arc<Supervisor>`; we
//!     never call the async `Supervisor::shutdown` from this map (it consumes
//!     `self` and would have to be awaited under the std `Mutex`, which is
//!     forbidden). Dropping the last `Arc` drops the coordinator's sender; the
//!     actor's `rx.recv()` then returns `None` AFTER draining every buffered
//!     message, `run()` returns, and the JSONL file descriptor closes. An
//!     in-flight handler that already cloned the `Arc` keeps the coordinator
//!     alive until its append is acknowledged, so no 201'd write is ever lost
//!     (DR-103 durability is preserved).
//!
//! Exactly one coordinator may ever exist per canonical root: two writers on one
//! `AGENT_LEARNINGS.jsonl` corrupt it (DR-103). [`SupervisorMap::get_or_start`]
//! returns the SAME `Arc` for repeated calls on a live root — the no-double-start
//! invariant the unit tests guard with `Arc::ptr_eq`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::server::lifecycle::{ServerError, Supervisor};

/// Lifecycle parameters for a [`SupervisorMap`]. Defaults match
/// [`crate::server::index_map::ProjectIndexMapConfig`] so a project's coordinator
/// and its index handle reap on the same cadence.
#[derive(Debug, Clone)]
pub struct SupervisorMapConfig {
    pub capacity: usize,
    pub idle_timeout: Duration,
}

impl Default for SupervisorMapConfig {
    fn default() -> Self {
        Self {
            capacity: 10,
            idle_timeout: Duration::from_secs(30 * 60),
        }
    }
}

/// Lazy-started, LRU + idle-evicting map of per-project [`Supervisor`]s.
pub struct SupervisorMap {
    config: SupervisorMapConfig,
    /// MRU at the back. `(canonical root, supervisor, last_used)`. A `Vec`
    /// (not `LruCache`) because the cap is small (~10) and idle eviction has to
    /// walk every entry anyway — same shape as `ProjectIndexMap`.
    entries: Vec<(PathBuf, Arc<Supervisor>, Instant)>,
}

impl SupervisorMap {
    pub fn new(config: SupervisorMapConfig) -> Self {
        Self {
            config,
            entries: Vec::new(),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(SupervisorMapConfig::default())
    }

    /// Number of live coordinators currently held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Return the coordinator for `root`, starting it via `start` on a miss.
    ///
    /// Idle entries are reaped first, then the LRU front if at capacity.
    /// Returns the SAME `Arc` for repeated calls on a live root — this never
    /// double-starts a coordinator on one JSONL (two writers would corrupt it,
    /// DR-103). On a miss, the freshly started [`Supervisor`] is wrapped in an
    /// `Arc` and inserted MRU-at-back.
    pub fn get_or_start<F>(&mut self, root: &Path, start: F) -> Result<Arc<Supervisor>, ServerError>
    where
        F: FnOnce() -> Result<Supervisor, ServerError>,
    {
        // Reap idle coordinators up front so a slow caller can't keep a stale
        // coordinator (and its open JSONL fd) alive merely by having opened it.
        self.evict_idle();

        if let Some(idx) = self.entries.iter().position(|(p, _, _)| p == root) {
            // LRU touch: move the live entry to the back and refresh last_used.
            let mut entry = self.entries.remove(idx);
            entry.2 = Instant::now();
            let sup = entry.1.clone();
            self.entries.push(entry);
            return Ok(sup);
        }

        // Miss path: make room (drop LRU front → reap), then start.
        while self.entries.len() >= self.config.capacity {
            // Dropping the front entry drops its `Arc<Supervisor>`; if no
            // in-flight handler holds a clone, the coordinator drains and exits.
            self.entries.remove(0);
        }

        let sup = Arc::new(start()?);
        self.entries
            .push((root.to_path_buf(), sup.clone(), Instant::now()));
        Ok(sup)
    }

    /// Reap every entry whose `last_used` is older than `idle_timeout` by
    /// dropping its `Arc<Supervisor>`. Returns the number reaped.
    pub fn evict_idle(&mut self) -> usize {
        let Some(cutoff) = Instant::now().checked_sub(self.config.idle_timeout) else {
            return 0;
        };
        let before = self.entries.len();
        self.entries.retain(|(_, _, last)| *last >= cutoff);
        before - self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A start closure that counts invocations and hands back a no-op
    /// supervisor (channel + detached no-op task — see
    /// `Supervisor::for_backpressure_test`). The map only exercises
    /// `Arc` identity, length, and eviction, so a real coordinator is overkill.
    fn counting_start(counter: Arc<AtomicUsize>) -> impl Fn() -> Result<Supervisor, ServerError> {
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(Supervisor::for_backpressure_test().0)
        }
    }

    #[tokio::test]
    async fn get_or_start_returns_same_arc_for_live_root() {
        let mut map = SupervisorMap::with_defaults();
        let counter = Arc::new(AtomicUsize::new(0));
        let root = Path::new("/p/a");

        let a1 = map
            .get_or_start(root, counting_start(counter.clone()))
            .unwrap();
        let a2 = map
            .get_or_start(root, counting_start(counter.clone()))
            .unwrap();

        assert!(
            Arc::ptr_eq(&a1, &a2),
            "a live root must return the SAME Arc — two coordinators on one JSONL corrupt it (DR-103)"
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "the second call must reuse the live coordinator, not start a new one"
        );
        assert_eq!(map.len(), 1);
    }

    #[tokio::test]
    async fn lru_evicts_at_capacity() {
        let mut map = SupervisorMap::new(SupervisorMapConfig {
            capacity: 2,
            idle_timeout: Duration::from_secs(3600),
        });
        let counter = Arc::new(AtomicUsize::new(0));

        let a = map
            .get_or_start(Path::new("/p/a"), counting_start(counter.clone()))
            .unwrap();
        let _b = map
            .get_or_start(Path::new("/p/b"), counting_start(counter.clone()))
            .unwrap();
        // Third distinct root at cap 2 evicts the LRU front (/p/a).
        let _c = map
            .get_or_start(Path::new("/p/c"), counting_start(counter.clone()))
            .unwrap();

        assert_eq!(
            map.len(),
            2,
            "capacity 2 caps the map at two live coordinators"
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            3,
            "three distinct roots started three coordinators"
        );

        // /p/a was evicted from the map; requesting it again starts a NEW one.
        let a_again = map
            .get_or_start(Path::new("/p/a"), counting_start(counter.clone()))
            .unwrap();
        assert_eq!(
            counter.load(Ordering::SeqCst),
            4,
            "the evicted root must re-start on its next request"
        );
        assert!(
            !Arc::ptr_eq(&a, &a_again),
            "the re-started coordinator is a distinct instance"
        );
    }

    #[tokio::test]
    async fn idle_eviction_reaps_aged_entries() {
        let mut map = SupervisorMap::new(SupervisorMapConfig {
            capacity: 10,
            idle_timeout: Duration::from_millis(20),
        });
        let counter = Arc::new(AtomicUsize::new(0));

        let _a = map
            .get_or_start(Path::new("/p/a"), counting_start(counter.clone()))
            .unwrap();
        assert_eq!(map.len(), 1);

        // Age /p/a past the idle timeout. The next get_or_start runs evict_idle
        // first, so /p/a is reaped and only the freshly started /p/b remains.
        std::thread::sleep(Duration::from_millis(40));
        let _b = map
            .get_or_start(Path::new("/p/b"), counting_start(counter.clone()))
            .unwrap();

        assert_eq!(
            map.len(),
            1,
            "the aged /p/a entry must be idle-reaped; only /p/b remains"
        );
    }
}
