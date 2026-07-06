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
//! `dreamd-server` crate yet. Re-evaluation trigger is WEG-42 compile pressure
//! or a second Rust binary consumer (see `docs/architecture.md`).
//!
//! Submodule layout:
//!   * [`project_resource_map`] — shared lazy-open LRU + idle-eviction container
//!     (cap 10, 30 min idle) keyed on project root. Index and supervisor maps
//!     are thin adapters over this module.
//!   * [`index_map`] — `IndexHandle` trait + `ProjectIndexMap<H>` adapter. Real
//!     Tantivy-backed handle lands in WEG-42; we ship `TestIndexHandle` here
//!     so the eviction + shutdown-drain tests can run without `tantivy`.
//!   * [`uds`] — bind/connect/cleanup for `~/.agent/dreamd.sock` with `0600`
//!     perms and orphaned-socket recovery.
//!   * [`lifecycle`] — supervisor (owns `MemoryCoordinator` senders + handle)
//!     and the Unix double-fork helper.

pub mod http;
pub mod index_map;
pub mod lifecycle;
pub mod project_resource_map;
pub mod supervisor_map;
pub mod tantivy_handle;
pub mod uds;
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
    assess_index_freshness, IndexFreshness, IndexerMsg, TantivyIndexHandle, DEFAULT_COMMIT_CADENCE,
};
pub use uds::{
    bind_writer_socket, is_daemon_socket_live, try_connect_existing, SocketGuard, UdsBindError,
};
#[cfg(unix)]
pub use watch::{run_watch, WatchError};
