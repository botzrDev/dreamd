//! Process-wide tracing baseline (WEG-32).
//!
//! The crate logs through the `tracing` facade; until a subscriber is
//! installed those events fire into a no-op. This module installs that
//! subscriber once at CLI startup so they land somewhere.
//!
//! Two layers, each with its own filter:
//!   - **Console → stderr always.** stdout is reserved for the MCP JSON-RPC
//!     channel (`rmcp::transport::stdio`), so logs must never touch it. The
//!     format is readable text whether or not stderr is a terminal, with ANSI
//!     colour only on a terminal. `DREAMD_LOG_FORMAT=json` (exactly `json`)
//!     prints one JSON object per line instead (BZR-1470 D1).
//!   - **File → the daemon log,** JSON always, non-blocking, appended to and
//!     never truncated. Only the daemon (`dreamd watch`, AILAB-184) has one —
//!     see `cli::wants_daemon_log`, which owns that policy. It installs
//!     [`init_daemon_tracing`], whose file layer holds its lines in memory
//!     until `watch` has bound its socket and calls [`open_daemon_log`]
//!     (DRM-119). So a `watch` that stops at the writer lock, the config or the
//!     bind never touches the file a running daemon writes. At that open, a
//!     file of [`DAEMON_LOG_ROTATE_BYTES`] or more is renamed to `<log>.1`, and
//!     the file is mode `0600`. The path is [`DaemonHome::log_file_for`] the
//!     bound socket: `~/.agent/dreamd.log` for the default socket, else
//!     `<socket>.log`. Every other process calls `init_tracing(None)` and is
//!     console-only.
//!
//! Filters come from `DREAMD_LOG`. When it is unset, empty, invalid or not
//! UTF-8, each layer gets its own default (BZR-1470 D2, D3, D4):
//!   - [`CONSOLE_DEFAULT_FILTER`]: dreamd's own targets at INFO, every other
//!     target (tantivy, rmcp, hyper, tower-http, …) held to WARN, and the
//!     per-request `http` span and `http request` event left to the file;
//!   - [`FILE_DEFAULT_FILTER`]: the same, but it keeps one `http request` line
//!     per request.
//!
//! A valid non-empty `DREAMD_LOG` replaces both defaults and is used as written
//! for both layers: `DREAMD_LOG=info` shows library INFO and `watch`'s request
//! lines on the console, and `DREAMD_LOG=off` silences both layers.
//!
//! Per-request / peer-UID enrichment (AILAB-189) is a router concern, not a
//! subscriber one: it ships as a `TraceLayer` in [`server::http::router`],
//! which opens an INFO span per HTTP request and emits one INFO event into it.
//! This module installs no instrumentation of its own — it only provides the
//! subscriber those spans land in, and the file default is what keeps them in
//! `dreamd.log`.
//!
//! [`server::http::router`]: crate::server::http
//!
//! [`DaemonHome::log_file_for`]: crate::layout::DaemonHome::log_file_for

use std::fs::File;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Console filter when `DREAMD_LOG` is unset: dreamd's own INFO, every other
/// target held to WARN, and the per-request `http` span and event left to the
/// file (BZR-1470 D2, D3).
pub const CONSOLE_DEFAULT_FILTER: &str =
    "warn,dreamd=info,dreamd_core=info,dreamd_protocol=info,dreamd_core::server::http::router=warn";

/// File filter when `DREAMD_LOG` is unset: as the console, but it keeps one
/// `http request` line per request.
pub const FILE_DEFAULT_FILTER: &str = "warn,dreamd=info,dreamd_core=info,dreamd_protocol=info";

/// How the console layer prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleFormat {
    /// Text, colour only on a terminal (BZR-1470 D1).
    Text { ansi: bool },
    /// One JSON object per line, for `DREAMD_LOG_FORMAT=json`.
    Json,
}

impl ConsoleFormat {
    /// `DREAMD_LOG_FORMAT`'s value. Only the exact string `json` means JSON.
    pub fn from_env_value(value: Option<&str>, stderr_is_terminal: bool) -> Self {
        match value {
            Some("json") => ConsoleFormat::Json,
            _ => ConsoleFormat::Text {
                ansi: stderr_is_terminal,
            },
        }
    }
}

/// `DREAMD_LOG` if it is set, non-empty and parses; otherwise `default`.
fn filter_or_default(dreamd_log: Option<&str>, default: &str) -> EnvFilter {
    dreamd_log
        .filter(|v| !v.is_empty())
        .and_then(|v| EnvFilter::try_new(v).ok())
        .unwrap_or_else(|| EnvFilter::new(default))
}

