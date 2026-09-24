//! Writer-process server (WEG-21 / DR-118).
//!
//! Owns the per-user UDS lifecycle. The first process to bind
//! `~/.agent/dreamd.sock` becomes the writer-process; subsequent starts
//! detect the existing socket and connect as clients. Designed so the
//! `npx dreamd-mcp` install funnel stays one-line — no separately-installed
//! daemon, no background install step.
//!
//! Crate placement decision (2026-05-14): UDS binding, double-fork, and the
//! supervisor lifecycle live in `dreamd-core` behind `pub mod server`. No
//! `dreamd-server` crate yet. Re-evaluation trigger is compile pressure or a
//! second Rust binary consumer (see `docs/architecture.md`).
//!
//! Target scope (AILAB-192): this module compiles everywhere. The router,
//! `AppState`, the supervisor and the project maps are transport-agnostic, so
//! Windows reuses them verbatim and serves the same `/api/v1` surface over
//! loopback TCP. Only the *transport* submodules are Unix-only — `uds` and
//! `uds_server` — because they are `std::os::unix::net` plus `chmod 0600` end
//! to end, and `SO_PEERCRED` peer-UID auth has no Windows analogue. [`watch`]
//! spans both: it compiles everywhere and picks the transport per target.
//!
//! Submodule layout:
//!   * [`project_resource_map`] — shared lazy-open LRU + idle-eviction container
//!     (cap 10, 30 min idle) keyed on project root. Index and supervisor maps
//!     are thin adapters over this module.
//!   * [`index_map`] — `IndexHandle` trait + `ProjectIndexMap<H>` adapter.
//!     Production writes use [`tantivy_handle`] (WEG-42); `TestIndexHandle`
//!     remains for eviction + shutdown-drain tests without a live index.
//!   * [`tantivy_handle`] — Tantivy-backed `IndexHandle` (open/flush/close).
//!   * [`indexer_actor`] — the writer-owning indexer task and `IndexerMsg`.
//!   * [`index_freshness`] — the on-disk watermark and `assess_index_freshness`.
//!   * `uds` — **`#[cfg(unix)]`** — bind/connect/cleanup for
//!     `~/.agent/dreamd.sock` with `0600` perms and orphaned-socket recovery.
//!   * `uds_server` — **`#[cfg(unix)]`** — the async `bind_api_socket` wrapper
//!     over `uds` plus the `serve_uds` accept loop that injects `PeerUid`.
//!   * [`lifecycle`] — supervisor (owns `MemoryCoordinator` senders + handle)
//!     and the Unix double-fork helper.
//!   * [`watch`] — the `dreamd watch` foreground daemon, on every target. One
//!     public [`run_watch`] over two whole sequences: Unix binds the UDS and
//!     waits SIGINT/SIGTERM; off Unix [`bind_listen`] takes `127.0.0.1:0` (or
//!     the `--bind` address; non-loopback only with `--insecure`, AILAB-197),
//!     the port is published in `~/.agent/server.json`, [`serve_tcp`] serves the
//!     same router behind the AILAB-203 bearer token, and the wait is `ctrl_c`
//!     alone. That arm opens no Tantivy index — `io::write_atomic` is
//!     `Unsupported` off Unix, so `TantivyIndexHandle::open` cannot run there.
//!
//! (`uds` / `uds_server` are deliberately named without intra-doc links: the
//! modules do not exist on Windows, and a link to a `cfg`-absent item resolves
//! to nothing there — the same reason `http::router` spells `PeerUid` out in
//! its `#[cfg(not(unix))]` arm.)

pub mod http;
pub mod index_freshness;
pub mod index_map;
pub mod indexer_actor;
pub mod lifecycle;
pub mod project_resource_map;
pub mod supervisor_map;
pub mod tantivy_handle;
// AILAB-192: both files also carry a file-level `#![cfg(unix)]`, which is what
// actually removes them off-target. The gate is restated on the declaration so
// the module list reads as the platform contract rather than requiring a jump
// into each file to discover it.
#[cfg(unix)]
pub mod uds;
#[cfg(unix)]
pub mod uds_server;
pub mod watch;

pub use http::{build_router, AppState};
pub use index_map::{IndexError, IndexHandle, ProjectIndexMap, TestIndexHandle};
pub use lifecycle::{
    CoordinatorSendError, ServerConfig, ServerError, Supervisor, COORDINATOR_CHANNEL_CAPACITY,
};
pub use project_resource_map::{ProjectMapResource, ProjectResourceMap, ProjectResourceMapConfig};
pub use supervisor_map::{SupervisorMap, SupervisorMapConfig};
pub use tantivy_handle::{
    assess_index_freshness, read_semantic_pass_record, IndexFreshness, IndexerMsg,
    SemanticPassRecord, TantivyIndexHandle, DEFAULT_COMMIT_CADENCE, INDEX_DIR_NAME,
    INDEX_PROGRESS_FILENAME, SEMANTIC_PASS_FILENAME,
};
// AILAB-192: the UDS surface is re-exported only where `uds` exists. Every
// item here names a `UnixListener` / `UnixStream` in its signature or its error
// type, so there is nothing to widen — Windows gets the loopback-TCP siblings
// instead, not a portable version of these.
#[cfg(unix)]
pub use uds::{
    bind_writer_socket, is_daemon_socket_live, try_connect_existing, SocketGuard, UdsBindError,
};
// Cfg-free as of AILAB-192, in step with `watch.rs` dropping its file-level
// `#![cfg(unix)]`: `dreamd watch` is now a real command on every target, so the
// CLI imports one `run_watch` and one `WatchError` and does no cfg of its own.
// The transport split lives inside the module, not in this re-export.
pub use watch::{run_watch, WatchError, WatchListen};
// The loopback-TCP transport primitives, exported for the same reason the UDS
// bind helpers above are: they are the pair a caller needs to stand the API up
// on a listener it owns. Cfg-free — both are compiled and
// tested on Linux even though only the off-Unix `run_watch` arm ships them
// (AILAB-192), because a `#[cfg(not(unix))]` body no check on this machine can
// reach is how the last round of Windows drift got in.
pub use watch::{bind_listen, bind_loopback, serve_tcp};
