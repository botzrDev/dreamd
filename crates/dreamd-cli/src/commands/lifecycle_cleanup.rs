//! Shared lifecycle-cleanup helpers for `dreamd uninstall` / `dreamd update`
//! (AILAB-226).
//!
//! Best-effort process stop (the README `pkill -f` recipe), daemon-socket
//! removal, and cache-tree clearing. Every path is injected by the caller:
//! `cli.rs` resolves the real home-derived locations ($DREAMD_SOCK /
//! `DaemonHome::socket_path()` for the socket), tests pass temp dirs — nothing
//! in this module resolves `~` on its own, so tests never touch the real
//! `~/.agent`, `~/.cache`, or npx cache.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Full-argv patterns handed to `pkill -f`, mirroring the documented manual
/// recipe. Deliberately two-token patterns: a bare `dreamd` would match
/// unrelated invocations — including this very process.
pub const STOP_PATTERNS: [&str; 2] = ["dreamd mcp", "dreamd watch"];

/// Outcome of a best-effort [`stop_local_servers`] pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StopReport {
    /// Whether a stop was attempted at all (`false` on non-unix platforms).
    pub attempted: bool,
    /// Whether at least one `dreamd mcp` / `dreamd watch` process matched.
    pub matched: bool,
}

/// Injectable stop hook. `cli.rs` passes [`stop_local_servers`]; tests pass a
/// fake so the suite never signals a real process (a live `dreamd watch` on a
/// dev machine must survive `cargo test`).
pub type Stopper<'a> = &'a mut dyn FnMut(&mut dyn Write) -> StopReport;

/// Best-effort terminate local `dreamd mcp` / `dreamd watch` processes.
///
/// Shells out to `pkill -f` (procps), matching the *full* command line the
/// same way the README manual recipe does. The current process is never
/// killed: `pkill` never signals itself, and this process's own argv
/// (`dreamd uninstall` / `dreamd update`) cannot match either
/// [`STOP_PATTERNS`] entry. No match is success (idempotent when nothing is
/// running); a missing or failing `pkill` degrades to a stderr warning,
/// never an error.
#[cfg(unix)]
pub fn stop_local_servers(err: &mut dyn Write) -> StopReport {
    let mut report = StopReport {
        attempted: true,
        matched: false,
    };
    for pattern in STOP_PATTERNS {
        match std::process::Command::new("pkill")
            .args(["-f", "--", pattern])
            .status()
        {
            // pkill exit codes: 0 = one or more matched, 1 = none matched.
            Ok(status) => match status.code() {
                Some(0) => report.matched = true,
                Some(1) => {}
                other => {
                    let _ = writeln!(
                        err,
                        "dreamd: warning — `pkill -f '{pattern}'` failed (status {other:?}); \
                         stop it manually if still running."
                    );
                }
            },
            Err(e) => {
                let _ = writeln!(
                    err,
                    "dreamd: warning — could not run pkill ({e}); \
                     stop `{pattern}` manually if still running."
                );
            }
        }
    }
    report
}

/// Non-unix: no-op. Process stop is unix-only in v0.1 — Windows detached
/// lifecycle is AILAB-203's scope.
#[cfg(not(unix))]
pub fn stop_local_servers(err: &mut dyn Write) -> StopReport {
    let _ = writeln!(
        err,
        "dreamd: note — stopping dreamd mcp/watch processes is unix-only in v0.1; stop them manually."
    );
    StopReport {
        attempted: false,
        matched: false,
    }
}

/// Outcome of [`remove_socket`].
#[derive(Debug)]
pub enum SocketRemoval {
    /// A socket file existed and was unlinked.
    Removed,
    /// Nothing to unlink — benign, the socket was already gone.
    NotFound,
    /// A daemon answered on the socket, so it was left in place. Callers warn
    /// and continue; see [`remove_socket`] for why this is not an error.
    RefusedLive,
    /// A real unlink failure (e.g. permission denied). Callers warn on
    /// stderr with the path and continue — an unremovable socket must not
    /// abort the wider uninstall/update flow.
    Failed(std::io::Error),
}

