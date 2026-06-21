//! Test-only helper binary for the WEG-268 SIGTERM socket-cleanup regression
//! test.
//!
//! NOT a shipping binary: `Cargo.toml` flags it `test = false, bench = false,
//! doc = false`, and it lives under `tests/bin/`. Compiled only when the
//! dreamd-core test suite is built.
//!
//! Usage: `weg268_watch_helper <project-root>`
//!
//! Runs the real [`dreamd_core::server::run_watch`] foreground daemon against
//! `<project-root>`. The parent test sets `HOME` to an isolated tempdir so the
//! socket binds at `$HOME/.agent/dreamd.sock` instead of the developer's real
//! home; the parent then polls for that socket, delivers SIGTERM, and asserts
//! the socket is gone after the daemon exits.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::ExitCode;

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

    match rt.block_on(dreamd_core::server::run_watch(&project_root)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("helper: run_watch error: {e}");
            ExitCode::from(2)
        }
    }
}
