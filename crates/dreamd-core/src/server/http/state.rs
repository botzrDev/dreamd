//! `AppState` — shared Axum state and WEG-272 multi-project routing.
//!
//! Lock order is always `supervisor_map` AFTER `index_map`: resolve the indexer
//! `Sender` first (releasing `index_map`'s lock) and only then lock
//! `supervisor_map`. No other path locks `index_map` while holding
//! `supervisor_map`, so there is no deadlock.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::config::Config;
use crate::server::index_map::IndexError;
use crate::server::supervisor_map::SupervisorMap;
use crate::server::{ProjectIndexMap, Supervisor, TantivyIndexHandle};

/// Shared application state cloned into every Axum request via
/// `State<AppState>`. All fields behind `Arc` so cloning is cheap.
///
/// `registry_path` — path to `~/.agent/registry.toml`. Per-request
/// middleware calls `resolve_project(&state.registry_path, agent_root)`.
///
/// `supervisor` — the boot project's coordinator (the project `run_watch`
/// started for `primary.0`). Handlers no longer dispatch to it directly; they
/// call [`AppState::resolve_supervisor`], which returns this for the pinned
/// boot project and a lazily-started per-root coordinator otherwise.
///
/// `config` — layered runtime config loaded at startup.
///
/// `index_map` — lazy-opened per-project Tantivy handles. `Mutex` (not
/// `RwLock`) because `ProjectIndexMap::get_or_open` is `&mut self` even
/// for reads (mutates LRU ordering).
///
/// `daemon_uid` — UID of the process that started the daemon. Used by
/// `peer_uid_middleware` (WEG-72 / DR-407) to reject connections from other
/// users. Set to `nix::unistd::Uid::current().as_raw()` at startup.
///
/// `primary` — the pinned per-project Tantivy handle for the daemon's booted
/// project (WEG-264 Defect 2). When set, recall and dream for this exact
/// (canonicalized) project root use this handle instead of `index_map`, so the
/// coordinator's live appends and the recall reader share **one** index — and
/// therefore one Tantivy `IndexWriter` (only one is permitted per index dir).
/// Never evicted. `None` outside the daemon (Phase 1, tests).
///
/// `supervisor_map` — lazily-started, LRU + idle-evicting per-project
/// coordinators for every registered root that is NOT the pinned boot project
/// (WEG-272). `resolve_supervisor` consults this so a `POST /learn` /
/// `POST /dream` for project B appends to B's JSONL instead of misfiling into
/// the boot project's. Only engaged when `primary` is `Some` (the daemon);
/// `None`/Phase-1/test states route everything to `supervisor`.
#[derive(Clone)]
pub struct AppState {
    pub registry_path: PathBuf,
    pub supervisor: Arc<Supervisor>,
    pub config: Arc<Config>,
    pub index_map: Arc<Mutex<ProjectIndexMap<TantivyIndexHandle>>>,
    pub daemon_uid: u32,
    pub primary: Option<(PathBuf, Arc<TantivyIndexHandle>)>,
    pub supervisor_map: Arc<Mutex<SupervisorMap>>,
}

impl AppState {
    pub fn new(
        registry_path: PathBuf,
        supervisor: Supervisor,
        config: Config,
        index_map: ProjectIndexMap<TantivyIndexHandle>,
        daemon_uid: u32,
    ) -> Self {
        Self {
            registry_path,
            supervisor: Arc::new(supervisor),
            config: Arc::new(config),
            index_map: Arc::new(Mutex::new(index_map)),
            daemon_uid,
            primary: None,
            supervisor_map: Arc::new(Mutex::new(SupervisorMap::with_defaults())),
        }
    }

    /// Pin `handle` as the primary index for `root` (a canonicalized project
    /// root, matching the canonical form `resolve_project` returns). Called
    /// only by `run_watch`.
    pub fn with_primary(mut self, root: PathBuf, handle: Arc<TantivyIndexHandle>) -> Self {
        self.primary = Some((root, handle));
        self
    }

    /// Resolve the live index handle for `root` and run `f` against it.
    ///
    /// If `root` is the daemon's pinned primary project, `f` runs against that
    /// shared handle (no lock, never evicted — the coordinator feeds it and
    /// recall reads it). Otherwise the handle is looked up (or lazily opened)
    /// in `index_map`. `f` does cheap, non-blocking work only — clone the
    /// `IndexReader` or the indexer `Sender` and return it; never hold the
    /// returned value's work across the `index_map` mutex.
    pub(crate) fn with_index_handle<T>(
        &self,
        root: &Path,
        f: impl FnOnce(&TantivyIndexHandle) -> T,
    ) -> Result<T, IndexError> {
        if let Some((primary_root, handle)) = self.primary.as_ref() {
            if primary_root.as_path() == root {
                return Ok(f(handle.as_ref()));
            }
        }
        let mut map = self
            .index_map
            .lock()
            .map_err(|_| IndexError("index map lock poisoned".to_string()))?;
        let handle = map.get_or_open(root, |r| {
            TantivyIndexHandle::open(
                &crate::layout::AgentRoot::new(r),
                crate::server::tantivy_handle::DEFAULT_COMMIT_CADENCE,
            )
        })?;
        Ok(f(handle))
    }

    /// Resolve the coordinator that owns `root` (WEG-272).
    ///
    /// * `primary` is `None` (Phase 1 / tests, single project) → the boot
    ///   `state.supervisor`. Preserves every existing single-coordinator test.
    /// * `root` IS the pinned boot project → the boot `state.supervisor`.
    /// * otherwise → a lazily-started, idle-reaped per-root coordinator from
    ///   `supervisor_map`, wired to that root's index handle so its appends
    ///   index into the same handle recall and dream read.
    ///
    /// Returns the SAME `Arc<Supervisor>` for repeated calls on one live root,
    /// so two writers never open the same `AGENT_LEARNINGS.jsonl` (DR-103).
    pub(crate) fn resolve_supervisor(&self, root: &Path) -> Result<Arc<Supervisor>, IndexError> {
        match self.primary.as_ref() {
            // No pinned project → single-coordinator behaviour (every test).
            None => return Ok(self.supervisor.clone()),
            // The request targets the pinned boot project → its coordinator.
            Some((boot_root, _)) if boot_root.as_path() == root => {
                return Ok(self.supervisor.clone());
            }
            Some(_) => {}
        }

        // Non-boot root: recover any stale WAL before opening indexes or coordinators.
        let agent_root = crate::layout::AgentRoot::new(root);
        crate::wal::recover_on_startup(&agent_root)
            .map_err(|e| IndexError(format!("wal recovery: {e}")))?;

        // Wire the per-root coordinator to the per-root indexer (the same handle
        // recall/dream use) so its appends become searchable. Resolve + release
        // index_map's lock BEFORE locking supervisor_map.
        let indexer_tx = self.with_index_handle(root, |h| h.sender())?;

        let mut map = self
            .supervisor_map
            .lock()
            .map_err(|_| IndexError("supervisor map lock poisoned".to_string()))?;
        map.get_or_start(root, || {
            Supervisor::start(
                &agent_root,
                crate::server::COORDINATOR_CHANNEL_CAPACITY,
                Some(indexer_tx),
            )
        })
        .map_err(|e| IndexError(format!("supervisor start failed: {e}")))
    }
}
