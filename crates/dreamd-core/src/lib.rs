//! dreamd core engine.
//!
//! Modules land here over Sprint 1-5 (PRD Part IV / `context/AGILE/plan1.md`).
//! DR-101 ships [`layout`] — the typed path-resolution module that every adapter,
//! harness, and CLI command goes through. All other call sites MUST resolve
//! `.agent/` and `~/.agent/` paths via [`AgentRoot`] / [`DaemonHome`] rather
//! than building strings.

pub mod layout;

pub use layout::{AgentRoot, DaemonHome, GITIGNORE_SNIPPET};