/// The subscriber `init_tracing` installs. Pure apart from the writers, so a
/// test can capture both layers with `tracing::subscriber::with_default`.
///
/// `dreamd_log` is `DREAMD_LOG`'s value, if any. Each layer gets its own
/// filter (there is no global one): the console's from
/// [`CONSOLE_DEFAULT_FILTER`], the file's from [`FILE_DEFAULT_FILTER`], and a
/// valid non-empty `dreamd_log` replaces both. `file` is `None` for a
/// console-only process.
pub fn build_subscriber<C, F>(
    dreamd_log: Option<&str>,
    format: ConsoleFormat,
    console: C,
    file: Option<F>,
) -> impl tracing::Subscriber + Send + Sync
where
    C: for<'w> fmt::MakeWriter<'w> + Send + Sync + 'static,
    F: for<'w> fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    // `EnvFilter` is not `Clone`, so each layer builds its own.
    let console_filter = filter_or_default(dreamd_log, CONSOLE_DEFAULT_FILTER);
    let console_layer = match format {
        ConsoleFormat::Text { ansi } => fmt::layer()
            .with_writer(console)
            .with_ansi(ansi)
            .with_filter(console_filter)
            .boxed(),
        ConsoleFormat::Json => fmt::layer()
            .with_writer(console)
            .json()
            .with_ansi(false)
            .with_filter(console_filter)
            .boxed(),
    };

    // `Option<Layer>` is a `Layer`: `None` adds nothing.
    let file_layer = file.map(|file| {
        fmt::layer()
            .json()
            .with_ansi(false)
            .with_writer(file)
            .with_filter(filter_or_default(dreamd_log, FILE_DEFAULT_FILTER))
    });

    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
}

/// Initialize the process-wide tracing subscriber.
///
/// Pass a log-file path to add a file layer that appends to it; pass `None`
/// to run console-only, as every command but `watch` does. The daemon uses
/// [`init_daemon_tracing`] instead. Returns the appender [`WorkerGuard`] whenever a
/// file layer is installed — **the caller must hold it for the whole process
/// lifetime** or buffered file logs are dropped on early drop. Returns `None`
/// when running console-only (no path given, or the log dir is not writable —
/// the function degrades to console rather than failing).
///
/// Reads `DREAMD_LOG` and `DREAMD_LOG_FORMAT`; see the module docs for what
/// each one does.
///
/// Idempotent: uses `try_init`, so a second call is a silent no-op rather than
/// a panic.
pub fn init_tracing(log_file: Option<PathBuf>) -> Option<WorkerGuard> {
    let dreamd_log = std::env::var("DREAMD_LOG").ok();
    let log_format = std::env::var("DREAMD_LOG_FORMAT").ok();
    let format =
        ConsoleFormat::from_env_value(log_format.as_deref(), std::io::stderr().is_terminal());

    // File → JSON, appended to, non-blocking. If the dir is not writable,
    // this falls through to the console-only subscriber below.
    if let Some(path) = log_file {
        if let Ok(file) = open_private_append(&path) {
            let (non_blocking, guard) = tracing_appender::non_blocking(file);
            let _ = build_subscriber(
                dreamd_log.as_deref(),
                format,
                std::io::stderr,
                Some(non_blocking),
            )
            .try_init();
            return Some(guard);
        }
    }

    // Console-only fallback (no path given or log dir not writable).
    let _ = build_subscriber(
        dreamd_log.as_deref(),
        format,
        std::io::stderr,
        None::<fn() -> std::io::Sink>,
    )
    .try_init();
    None
}

/// A daemon log of this size or more is renamed to `<log>.1` when `watch`
/// starts serving (DRM-119).
pub const DAEMON_LOG_ROTATE_BYTES: u64 = 8 * 1024 * 1024;

/// Lines logged before the daemon log opens are held up to this many bytes.
/// A line that would pass it is dropped whole.
const PRE_OPEN_CAP: usize = 1024 * 1024;

/// The daemon's file writer: registered by [`init_daemon_tracing`], opened by
/// [`open_daemon_log`].
static DAEMON_LOG: OnceLock<Arc<DeferredLog>> = OnceLock::new();

