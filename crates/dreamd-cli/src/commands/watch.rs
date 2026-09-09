//! `dreamd watch` — foreground daemon mode (WEG-88 / DR-702).
//!
//! Thin CLI wrapper around [`dreamd_core::server::run_watch`]. The watch
//! command boots a per-project Supervisor against `cwd`'s AgentRoot, binds
//! `~/.agent/dreamd.sock`, serves the API router, and blocks until SIGINT.
//!
//! `dreamd watch` is the foreground form of the daemon. OS-level service
//! registration is `dreamd service install`, which supervises this same
//! foreground process (systemd `--user` on Linux, a LaunchAgent on macOS).

use std::path::Path;
use std::process::ExitCode;

#[cfg(unix)]
use dreamd_core::server::{run_watch, WatchError};

/// Entry point for `dreamd watch`.
///
/// Blocks until SIGINT or a fatal server error. Returns exit code 0 on clean
/// shutdown, 2 for missing project root, 1 for all other errors.
#[cfg(unix)]
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

/// Exit code for the non-Unix refusal: a usage refusal, not a crash. Must match
/// `commands::mcp::exit_code_for(&McpRunError::Unsupported)`.
///
/// Declared unconditionally — the `not(unix)` `run` below is its only caller, so
/// it is dead on Unix outside `cfg(test)`, but keeping it cfg-free is what lets
/// the test on Linux pin the value the Windows build actually uses.
#[allow(dead_code)]
const UNSUPPORTED_EXIT: u8 = 2;

/// The exact stderr line the non-Unix [`run`] prints. The copy itself is owned
/// by `McpRunError::Unsupported`, so `dreamd watch` and `dreamd mcp` cannot
/// drift apart. Cfg-free for the same reason as [`UNSUPPORTED_EXIT`].
#[allow(dead_code)]
fn unsupported_line() -> String {
    format!(
        "dreamd watch: {}",
        dreamd_core::mcp::McpRunError::Unsupported
    )
}

/// Non-Unix entry point for `dreamd watch` (AILAB-174).
///
/// `dreamd_core::server` is `#[cfg(unix)]` — the daemon binds a Unix socket and
/// its writes go through [`dreamd_core::io::write_atomic`], which is
/// `ErrorKind::Unsupported` on Windows. There is nothing to run, so this is a
/// usage refusal carrying the same copy as `dreamd mcp`. The Windows daemon is
/// DR-121 / AILAB-203.
#[cfg(not(unix))]
pub fn run(_cwd: &Path) -> ExitCode {
    eprintln!("{}", unsupported_line());
    ExitCode::from(UNSUPPORTED_EXIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the two values the `not(unix)` `run` is built from. That arm is not
    /// compiled on Linux, so this is the closest thing to coverage CI can give
    /// it: change the copy or the code in `run` and this fails, because `run`
    /// has no literals of its own left to drift.
    #[test]
    fn windows_refusal_line_and_exit_code() {
        assert_eq!(
            unsupported_line(),
            "dreamd watch: Windows is not supported in v0.1; use WSL2 or a Linux/macOS host (see docs/windows.md)"
        );
        assert_eq!(UNSUPPORTED_EXIT, 2);
    }

    /// `watch` and `mcp` must refuse with the same code, or the npm shim and the
    /// docs contradict one command or the other.
    #[test]
    fn watch_and_mcp_refuse_with_the_same_exit_code() {
        assert_eq!(
            UNSUPPORTED_EXIT,
            crate::commands::mcp::exit_code_for(&dreamd_core::mcp::McpRunError::Unsupported)
        );
    }
}
