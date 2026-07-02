//! `dreamd status` — one-shot daemon + project status snapshot (WEG-103).
//!
//! Prints a structured plain-text block: daemon liveness, the socket path, the
//! project resolved from `$CWD` and whether it is registered with the daemon,
//! the last dream cycle recorded in per-project state, and the tail of the
//! daemon log. Liveness is a pure filesystem check — the socket's presence at
//! the expected path — so `status` never connects and never hangs, even with no
//! daemon running.

use std::io::{self, Write};
use std::path::Path;

use dreamd_core::layout::AgentRoot;
use dreamd_core::{registry, wal};

/// Number of trailing log lines echoed by the report.
pub(crate) const LOG_TAIL_LINES: usize = 5;

/// Read the last [`LOG_TAIL_LINES`] lines of the daemon log for the report.
///
/// Returns an empty vec when the file is absent or unreadable (`status` then
/// prints "recent log: (none)"). The caller MUST invoke this *before* the
/// tracing subscriber is installed: `init_tracing` opens the same log path with
/// `truncate(true)` at startup, so reading afterward would always see an empty
/// file. See `cli::run`.
pub(crate) fn read_log_tail(log_file: &Path) -> Vec<String> {
    let Ok(contents) = std::fs::read_to_string(log_file) else {
        return Vec::new();
    };
    let lines: Vec<&str> = contents.lines().collect();
    let start = lines.len().saturating_sub(LOG_TAIL_LINES);
    lines[start..].iter().map(|s| s.to_string()).collect()
}