/// The daemon's subscriber: the console as `init_tracing`, plus a file layer
/// whose file is not opened until [`open_daemon_log`] (DRM-119). The caller
/// holds the guard for the process lifetime.
///
/// Until then the file layer's lines are held in memory, up to 1 MiB, so a
/// `watch` that fails before it binds its socket never touches the log a
/// running daemon writes, and one that binds keeps its earlier lines (WAL
/// recovery, the index open). Reads `DREAMD_LOG` and `DREAMD_LOG_FORMAT` as
/// [`init_tracing`] does, and like it uses `try_init`.
pub fn init_daemon_tracing() -> WorkerGuard {
    let dreamd_log = std::env::var("DREAMD_LOG").ok();
    let log_format = std::env::var("DREAMD_LOG_FORMAT").ok();
    let format =
        ConsoleFormat::from_env_value(log_format.as_deref(), std::io::stderr().is_terminal());

    let log = Arc::clone(DAEMON_LOG.get_or_init(|| Arc::new(DeferredLog::default())));
    let (non_blocking, guard) = tracing_appender::non_blocking(DeferredWriter(log));
    let _ = build_subscriber(
        dreamd_log.as_deref(),
        format,
        std::io::stderr,
        Some(non_blocking),
    )
    .try_init();
    guard
}

/// Open the daemon log at `path`, once, after the socket is bound:
/// - rotate it to `<path>.1` if it is at least [`DAEMON_LOG_ROTATE_BYTES`];
/// - open it with create and append, and on Unix mode `0600`, also setting
///   `0600` on a file that already existed;
/// - write the held lines, then send every later line to the file.
///
/// A no-op without [`init_daemon_tracing`], and on any call after the first.
/// On an error the file layer discards its lines from then on; nothing panics.
pub fn open_daemon_log(path: &Path) -> std::io::Result<()> {
    match DAEMON_LOG.get() {
        Some(log) => log.open(path),
        None => Ok(()),
    }
}

/// Where the daemon's file layer writes (DRM-119). One lock covers the held
/// lines and the switch to the file, so the held lines are written first and
/// in order.
#[derive(Debug, Default)]
struct DeferredLog(Mutex<Sink>);

#[derive(Debug)]
enum Sink {
    /// Before the open: whole lines, at most [`PRE_OPEN_CAP`] bytes.
    Held(Vec<u8>),
    /// After an open that succeeded.
    Open(File),
    /// After an open that failed: lines are discarded.
    Discard,
}

impl Default for Sink {
    fn default() -> Self {
        Sink::Held(Vec::new())
    }
}

impl DeferredLog {
    /// Hold, write or discard one line. The non-blocking worker hands each
    /// formatted line to one `write_all`, so `line` is a whole line.
    fn write_line(&self, line: &[u8]) -> std::io::Result<()> {
        let mut sink = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match &mut *sink {
            Sink::Held(held) => {
                if held.len() + line.len() <= PRE_OPEN_CAP {
                    held.extend_from_slice(line);
                }
                Ok(())
            }
            Sink::Open(file) => file.write_all(line),
            Sink::Discard => Ok(()),
        }
    }

    /// Flush the file, once there is one.
    fn flush(&self) -> std::io::Result<()> {
        let mut sink = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match &mut *sink {
            Sink::Open(file) => file.flush(),
            Sink::Held(_) | Sink::Discard => Ok(()),
        }
    }

    /// [`open_daemon_log`]'s body: a no-op unless the lines are still held.
    fn open(&self, path: &Path) -> std::io::Result<()> {
        let mut sink = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let Sink::Held(held) = &mut *sink else {
            return Ok(());
        };
        let held = std::mem::take(held);
        let opened = rotate_if_large(path)
            .and_then(|()| open_private_append(path))
            .and_then(|mut file| file.write_all(&held).map(|()| file));
        match opened {
            Ok(file) => {
                *sink = Sink::Open(file);
                Ok(())
            }
            Err(e) => {
                *sink = Sink::Discard;
                Err(e)
            }
        }
    }
}

/// The `Write` the non-blocking appender owns; every write goes to the shared
/// [`DeferredLog`].
struct DeferredWriter(Arc<DeferredLog>);

impl Write for DeferredWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write_line(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// `<path>.1`: the name a rotated daemon log takes.
fn rotated_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}

