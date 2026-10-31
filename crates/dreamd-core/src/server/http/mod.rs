//! Axum 0.8 HTTP server — router, state, and request handlers.
//!
//! Submodule layout (the submodules are private; their public items are
//! re-exported below):
//!   * `state` — [`AppState`], WEG-272 multi-project routing, lock ordering
//!   * `router` — route mounting, middleware, error responses
//!   * `types` — re-exports wire shapes from [`crate::ingress`]
//!   * `handlers` — the `learn`, `recall`, `dream`, `preferences`, `health`,
//!     `migrate` and `salience` handlers
//!
//! Per-request tracing (peer UID, request id, status, latency) is mounted by
//! [`build_router`] (AILAB-189) — see `http_make_span` there.
//! The transport lives outside this module: `uds_server::serve_uds` on Unix
//! and `watch::serve_tcp` off Unix serve the same router.
//! Recall reads through `TantivyIndexHandle::reader` (WEG-69).

mod handlers;
mod router;
mod state;
mod types;

// `tests.rs` is Unix-only throughout: `nix::unistd::Uid` for every
// `daemon_uid` (`nix` is a Unix-only dependency), `PermissionsExt` on the
// socket mode assertions, live `serve_uds` round trips, and the `PeerUid`
// extension itself.
#[cfg(all(test, unix))]
mod tests;

// `bearer_auth_middleware` is re-exported unconditionally alongside
// `agent_root_middleware`, not gated the way `peer_uid_middleware` below is:
// the fn is `cfg`-free so that Linux type-checks and tests it (AILAB-192), and
// an item that is `pub` inside this private module but re-exported nowhere is
// dead code on every target that does not mount it.
pub use router::{agent_root_middleware, bearer_auth_middleware, build_router};
// `PeerUid` is a `#[cfg(unix)]` type and `peer_uid_middleware` takes it out of
// the request extensions, so neither can be re-exported unconditionally
// (AILAB-192). Windows has no `SO_PEERCRED` to read them from.
#[cfg(unix)]
pub use router::{peer_uid_middleware, PeerUid};
pub use state::AppState;
