//! `dreamd status` — one-shot daemon + project status snapshot (WEG-103).
//!
//! Prints a structured plain-text block: daemon liveness, the daemon's address,
//! the project resolved from `$CWD` and whether it is registered with the
//! daemon, the last dream cycle recorded in per-project state, whether git has
//! a commit of the episodic log (DRM-100), and the tail of the daemon log.
//! While the daemon home holds a project store, which discovery skips, a
//! `daemon_home_store:` line before the log tail says so (DRM-101).
//!
//! Liveness is always a bounded probe, never file presence, so a stale
//! address file left behind by a `SIGKILL`ed daemon reports `not running`
//! without hanging on a wedged listener. On Unix that is a UDS connect to
//! `~/.agent/dreamd.sock` (see
//! [`dreamd_core::server::is_daemon_socket_live`]), then one
//! `GET /api/v1/health` with no `X-Agent-Root` and a 2 s deadline
//! ([`dreamd_core::daemon_client::STATUS_DEADLINE`], DRM-121). Any answer is
//! `running`; a daemon that accepts and does not answer, such as one stopped
//! with `kill -STOP`, is `not responding`. Off Unix it is a TCP connect
//! to the loopback address published in `~/.agent/server.json` (see
//! [`dreamd_core::daemon_client::tcp_daemon_is_live`], AILAB-192), and the
//! report labels that line `server_json` rather than `socket`.

use std::io::{self, Write};
use std::path::Path;

use dreamd_core::autobiography::{self, LogGitState};
use dreamd_core::layout::AgentRoot;
use dreamd_core::{registry, wal};

/// Number of trailing log lines echoed by the report.
pub(crate) const LOG_TAIL_LINES: usize = 5;

/// Read the last `n` lines of the daemon log.
///
/// Returns an empty vec when the file is absent or unreadable (the caller then
/// prints "recent log: (none)"). There is no ordering constraint against the
/// tracing subscriber (AILAB-184): only `dreamd watch` has a file layer, and
/// it opens the file only after binding its socket, to append (DRM-119), so no
/// one-shot ever writes that file or has to race its own startup to read it;
/// the read happens in-command and sees whatever a running daemon has written. `dreamd status` reads
/// [`LOG_TAIL_LINES`] through [`read_log_tail`]; `dreamd service status` reads
/// its own, longer tail (`service::STATUS_LOG_TAIL_LINES`, AILAB-178).
pub(crate) fn read_log_tail_n(log_file: &Path, n: usize) -> Vec<String> {
    let Ok(contents) = std::fs::read_to_string(log_file) else {
        return Vec::new();
    };
    let lines: Vec<&str> = contents.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].iter().map(|s| s.to_string()).collect()
}

/// Read the last [`LOG_TAIL_LINES`] lines of the daemon log for the report —
/// [`read_log_tail_n`] at this report's fixed width; see it for the read
/// contract. Called in-command from `cli::run_status`.
pub(crate) fn read_log_tail(log_file: &Path) -> Vec<String> {
    read_log_tail_n(log_file, LOG_TAIL_LINES)
}

