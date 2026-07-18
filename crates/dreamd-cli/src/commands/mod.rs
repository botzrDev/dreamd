//! CLI subcommand implementations.
//!
//! Dispatch lives in `cli.rs` (clap + exit codes). Prefer `dreamd init` for
//! first-time `.agent/` layout; `doctor` / discover paths diagnose an existing
//! workspace without rewriting golden files.

pub mod archive;
pub mod doctor;
pub mod dream;
pub mod init;
pub mod mcp;
pub mod migrate;
pub mod recall;
pub mod reset;
pub mod score;
pub mod status;
pub mod version;
pub mod watch;
