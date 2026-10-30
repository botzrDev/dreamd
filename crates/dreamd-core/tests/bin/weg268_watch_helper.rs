//! Test-only helper binary for the WEG-268 SIGTERM socket-cleanup regression
//! test.
//!
//! Not a shipping binary: `Cargo.toml` flags it `test = false, bench = false,
//! doc = false`, it lives under `tests/bin/`, and no release artifact
//! includes it.
//!
//! Usage: `weg268_watch_helper <project-root>`
//!
//! Runs the real [`dreamd_core::server::run_watch`] foreground daemon against
//! `<project-root>`. The parent test sets `HOME` to an isolated tempdir so the
//! socket binds at `$HOME/.agent/dreamd.sock` instead of the developer's real
//! home; the parent then polls for that socket, delivers SIGTERM, and asserts
//! the socket is gone after the daemon exits.

// AILAB-174: the helper is Unix-only because its test signals the daemon with
// SIGTERM and checks the UDS socket; `run_watch` itself also runs on Windows.
// Items are gated individually and `main` has a `not(unix)` twin, because a
// crate-level `#![cfg(unix)]` strips `main` on Windows and the `[[bin]]` build
// fails with `E0601: main function not found`.
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::process::ExitCode;

#[cfg(unix)]
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: weg268_watch_helper <project-root>");
        return ExitCode::from(64);
    }
    let project_root = PathBuf::from(&args[1]);

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("helper: tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };

    match rt.block_on(dreamd_core::server::run_watch(
        &project_root,
        dreamd_core::server::WatchListen::default(),
    )) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("helper: run_watch error: {e}");
            ExitCode::from(2)
        }
    }
}

/// Non-Unix `main` so the `[[bin]]` target still links (AILAB-174). The only
/// consumer, `tests/weg268_sigterm_socket.rs`, is `#![cfg(unix)]`.
#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("weg268_watch_helper: unix-only test helper");
    std::process::ExitCode::from(64)
}
