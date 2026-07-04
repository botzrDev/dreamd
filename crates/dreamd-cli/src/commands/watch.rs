//! `dreamd watch` — foreground daemon mode (WEG-88 / DR-702).
//!
//! Thin CLI wrapper around [`dreamd_core::server::run_watch`]. The watch
//! command boots a per-project Supervisor against `cwd`'s AgentRoot, binds
//! `~/.agent/dreamd.sock`, serves the API router, and blocks until SIGINT.
//!
//! `dreamd watch` is the foreground form of the daemon — multi-project
//! support and OS-level service registration are v0.1.1 work.

use std::path::Path;
use std::process::ExitCode;

use dreamd_core::server::{run_watch, WatchError};

/// Entry point for `dreamd watch`.
///
/// Blocks until SIGINT or a fatal server error. Returns exit code 0 on clean
/// shutdown, 2 for missing project root, 1 for all other errors.
pub fn run(cwd: &Path) -> ExitCode {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    match rt.block_on(run_watch(cwd)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e @ WatchError::NoProjectRoot(_)) => {
            eprintln!("dreamd watch: {e}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("dreamd watch: {e}");
            ExitCode::from(1)
        }
    }
}