/// Rename `path` to `<path>.1`, replacing an older `.1`, when it is at least
/// [`DAEMON_LOG_ROTATE_BYTES`]. The renamed file is set to `0600` on Unix.
fn rotate_if_large(path: &Path) -> std::io::Result<()> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() >= DAEMON_LOG_ROTATE_BYTES => {
            let rotated = rotated_path(path);
            std::fs::rename(path, &rotated)?;
            set_private(&std::fs::File::open(&rotated)?)
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Open `path` to append, creating it (and its folder) if needed. On Unix the
/// file is created `0600`, and an existing file is set to `0600`.
fn open_private_append(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    set_private(&file)?;
    Ok(file)
}

/// Set an open file to mode `0600` on Unix; nothing elsewhere.
fn set_private(file: &File) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::{Arc, Mutex};

    /// A `MakeWriter` that appends every write to a shared buffer.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Capture {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).expect("utf8 log")
        }

        fn lines(&self) -> Vec<String> {
            self.text()
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect()
        }
    }

    impl io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> fmt::MakeWriter<'a> for Capture {
        type Writer = Capture;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    const STATUS: &str = "dreamd watch: serving on /x";

    /// Every event §4 of the BZR-1470 spec names, by target.
    fn emit_all() {
        // lib INFO
        tracing::info!(target: "tantivy::indexer::index_writer", "Preparing commit");
        tracing::info!(target: "rmcp::service", "received notification");
        // lib WARN
        tracing::warn!(target: "tantivy::indexer::segment_updater", "merge failed");
        // status
        tracing::info!(target: "dreamd_core::server::watch", "dreamd watch: serving on /x");
        // request
        let span = tracing::info_span!(target: "dreamd_core::server::http::router", "http", method = "GET");
        tracing::info!(target: "dreamd_core::server::http::router", parent: &span, "http request");
        // router WARN
        tracing::warn!(target: "dreamd_core::server::http::router", "slow request");
    }

    /// Run [`emit_all`] under `build_subscriber` and return the console and
    /// file captures.
    fn run(dreamd_log: Option<&str>, format: ConsoleFormat) -> (Capture, Capture) {
        let console = Capture::default();
        let file = Capture::default();
        let subscriber = build_subscriber(dreamd_log, format, console.clone(), Some(file.clone()));
        tracing::subscriber::with_default(subscriber, emit_all);
        (console, file)
    }

    const PLAIN: ConsoleFormat = ConsoleFormat::Text { ansi: false };

    #[test]
    fn default_console_holds_libraries_to_warn() {
        let (console, _) = run(None, PLAIN);
        let out = console.text();
        assert!(out.contains("merge failed"), "lib WARN must print: {out}");
        assert!(
            !out.contains("Preparing commit"),
            "tantivy INFO leaked: {out}"
        );
        assert!(
            !out.contains("received notification"),
            "rmcp INFO leaked: {out}"
        );
    }

    #[test]
    fn default_console_keeps_status_and_leaves_requests_to_the_file() {
        let (console, file) = run(None, PLAIN);
        let out = console.text();
        assert!(out.contains(STATUS), "status line missing: {out}");
        assert!(out.contains("slow request"), "router WARN missing: {out}");
        assert!(
            !out.contains("http request"),
            "request line on console: {out}"
        );

        let log = file.text();
        assert!(log.contains("http request"), "file lost the request: {log}");
        assert!(log.contains(STATUS), "file lost the status: {log}");
        assert!(
            !log.contains("Preparing commit"),
            "tantivy INFO in file: {log}"
        );
    }

    #[test]
    fn default_file_is_json_lines() {
        let (_, file) = run(None, PLAIN);
        let lines = file.lines();
        assert!(!lines.is_empty(), "file got nothing");
        for line in lines {
            let v: serde_json::Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("file line is not JSON ({e}): {line}"));
            assert!(v.get("level").is_some(), "no level: {line}");
            assert!(v.get("target").is_some(), "no target: {line}");
        }
    }

    #[test]
    fn a_set_dreamd_log_replaces_both_defaults() {
        let (console, file) = run(Some("info"), PLAIN);
        for (name, out) in [("console", console.text()), ("file", file.text())] {
            assert!(
                out.contains("Preparing commit"),
                "{name} must show lib INFO under DREAMD_LOG=info: {out}"
            );
            assert!(
                out.contains("http request"),
                "{name} must show the request under DREAMD_LOG=info: {out}"
            );
        }

        let (console, file) = run(Some("off"), PLAIN);
        assert_eq!(
            console.text(),
            "",
            "DREAMD_LOG=off must silence the console"
        );
        assert_eq!(file.text(), "", "DREAMD_LOG=off must silence the file");
    }

    #[test]
    fn empty_or_invalid_dreamd_log_gets_the_defaults() {
        for value in ["", "dreamd=["] {
            let (console, _) = run(Some(value), PLAIN);
            let out = console.text();
            assert!(
                out.contains(STATUS),
                "DREAMD_LOG={value:?} must get the defaults (status line): {out}"
            );
            assert!(
                !out.contains("Preparing commit"),
                "DREAMD_LOG={value:?} must hold libraries to WARN: {out}"
            );
            assert!(
                !out.contains("http request"),
                "DREAMD_LOG={value:?} must leave requests to the file: {out}"
            );
        }
    }

    #[test]
    fn console_format_is_json_only_for_the_exact_value() {
        for t in [true, false] {
            for value in [None, Some(""), Some("JSON"), Some(" json"), Some("text")] {
                assert_eq!(
                    ConsoleFormat::from_env_value(value, t),
                    ConsoleFormat::Text { ansi: t },
                    "DREAMD_LOG_FORMAT={value:?}, terminal={t}"
                );
            }
            assert_eq!(
                ConsoleFormat::from_env_value(Some("json"), t),
                ConsoleFormat::Json,
                "DREAMD_LOG_FORMAT=json, terminal={t}"
            );
        }
    }

    #[test]
    fn text_console_has_colour_only_on_a_terminal() {
        let (console, _) = run(None, ConsoleFormat::Text { ansi: false });
        let out = console.text();
        assert!(!out.contains('\x1b'), "ANSI off a terminal: {out:?}");
        assert!(
            !out.lines().any(|l| l.starts_with('{')),
            "a JSON line on the text console: {out}"
        );
        let status = out
            .lines()
            .find(|l| l.contains(STATUS))
            .unwrap_or_else(|| panic!("status line missing: {out}"));
        assert!(status.contains(" INFO "), "no INFO level: {status}");

        let (console, _) = run(None, ConsoleFormat::Text { ansi: true });
        let out = console.text();
        assert!(out.contains("\x1b["), "no ANSI on a terminal: {out:?}");
    }

    #[test]
    fn json_console_writes_json_lines() {
        let (console, _) = run(None, ConsoleFormat::Json);
        let lines = console.lines();
        assert!(!lines.is_empty(), "console got nothing");
        for line in &lines {
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|e| panic!("console line is not JSON ({e}): {line}"));
        }
        let status = lines
            .iter()
            .find(|l| l.contains(STATUS))
            .unwrap_or_else(|| panic!("status line missing: {lines:?}"));
        assert!(status.contains(r#""level":"INFO""#), "not INFO: {status}");
    }

    fn read(path: &std::path::Path) -> String {
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    /// DRM-119 test 1: the held lines are written first, later lines append,
    /// and only the first open counts.
    #[test]
    fn held_lines_go_first_and_later_lines_append() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.log");
        let second = dir.path().join("second.log");
        std::fs::write(&first, "old\n").unwrap();

        let log = Arc::new(DeferredLog::default());
        let mut writer = DeferredWriter(Arc::clone(&log));
        writer.write_all(b"a\n").unwrap();
        assert_eq!(read(&first), "old\n", "a held line reached the file");

        log.open(&first).expect("first open");
        assert_eq!(read(&first), "old\na\n");

        writer.write_all(b"b\n").unwrap();
        assert_eq!(read(&first), "old\na\nb\n");

        log.open(&second).expect("second open is a no-op");
        writer.write_all(b"c\n").unwrap();
        assert!(
            !second.exists(),
            "a second open created {}",
            second.display()
        );
        assert_eq!(read(&first), "old\na\nb\nc\n");
    }

    /// DRM-119 test 2: past the cap a line is dropped whole, so the file
    /// gets only whole lines and at most `PRE_OPEN_CAP` bytes of them.
    #[test]
    fn the_hold_drops_whole_lines_past_the_cap() {
        use std::io::Write as _;
        // 1,001 bytes, so the cap is not a multiple of the line length.
        let line = |i: usize| format!("{i:06} {}\n", "x".repeat(993));
        let lines = PRE_OPEN_CAP / line(0).len() + 5;

        let log = Arc::new(DeferredLog::default());
        let mut writer = DeferredWriter(Arc::clone(&log));
        for i in 0..lines {
            writer.write_all(line(i).as_bytes()).unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.log");
        log.open(&path).expect("open");

        let text = read(&path);
        assert!(text.len() <= PRE_OPEN_CAP, "{} bytes held", text.len());
        assert!(
            text.len() > PRE_OPEN_CAP - line(0).len(),
            "the hold stopped early: {} bytes",
            text.len()
        );
        assert!(text.ends_with('\n'), "the file ends inside a line");
        let kept: Vec<&str> = text.split_inclusive('\n').collect();
        assert!(kept.len() < lines, "nothing was dropped");
        for (i, got) in kept.iter().enumerate() {
            assert_eq!(*got, line(i), "line {i} is not whole, or out of order");
        }
    }
}