/// Run `dreamd status` and write the report to `out`.
///
/// Arguments:
/// - `socket` is the resolved daemon address path: the UDS path on Unix,
///   `~/.agent/server.json` off it, and `None` when the home directory can't
///   be resolved.
/// - `registry_path` is the daemon-home `registry.toml`, `None` when the home
///   directory can't be resolved; the project then reports `not registered`
///   without reading anything.
/// - `log_tail` holds the daemon log's last lines, pre-read by the caller
///   (see `read_log_tail`).
///
/// Returns `Ok(true)` when the probe finds the daemon running (the caller
/// exits 0) and `Ok(false)` when it is not running or not responding (exit 1).
/// Reads that fail on
/// malformed on-disk state print a fallback string instead of aborting, so
/// `status` always prints a complete block.
pub fn run(
    cwd: &Path,
    socket: Option<&Path>,
    registry_path: Option<&Path>,
    log_tail: &[String],
    out: &mut impl Write,
) -> io::Result<bool> {
    let liveness = socket.map(daemon_liveness).unwrap_or(Liveness::NotRunning);
    match liveness {
        Liveness::Running => writeln!(out, "daemon: running")?,
        Liveness::NotRunning => writeln!(out, "daemon: not running")?,
        Liveness::NotResponding => writeln!(
            out,
            "daemon: not responding (it accepted a connection but did not answer within {} s)",
            STATUS_DEADLINE_SECS
        )?,
    }
    let live = liveness == Liveness::Running;
    // Label the address line for the transport that is actually in use. On Unix
    // the daemon's address *is* a filesystem socket. Off Unix it is
    // `~/.agent/server.json` — the file a loopback-TCP daemon publishes its
    // host/port into (AILAB-192) — and calling that a socket would mislead an
    // operator searching for it.
    #[cfg(unix)]
    const ADDRESS_LABEL: &str = "socket";
    #[cfg(not(unix))]
    const ADDRESS_LABEL: &str = "server_json";
    match socket {
        Some(p) => writeln!(out, "{ADDRESS_LABEL}: {}", p.display())?,
        None => writeln!(out, "{ADDRESS_LABEL}: (unresolved — no home directory)")?,
    }

    // Project resolved from CWD, plus its registration with the daemon.
    match AgentRoot::discover(cwd) {
        Ok(root) => {
            let registered = registry_path
                .and_then(|path| registry::resolve_project(path, root.project_root()).ok())
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

            // DRM-100: whether git has a commit of the episodic log.
            let project = root.project_root();
            match autobiography::log_git_state(project) {
                Ok(LogGitState::NoRepo) => writeln!(out, "memory_in_git: no git repo")?,
                Ok(LogGitState::Committed) => writeln!(out, "memory_in_git: committed")?,
                Ok(LogGitState::Ignored) => writeln!(out, "memory_in_git: ignored")?,
                Ok(LogGitState::NotCommitted) => writeln!(
                    out,
                    "memory_in_git: not committed  (`git clean -fd` can delete it; commit it \
                     from the project root: {})",
                    autobiography::commit_store_command(project)
                )?,
                Err(e) => writeln!(out, "memory_in_git: error ({e})")?,
            }
        }
        Err(_) => {
            writeln!(out, "project: no .agent/ store in CWD")?;
            writeln!(out, "last_dream_cycle: (no store)")?;
            writeln!(out, "memory_in_git: (no store)")?;
        }
    }

    // DRM-101: a project store in the daemon home, which discovery skips. The
    // registry path is `DaemonHome::registry_toml`, so its parent is the home.
    if let Some(line) = registry_path
        .and_then(Path::parent)
        .and_then(crate::commands::daemon_home_store::report_line)
    {
        writeln!(out, "{line}")?;
    }

    // Tail of the daemon log, pre-read by the caller (see `read_log_tail_n`);
    // an empty slice means the log was absent or empty.
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

/// What the liveness probe found (DRM-121).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Liveness {
    /// Nothing accepted the connection.
    NotRunning,
    /// The daemon answered.
    Running,
    /// Something accepted the connection but did not answer in time.
    #[cfg_attr(not(unix), allow(dead_code))]
    NotResponding,
}

/// The status probe's deadline in whole seconds, as the report prints it.
#[cfg(unix)]
const STATUS_DEADLINE_SECS: u64 = dreamd_core::daemon_client::STATUS_DEADLINE.as_secs();
/// Off Unix the probe is a connect, which never reports `NotResponding`.
#[cfg(not(unix))]
const STATUS_DEADLINE_SECS: u64 = 2;

