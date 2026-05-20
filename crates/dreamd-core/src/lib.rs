//! dreamd core engine.
//!
//! Modules land here over Sprint 1-5 (PRD Part IV / `context/AGILE/plan1.md`).
//! DR-101 ships [`layout`] — the typed path-resolution module that every adapter,
//! harness, and CLI command goes through. All other call sites MUST resolve
//! `.agent/` and `~/.agent/` paths via [`AgentRoot`] / [`DaemonHome`] rather
//! than building strings.

pub mod collector;
pub mod config;
pub mod coordinator;
pub mod index;
pub mod io;
pub mod layout;
pub mod lessons;
pub mod mcp;
pub mod privacy;
pub mod redaction;
pub mod registry;
pub mod salience;

pub use collector::{recall, RecallResult, SalienceCollector};

// WEG-21 / DR-118: per-user UDS writer-process lifecycle. Unix-only; the
// `server` submodules guard themselves with `#![cfg(unix)]` where they touch
// `std::os::unix::net` or `nix`. Windows compile path is DR-121, deferred.
#[cfg(unix)]
pub mod server;

pub use layout::{AgentRoot, DaemonHome, LayoutError, DEFAULT_WORKSPACE_MD, GITIGNORE_SNIPPET};
