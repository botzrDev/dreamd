//! Axum 0.8 HTTP server — router, state, and request handlers.
//!
//! Submodule layout:
//!   * [`state`] — `AppState`, WEG-272 multi-project routing, lock ordering
//!   * [`router`] — route mounting, middleware, error responses
//!   * [`types`] — re-exports wire shapes from [`crate::ingress`]
//!   * [`handlers`] — `learn`, `recall`, `dream`, `preferences` handlers
//!
//! Out of scope here: TraceLayer (WEG-144), TCP binding (WEG-73),
//! TantivyIndexHandle::reader (WEG-69).

mod handlers;
mod router;
mod state;
mod types;

#[cfg(test)]
mod tests;

pub use router::{agent_root_middleware, build_router, peer_uid_middleware, PeerUid};
pub use state::AppState;