/// Run `dreamd status` and write the report to `out`.
///
/// `socket` is the resolved daemon UDS path (`None` when the home directory
/// can't be resolved); `registry_path` is a daemon-home path; `log_tail` holds
/// the daemon log's last lines, pre-read by the caller (see [`read_log_tail`]).
/// Returns `Ok(true)` when the daemon appears live — the socket is present, so
/// the caller exits 0 — and `Ok(false)` otherwise (exit 1). Reads that fail on
/// malformed on-disk state degrade to a fallback string rather than aborting the
/// report, so `status` always prints a clean block.
pub fn run(
    cwd: &Path,
    socket: Option<&Path>,
    registry_path: &Path,
    log_tail: &[String],
    out: &mut impl Write,
) -> io::Result<bool> {
    // Liveness is socket presence at the expected path. Never connect — a
    // connect to a wedged daemon can hang, and this command must not.
    let live = socket.map(Path::exists).unwrap_or(false);
    writeln!(
        out,
        "daemon: {}",
        if live { "running" } else { "not running" }
    )?;
    match socket {
        Some(p) => writeln!(out, "socket: {}", p.display())?,
        None => writeln!(out, "socket: (unresolved — no home directory)")?,
    }

    // Project resolved from CWD, plus its registration with the daemon.
    match AgentRoot::discover(cwd) {
        Ok(root) => {
            let registered = registry::resolve_project(registry_path, root.project_root())
                .ok()
                .flatten()
                .is_some();
            writeln!(
                out,
                "project: {} ({})",
                root.project_root().display(),
                if registered {
                    "registered"
                } else {
                    "not registered"
                }
            )?;

            let status =
                wal::read_last_cycle_status(&root).unwrap_or_else(|_| "unknown".to_string());
            match wal::read_cycle_started_at(&root).unwrap_or(None) {
                Some(at) => writeln!(out, "last_dream_cycle: {at} ({status})")?,
                None => writeln!(out, "last_dream_cycle: never run ({status})")?,
            }
        }
        Err(_) => {
            writeln!(out, "project: no .agent/ store in CWD")?;
            writeln!(out, "last_dream_cycle: (no store)")?;
        }
    }

    // Tail of the daemon log. Lines are pre-read by the caller before the
    // tracing subscriber truncates the log at startup; an empty slice means the
    // log was absent or empty.
    if log_tail.is_empty() {
        writeln!(out, "recent log: (none)")?;
    } else {
        writeln!(out, "recent log (last {LOG_TAIL_LINES} lines):")?;
        for line in log_tail {
            writeln!(out, "  {line}")?;
        }
    }

    Ok(live)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn daemon_running_when_socket_present() {
        let sock = tempfile::NamedTempFile::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut buf = Vec::new();
        let live = run(
            dir.path(),
            Some(sock.path()),
            Path::new("/no/registry.toml"),
            &[],
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(live, "present socket must report the daemon live");
        assert!(out.contains("daemon: running"), "got: {out}");
    }

    #[test]
    fn daemon_not_running_when_socket_absent() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.sock");
        let mut buf = Vec::new();
        let live = run(
            dir.path(),
            Some(&missing),
            Path::new("/no/registry.toml"),
            &[],
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(!live, "absent socket must report the daemon down");
        assert!(out.contains("daemon: not running"), "got: {out}");
    }

    #[test]
    fn no_store_in_cwd_reported() {
        let dir = tempfile::tempdir().unwrap();
        let mut buf = Vec::new();
        run(
            dir.path(),
            None,
            Path::new("/no/registry.toml"),
            &[],
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("no .agent/ store in CWD"), "got: {out}");
    }

    #[test]
    fn registered_project_annotated() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        let root = AgentRoot::new(&project);
        fs::create_dir_all(root.agent_dir()).unwrap();
        // Stored roots are canonicalized at write time; store the canonical path.
        let canonical = fs::canonicalize(&project).unwrap();
        let mut reg = tempfile::NamedTempFile::new().unwrap();
        writeln!(reg, "[[projects]]").unwrap();
        writeln!(reg, "root = \"{}\"", canonical.display()).unwrap();

        let mut buf = Vec::new();
        run(&project, None, reg.path(), &[], &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("(registered)"), "got: {out}");
    }

    #[test]
    fn unregistered_project_annotated() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        let root = AgentRoot::new(&project);
        fs::create_dir_all(root.agent_dir()).unwrap();

        let mut buf = Vec::new();
        run(
            &project,
            None,
            Path::new("/no/registry.toml"),
            &[],
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("(not registered)"), "got: {out}");
    }

    #[test]
    fn last_cycle_rendered_after_commit() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        let root = AgentRoot::new(&project);
        fs::create_dir_all(root.agent_dir()).unwrap();
        // Drive the state through the core WAL writers rather than touching the
        // state file directly (keeps this command's state access via wal::).
        wal::begin_cycle(&root, 1_751_371_200).unwrap();
        wal::commit_cycle(&root, 1_751_371_200).unwrap();

        let mut buf = Vec::new();
        run(
            &project,
            None,
            Path::new("/no/registry.toml"),
            &[],
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("last_dream_cycle:"), "got: {out}");
        assert!(
            out.contains("complete"),
            "committed status must show; got: {out}"
        );
        assert!(
            !out.contains("never run"),
            "timestamp present, not 'never run'; got: {out}"
        );
    }

    #[test]
    fn log_tail_lines_rendered() {
        let dir = tempfile::tempdir().unwrap();
        let tail = vec!["first line".to_string(), "second line".to_string()];
        let mut buf = Vec::new();
        run(
            dir.path(),
            None,
            Path::new("/no/registry.toml"),
            &tail,
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("recent log (last 5 lines):"), "got: {out}");
        assert!(out.contains("first line"), "got: {out}");
        assert!(out.contains("second line"), "got: {out}");
    }

    #[test]
    fn empty_log_tail_reports_none() {
        let dir = tempfile::tempdir().unwrap();
        let mut buf = Vec::new();
        run(
            dir.path(),
            None,
            Path::new("/no/registry.toml"),
            &[],
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("recent log: (none)"), "got: {out}");
    }

    #[test]
    fn read_log_tail_returns_last_five() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("dreamd.log");
        fs::write(&log, "l1\nl2\nl3\nl4\nl5\nl6\nl7\n").unwrap();
        let tail = read_log_tail(&log);
        assert_eq!(
            tail,
            vec!["l3", "l4", "l5", "l6", "l7"],
            "must keep last five"
        );
    }

    #[test]
    fn read_log_tail_absent_file_is_empty() {
        let tail = read_log_tail(Path::new("/no/such/dreamd.log"));
        assert!(tail.is_empty(), "absent log must yield an empty tail");
    }
}
