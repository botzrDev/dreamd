#![cfg(unix)]

//! Cross-`$HOME` stop scoping for `dreamd update` (AILAB-584).
//!
//! The `lifecycle_cleanup` unit tests drive the scoped stop pass against a
//! synthetic `/proc` tree with the signal *recorded* instead of sent. That pins
//! the decision rule but not the wiring: it cannot catch `cli.rs` handing the
//! stopper the wrong `$HOME`, `run_watch` resolving its socket off a different
//! home than the one the stopper attributes against, or a really-spawned
//! daemon's `/proc/<pid>/cmdline` failing to match `STOP_PATTERNS`. This suite
//! runs the built binary end to end against a really-running `dreamd watch`.
//!
//! **Two assertions, and the negative one is worthless alone** — a stopper that
//! never signalled anything would sail through it. So the *same* daemon is
//! first spared by another home's `update` and then stopped by its own:
//!
//! 1. **Spare** — the AILAB-554 F1 regression. `HOME=A` serves a daemon;
//!    `HOME=B dreamd update` (non-dry-run) must exit 0, leave A's process
//!    alive, and leave A's socket answering.
//! 2. **Stop** — the positive control. `HOME=A dreamd update` must actually
//!    terminate that same daemon, which is what proves assertion 1 measured
//!    scoping rather than inertness.
//!
//! Both live in one `#[test]` so a single spawned daemon covers both halves:
//! two daemons would mean two Tantivy writers and twice the cleanup surface.
//!
//! ## `dreamd watch` does not detach — liveness is the `Child` handle
//!
//! `commands::watch::run` calls [`dreamd_core::server::run_watch`] on the
//! calling thread and blocks. `dreamd-core` does carry a `detach_double_fork`
//! helper, but nothing calls it — verified by grep, and confirmed here by the
//! spawned `dreamd watch` having no children in `/proc`. The `Child` this suite
//! spawns therefore **is** the daemon, so `Child::try_wait` is authoritative
//! and does not lie the way it would over a double fork. The socket probe is
//! asserted alongside it as the user-visible half of the same fact: a daemon
//! that is alive but no longer answering would fail assertion 1 just as hard as
//! a dead one.
//!
//! ## Sandbox discipline
//!
//! Every spawn sets `HOME` to a tempdir and clears `DREAMD_SOCK`. `run_watch`
//! binds `$HOME/.agent/dreamd.sock` and ignores `$DREAMD_SOCK` (known drift,
//! deliberately not fixed here), so `HOME` is the *only* lever that keeps this
//! suite off the developer's real `~/.agent` — and off any real daemon they
//! have running. Each temp project carries a `.git` sentinel because `init`
//! and `AgentRoot::discover` both refuse a bare `mktemp -d`.
//!
//! Every wait is bounded. A test that hangs in CI is worse than one that fails.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

fn dreamd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dreamd")
}

/// How long a freshly spawned `dreamd watch` gets to recover its WAL, open its
/// Tantivy index, and bind its socket. Generous because a cold index open on a
/// loaded CI box is not fast — but a hard ceiling, never a sleep.
const SOCKET_DEADLINE: Duration = Duration::from_secs(30);

/// How long the signalled daemon gets to actually exit.
///
/// `kill` returns once SIGTERM is *delivered*, not once the target exits, and
/// `run_watch` drains the coordinator and the Tantivy indexer (commit cadence
/// `DEFAULT_COMMIT_CADENCE` = 5s) before unwinding and unlinking its socket. So
/// "dead immediately" is the wrong assertion; the window has to clear that
/// drain comfortably.
const EXIT_DEADLINE: Duration = Duration::from_secs(15);

/// Re-probe interval for both bounded waits.
const POLL: Duration = Duration::from_millis(50);

/// How long the spared daemon must be observed *staying* alive.
///
/// A one-shot "is it still alive?" check straight after `HOME=B dreamd update`
/// is a race, not an assertion: `kill` returns on signal delivery, so a daemon
/// that was wrongly signalled is still running — and still answering — for a
/// moment afterwards. Holding the check open for a window that comfortably
/// exceeds the shutdown of a daemon with nothing to drain (milliseconds) turns
/// the snapshot into a real observation.
const SPARE_HOLD: Duration = Duration::from_secs(1);

