//! WEG-32 / DR-004 — process-wide tracing baseline.
//!
//! The `tracing` facade and its ~20 macro callsites already exist across the
//! crate; until a subscriber is installed they fire into a no-op. This module
//! installs that subscriber once at CLI startup so those events land somewhere.
//!
//! Two layers:
//!   - **Console → stderr always.** stdout is reserved for the MCP JSON-RPC
//!     channel (`rmcp::transport::stdio`), so logs must never touch it. Pretty
//!     text when stderr is a TTY; JSON when it is not (CI / service-managed
//!     daemon).
//!   - **File → `~/.agent/dreamd.log`,** JSON always, non-blocking, truncated
//!     at startup (rotation is v0.1.1). Resolved via [`DaemonHome::log_file`]
//!     by the caller; this module only consumes the path.
//!
//! Level comes from `DREAMD_LOG` (default `info`). Per-request / peer-UID
//! enrichment is DR-410 (WEG-144), still deferred — no new instrumentation
//! ships here.
//!
//! [`DaemonHome::log_file`]: crate::layout::DaemonHome::log_file

use std::io::IsTerminal;
use std::path::PathBuf;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Initialize the process-wide tracing subscriber.
///
/// Pass the resolved log-file path (normally [`DaemonHome::log_file`]); pass
/// `None` to run console-only. Returns the appender [`WorkerGuard`] whenever a
/// file layer is installed — **the caller MUST hold it for the whole process
/// lifetime** or buffered file logs are dropped on early drop. Returns `None`
/// when running console-only (no path given, or the log dir is not writable —
/// the function degrades to console rather than failing).
///
/// Idempotent: uses `try_init`, so a second call is a silent no-op rather than
/// a panic.
///
/// [`DaemonHome::log_file`]: crate::layout::DaemonHome::log_file
pub fn init_tracing(log_file: Option<PathBuf>) -> Option<WorkerGuard> {
    let filter = EnvFilter::try_from_env("DREAMD_LOG").unwrap_or_else(|_| EnvFilter::new("info"));

    // Console → stderr ALWAYS (stdout is the MCP JSON-RPC channel). Pretty when
    // stderr is a TTY; JSON otherwise. `.boxed()` unifies the two arms.
    let console = fmt::layer().with_writer(std::io::stderr);
    let console = if std::io::stderr().is_terminal() {
        console.boxed()
    } else {
        console.json().boxed()
    };

    // File → JSON, truncate at startup, non-blocking. If the dir is not
    // writable we fall through to the console-only registry below.
    if let Some(path) = log_file {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
        {
            let (non_blocking, guard) = tracing_appender::non_blocking(file);
            let file_layer = fmt::layer()
                .json()
                .with_ansi(false)
                .with_writer(non_blocking);
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(console)
                .with(file_layer)
                .try_init();
            return Some(guard);
        }
    }

    // Console-only fallback (no path given or log dir not writable).
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(console)
        .try_init();
    None
}
