//! Foreground daemon: `dreamd watch` (WEG-88 / DR-702).
//!
//! Two transports, one router (AILAB-192). Unix serves the `/api/v1` surface
//! over `~/.agent/dreamd.sock` and authenticates callers by peer UID
//! (`SO_PEERCRED`). Off Unix there is neither a UDS nor a peer credential, so the
//! *same* router is served over a loopback TCP listener: the port is published in
//! `~/.agent/server.json` and the auth is the `Authorization: Bearer` token that
//! `dreamd service install` (AILAB-203) minted into `~/.agent/auth.json`. Nothing
//! about the router, `AppState`, the supervisor or the project maps differs
//! between the two — only the listener, the auth layer and the shutdown signal.
//!
//! ## Actor topology
//!
//! ```text
//! run_watch
//!  ├─ #[cfg(unix)] run_watch_uds
//!  │   ├─ Supervisor (boot project) ──► MemoryCoordinator actor
//!  │   │     └─ indexer_tx ──► TantivyIndexHandle (pinned "primary")
//!  │   ├─ AppState.supervisor_map ──► lazy per-project Supervisors (WEG-272)
//!  │   ├─ AppState.index_map ──► lazy Tantivy handles for non-boot projects
//!  │   └─ serve_uds ──► HTTP router (peer UID + X-Agent-Root middleware)
//!  └─ #[cfg(not(unix))] run_watch_loopback
//!      ├─ Supervisor (boot project) ──► MemoryCoordinator actor (no indexer_tx)
//!      ├─ AppState.supervisor_map ──► lazy per-project Supervisors (WEG-272)
//!      ├─ AppState.index_map ──► empty; nothing on this path opens Tantivy
//!      └─ serve_tcp ──► HTTP router (bearer + X-Agent-Root middleware)
//! ```
//!
//! The **primary handle** is handed off at boot so the coordinator's live appends
//! and recall/dream read the same Tantivy index (one `IndexWriter` per dir). The
//! loopback arm has no primary and no index at all: [`TantivyIndexHandle::open`] is never
//! called there, because it writes the index manifest through
//! [`io::write_atomic`](crate::io::write_atomic), which is
//! `ErrorKind::Unsupported` off Unix — a watch that opened the index could not boot.
//! JSONL appends are still durable there; index-backed recall and the dream cycle
//! are not available (see `docs/windows.md`).
//!
//! ## Signal handling
//!
//! Unix blocks until SIGINT (`ctrl_c`) or SIGTERM, then unlinks
//! `~/.agent/dreamd.sock`. The loopback arm blocks on `ctrl_c` alone —
//! `tokio::signal::unix` and its `SignalKind` do not exist off Unix, so there is
//! no SIGTERM to wait for — and unlinks `~/.agent/server.json` instead of the
//! socket. Both then drain the boot coordinator (and, on Unix, the primary
//! index), bounded by [`DRAIN_TIMEOUT`] — see [`drain_daemon`]. Lazily-opened
//! per-project coordinators and index handles are not touched on either path;
//! eviction reaps them on drop (AILAB-306).

use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

// `SignalKind` is the SIGTERM half of the Unix shutdown wait and has no off-Unix
// equivalent, so the import is gated exactly like the arm that uses it.
#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};

use crate::config::{load_config, DreamCycleMode};
use crate::layout::{AgentRoot, DaemonHome};
use crate::server::build_router;
use crate::server::http::AppState;
use crate::server::index_map::{ProjectIndexMap, ProjectIndexMapConfig};
use crate::server::lifecycle::{ServerError, Supervisor, COORDINATOR_CHANNEL_CAPACITY};
// `TantivyIndexHandle` is cfg-free here because it is still the `index_map` and
// `drain_daemon` type parameter on every target; only `DEFAULT_COMMIT_CADENCE`
// and the `open` call that consumes it are Unix-only.
use crate::server::tantivy_handle::TantivyIndexHandle;
#[cfg(unix)]
use crate::server::tantivy_handle::DEFAULT_COMMIT_CADENCE;
// `uds_server` itself is `#[cfg(unix)]`; both items name a `UnixListener`.
#[cfg(unix)]
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
    /// `~/.agent/auth.json` is missing, unreadable, or not the AILAB-203 wire
    /// shape (loopback-TCP targets only). A start failure rather than a warning:
    /// the bearer token is that listener's only auth, so booting without it
    /// would mean serving the API to anything that can reach localhost. The
    /// payload is [`AuthTokenError`](crate::daemon_client::AuthTokenError)'s own
    /// `Display`, which names `dreamd service install` — the command that mints
    /// the file — and never echoes the file's contents.
    #[error("auth token: {0}")]
    AuthToken(String),
    /// The listener came up on an address that is not loopback. AILAB-192 is
    /// localhost-only by construction and there is deliberately no flag, env var
    /// or config key that asks for anything else; a non-localhost bind is
    /// AILAB-197.
    #[error("bind: {0}")]
    Bind(String),
}

