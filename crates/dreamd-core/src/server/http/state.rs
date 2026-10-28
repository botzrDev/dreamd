//! `AppState` — shared Axum state and WEG-272 multi-project routing.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::config::Config;
use crate::registry::RegistryCache;
#[cfg(unix)]
use crate::server::{AppendMark, CommitMark};
use crate::server::{
    IndexError, ProjectResourceMap, ProjectRuntime, Supervisor, TantivyIndexHandle,
};
use crate::writer_lock::WriterKind;

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
/// started for `primary.0`). Handlers no longer dispatch to it directly; they
/// call [`AppState::resolve_supervisor`], which returns this for the pinned
/// boot project and a lazily-started per-root coordinator otherwise.
///
/// `config` — layered runtime config loaded at startup.
///
/// `runtimes` — lazily-opened, LRU + idle-evicting [`ProjectRuntime`]s for
/// every registered root that is not the pinned boot project. Each holds the
/// project writer lock, the project's index (Unix) and its coordinator wired to
/// that index, so the three open and evict as one entry. `Mutex` (not
/// `RwLock`) because every lookup mutates LRU ordering. Opening a runtime does
/// NOT happen under this lock; see [`AppState::with_runtime`] (AILAB-186).
///
/// `daemon_uid` — UID of the process that started the daemon. Used by
/// `peer_uid_middleware` (WEG-72 / DR-407) to reject connections from other
/// users. Set to `nix::unistd::Uid::current().as_raw()` at startup.
///
/// `primary` — the pinned per-project Tantivy handle for the daemon's booted
/// project (WEG-264 Defect 2). When set, recall and dream for this exact
/// (canonicalized) project root use this handle instead of `runtimes`, so the
/// coordinator's live appends and the recall reader share **one** index — and
/// therefore one Tantivy `IndexWriter` (only one is permitted per index dir).
/// Never evicted. `None` outside the daemon (in-process MCP fallback, tests).
///
/// `auth_json` — path to `~/.agent/auth.json`, the AILAB-203 bearer token file
/// that `bearer_auth_middleware` re-reads on every request (AILAB-192). `None`
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
    /// Boot-project coordinator; prefer [`Self::resolve_supervisor`].
    pub supervisor: Arc<Supervisor>,
    /// Layered runtime config loaded at daemon start.
    pub config: Arc<Config>,
    /// Lazy per-project lock + index + coordinator (LRU + idle eviction).
    pub runtimes: Arc<Mutex<ProjectResourceMap<ProjectRuntime>>>,
    /// Daemon process UID for `peer_uid_middleware` (WEG-72).
    pub daemon_uid: u32,
    /// Pinned boot-project index handle; `None` in in-process / test setups.
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
            daemon_uid,
            primary: None,
            auth_json: None,
        }
    }

    /// Pin `handle` as the primary index for `root` (a canonicalized project
    /// root, matching the canonical form `resolve_project` returns). Called
    /// only by `run_watch`.
    pub fn with_primary(mut self, root: PathBuf, handle: Arc<TantivyIndexHandle>) -> Self {
        self.primary = Some((root, handle));
        self
    }

    /// Point `bearer_auth_middleware` at `path` — `DaemonHome::auth_json()`,
    /// the AILAB-203 token file (AILAB-192).
    ///
    /// Additive on purpose: [`Self::new`] leaves `auth_json` as `None` so no
    /// existing caller or test has to change, and the Unix daemon never calls
    /// this at all — `peer_uid_middleware` is its auth and `build_router`
    /// mounts no bearer layer on that target. Only the loopback-TCP
    /// `run_watch` calls it, and it must, because `None` there is fail-closed.
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
    /// A hit is touched and `f` runs under the `runtimes` mutex. On a miss the
    /// mutex is **released before** [`ProjectRuntime::open`] runs, then re-taken
    /// to publish the result (AILAB-186). `open` takes the project writer lock,
    /// runs WAL recovery, replays the entire JSONL into the index, issues a
    /// Tantivy commit, allocates a 50 MB `IndexWriter` and starts a coordinator;
    /// holding the map lock across that blocked every *other* project's requests
    /// for its duration.
    ///
    /// That window is racy by construction — two first-touches of one root can
    /// both try to open. The project writer lock refuses the second, and
    /// [`ProjectResourceMap::insert_or_adopt`] settles any pair that both
    /// opened: the runtime already in the map wins and the loser is released,
    /// never leaked. If our own open *fails*, the map is re-checked before the
    /// error is surfaced, so a caller that lost the race to a concurrent opener
    /// still gets the winner's runtime.
    ///
    /// Recovery runs only here, on the open. A request for a project whose
    /// runtime is live never recovers it, because a WAL on a live project
    /// belongs to a cycle that is running.
    ///
    /// `f` does cheap, non-blocking work only — clone what it needs and return
    /// it; never hold the returned value's work across the `runtimes` mutex.
    fn with_runtime<T>(
        &self,
        root: &Path,
        f: impl FnOnce(&ProjectRuntime) -> T,
    ) -> Result<T, IndexError> {
        // Hit path. The guard dies at this block's closing brace.
        {
            let mut map = self.lock_runtimes()?;
            if let Some(runtime) = map.get_mut(root) {
                return Ok(f(runtime));
            }
        }

        // No lock is held from here to the re-lock below — the open is the only
        // thing between them.
        let opened =
            match ProjectRuntime::open(&crate::layout::AgentRoot::new(root), WriterKind::Watch) {
                Ok(runtime) => runtime,
                Err(e) => {
                    let mut map = self.lock_runtimes()?;
                    return match map.get_mut(root) {
                        Some(runtime) => Ok(f(runtime)),
                        None => Err(e),
                    };
                }
            };

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
    /// Off Unix a runtime has no index (`TantivyIndexHandle::open` writes
    /// through `io::write_atomic`, which is `Unsupported` there), so a
    /// non-primary root is an error.
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

        #[cfg(unix)]
        {
            self.with_runtime(root, |runtime| f(runtime.index()))
        }
        #[cfg(not(unix))]
        {
            let _ = f;
            Err(IndexError::Io(format!(
                "no index for {} on this target",
                root.display()
            )))
        }
    }

    /// The marks of `root`'s coordinator and index, as one wired pair: `primary`
    /// with the boot supervisor for the boot root, otherwise `root`'s runtime,
    /// opened on a miss. Unlike `resolve_supervisor`, it never pairs the boot
    /// supervisor with another root's index.
    ///
    /// The commit mark is read before the append mark, so `appended` is never
    /// older than `committed_appends` (BZR-1243).
    #[cfg(unix)]
    pub(crate) fn live_marks(&self, root: &Path) -> Result<(CommitMark, AppendMark), IndexError> {
        if let Some((primary_root, handle)) = self.primary.as_ref() {
            if primary_root.as_path() == root {
                let commit = handle.commit_mark();
                return Ok((commit, self.supervisor.append_mark()));
            }
        }
        self.with_runtime(root, |runtime| {
            let commit = runtime.index().commit_mark();
            (commit, runtime.supervisor().append_mark())
        })
    }

    /// Resolve the coordinator that owns `root` (WEG-272).
    ///
    /// * `primary` is `None` (in-process MCP fallback / tests, single project) → the boot
    ///   `state.supervisor`. Preserves every existing single-coordinator test.
    /// * `root` IS the pinned boot project → the boot `state.supervisor`.
    /// * otherwise → the coordinator of `root`'s [`ProjectRuntime`], opened on a
    ///   miss, wired to that root's index so its appends index into the same
    ///   handle recall and dream read.
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

        self.with_runtime(root, |runtime| Arc::clone(runtime.supervisor()))
    }
}
