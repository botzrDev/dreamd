//! Foreground daemon: `dreamd watch` (WEG-88 / DR-702).
//!
//! ## Actor topology
//!
//! ```text
//! run_watch
//!   ├─ Supervisor (boot project) ──► MemoryCoordinator actor
//!   │     └─ indexer_tx ──► TantivyIndexHandle (pinned "primary")
//!   ├─ AppState.supervisor_map ──► lazy per-project Supervisors (WEG-272)
//!   ├─ AppState.index_map ──► lazy Tantivy handles for non-boot projects
//!   └─ serve_uds ──► HTTP router (peer UID + X-Agent-Root middleware)
//! ```
//!
//! The **primary handle** is handed off at boot so the coordinator's live appends
//! and recall/dream read the same Tantivy index (one `IndexWriter` per dir).
//!
//! ## Signal handling
//!
//! Blocks until SIGINT (`ctrl_c`) or SIGTERM, then unlinks
//! `~/.agent/dreamd.sock` and drains the boot coordinator and the primary
//! index, bounded by [`DRAIN_TIMEOUT`] — see [`drain_daemon`]. Lazily-opened
//! per-project coordinators and index handles are not touched on this path;
//! eviction reaps them on drop (AILAB-306).

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::signal::unix::{signal, SignalKind};

use crate::config::{load_config, DreamCycleMode};
use crate::layout::{AgentRoot, DaemonHome};
use crate::server::build_router;
use crate::server::http::AppState;
use crate::server::index_map::{ProjectIndexMap, ProjectIndexMapConfig};
use crate::server::lifecycle::{ServerError, Supervisor, COORDINATOR_CHANNEL_CAPACITY};
use crate::server::tantivy_handle::{TantivyIndexHandle, DEFAULT_COMMIT_CADENCE};
use crate::server::uds_server::{bind_api_socket, serve_uds};

