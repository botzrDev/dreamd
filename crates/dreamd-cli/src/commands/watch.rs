//! `dreamd watch` — foreground daemon mode (WEG-88 / DR-702).
//!
//! Thin CLI wrapper around [`dreamd_core::server::run_watch`]. One `run` for
//! every target: the watch command boots a per-project Supervisor against
//! `cwd`'s AgentRoot, serves the API router, and blocks until a shutdown
//! signal. Only the transport differs, and that difference lives entirely in
//! core (AILAB-192):
//!
//! - **Unix** binds `~/.agent/dreamd.sock` and authenticates callers by peer
//!   UID (`SO_PEERCRED`); it blocks until SIGINT or SIGTERM.
//! - **Off Unix** there is no UDS and no `SO_PEERCRED`, so `watch` binds
//!   loopback TCP on `127.0.0.1:0`, publishes the bound port to
//!   `~/.agent/server.json` for clients to discover, and requires a bearer
//!   token at `~/.agent/auth.json` — minted by `dreamd service install`
//!   (AILAB-203), never by `watch`. A missing or malformed token file is a
//!   start failure (exit 1), because that token is the listener's only auth.
//!   It blocks until Ctrl-C.
//!
//! `dreamd watch` is the foreground form of the daemon. OS-level service
//! registration is `dreamd service install`, which supervises this same
//! foreground process (systemd `--user` on Linux, a LaunchAgent on macOS).

use std::path::Path;
use std::process::ExitCode;

use dreamd_core::server::{run_watch, WatchError};

/// Entry point for `dreamd watch`.
///
/// Blocks until a shutdown signal or a fatal server error. Returns exit code 0
/// on clean shutdown, otherwise [`exit_code_for`]'s verdict.
pub fn run(cwd: &Path) -> ExitCode {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    match rt.block_on(run_watch(cwd)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dreamd watch: {e}");
            ExitCode::from(exit_code_for(&e))
        }
    }
}

/// Exit code for a watch failure: 2 for "no project root" (a usage refusal —
/// the operator pointed `watch` at a directory with no store, the same class of
/// mistake `dreamd mcp` exits 2 on), 1 for everything else.
///
/// Everything else explicitly includes [`WatchError::AuthToken`] — a missing or
/// malformed `~/.agent/auth.json` on a loopback-TCP target. That is an operator
/// *setup* error to be fixed with `dreamd service install`, not a malformed
/// invocation, so it exits 1 like every other start failure.
///
/// Split out of [`run`] so the mapping is unit-testable without binding a
/// listener or spawning a runtime.
fn exit_code_for(err: &WatchError) -> u8 {
    match err {
        WatchError::NoProjectRoot(_) => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pointing `watch` at a directory with no `.agent/` store is a usage
    /// refusal, and `dreamd mcp` exits 2 on the same class of mistake.
    #[test]
    fn no_project_root_exits_2() {
        assert_eq!(
            exit_code_for(&WatchError::NoProjectRoot("/tmp/nope".to_string())),
            2
        );
    }

    /// A missing/malformed `~/.agent/auth.json` is a setup error, not a usage
    /// error: the invocation was fine, the daemon home is not provisioned yet.
    #[test]
    fn auth_token_error_exits_1() {
        let missing = std::path::Path::new("/nonexistent/dreamd-auth-probe/auth.json");
        let err = dreamd_core::daemon_client::read_auth_token(missing)
            .expect_err("a nonexistent path cannot yield a token");
        assert_eq!(exit_code_for(&WatchError::AuthToken(err.to_string())), 1);
    }

    /// The operator has to be told which command mints the file, or "auth
    /// token: ..." is a dead end. The copy is owned by `AuthTokenError`, so
    /// this pins the sentence `watch` actually prints.
    #[test]
    fn auth_token_error_names_service_install() {
        let missing = std::path::Path::new("/nonexistent/dreamd-auth-probe/auth.json");
        let err = dreamd_core::daemon_client::read_auth_token(missing)
            .expect_err("a nonexistent path cannot yield a token");
        let line = WatchError::AuthToken(err.to_string()).to_string();
        assert!(
            line.contains("dreamd service install"),
            "watch's auth-token failure must name the minting command; got: {line}"
        );
    }

    /// A non-loopback bind is refused at start (AILAB-192 is localhost-only);
    /// that is a runtime failure, not a usage one.
    #[test]
    fn bind_error_exits_1() {
        assert_eq!(
            exit_code_for(&WatchError::Bind(
                "bound 10.0.0.5:1234, not loopback".to_string()
            )),
            1
        );
    }
}