/// Daemon liveness probe for the resolved socket path: a bounded connect,
/// then one `GET /api/v1/health` with no `X-Agent-Root`, which the daemon
/// answers `400` without opening a project, within
/// [`dreamd_core::daemon_client::STATUS_DEADLINE`] (DRM-121).
///
/// Any HTTP answer, or a version mismatch, is `Running`; a timeout is
/// `NotResponding`; any other failure is `NotRunning`.
#[cfg(unix)]
fn daemon_liveness(path: &Path) -> Liveness {
    use dreamd_core::daemon_client::{
        health_probe_request, send_one_within, DaemonTransportError, STATUS_DEADLINE,
    };

    if !dreamd_core::server::is_daemon_socket_live(path) {
        return Liveness::NotRunning;
    }
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        // No runtime for the request: keep the connect's answer.
        return Liveness::Running;
    };
    match rt.block_on(send_one_within(
        path,
        health_probe_request(),
        STATUS_DEADLINE,
    )) {
        Ok(_) | Err(DaemonTransportError::VersionMismatch(_)) => Liveness::Running,
        Err(DaemonTransportError::TimedOut { .. }) => Liveness::NotResponding,
        Err(_) => Liveness::NotRunning,
    }
}

/// Daemon liveness probe off Unix, where `path` is `~/.agent/server.json` and
/// the daemon is a loopback TCP listener (AILAB-192).
///
/// Reads the published address and attempts a bounded TCP connect to it, which
/// is the exact analogue of the Unix bounded UDS connect: a `server.json` a
/// `SIGKILL`ed watch left behind still parses, so presence proves nothing and
/// only the connect can tell "running" from "stale address file".
#[cfg(not(unix))]
fn daemon_liveness(path: &Path) -> Liveness {
    if dreamd_core::daemon_client::tcp_daemon_is_live(path) {
        Liveness::Running
    } else {
        Liveness::NotRunning
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg(unix)]
    #[test]
    fn daemon_running_when_socket_present() {
        use std::io::{BufRead, Write};

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("dreamd.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind listener");
        // DRM-121: the listener answers, as a daemon's middleware answers the
        // probe with no `X-Agent-Root`. Each connection on its own thread; the
        // connect probe's connection ends before it sends anything.
        let server = listener.try_clone().expect("clone listener");
        std::thread::spawn(move || {
            for stream in server.incoming() {
                let Ok(mut stream) = stream else { return };
                std::thread::spawn(move || {
                    let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => return,
                            Ok(_) if line == "\r\n" => break,
                            Ok(_) => {}
                        }
                    }
                    let _ = stream.write_all(
                        b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    );
                });
            }
        });
        let mut buf = Vec::new();
        let live = run(
            dir.path(),
            Some(&sock),
            Some(Path::new("/no/registry.toml")),
            &[],
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(live, "live listener must report the daemon running");
        assert!(out.contains("daemon: running"), "got: {out}");
        drop(listener);
    }

    #[cfg(unix)]
    #[test]
    fn daemon_not_running_when_orphan_socket_present() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("dreamd.sock");
        // File present at the socket path but no listener — the SIGKILL orphan case.
        std::fs::write(&sock, b"").unwrap();

        let mut buf = Vec::new();
        let live = run(
            dir.path(),
            Some(&sock),
            Some(Path::new("/no/registry.toml")),
            &[],
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(!live, "orphan socket must not report the daemon live");
        assert!(out.contains("daemon: not running"), "got: {out}");
    }

    #[test]
    fn daemon_not_running_when_socket_absent() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.sock");
        let mut buf = Vec::new();
        let live = run(
            dir.path(),
            Some(&missing),
            Some(Path::new("/no/registry.toml")),
            &[],
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(!live, "absent socket must report the daemon down");
        assert!(out.contains("daemon: not running"), "got: {out}");
    }

    /// DRM-100 test 22: the sixth line says whether git has a commit of the
    /// episodic log, from no store through a broken HEAD.
    #[test]
    fn status_reports_whether_git_has_a_commit_of_the_log() {
        fn git_line(cwd: &Path) -> String {
            let mut buf = Vec::new();
            run(
                cwd,
                None,
                Some(Path::new("/no/registry.toml")),
                &[],
                &mut buf,
            )
            .unwrap();
            let out = String::from_utf8(buf).unwrap();
            let lines: Vec<&str> = out
                .lines()
                .filter(|line| line.starts_with("memory_in_git:"))
                .collect();
            assert_eq!(lines.len(), 1, "one memory_in_git line; got:\n{out}");
            lines[0].to_string()
        }

        let dir = tempfile::tempdir().unwrap();
        assert_eq!(git_line(dir.path()), "memory_in_git: (no store)");

        let project = dir.path().join("proj");
        let root = AgentRoot::new(&project);
        fs::create_dir_all(root.episodic_dir()).unwrap();
        fs::write(root.episodic_jsonl(), b"").unwrap();
        assert_eq!(git_line(&project), "memory_in_git: no git repo");

        let repo = git2::Repository::init(&project).unwrap();
        assert_eq!(
            git_line(&project),
            "memory_in_git: not committed  (`git clean -fd` can delete it; commit it from \
             the project root: git add .agent && git commit -m \"Add dreamd memory\" -- .agent)"
        );

        let mut index = repo.index().unwrap();
        index
            .add_path(Path::new(".agent/episodic/AGENT_LEARNINGS.jsonl"))
            .unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("user", "user@example.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "commit the store", &tree, &[])
            .unwrap();
        assert_eq!(git_line(&project), "memory_in_git: committed");

        // HEAD's branch now names an object the repo does not hold.
        let branch = repo
            .find_reference("HEAD")
            .unwrap()
            .symbolic_target()
            .unwrap()
            .expect("HEAD is symbolic")
            .to_string();
        fs::write(
            repo.path().join(branch),
            "1111111111111111111111111111111111111111\n",
        )
        .unwrap();
        let line = git_line(&project);
        assert!(line.starts_with("memory_in_git: error ("), "got: {line}");
    }

    #[test]
    fn no_store_in_cwd_reported() {
        let dir = tempfile::tempdir().unwrap();
        let mut buf = Vec::new();
        run(
            dir.path(),
            None,
            Some(Path::new("/no/registry.toml")),
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
        run(&project, None, Some(reg.path()), &[], &mut buf).unwrap();
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
            Some(Path::new("/no/registry.toml")),
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
        // Drive the state through the core cycle writers rather than touching
        // the state file directly (keeps this command's state access via wal::):
        // an empty stage, committed under the project lock's ledger.
        let lock = dreamd_core::writer_lock::WriterLock::acquire(
            &root,
            dreamd_core::writer_lock::WriterKind::Dream,
        )
        .unwrap();
        let stage = wal::begin_cycle(&root, wal::CycleKind::Dream, 1_751_371_200).unwrap();
        wal::commit_cycle(stage, lock.ledger(), 1_751_371_200).unwrap();
        drop(lock);

        let mut buf = Vec::new();
        run(
            &project,
            None,
            Some(Path::new("/no/registry.toml")),
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
            Some(Path::new("/no/registry.toml")),
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
            Some(Path::new("/no/registry.toml")),
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
    fn read_log_tail_n_returns_last_n() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("dreamd.log");
        let body: String = (1..=12).map(|i| format!("l{i}\n")).collect();
        fs::write(&log, body).unwrap();

        let expected: Vec<String> = (3..=12).map(|i| format!("l{i}")).collect();
        assert_eq!(read_log_tail_n(&log, 10), expected, "must keep last ten");
        // The `dreamd status` reader on the same file is still the five-line
        // tail (AILAB-178 leaves LOG_TAIL_LINES alone).
        assert_eq!(
            read_log_tail(&log),
            vec!["l8", "l9", "l10", "l11", "l12"],
            "dreamd status must stay at LOG_TAIL_LINES"
        );
    }

    #[test]
    fn read_log_tail_absent_file_is_empty() {
        let tail = read_log_tail(Path::new("/no/such/dreamd.log"));
        assert!(tail.is_empty(), "absent log must yield an empty tail");
    }
}