/// Boot a per-project daemon in the foreground and block until a shutdown
/// signal. Called by `dreamd watch`.
///
/// A dispatcher, not a body: the transports differ enough (listener type, auth
/// layer, whether Tantivy can be opened at all, which signals exist) that one
/// function with cfg branches sprinkled through it would be less honest than two
/// whole sequences. Unix runs [`run_watch_uds`] and off Unix runs
/// `run_watch_loopback` — see the module docs for the two topologies.
pub async fn run_watch(cwd: &Path) -> Result<(), WatchError> {
    #[cfg(unix)]
    {
        run_watch_uds(cwd).await
    }
    #[cfg(not(unix))]
    {
        run_watch_loopback(cwd).await
    }
}

/// Boot a per-project daemon in the foreground, binding `~/.agent/dreamd.sock`,
/// and block until SIGINT/SIGTERM. The Unix half of [`run_watch`].
///
/// The sequence is: discover AgentRoot → load config + WEG-66 guard →
/// WAL recovery → boot Supervisor → compose AppState → bind socket → serve
/// until a shutdown signal (SIGINT/SIGTERM), then unlink the socket.
#[cfg(unix)]
async fn run_watch_uds(cwd: &Path) -> Result<(), WatchError> {
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

/// Boot a per-project daemon in the foreground on a loopback TCP listener,
/// publish the port at `~/.agent/server.json`, and block until Ctrl-C. The
/// off-Unix half of [`run_watch`] (AILAB-192).
///
/// The sequence is: discover AgentRoot → load config + WEG-66 guard → WAL
/// recovery → resolve the daemon home → require `~/.agent/auth.json` → boot
/// Supervisor (no indexer) → compose AppState → bind `127.0.0.1:0` → publish
/// `server.json` → serve until Ctrl-C → unlink `server.json` → drain.
///
/// Two departures from [`run_watch_uds`] are load-bearing, not incidental:
///
/// * **No Tantivy.** [`TantivyIndexHandle::open`] is never called here. It writes
///   the index manifest through [`io::write_atomic`](crate::io::write_atomic), which is
///   `ErrorKind::Unsupported` off Unix, so opening the index would fail the boot
///   outright rather than degrade it. `AppState` therefore gets a fresh empty
///   `ProjectIndexMap` and no pinned primary, the coordinator is started with no
///   `indexer_tx`, and the shutdown drain gets `None` for the index. JSONL
///   appends are durable; index-backed recall and the dream cycle are not
///   available on this target (`docs/windows.md`).
/// * **`auth.json` is read, never written.** The bearer token is minted by
///   `dreamd service install` and rotated by that command's `--force`; a watch
///   that minted one would silently invalidate every client holding the old
///   credential on every restart. This function only checks that a readable,
///   well-formed token exists, and does it **before** binding anything, so "no
///   credential" is a clean start failure instead of a listener nobody can
///   authenticate against.
///
/// ## Why this is compiled on Linux
///
/// Gated `#[cfg(any(test, not(unix)))]`, not plain `#[cfg(not(unix))]`. Windows
/// cannot be compiled in this repo's environment (a third-party C build script in
/// the dependency graph needs an MSVC toolchain), and a `#[cfg(not(unix))]` body
/// is invisible to every check that guards `main` — the exact drift trap
/// `cfg(not(unix))` dead code has produced here before. Under
/// `any(test, not(unix))`, `cargo test` and `cargo clippy --all-targets` on Linux
/// type-check this body against the real `AppState`, `Supervisor`, router and
/// `daemon_client` APIs, while a release Linux build still contains no TCP watch
/// path at all. It is unreachable from Unix: [`run_watch`] dispatches to
/// [`run_watch_uds`] there, which is why the `all(unix, test)` build allows the
/// dead code rather than inventing a caller for it.
#[cfg(any(test, not(unix)))]
#[cfg_attr(all(unix, test), allow(dead_code))]
async fn run_watch_loopback(cwd: &Path) -> Result<(), WatchError> {
    // Function-local: in a Unix release build this fn does not exist, and a
    // top-level import only it uses would be an `unused_imports` warning there.
    use crate::daemon_client::read_auth_token;

    // 1. Discover project root from cwd (CLI maps this to exit 2).
    let agent_root = AgentRoot::discover(cwd)
        .map_err(|_| WatchError::NoProjectRoot(cwd.display().to_string()))?;

    // 2. Load config + WEG-66 startup guard (rejects DreamCycleMode::Auto in
    //    v0.1) — the same refusal as Unix, because the guard is about the
    //    v0.1 feature set, not about the transport.
    let config =
        load_config(agent_root.project_root()).map_err(|e| WatchError::Config(e.to_string()))?;
    if config.dream_cycle_mode == DreamCycleMode::Auto {
        return Err(WatchError::DreamMode(
            "dream_cycle_mode = \"auto\" is not supported in v0.1 \
             (LLM mode ships in v0.1.1)"
                .into(),
        ));
    }

    // 2.5. Recover any stale dream-cycle WAL before opening coordinators. The
    //      error is propagated rather than logged and swallowed: recovery
    //      restores a store whose consolidation was interrupted mid-cycle, and
    //      on this target it can legitimately fail (`write_atomic` is
    //      `Unsupported`), so continuing past it would serve a half-applied
    //      store as if it were intact.
    crate::wal::recover_on_startup(&agent_root)?;

    // 3. Resolve the daemon home. `dirs::home_dir()`, exactly as the Unix arm
    //    does — the `USERPROFILE` fallback belongs to the CLI's `service` verbs,
    //    which have to resolve a home for a scheduled task's environment; a
    //    foreground watch inherits the user's own.
    let home = dirs::home_dir().ok_or_else(|| {
        WatchError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "could not determine home directory",
        ))
    })?;
    let daemon_home = DaemonHome::new(home.join(".agent"));

    // 4. Require the bearer credential BEFORE binding anything. This listener
    //    has no `SO_PEERCRED` to fall back on, so a missing or malformed
    //    `auth.json` is a start failure (the CLI maps it to exit 1 — an operator
    //    setup error, not a usage error). The token is read and dropped on
    //    purpose: `bearer_auth_middleware` re-reads the file on every request so
    //    a `--force` rotation takes effect without a restart, and nothing here
    //    caches, logs, mints or rewrites it.
    read_auth_token(&daemon_home.auth_json()).map_err(|e| WatchError::AuthToken(e.to_string()))?;

    // 5. Boot the project coordinator with no indexer. `Supervisor::start`'s
    //    third parameter is `#[cfg(unix)]`, so off Unix the call takes two
    //    arguments; under `cfg(test)` on Unix it takes three and is handed
    //    `None`, which is the same "no indexer" wiring this target ships. Either
    //    way no Tantivy handle is opened anywhere on this path.
    let supervisor = Supervisor::start(
        &agent_root,
        COORDINATOR_CHANNEL_CAPACITY,
        #[cfg(unix)]
        None,
    )?;

    // 6. Compose AppState. `daemon_uid` is 0 and never read: `peer_uid_middleware`
    //    is not mounted on this router (TCP carries no peer credential), and
    //    `nix` is a `cfg(unix)` dependency, so `Uid::current()` does not even
    //    exist here to ask. `index_map` starts empty and stays empty — nothing
    //    on this path opens a handle — and there is no `with_primary`.
    //    `with_auth_json` is mandatory rather than optional: `auth_json: None`
    //    is fail-closed, and a router built without it 401s every request.
    let index_map = ProjectIndexMap::<TantivyIndexHandle>::new(ProjectIndexMapConfig::default());
    let state = AppState::new(
        daemon_home.registry_toml(),
        supervisor,
        config,
        index_map,
        0,
    )
    .with_auth_json(daemon_home.auth_json());
    // Retain the shutdown handle BEFORE `state` moves into `build_router`
    // (AILAB-162). After the move the only owners are the router and its live
    // connection tasks, so there would be nothing left here to drain. There is
    // no index handle to retain on this arm — see this function's docs.
    let drain_supervisor = Arc::clone(&state.supervisor);
    let router = build_router(state);

    // 7. Bind loopback, then publish the port the kernel actually gave us.
    //    Order matters: publishing first would advertise a port nothing is
    //    listening on, and a client that read the file in that window would get
    //    a refused connect and conclude the daemon is down. Once the bind has
    //    returned the address is real — a connect that arrives before
    //    `serve_tcp` reaches its accept loop waits in the kernel's backlog
    //    instead of failing.
    let listener = bind_loopback().await?;
    let addr = listener.local_addr()?;
    let server_json = daemon_home.server_json();
    write_server_json(&server_json, addr.port())?;

    // The bound address and the project root, never the token.
    tracing::info!(
        "dreamd watch: serving on http://{} (project: {})",
        addr,
        agent_root.project_root().display(),
    );

    // 8. Serve until Ctrl-C. There is no SIGTERM half to this `select!`:
    //    `tokio::signal::unix` — and with it `SignalKind` — does not exist off
    //    Unix, so `ctrl_c()` is the whole signal set this arm handles.
    //    `tokio::signal::windows` additionally offers `ctrl_close` /
    //    `ctrl_shutdown`, which an abrupt service stop delivers instead of
    //    CTRL_C; wiring those is out of scope for AILAB-192. The cost of an
    //    unhandled stop is a stale `server.json`: the next start overwrites it,
    //    and until then every liveness probe fails its TCP connect and reports
    //    the daemon down, which is the truth.
    let outcome = tokio::select! {
        result = serve_tcp(listener, router) => result,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("dreamd watch: Ctrl-C received, shutting down");
            Ok(())
        }
    };

    // 9. Unlink `server.json` FIRST, on every shutdown path — Ctrl-C, a clean
    //    serve return, or a serve error. This is the sibling of the socket
    //    unlink in `run_watch_uds` and the ordering argument there transfers
    //    unchanged: the drain below is bounded by DRAIN_TIMEOUT (AILAB-748), a
    //    `Shutdown` queued behind an in-progress cycle can consume that whole
    //    budget, and a supervisor that kills the process on its own stop timeout
    //    would then strand a file advertising the port of a daemon that is gone.
    //    A client that reads it in the window between the two gets a refused
    //    connect instead of a 503, the better of the two failures.
    remove_server_json(&server_json);

    // 10. Drain the boot coordinator (AILAB-162), `None` for the index because
    //     nothing on this path ever opened one. Appends already accepted are
    //     durable only once the coordinator has processed them, so returning
    //     without draining would drop whatever it still had queued.
    let _ = with_drain_timeout(DRAIN_TIMEOUT, drain_daemon(drain_supervisor, None)).await;

    outcome.map_err(WatchError::from)
}

