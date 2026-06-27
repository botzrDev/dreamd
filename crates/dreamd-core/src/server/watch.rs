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
//! Blocks until SIGINT (`ctrl_c`) or SIGTERM, then unlinks `~/.agent/dreamd.sock`.
//! Coordinator drain on shutdown is best-effort in v0.1.

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;

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
    //    below; the Supervisor drops at end of scope, draining the coordinator
    //    channel.
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

    // Unlink the socket on every shutdown path — SIGINT, SIGTERM, or a serve
    // error. Best-effort: the next bind would recover a stale socket anyway
    // (bind_socket_raw), but a service-managed daemon stopped via SIGTERM must
    // not leave ~/.agent/dreamd.sock behind (WEG-268). The full coordinator/
    // indexer drain is WEG-283 (v0.1.1) — out of scope here.
    let _ = std::fs::remove_file(&sock_path);

    outcome.map_err(WatchError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
}
