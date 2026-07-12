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
/// Shared outbound HTTP-over-UDS transport to the dreamd daemon.
#[cfg(unix)]
pub mod daemon_client;
pub mod decay;
pub mod dream_cycle;
pub mod episodic;
pub mod index;
pub mod ingress;
pub mod io;
pub mod layout;
pub mod lessons;
pub mod mcp;
pub mod observability;
pub mod privacy;
pub mod redaction;
pub mod registry;
pub mod salience;
pub use salience::{salience_with_context, RecurrenceContext};
pub mod wal;

pub use collector::{recall, RecallResult, SalienceCollector};

// WEG-21 / DR-118: per-user UDS writer-process lifecycle. Unix-only; the
// `server` submodules guard themselves with `#![cfg(unix)]` where they touch
// `std::os::unix::net` or `nix`. Windows compile path is DR-121, deferred.
#[cfg(unix)]
pub mod server;

pub use layout::{AgentRoot, DaemonHome, LayoutError, DEFAULT_WORKSPACE_MD, GITIGNORE_SNIPPET};