/// Unlink the daemon socket if present, refusing while a daemon still answers
/// on it.
///
/// The liveness guard is the point of this function. Both callers
/// (`uninstall`, `update`) run it *after* a best-effort [`stop_local_servers`]
/// pass whose failure is only a stderr warning — so on a box with no `pkill`,
/// or where the signal is denied, the daemon is still serving. Unlinking then
/// leaves it alive on an orphaned inode: it keeps working, but every client
/// resolves a path that no longer exists, so MCP silently falls back to the
/// in-process Phase 1 backend and cross-harness recall stops with no error.
/// Refusing is the same rule `dreamd archive` and `dreamd doctor --repair`
/// already apply before touching a live daemon's state.
///
/// `NotFound` is the only error kind treated as benign; any other unlink
/// failure is reported as [`SocketRemoval::Failed`] so callers can print an
/// actionable warning instead of a false "no daemon socket" line.
///
/// The probe is not instantaneous: it waits out a daemon that is merely
/// *stopping*. See [`STOP_GRACE`].
pub fn remove_socket(socket: &Path) -> SocketRemoval {
    remove_socket_within(socket, STOP_GRACE)
}

/// How long a signalled daemon gets to finish unwinding before the socket is
/// called live.
///
/// Without this window the guard misfires on the *common* upgrade path.
/// `pkill` returns once the signal is **delivered**, not once the target
/// exits, and `dreamd watch` handles SIGTERM gracefully: it drains the
/// coordinator and indexer (a Tantivy commit, cadence
/// `DEFAULT_COMMIT_CADENCE` = 5s) and only then unlinks its own socket. The
/// listener therefore stays bound and answering for the whole drain, while
/// `remove_socket` runs microseconds after `pkill` returns. Probing once would
/// report a daemon that is already shutting down as live, and `dreamd update`
/// with a running `watch` — the path the 2026-08-03 clean-box audit exercised
/// as R6 — would print a "still live" warning telling the user to stop a
/// daemon that is mid-exit.
///
/// Matched to the commit cadence because that drain is the slow step. A daemon
/// that is still answering after this window is genuinely not going away
/// (no `pkill` on the box, or the signal was denied), which is the case the
/// guard exists to catch. Costs nothing when the socket is already dead or
/// absent: the wait only starts once a probe has come back live.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// Re-probe interval while waiting out a stopping daemon.
const STOP_POLL: Duration = Duration::from_millis(50);

/// [`remove_socket`] with an injectable grace window so tests can pin the
/// refuse path (`Duration::ZERO`) without waiting out [`STOP_GRACE`].
fn remove_socket_within(socket: &Path, grace: Duration) -> SocketRemoval {
    if !wait_until_quiet(socket, grace) {
        return SocketRemoval::RefusedLive;
    }
    match std::fs::remove_file(socket) {
        Ok(()) => SocketRemoval::Removed,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SocketRemoval::NotFound,
        Err(e) => SocketRemoval::Failed(e),
    }
}

/// Poll until nothing answers on `socket`, or `grace` elapses. `true` means
/// the socket is safe to unlink; `false` means a daemon outlasted the window.
fn wait_until_quiet(socket: &Path, grace: Duration) -> bool {
    if !socket_is_live(socket) {
        return true;
    }
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        std::thread::sleep(STOP_POLL);
        if !socket_is_live(socket) {
            return true;
        }
    }
    false
}

/// Connect-probe the daemon socket. Delegates to the one canonical probe so
/// this agrees with `dreamd status`, `dreamd doctor`, `dreamd archive`, and
/// MCP backend selection rather than becoming a fifth opinion.
#[cfg(unix)]
fn socket_is_live(socket: &Path) -> bool {
    dreamd_core::server::is_daemon_socket_live(socket)
}

/// Non-unix: there is no UDS daemon in v0.1, so there is nothing live to
/// protect. Matches the `cfg(not(unix))` stance in `status` and `setup`.
#[cfg(not(unix))]
fn socket_is_live(_socket: &Path) -> bool {
    false
}

/// Attach the offending path so `dreamd: error — …` output is actionable
/// (e.g. permission denied names the tree the user must fix). Shared by the
/// uninstall and update cache-clear steps.
pub fn io_with_path(e: std::io::Error, path: &Path) -> std::io::Error {
    std::io::Error::new(
        e.kind(),
        format!("{} (while clearing {})", e, path.display()),
    )
}

/// Remove a whole cache tree.
///
/// `Ok(true)` when the tree existed and was removed; `Ok(false)` when it was
/// already absent (idempotent). Other I/O errors (e.g. permission denied)
/// are real failures and propagate to the caller for an actionable message.
pub fn clear_dir_tree(dir: &Path) -> std::io::Result<bool> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Scoped npx-cache clear: delete only entries that
/// [`entry_is_dreamd_mcp_npx_install`] recognises as dreamd-mcp installs —
/// the same scoping as the README manual recipe. Never wipes unrelated
/// packages; the blanket wipe is gated behind `dreamd uninstall --all-npx`
/// (see `commands::uninstall`). A missing cache root is `Ok` with nothing
/// removed. Returns the removed entry paths.
pub fn clear_scoped_npx(npx_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    let entries = match std::fs::read_dir(npx_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(removed),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if entry_is_dreamd_mcp_npx_install(&path) {
            std::fs::remove_dir_all(&path)?;
            removed.push(path);
        }
    }
    Ok(removed)
}