/// Bind the API listener on the loopback interface, ephemeral port — the
/// address `run_watch_loopback` serves and publishes in `~/.agent/server.json`.
///
/// Cfg-free on purpose (AILAB-192): a `#[cfg(not(unix))]` body would never be
/// compiled by any check that guards this repo's `main`, so the bind that
/// Windows runs is written once here and exercised on Linux by the tests below.
///
/// The bound address is read back off the listener and refused unless it is
/// loopback. `Ipv4Addr::LOCALHOST` cannot resolve to anything else today, so the
/// check guards against a future edit widening the requested address rather than
/// against the kernel — it fails the start instead of quietly exposing the API on
/// a routable interface. Serving a non-loopback address is AILAB-197; there is no
/// flag, env var or config key here that asks for it.
///
/// # Errors
///
/// [`WatchError::Io`] if the bind or the `local_addr` lookup fails;
/// [`WatchError::Bind`] if the bound address is not loopback.
pub async fn bind_loopback() -> Result<tokio::net::TcpListener, WatchError> {
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?;
    let addr = listener.local_addr()?;
    if !addr.ip().is_loopback() {
        return Err(WatchError::Bind(format!(
            "refusing to serve the API on non-loopback address {addr}"
        )));
    }
    Ok(listener)
}

/// Serve `router` on an already-bound TCP listener. The sibling of `serve_uds`,
/// and cfg-free for the same reason [`bind_loopback`] is.
///
/// `axum::serve` owns the accept loop here where the UDS path hand-rolls one:
/// that loop exists only to read `SO_PEERCRED` and inject `PeerUid` per
/// connection, and a TCP connection carries no peer credential to read. Auth on
/// this transport lives inside the router instead, as `bearer_auth_middleware`.
pub async fn serve_tcp(
    listener: tokio::net::TcpListener,
    router: axum::Router,
) -> std::io::Result<()> {
    axum::serve(listener, router).await
}

