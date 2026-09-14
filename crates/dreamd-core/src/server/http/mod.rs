//! Axum 0.8 HTTP server — router, state, and request handlers.
//!
//! Submodule layout:
//!   * [`state`] — `AppState`, WEG-272 multi-project routing, lock ordering
//!   * [`router`] — route mounting, middleware, error responses
//!   * [`types`] — re-exports wire shapes from [`crate::ingress`]
//!   * [`handlers`] — `learn`, `recall`, `dream`, `preferences` handlers
//!
//! Per-request tracing (peer UID, request id, status, latency) is mounted by
//! [`router::build_router`] as of AILAB-189 — see `http_make_span` there.
//! Out of scope here: TCP binding (WEG-73).
//! Recall already goes through `TantivyIndexHandle::reader` (WEG-69).

mod handlers;
mod router;
mod state;
mod types;

// AILAB-192: `tests.rs` is Unix-shaped from top to bottom — `nix::unistd::Uid`
// for every `daemon_uid`, `PermissionsExt` on the socket mode assertions, live
// `serve_uds` round trips, and the `PeerUid` extension itself. Gating it to
// `all(test, unix)` preserves exactly today's behaviour: the whole `server`
// module was `#[cfg(unix)]` before this ticket, so these tests never compiled
// off-target anyway. Linux still runs every one of them, which is where the
// AILAB-192 proofs live.
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