/// Failure modes surfaced by [`run_watch`].
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("no project root found from {0:?}; run `dreamd init` first")]
    NoProjectRoot(String),
    #[error("config: {0}")]
    DreamMode(String),
    #[error("supervisor: {0}")]
    Server(#[from] ServerError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("config load: {0}")]
    Config(String),
    #[error("index: {0}")]
    Index(#[from] crate::server::index_map::IndexError),
    #[error("wal recovery: {0}")]
    Wal(#[from] crate::wal::WalError),
}

/// Boot a per-project daemon in the foreground, binding `~/.agent/dreamd.sock`,
/// and block until SIGINT/SIGTERM. Called by `dreamd watch`.
///
/// The sequence is: discover AgentRoot → load config + WEG-66 guard →
/// WAL recovery → boot Supervisor → compose AppState → bind socket → serve
/// until a shutdown signal (SIGINT/SIGTERM), then unlink the socket.
pub async fn run_watch(cwd: &Path) -> Result<(), WatchError> {
    // 1. Discover project root from cwd.
    let agent_root = AgentRoot::discover(cwd)
        .map_err(|_| WatchError::NoProjectRoot(cwd.display().to_string()))?;

    // 2. Load config + WEG-66 startup guard (rejects DreamCycleMode::Auto in v0.1).
    let config =
        load_config(agent_root.project_root()).map_err(|e| WatchError::Config(e.to_string()))?;
    if config.dream_cycle_mode == DreamCycleMode::Auto {
        return Err(WatchError::DreamMode(
            "dream_cycle_mode = \"auto\" is not supported in v0.1 \
             (LLM mode ships in v0.1.1)"
                .into(),
        ));
    }

    // 2.5. Recover any stale dream-cycle WAL before opening indexes or coordinators.
    crate::wal::recover_on_startup(&agent_root)?;

    // 3. Open the project index up front and wire it as the coordinator's live
    //    indexer (the "Option B" hand-off documented in tantivy_handle.rs). The
    //    same handle is pinned into AppState below so recall reads exactly what
    //    appends write — one handle, one Tantivy writer (WEG-264 Defect 2).
    let primary_handle = Arc::new(TantivyIndexHandle::open(
        &agent_root,
        DEFAULT_COMMIT_CADENCE,
    )?);
    let supervisor = Supervisor::start(
        &agent_root,
        COORDINATOR_CHANNEL_CAPACITY,
        Some(primary_handle.sender()),
    )?;

    // 4. Compose AppState. daemon_uid is this process's UID — peer_uid_middleware
    //    (WEG-72 / DR-407) rejects any connection whose peer UID differs.
    let home = dirs::home_dir().ok_or_else(|| {
        WatchError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "could not determine home directory",
        ))
    })?;
    let daemon_home = DaemonHome::new(home.join(".agent"));
    let registry_path = daemon_home.registry_toml();
    let index_map = ProjectIndexMap::<TantivyIndexHandle>::new(ProjectIndexMapConfig::default());
    let daemon_uid = nix::unistd::Uid::current().as_raw();
    // Pin under the canonical project root: `resolve_project` canonicalizes
    // every X-Agent-Root lookup, so recall/dream must match on the same form.
    let primary_root = std::fs::canonicalize(agent_root.project_root())
        .unwrap_or_else(|_| agent_root.project_root().to_path_buf());
    let state = AppState::new(registry_path, supervisor, config, index_map, daemon_uid)
        .with_primary(primary_root, primary_handle);
    // Retain shutdown handles BEFORE `state` moves into `build_router`
    // (AILAB-162). After the move the only owners are the router and its live
    // connection tasks, so there would be nothing left here to drain.
    let drain_supervisor = Arc::clone(&state.supervisor);
    let drain_index = state.primary.as_ref().map(|(_, h)| Arc::clone(h));
    let router = build_router(state);

    // 5. Bind socket + serve. bind_api_socket handles stale-socket recovery.
    let sock_path = daemon_home.socket_path();
    let listener = bind_api_socket(&sock_path)?;

    tracing::info!(
        "dreamd watch: serving on {} (project: {})",
        sock_path.display(),
        agent_root.project_root().display(),
    );

    // 6. Serve until SIGINT or SIGTERM. A service manager (systemd/launchd)
    //    stops the daemon with SIGTERM, so both signals must reach the cleanup
    //    below, which drains the coordinator and the primary index.
    let mut sigterm = signal(SignalKind::terminate())?;
    let outcome = tokio::select! {
        result = serve_uds(listener, router) => result,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("dreamd watch: SIGINT received, shutting down");
            Ok(())
        }
        _ = sigterm.recv() => {
            tracing::info!("dreamd watch: SIGTERM received, shutting down");
            Ok(())
        }
    };

    // Unlink the socket FIRST, on every shutdown path — SIGINT, SIGTERM, or a
    // serve error. Best-effort: the next bind would recover a stale socket
    // anyway (bind_socket_raw), but a service-managed daemon stopped via
    // SIGTERM must not leave ~/.agent/dreamd.sock behind (WEG-268).
    //
    // Ordering against the drain is deliberate. The drain below is bounded by
    // DRAIN_TIMEOUT (AILAB-748), but a `Shutdown` queued behind an in-progress
    // dream cycle can still hold it for the full bound — so putting the unlink
    // after it would put a shipped guarantee behind that wait, and systemd's
    // `TimeoutStopSec` SIGKILL could land first and strand the socket. AILAB-162
    // requires only that the drain happen before process exit, not before the
    // unlink. A client that connects in the window between the two gets
    // ECONNREFUSED instead of a 503, which is the better failure anyway.
    let _ = std::fs::remove_file(&sock_path);

    // Drain the boot coordinator and the primary index (AILAB-162). Appends
    // already accepted are only durable once the coordinator has processed
    // them and the index writer has committed; returning here without
    // draining would drop the uncommitted batch. Bounded by DRAIN_TIMEOUT
    // (AILAB-748): a drain that overruns the cap is logged and abandoned —
    // the JSONL append is already durable, so a lost final commit costs a
    // rebuild on next open, not data.
    let _ = with_drain_timeout(DRAIN_TIMEOUT, drain_daemon(drain_supervisor, drain_index)).await;

    outcome.map_err(WatchError::from)
}