// The three `server.json` helpers below are cfg-free so Linux compiles and tests
// the bytes Windows writes. In a Unix release build nothing calls them —
// `run_watch_loopback` is absent there — hence the narrow `dead_code` allowance
// rather than a `cfg` that would take them out of the Linux type-check too.

/// The exact bytes of `~/.agent/server.json`: one JSON object, one trailing
/// newline.
///
/// A wire contract, not a formatting preference — `daemon_client::read_server_json`
/// parses this and the CLI's `status` / `archive` liveness probes dial the port it
/// names — so it is hand-written and pinned by a byte-exact test instead of left
/// to a serializer's field order. `host` is the literal `127.0.0.1` the daemon
/// binds; the reader re-checks `is_loopback()` anyway and refuses anything else.
#[cfg_attr(all(unix, not(test)), allow(dead_code))]
pub(crate) fn server_json_body(port: u16) -> String {
    format!("{{\"host\":\"127.0.0.1\",\"port\":{port}}}\n")
}

/// Publish the bound port at `path` (`DaemonHome::server_json()`), creating
/// `~/.agent/` if this is the first start.
///
/// `std::fs::create_dir_all` + `std::fs::write`, deliberately **not**
/// [`io::write_atomic`](crate::io::write_atomic): that helper is
/// `ErrorKind::Unsupported` off Unix (AILAB-192 does not implement it), so a
/// watch that used it could never publish its address at all. The trade is
/// acceptable here and only here — this file is a discardable address hint the
/// daemon rewrites on every bind and unlinks on every stop, not memory state, so
/// the worst a torn write costs is one `read_server_json` failure (reported as
/// `Unreachable`) and a restart, never a lost learning. `create_dir_all` sets no
/// mode bits: `0700` on `~/.agent/` is the UDS path's business (`bind_api_socket`)
/// and `PermissionsExt` does not exist off Unix. The port is not a secret; the
/// token beside it is, and this function never touches `auth.json`.
#[cfg_attr(all(unix, not(test)), allow(dead_code))]
pub(crate) fn write_server_json(path: &Path, port: u16) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, server_json_body(port))
}

