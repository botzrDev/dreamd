//! `AppState` — shared Axum state and WEG-272 multi-project routing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::config::Config;
use crate::recurrence::RecallReader;
use crate::registry::RegistryCache;
use crate::server::tantivy_handle::RecallFlusher;
#[cfg(unix)]
use crate::server::{AppendMark, CommitMark};
use crate::server::{
    IndexError, ProjectResourceMap, ProjectRuntime, Supervisor, TantivyIndexHandle,
};

/// Shared application state cloned into every Axum request via
/// `State<AppState>`. All fields behind `Arc` so cloning is cheap.
///
/// `registry_path` — path to `~/.agent/registry.toml`.
///
/// `registry` — that file, parsed once per file identity (BZR-1243).
/// `agent_root_middleware` resolves every request's `X-Agent-Root` through it,
/// on a blocking thread.
///
/// `supervisor` — the boot project's coordinator (the project `run_watch`
/// started for `primary.0`). Handlers do not dispatch to it directly; they
/// call `AppState::resolve_supervisor`, which returns this for the pinned
/// boot project and a lazily-started per-root coordinator otherwise.
///
/// `config` — layered runtime config loaded at startup.
///
/// `runtimes` — lazily-opened, LRU + idle-evicting [`ProjectRuntime`]s for
/// every registered root that is not the pinned boot project. Each holds the
/// project writer lock, the project's index and its coordinator wired to
/// that index, so the three open and evict as one entry. `Mutex` (not
/// `RwLock`) because every lookup mutates LRU ordering. A runtime is not
/// opened under this lock; see `AppState::with_runtime` (AILAB-186).
///
/// `opening` — one gate per root, so one request at a time opens that root's
/// runtime (DRM-50).
///
/// `daemon_uid` — UID of the process that started the daemon. Used by
/// `peer_uid_middleware` (WEG-72) to reject connections from other users.
/// `run_watch` sets it to `nix::unistd::Uid::current().as_raw()` on Unix, and
/// to 0 off Unix, where nothing reads it.
///
/// `primary` — the pinned per-project Tantivy handle for the daemon's booted
/// project (WEG-264). When set, recall and dream for this exact
/// (canonicalized) project root use this handle instead of `runtimes`, so the
/// coordinator's live appends and the recall reader share **one** index — and
/// therefore one Tantivy `IndexWriter` (only one is permitted per index dir).
/// Never evicted. Once a check finds its store replaced (`git clean -fdX`),
/// the pair is retired, and the boot root is served from `runtimes` like any
/// other project (DRM-95). `None` only in tests; `run_watch` always pins one.
///
/// `auth_json` — path to `~/.agent/auth.json`, the bearer token file
/// (AILAB-203) that `bearer_auth_middleware` re-reads on every request. `None`
/// on Unix because the peer UID from `SO_PEERCRED` is the auth there and
/// `build_router` mounts no bearer layer at all, so the field is never read.
/// `None` off Unix is **fail-closed**: the bearer layer *is* mounted there, and
/// with no path to read it 401s every request rather than serving a router
/// whose only auth was never configured. Set by
/// [`AppState::with_auth_json`], which only the loopback-TCP `run_watch` calls.
#[derive(Clone)]
pub struct AppState {
    /// Path to `~/.agent/registry.toml` (see struct docs).
    pub registry_path: PathBuf,
    /// `registry_path`, parsed once per file identity (BZR-1243).
    pub registry: Arc<RegistryCache>,
    /// Boot-project coordinator; prefer `Self::resolve_supervisor`.
    pub supervisor: Arc<Supervisor>,
    /// Layered runtime config loaded at daemon start.
    pub config: Arc<Config>,
    /// Lazy per-project lock + index + coordinator (LRU + idle eviction).
    pub runtimes: Arc<Mutex<ProjectResourceMap<ProjectRuntime>>>,
    /// One open at a time per root (DRM-50); see `AppState::with_runtime`.
    pub(crate) opening: Arc<OpenGates>,
    /// Daemon process UID for `peer_uid_middleware` (WEG-72).
    pub daemon_uid: u32,
    /// Pinned boot-project index handle; `None` only in tests.
    pub primary: Option<(PathBuf, Arc<TantivyIndexHandle>)>,
    /// Path to `~/.agent/auth.json` for `bearer_auth_middleware` (AILAB-192).
    /// `None` on Unix (peer UID is the auth) and fail-closed off Unix.
    pub auth_json: Option<PathBuf>,
}

impl AppState {
    pub fn new(
        registry_path: PathBuf,
        supervisor: Supervisor,
        config: Config,
        daemon_uid: u32,
    ) -> Self {
        Self {
            registry: Arc::new(RegistryCache::new(registry_path.clone())),
            registry_path,
            supervisor: Arc::new(supervisor),
            config: Arc::new(config),
            runtimes: Arc::new(Mutex::new(ProjectResourceMap::with_defaults())),
            opening: Arc::default(),
            daemon_uid,
            primary: None,
            auth_json: None,
        }
    }