/// Upper bound on the post-signal coordinator/index drain (AILAB-748).
/// Past this, `run_watch` logs and exits anyway — JSONL is already durable;
/// a lost final commit costs a rebuild on next open.
pub(crate) const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Await `fut` up to `limit`. Returns `true` if it finished, `false` on timeout.
pub(crate) async fn with_drain_timeout<F>(limit: Duration, fut: F) -> bool
where
    F: std::future::Future<Output = ()>,
{
    match tokio::time::timeout(limit, fut).await {
        Ok(()) => true,
        Err(_elapsed) => {
            tracing::warn!(
                timeout_secs = limit.as_secs(),
                "dreamd watch: drain timed out; exiting without waiting further"
            );
            false
        }
    }
}

/// Shutdown drain for the boot project (AILAB-162). Split out of [`run_watch`]
/// so the ordering contract is unit-testable without binding a socket or
/// spawning a daemon.
///
/// **Order is load-bearing: coordinator first, then index.** `Supervisor::start`
/// is handed `primary_handle.sender()`, so the coordinator task owns a clone of
/// the indexer channel. Draining the coordinator first lets that task exit and
/// release the clone, and puts every append it had queued on the indexer
/// channel ahead of our `Flush` — FIFO then guarantees the commit sees them.
/// Flushing first would commit a batch that the coordinator was still adding
/// to. (An append the coordinator's `try_send` already shed under `Full`
/// backpressure stays shed, and is *not* recovered later: replay filters on
/// `id > watermark`, so a gap followed by any indexed event is skipped
/// forever. That hole is pre-existing — `coordinator.rs` sheds it long before
/// any signal arrives — and closing it is not this ticket.)
///
/// Which index arm runs depends on who still holds the handle.
/// `tokio::select!` drops the losing `serve_uds` future and with it the
/// `router` and its `AppState`, so on an **idle** daemon — the ordinary
/// `systemctl stop` — nothing else owns the `Arc` and `Arc::try_unwrap`
/// succeeds. That arm takes [`TantivyIndexHandle::shutdown`], which drops the
/// sender and awaits the indexer task's own final commit. But `serve_uds`
/// `tokio::spawn`s a task per connection holding a cloned `AppState`, and
/// dropping the serve future does not cancel them; while any is still in
/// flight the unwrap fails, and we fall back to [`TantivyIndexHandle::flush`],
/// which rides the existing `IndexerMsg::Flush` path and awaits the same
/// `commit_and_persist`. `IndexHandle::close` is deliberately not used on
/// either arm: called from inside a runtime it `abort`s the indexer task and
/// loses the in-flight batch.
///
/// Both arms are best-effort — a failure is logged, never propagated. The
/// process is exiting either way, and the JSONL append is already durable; a
/// lost commit costs a rebuild on next open, not data.
pub(crate) async fn drain_daemon(
    supervisor: Arc<Supervisor>,
    primary: Option<Arc<TantivyIndexHandle>>,
) {
    supervisor.drain().await;
    tracing::debug!("dreamd watch: boot coordinator drained");

    let Some(handle) = primary else {
        return;
    };
    match Arc::try_unwrap(handle) {
        Ok(owned) => {
            if let Err(e) = owned.shutdown().await {
                tracing::warn!(error = %e, "dreamd watch: primary index shutdown failed");
            }
        }
        Err(shared) => {
            if let Err(e) = shared.flush().await {
                tracing::warn!(error = %e, "dreamd watch: primary index flush failed");
            }
        }
    }
    tracing::debug!("dreamd watch: primary index drained");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::MemoryCoordinatorMsg;
    use crate::server::tantivy_handle::INDEX_PROGRESS_FILENAME;
    use chrono::{DateTime, Utc};
    use dreamd_protocol::{AgentLearning, EventId};
    use tempfile::tempdir;
    use tokio::sync::oneshot;

    fn sample_learning() -> AgentLearning {
        AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: EventId::parse("evt_01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
            timestamp: DateTime::parse_from_rfc3339("2026-08-18T08:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            pain: 5.0,
            importance: 5.0,
            pinned: false,
            skill_action: "rust.watch.drain".to_string(),
            source_harness: "test-harness".to_string(),
            content: "watch drain body".to_string(),
        }
    }

    #[tokio::test]
    async fn run_watch_rejects_missing_project_root() {
        let dir = tempdir().expect("tempdir");
        // No .agent/ directory — AgentRoot::discover returns Err
        let result = run_watch(dir.path()).await;
        assert!(
            matches!(result, Err(WatchError::NoProjectRoot(_))),
            "expected NoProjectRoot, got: {result:?}",
        );
    }

    #[tokio::test]
    async fn drain_daemon_persists_in_flight_append_and_commits_index() {
        // AC (AILAB-162): this is what `run_watch` runs after the `select!`.
        // An append accepted just before the signal must be on disk AND
        // committed to Tantivy by the time the process exits. Both handles are
        // held behind shared `Arc`s (the prod shape), so `Supervisor::shutdown`
        // is unreachable and `Arc::try_unwrap` on the index handle fails —
        // this drives the `flush` fallback arm.
        let dir = tempdir().expect("tempdir");
        let agent_root = AgentRoot::new(dir.path());
        std::fs::create_dir_all(agent_root.episodic_dir()).expect("episodic dir");

        let handle = Arc::new(
            TantivyIndexHandle::open(&agent_root, DEFAULT_COMMIT_CADENCE).expect("open index"),
        );
        let supervisor = Arc::new(
            Supervisor::start(&agent_root, 8, Some(handle.sender())).expect("start supervisor"),
        );

        // Stand-ins for the clones `build_router` moved into `AppState`; they
        // are what makes `Arc::try_unwrap` fail below.
        let state_supervisor = Arc::clone(&supervisor);
        let state_index = Arc::clone(&handle);

        // Queue an append from a live "connection" sender and do NOT await its
        // response — it must be drained, not raced.
        let client_tx = supervisor.sender();
        let (resp_tx, resp_rx) = oneshot::channel();
        client_tx
            .send(MemoryCoordinatorMsg::AppendLearning {
                learning: sample_learning(),
                client_dedup_key: None,
                response_tx: resp_tx,
            })
            .await
            .expect("send append");

        // Pin the arm under test: with `handle` + `state_index` + the argument
        // all live, `Arc::try_unwrap` cannot succeed, so this drives the
        // `flush` fallback. Without this assertion a future edit that moved
        // `drop(state_index)` up would silently swap arms while the comments
        // still claimed the fallback.
        assert!(
            Arc::strong_count(&handle) > 1,
            "this test must drive the flush fallback arm"
        );
        drain_daemon(Arc::clone(&supervisor), Some(Arc::clone(&handle))).await;

        let minted = resp_rx
            .await
            .expect("append oneshot must be resolved by drain")
            .expect("append must succeed under drain")
            .id;

        // JSONL: the coordinator processed the queued append before exiting.
        let raw = std::fs::read_to_string(agent_root.episodic_jsonl()).expect("read jsonl");
        let lines: Vec<&str> = raw.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 1, "queued append must land on disk");

        // Index: `index_progress.json` is written only after a successful
        // Tantivy commit, so its watermark naming the minted id proves the
        // final commit ran rather than the batch being abandoned.
        let progress =
            std::fs::read_to_string(agent_root.dreamd_dir().join(INDEX_PROGRESS_FILENAME))
                .expect("read index_progress.json");
        assert!(
            progress.contains(minted.as_str()),
            "final commit must advance the watermark to {}; got {progress}",
            minted.as_str()
        );

        drop(client_tx);
        drop(state_supervisor);
        drop(state_index);
    }

    #[tokio::test]
    async fn drain_daemon_sole_owner_takes_shutdown_arm_and_commits_index() {
        // AC (AILAB-162): the arm an ordinary `systemctl stop` on an idle
        // daemon actually takes. `tokio::select!` drops the losing
        // `serve_uds` future along with the router and its `AppState`, so with
        // no live connection task nothing else owns the handle and
        // `Arc::try_unwrap` succeeds — `TantivyIndexHandle::shutdown` runs,
        // dropping the sender and awaiting the indexer's own final commit.
        let dir = tempdir().expect("tempdir");
        let agent_root = AgentRoot::new(dir.path());
        std::fs::create_dir_all(agent_root.episodic_dir()).expect("episodic dir");

        let handle = Arc::new(
            TantivyIndexHandle::open(&agent_root, DEFAULT_COMMIT_CADENCE).expect("open index"),
        );
        let supervisor = Arc::new(
            Supervisor::start(&agent_root, 8, Some(handle.sender())).expect("start supervisor"),
        );

        let client_tx = supervisor.sender();
        let (resp_tx, resp_rx) = oneshot::channel();
        client_tx
            .send(MemoryCoordinatorMsg::AppendLearning {
                learning: sample_learning(),
                client_dedup_key: None,
                response_tx: resp_tx,
            })
            .await
            .expect("send append");
        drop(client_tx);

        // Sole ownership: hand the only `Arc` to the drain.
        assert_eq!(
            Arc::strong_count(&handle),
            1,
            "this test must drive the shutdown arm"
        );
        drain_daemon(Arc::clone(&supervisor), Some(handle)).await;

        let minted = resp_rx
            .await
            .expect("append oneshot must be resolved by drain")
            .expect("append must succeed under drain")
            .id;

        let raw = std::fs::read_to_string(agent_root.episodic_jsonl()).expect("read jsonl");
        assert_eq!(raw.lines().filter(|l| !l.is_empty()).count(), 1);

        let progress =
            std::fs::read_to_string(agent_root.dreamd_dir().join(INDEX_PROGRESS_FILENAME))
                .expect("read index_progress.json");
        assert!(
            progress.contains(minted.as_str()),
            "shutdown arm must run the indexer's final commit; got {progress}"
        );
    }

    #[tokio::test]
    async fn drain_daemon_without_primary_index_drains_coordinator() {
        // `primary` is `None` outside the daemon (in-process MCP fallback).
        // The coordinator must still drain, and the index arm must be a no-op
        // rather than a panic.
        let dir = tempdir().expect("tempdir");
        let agent_root = AgentRoot::new(dir.path());
        std::fs::create_dir_all(agent_root.episodic_dir()).expect("episodic dir");

        let supervisor = Arc::new(Supervisor::start(&agent_root, 8, None).expect("start"));
        let client_tx = supervisor.sender();
        let (resp_tx, resp_rx) = oneshot::channel();
        client_tx
            .send(MemoryCoordinatorMsg::AppendLearning {
                learning: sample_learning(),
                client_dedup_key: None,
                response_tx: resp_tx,
            })
            .await
            .expect("send append");

        drain_daemon(Arc::clone(&supervisor), None).await;

        resp_rx
            .await
            .expect("append oneshot must be resolved by drain")
            .expect("append must succeed under drain");
        let raw = std::fs::read_to_string(agent_root.episodic_jsonl()).expect("read jsonl");
        assert_eq!(raw.lines().filter(|l| !l.is_empty()).count(), 1);
    }

    #[tokio::test]
    async fn run_watch_rejects_dream_mode_auto() {
        let dir = tempdir().expect("tempdir");
        // Create .agent/ so AgentRoot::discover succeeds
        std::fs::create_dir(dir.path().join(".agent")).expect("create .agent");
        // Write config with dream_cycle_mode = "auto"
        std::fs::create_dir_all(dir.path().join(".agent/.dreamd")).expect("create .dreamd");
        std::fs::write(
            dir.path().join(".agent/.dreamd/config.toml"),
            r#"dream_cycle_mode = "auto""#,
        )
        .expect("write config");
        let result = run_watch(dir.path()).await;
        assert!(
            matches!(result, Err(WatchError::DreamMode(_))),
            "expected DreamMode error, got: {result:?}",
        );
    }

    #[tokio::test]
    async fn with_drain_timeout_completes_when_inner_finishes() {
        // AC (AILAB-748): a drain that finishes inside the bound reports so.
        assert!(with_drain_timeout(Duration::from_millis(50), async {}).await);
    }

    #[tokio::test]
    async fn with_drain_timeout_returns_false_when_inner_hangs() {
        // AC (AILAB-748): a drain that outlives the bound is abandoned —
        // `false`, not a hang and not an error.
        let hung = tokio::time::sleep(Duration::from_secs(5));
        assert!(!with_drain_timeout(Duration::from_millis(50), hung).await);
    }
}