/// Best-effort unlink of `~/.agent/server.json`. Never fails: it runs on every
/// shutdown path, including one where the publish never happened, and "the file
/// is gone" is the desired end state either way.
#[cfg_attr(all(unix, not(test)), allow(dead_code))]
pub(crate) fn remove_server_json(path: &Path) {
    let _ = std::fs::remove_file(path);
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
/// to. The coordinator now *awaits* its indexer `send`, so nothing is shed on a
/// full channel and everything it queued is genuinely ahead of the `Flush`.
/// That await also couples the two phases: [`Supervisor::drain`] sends
/// `Shutdown` and waits for the ack with no deadline of its own, so a
/// coordinator parked on a full indexer channel can spend the entire
/// [`DRAIN_TIMEOUT`] budget that [`with_drain_timeout`] wraps this call in,
/// leaving the index flush below unrun — best-effort as ever: a lost commit
/// costs a rebuild on next open, not data.
///
/// `primary` is `None` on two paths: outside the daemon (in-process MCP
/// fallback) and on the loopback-TCP watch, which never opens an index at all
/// (AILAB-192). The index phase is then a no-op and the coordinator drain still
/// runs — the JSONL half is the durable half.
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
    use tempfile::tempdir;

    // AILAB-192: the imports and the fixture below are read only by the
    // `drain_daemon_*` tests, which open a real `TantivyIndexHandle` and pass
    // `Supervisor::start`'s `#[cfg(unix)]` `indexer_tx`. They are gated together
    // with those tests so nothing goes unused off-target. Nothing about what
    // Linux runs changes — this whole module was `#![cfg(unix)]` until AILAB-192,
    // and Linux is where every proof in this ticket runs.
    #[cfg(unix)]
    use crate::coordinator::MemoryCoordinatorMsg;
    #[cfg(unix)]
    use crate::server::tantivy_handle::INDEX_PROGRESS_FILENAME;
    #[cfg(unix)]
    use chrono::{DateTime, Utc};
    #[cfg(unix)]
    use dreamd_protocol::{AgentLearning, EventId};
    #[cfg(unix)]
    use tokio::sync::oneshot;

    #[cfg(unix)]
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

    /// One HTTP/1 GET over a real `TcpStream`, mirroring
    /// `daemon_client::send_one_tcp`'s shape (per-call connect, `TokioIo` wrap,
    /// spawned connection task) without calling it — `serve_tcp` is what is under
    /// test here, so the client must not be.
    async fn get_once(addr: SocketAddr, path: &str) -> (hyper::StatusCode, bytes::Bytes) {
        use http_body_util::{BodyExt, Full};
        use hyper::client::conn::http1;
        use hyper_util::rt::TokioIo;

        let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let (mut sender, conn) = http1::handshake(TokioIo::new(stream))
            .await
            .expect("handshake");
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let req = hyper::Request::builder()
            .method(hyper::Method::GET)
            .uri(path)
            .header(hyper::header::HOST, "localhost")
            .body(Full::new(bytes::Bytes::new()))
            .expect("request");
        let resp = sender.send_request(req).await.expect("send request");
        let status = resp.status();
        let body = resp
            .into_body()
            .collect()
            .await
            .expect("collect body")
            .to_bytes();
        (status, body)
    }

    #[tokio::test]
    async fn serve_tcp_round_trips_on_loopback() {
        // AC (AILAB-192): the transport Windows serves, proved on Linux. Bind
        // through the production helper, serve with the production `serve_tcp`,
        // and drive a real HTTP/1 request over TCP end to end.
        let listener = bind_loopback().await.expect("bind loopback");
        let addr = listener.local_addr().expect("local_addr");
        assert!(
            addr.ip().is_loopback(),
            "bind_loopback must never come up on a routable address"
        );
        assert_eq!(
            addr.ip(),
            std::net::IpAddr::V4(Ipv4Addr::LOCALHOST),
            "the bound host must be the same 127.0.0.1 server.json publishes"
        );

        let router = axum::Router::new().route("/ping", axum::routing::get(|| async { "pong" }));
        let served = tokio::spawn(serve_tcp(listener, router));

        let (status, body) = get_once(addr, "/ping").await;
        assert_eq!(status, hyper::StatusCode::OK);
        assert_eq!(body.as_ref(), b"pong");

        served.abort();
    }

    #[test]
    fn server_json_body_is_the_locked_wire_shape() {
        // AC (AILAB-192): byte-exact. `daemon_client::read_server_json` parses
        // these bytes and the CLI liveness probes dial the port they name, so the
        // trailing newline and the field order are part of the contract.
        assert_eq!(
            server_json_body(54321),
            "{\"host\":\"127.0.0.1\",\"port\":54321}\n"
        );
    }

    #[test]
    fn write_server_json_creates_the_parent_and_writes_the_body() {
        // AC (AILAB-192): on a first start `~/.agent/` may not exist yet, and
        // nothing else on the loopback path creates it — there is no
        // `bind_api_socket` here to do it as a side effect.
        let dir = tempdir().expect("tempdir");
        let path = dir
            .path()
            .join("does-not-exist-yet")
            .join("nor-this")
            .join("server.json");
        write_server_json(&path, 4242).expect("write server.json");

        let raw = std::fs::read(&path).expect("read server.json");
        assert_eq!(raw, server_json_body(4242).into_bytes());
        assert_eq!(
            String::from_utf8(raw).expect("utf8"),
            "{\"host\":\"127.0.0.1\",\"port\":4242}\n"
        );
    }

    #[test]
    fn remove_server_json_is_idempotent() {
        // AC (AILAB-192): it runs on every shutdown path, including one that
        // never got as far as publishing, so "already gone" must be a no-op and
        // not an error.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("server.json");
        write_server_json(&path, 1234).expect("write server.json");
        assert!(path.exists());

        remove_server_json(&path);
        assert!(!path.exists());
        remove_server_json(&path);
        assert!(!path.exists(), "a second unlink must stay a no-op");
    }

    #[tokio::test]
    async fn bind_loopback_uses_an_ephemeral_port() {
        // AC (AILAB-192): port 0 is the *request*. `local_addr` must report the
        // port the kernel assigned, because that number is what goes into
        // `server.json` — and `read_server_json` rejects a 0 there.
        let listener = bind_loopback().await.expect("bind loopback");
        let addr = listener.local_addr().expect("local_addr");
        assert_ne!(addr.port(), 0, "server.json must never publish port 0");
        assert!(addr.ip().is_loopback());
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
    #[tokio::test]
    async fn drain_daemon_without_primary_index_drains_coordinator() {
        // `primary` is `None` outside the daemon (in-process MCP fallback) and
        // on the loopback-TCP watch, which never opens an index (AILAB-192).
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