    /// Pin `handle` as the primary index for `root` (a canonicalized project
    /// root, matching the canonical form `RegistryCache::resolve` returns).
    /// Called only by `run_watch`.
    pub fn with_primary(mut self, root: PathBuf, handle: Arc<TantivyIndexHandle>) -> Self {
        self.primary = Some((root, handle));
        self
    }

    /// Point `bearer_auth_middleware` at `path` — `DaemonHome::auth_json()`,
    /// the bearer token file (AILAB-192, AILAB-203).
    ///
    /// [`Self::new`] leaves `auth_json` as `None`. The Unix daemon never calls
    /// this: `peer_uid_middleware` is its auth and `build_router` mounts no
    /// bearer layer on that target. Only the loopback-TCP `run_watch` calls
    /// it, and it must, because `None` there is fail-closed.
    pub fn with_auth_json(mut self, path: PathBuf) -> Self {
        self.auth_json = Some(path);
        self
    }

    /// Lock `runtimes`, mapping poisoning onto [`IndexError`]. Exists so every
    /// guard in [`Self::with_runtime`] is one line and its scope is
    /// unmistakable.
    fn lock_runtimes(
        &self,
    ) -> Result<MutexGuard<'_, ProjectResourceMap<ProjectRuntime>>, IndexError> {
        self.runtimes.lock().map_err(|_| IndexError::LockPoisoned)
    }

    /// Resolve the live [`ProjectRuntime`] for `root` and run `f` against it.
    ///
    /// A hit is touched and `f` runs under the `runtimes` mutex. A miss opens
    /// the runtime with the `runtimes` mutex **released** (AILAB-186), then
    /// re-takes it to publish the result. The open takes the project writer
    /// lock, runs WAL recovery, runs the index coverage pass (indexing every
    /// log id the index lacks), issues a Tantivy commit, allocates a 50 MB
    /// `IndexWriter` and starts a coordinator; holding the map lock across that
    /// would block every *other* project's requests for its duration.
    ///
    /// One request at a time opens a given root (DRM-50). A miss takes the
    /// root's gate in `opening` and checks the map again under it, so a request
    /// that waited behind another request's open gets the runtime that request
    /// published. Without the gate, every request but the first was refused by
    /// the writer lock the first one held while it opened. The gate is per
    /// root, so an open never holds up another root's requests.
    ///
    /// [`ProjectRuntime::open_for_watch`] waits, for a bounded time, while a
    /// runtime this map released for `root` still holds its writer lock.
    ///
    /// Recovery runs only here, on the open. A request for a project whose
    /// runtime is live never recovers it, because a WAL on a live project
    /// belongs to a cycle that is running.
    ///
    /// A runtime whose store a check found replaced is not a hit (DRM-95). It
    /// is taken out of the map and released, and this call opens the project
    /// again: a new writer lock on the new store, WAL recovery, and the
    /// coverage pass, which indexes the log.
    ///
    /// `f` does cheap, non-blocking work only — clone what it needs and return
    /// it; never hold the returned value's work across the `runtimes` mutex.
    async fn with_runtime<T>(
        &self,
        root: &Path,
        f: impl FnOnce(&ProjectRuntime) -> T,
    ) -> Result<T, IndexError> {
        // Hit path. The guard dies at this block's closing brace.
        {
            let mut map = self.lock_runtimes()?;
            if let Some(runtime) = live_runtime(&mut map, root) {
                return Ok(f(runtime));
            }
        }

        let gate = self.opening.gate(root)?;
        let _opening = gate.lock().await;

        // A request that held the gate before this one may have published.
        {
            let mut map = self.lock_runtimes()?;
            if let Some(runtime) = live_runtime(&mut map, root) {
                return Ok(f(runtime));
            }
        }

        // Only this root's gate is held across the open.
        let opened = ProjectRuntime::open_for_watch(&crate::layout::AgentRoot::new(root)).await?;

        let mut map = self.lock_runtimes()?;
        Ok(f(map.insert_or_adopt(root, opened)))
    }

    /// Resolve the live index handle for `root` and run `f` against it.
    ///
    /// If `root` is the daemon's pinned primary project, `f` runs against that
    /// shared handle (no lock, never evicted — the coordinator feeds it and
    /// recall reads it). Otherwise `f` runs against the index of `root`'s
    /// [`ProjectRuntime`], opened on a miss by [`Self::with_runtime`].
    ///
    /// A root that is not the pinned boot project opens a [`ProjectRuntime`],
    /// which holds that project's index.
    pub(crate) async fn with_index_handle<T>(
        &self,
        root: &Path,
        f: impl FnOnce(&TantivyIndexHandle) -> T,
    ) -> Result<T, IndexError> {
        if let Some(handle) = self.live_primary(root) {
            return Ok(f(handle.as_ref()));
        }

        self.with_runtime(root, |runtime| f(runtime.index())).await
    }

    /// The pinned boot handle, when `root` is the boot project and no check
    /// has found its store replaced (DRM-95).
    ///
    /// Once one has, the boot project is retired: every resolver serves its
    /// root from `runtimes`, like any other project, so the next request
    /// reopens it. The guard is sticky, so the boot pair is never used again.
    /// It reads the guard's flag only. The write or recall that runs next makes
    /// its own check, and the handler that gets [`IndexError::StoreReplaced`]
    /// or its coordinator twin sends the request once more.
    fn live_primary(&self, root: &Path) -> Option<&Arc<TantivyIndexHandle>> {
        match self.primary.as_ref() {
            Some((primary_root, handle))
                if primary_root.as_path() == root && !handle.store_replaced() =>
            {
                Some(handle)
            }
            _ => None,
        }
    }

    /// What a recall on `root` needs (BZR-1437): the index's reader, its
    /// [`RecallFlusher`], and the paired coordinator's append count, read now.
    /// It pairs as `live_marks` does: `primary` with the boot supervisor for
    /// the boot root, otherwise `root`'s runtime, opened on a miss. Every part
    /// is a clone, so no lock is held across the flush or the search.
    pub(crate) async fn recall_target(
        &self,
        root: &Path,
    ) -> Result<(RecallReader, RecallFlusher, u64), IndexError> {
        if let Some(handle) = self.live_primary(root) {
            handle.check_store()?;
            return Ok((
                handle.reader().clone(),
                handle.recall_flusher(),
                self.supervisor.append_mark().appended,
            ));
        }
        self.with_runtime(root, |runtime| {
            let index = runtime.index();
            index.check_store()?;
            Ok((
                index.reader().clone(),
                index.recall_flusher(),
                runtime.supervisor().append_mark().appended,
            ))
        })
        .await?
    }

    /// The marks of `root`'s coordinator and index, as one wired pair: `primary`
    /// with the boot supervisor for the boot root, otherwise `root`'s runtime,
    /// opened on a miss. Unlike `resolve_supervisor`, it never pairs the boot
    /// supervisor with another root's index.
    ///
    /// The commit mark is read before the append mark, so `appended` is never
    /// older than `committed_appends` (BZR-1243).
    #[cfg(unix)]
    pub(crate) async fn live_marks(
        &self,
        root: &Path,
    ) -> Result<(CommitMark, AppendMark), IndexError> {
        if let Some(handle) = self.live_primary(root) {
            let commit = handle.commit_mark();
            return Ok((commit, self.supervisor.append_mark()));
        }
        self.with_runtime(root, |runtime| {
            let commit = runtime.index().commit_mark();
            (commit, runtime.supervisor().append_mark())
        })
        .await
    }

    /// Resolve the coordinator that owns `root` (WEG-272).
    ///
    /// * `primary` is `None` (tests only) → the boot `state.supervisor`, the
    ///   one coordinator those tests start.
    /// * `root` is the pinned boot project, and no check has found its store
    ///   replaced → the boot `state.supervisor`.
    /// * otherwise → the coordinator of `root`'s [`ProjectRuntime`], opened on a
    ///   miss, wired to that root's index so its appends index into the same
    ///   handle recall and dream read. A retired boot project lands here too
    ///   (DRM-95).
    ///
    /// Returns the same `Arc<Supervisor>` for repeated calls on one live root,
    /// so two writers never open the same `AGENT_LEARNINGS.jsonl`.
    pub(crate) async fn resolve_supervisor(
        &self,
        root: &Path,
    ) -> Result<Arc<Supervisor>, IndexError> {
        // No pinned project (tests only) → the one coordinator.
        if self.primary.is_none() || self.live_primary(root).is_some() {
            return Ok(self.supervisor.clone());
        }

        self.with_runtime(root, |runtime| Arc::clone(runtime.supervisor()))
            .await
    }
}