/// `true` iff the npx cache entry at `entry` is a dreamd-mcp install.
///
/// npm writes `~/.npm/_npx/<hash>/package.json` with NO top-level `name` —
/// the requested package is recorded as a dependencies key, e.g.
/// `{"dependencies":{"dreamd-mcp":"^0.1.0-rc.6"}}`. An entry matches when
/// any of:
/// - the top-level manifest has `"name": "dreamd-mcp"` (fallback), or
/// - its `"dependencies"` object contains the exact key `"dreamd-mcp"`
///   (precise: a tool that merely depends on dreamd-mcp gets its own name
///   as the sole dependency key), or
/// - `<entry>/node_modules/dreamd-mcp/package.json` exists with
///   `"name": "dreamd-mcp"`.
///
/// Unreadable or malformed manifests are a non-match (skip, never delete on
/// doubt).
fn entry_is_dreamd_mcp_npx_install(entry: &Path) -> bool {
    if let Some(value) = read_package_json(&entry.join("package.json")) {
        if value.get("name").and_then(|n| n.as_str()) == Some("dreamd-mcp") {
            return true;
        }
        if value
            .get("dependencies")
            .and_then(|d| d.as_object())
            .is_some_and(|deps| deps.contains_key("dreamd-mcp"))
        {
            return true;
        }
    }
    read_package_json(
        &entry
            .join("node_modules")
            .join("dreamd-mcp")
            .join("package.json"),
    )
    .is_some_and(|v| v.get("name").and_then(|n| n.as_str()) == Some("dreamd-mcp"))
}

