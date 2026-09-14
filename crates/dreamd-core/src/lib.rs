//! dreamd core engine: episodic actor, Tantivy BM25 index, dream cycle, MCP,
//! and the Unix UDS HTTP server.
//!
//! Path resolution goes through [`layout`] (DR-101). All call sites MUST resolve
//! `.agent/` and `~/.agent/` via [`AgentRoot`] / [`DaemonHome`] rather than
//! building path strings.

pub mod autobiography;
/// HTTP-over-UDS client for CLI → daemon proxying (WEG-271 fast-follow).
#[cfg(unix)]
pub mod client;
pub mod collector;
pub mod config;
pub mod consolidation;
pub mod coordinator;
/// Shared outbound transport to the dreamd daemon.
///
/// Compiled on every target as of AILAB-192. The UDS half (`send_one`,
/// `resolve_daemon_socket`, `SockPathError`) keeps its own `#[cfg(unix)]` gates
/// because it is `tokio::net::UnixStream` end to end; the module is where the
/// loopback-TCP + bearer transport for Windows lands, so the two transports
/// share one error type and one "per-call connect, no pool" shape.
pub mod daemon_client;
pub mod daemon_state;
pub mod decay;
pub mod dream_cycle;
pub mod episodic;
pub mod index;
pub mod ingress;
pub mod io;
pub mod layout;
pub mod lessons;
/// AILAB-204 / DR-304 — LLM client wrapper with deterministic auto-fallback.
pub mod llm;
pub mod mcp;
pub mod migrate;
pub mod observability;
pub mod privacy;
pub mod redaction;
pub mod registry;
pub mod salience;
pub use salience::{salience_with_context, RecurrenceContext};
#[cfg(test)]
mod test_support;
pub mod wal;

pub use collector::{recall, score_by_salience, RecallResult, SalienceCollector};
pub use migrate::{IdentityMigration, MigrateError, Migration, MigrationRegistry};

// WEG-21 / DR-118: per-user writer-process lifecycle. The module graph compiles
// on every target as of AILAB-192 — the axum router, `AppState`, `Supervisor`
// and the project maps are transport-agnostic, and Windows needs all four to
// serve that same router over loopback TCP.
//
// What stays Unix-only is the UDS transport and the peer-credential auth built
// on it: `server::uds` and `server::uds_server` carry file-level
// `#![cfg(unix)]` (they are `std::os::unix::net` + `chmod 0600` end to end),
// and `PeerUid` / `peer_uid_middleware` are `#[cfg(unix)]` re-exports because
// `SO_PEERCRED` has no Windows analogue. `server::watch` is cfg-free and
// dispatches: Unix binds the socket, every other target binds loopback TCP on
// `127.0.0.1:0`, publishes the port to `~/.agent/server.json`, and puts a
// bearer layer from `~/.agent/auth.json` in the slot peer-UID holds on Unix.
pub mod server;

pub use layout::{AgentRoot, DaemonHome, LayoutError, DEFAULT_WORKSPACE_MD, GITIGNORE_SNIPPET};
