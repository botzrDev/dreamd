//! The daemon's server side: transport, HTTP router, supervisor and the
//! per-project maps (WEG-21).
//!
//! Only a process that holds a project's writer lock
//! (`writer_lock::WriterLock`) starts that project's coordinator. `dreamd
//! watch` binds the per-user `~/.agent/dreamd.sock` and exits when a live
//! daemon already answers on it. `dreamd mcp` proxies to that daemon when one
//! is live and serves in process otherwise, so the `npx dreamd-mcp` install
//! stays one line: no separately installed daemon, no background install step.
//!
//! Crate placement: UDS binding, the HTTP surface and the supervisor live in
//! `dreamd-core` behind `pub mod server`, not in a separate `dreamd-server`
//! crate. Revisit that when compile time grows or a second Rust binary needs
//! them (see `docs/architecture.md`).
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
//!     (cap 10, 30 min idle) keyed on project root.
//!   * [`project_runtime`] — one project's writer lock, index and coordinator,
//!     opened together; the daemon's per-project map holds these.
//!   * [`index_map`] — `IndexHandle` trait + `ProjectIndexMap<H>` adapter.
//!     Production writes use [`tantivy_handle`] (WEG-42); `TestIndexHandle`
//!     remains for eviction + shutdown-drain tests without a live index.
//!   * [`tantivy_handle`] — Tantivy-backed `IndexHandle` (open/flush/close).
//!   * [`indexer_actor`] — the writer-owning indexer task and `IndexerMsg`.
//!   * [`index_freshness`] — the on-disk watermark, `assess_index_freshness`,
//!     and the live marks `/health` reads (BZR-1243).
//!   * [`index_coverage`] — the set difference between the log and the index
//!     (`cover`, `assess_index_coverage`).
//!   * `uds` — **`#[cfg(unix)]`** — bind/connect/cleanup for
//!     `~/.agent/dreamd.sock` with `0600` perms and orphaned-socket recovery.
//!   * `uds_server` — **`#[cfg(unix)]`** — the async `bind_api_socket` wrapper
//!     over `uds` plus the `serve_uds` accept loop that injects `PeerUid`.
//!   * [`lifecycle`] — the supervisor (owns `MemoryCoordinator` senders + handle).
//!   * [`watch`] — the `dreamd watch` foreground daemon, on every target. One
//!     public [`run_watch`] over two whole sequences: Unix binds the UDS and
//!     waits SIGINT/SIGTERM; off Unix [`bind_listen`] takes `127.0.0.1:0` (or
//!     the `--bind` address; non-loopback only with `--insecure`, AILAB-197),
//!     the port is published in `~/.agent/server.json`, [`serve_tcp`] serves the
//!     same router behind the `~/.agent/auth.json` bearer token (AILAB-203),
//!     and the wait is `ctrl_c`
//!     alone. That arm opens the Tantivy index and pins it, the same hand-off
//!     as the Unix arm. `io::write_atomic` runs on Windows; the directory flush
//!     is not a power-loss proof. A mapped segment file stays in the index
//!     directory until the mapping is dropped.
//!
//! (`uds` / `uds_server` are deliberately named without intra-doc links: the
//! modules do not exist on Windows, and a link to a `cfg`-absent item resolves
//! to nothing there — the same reason `http::router` spells `PeerUid` out in
//! its `#[cfg(not(unix))]` arm.)

pub mod http;
pub mod index_coverage;
pub mod index_freshness;
pub mod index_map;
pub mod indexer_actor;
pub mod lifecycle;
pub mod project_resource_map;
pub mod project_runtime;
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
pub use index_coverage::{assess_index_coverage, CoverageReport, IndexCoverage};
pub use index_freshness::{live_index_freshness, AppendMark, CommitMark};
pub use index_map::{IndexError, IndexHandle, ProjectIndexMap, TestIndexHandle};
pub use lifecycle::{CoordinatorSendError, ServerError, Supervisor, COORDINATOR_CHANNEL_CAPACITY};
pub use project_resource_map::{ProjectMapResource, ProjectResourceMap, ProjectResourceMapConfig};
pub use project_runtime::ProjectRuntime;
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
pub use uds::{is_daemon_socket_live, try_connect_existing, UdsBindError};
// Cfg-free: `dreamd watch` runs on every target, so the CLI imports one
// `run_watch` and one `WatchError` and needs no cfg of its own. The transport
// split lives inside the module, not in this re-export.
pub use watch::{run_watch, WatchError, WatchListen};
// The loopback-TCP transport primitives: what a caller needs to stand the API
// up on a listener it owns. Cfg-free: only the off-Unix `run_watch` arm uses
// them, but Linux compiles and tests them too, so a Windows-only body cannot
// drift where no local check reaches it (AILAB-192).
pub use watch::{bind_listen, bind_loopback, serve_tcp};