/// `root`'s runtime in `map`, unless a check found its store replaced. That
/// runtime is taken out and released, and the caller opens a new one (DRM-95).
/// Its tasks keep their clones of the old writer lock until they end. That
/// lock is on the deleted lockfile, so it does not block the new one.
fn live_runtime<'m>(
    map: &'m mut ProjectResourceMap<ProjectRuntime>,
    root: &Path,
) -> Option<&'m mut ProjectRuntime> {
    if map.get_mut(root)?.index().store_replaced() {
        if let Some(stale) = map.remove(root) {
            if let Err(e) = crate::server::project_resource_map::ProjectMapResource::release(stale)
            {
                tracing::warn!(root = %root.display(), "releasing a replaced store's runtime: {e}");
            }
        }
        return None;
    }
    map.get_mut(root)
}

/// One async gate per project root, so one request at a time opens a root's
/// runtime (DRM-50). A root's gate is made on its first miss and never
/// removed: there is one per root the daemon has opened, and each is an empty
/// async mutex.
#[derive(Default)]
pub(crate) struct OpenGates(Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>);

impl OpenGates {
    /// `root`'s gate, made on first use.
    fn gate(&self, root: &Path) -> Result<Arc<tokio::sync::Mutex<()>>, IndexError> {
        let mut gates = self.0.lock().map_err(|_| IndexError::LockPoisoned)?;
        Ok(Arc::clone(gates.entry(root.to_path_buf()).or_default()))
    }
}