/// A temp project carrying the `.git` root sentinel that `dreamd init` and
/// `AgentRoot::discover` walk up to find.
fn new_project() -> tempfile::TempDir {
    let project = tempfile::tempdir().expect("temp project dir");
    std::fs::create_dir(project.path().join(".git")).expect("write .git root sentinel");
    project
}

/// `$HOME/.agent/dreamd.sock` — the path `run_watch` binds, resolved the same
/// way `DaemonHome::socket_path()` does.
fn socket_of(home: &Path) -> PathBuf {
    home.join(".agent").join("dreamd.sock")
}

/// Run the built binary to completion in `project` under `home`.
fn run(project: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(dreamd_bin())
        .args(args)
        .current_dir(project)
        .env("HOME", home)
        .env_remove("DREAMD_SOCK")
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("run `dreamd {}`: {e}", args.join(" ")))
}

fn assert_ok(out: &Output, label: &str) {
    assert!(
        out.status.success(),
        "{label} must exit 0; exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A really-running `dreamd watch`, killed on drop.
struct Watch {
    child: Child,
    log: PathBuf,
}

impl Watch {
    /// Spawn `dreamd watch` in `project` under `home`, with both output streams
    /// captured to a file inside `home`.
    ///
    /// The log is redirected rather than piped because nothing reads it until
    /// something fails: a piped stream with no reader would fill its buffer and
    /// wedge the daemon mid-test, and `wait_with_output` cannot be used on a
    /// process this suite deliberately keeps running.
    fn spawn(project: &Path, home: &Path) -> Self {
        let log = home.join("watch.log");
        let out = std::fs::File::create(&log).expect("create watch log");
        let err = out.try_clone().expect("clone watch log handle");
        let child = Command::new(dreamd_bin())
            .arg("watch")
            .current_dir(project)
            .env("HOME", home)
            .env_remove("DREAMD_SOCK")
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .expect("spawn dreamd watch");
        Self { child, log }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// `Some(status)` once the daemon has exited, `None` while it runs.
    /// Authoritative because `dreamd watch` is a foreground process — see the
    /// module header.
    fn exited(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().expect("try_wait on dreamd watch")
    }

    /// The daemon's captured output, for failure messages.
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for Watch {
    /// SIGKILL unconditionally, even while an assertion is unwinding. A leaked
    /// daemon holds a Tantivy writer lock on a tempdir that is about to
    /// disappear and would poison later runs.
    ///
    /// `Child::kill` is used rather than shelling out to `kill -KILL <pid>`:
    /// it is a safe `std` API (nothing here needs `libc`, which the workspace's
    /// `unsafe_code = "forbid"` would reject anyway), it does not fork a helper
    /// process during panic unwinding, and — unlike a raw pid signal — it
    /// refuses to fire once the child has been reaped, so the positive-control
    /// path cannot end up signalling a recycled pid. Both results are ignored:
    /// an already-exited daemon is the success case, not an error.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Poll until `socket` answers, bounded by [`SOCKET_DEADLINE`].
///
/// The child is watched alongside the socket so the common failure — a daemon
/// that refuses to boot — becomes an immediate, legible panic carrying its log
/// instead of a 30-second stall ending in "timed out".
fn wait_for_live_socket(watch: &mut Watch, socket: &Path) {
    let deadline = Instant::now() + SOCKET_DEADLINE;
    loop {
        if dreamd_core::server::is_daemon_socket_live(socket) {
            return;
        }
        if let Some(status) = watch.exited() {
            panic!(
                "dreamd watch exited ({status}) before binding {}; log:\n{}",
                socket.display(),
                watch.log()
            );
        }
        assert!(
            Instant::now() < deadline,
            "dreamd watch pid {} never bound {} within {SOCKET_DEADLINE:?}; log:\n{}",
            watch.pid(),
            socket.display(),
            watch.log(),
        );
        std::thread::sleep(POLL);
    }
}

/// Assert the daemon stays up — process alive, socket present, socket still
/// answering — continuously for [`SPARE_HOLD`], rather than at one instant.
fn assert_stays_live(watch: &mut Watch, socket: &Path, after: &str) {
    let until = Instant::now() + SPARE_HOLD;
    let pid = watch.pid();
    while Instant::now() < until {
        assert!(
            watch.exited().is_none(),
            "{after} must not stop a daemon serving another $HOME (pid {pid} exited); log:\n{}",
            watch.log(),
        );
        assert!(
            socket.exists(),
            "{after} must leave the other home's socket {} in place",
            socket.display(),
        );
        assert!(
            dreamd_core::server::is_daemon_socket_live(socket),
            "the other home's daemon must still answer on {} after {after}",
            socket.display(),
        );
        std::thread::sleep(POLL);
    }
}

/// Poll until the daemon exits, bounded by [`EXIT_DEADLINE`].
fn wait_for_exit(watch: &mut Watch) -> ExitStatus {
    let deadline = Instant::now() + EXIT_DEADLINE;
    loop {
        if let Some(status) = watch.exited() {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "dreamd watch pid {} was still running {EXIT_DEADLINE:?} after its own \
             $HOME ran `dreamd update`; the scoped stop pass did not signal it. log:\n{}",
            watch.pid(),
            watch.log(),
        );
        std::thread::sleep(POLL);
    }
}

/// The whole AILAB-584 contract against real processes: another home's
/// `dreamd update` leaves this daemon alone, its own home's `dreamd update`
/// stops it.
///
/// Declaration order matters. The tempdirs are bound before the [`Watch`], so
/// Rust's reverse-order drop kills the daemon *before* the directories holding
/// its socket and index are removed.
#[test]
fn update_spares_another_homes_watch_and_stops_its_own() {
    let home_a = tempfile::tempdir().expect("temp HOME A");
    let project_a = new_project();
    let home_b = tempfile::tempdir().expect("temp HOME B");
    let project_b = new_project();
    let socket_a = socket_of(home_a.path());

    assert_ok(
        &run(project_a.path(), home_a.path(), &["init"]),
        "dreamd init in project A",
    );

    let mut watch = Watch::spawn(project_a.path(), home_a.path());
    wait_for_live_socket(&mut watch, &socket_a);
    let watch_pid = watch.pid();

    // ---- 1. Cross-HOME spare (AILAB-554 F1) --------------------------------
    // A sandboxed `HOME=B dreamd update` used to `pkill -f 'dreamd watch'`,
    // which is machine-global: it SIGTERMed daemons serving every other home on
    // the box. Nothing about A is attributable to B — different `HOME=`, and
    // the target/debug binary is under neither home's `~/.cache/dreamd-mcp`.
    let update_b = run(project_b.path(), home_b.path(), &["update"]);
    assert_ok(&update_b, "HOME=B dreamd update");

    // Asserted first because it is the only *non-racy* half: the warning is
    // printed at decision time, so it proves the stopper saw this pid as a
    // candidate and refused it. The liveness hold below would also pass if
    // `STOP_PATTERNS` had silently stopped matching and the daemon were never
    // considered at all.
    //
    // Anchored to ONE substring spanning both facts. Two independent
    // `contains` calls would pass on a box where some *other* unattributable
    // dreamd process supplied "not attributable" while these bare pid digits
    // matched inside a longer pid (`5406` ⊂ `540696`) or inside a path — the
    // assertion would then be green without this daemon ever being mentioned.
    let spared = stderr_of(&update_b);
    let anchor = format!("pid {watch_pid} running: not attributable");
    assert!(
        spared.contains(&anchor),
        "HOME=B `dreamd update` must warn `{anchor}`; stderr={spared}",
    );
    assert_stays_live(&mut watch, &socket_a, "HOME=B `dreamd update`");

    // ---- 2. Same-HOME stop (positive control) ------------------------------
    // Without this, assertion 1 would pass against a stopper that never
    // signals anything at all.
    let update_a = run(project_a.path(), home_a.path(), &["update"]);
    assert_ok(&update_a, "HOME=A dreamd update");

    // Bounded poll, never an immediate assertion: `kill` returns on delivery
    // and the daemon drains before it exits.
    wait_for_exit(&mut watch);
    assert!(
        !dreamd_core::server::is_daemon_socket_live(&socket_a),
        "nothing may still answer on {} once A's daemon has exited",
        socket_a.display(),
    );
    assert!(
        stdout_of(&update_a).contains("stopped local `dreamd mcp` / `dreamd watch` process(es)"),
        "HOME=A `dreamd update` must report the stop it performed; stdout={}",
        stdout_of(&update_a),
    );
}