/// Parse `package_json` as JSON. Unreadable or malformed files are `None` —
/// callers treat that as a non-match.
fn read_package_json(package_json: &Path) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(package_json).ok()?;
    serde_json::from_str(&raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn remove_socket_unlinks_present_file() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("dreamd.sock");
        fs::write(&sock, b"").unwrap();
        assert!(
            matches!(remove_socket(&sock), SocketRemoval::Removed),
            "present socket file must be removed"
        );
        assert!(!sock.exists());
    }

    #[test]
    fn remove_socket_absent_is_not_found_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("nope.sock");
        assert!(
            matches!(remove_socket(&sock), SocketRemoval::NotFound),
            "absent socket must report NotFound"
        );
    }

    /// A daemon that outlasts the grace window must survive `remove_socket`.
    /// The stop pass both callers run first is best-effort, so unlinking here
    /// would leave a live daemon on an orphaned inode — still serving, but on
    /// a path no client can resolve. Grace is pinned to zero so the refuse
    /// path is exercised without waiting out `STOP_GRACE`.
    #[cfg(unix)]
    #[test]
    fn remove_socket_refuses_a_daemon_that_outlasts_the_grace_window() {
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("dreamd.sock");
        let _listener = UnixListener::bind(&sock).expect("bind test listener");

        assert!(
            matches!(
                remove_socket_within(&sock, Duration::ZERO),
                SocketRemoval::RefusedLive
            ),
            "a socket a daemon still answers on must not be unlinked"
        );
        assert!(sock.exists(), "the live socket must be left in place");
    }

    /// The regression the grace window exists for: `pkill` returns as soon as
    /// SIGTERM is *delivered*, so on the ordinary `dreamd update`-with-`watch`
    /// path the daemon is still bound and answering while it drains. Probing
    /// once would refuse a socket whose daemon is already mid-exit. The guard
    /// must wait it out and then unlink, not warn.
    #[cfg(unix)]
    #[test]
    fn remove_socket_waits_out_a_daemon_that_is_still_shutting_down() {
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("dreamd.sock");
        let listener = UnixListener::bind(&sock).expect("bind test listener");

        // Answers now, goes quiet shortly after — a daemon finishing its drain.
        let shutting_down = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(listener);
        });

        assert!(
            matches!(
                remove_socket_within(&sock, Duration::from_secs(5)),
                SocketRemoval::Removed
            ),
            "a daemon that exits within the grace window must not be refused"
        );
        assert!(
            !sock.exists(),
            "the socket must be unlinked once it goes quiet"
        );
        shutting_down.join().unwrap();
    }

    /// The liveness guard must not over-refuse: an orphan socket file — daemon
    /// gone, file left behind — is the ordinary uninstall case and must still
    /// unlink. Dropping the listener closes the fd without unlinking the path,
    /// which is exactly what a SIGKILLed daemon leaves on disk.
    #[cfg(unix)]
    #[test]
    fn remove_socket_unlinks_orphan_socket_file() {
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("dreamd.sock");
        drop(UnixListener::bind(&sock).expect("bind test listener"));
        assert!(sock.exists(), "orphan socket file expected for the test");

        assert!(
            matches!(remove_socket(&sock), SocketRemoval::Removed),
            "an orphan socket file must still be unlinked"
        );
        assert!(!sock.exists());
    }

    #[test]
    fn clear_dir_tree_removes_and_reports_absent() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("cache");
        fs::create_dir_all(tree.join("v1")).unwrap();
        fs::write(tree.join("v1/dreamd"), b"bin").unwrap();
        assert!(clear_dir_tree(&tree).unwrap(), "existing tree removed");
        assert!(!tree.exists());
        assert!(
            !clear_dir_tree(&tree).unwrap(),
            "second clear is Ok(false), not an error"
        );
    }

    #[test]
    fn scoped_npx_removes_only_dreamd_mcp_entries() {
        let dir = tempfile::tempdir().unwrap();
        // Real npm shape: no top-level name, requested package as a
        // dependencies key.
        let ours = dir.path().join("aaa111");
        fs::create_dir_all(&ours).unwrap();
        fs::write(
            ours.join("package.json"),
            br#"{"dependencies":{"dreamd-mcp":"^0.1.0-rc.6"}}"#,
        )
        .unwrap();
        let other = dir.path().join("bbb222");
        fs::create_dir_all(&other).unwrap();
        fs::write(
            other.join("package.json"),
            br#"{"dependencies":{"other-tool":"^1"}}"#,
        )
        .unwrap();
        let no_manifest = dir.path().join("ccc333");
        fs::create_dir_all(&no_manifest).unwrap();

        let removed = clear_scoped_npx(dir.path()).unwrap();
        assert_eq!(removed, vec![ours.clone()], "only the dreamd-mcp entry");
        assert!(!ours.exists());
        assert!(other.exists(), "unrelated package must survive");
        assert!(no_manifest.exists(), "manifest-less entry must survive");
    }

    #[test]
    fn scoped_npx_missing_root_is_ok_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("_missing");
        let removed = clear_scoped_npx(&missing).unwrap();
        assert!(removed.is_empty(), "missing npx root must be a no-op");
    }

    #[test]
    fn npx_entry_match_covers_real_npm_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let entry = dir.path().join("entry");
        fs::create_dir_all(&entry).unwrap();
        let manifest = entry.join("package.json");

        // Malformed JSON: never a match — never delete on doubt.
        fs::write(&manifest, b"not json").unwrap();
        assert!(
            !entry_is_dreamd_mcp_npx_install(&entry),
            "malformed manifest must never be deleted"
        );

        // (b) Real npx shape: no name field, dreamd-mcp as the exact
        // dependencies key.
        fs::write(
            &manifest,
            br#"{"dependencies":{"dreamd-mcp":"^0.1.0-rc.6"}}"#,
        )
        .unwrap();
        assert!(
            entry_is_dreamd_mcp_npx_install(&entry),
            "the dependencies-key shape npm actually writes must match"
        );

        // A different dependency key is not a match.
        fs::write(&manifest, br#"{"dependencies":{"other-tool":"^1"}}"#).unwrap();
        assert!(
            !entry_is_dreamd_mcp_npx_install(&entry),
            "an unrelated dependency key must not match"
        );

        // (a) Top-level name fallback still matches.
        fs::write(&manifest, br#"{"name":"dreamd-mcp"}"#).unwrap();
        assert!(entry_is_dreamd_mcp_npx_install(&entry));

        // (c) node_modules/dreamd-mcp/package.json naming the package
        // matches even when the top-level manifest doesn't.
        fs::write(&manifest, br#"{"name":"some-wrapper"}"#).unwrap();
        assert!(!entry_is_dreamd_mcp_npx_install(&entry));
        let nm = entry.join("node_modules").join("dreamd-mcp");
        fs::create_dir_all(&nm).unwrap();
        fs::write(nm.join("package.json"), br#"{"name":"dreamd-mcp"}"#).unwrap();
        assert!(
            entry_is_dreamd_mcp_npx_install(&entry),
            "an installed node_modules/dreamd-mcp must match"
        );
    }
}
