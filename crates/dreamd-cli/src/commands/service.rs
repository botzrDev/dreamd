//! `dreamd service install` / `start` / `restart` / `status` / `uninstall` —
//! the per-user service that supervises the daemon: a Linux systemd **user**
//! unit (AILAB-190), a macOS **LaunchAgent** (AILAB-169), or a Windows Task
//! Scheduler **logon task** (AILAB-203). One module, one backend picked at
//! runtime by [`detect_backend`].
//!
//! Linux: `install` writes `~/.config/systemd/user/dreamd.service` and runs
//! `systemctl --user daemon-reload` + `enable --now`; `start` runs
//! `systemctl --user start dreamd.service`; `restart` (AILAB-185) runs
//! `systemctl --user restart dreamd.service`. macOS: `install` writes
//! `~/Library/LaunchAgents/dev.dreamd.dreamd.plist` and runs
//! `launchctl bootstrap gui/<uid> <plist>` (after a best-effort `bootout` when
//! `--force` replaces an existing plist); `start` runs
//! `launchctl kickstart -k gui/<uid>/dev.dreamd.dreamd`, and `restart` issues
//! that *same* argv — `-k` already kills the running instance first, so it is
//! itself the bounce; only the printed verb differs (`restarted`).
//!
//! Windows: `install` writes a Task Scheduler 1.2 definition at
//! `<home>/AppData/Roaming/dreamd/dev.dreamd.dreamd.xml` and runs `schtasks
//! /Create /TN dev.dreamd.dreamd /XML <path>` (`/F` only under `--force`,
//! which is the same overwrite gate the plist has); `start` runs `schtasks
//! /Run /TN <name>`; `restart` is the one bounce that genuinely takes two
//! calls, because the Task Scheduler has no `kickstart -k` — a best-effort
//! `/End` precedes the `/Run`, its non-zero exit on an idle task discarded
//! for the same reason the Darwin `bootout` is.
//!
//! The Windows task is a **`LogonTrigger`**, never a boot trigger and never a
//! Windows Service under `sc.exe`: its action is the same **foreground**
//! `dreamd watch` the unit and the plist supervise, `WorkingDirectory` is the
//! install-time [`AgentRoot`], and `RestartOnFailure` (`PT1M`, 3) stands in
//! for `Restart=on-failure` / `KeepAlive.SuccessfulExit=false`. `install`
//! never issues a `/Run`: `dreamd watch` on Windows still prints the
//! AILAB-174 refusal and exits 2 until AILAB-192 lands the localhost TCP
//! transport, so starting the task at install time would only spawn a process
//! designed to die — a logon trigger that first fires usefully *after* 192 is
//! the entire point. Nothing on this backend detaches a process, tracks a PID
//! of its own, or keeps a liveness ping; that was the pre-`watch`
//! architecture (AILAB-210), and `detach_double_fork` keeps its zero
//! production call sites here too.
//!
//! `install` is also where the Windows bearer token is minted: 32
//! cryptographically random bytes, hex-encoded into `DaemonHome::auth_json()`
//! (`~/.agent/auth.json`), then locked to the installing user's SID with
//! `icacls <path> /inheritance:r /grant:r <sid>:F` — full control rather than
//! read, because `install --force` has to be able to rewrite what it wrote,
//! and inheritance stripped because that is what removes the inherited
//! `Users` ACE. AILAB-192 is the consumer; rotating the token at writer
//! start-up is 192's job, so here it turns over only on a reinstall. The
//! token is never printed and never logged — only its path is. Both Windows
//! files skip [`write_service_file`], because `dreamd_core::io::write_atomic`
//! is `ErrorKind::Unsupported` on Windows and implementing it is not this
//! ticket.
//!
//! `restart` is **bounce-only**: it never rewrites the unit or the plist, so
//! the `ExecStart` / `ProgramArguments` path stays the one captured at
//! `service install`. Bouncing is enough after an in-place `cargo install` to
//! the same path; a binary that *moved* (an npx cache bump) still needs
//! `dreamd service install` (`--force` on macOS) first. It is also not
//! `dreamd update --restart`, which stops local `mcp` / `watch` processes —
//! the supervised one included, since the unit sets no `Environment=HOME=` and
//! so shares the invoking user's — and never brings the unit back up.
//!
//! `status` (AILAB-178) is the supervisor's own view of that service —
//! running / stopped / failed / not-installed, PID, active-since — plus the
//! last [`STATUS_LOG_TAIL_LINES`] lines of `~/.agent/dreamd.log`. On Linux it
//! reads `systemctl --user show dreamd.service` as a `Key=Value` dump (never
//! the human-oriented `status` verb, which is pager-, colour- and
//! locale-bound); on macOS `launchctl print gui/<uid>/dev.dreamd.dreamd`; on
//! Windows `schtasks /Query … /FO LIST /V`, the `Key: Value` form for the
//! same reason.
//! Daemon-level UDS liveness stays `dreamd status` (WEG-103); the two verbs
//! answer different questions and are deliberately separate.
//!
//! `uninstall` (AILAB-202) is the inverse of `install`, and **only** of
//! `install`: it tears down the OS service entry — Linux `disable --now`, the
//! unit file unlinked in-process, then `daemon-reload`; macOS a best-effort
//! `bootout gui/<uid>/<label>` and the plist unlinked. Unlike `start`, it is
//! idempotent: a unit or plist that is already gone is not a failure, and on
//! Linux an absent unit file means `systemctl` is never spawned at all. By
//! default it deletes no data whatsoever and says so, reporting the daemon
//! home and the per-project stores on `preserved:` lines.
//!
//! `--purge` widens that to exactly one directory: the daemon home
//! `~/.agent/` — the registry, the socket and `dreamd.log`. It never walks or
//! deletes a per-project `<repo>/.agent/` store (that wipe is still the manual
//! "Full fresh store" in `docs/troubleshooting.md`) and never touches
//! `~/.config/dreamd/`. Because it is destructive it borrows the
//! `reset workspace` confirmation idiom wholesale — `--yes`, or a typed `y` /
//! `Y` on a tty, and a refusal (exit 2) on non-tty stdin — and confirms
//! **before** any mutation, so a declined prompt leaves the unit installed.
//! The tty probe and the line read are injected for the same reason the
//! process runners are: no unit test may block on real stdin.
//!
//! `service uninstall` is **not** the top-level `dreamd uninstall`
//! (AILAB-226), which stops local `mcp` / `watch` processes, drops the socket
//! and clears caches while leaving the unit in place. This module never calls
//! `commands::uninstall::run`, never reaches for `lifecycle_cleanup`, and
//! never signals a process: on a supervised host `disable --now` / `bootout`
//! *is* how the daemon stops.
//!
//! The unit is `Type=simple`, and launchd's `KeepAlive` plus a **foreground**
//! `ProgramArguments = [exe, watch]` is the plist analogue of it: the OS
//! supervisor itself tracks a **foreground** `dreamd watch` (which already
//! handles SIGTERM, WEG-268) and restarts it on failure (`Restart=on-failure`
//! on Linux, `KeepAlive.SuccessfulExit=false` on macOS — never a bare
//! `KeepAlive=true`, which would restart after a clean SIGTERM). That choice
//! shapes everything else in this module:
//!
//! - **`detach_double_fork` is not this module's call site — do not call it.**
//!   Under `Type=simple` systemd tracks the process it spawned, launchd
//!   tracks the `ProgramArguments` process the same way, and the Task
//!   Scheduler tracks the `Actions/Exec` process it started; a double-fork
//!   would leave the supervisor watching the intermediate parent while the
//!   real daemon is reparented to init and escapes supervision entirely. The
//!   helper keeps its zero production call sites (ARCHITECTURE.md §8.1).
//! - **`WorkingDirectory` (unit line and plist key alike) is the [`AgentRoot`]
//!   project root discovered from `cwd` at install time.** `run_watch` does
//!   `AgentRoot::discover(cwd)` and exits 2 without a project root, so a
//!   service with no working directory would never start. Reinstalling from
//!   another project retargets it: silently on Linux (the unit is simply
//!   overwritten, no `--force` gate), and on macOS and Windows only with
//!   `--force` — the flag is the confirmation (no stdin prompt, same shape as
//!   `reset workspace --yes`); without it an existing plist is
//!   [`ServiceError::PlistExists`] and an existing task XML is
//!   [`ServiceError::TaskExists`], exit 2, and nothing is rewritten — on
//!   Windows the already-minted `auth.json` included.
//! - **No `Environment=` line is ever emitted — in particular never an empty
//!   HOME environment line** (AILAB-584): an empty `HOME` makes every derived
//!   path (`~/.agent`, the cache dir, the npx dir) relative. systemd --user
//!   already provides `HOME` and `%h`; launchd's `gui/<uid>` domain inherits
//!   the login session's, so the plist never carries `EnvironmentVariables`.
//! - **The plist never sets `StandardOutPath` / `StandardErrorPath`** (AILAB-184):
//!   the supervised `watch` already owns `~/.agent/dreamd.log` through the
//!   tracing file layer, which truncates on open, so a second writer aimed at
//!   the same file would clobber it. Likewise never `AbandonProcessGroup`:
//!   launchd must keep tracking the PID it spawned.
//! - **All five verbs are one-shots and stay console-only** (AILAB-184): the
//!   tracing file layer truncates `~/.agent/dreamd.log`, so `wants_daemon_log`
//!   in `cli.rs` never grants it to `service`. The supervised `watch` still
//!   gets the file layer, and its stderr additionally lands in the user
//!   journal on Linux (JSON when stderr is not a TTY).
//!
//! Hosts with no backend at all (Linux without a reachable systemd --user
//! manager: containers, most CI runners) get [`ServiceError::NoSystemd`] and
//! are pointed at foreground `dreamd watch`. The probe is `/run/systemd/system`
//! — systemd's documented "booted with systemd" check — and never spawns
//! `systemctl`; macOS is picked by `cfg!(target_os = "macos")` and Windows by
//! `cfg!(windows)`, neither of them probed. Every process runner is injected
//! — [`Systemctl`] / [`Launchctl`] / [`Schtasks`] / [`Icacls`] for the verbs
//! that only need an exit status, the stdout-returning [`Query`] for the one
//! that reads a report — so unit tests pass a recording closure and can never
//! spawn any of the three supervisors; that is how Linux CI exercises the
//! macOS and the Windows path alike. The uid for the `gui/<uid>` domain and
//! the SID for the `auth.json` ACL are injected the same way (production
//! reads `id -u` and `whoami /user` stdout — no FFI, and so no `windows-sys`
//! anywhere in the dependency graph).

use std::fmt::Write as _;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use dreamd_core::io::write_atomic;
use dreamd_core::{AgentRoot, DaemonHome, LayoutError};

/// Name of the user unit written by `install` and started by `start`.
pub(crate) const UNIT_NAME: &str = "dreamd.service";

/// launchd label of the LaunchAgent written by `install` on macOS; also the
/// plist's basename (`<label>.plist`) and the service name under `gui/<uid>/`.
pub(crate) const LAUNCHD_LABEL: &str = "dev.dreamd.dreamd";

/// Task Scheduler name of the logon task `install` creates on Windows; also
/// the basename of its XML definition (`<name>.xml`). Deliberately the same
/// identity string as [`LAUNCHD_LABEL`] — one service, one name across the
/// three supervisors — but its own const, so renaming either backend's
/// identity can never silently rename the other's.
pub(crate) const SCHTASKS_TASK_NAME: &str = "dev.dreamd.dreamd";

/// Number of trailing `~/.agent/dreamd.log` lines `service status` echoes
/// (Linear AILAB-178). `dreamd status` keeps its own 5 (`status::LOG_TAIL_LINES`).
pub(crate) const STATUS_LOG_TAIL_LINES: usize = 10;

/// How `install` / `start` / `restart` / `uninstall` reach systemd. Production
/// passes [`systemctl_user`]; tests pass a recording closure, so no unit test
/// can ever spawn `systemctl` (this dev box has a live user manager; GHA
/// runners mostly do not).
type Systemctl<'a> = &'a mut dyn FnMut(&[&str]) -> Result<(), ServiceError>;

/// How `install` / `start` / `restart` / `uninstall` reach launchd. Same shape
/// as [`Systemctl`]: production passes [`launchctl`]; tests pass a recording or
/// panicking closure, so no unit test can ever bootstrap a real LaunchAgent
/// (GHA `macos-latest` has a live `launchctl` under the runner user).
type Launchctl<'a> = &'a mut dyn FnMut(&[&str]) -> Result<(), ServiceError>;

/// How `install` / `start` / `restart` / `uninstall` reach the Windows Task
/// Scheduler. Same shape as [`Systemctl`] / [`Launchctl`]: production passes
/// [`schtasks`]; tests pass a recording or panicking closure, so no unit test
/// can ever register a real scheduled task — which is also what lets Linux CI
/// exercise the Windows path, exactly as it already exercises the Darwin one.
type Schtasks<'a> = &'a mut dyn FnMut(&[&str]) -> Result<(), ServiceError>;

/// How `install` locks `~/.agent/auth.json` down to a single SID, in the same
/// injection shape again: production passes [`icacls`], tests a recording
/// closure, so no unit test rewrites a real ACL. Shelling out to the OS's own
/// tool is what keeps this crate FFI-free and free of `windows-sys` (NFR-2)
/// — the same trade [`current_uid`] makes with `id -u`.
type Icacls<'a> = &'a mut dyn FnMut(&[&str]) -> Result<(), ServiceError>;

/// How `status` reads the supervisor: the same injection shape as
/// [`Systemctl`] / [`Launchctl`], but the runner yields the captured stdout of
/// a successful call. Production passes [`systemctl_user_query`] /
/// [`launchctl_query`]; tests pass a closure answering with a fixture, so no
/// unit test can ever query a live supervisor.
type Query<'a> = &'a mut dyn FnMut(&[&str]) -> Result<String, ServiceError>;

/// How `uninstall --purge` asks for confirmation, in the same injection shape
/// as the process runners above. The argument is the prompt (so the wording
/// stays in [`confirm_purge`] next to the decision it guards); `Some(line)` is
/// one line read from a tty, and `None` means stdin is not a tty, which the
/// caller turns into [`ServiceError::NotATty`] — the runner itself never
/// decides, it only answers. Production passes [`confirm_on_tty`]; tests pass
/// a closure, so no unit test can ever block on real stdin or be at the mercy
/// of how the harness wired it up.
type Confirm<'a> = &'a mut dyn FnMut(&str) -> Result<Option<String>, ServiceError>;

/// Which OS supervisor `install` / `start` / `restart` / `status` /
/// `uninstall` talk to on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServiceBackend {
    /// Linux systemd `--user` unit (`systemctl --user`).
    Systemd,
    /// macOS LaunchAgent (`launchctl` in the `gui/<uid>` domain).
    Launchd,
    /// Windows Task Scheduler logon task (`schtasks`), AILAB-203.
    Schtasks,
}

impl ServiceBackend {
    /// The word `service status` prints on its `backend:` line.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Systemd => "systemd",
            Self::Launchd => "launchd",
            Self::Schtasks => "schtasks",
        }
    }
}

/// The supervisor's view of the service, as `service status` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServiceState {
    /// The supervised `watch` is up (`ActiveState=active` / `state = running`).
    Running,
    /// Installed but not running: a loaded, inactive unit; a plist on disk
    /// that launchd has not loaded, or has loaded and is not running; or a
    /// registered scheduled task that is `Ready` / `Disabled`.
    Stopped,
    /// systemd's `ActiveState=failed` — the unit gave up restarting.
    Failed,
    /// No unit at all (`LoadState=not-found`); no plist file and no loaded
    /// label; no registered task and no task XML on disk.
    NotInstalled,
}

impl std::fmt::Display for ServiceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::NotInstalled => "not-installed",
        })
    }
}

/// What `service status` learned from the supervisor; [`render_report`]
/// turns it into the locked stdout block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServiceReport {
    pub(crate) backend: ServiceBackend,
    pub(crate) state: ServiceState,
    /// systemd `MainPID` / launchd `pid = …`; `None` when absent, unparseable,
    /// or `0` (systemd's "no main process"). Always `None` on schtasks: the
    /// `/Query` view reports no process id.
    pub(crate) pid: Option<u32>,
    /// systemd's `ActiveEnterTimestamp`, passed through raw (no duration
    /// math). Always `None` on launchd (`launchctl print` has no start time)
    /// and on schtasks (`/Query` reports a *next* run time, not a start one).
    pub(crate) active_since: Option<String>,
}

#[derive(Debug)]
pub enum ServiceError {
    /// No supported backend: neither macOS nor Windows, and no systemd --user
    /// on this host (`/run/systemd/system` absent).
    NoSystemd,
    /// cwd is not inside a dreamd project (`AgentRoot::discover` failed).
    NoProjectRoot,
    /// macOS: the LaunchAgent plist already exists and `--force` was absent.
    PlistExists(PathBuf),
    /// Windows: the scheduled-task XML already exists and `--force` was
    /// absent. Nothing is rewritten — the already-minted `auth.json`
    /// included, so a bare reinstall can never invalidate a live token.
    TaskExists(PathBuf),
    /// `uninstall --purge` without `--yes` on non-tty stdin: refuse rather
    /// than delete the daemon home unattended in a pipeline. Nothing is
    /// mutated. Same wording as `reset workspace`.
    NotATty,
    /// `uninstall --purge` prompted on a tty and the answer was not `y` / `Y`.
    /// Nothing is mutated — the confirmation runs before any teardown.
    Declined,
    /// The `uninstall --purge` prompt could not be written or its answer could
    /// not be read. Nothing is mutated.
    Stdin(std::io::Error),
    /// `std::env::current_exe()` / canonicalize failed.
    CurrentExe(std::io::Error),
    /// Creating the service directory or writing the service file failed.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// `systemctl` exited non-zero; carries the subcommand words and captured stderr.
    Systemctl {
        args: String,
        status: ExitStatus,
        stderr: String,
    },
    /// `launchctl` exited non-zero; carries the subcommand words and captured stderr.
    Launchctl {
        args: String,
        status: ExitStatus,
        stderr: String,
    },
    /// `schtasks` exited non-zero; carries the argv words and captured stderr.
    Schtasks {
        args: String,
        status: ExitStatus,
        stderr: String,
    },
    /// `icacls` exited non-zero while locking `auth.json` down to one SID;
    /// carries the argv words and captured stderr. Its own variant rather
    /// than [`ServiceError::Io`] on purpose: the file *was* written, and only
    /// its ACL did not take — folding that into an I/O error would report the
    /// wrong failure and point at the wrong fix.
    Icacls {
        args: String,
        status: ExitStatus,
        stderr: String,
    },
    /// `id -u` ran but its exit status or stdout was unusable (macOS uid lookup).
    Uid(String),
    /// `whoami /user` ran but its exit status or stdout was unusable (the
    /// Windows SID lookup; the analogue of [`ServiceError::Uid`]).
    Sid(String),
    /// A helper program (`systemctl`, `launchctl`, `id`, `schtasks`, `icacls`,
    /// `whoami`) could not be spawned at all.
    Spawn {
        program: &'static str,
        source: std::io::Error,
    },
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // User-facing refusal copy points at the foreground daemon only;
            // never mention the macOS LaunchAgent path here (AILAB-169).
            Self::NoSystemd => write!(
                f,
                "dreamd service requires systemd --user; run `dreamd watch` in the foreground instead."
            ),
            Self::NoProjectRoot => {
                write!(f, "no .agent/ directory found. Run `dreamd init` first.")
            }
            Self::PlistExists(path) => write!(
                f,
                "LaunchAgent plist {} already exists; rerun `dreamd service install --force` to overwrite it.",
                path.display()
            ),
            Self::TaskExists(path) => write!(
                f,
                "scheduled-task XML {} already exists; rerun `dreamd service install --force` to overwrite it.",
                path.display()
            ),
            Self::NotATty => write!(f, "stdin is not a tty; pass --yes to confirm"),
            Self::Declined => write!(f, "uninstall declined"),
            Self::Stdin(e) => write!(f, "could not read the confirmation from stdin: {e}"),
            Self::CurrentExe(e) => write!(f, "could not resolve this binary's path: {e}"),
            Self::Io { path, source } => {
                write!(f, "could not write {}: {source}", path.display())
            }
            Self::Systemctl {
                args,
                status,
                stderr,
            } => write!(
                f,
                "systemctl --user {args} failed ({status}): {}",
                stderr.trim()
            ),
            Self::Launchctl {
                args,
                status,
                stderr,
            } => write!(f, "launchctl {args} failed ({status}): {}", stderr.trim()),
            Self::Schtasks {
                args,
                status,
                stderr,
            } => write!(f, "schtasks {args} failed ({status}): {}", stderr.trim()),
            Self::Icacls {
                args,
                status,
                stderr,
            } => write!(f, "icacls {args} failed ({status}): {}", stderr.trim()),
            Self::Uid(detail) => {
                write!(f, "could not determine the current uid via `id -u`: {detail}")
            }
            Self::Sid(detail) => write!(
                f,
                "could not determine the current SID via `whoami /user`: {detail}"
            ),
            Self::Spawn { program, source } => write!(f, "could not run {program}: {source}"),
        }
    }
}

impl std::error::Error for ServiceError {}

/// True when a systemd --user manager can plausibly be reached: Linux and
/// `/run/systemd/system` exists (systemd's documented "am I booted with
/// systemd" probe). Never spawns `systemctl`.
pub(crate) fn systemd_user_available() -> bool {
    cfg!(target_os = "linux") && Path::new("/run/systemd/system").is_dir()
}

/// Production backend pick: macOS is always launchd and Windows is always the
/// Task Scheduler (neither is probed — the target settles it, and there is no
/// second supervisor to lose to on either OS); anything else is systemd when
/// the user-manager probe passes, otherwise `None`, which the verbs map to
/// [`ServiceError::NoSystemd`]. The Windows arm sits *above* the systemd
/// probe only for symmetry with the macOS one: [`systemd_user_available`] is
/// already `cfg!(target_os = "linux")`-gated and would answer `false` there
/// regardless.
fn detect_backend() -> Option<ServiceBackend> {
    if cfg!(target_os = "macos") {
        Some(ServiceBackend::Launchd)
    } else if cfg!(windows) {
        Some(ServiceBackend::Schtasks)
    } else if systemd_user_available() {
        Some(ServiceBackend::Systemd)
    } else {
        None
    }
}

/// Path of the user unit under `home`: `<home>/.config/systemd/user/dreamd.service`.
pub(crate) fn unit_path(home: &Path) -> PathBuf {
    home.join(".config")
        .join("systemd")
        .join("user")
        .join(UNIT_NAME)
}

/// Path of the LaunchAgent plist under `home`:
/// `<home>/Library/LaunchAgents/dev.dreamd.dreamd.plist`.
pub(crate) fn plist_path(home: &Path) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"))
}

/// Path of the Task Scheduler definition under `home`:
/// `<home>/AppData/Roaming/dreamd/dev.dreamd.dreamd.xml`, the analogue of
/// [`plist_path`]. Joined component by component rather than as one
/// `AppData/Roaming/…` literal so the separator is the platform's; a native
/// `%USERPROFILE%` and a Git-Bash `$HOME` both nest `AppData` directly under
/// the user profile, so one path serves both shells.
pub(crate) fn task_xml_path(home: &Path) -> PathBuf {
    home.join("AppData")
        .join("Roaming")
        .join("dreamd")
        .join(format!("{SCHTASKS_TASK_NAME}.xml"))
}

/// Double every `%` so systemd reads it literally. Both `ExecStart=` and
/// `WorkingDirectory=` undergo specifier expansion (`%h`, `%n`, …; see
/// systemd.unit(5) "Specifiers"), so an unescaped `%` in a path either fails
/// to load or silently points somewhere else.
fn escape_specifiers(path: &str) -> String {
    path.replace('%', "%%")
}

/// Render the unit body. `exe` and `project_root` must be absolute.
pub(crate) fn render_user_unit(exe: &Path, project_root: &Path) -> String {
    let exe = escape_specifiers(&exe.display().to_string());
    let project_root = escape_specifiers(&project_root.display().to_string());
    // `ExecStart=` goes through systemd's shell-style quote processing, so an
    // exe path containing whitespace must be double-quoted to survive word
    // splitting; a plain path is emitted bare so the common case stays
    // readable. `WorkingDirectory=` takes a literal path with no quote
    // processing at all, so it is always emitted raw — quoting it would make
    // systemd look for a directory whose name contains the quote characters.
    // Quotes or backslashes inside a whitespace-bearing exe path are not
    // escaped; that corner is out of scope here.
    let exec = if exe.chars().any(char::is_whitespace) {
        format!("\"{exe}\"")
    } else {
        exe
    };
    // Deliberately no `After=` line. `WantedBy=default.target` makes
    // default.target `Wants=` this unit, and a target implicitly orders itself
    // `After=` the units it wants, so `After=default.target` on the unit would
    // form an ordering cycle that systemd breaks by deleting a job.
    let lines = [
        "[Unit]".to_string(),
        "Description=dreamd memory daemon".to_string(),
        String::new(),
        "[Service]".to_string(),
        "Type=simple".to_string(),
        format!("ExecStart={exec} watch"),
        format!("WorkingDirectory={project_root}"),
        "Restart=on-failure".to_string(),
        "RestartSec=5".to_string(),
        String::new(),
        "[Install]".to_string(),
        "WantedBy=default.target".to_string(),
    ];
    let mut unit = lines.join("\n");
    unit.push('\n');
    unit
}

/// Escape the four characters that cannot appear raw inside a plist
/// `<string>` element — or, since AILAB-203, inside a Task Scheduler
/// `<Command>` / `<WorkingDirectory>`, which is the same XML text rule.
/// `&` goes first so the entities introduced for the other three are not
/// themselves re-escaped. No systemd `%` doubling here: neither launchd nor
/// the Task Scheduler does specifier expansion on these fields, so a literal
/// `%` stays a literal `%`.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Render the LaunchAgent plist. `exe` and `project_root` must be absolute.
/// Tab-indented (what Apple's `plutil` emits) with a trailing newline; the
/// byte-stable test locks the exact text. Only five keys, deliberately:
/// `Label`, `ProgramArguments`, `WorkingDirectory`, `RunAtLoad`, `KeepAlive`.
/// See the module docs for why the stdout/stderr redirection keys, the
/// process-group key, and the environment dict are never emitted.
pub(crate) fn render_plist(exe: &Path, project_root: &Path) -> String {
    let exe = xml_escape(&exe.display().to_string());
    let project_root = xml_escape(&project_root.display().to_string());
    let lines = [
        r#"<?xml version="1.0" encoding="UTF-8"?>"#.to_string(),
        r#"<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">"#
            .to_string(),
        r#"<plist version="1.0">"#.to_string(),
        "<dict>".to_string(),
        "\t<key>Label</key>".to_string(),
        format!("\t<string>{LAUNCHD_LABEL}</string>"),
        "\t<key>ProgramArguments</key>".to_string(),
        "\t<array>".to_string(),
        format!("\t\t<string>{exe}</string>"),
        "\t\t<string>watch</string>".to_string(),
        "\t</array>".to_string(),
        "\t<key>WorkingDirectory</key>".to_string(),
        format!("\t<string>{project_root}</string>"),
        "\t<key>RunAtLoad</key>".to_string(),
        "\t<true/>".to_string(),
        "\t<key>KeepAlive</key>".to_string(),
        "\t<dict>".to_string(),
        "\t\t<key>SuccessfulExit</key>".to_string(),
        "\t\t<false/>".to_string(),
        "\t</dict>".to_string(),
        "</dict>".to_string(),
        "</plist>".to_string(),
    ];
    let mut plist = lines.join("\n");
    plist.push('\n');
    plist
}

/// Render the Task Scheduler 1.2 task definition. `exe` and `project_root`
/// must be absolute, and both go through [`xml_escape`]. Tab-indented like
/// [`render_plist`], with a single trailing newline; the byte-stable test
/// locks the exact text.
///
/// The element set is deliberately minimal. `LogonTrigger` and nothing else —
/// never a `BootTrigger`, because the task has to land in the logged-in
/// user's interactive session, which is where `%USERPROFILE%` and the
/// per-user `~/.agent` are, and never a Windows Service under `sc.exe`.
/// `InteractiveToken` + `LeastPrivilege`, never `HighestAvailable`: a memory
/// daemon has no business asking for elevation. `RestartOnFailure` (`PT1M`,
/// three attempts) is this backend's `Restart=on-failure` /
/// `KeepAlive.SuccessfulExit=false`. There is no `StandardOutput` /
/// `StandardError` node for the reason the plist has no `StandardOutPath`
/// (AILAB-184: the supervised `watch` owns `~/.agent/dreamd.log` through the
/// tracing file layer, which truncates on open, so a second writer aimed at
/// the same file would clobber it), and no `EnvironmentVariables` for the
/// reason the unit has no `Environment=` line (AILAB-584: an inherited empty
/// `HOME` makes every derived path relative).
pub(crate) fn render_task_xml(exe: &Path, project_root: &Path) -> String {
    let exe = xml_escape(&exe.display().to_string());
    let project_root = xml_escape(&project_root.display().to_string());
    let lines = [
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>".to_string(),
        "<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">"
            .to_string(),
        "\t<Triggers>".to_string(),
        "\t\t<LogonTrigger>".to_string(),
        "\t\t\t<Enabled>true</Enabled>".to_string(),
        "\t\t</LogonTrigger>".to_string(),
        "\t</Triggers>".to_string(),
        "\t<Principals>".to_string(),
        "\t\t<Principal id=\"Author\">".to_string(),
        "\t\t\t<LogonType>InteractiveToken</LogonType>".to_string(),
        "\t\t\t<RunLevel>LeastPrivilege</RunLevel>".to_string(),
        "\t\t</Principal>".to_string(),
        "\t</Principals>".to_string(),
        "\t<Settings>".to_string(),
        "\t\t<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>".to_string(),
        "\t\t<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>".to_string(),
        "\t\t<StartWhenAvailable>true</StartWhenAvailable>".to_string(),
        "\t\t<AllowStartOnDemand>true</AllowStartOnDemand>".to_string(),
        "\t\t<RestartOnFailure>".to_string(),
        "\t\t\t<Interval>PT1M</Interval>".to_string(),
        "\t\t\t<Count>3</Count>".to_string(),
        "\t\t</RestartOnFailure>".to_string(),
        "\t</Settings>".to_string(),
        "\t<Actions Context=\"Author\">".to_string(),
        "\t\t<Exec>".to_string(),
        format!("\t\t\t<Command>{exe}</Command>"),
        "\t\t\t<Arguments>watch</Arguments>".to_string(),
        format!("\t\t\t<WorkingDirectory>{project_root}</WorkingDirectory>"),
        "\t\t</Exec>".to_string(),
        "\t</Actions>".to_string(),
        "</Task>".to_string(),
    ];
    let mut xml = lines.join("\n");
    xml.push('\n');
    xml
}

/// Create `path`'s parent directory and write `body` atomically. Every
/// failure is [`ServiceError::Io`] naming the path that could not be created
/// or written.
fn write_service_file(path: &Path, body: &str) -> Result<(), ServiceError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ServiceError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    write_atomic(path, body.as_bytes()).map_err(|source| ServiceError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Create `path`'s parent and write `body` with a plain `std::fs::write`.
/// Every failure is [`ServiceError::Io`] naming the path that could not be
/// created or written — the same contract as [`write_service_file`].
///
/// It is deliberately not [`write_service_file`]: that goes through
/// `dreamd_core::io::write_atomic`, which is `ErrorKind::Unsupported` on
/// Windows (AILAB-203 does not implement it; that is still v0.1.1), so
/// routing the task XML or `auth.json` through it would fail `service
/// install` on the one OS that needs those two files. Nothing is being given
/// up that matters here: neither file has a concurrent writer — `install` is
/// a one-shot, and the token turns over only under another
/// `install --force` — so the crash-atomicity the durable helper buys has no
/// reader to protect.
fn write_plain_file(path: &Path, body: &[u8]) -> Result<(), ServiceError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ServiceError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(path, body).map_err(|source| ServiceError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Render + write the unit file (creates parents). Returns the path written.
/// Never spawns anything — this is the unit-testable half of `install`.
pub(crate) fn write_user_unit(
    home: &Path,
    exe: &Path,
    project_root: &Path,
) -> Result<PathBuf, ServiceError> {
    let path = unit_path(home);
    write_service_file(&path, &render_user_unit(exe, project_root))?;
    Ok(path)
}

/// Render + write the LaunchAgent plist (creates `~/Library/LaunchAgents`).
/// Returns the path written. Never spawns anything; the `--force` gate lives
/// in the caller, so this overwrites unconditionally like [`write_user_unit`].
pub(crate) fn write_plist(
    home: &Path,
    exe: &Path,
    project_root: &Path,
) -> Result<PathBuf, ServiceError> {
    let path = plist_path(home);
    write_service_file(&path, &render_plist(exe, project_root))?;
    Ok(path)
}

/// Mint 32 cryptographically random bytes (256 bits), write them as compact
/// JSON — `{"token":"<64 lowercase hex chars>"}` plus a trailing newline — to
/// `<home>/.agent/auth.json`, then hand the file to `icacls` so only `sid`
/// can read or rewrite it. Returns the path written.
///
/// The token is never printed and never logged; only the path is, and only by
/// the caller. AILAB-192 is the consumer (localhost TCP + a bearer header);
/// rotating the token at writer start-up is 192's job too, so on this ticket
/// it turns over exactly when `service install` runs — which `--force` makes
/// a deliberate act rather than an accidental one.
///
/// The path comes from [`DaemonHome::auth_json`] rather than a local join, so
/// the daemon-home layout stays stated in one place. `home.join(".agent")` is
/// the daemon home and never a per-project store (`layout.rs`'s
/// `daemon_home_is_never_inside_a_project_store`), which is precisely what
/// keeps a bearer token out of somebody's git history.
///
/// The ACL argv strips inheritance first (`/inheritance:r`), because that is
/// what removes the inherited `Users` ACE, then grants the owner SID **full**
/// control (`/grant:r <sid>:F`) — read alone would leave the next
/// `install --force` unable to rewrite the file it had just created. On an
/// `icacls` failure the file stays on disk, exactly as the unit stays on disk
/// when `systemctl` fails: the caller can inspect it and rerun.
fn write_auth_token(home: &Path, sid: &str, icacls: Icacls<'_>) -> Result<PathBuf, ServiceError> {
    let path = DaemonHome::new(home.join(".agent")).auth_json();
    let mut bytes = [0u8; 32];
    // The OS CSPRNG, not a seeded PRNG: this is a credential. A failure here
    // is reported against the path the token was headed for, because that is
    // the file the user is being told does not exist.
    getrandom::getrandom(&mut bytes).map_err(|e| ServiceError::Io {
        path: path.clone(),
        source: std::io::Error::other(format!("could not read 32 random bytes: {e}")),
    })?;
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Hand-rolled lowercase hex rather than a `hex` crate: 64 characters
        // are not worth a dependency (NFR-2). Writing into a `String` cannot
        // fail — `fmt::Write` returns a `Result` only because the trait is
        // shared with formatters that can.
        let _ = write!(&mut token, "{byte:02x}");
    }
    // Hand-built, compact, single trailing newline — the body is a locked
    // wire format AILAB-192 parses, so it is not left to a serializer's
    // whitespace defaults.
    write_plain_file(&path, format!("{{\"token\":\"{token}\"}}\n").as_bytes())?;
    icacls(&[
        &path.display().to_string(),
        "/inheritance:r",
        "/grant:r",
        &format!("{sid}:F"),
    ])?;
    Ok(path)
}

/// The installing user's SID for the `auth.json` ACL, read from
/// `whoami /user` stdout — the Windows analogue of [`current_uid`]'s `id -u`,
/// and chosen for the same reason: spawning the OS's own utility keeps the
/// CLI crate free of FFI and of `windows-sys` (NFR-2). The cost is one extra
/// process per Windows `service install`, which is a one-shot anyway.
///
/// `whoami /user` prints a localised header block and then a
/// `DOMAIN\user  S-1-5-21-…` row, so the parse is "the first
/// whitespace-separated token beginning `S-1-`" rather than a column index:
/// the header wording and the column widths both move, the SID prefix does
/// not.
fn current_sid() -> Result<String, ServiceError> {
    let output = std::process::Command::new("whoami")
        .arg("/user")
        .output()
        .map_err(|source| ServiceError::Spawn {
            program: "whoami",
            source,
        })?;
    if !output.status.success() {
        return Err(ServiceError::Sid(format!(
            "`whoami /user` failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .split_whitespace()
        .find(|word| word.starts_with("S-1-"))
        .map(str::to_string)
        .ok_or_else(|| {
            ServiceError::Sid(format!(
                "no S-1- SID in `whoami /user` output {:?}",
                stdout.trim()
            ))
        })
}

/// Absolute, symlink-resolved path of this binary for `ExecStart=` /
/// `ProgramArguments`.
fn current_exe_canonical() -> Result<PathBuf, ServiceError> {
    std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(ServiceError::CurrentExe)
}

/// Full install: pick the backend, then run its install path.
///
/// Linux: probe passed → [`run_install_with`], and `force` is ignored (the
/// unit is silently overwritten, exactly as AILAB-190 shipped). macOS:
/// [`run_launchd_install_with`] with the uid from `id -u`. Windows:
/// [`run_schtasks_install_with`], where `force` is the overwrite gate it is
/// on macOS. No backend → [`ServiceError::NoSystemd`], nothing written.
///
/// `current_sid` is passed **unapplied** — a resolver, not a value — for the
/// same reason `current_uid()?` sits inside the launchd arm: evaluated as a
/// call argument it would spawn `whoami` on Linux and macOS too, and ahead of
/// the no-backend refusal at that.
pub fn run_install(cwd: &Path, home: &Path, force: bool) -> Result<(), ServiceError> {
    match detect_backend() {
        None => Err(ServiceError::NoSystemd),
        Some(ServiceBackend::Systemd) => {
            run_install_with(cwd, home, true, current_exe_canonical, &mut systemctl_user)
        }
        Some(ServiceBackend::Launchd) => run_launchd_install_with(
            cwd,
            home,
            force,
            current_uid()?,
            current_exe_canonical,
            &mut launchctl,
        ),
        Some(ServiceBackend::Schtasks) => run_schtasks_install_with(
            cwd,
            home,
            force,
            current_exe_canonical,
            current_sid,
            &mut schtasks,
            &mut icacls,
        ),
    }
}

/// Same as [`run_install`] with the probe result, the exe resolver, and the
/// `systemctl` runner injected. Tests pass a fake exe and a recording or
/// panicking runner, so they never reach a real `systemctl`.
///
/// Order is load-bearing: the two usage refusals (no systemd, no project root)
/// come first and nothing is written before both pass; the exe is resolved
/// only after the project-root check so a `CurrentExe` failure (exit 1) can
/// never mask `NoProjectRoot` (exit 2). On a `systemctl` failure the unit file
/// stays on disk so the user can inspect it or retry with
/// `dreamd service start`.
pub(crate) fn run_install_with(
    cwd: &Path,
    home: &Path,
    systemd_available: bool,
    resolve_exe: impl FnOnce() -> Result<PathBuf, ServiceError>,
    systemctl: Systemctl<'_>,
) -> Result<(), ServiceError> {
    if !systemd_available {
        return Err(ServiceError::NoSystemd);
    }
    let root = match AgentRoot::discover(cwd) {
        Ok(root) => root,
        Err(LayoutError::NotFound) => return Err(ServiceError::NoProjectRoot),
    };
    let exe = resolve_exe()?;
    let path = write_user_unit(home, &exe, root.project_root())?;
    println!("wrote {}", path.display());
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", UNIT_NAME])?;
    println!("enabled and started {UNIT_NAME} (systemctl --user)");
    Ok(())
}

/// `dreamd service start`: pick the backend, then `systemctl --user start
/// dreamd.service` (Linux), `launchctl kickstart -k gui/<uid>/<label>`
/// (macOS), or `schtasks /Run /TN <name>` (Windows). No backend →
/// [`ServiceError::NoSystemd`].
pub fn run_start() -> Result<(), ServiceError> {
    match detect_backend() {
        None => Err(ServiceError::NoSystemd),
        Some(ServiceBackend::Systemd) => run_start_with(true, &mut systemctl_user),
        Some(ServiceBackend::Launchd) => run_launchd_start_with(current_uid()?, &mut launchctl),
        Some(ServiceBackend::Schtasks) => run_schtasks_start_with(&mut schtasks),
    }
}

/// `dreamd service restart` (AILAB-185): pick the backend, then `systemctl
/// --user restart dreamd.service` (Linux), the same `launchctl kickstart -k
/// gui/<uid>/<label>` argv `start` issues (macOS — `-k` already kills the
/// running instance first, so it *is* the bounce), or a best-effort `/End`
/// followed by `/Run` (Windows, which has no one-call bounce). No backend →
/// [`ServiceError::NoSystemd`]. Bounce-only on all three: the unit / plist /
/// task XML is never rewritten, so a binary that moved (an npx cache bump)
/// still needs `dreamd service install` first.
pub fn run_restart() -> Result<(), ServiceError> {
    match detect_backend() {
        None => Err(ServiceError::NoSystemd),
        Some(ServiceBackend::Systemd) => run_restart_with(true, &mut systemctl_user),
        Some(ServiceBackend::Launchd) => run_launchd_restart_with(current_uid()?, &mut launchctl),
        Some(ServiceBackend::Schtasks) => run_schtasks_restart_with(&mut schtasks),
    }
}

/// macOS install with the uid, the exe resolver, and the `launchctl` runner
/// injected. Tests pass `uid = 501`, a fake exe, and a recording or panicking
/// runner, so they never reach a real `launchctl` — and they run on Linux CI.
///
/// Order is load-bearing, mirroring [`run_install_with`]: the project-root
/// refusal comes first; the exe is resolved next so a `CurrentExe` failure
/// (exit 1) can never mask `NoProjectRoot` (exit 2); then, if the plist
/// already exists and `force` is off, [`ServiceError::PlistExists`] (exit 2)
/// with nothing rewritten and `launchctl` untouched. Only after all three
/// gates is the plist written. With `force` a best-effort `bootout` precedes
/// the `bootstrap` so launchd drops the old definition; the `bootstrap` must
/// succeed. On a `launchctl` failure the plist stays on disk so the user can
/// inspect it or retry with `dreamd service start`.
pub(crate) fn run_launchd_install_with(
    cwd: &Path,
    home: &Path,
    force: bool,
    uid: u32,
    resolve_exe: impl FnOnce() -> Result<PathBuf, ServiceError>,
    launchctl: Launchctl<'_>,
) -> Result<(), ServiceError> {
    let root = match AgentRoot::discover(cwd) {
        Ok(root) => root,
        Err(LayoutError::NotFound) => return Err(ServiceError::NoProjectRoot),
    };
    let exe = resolve_exe()?;
    let path = plist_path(home);
    if path.exists() && !force {
        return Err(ServiceError::PlistExists(path));
    }
    let path = write_plist(home, &exe, root.project_root())?;
    println!("wrote {}", path.display());
    let domain = format!("gui/{uid}");
    if force {
        // Best effort, errors ignored: the label may well not be loaded yet
        // (a plist written by an earlier install whose bootstrap failed, or
        // one booted out by hand), and `bootout` of an unloaded service exits
        // non-zero. The `bootstrap` below is the call that must succeed.
        let _ = launchctl(&["bootout", &format!("{domain}/{LAUNCHD_LABEL}")]);
    }
    launchctl(&["bootstrap", &domain, &path.display().to_string()])?;
    println!("loaded {LAUNCHD_LABEL} (launchctl bootstrap {domain})");
    Ok(())
}

/// macOS `dreamd service start` with the uid and the `launchctl` runner
/// injected: `launchctl kickstart -k gui/<uid>/<label>`. `-k` kills a running
/// instance first so the supervised `watch` restarts rather than no-ops. A
/// label that was never bootstrapped is a [`ServiceError::Launchctl`] (exit 1).
pub(crate) fn run_launchd_start_with(
    uid: u32,
    launchctl: Launchctl<'_>,
) -> Result<(), ServiceError> {
    launchctl(&["kickstart", "-k", &format!("gui/{uid}/{LAUNCHD_LABEL}")])?;
    println!("started {LAUNCHD_LABEL} (launchctl kickstart gui/{uid})");
    Ok(())
}

/// macOS `dreamd service restart` (AILAB-185) with the uid and the `launchctl`
/// runner injected, the twin of the `start` helper above — and issuing the
/// **byte-identical** `kickstart -k gui/<uid>/<label>` argv, because `-k`
/// already kills the running instance before relaunching it, so start's argv
/// *is* the bounce; only the printed verb differs, which is why this is its own
/// helper and never delegates to the `start` one (whose stdout would say
/// `started`). Nothing is rewritten: the plist's `ProgramArguments` stays the
/// path `service install` captured. A label that was never bootstrapped is a
/// [`ServiceError::Launchctl`] (exit 1)
/// — deliberately the same failure `start` gives, not a distinct
/// `NotInstalled`. Tests pass a recording or panicking runner, so they never
/// reach a real `launchctl`, and the Darwin path runs on Linux CI.
pub(crate) fn run_launchd_restart_with(
    uid: u32,
    launchctl: Launchctl<'_>,
) -> Result<(), ServiceError> {
    launchctl(&["kickstart", "-k", &format!("gui/{uid}/{LAUNCHD_LABEL}")])?;
    println!("restarted {LAUNCHD_LABEL} (launchctl kickstart gui/{uid})");
    Ok(())
}

/// Windows install with the exe resolver, the SID resolver and both process
/// runners injected. Tests pass a fake exe, a fake SID and recording or
/// panicking runners, so they never register a real scheduled task or rewrite
/// a real ACL — and, like the Darwin helpers, they run on Linux CI.
///
/// Order is load-bearing, mirroring [`run_launchd_install_with`]: the
/// project-root refusal comes first and nothing is written before it passes;
/// the exe is resolved next so a `CurrentExe` failure (exit 1) can never mask
/// `NoProjectRoot` (exit 2); then, if the task XML already exists and `force`
/// is off, [`ServiceError::TaskExists`] (exit 2) with nothing rewritten and
/// both runners unspawned. That third gate carries one more promise than the
/// Darwin one: an existing `auth.json` is left exactly as it was, so a bare
/// reinstall can never silently invalidate a token AILAB-192 already handed
/// out. Only past all three gates is the XML written, the token minted and
/// the task registered.
///
/// `/F` rides along only under `force`, and it is the *only* difference in
/// the registration argv: `schtasks /Create` refuses an existing task name
/// without it, so the flag is the same confirmation `--force` already is for
/// the plist. There is deliberately never a `/Run` here — see the module docs
/// — because `dreamd watch` on Windows still exits 2 until AILAB-192, and the
/// logon trigger is what starts it once that lands. On a `schtasks` failure
/// the XML and the token stay on disk, exactly as the unit stays on disk when
/// `systemctl` fails.
pub(crate) fn run_schtasks_install_with(
    cwd: &Path,
    home: &Path,
    force: bool,
    resolve_exe: impl FnOnce() -> Result<PathBuf, ServiceError>,
    resolve_sid: impl FnOnce() -> Result<String, ServiceError>,
    schtasks: Schtasks<'_>,
    icacls: Icacls<'_>,
) -> Result<(), ServiceError> {
    let root = match AgentRoot::discover(cwd) {
        Ok(root) => root,
        Err(LayoutError::NotFound) => return Err(ServiceError::NoProjectRoot),
    };
    let exe = resolve_exe()?;
    let xml = task_xml_path(home);
    if xml.exists() && !force {
        return Err(ServiceError::TaskExists(xml));
    }
    write_plain_file(&xml, render_task_xml(&exe, root.project_root()).as_bytes())?;
    let sid = resolve_sid()?;
    let auth = write_auth_token(home, &sid, icacls)?;
    let xml_arg = xml.display().to_string();
    let mut argv = vec!["/Create", "/TN", SCHTASKS_TASK_NAME, "/XML", &xml_arg];
    if force {
        argv.push("/F");
    }
    schtasks(&argv)?;
    println!("wrote {}", xml.display());
    println!("wrote {}", auth.display());
    println!("registered {SCHTASKS_TASK_NAME} (schtasks /Create)");
    Ok(())
}

/// Windows `dreamd service start` with the `schtasks` runner injected:
/// `schtasks /Run /TN dev.dreamd.dreamd`. A task name the scheduler does not
/// have is a [`ServiceError::Schtasks`] (exit 1) — deliberately the same
/// failure `start` gives on the other two backends, not a distinct
/// `NotInstalled`.
///
/// What this surfaces is **schtasks'** exit, not the supervised process's.
/// Until AILAB-192 a perfectly successful start still leaves `service status`
/// at `stopped`, because `dreamd watch` on Windows exits 2 the moment it
/// runs. That is expected, and this verb deliberately makes no attempt to
/// detect it: special-casing the exit code of a process the scheduler owns
/// would be inventing supervision the OS already does.
pub(crate) fn run_schtasks_start_with(schtasks: Schtasks<'_>) -> Result<(), ServiceError> {
    schtasks(&["/Run", "/TN", SCHTASKS_TASK_NAME])?;
    println!("started {SCHTASKS_TASK_NAME} (schtasks /Run)");
    Ok(())
}

/// Windows `dreamd service restart` (AILAB-185's verb) with the `schtasks`
/// runner injected, the twin of the `start` helper above — and, unlike
/// Darwin's, genuinely two calls: the Task Scheduler has no `kickstart -k`
/// that kills before relaunching, so the bounce is `/End` then `/Run`.
///
/// The `/End` is best-effort and issued unconditionally, its result discarded
/// for exactly the reason the `bootout` in [`run_launchd_uninstall_with`] is:
/// ending a task that is not running exits non-zero, and on this backend that
/// is the *common* case — until AILAB-192 the supervised `watch` exits 2
/// immediately, so the task is nearly always idle. The `/Run` is the call
/// that must succeed, and a missing task fails there, exactly as it does in
/// `start`.
///
/// Bounce-only, like the other two backends: the task XML is never rewritten,
/// so `Command` / `WorkingDirectory` stay whatever `service install`
/// captured, and the `auth.json` token is not rotated — rotation is
/// `install --force`, and at writer start-up it is AILAB-192.
pub(crate) fn run_schtasks_restart_with(schtasks: Schtasks<'_>) -> Result<(), ServiceError> {
    let _ = schtasks(&["/End", "/TN", SCHTASKS_TASK_NAME]);
    schtasks(&["/Run", "/TN", SCHTASKS_TASK_NAME])?;
    println!("restarted {SCHTASKS_TASK_NAME} (schtasks /Run)");
    Ok(())
}

/// Same as [`run_start`] with the probe result and the `systemctl` runner
/// injected (tests pass a recording or panicking runner and never spawn).
pub(crate) fn run_start_with(
    systemd_available: bool,
    systemctl: Systemctl<'_>,
) -> Result<(), ServiceError> {
    if !systemd_available {
        return Err(ServiceError::NoSystemd);
    }
    systemctl(&["start", UNIT_NAME])?;
    println!("started {UNIT_NAME} (systemctl --user)");
    Ok(())
}

/// Same as [`run_restart`] with the probe result and the `systemctl` runner
/// injected (tests pass a recording or panicking runner and never spawn), the
/// twin of [`run_start_with`]. The argv is `restart` — never `try-restart`,
/// which would silently no-op on a stopped unit, and never the pager-bound
/// `status` verb. Bounce-only: the unit file is not rewritten, so `ExecStart`
/// stays whatever `service install` captured. A unit that was never installed
/// is a [`ServiceError::Systemctl`] (exit 1) — deliberately the same failure
/// `start` gives, not a distinct `NotInstalled`.
pub(crate) fn run_restart_with(
    systemd_available: bool,
    systemctl: Systemctl<'_>,
) -> Result<(), ServiceError> {
    if !systemd_available {
        return Err(ServiceError::NoSystemd);
    }
    systemctl(&["restart", UNIT_NAME])?;
    println!("restarted {UNIT_NAME} (systemctl --user)");
    Ok(())
}

/// `dreamd service uninstall` (AILAB-202): pick the backend, then tear down
/// its service entry. Linux → [`run_uninstall_with`], macOS →
/// [`run_launchd_uninstall_with`] with the uid from `id -u`, Windows →
/// [`run_schtasks_uninstall_with`]. No backend →
/// [`ServiceError::NoSystemd`], nothing removed and nothing prompted: a host
/// with no supervisor has no unit to take away.
///
/// `current_uid()?` sits inside the launchd arm for the same reason as in
/// [`run_install`]: as a call argument it would spawn `id` on Linux too.
pub fn run_uninstall(
    home: &Path,
    daemon_home: &Path,
    purge: bool,
    yes: bool,
) -> Result<(), ServiceError> {
    match detect_backend() {
        None => Err(ServiceError::NoSystemd),
        Some(ServiceBackend::Systemd) => run_uninstall_with(
            home,
            daemon_home,
            true,
            purge,
            yes,
            &mut confirm_on_tty,
            &mut systemctl_user,
        ),
        Some(ServiceBackend::Launchd) => run_launchd_uninstall_with(
            home,
            daemon_home,
            purge,
            yes,
            current_uid()?,
            &mut confirm_on_tty,
            &mut launchctl,
        ),
        Some(ServiceBackend::Schtasks) => run_schtasks_uninstall_with(
            home,
            daemon_home,
            purge,
            yes,
            &mut confirm_on_tty,
            &mut schtasks,
        ),
    }
}

/// Same as [`run_uninstall`] on Linux with the probe result, the confirmation
/// reader and the `systemctl` runner injected.
///
/// Order is load-bearing, and differs from the other verbs in one way: the
/// destructive confirmation comes **before** any mutation, so a refused or
/// declined `--purge` leaves the unit installed and `systemctl` unspawned.
/// The no-backend refusal still comes first of all — there is nothing to
/// remove on such a host, so it must not prompt either.
///
/// The teardown itself is idempotent, unlike [`run_start_with`]: with no unit
/// file on disk the runner is never called at all (there is nothing to
/// `disable` and nothing for `daemon-reload` to pick up) and the report says
/// `removed: (none)`. The unit file is unlinked in-process between `disable
/// --now` and `daemon-reload` — never through `systemctl`, which has no verb
/// for it.
pub(crate) fn run_uninstall_with(
    home: &Path,
    daemon_home: &Path,
    systemd_available: bool,
    purge: bool,
    yes: bool,
    confirm: Confirm<'_>,
    systemctl: Systemctl<'_>,
) -> Result<(), ServiceError> {
    if !systemd_available {
        return Err(ServiceError::NoSystemd);
    }
    confirm_purge(daemon_home, purge, yes, confirm)?;
    let unit = unit_path(home);
    let removed = if unit.exists() {
        systemctl(&["disable", "--now", UNIT_NAME])?;
        remove_service_file(&unit)?;
        systemctl(&["daemon-reload"])?;
        Some(unit)
    } else {
        None
    };
    finish_uninstall(removed.as_deref(), daemon_home, purge)
}

/// Same as [`run_uninstall`] on macOS with the uid, the confirmation reader
/// and the `launchctl` runner injected — so the Darwin path runs on Linux CI,
/// exactly like the install / start / restart helpers.
///
/// The confirmation comes first here too. The `bootout` is best-effort and
/// issued unconditionally: `bootout` of a label launchd never loaded exits
/// non-zero, and so does one whose plist was already deleted by hand, but in
/// both cases the end state is the one the user asked for. That is the same
/// reasoning as the `--force` bootout in [`run_launchd_install_with`], and it
/// is why the result is discarded rather than propagated. Only the plist
/// unlink can fail the verb.
pub(crate) fn run_launchd_uninstall_with(
    home: &Path,
    daemon_home: &Path,
    purge: bool,
    yes: bool,
    uid: u32,
    confirm: Confirm<'_>,
    launchctl: Launchctl<'_>,
) -> Result<(), ServiceError> {
    confirm_purge(daemon_home, purge, yes, confirm)?;
    let _ = launchctl(&["bootout", &format!("gui/{uid}/{LAUNCHD_LABEL}")]);
    let plist = plist_path(home);
    let removed = if plist.exists() {
        remove_service_file(&plist)?;
        Some(plist)
    } else {
        None
    };
    finish_uninstall(removed.as_deref(), daemon_home, purge)
}

/// Same as [`run_uninstall`] on Windows with the confirmation reader and the
/// `schtasks` runner injected — so this path runs on Linux CI too, exactly
/// like the install / start / restart helpers above it.
///
/// The confirmation comes first here as well. The `/Delete` is best-effort
/// and issued unconditionally, on the same reasoning as the Darwin `bootout`:
/// deleting a task the scheduler never had exits non-zero, and so does one
/// whose XML was removed by hand, but in both cases the end state is the one
/// the user asked for. Its `/F` is the *no-prompt* flag, not the install
/// overwrite gate — `schtasks /Delete` otherwise asks for a keystroke on
/// stdin, which a one-shot verb must never do. Only the XML unlink can fail
/// the verb.
///
/// `auth.json` is deliberately left alone: it lives in the daemon home, and
/// the only thing on this verb that removes anything from there is `--purge`,
/// which takes the whole directory through [`finish_uninstall`]. A Windows
/// `uninstall` without `--purge` therefore deletes no data whatsoever, and
/// the `preserved:` lines say so.
pub(crate) fn run_schtasks_uninstall_with(
    home: &Path,
    daemon_home: &Path,
    purge: bool,
    yes: bool,
    confirm: Confirm<'_>,
    schtasks: Schtasks<'_>,
) -> Result<(), ServiceError> {
    confirm_purge(daemon_home, purge, yes, confirm)?;
    let _ = schtasks(&["/Delete", "/TN", SCHTASKS_TASK_NAME, "/F"]);
    let xml = task_xml_path(home);
    let removed = if xml.exists() {
        remove_service_file(&xml)?;
        Some(xml)
    } else {
        None
    };
    finish_uninstall(removed.as_deref(), daemon_home, purge)
}

/// The `--purge` gate, run before any mutation. A no-op without `--purge`
/// (there is nothing destructive to confirm) and with `--yes` (the flag *is*
/// the confirmation, exactly as on `reset workspace` and on the macOS install
/// `--force`). `--yes` without `--purge` therefore lands here and is ignored.
///
/// Otherwise this is the `reset workspace` decision verbatim: non-tty stdin is
/// refused rather than silently obeyed in a pipeline, and only `y` / `Y`
/// proceeds.
///
/// Neither refusal is printed here. The user-facing wording is carried by
/// [`ServiceError::NotATty`] and [`ServiceError::Declined`], and printed
/// exactly once by `cli::service_exit` — the same single-print contract
/// `reset workspace` has, reached from the other end: there the copy is
/// written inside `reset::run_workspace` and the caller returns a bare exit 2,
/// here the copy lives in `Display` and the caller is the only writer.
fn confirm_purge(
    daemon_home: &Path,
    purge: bool,
    yes: bool,
    confirm: Confirm<'_>,
) -> Result<(), ServiceError> {
    if !purge || yes {
        return Ok(());
    }
    let prompt = format!("Delete the daemon home {}? [y/N] ", daemon_home.display());
    let Some(line) = confirm(&prompt)? else {
        return Err(ServiceError::NotATty);
    };
    let answer = line.trim();
    if answer == "y" || answer == "Y" {
        return Ok(());
    }
    Err(ServiceError::Declined)
}

/// The production [`Confirm`]: `None` when stdin is not a tty, otherwise the
/// prompt on **stderr** (stdout stays the machine-readable report) and one
/// line from stdin. Unlocked `eprint!` — this one-shot opens no Tantivy index,
/// so there is no lock to hoist across the blocking read.
fn confirm_on_tty(prompt: &str) -> Result<Option<String>, ServiceError> {
    if !std::io::stdin().is_terminal() {
        return Ok(None);
    }
    eprint!("{prompt}");
    std::io::stderr().flush().map_err(ServiceError::Stdin)?;
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(ServiceError::Stdin)?;
    Ok(Some(line))
}

/// Unlink one service file, naming it on failure. Deliberately not
/// `remove_dir_all`: this only ever takes the unit file, the plist or the
/// task XML.
fn remove_service_file(path: &Path) -> Result<(), ServiceError> {
    std::fs::remove_file(path).map_err(|source| ServiceError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Purge (if it was confirmed), then print the report. Purge goes first so a
/// failed delete never gets announced as a success.
fn finish_uninstall(
    removed: Option<&Path>,
    daemon_home: &Path,
    purge: bool,
) -> Result<(), ServiceError> {
    if purge {
        remove_daemon_home(daemon_home)?;
    }
    // Unlocked `print!`: this one-shot never opens a Tantivy index, so there
    // is no lock to hoist (same as `run_status`).
    print!("{}", render_uninstall_report(removed, daemon_home, purge));
    Ok(())
}

/// `--purge`: delete the daemon home and nothing else.
///
/// The one `remove_dir_all` in this module, and it is reached only through
/// [`confirm_purge`]. `daemon_home` is `$HOME/.agent` — the registry, the
/// socket and `dreamd.log` — never a project root: nothing here discovers an
/// [`AgentRoot`] or joins `.agent` onto a cwd, so a per-project
/// `<repo>/.agent/` store cannot be reached from this path. An already-absent
/// directory is success, which keeps `--purge` idempotent alongside the
/// teardown.
fn remove_daemon_home(daemon_home: &Path) -> Result<(), ServiceError> {
    match std::fs::remove_dir_all(daemon_home) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(ServiceError::Io {
            path: daemon_home.to_path_buf(),
            source,
        }),
    }
}

/// Render the locked `uninstall` stdout (byte-tested): what went away, then
/// what did not. The daemon-home line flips from `preserved:` to `removed:`
/// under `--purge`; the per-project line is unconditional, because no flag on
/// this verb ever deletes a project store and the output is where that promise
/// is visible.
pub(crate) fn render_uninstall_report(
    removed: Option<&Path>,
    daemon_home: &Path,
    purged: bool,
) -> String {
    let removed = removed.map_or_else(|| "(none)".to_string(), |path| path.display().to_string());
    let daemon_home = daemon_home.display();
    let lines = [
        format!("removed: {removed}"),
        if purged {
            format!("removed: {daemon_home} (daemon home)")
        } else {
            format!("preserved: {daemon_home} (daemon home)")
        },
        "preserved: per-project .agent/ stores".to_string(),
    ];
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// `dreamd service status` (AILAB-178): pick the backend, query it, print the
/// report with the pre-read log tail. Never opens the log itself and never
/// touches the tracing file layer — `log_tail` comes from
/// `status::read_log_tail_n` in `cli::run_service_status`. No backend →
/// [`ServiceError::NoSystemd`] before any process is spawned. Unlocked
/// `print!`: this one-shot never opens a Tantivy index, so there is no lock
/// to hoist.
pub fn run_status(home: &Path, log_tail: &[String]) -> Result<(), ServiceError> {
    let report = run_status_with(
        detect_backend(),
        home,
        current_uid,
        &mut systemctl_user_query,
        &mut launchctl_query,
        &mut schtasks_query,
    )?;
    print!("{}", render_report(&report, log_tail));
    Ok(())
}

/// Same as [`run_status`] with the backend pick, the uid resolver, and all
/// three query runners injected. `resolve_uid` is only ever called on the
/// launchd arm: passing `current_uid()?` as a call argument would spawn `id`
/// on Linux too, and ahead of the no-backend refusal — the AILAB-169 shape
/// this deliberately avoids. Tests pass fixtures and panicking runners and
/// never spawn anything.
///
/// The third runner is why this signature grew a parameter rather than the
/// Windows arm being answered somewhere outside it. This function must stay
/// exhaustive over [`ServiceBackend`], and the two ways to avoid the extra
/// argument both cost the same thing: an arm that panics, or an arm reachable
/// only from production [`run_status`], is a Windows path that no test on
/// Linux ever executes. Every other backend here is covered on Linux CI
/// *because* its runner is injected; schtasks gets the identical treatment,
/// and the existing callers pay one `&mut never_query`.
pub(crate) fn run_status_with(
    backend: Option<ServiceBackend>,
    home: &Path,
    resolve_uid: impl FnOnce() -> Result<u32, ServiceError>,
    systemctl: Query<'_>,
    launchctl: Query<'_>,
    schtasks: Query<'_>,
) -> Result<ServiceReport, ServiceError> {
    match backend {
        None => Err(ServiceError::NoSystemd),
        Some(ServiceBackend::Systemd) => query_systemd_status(systemctl),
        Some(ServiceBackend::Launchd) => query_launchd_status(home, resolve_uid, launchctl),
        Some(ServiceBackend::Schtasks) => query_schtasks_status(home, schtasks),
    }
}

/// Linux: `systemctl --user show dreamd.service --no-pager -p …`, a stable
/// `Key=Value` dump — never the human-oriented `status` verb (paged,
/// coloured, localised). systemd exits 0 for an unknown unit and answers
/// `LoadState=not-found`, so a runner `Err` here is a genuine failure and
/// propagates. `SubState` is part of the locked argv but is not rendered.
pub(crate) fn query_systemd_status(query: Query<'_>) -> Result<ServiceReport, ServiceError> {
    let stdout = query(&[
        "show",
        UNIT_NAME,
        "--no-pager",
        "-p",
        "LoadState",
        "-p",
        "ActiveState",
        "-p",
        "SubState",
        "-p",
        "MainPID",
        "-p",
        "ActiveEnterTimestamp",
    ])?;
    Ok(parse_systemd_show(&stdout))
}

/// Parse `systemctl show` output into a report. `LoadState=not-found` wins
/// (there is no unit at all); otherwise `ActiveState` decides — `failed`,
/// `active` → running, anything else (`inactive`, `activating`,
/// `deactivating`, …) → stopped. `MainPID` is `None` when `0`, absent, or
/// unparseable; `ActiveEnterTimestamp` is passed through raw when non-empty.
pub(crate) fn parse_systemd_show(stdout: &str) -> ServiceReport {
    let mut load_state = "";
    let mut active_state = "";
    let mut main_pid = "";
    let mut active_since = "";
    for line in stdout.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "LoadState" => load_state = value,
            "ActiveState" => active_state = value,
            "MainPID" => main_pid = value,
            "ActiveEnterTimestamp" => active_since = value,
            _ => {}
        }
    }
    let state = if load_state == "not-found" {
        ServiceState::NotInstalled
    } else {
        match active_state {
            "failed" => ServiceState::Failed,
            "active" => ServiceState::Running,
            _ => ServiceState::Stopped,
        }
    };
    ServiceReport {
        backend: ServiceBackend::Systemd,
        state,
        pid: main_pid.parse::<u32>().ok().filter(|pid| *pid != 0),
        active_since: (!active_since.is_empty()).then(|| active_since.to_string()),
    }
}

/// macOS: `launchctl print gui/<uid>/dev.dreamd.dreamd`. A non-zero exit is
/// what launchd says for a label that is not loaded — a state answer, not a
/// [`ServiceError::Launchctl`] failure: stopped when the plist is on disk
/// (installed, not loaded), not-installed when it is absent. Any other error
/// (a `launchctl` that could not be spawned, an unreadable uid) propagates.
/// `active_since` is always `None`: `launchctl print` has no start timestamp.
pub(crate) fn query_launchd_status(
    home: &Path,
    resolve_uid: impl FnOnce() -> Result<u32, ServiceError>,
    query: Query<'_>,
) -> Result<ServiceReport, ServiceError> {
    let uid = resolve_uid()?;
    let (state, pid) = match query(&["print", &format!("gui/{uid}/{LAUNCHD_LABEL}")]) {
        Ok(stdout) => parse_launchctl_print(&stdout),
        Err(ServiceError::Launchctl { .. }) => {
            let state = if plist_path(home).exists() {
                ServiceState::Stopped
            } else {
                ServiceState::NotInstalled
            };
            (state, None)
        }
        Err(other) => return Err(other),
    };
    Ok(ServiceReport {
        backend: ServiceBackend::Launchd,
        state,
        pid,
        active_since: None,
    })
}

/// Parse `launchctl print` output: the first `state = …` line (`running` →
/// running; any other word, or no such line → stopped) and the first
/// `pid = …` line (unparseable → `None`). Lines are trimmed on both sides,
/// so the tab-indented dump and a flat one parse alike.
pub(crate) fn parse_launchctl_print(stdout: &str) -> (ServiceState, Option<u32>) {
    fn first<'a>(stdout: &'a str, key: &str) -> Option<&'a str> {
        stdout
            .lines()
            .map(str::trim)
            .find_map(|line| line.strip_prefix(key))
            .map(str::trim)
    }
    let state = match first(stdout, "state = ") {
        Some("running") => ServiceState::Running,
        _ => ServiceState::Stopped,
    };
    let pid = first(stdout, "pid = ").and_then(|word| word.parse::<u32>().ok());
    (state, pid)
}

/// Windows: `schtasks /Query /TN dev.dreamd.dreamd /FO LIST /V`. `/FO LIST`
/// pins the output to a `Key: Value` block — never the default table, whose
/// columns are truncated to the console width, and never a pager. A non-zero
/// exit is what schtasks says for a task name it does not have: a state
/// answer, not a [`ServiceError::Schtasks`] failure — stopped when the XML is
/// on disk (written, never registered), not-installed when it is absent.
/// That is the [`query_launchd_status`] shape verbatim. Any other error (a
/// `schtasks` that could not be spawned at all) propagates.
///
/// `pid` and `active_since` are always `None`: this view reports neither, and
/// [`render_report`] prints `-` for both.
pub(crate) fn query_schtasks_status(
    home: &Path,
    query: Query<'_>,
) -> Result<ServiceReport, ServiceError> {
    let state = match query(&["/Query", "/TN", SCHTASKS_TASK_NAME, "/FO", "LIST", "/V"]) {
        Ok(stdout) => parse_schtasks_query(&stdout),
        Err(ServiceError::Schtasks { .. }) => {
            if task_xml_path(home).exists() {
                ServiceState::Stopped
            } else {
                ServiceState::NotInstalled
            }
        }
        Err(other) => return Err(other),
    };
    Ok(ServiceReport {
        backend: ServiceBackend::Schtasks,
        state,
        pid: None,
        active_since: None,
    })
}

/// Parse `schtasks /Query /FO LIST /V` output: the first line whose trimmed
/// text starts with `Status:` decides, and `Running` is the only running
/// answer — `Ready`, `Disabled` and anything else, including no `Status:`
/// line at all, are stopped. Lines are trimmed on both sides, so the padded
/// `/V` dump and a flat one parse alike (the [`parse_launchctl_print`]
/// treatment).
///
/// Nothing here maps to [`ServiceState::Failed`], and that is not an
/// omission: the Task Scheduler records a *last result* code rather than a
/// give-up condition, so it has no state equivalent to systemd's unit that
/// stopped restarting.
pub(crate) fn parse_schtasks_query(stdout: &str) -> ServiceState {
    let status = stdout
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("Status:"))
        .map(str::trim);
    match status {
        Some("Running") => ServiceState::Running,
        _ => ServiceState::Stopped,
    }
}

/// Render the report the way the spec locks it (byte-tested): four
/// `key: value` lines, then the log tail two-space-indented under a
/// `recent log (last N lines):` header — or the single line
/// `recent log: (none)`, the same wording as `dreamd status`. `pid` and
/// `active_since` print `-` when unknown. The header always names
/// [`STATUS_LOG_TAIL_LINES`], just as `dreamd status` always names its five.
pub(crate) fn render_report(report: &ServiceReport, log_tail: &[String]) -> String {
    let mut lines = vec![
        format!("service: {}", report.state),
        format!("backend: {}", report.backend.label()),
        format!(
            "pid: {}",
            report
                .pid
                .map_or_else(|| "-".to_string(), |pid| pid.to_string())
        ),
        format!(
            "active_since: {}",
            report.active_since.as_deref().unwrap_or("-")
        ),
    ];
    if log_tail.is_empty() {
        lines.push("recent log: (none)".to_string());
    } else {
        lines.push(format!("recent log (last {STATUS_LOG_TAIL_LINES} lines):"));
        lines.extend(log_tail.iter().map(|line| format!("  {line}")));
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Spawn `systemctl --user <args>` and capture its output. A spawn failure is
/// [`ServiceError::Spawn`]; a non-zero exit is [`ServiceError::Systemctl`]
/// carrying the subcommand words and systemd's stderr. Both `systemctl`
/// runners go through here — [`systemctl_user`] discards stdout,
/// [`systemctl_user_query`] returns it — so this is one of the three places
/// in the module that spawn a process ([`launchctl_output`], [`current_uid`]).
fn systemctl_user_output(args: &[&str]) -> Result<std::process::Output, ServiceError> {
    let output = std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|source| ServiceError::Spawn {
            program: "systemctl",
            source,
        })?;
    if output.status.success() {
        return Ok(output);
    }
    Err(ServiceError::Systemctl {
        args: args.join(" "),
        status: output.status,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// The [`Systemctl`] runner `install` / `start` use: stdout is discarded.
fn systemctl_user(args: &[&str]) -> Result<(), ServiceError> {
    systemctl_user_output(args)?;
    Ok(())
}

/// The [`Query`] runner the report verb uses: the (lossy) stdout of a
/// successful `systemctl --user <args>`.
fn systemctl_user_query(args: &[&str]) -> Result<String, ServiceError> {
    let output = systemctl_user_output(args)?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Spawn `launchctl <args>` and capture its output. A spawn failure is
/// [`ServiceError::Spawn`]; a non-zero exit is [`ServiceError::Launchctl`]
/// carrying the subcommand words and launchd's stderr. Both `launchctl`
/// runners go through here ([`launchctl`] discards stdout,
/// [`launchctl_query`] returns it).
fn launchctl_output(args: &[&str]) -> Result<std::process::Output, ServiceError> {
    let output = std::process::Command::new("launchctl")
        .args(args)
        .output()
        .map_err(|source| ServiceError::Spawn {
            program: "launchctl",
            source,
        })?;
    if output.status.success() {
        return Ok(output);
    }
    Err(ServiceError::Launchctl {
        args: args.join(" "),
        status: output.status,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// The [`Launchctl`] runner `install` / `start` use: stdout is discarded.
fn launchctl(args: &[&str]) -> Result<(), ServiceError> {
    launchctl_output(args)?;
    Ok(())
}

/// The [`Query`] runner the report verb uses: the (lossy) stdout of a
/// successful `launchctl <args>`.
fn launchctl_query(args: &[&str]) -> Result<String, ServiceError> {
    let output = launchctl_output(args)?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Spawn `schtasks <args>` and capture its output. A spawn failure is
/// [`ServiceError::Spawn`]; a non-zero exit is [`ServiceError::Schtasks`]
/// carrying the argv words and the scheduler's stderr. Both `schtasks`
/// runners go through here ([`schtasks`] discards stdout, [`schtasks_query`]
/// returns it), on the [`systemctl_user_output`] / [`launchctl_output`]
/// pattern.
fn schtasks_output(args: &[&str]) -> Result<std::process::Output, ServiceError> {
    let output = std::process::Command::new("schtasks")
        .args(args)
        .output()
        .map_err(|source| ServiceError::Spawn {
            program: "schtasks",
            source,
        })?;
    if output.status.success() {
        return Ok(output);
    }
    Err(ServiceError::Schtasks {
        args: args.join(" "),
        status: output.status,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// The [`Schtasks`] runner `install` / `start` / `restart` / `uninstall` use:
/// stdout is discarded.
fn schtasks(args: &[&str]) -> Result<(), ServiceError> {
    schtasks_output(args)?;
    Ok(())
}

/// The [`Query`] runner the report verb uses on Windows: the (lossy) stdout
/// of a successful `schtasks <args>`.
fn schtasks_query(args: &[&str]) -> Result<String, ServiceError> {
    let output = schtasks_output(args)?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Spawn `icacls <args>` and capture its output. A spawn failure is
/// [`ServiceError::Spawn`]; a non-zero exit is [`ServiceError::Icacls`]
/// carrying the argv words and its stderr — and never a
/// [`ServiceError::Io`], which would claim `auth.json` could not be written
/// when in fact it was and only the ACL did not take.
fn icacls_output(args: &[&str]) -> Result<std::process::Output, ServiceError> {
    let output = std::process::Command::new("icacls")
        .args(args)
        .output()
        .map_err(|source| ServiceError::Spawn {
            program: "icacls",
            source,
        })?;
    if output.status.success() {
        return Ok(output);
    }
    Err(ServiceError::Icacls {
        args: args.join(" "),
        status: output.status,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// The [`Icacls`] runner `install` uses: stdout is discarded.
fn icacls(args: &[&str]) -> Result<(), ServiceError> {
    icacls_output(args)?;
    Ok(())
}

/// The current uid for launchd's `gui/<uid>` domain, read from `id -u`
/// stdout. Spawning a POSIX utility keeps the CLI crate free of FFI and of
/// any new dependency (NFR-2); the cost is one extra process per `service`
/// verb on macOS, which is a one-shot anyway.
fn current_uid() -> Result<u32, ServiceError> {
    let output = std::process::Command::new("id")
        .arg("-u")
        .output()
        .map_err(|source| ServiceError::Spawn {
            program: "id",
            source,
        })?;
    if !output.status.success() {
        return Err(ServiceError::Uid(format!(
            "`id -u` failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let text = stdout.trim();
    text.parse::<u32>()
        .map_err(|e| ServiceError::Uid(format!("unparseable `id -u` output {text:?}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // No test here can spawn `systemctl` or `launchctl`: every call into
    // `run_*_with` passes either a panicking runner (paths that must refuse
    // first) or a recording closure (paths that must reach the supervisor).
    // This host may well have a live user manager, GHA macos-latest has a
    // live launchd, and a test must never enable a real unit or bootstrap a
    // real LaunchAgent under the developer's HOME. The uid is injected (501)
    // so `id -u` is never spawned either, and every launchd test runs on
    // Linux — none is gated on the macOS target. `uninstall`'s confirmation
    // is injected on the same principle (AILAB-202): the tty probe and the
    // line read are closures, so no test blocks on real stdin or depends on
    // whether the harness gave the runner a terminal.

    const FAKE_EXE: &str = "/opt/dreamd/bin/dreamd";
    const FAKE_UID: u32 = 501;

    fn fake_exe() -> Result<PathBuf, ServiceError> {
        Ok(PathBuf::from(FAKE_EXE))
    }

    fn never_systemctl(args: &[&str]) -> Result<(), ServiceError> {
        panic!("systemctl must not be reached, got {args:?}")
    }

    fn never_launchctl(args: &[&str]) -> Result<(), ServiceError> {
        panic!("launchctl must not be reached, got {args:?}")
    }

    /// A canonical temp project with a bare `.agent/` (all `AgentRoot::discover`
    /// needs) plus a fake HOME. Returns `(project_guard, cwd, home_guard)`.
    #[cfg(unix)]
    fn fake_project_and_home() -> (tempfile::TempDir, PathBuf, tempfile::TempDir) {
        let project = tempfile::tempdir().unwrap();
        let cwd = project.path().canonicalize().unwrap();
        fs::create_dir(cwd.join(".agent")).unwrap();
        let home = tempfile::tempdir().unwrap();
        (project, cwd, home)
    }

    #[test]
    fn render_user_unit_is_byte_stable() {
        let unit = render_user_unit(Path::new(FAKE_EXE), Path::new("/home/u/proj"));
        let expected = "\
[Unit]
Description=dreamd memory daemon

[Service]
Type=simple
ExecStart=/opt/dreamd/bin/dreamd watch
WorkingDirectory=/home/u/proj
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
";
        assert_eq!(unit, expected);
    }

    #[test]
    fn render_user_unit_contract_lines() {
        let unit = render_user_unit(Path::new(FAKE_EXE), Path::new("/home/u/proj"));
        assert!(unit.contains("Type=simple"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("RestartSec=5"));
        let exec = unit
            .lines()
            .find(|l| l.starts_with("ExecStart="))
            .expect("unit must carry an ExecStart= line");
        assert!(exec.ends_with(" watch"), "ExecStart must run watch: {exec}");
        assert!(
            unit.lines().any(|l| l == "WorkingDirectory=/home/u/proj"),
            "WorkingDirectory must be the exact project root"
        );
        // The literals below are assembled so the forbidden strings never
        // appear verbatim in this source file (anti-pattern greps).
        assert!(
            !unit.contains(concat!("Type=", "forking")),
            "systemd supervises the foreground daemon; never the forking type"
        );
        assert!(
            !unit.contains(concat!("Environment=", "HOME=")),
            "never inject a HOME environment line (AILAB-584)"
        );
        assert!(
            !unit.contains("After="),
            "no After= — it would cycle with WantedBy"
        );
    }

    #[test]
    fn render_user_unit_quotes_exec_only_when_exe_has_whitespace() {
        let unit = render_user_unit(
            Path::new("/opt/my tools/dreamd"),
            Path::new("/home/u/my proj"),
        );
        assert!(
            unit.contains("ExecStart=\"/opt/my tools/dreamd\" watch"),
            "exe with whitespace must be double-quoted: {unit}"
        );
        assert!(
            unit.lines()
                .any(|l| l == "WorkingDirectory=/home/u/my proj"),
            "WorkingDirectory is a literal path and must stay unquoted: {unit}"
        );
    }

    #[test]
    fn render_user_unit_escapes_percent_specifiers() {
        let unit = render_user_unit(Path::new("/opt/100%/dreamd"), Path::new("/home/u/50%done"));
        assert!(
            unit.lines()
                .any(|l| l == "ExecStart=/opt/100%%/dreamd watch"),
            "a literal % in the exe path must be doubled: {unit}"
        );
        assert!(
            unit.lines()
                .any(|l| l == "WorkingDirectory=/home/u/50%%done"),
            "a literal % in the project root must be doubled: {unit}"
        );
    }

    #[test]
    fn unit_path_is_under_config_systemd_user() {
        assert_eq!(
            unit_path(Path::new("/h")),
            PathBuf::from("/h/.config/systemd/user/dreamd.service")
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_user_unit_writes_under_fake_home() {
        let home = tempfile::tempdir().unwrap();
        let exe = Path::new(FAKE_EXE);
        let root = Path::new("/home/u/proj");

        let written = write_user_unit(home.path(), exe, root).unwrap();
        assert_eq!(written, unit_path(home.path()));
        assert!(written.is_file());
        assert_eq!(
            fs::read_to_string(&written).unwrap(),
            render_user_unit(exe, root)
        );

        // Reinstall from another project overwrites in place (no --force).
        let other_root = Path::new("/home/u/other");
        let rewritten = write_user_unit(home.path(), exe, other_root).unwrap();
        assert_eq!(rewritten, written);
        assert_eq!(
            fs::read_to_string(&rewritten).unwrap(),
            render_user_unit(exe, other_root)
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_user_unit_reports_the_path_that_failed() {
        let home = tempfile::tempdir().unwrap();
        // A regular file where the `.config` directory must go makes
        // `create_dir_all` fail; the error must name that directory.
        fs::write(home.path().join(".config"), b"not a dir").unwrap();

        let err = write_user_unit(home.path(), Path::new(FAKE_EXE), Path::new("/home/u/proj"))
            .unwrap_err();
        match &err {
            ServiceError::Io { path, .. } => {
                assert_eq!(path, &home.path().join(".config/systemd/user"));
            }
            other => panic!("expected Io, got {other:?}"),
        }
        assert!(err.to_string().starts_with("could not write "), "{err}");
    }

    #[test]
    fn run_install_without_systemd_refuses_and_writes_nothing() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();

        let result = run_install_with(
            cwd.path(),
            home.path(),
            false,
            fake_exe,
            &mut never_systemctl,
        );
        let err = match result {
            Err(e @ ServiceError::NoSystemd) => e,
            other => panic!("expected NoSystemd, got {other:?}"),
        };
        assert!(
            !unit_path(home.path()).exists(),
            "refusal must happen before anything is written"
        );
        assert!(
            !home.path().join(".config").exists(),
            "refusal must not even create the unit directory"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("dreamd watch"),
            "must point at the foreground daemon: {msg}"
        );
        assert!(
            !msg.contains("LaunchAgent"),
            "never mention the macOS path in the Linux refusal: {msg}"
        );
    }

    #[test]
    fn run_install_without_project_root_refuses_and_writes_nothing() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        assert!(
            AgentRoot::discover(cwd.path()).is_err(),
            "precondition: an ancestor of {} carries .agent/",
            cwd.path().display()
        );

        // The probe is forced true; the runner panics if the refusal does not
        // come first, and the resolver panics if the exe is resolved before
        // the project-root check (a CurrentExe failure would otherwise mask
        // the exit-2 usage refusal).
        let result = run_install_with(
            cwd.path(),
            home.path(),
            true,
            || -> Result<PathBuf, ServiceError> {
                panic!("exe must not be resolved before the project-root check")
            },
            &mut never_systemctl,
        );
        assert!(
            matches!(result, Err(ServiceError::NoProjectRoot)),
            "expected NoProjectRoot, got {result:?}"
        );
        assert!(
            !unit_path(home.path()).exists(),
            "refusal must happen before anything is written"
        );
        assert!(
            !home.path().join(".config").exists(),
            "refusal must not even create the unit directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_install_writes_unit_then_reloads_and_enables() {
        let (_project, cwd, home) = fake_project_and_home();
        let root = AgentRoot::discover(&cwd).unwrap();
        let unit = unit_path(home.path());

        // Each call records the argv and whether the unit file already existed.
        let mut calls: Vec<(Vec<String>, bool)> = Vec::new();
        let result = run_install_with(&cwd, home.path(), true, fake_exe, &mut |args: &[&str]| {
            calls.push((args.iter().map(|s| s.to_string()).collect(), unit.exists()));
            Ok(())
        });
        assert!(result.is_ok(), "{result:?}");

        assert_eq!(
            fs::read_to_string(&unit).unwrap(),
            render_user_unit(Path::new(FAKE_EXE), root.project_root())
        );
        let argv: Vec<&[String]> = calls.iter().map(|(a, _)| a.as_slice()).collect();
        assert_eq!(
            argv,
            [
                &["daemon-reload".to_string()][..],
                &[
                    "enable".to_string(),
                    "--now".to_string(),
                    UNIT_NAME.to_string()
                ][..],
            ]
        );
        assert!(
            calls.iter().all(|(_, existed)| *existed),
            "the unit file must be on disk before either systemctl call: {calls:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_install_keeps_unit_file_when_systemctl_fails() {
        use std::os::unix::process::ExitStatusExt;

        let (_project, cwd, home) = fake_project_and_home();

        let result = run_install_with(&cwd, home.path(), true, fake_exe, &mut |args: &[&str]| {
            if args == ["daemon-reload"] {
                Ok(())
            } else {
                Err(ServiceError::Systemctl {
                    args: args.join(" "),
                    status: ExitStatus::from_raw(1 << 8),
                    stderr: "boom".into(),
                })
            }
        });
        assert!(
            matches!(result, Err(ServiceError::Systemctl { .. })),
            "expected Systemctl, got {result:?}"
        );
        assert!(
            unit_path(home.path()).is_file(),
            "the unit file must stay on disk after a systemctl failure"
        );
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("enable --now"), "{msg}");
        assert!(msg.contains("boom"), "{msg}");
    }

    #[test]
    fn run_start_without_systemd_refuses() {
        assert!(matches!(
            run_start_with(false, &mut never_systemctl),
            Err(ServiceError::NoSystemd)
        ));
    }

    #[test]
    fn run_start_calls_systemctl_start() {
        let mut calls: Vec<Vec<String>> = Vec::new();
        let result = run_start_with(true, &mut |args: &[&str]| {
            calls.push(args.iter().map(|s| s.to_string()).collect());
            Ok(())
        });
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(calls, [["start".to_string(), UNIT_NAME.to_string()]]);
    }

    /// AILAB-185 — same refusal as `start` on a host with no supervisor: exit 2
    /// pointing at foreground `dreamd watch`, and the runner is never reached
    /// (`never_systemctl` panics if it is).
    #[test]
    fn run_restart_without_systemd_refuses() {
        assert!(matches!(
            run_restart_with(false, &mut never_systemctl),
            Err(ServiceError::NoSystemd)
        ));
    }

    /// AILAB-185 — `restart`, never `try-restart` and never the pager `status`
    /// verb. `--user` is prepended by production's [`systemctl_user`], exactly
    /// as it is for `start`.
    #[test]
    fn run_restart_calls_systemctl_restart() {
        let mut calls: Vec<Vec<String>> = Vec::new();
        let result = run_restart_with(true, &mut |args: &[&str]| {
            calls.push(args.iter().map(|s| s.to_string()).collect());
            Ok(())
        });
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(calls, [["restart".to_string(), UNIT_NAME.to_string()]]);
    }

    #[test]
    fn no_systemd_message_points_at_watch() {
        let msg = ServiceError::NoSystemd.to_string();
        assert!(msg.contains("dreamd watch"), "{msg}");
        assert!(msg.contains("systemd --user"), "{msg}");
    }

    #[test]
    fn display_covers_remaining_variants() {
        assert_eq!(
            ServiceError::NoProjectRoot.to_string(),
            "no .agent/ directory found. Run `dreamd init` first."
        );
        let io = ServiceError::Io {
            path: PathBuf::from("/h/.config/systemd/user/dreamd.service"),
            source: std::io::Error::other("disk full"),
        };
        assert_eq!(
            io.to_string(),
            "could not write /h/.config/systemd/user/dreamd.service: disk full"
        );
        let exe = ServiceError::CurrentExe(std::io::Error::other("gone"));
        assert_eq!(
            exe.to_string(),
            "could not resolve this binary's path: gone"
        );
        let spawn = ServiceError::Spawn {
            program: "systemctl",
            source: std::io::Error::other("ENOENT"),
        };
        assert_eq!(spawn.to_string(), "could not run systemctl: ENOENT");
        let spawn = ServiceError::Spawn {
            program: "launchctl",
            source: std::io::Error::other("ENOENT"),
        };
        assert_eq!(spawn.to_string(), "could not run launchctl: ENOENT");
        let exists = ServiceError::PlistExists(PathBuf::from(
            "/h/Library/LaunchAgents/dev.dreamd.dreamd.plist",
        ));
        assert_eq!(
            exists.to_string(),
            "LaunchAgent plist /h/Library/LaunchAgents/dev.dreamd.dreamd.plist already exists; \
             rerun `dreamd service install --force` to overwrite it."
        );
        let uid = ServiceError::Uid("unparseable `id -u` output \"abc\"".into());
        assert_eq!(
            uid.to_string(),
            "could not determine the current uid via `id -u`: unparseable `id -u` output \"abc\""
        );
    }

    /// AILAB-202 — the `--purge` confirmation refusals. `NotATty` is worded
    /// byte-identically to `reset workspace`'s, because it is the same idiom
    /// and users meet it the same way.
    #[test]
    fn display_covers_uninstall_confirmation_variants() {
        assert_eq!(
            ServiceError::NotATty.to_string(),
            "stdin is not a tty; pass --yes to confirm"
        );
        assert_eq!(ServiceError::Declined.to_string(), "uninstall declined");
        let stdin = ServiceError::Stdin(std::io::Error::other("closed"));
        assert_eq!(
            stdin.to_string(),
            "could not read the confirmation from stdin: closed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn display_covers_launchctl_failure() {
        use std::os::unix::process::ExitStatusExt;

        let err = ServiceError::Launchctl {
            args: "bootstrap gui/501 /h/Library/LaunchAgents/dev.dreamd.dreamd.plist".into(),
            status: ExitStatus::from_raw(5 << 8),
            stderr: "Bootstrap failed: 5: Input/output error\n".into(),
        };
        assert_eq!(
            err.to_string(),
            "launchctl bootstrap gui/501 /h/Library/LaunchAgents/dev.dreamd.dreamd.plist \
             failed (exit status: 5): Bootstrap failed: 5: Input/output error"
        );
    }

    // ---- macOS LaunchAgent path (AILAB-169) ---------------------------------

    /// The plist text the spec locks, for the fake exe and `/Users/u/proj`.
    /// Tab-indented; `\t` escapes keep the tabs visible and safe from editors.
    const EXPECTED_PLIST: &str = "\
<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
<dict>
\t<key>Label</key>
\t<string>dev.dreamd.dreamd</string>
\t<key>ProgramArguments</key>
\t<array>
\t\t<string>/opt/dreamd/bin/dreamd</string>
\t\t<string>watch</string>
\t</array>
\t<key>WorkingDirectory</key>
\t<string>/Users/u/proj</string>
\t<key>RunAtLoad</key>
\t<true/>
\t<key>KeepAlive</key>
\t<dict>
\t\t<key>SuccessfulExit</key>
\t\t<false/>
\t</dict>
</dict>
</plist>
";

    #[test]
    fn render_plist_is_byte_stable() {
        let plist = render_plist(Path::new(FAKE_EXE), Path::new("/Users/u/proj"));
        assert_eq!(plist, EXPECTED_PLIST);
    }

    #[test]
    fn render_plist_contract_keys() {
        let plist = render_plist(Path::new(FAKE_EXE), Path::new("/Users/u/proj"));
        let lines: Vec<&str> = plist.lines().collect();
        let after = |key: &str| -> &str {
            let needle = format!("<key>{key}</key>");
            let i = lines
                .iter()
                .position(|l| l.trim() == needle)
                .unwrap_or_else(|| panic!("plist must carry {needle}: {plist}"));
            lines[i + 1].trim()
        };

        assert_eq!(after("Label"), "<string>dev.dreamd.dreamd</string>");
        assert_eq!(
            after("WorkingDirectory"),
            "<string>/Users/u/proj</string>",
            "WorkingDirectory must be the exact project root"
        );
        assert_eq!(after("RunAtLoad"), "<true/>");
        assert_eq!(
            after("SuccessfulExit"),
            "<false/>",
            "KeepAlive.SuccessfulExit=false is the on-failure restart policy"
        );
        assert_eq!(
            after("KeepAlive"),
            "<dict>",
            "KeepAlive must be the dict form"
        );

        // ProgramArguments: the last <string> inside the array is `watch`, so
        // launchd runs the foreground daemon and supervises that PID.
        let start = plist.find("<array>").expect("ProgramArguments array");
        let end = plist.find("</array>").expect("ProgramArguments array end");
        let args: Vec<&str> = plist[start..end]
            .lines()
            .filter_map(|l| {
                l.trim()
                    .strip_prefix("<string>")
                    .and_then(|s| s.strip_suffix("</string>"))
            })
            .collect();
        assert_eq!(args, [FAKE_EXE, "watch"]);

        // The literals below are assembled so the forbidden keys never appear
        // verbatim in this source file (anti-pattern greps). The redirection
        // keys would put a second writer on ~/.agent/dreamd.log; the process
        // group key would detach the daemon from launchd's supervision; the
        // environment dict is the plist form of the HOME injection AILAB-584
        // banned from the unit.
        for forbidden in [
            concat!("StandardOut", "Path"),
            concat!("StandardError", "Path"),
            concat!("Standard", "Out"),
            concat!("Standard", "Error"),
            concat!("Abandon", "ProcessGroup"),
            concat!("Environment", "Variables"),
        ] {
            assert!(
                !plist.contains(forbidden),
                "plist must never carry {forbidden}: {plist}"
            );
        }
    }

    #[test]
    fn render_plist_xml_escapes_paths() {
        let plist = render_plist(
            Path::new("/opt/a&b<c>d\"e/dreamd"),
            Path::new("/Users/u/50%done&more"),
        );
        assert!(
            plist.contains("\t\t<string>/opt/a&amp;b&lt;c&gt;d&quot;e/dreamd</string>"),
            "exe path must be XML-escaped: {plist}"
        );
        assert!(
            plist.contains("\t<string>/Users/u/50%done&amp;more</string>"),
            "root must be XML-escaped and a % must stay single (no systemd doubling): {plist}"
        );
        assert!(
            !plist.contains("%%"),
            "never systemd-escape a plist: {plist}"
        );
    }

    #[test]
    fn plist_path_is_under_library_launchagents() {
        assert_eq!(
            plist_path(Path::new("/h")),
            PathBuf::from("/h/Library/LaunchAgents/dev.dreamd.dreamd.plist")
        );
    }

    #[cfg(unix)]
    #[test]
    fn launchd_install_refuses_when_plist_exists_without_force() {
        let (_project, cwd, home) = fake_project_and_home();
        let path = plist_path(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"sentinel: not a plist").unwrap();

        let result = run_launchd_install_with(
            &cwd,
            home.path(),
            false,
            FAKE_UID,
            fake_exe,
            &mut never_launchctl,
        );
        let err = match result {
            Err(e @ ServiceError::PlistExists(_)) => e,
            other => panic!("expected PlistExists, got {other:?}"),
        };
        match &err {
            ServiceError::PlistExists(p) => assert_eq!(p, &path),
            _ => unreachable!(),
        }
        assert_eq!(
            fs::read(&path).unwrap(),
            b"sentinel: not a plist",
            "refusal must leave the existing plist byte-identical"
        );
        assert!(
            !path.with_extension("tmp").exists(),
            "refusal must not even stage a temp file"
        );
        let msg = err.to_string();
        assert!(msg.contains("--force"), "{msg}");
        assert!(msg.contains(&path.display().to_string()), "{msg}");
    }

    #[cfg(unix)]
    #[test]
    fn launchd_install_with_force_overwrites_and_boots_out_then_bootstraps() {
        let (_project, cwd, home) = fake_project_and_home();
        let root = AgentRoot::discover(&cwd).unwrap();
        let path = plist_path(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"sentinel: not a plist").unwrap();
        let expected = render_plist(Path::new(FAKE_EXE), root.project_root());

        // Each call records the argv and the plist bytes on disk at that moment.
        let mut calls: Vec<(Vec<String>, Option<String>)> = Vec::new();
        let result = run_launchd_install_with(
            &cwd,
            home.path(),
            true,
            FAKE_UID,
            fake_exe,
            &mut |args: &[&str]| {
                calls.push((
                    args.iter().map(|s| s.to_string()).collect(),
                    fs::read_to_string(&path).ok(),
                ));
                Ok(())
            },
        );
        assert!(result.is_ok(), "{result:?}");

        assert_eq!(fs::read_to_string(&path).unwrap(), expected);
        let argv: Vec<&[String]> = calls.iter().map(|(a, _)| a.as_slice()).collect();
        assert_eq!(
            argv,
            [
                &[
                    "bootout".to_string(),
                    "gui/501/dev.dreamd.dreamd".to_string()
                ][..],
                &[
                    "bootstrap".to_string(),
                    "gui/501".to_string(),
                    path.display().to_string()
                ][..],
            ]
        );
        assert!(
            calls
                .iter()
                .all(|(_, on_disk)| on_disk.as_deref() == Some(expected.as_str())),
            "the new plist must be on disk before either launchctl call: {calls:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn launchd_install_with_force_ignores_bootout_failure() {
        use std::os::unix::process::ExitStatusExt;

        let (_project, cwd, home) = fake_project_and_home();
        let root = AgentRoot::discover(&cwd).unwrap();
        let path = plist_path(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"sentinel: not a plist").unwrap();

        let mut calls: Vec<Vec<String>> = Vec::new();
        let result = run_launchd_install_with(
            &cwd,
            home.path(),
            true,
            FAKE_UID,
            fake_exe,
            &mut |args: &[&str]| {
                calls.push(args.iter().map(|s| s.to_string()).collect());
                if args[0] == "bootout" {
                    // What launchd says for a label that is not loaded.
                    Err(ServiceError::Launchctl {
                        args: args.join(" "),
                        status: ExitStatus::from_raw(3 << 8),
                        stderr: "Boot-out failed: 3: No such process".into(),
                    })
                } else {
                    Ok(())
                }
            },
        );
        assert!(
            result.is_ok(),
            "a failed bootout must not fail install: {result:?}"
        );
        assert_eq!(calls.len(), 2, "bootstrap must still follow: {calls:?}");
        assert_eq!(calls[1][0], "bootstrap");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            render_plist(Path::new(FAKE_EXE), root.project_root())
        );
    }

    #[cfg(unix)]
    #[test]
    fn launchd_first_install_writes_plist_then_bootstraps_only() {
        let (_project, cwd, home) = fake_project_and_home();
        let root = AgentRoot::discover(&cwd).unwrap();
        let path = plist_path(home.path());
        assert!(!path.exists(), "precondition: fresh HOME");

        let mut calls: Vec<(Vec<String>, bool)> = Vec::new();
        let result = run_launchd_install_with(
            &cwd,
            home.path(),
            false,
            FAKE_UID,
            fake_exe,
            &mut |args: &[&str]| {
                calls.push((args.iter().map(|s| s.to_string()).collect(), path.is_file()));
                Ok(())
            },
        );
        assert!(result.is_ok(), "{result:?}");

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            render_plist(Path::new(FAKE_EXE), root.project_root())
        );
        let argv: Vec<&[String]> = calls.iter().map(|(a, _)| a.as_slice()).collect();
        assert_eq!(
            argv,
            [&[
                "bootstrap".to_string(),
                "gui/501".to_string(),
                path.display().to_string()
            ][..]],
            "a first install must not bootout: {argv:?}"
        );
        assert!(
            calls.iter().all(|(_, existed)| *existed),
            "the plist must be on disk before bootstrap: {calls:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn launchd_install_keeps_plist_when_bootstrap_fails() {
        use std::os::unix::process::ExitStatusExt;

        let (_project, cwd, home) = fake_project_and_home();
        let path = plist_path(home.path());

        let result = run_launchd_install_with(
            &cwd,
            home.path(),
            false,
            FAKE_UID,
            fake_exe,
            &mut |args: &[&str]| {
                Err(ServiceError::Launchctl {
                    args: args.join(" "),
                    status: ExitStatus::from_raw(5 << 8),
                    stderr: "Bootstrap failed: 5: Input/output error".into(),
                })
            },
        );
        assert!(
            matches!(result, Err(ServiceError::Launchctl { .. })),
            "expected Launchctl, got {result:?}"
        );
        assert!(
            path.is_file(),
            "the plist must stay on disk after a launchctl failure"
        );
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("bootstrap"), "{msg}");
        assert!(msg.contains("Input/output error"), "{msg}");
    }

    #[test]
    fn launchd_install_without_project_root_refuses_and_writes_nothing() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        assert!(
            AgentRoot::discover(cwd.path()).is_err(),
            "precondition: an ancestor of {} carries .agent/",
            cwd.path().display()
        );

        // The resolver panics if the exe is resolved before the project-root
        // check, and the runner panics if launchctl is reached at all.
        let result = run_launchd_install_with(
            cwd.path(),
            home.path(),
            true,
            FAKE_UID,
            || -> Result<PathBuf, ServiceError> {
                panic!("exe must not be resolved before the project-root check")
            },
            &mut never_launchctl,
        );
        assert!(
            matches!(result, Err(ServiceError::NoProjectRoot)),
            "expected NoProjectRoot, got {result:?}"
        );
        assert!(
            !plist_path(home.path()).exists(),
            "refusal must happen before anything is written"
        );
        assert!(
            !home.path().join("Library").exists(),
            "refusal must not even create the LaunchAgents directory"
        );
    }

    #[test]
    fn launchd_start_kickstarts() {
        let mut calls: Vec<Vec<String>> = Vec::new();
        let result = run_launchd_start_with(FAKE_UID, &mut |args: &[&str]| {
            calls.push(args.iter().map(|s| s.to_string()).collect());
            Ok(())
        });
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            calls,
            [[
                "kickstart".to_string(),
                "-k".to_string(),
                "gui/501/dev.dreamd.dreamd".to_string()
            ]]
        );
    }

    /// AILAB-185 — the expected argv here is **byte-identical** to
    /// `launchd_start_kickstarts` above, on purpose: `kickstart -k` already
    /// kills the running instance before relaunching it, so start's argv is
    /// itself the bounce and restart reuses it verbatim. Only the printed verb
    /// differs (`restarted` vs `started`), which is why this goes through
    /// `run_launchd_restart_with` instead of calling `run_launchd_start_with`.
    #[test]
    fn launchd_restart_kickstarts() {
        let mut calls: Vec<Vec<String>> = Vec::new();
        let result = run_launchd_restart_with(FAKE_UID, &mut |args: &[&str]| {
            calls.push(args.iter().map(|s| s.to_string()).collect());
            Ok(())
        });
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            calls,
            [[
                "kickstart".to_string(),
                "-k".to_string(),
                "gui/501/dev.dreamd.dreamd".to_string()
            ]]
        );
    }

    // ---- `service status` (AILAB-178) ---------------------------------------
    //
    // Same rule as above: no test here can spawn `systemctl`, `launchctl`, or
    // `id`. Every supervisor query is a recording closure answering with a
    // fixture, or a panicking one on paths that must refuse first; the uid is
    // `FAKE_UID` via an injected closure; the macOS paths run on Linux.

    /// A [`Query`] runner that must never be reached.
    fn never_query(args: &[&str]) -> Result<String, ServiceError> {
        panic!("no supervisor query must be reached, got {args:?}")
    }

    /// The exact argv the report verb hands its `Query` runner: the words
    /// after `--user`, which production prepends.
    const EXPECTED_SHOW_ARGV: [&str; 13] = [
        "show",
        "dreamd.service",
        "--no-pager",
        "-p",
        "LoadState",
        "-p",
        "ActiveState",
        "-p",
        "SubState",
        "-p",
        "MainPID",
        "-p",
        "ActiveEnterTimestamp",
    ];

    /// `systemctl --user show` stdout for a loaded, running unit.
    const SHOW_RUNNING: &str = "\
LoadState=loaded
ActiveState=active
SubState=running
MainPID=1234
ActiveEnterTimestamp=Sat 2026-09-05 16:00:00 UTC
";

    /// `systemctl --user show` stdout for a unit that was never installed.
    /// systemd exits 0 here and reports `LoadState=not-found`, so the runner
    /// answers `Ok` — a runner `Err` on this path is a genuine failure.
    const SHOW_NOT_FOUND: &str = "\
LoadState=not-found
ActiveState=inactive
SubState=dead
MainPID=0
ActiveEnterTimestamp=
";

    /// A realistic `launchctl print gui/501/dev.dreamd.dreamd` dump for a
    /// running agent. Tab-indented like the real thing; `\t` escapes keep the
    /// tabs visible and safe from editors.
    const LAUNCHCTL_PRINT_RUNNING: &str = "\
gui/501/dev.dreamd.dreamd = {
\tactive count = 1
\tpath = /Users/u/Library/LaunchAgents/dev.dreamd.dreamd.plist
\tstate = running
\tprogram = /opt/dreamd/bin/dreamd
\targuments = {
\t\t/opt/dreamd/bin/dreamd
\t\twatch
\t}
\tworking directory = /Users/u/proj
\tspawn type = daemon (3)
\tpid = 99
\truns = 1
}
";

    #[test]
    fn status_systemd_argv_is_show_with_properties() {
        let mut calls: Vec<Vec<String>> = Vec::new();
        let report = query_systemd_status(&mut |args: &[&str]| {
            calls.push(args.iter().map(|s| s.to_string()).collect());
            Ok(SHOW_RUNNING.to_string())
        })
        .unwrap();
        assert_eq!(report.state, ServiceState::Running);
        assert_eq!(calls.len(), 1, "exactly one `show` query: {calls:?}");
        assert_eq!(calls[0], EXPECTED_SHOW_ARGV);
    }

    #[test]
    fn status_systemd_not_found_is_not_installed() {
        let mut calls = 0;
        let report = run_status_with(
            Some(ServiceBackend::Systemd),
            Path::new("/h"),
            || -> Result<u32, ServiceError> {
                panic!("uid must not be resolved on the systemd arm")
            },
            &mut |_args: &[&str]| {
                calls += 1;
                Ok(SHOW_NOT_FOUND.to_string())
            },
            &mut never_query,
            &mut never_query,
        )
        .unwrap();
        assert_eq!(calls, 1, "the query runner must be called exactly once");
        assert_eq!(
            report,
            ServiceReport {
                backend: ServiceBackend::Systemd,
                state: ServiceState::NotInstalled,
                pid: None,
                active_since: None,
            }
        );
        let rendered = render_report(&report, &[]);
        assert!(rendered.contains("service: not-installed\n"), "{rendered}");
        assert!(rendered.contains("pid: -\n"), "{rendered}");
    }

    #[test]
    fn status_systemd_active_is_running_with_pid_and_timestamp() {
        assert_eq!(
            parse_systemd_show(SHOW_RUNNING),
            ServiceReport {
                backend: ServiceBackend::Systemd,
                state: ServiceState::Running,
                pid: Some(1234),
                active_since: Some("Sat 2026-09-05 16:00:00 UTC".to_string()),
            }
        );
    }

    #[test]
    fn status_systemd_failed() {
        let report = parse_systemd_show(
            "LoadState=loaded\nActiveState=failed\nSubState=failed\nMainPID=0\n\
             ActiveEnterTimestamp=Sat 2026-09-05 16:00:00 UTC\n",
        );
        assert_eq!(report.state, ServiceState::Failed);
        assert_eq!(report.pid, None, "MainPID=0 is unknown, never pid 0");
    }

    #[test]
    fn status_systemd_inactive_loaded_is_stopped() {
        let report = parse_systemd_show(
            "LoadState=loaded\nActiveState=inactive\nSubState=dead\nMainPID=0\n\
             ActiveEnterTimestamp=\n",
        );
        assert_eq!(report.state, ServiceState::Stopped);
        assert_eq!(report.pid, None);
        assert_eq!(
            report.active_since, None,
            "an empty timestamp is unknown, never an empty string"
        );
    }

    #[test]
    fn render_report_is_byte_stable() {
        let running = ServiceReport {
            backend: ServiceBackend::Systemd,
            state: ServiceState::Running,
            pid: Some(1234),
            active_since: Some("Sat 2026-09-05 16:00:00 UTC".to_string()),
        };
        let tail = vec!["line-a".to_string(), "line-b".to_string()];
        let expected = "\
service: running
backend: systemd
pid: 1234
active_since: Sat 2026-09-05 16:00:00 UTC
recent log (last 10 lines):
  line-a
  line-b
";
        assert_eq!(render_report(&running, &tail), expected);

        let absent = ServiceReport {
            backend: ServiceBackend::Systemd,
            state: ServiceState::NotInstalled,
            pid: None,
            active_since: None,
        };
        assert_eq!(
            render_report(&absent, &[]),
            "service: not-installed\nbackend: systemd\npid: -\nactive_since: -\nrecent log: (none)\n"
        );
    }

    #[test]
    fn status_launchd_argv_and_running_fixture() {
        let mut calls: Vec<Vec<String>> = Vec::new();
        let report = run_status_with(
            Some(ServiceBackend::Launchd),
            Path::new("/h"),
            || Ok(FAKE_UID),
            &mut never_query,
            &mut |args: &[&str]| {
                calls.push(args.iter().map(|s| s.to_string()).collect());
                Ok(LAUNCHCTL_PRINT_RUNNING.to_string())
            },
            &mut never_query,
        )
        .unwrap();
        assert_eq!(
            calls,
            [["print".to_string(), "gui/501/dev.dreamd.dreamd".to_string()]]
        );
        assert_eq!(
            report,
            ServiceReport {
                backend: ServiceBackend::Launchd,
                state: ServiceState::Running,
                pid: Some(99),
                active_since: None,
            }
        );
        let rendered = render_report(&report, &[]);
        assert!(rendered.contains("backend: launchd\n"), "{rendered}");
        assert!(rendered.contains("pid: 99\n"), "{rendered}");
        assert!(
            rendered.contains("active_since: -\n"),
            "launchctl print carries no start timestamp: {rendered}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn status_launchd_print_error_without_plist_is_not_installed() {
        use std::os::unix::process::ExitStatusExt;

        let home = tempfile::tempdir().unwrap();
        assert!(
            !plist_path(home.path()).exists(),
            "precondition: fresh HOME"
        );

        let mut calls = 0;
        let report = query_launchd_status(home.path(), || Ok(FAKE_UID), &mut |args: &[&str]| {
            calls += 1;
            // What launchd says for a label that is not loaded: a non-zero
            // exit, which is a state answer here rather than a failure.
            Err(ServiceError::Launchctl {
                args: args.join(" "),
                status: ExitStatus::from_raw(113 << 8),
                stderr: "Could not find service \"dev.dreamd.dreamd\" in domain for uid: 501"
                    .into(),
            })
        })
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(
            report,
            ServiceReport {
                backend: ServiceBackend::Launchd,
                state: ServiceState::NotInstalled,
                pid: None,
                active_since: None,
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn status_launchd_print_error_with_plist_is_stopped() {
        use std::os::unix::process::ExitStatusExt;

        let home = tempfile::tempdir().unwrap();
        let path = plist_path(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"sentinel: not a plist").unwrap();

        let report = query_launchd_status(home.path(), || Ok(FAKE_UID), &mut |args: &[&str]| {
            Err(ServiceError::Launchctl {
                args: args.join(" "),
                status: ExitStatus::from_raw(113 << 8),
                stderr: "Could not find service \"dev.dreamd.dreamd\" in domain for uid: 501"
                    .into(),
            })
        })
        .unwrap();
        assert_eq!(
            report,
            ServiceReport {
                backend: ServiceBackend::Launchd,
                state: ServiceState::Stopped,
                pid: None,
                active_since: None,
            },
            "an installed plist that launchd has not loaded is stopped, not absent"
        );
    }

    #[test]
    fn status_launchd_spawn_failure_propagates() {
        let result =
            query_launchd_status(Path::new("/h"), || Ok(FAKE_UID), &mut |_args: &[&str]| {
                Err(ServiceError::Spawn {
                    program: "launchctl",
                    source: std::io::Error::other("ENOENT"),
                })
            });
        assert!(
            matches!(
                result,
                Err(ServiceError::Spawn {
                    program: "launchctl",
                    ..
                })
            ),
            "a launchctl spawn failure is a real error, not a state: {result:?}"
        );
    }

    #[test]
    fn status_without_backend_refuses_before_any_runner() {
        let result = run_status_with(
            None,
            Path::new("/h"),
            || -> Result<u32, ServiceError> { panic!("uid must not be resolved") },
            &mut never_query,
            &mut never_query,
            &mut never_query,
        );
        assert!(
            matches!(result, Err(ServiceError::NoSystemd)),
            "expected NoSystemd, got {result:?}"
        );
    }

    #[test]
    fn status_systemd_arm_never_resolves_uid() {
        let mut calls = 0;
        let result = run_status_with(
            Some(ServiceBackend::Systemd),
            Path::new("/h"),
            || -> Result<u32, ServiceError> {
                panic!("uid must not be resolved on the systemd arm")
            },
            &mut |_args: &[&str]| {
                calls += 1;
                Ok(SHOW_RUNNING.to_string())
            },
            &mut never_query,
            &mut never_query,
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(calls, 1);
    }

    #[test]
    fn parse_launchctl_print_without_state_line_is_stopped() {
        assert_eq!(parse_launchctl_print(""), (ServiceState::Stopped, None));
        assert_eq!(
            parse_launchctl_print("gui/501/dev.dreamd.dreamd = {\n\tactive count = 0\n}\n"),
            (ServiceState::Stopped, None)
        );
        // A loaded-but-idle agent reports a non-`running` state word.
        assert_eq!(
            parse_launchctl_print("\tstate = not running\n"),
            (ServiceState::Stopped, None)
        );
    }

    // ---- uninstall (AILAB-202) ----------------------------------------------

    /// A [`Confirm`] that must never be consulted: without `--purge` there is
    /// nothing destructive to confirm, and with `--yes` the flag *is* the
    /// confirmation.
    fn never_confirm(prompt: &str) -> Result<Option<String>, ServiceError> {
        panic!("confirmation must not be requested, got prompt {prompt:?}")
    }

    /// A [`Confirm`] standing in for non-tty stdin — a pipeline, or CI.
    fn not_a_tty(_prompt: &str) -> Result<Option<String>, ServiceError> {
        Ok(None)
    }

    /// A fake HOME carrying an installed unit file, an installed plist and a
    /// populated daemon home, plus a **separate** project directory with its
    /// own `.agent/` store. The project deliberately lives in its own tempdir,
    /// not under HOME: that is the store `--purge` must not be able to reach.
    /// Returns `(home_guard, project_guard)`.
    #[cfg(unix)]
    fn installed_home_and_project() -> (tempfile::TempDir, tempfile::TempDir) {
        let home = tempfile::tempdir().unwrap();
        let unit = unit_path(home.path());
        fs::create_dir_all(unit.parent().unwrap()).unwrap();
        fs::write(&unit, "[Unit]\n").unwrap();
        let plist = plist_path(home.path());
        fs::create_dir_all(plist.parent().unwrap()).unwrap();
        fs::write(&plist, "<plist/>\n").unwrap();
        let daemon_home = home.path().join(".agent");
        fs::create_dir_all(&daemon_home).unwrap();
        fs::write(daemon_home.join("registry.toml"), "").unwrap();
        fs::write(daemon_home.join("dreamd.log"), "log\n").unwrap();

        let project = tempfile::tempdir().unwrap();
        let project_store = project.path().join(".agent");
        fs::create_dir_all(&project_store).unwrap();
        fs::write(project_store.join("AGENT_LEARNINGS.jsonl"), "{}\n").unwrap();
        (home, project)
    }

    /// A host with no supervisor has nothing to remove: exit-2 refusal, the
    /// runner never reached, the confirmation never requested, and — the part
    /// that matters — the unit file and the daemon home still on disk.
    #[cfg(unix)]
    #[test]
    fn uninstall_without_systemd_refuses_and_touches_nothing() {
        let (home, _project) = installed_home_and_project();
        let daemon_home = home.path().join(".agent");
        let result = run_uninstall_with(
            home.path(),
            &daemon_home,
            false,
            true,
            false,
            &mut never_confirm,
            &mut never_systemctl,
        );
        assert!(matches!(result, Err(ServiceError::NoSystemd)), "{result:?}");
        assert!(unit_path(home.path()).is_file(), "unit must survive");
        assert!(daemon_home.is_dir(), "daemon home must survive");
    }

    /// The locked Linux argv, in order, with the unit unlinked in-process
    /// between them. `--user` is prepended by production's [`systemctl_user`],
    /// so the runner sees only the words after it — exactly as for `start` and
    /// `restart`. The bool records whether the unit still existed at each
    /// call, pinning the unlink between `disable` and `daemon-reload`.
    #[cfg(unix)]
    #[test]
    fn uninstall_disables_removes_unit_then_reloads() {
        let (home, _project) = installed_home_and_project();
        let daemon_home = home.path().join(".agent");
        let unit = unit_path(home.path());
        let mut calls: Vec<(Vec<String>, bool)> = Vec::new();
        let result = run_uninstall_with(
            home.path(),
            &daemon_home,
            true,
            false,
            false,
            &mut never_confirm,
            &mut |args: &[&str]| {
                calls.push((args.iter().map(|s| s.to_string()).collect(), unit.exists()));
                Ok(())
            },
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            calls,
            [
                (
                    vec![
                        "disable".to_string(),
                        "--now".to_string(),
                        UNIT_NAME.to_string()
                    ],
                    true
                ),
                (vec!["daemon-reload".to_string()], false),
            ]
        );
        assert!(!unit.exists(), "the unit file must be gone");
        assert!(
            daemon_home.is_dir(),
            "the daemon home must survive an unpurged uninstall"
        );
    }

    /// Idempotent, unlike `start`: with no unit on disk there is nothing to
    /// disable and nothing for `daemon-reload` to pick up, so `systemctl` is
    /// never spawned and the verb still succeeds.
    #[cfg(unix)]
    #[test]
    fn uninstall_without_unit_is_idempotent_and_never_spawns() {
        let home = tempfile::tempdir().unwrap();
        let daemon_home = home.path().join(".agent");
        let result = run_uninstall_with(
            home.path(),
            &daemon_home,
            true,
            false,
            false,
            &mut never_confirm,
            &mut never_systemctl,
        );
        assert!(result.is_ok(), "{result:?}");
    }

    /// Darwin teardown on Linux CI: the locked `bootout` argv against the
    /// injected uid, and the plist unlinked.
    #[cfg(unix)]
    #[test]
    fn launchd_uninstall_boots_out_and_removes_plist() {
        let (home, _project) = installed_home_and_project();
        let daemon_home = home.path().join(".agent");
        let plist = plist_path(home.path());
        let mut calls: Vec<Vec<String>> = Vec::new();
        let result = run_launchd_uninstall_with(
            home.path(),
            &daemon_home,
            false,
            false,
            FAKE_UID,
            &mut never_confirm,
            &mut |args: &[&str]| {
                calls.push(args.iter().map(|s| s.to_string()).collect());
                Ok(())
            },
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            calls,
            [[
                "bootout".to_string(),
                format!("gui/{FAKE_UID}/{LAUNCHD_LABEL}")
            ]]
        );
        assert!(!plist.exists(), "the plist must be gone");
        assert!(daemon_home.is_dir(), "the daemon home must survive");
    }

    /// A plist that is already gone: the `bootout` is still issued and its
    /// non-zero exit — what launchd says for a label it never loaded — is
    /// swallowed, so the verb succeeds and reports `(none)`.
    #[cfg(unix)]
    #[test]
    fn launchd_uninstall_without_plist_ignores_bootout_failure() {
        let home = tempfile::tempdir().unwrap();
        let daemon_home = home.path().join(".agent");
        let mut calls: Vec<Vec<String>> = Vec::new();
        let result = run_launchd_uninstall_with(
            home.path(),
            &daemon_home,
            false,
            false,
            FAKE_UID,
            &mut never_confirm,
            &mut |args: &[&str]| {
                calls.push(args.iter().map(|s| s.to_string()).collect());
                Err(ServiceError::Uid("no such service".into()))
            },
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(calls.len(), 1, "bootout is best-effort but still issued");
        assert!(!plist_path(home.path()).exists());
    }

    /// Confirm-before-mutation on the Darwin path, run on Linux CI. `bootout`
    /// is not a read: it stops a running LaunchAgent, so it counts as the
    /// mutation the confirmation guards. A `--purge` refused for non-tty stdin
    /// must therefore leave `launchctl` unspawned, the plist on disk and the
    /// daemon home intact — the same guarantee
    /// `purge_without_yes_on_non_tty_refuses_and_touches_nothing` pins on Linux.
    #[cfg(unix)]
    #[test]
    fn launchd_purge_without_yes_on_non_tty_refuses_before_bootout() {
        let (home, _project) = installed_home_and_project();
        let daemon_home = home.path().join(".agent");
        let result = run_launchd_uninstall_with(
            home.path(),
            &daemon_home,
            true,
            false,
            FAKE_UID,
            &mut not_a_tty,
            &mut never_launchctl,
        );
        assert!(matches!(result, Err(ServiceError::NotATty)), "{result:?}");
        assert!(plist_path(home.path()).is_file(), "the plist must survive");
        assert!(daemon_home.is_dir(), "daemon home must survive");
    }

    /// `--purge` without `--yes` in a pipeline: exit-2 refusal *before* any
    /// mutation, so the unit, the daemon home and the project store are all
    /// untouched and `systemctl` was never spawned.
    #[cfg(unix)]
    #[test]
    fn purge_without_yes_on_non_tty_refuses_and_touches_nothing() {
        let (home, project) = installed_home_and_project();
        let daemon_home = home.path().join(".agent");
        let result = run_uninstall_with(
            home.path(),
            &daemon_home,
            true,
            true,
            false,
            &mut not_a_tty,
            &mut never_systemctl,
        );
        assert!(matches!(result, Err(ServiceError::NotATty)), "{result:?}");
        assert!(unit_path(home.path()).is_file(), "unit must survive");
        assert!(daemon_home.is_dir(), "daemon home must survive");
        assert!(project.path().join(".agent").is_dir());
    }

    /// A tty that answers anything but `y` / `Y` declines: same
    /// nothing-was-touched guarantee, a different exit-2 error.
    #[cfg(unix)]
    #[test]
    fn purge_declined_on_tty_touches_nothing() {
        let (home, _project) = installed_home_and_project();
        let daemon_home = home.path().join(".agent");
        let mut prompts: Vec<String> = Vec::new();
        let result = run_uninstall_with(
            home.path(),
            &daemon_home,
            true,
            true,
            false,
            &mut |prompt: &str| {
                prompts.push(prompt.to_string());
                Ok(Some("n\n".to_string()))
            },
            &mut never_systemctl,
        );
        assert!(matches!(result, Err(ServiceError::Declined)), "{result:?}");
        assert_eq!(prompts.len(), 1);
        assert!(
            prompts[0].contains(&daemon_home.display().to_string()),
            "the prompt must name the directory it is about to delete: {:?}",
            prompts[0]
        );
        assert!(unit_path(home.path()).is_file(), "unit must survive");
        assert!(daemon_home.is_dir(), "daemon home must survive");
    }

    /// The accept half of the same branch `purge_declined_on_tty_touches_nothing`
    /// pins: a tty that answers `y` proceeds, the prompt was really issued and
    /// named the directory, and the blast radius is still exactly the daemon
    /// home — the sibling project store, in its own tree the way a user's repo
    /// is, comes through untouched.
    #[cfg(unix)]
    #[test]
    fn purge_accepted_on_tty_deletes_daemon_home() {
        let (home, project) = installed_home_and_project();
        let daemon_home = home.path().join(".agent");
        let project_store = project.path().join(".agent");
        let mut prompts: Vec<String> = Vec::new();
        let mut calls: Vec<Vec<String>> = Vec::new();
        let result = run_uninstall_with(
            home.path(),
            &daemon_home,
            true,
            true,
            false,
            &mut |prompt: &str| {
                prompts.push(prompt.to_string());
                Ok(Some("y\n".to_string()))
            },
            &mut |args: &[&str]| {
                calls.push(args.iter().map(|s| s.to_string()).collect());
                Ok(())
            },
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(prompts.len(), 1);
        assert!(
            prompts[0].contains(&daemon_home.display().to_string()),
            "the prompt must name the directory it is about to delete: {:?}",
            prompts[0]
        );
        assert!(
            !daemon_home.exists(),
            "a typed `y` must delete the daemon home"
        );
        assert!(
            project_store.is_dir(),
            "a typed `y` must never reach a per-project .agent/ store"
        );
        assert!(
            project_store.join("AGENT_LEARNINGS.jsonl").is_file(),
            "the project store's contents must be untouched"
        );
        assert_eq!(calls.len(), 2, "teardown still ran: {calls:?}");
    }

    /// The two edges of the typed answer. It is trimmed before it is judged, so
    /// a leading space and an uppercase `Y` still proceed; anything else
    /// declines and deletes nothing — `yes` included, which reads like consent
    /// but is not one of the two accepted words. Fresh fixtures per case,
    /// because the accepting half purges.
    #[cfg(unix)]
    #[test]
    fn purge_answer_is_trimmed_and_only_y_or_upper_y_accepted() {
        let (home, _project) = installed_home_and_project();
        let daemon_home = home.path().join(".agent");
        let result = run_uninstall_with(
            home.path(),
            &daemon_home,
            true,
            true,
            false,
            &mut |_prompt: &str| Ok(Some(" Y\n".to_string())),
            &mut |_args: &[&str]| Ok(()),
        );
        assert!(
            result.is_ok(),
            "` Y` must be trimmed and accepted: {result:?}"
        );
        assert!(!daemon_home.exists(), "` Y` must delete the daemon home");

        let (home, project) = installed_home_and_project();
        let daemon_home = home.path().join(".agent");
        let result = run_uninstall_with(
            home.path(),
            &daemon_home,
            true,
            true,
            false,
            &mut |_prompt: &str| Ok(Some("yes\n".to_string())),
            &mut never_systemctl,
        );
        assert!(matches!(result, Err(ServiceError::Declined)), "{result:?}");
        assert!(daemon_home.is_dir(), "`yes` must delete nothing");
        assert!(unit_path(home.path()).is_file(), "unit must survive");
        assert!(project.path().join(".agent").is_dir());
    }

    /// The founder lock, in one assertion: `--purge --yes` deletes the daemon
    /// home and **only** the daemon home. The sibling project store — a real
    /// `.agent/` directory in its own tree, exactly what a user's repo has —
    /// is still there afterwards, and the report says so on the line that
    /// always prints.
    #[cfg(unix)]
    #[test]
    fn purge_with_yes_deletes_daemon_home_and_spares_project_store() {
        let (home, project) = installed_home_and_project();
        let daemon_home = home.path().join(".agent");
        let project_store = project.path().join(".agent");
        let mut calls: Vec<Vec<String>> = Vec::new();
        let result = run_uninstall_with(
            home.path(),
            &daemon_home,
            true,
            true,
            true,
            &mut never_confirm,
            &mut |args: &[&str]| {
                calls.push(args.iter().map(|s| s.to_string()).collect());
                Ok(())
            },
        );
        assert!(result.is_ok(), "{result:?}");
        assert!(!daemon_home.exists(), "--purge must delete the daemon home");
        assert!(
            project_store.is_dir(),
            "--purge must never reach a per-project .agent/ store"
        );
        assert!(
            project_store.join("AGENT_LEARNINGS.jsonl").is_file(),
            "the project store's contents must be untouched"
        );
        assert!(!unit_path(home.path()).exists());
        assert_eq!(calls.len(), 2, "teardown still ran: {calls:?}");
    }

    /// `--purge` on a daemon home that is already gone is success, keeping the
    /// flag as idempotent as the teardown it follows.
    #[cfg(unix)]
    #[test]
    fn purge_with_missing_daemon_home_succeeds() {
        let home = tempfile::tempdir().unwrap();
        let daemon_home = home.path().join(".agent");
        let result = run_uninstall_with(
            home.path(),
            &daemon_home,
            true,
            true,
            true,
            &mut never_confirm,
            &mut never_systemctl,
        );
        assert!(result.is_ok(), "{result:?}");
    }

    /// The locked stdout, both shapes. The per-project `preserved:` line is
    /// present either way — it is the user-visible half of the promise that no
    /// flag on this verb touches a project store.
    #[test]
    fn render_uninstall_report_is_byte_stable() {
        let unit = PathBuf::from("/h/.config/systemd/user/dreamd.service");
        let daemon_home = PathBuf::from("/h/.agent");
        assert_eq!(
            render_uninstall_report(Some(&unit), &daemon_home, false),
            "removed: /h/.config/systemd/user/dreamd.service\n\
             preserved: /h/.agent (daemon home)\n\
             preserved: per-project .agent/ stores\n"
        );
        assert_eq!(
            render_uninstall_report(Some(&unit), &daemon_home, true),
            "removed: /h/.config/systemd/user/dreamd.service\n\
             removed: /h/.agent (daemon home)\n\
             preserved: per-project .agent/ stores\n"
        );
        assert_eq!(
            render_uninstall_report(None, &daemon_home, false),
            "removed: (none)\n\
             preserved: /h/.agent (daemon home)\n\
             preserved: per-project .agent/ stores\n"
        );
    }

    // ---- Windows Task Scheduler path (AILAB-203) ----------------------------
    //
    // Same rule as every section above, and for the same reason: no test here
    // may spawn `schtasks`, `icacls` or `whoami`. Both process runners are
    // injected — a recording closure on the paths that must reach the
    // scheduler, `never_schtasks` / `never_icacls` on the paths that must
    // refuse first — and the SID is `FAKE_SID` through an injected resolver.
    // Nothing below is gated on the Windows target: that is the point. The
    // Darwin path is already covered on Linux CI through `run_launchd_*_with`,
    // and `run_schtasks_*_with` gets the identical treatment, so the one OS
    // that cannot run this repository's merge gates still has its install,
    // start, restart, status and uninstall paths executed on every push.

    /// The installing user's SID, in the shape `whoami /user` prints: a
    /// machine-relative account under a real domain RID. Injected, so
    /// `whoami` is never spawned (the [`FAKE_UID`] treatment).
    const FAKE_SID: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";

    fn fake_sid() -> Result<String, ServiceError> {
        Ok(FAKE_SID.to_string())
    }

    /// A SID resolver that must never be consulted: every path that refuses
    /// before the token is minted has to refuse before `whoami` would run.
    fn never_sid() -> Result<String, ServiceError> {
        panic!("the SID must not be resolved")
    }

    fn never_schtasks(args: &[&str]) -> Result<(), ServiceError> {
        panic!("schtasks must not be reached, got {args:?}")
    }

    fn never_icacls(args: &[&str]) -> Result<(), ServiceError> {
        panic!("icacls must not be reached, got {args:?}")
    }

    /// The task definition the spec locks, for the fake exe and
    /// `/home/u/proj`. Tab-indented like [`EXPECTED_PLIST`]; `\t` escapes keep
    /// the tabs visible and safe from editors. The paths are POSIX only
    /// because the fixture reuses [`FAKE_EXE`] — the renderer does no path
    /// interpretation at all, so the separator is not part of what this pins.
    const EXPECTED_TASK_XML: &str = "\
<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">
\t<Triggers>
\t\t<LogonTrigger>
\t\t\t<Enabled>true</Enabled>
\t\t</LogonTrigger>
\t</Triggers>
\t<Principals>
\t\t<Principal id=\"Author\">
\t\t\t<LogonType>InteractiveToken</LogonType>
\t\t\t<RunLevel>LeastPrivilege</RunLevel>
\t\t</Principal>
\t</Principals>
\t<Settings>
\t\t<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
\t\t<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
\t\t<StartWhenAvailable>true</StartWhenAvailable>
\t\t<AllowStartOnDemand>true</AllowStartOnDemand>
\t\t<RestartOnFailure>
\t\t\t<Interval>PT1M</Interval>
\t\t\t<Count>3</Count>
\t\t</RestartOnFailure>
\t</Settings>
\t<Actions Context=\"Author\">
\t\t<Exec>
\t\t\t<Command>/opt/dreamd/bin/dreamd</Command>
\t\t\t<Arguments>watch</Arguments>
\t\t\t<WorkingDirectory>/home/u/proj</WorkingDirectory>
\t\t</Exec>
\t</Actions>
</Task>
";

    #[test]
    fn render_task_xml_is_byte_stable() {
        let xml = render_task_xml(Path::new(FAKE_EXE), Path::new("/home/u/proj"));
        assert_eq!(xml, EXPECTED_TASK_XML);
    }

    /// The element contract, asserted independently of the byte-stable text
    /// above so a reformat cannot quietly take a key with it: the action runs
    /// foreground `watch` in the install-time project root, the trigger is the
    /// logon one, and the five keys this backend must never grow are absent.
    #[test]
    fn render_task_xml_contract_nodes() {
        let xml = render_task_xml(Path::new(FAKE_EXE), Path::new("/home/u/proj"));

        assert!(
            xml.contains(&format!("<Command>{FAKE_EXE}</Command>")),
            "the action must be this binary: {xml}"
        );
        assert!(
            xml.contains("<Arguments>watch</Arguments>"),
            "the scheduler supervises foreground `watch`, nothing else: {xml}"
        );
        assert!(
            xml.lines()
                .any(|l| l.trim() == "<WorkingDirectory>/home/u/proj</WorkingDirectory>"),
            "WorkingDirectory must be the exact project root: {xml}"
        );
        assert!(
            xml.contains("<LogonTrigger>"),
            "the task must fire at logon: {xml}"
        );
        assert!(
            xml.contains("<RunLevel>LeastPrivilege</RunLevel>"),
            "a memory daemon never asks for elevation: {xml}"
        );

        // The literals below are assembled so the forbidden keys never appear
        // verbatim in this source file (anti-pattern greps). The redirection
        // nodes would put a second writer on ~/.agent/dreamd.log, whose
        // tracing file layer truncates on open (AILAB-184); the environment
        // node is the task form of the HOME injection AILAB-584 banned from
        // the unit; the elevated run level has no business on a per-user
        // memory daemon; and a boot trigger would fire outside the interactive
        // session that owns %USERPROFILE% and therefore ~/.agent.
        for forbidden in [
            concat!("Standard", "Output"),
            concat!("Standard", "Error"),
            concat!("Environment", "Variables"),
            concat!("Highest", "Available"),
            concat!("Boot", "Trigger"),
        ] {
            assert!(
                !xml.contains(forbidden),
                "the task definition must never carry {forbidden}: {xml}"
            );
        }

        assert!(
            xml.ends_with("</Task>\n"),
            "the definition must end with a single trailing newline: {xml:?}"
        );
        assert!(
            !xml.ends_with("\n\n"),
            "exactly one trailing newline, never two: {xml:?}"
        );
    }

    /// Both fields go through `xml_escape`, and neither goes through the
    /// systemd `%` doubling: the Task Scheduler does no specifier expansion,
    /// so a literal `%` in either path must survive as one character.
    #[test]
    fn render_task_xml_escapes_both_paths() {
        let xml = render_task_xml(
            Path::new("/opt/a&b<c>d\"e/dreamd"),
            Path::new("/home/u/50%done&more<x>\"y\""),
        );
        assert!(
            xml.contains("<Command>/opt/a&amp;b&lt;c&gt;d&quot;e/dreamd</Command>"),
            "the exe path must be XML-escaped: {xml}"
        );
        assert!(
            xml.contains(
                "<WorkingDirectory>/home/u/50%done&amp;more&lt;x&gt;&quot;y&quot;</WorkingDirectory>"
            ),
            "the project root must be XML-escaped: {xml}"
        );
        assert!(
            !xml.contains("a&b"),
            "no raw ampersand may survive in the exe path: {xml}"
        );
        assert!(
            !xml.contains("more<x>"),
            "no raw angle bracket may survive in the project root: {xml}"
        );
        assert!(
            !xml.contains("%%"),
            "never systemd-escape a task definition: {xml}"
        );
    }

    #[test]
    fn task_xml_path_is_under_appdata_roaming_dreamd() {
        let path = task_xml_path(Path::new("/h"));
        assert_eq!(
            path,
            PathBuf::from("/h/AppData/Roaming/dreamd/dev.dreamd.dreamd.xml")
        );
        assert_eq!(
            path.file_name().unwrap(),
            std::ffi::OsStr::new("dev.dreamd.dreamd.xml"),
            "the basename is the task name plus .xml, like the plist's is the label"
        );
    }

    /// Pull the token out of an `auth.json` body, asserting the wire format
    /// AILAB-192 will parse: one compact object, exactly one field, and 64
    /// **lowercase** hex characters (32 random bytes). Returns the token so a
    /// caller can compare two mints.
    fn auth_token(body: &str) -> String {
        let value: serde_json::Value = serde_json::from_str(body)
            .unwrap_or_else(|e| panic!("auth.json must be valid JSON ({e}): {body:?}"));
        let object = value
            .as_object()
            .unwrap_or_else(|| panic!("auth.json must be a JSON object: {body:?}"));
        assert_eq!(
            object.len(),
            1,
            "auth.json carries the token and nothing else: {body:?}"
        );
        let token = object["token"]
            .as_str()
            .unwrap_or_else(|| panic!("`token` must be a string: {body:?}"))
            .to_string();
        assert_eq!(
            token.len(),
            64,
            "32 random bytes hex-encode to exactly 64 characters: {token:?}"
        );
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "the token must be lowercase hex only: {token:?}"
        );
        token
    }

    /// A first install on a fresh HOME: the definition and the token land on
    /// disk, the ACL is tightened to the installing SID, and only then is the
    /// task registered — with no `/F` (nothing to overwrite) and, above all,
    /// no `/Run`. The booleans recorded alongside each argv are what pins that
    /// ordering: both files must already exist when the runner is called.
    #[cfg(unix)]
    #[test]
    fn schtasks_first_install_writes_xml_and_token_then_creates_task() {
        let (_project, cwd, home) = fake_project_and_home();
        let root = AgentRoot::discover(&cwd).unwrap();
        let xml = task_xml_path(home.path());
        let auth = DaemonHome::new(home.path().join(".agent")).auth_json();
        assert!(!xml.exists() && !auth.exists(), "precondition: fresh HOME");

        let mut schtasks_calls: Vec<(Vec<String>, bool, bool)> = Vec::new();
        let mut icacls_calls: Vec<(Vec<String>, bool)> = Vec::new();
        let result = run_schtasks_install_with(
            &cwd,
            home.path(),
            false,
            fake_exe,
            fake_sid,
            &mut |args: &[&str]| {
                schtasks_calls.push((
                    args.iter().map(|s| s.to_string()).collect(),
                    xml.is_file(),
                    auth.is_file(),
                ));
                Ok(())
            },
            &mut |args: &[&str]| {
                icacls_calls.push((args.iter().map(|s| s.to_string()).collect(), auth.is_file()));
                Ok(())
            },
        );
        assert!(result.is_ok(), "{result:?}");

        assert_eq!(
            fs::read_to_string(&xml).unwrap(),
            render_task_xml(Path::new(FAKE_EXE), root.project_root()),
            "the definition on disk must be the rendered body"
        );
        assert_eq!(
            auth,
            home.path().join(".agent").join("auth.json"),
            "the token lives in the daemon home, never in a project store"
        );
        auth_token(&fs::read_to_string(&auth).unwrap());

        let argv: Vec<&[String]> = schtasks_calls.iter().map(|(a, ..)| a.as_slice()).collect();
        assert_eq!(
            argv,
            [&[
                "/Create".to_string(),
                "/TN".to_string(),
                SCHTASKS_TASK_NAME.to_string(),
                "/XML".to_string(),
                xml.display().to_string(),
            ][..]],
            "exactly one registration call: {argv:?}"
        );
        let flat = schtasks_calls
            .iter()
            .flat_map(|(a, ..)| a.iter())
            .cloned()
            .collect::<Vec<String>>();
        assert!(
            !flat.iter().any(|word| word == "/F"),
            "a first install has nothing to overwrite, so never /F: {flat:?}"
        );
        assert!(
            !flat.iter().any(|word| word == "/Run"),
            "install must never start the task: `watch` still exits 2 (AILAB-192): {flat:?}"
        );
        assert!(
            schtasks_calls
                .iter()
                .all(|(_, had_xml, had_auth)| *had_xml && *had_auth),
            "both files must be on disk before the scheduler is called: {schtasks_calls:?}"
        );

        assert_eq!(
            icacls_calls
                .iter()
                .map(|(a, _)| a.as_slice())
                .collect::<Vec<&[String]>>(),
            [&[
                auth.display().to_string(),
                "/inheritance:r".to_string(),
                "/grant:r".to_string(),
                format!("{FAKE_SID}:F"),
            ][..]],
            "the ACL must strip inheritance and grant the owner SID full control"
        );
        assert!(
            icacls_calls.iter().all(|(_, had_auth)| *had_auth),
            "the token file must exist before its ACL is rewritten: {icacls_calls:?}"
        );
    }

    /// The locked body, byte for byte: compact one-line JSON with a single
    /// trailing newline and no second field. It cannot be asserted that the
    /// token never reaches stdout — `install` prints with `println!`, which no
    /// unit test can capture — so the invariant is pinned structurally
    /// instead, at the one place the secret is written down.
    #[cfg(unix)]
    #[test]
    fn schtasks_install_writes_compact_single_line_auth_json() {
        let (_project, cwd, home) = fake_project_and_home();
        let auth = DaemonHome::new(home.path().join(".agent")).auth_json();

        let result = run_schtasks_install_with(
            &cwd,
            home.path(),
            false,
            fake_exe,
            fake_sid,
            &mut |_args: &[&str]| Ok(()),
            &mut |_args: &[&str]| Ok(()),
        );
        assert!(result.is_ok(), "{result:?}");

        let body = fs::read_to_string(&auth).unwrap();
        let token = auth_token(&body);
        assert_eq!(
            body,
            format!("{{\"token\":\"{token}\"}}\n"),
            "the body is a locked wire format, never a pretty-printed one"
        );
        assert_eq!(body.lines().count(), 1, "one line: {body:?}");
        assert!(
            !body.contains(' ') && !body.contains('\t'),
            "compact: no serializer whitespace: {body:?}"
        );
    }

    /// The usage refusal comes before every write and before either runner:
    /// no definition, no daemon home, no token. The exe resolver and the SID
    /// resolver both panic if they are reached, so a `CurrentExe` or a
    /// `whoami` failure can never mask the exit-2 answer.
    #[test]
    fn schtasks_install_without_project_root_refuses_and_writes_nothing() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        assert!(
            AgentRoot::discover(cwd.path()).is_err(),
            "precondition: an ancestor of {} carries .agent/",
            cwd.path().display()
        );

        let result = run_schtasks_install_with(
            cwd.path(),
            home.path(),
            true,
            || -> Result<PathBuf, ServiceError> {
                panic!("exe must not be resolved before the project-root check")
            },
            never_sid,
            &mut never_schtasks,
            &mut never_icacls,
        );
        assert!(
            matches!(result, Err(ServiceError::NoProjectRoot)),
            "expected NoProjectRoot, got {result:?}"
        );
        assert!(
            !task_xml_path(home.path()).exists(),
            "refusal must happen before anything is written"
        );
        assert!(
            !home.path().join("AppData").exists(),
            "refusal must not even create the task directory"
        );
        assert!(
            !DaemonHome::new(home.path().join(".agent"))
                .auth_json()
                .exists(),
            "refusal must not mint a token"
        );
        assert!(
            !home.path().join(".agent").exists(),
            "refusal must not even create the daemon home"
        );
    }

    /// The Darwin `--force` gate, one promise wider. An existing definition
    /// without `--force` is exit 2 with nothing rewritten — and `auth.json`
    /// is part of "nothing": a bare reinstall must never silently invalidate a
    /// token AILAB-192 already handed out.
    #[cfg(unix)]
    #[test]
    fn schtasks_install_refuses_when_task_xml_exists_without_force() {
        let (_project, cwd, home) = fake_project_and_home();
        let xml = task_xml_path(home.path());
        fs::create_dir_all(xml.parent().unwrap()).unwrap();
        fs::write(&xml, b"sentinel: not a task definition").unwrap();
        let auth = DaemonHome::new(home.path().join(".agent")).auth_json();
        fs::create_dir_all(auth.parent().unwrap()).unwrap();
        fs::write(&auth, b"sentinel: a live token").unwrap();

        let result = run_schtasks_install_with(
            &cwd,
            home.path(),
            false,
            fake_exe,
            never_sid,
            &mut never_schtasks,
            &mut never_icacls,
        );
        let err = match result {
            Err(e @ ServiceError::TaskExists(_)) => e,
            other => panic!("expected TaskExists, got {other:?}"),
        };
        match &err {
            ServiceError::TaskExists(p) => assert_eq!(p, &task_xml_path(home.path())),
            _ => unreachable!(),
        }
        assert_eq!(
            fs::read(&xml).unwrap(),
            b"sentinel: not a task definition",
            "refusal must leave the existing definition byte-identical"
        );
        assert_eq!(
            fs::read(&auth).unwrap(),
            b"sentinel: a live token",
            "refusal must not rotate the token"
        );
        let msg = err.to_string();
        assert!(msg.contains("--force"), "{msg}");
        assert!(msg.contains(&xml.display().to_string()), "{msg}");
    }

    /// `--force` is the overwrite *and* the rotation: the definition is
    /// rewritten from the current exe and project root, the token turns over,
    /// and `/F` rides along so `schtasks /Create` accepts an existing task
    /// name. It is still not a start — no `/Run` appears anywhere.
    #[cfg(unix)]
    #[test]
    fn schtasks_install_with_force_overwrites_xml_and_rotates_token() {
        let (_project, cwd, home) = fake_project_and_home();
        let root = AgentRoot::discover(&cwd).unwrap();
        let xml = task_xml_path(home.path());
        fs::create_dir_all(xml.parent().unwrap()).unwrap();
        fs::write(&xml, b"sentinel: not a task definition").unwrap();
        let auth = DaemonHome::new(home.path().join(".agent")).auth_json();
        fs::create_dir_all(auth.parent().unwrap()).unwrap();
        let seeded = "0".repeat(64);
        fs::write(&auth, format!("{{\"token\":\"{seeded}\"}}\n")).unwrap();

        let mut calls: Vec<Vec<String>> = Vec::new();
        let result = run_schtasks_install_with(
            &cwd,
            home.path(),
            true,
            fake_exe,
            fake_sid,
            &mut |args: &[&str]| {
                calls.push(args.iter().map(|s| s.to_string()).collect());
                Ok(())
            },
            &mut |_args: &[&str]| Ok(()),
        );
        assert!(result.is_ok(), "{result:?}");

        assert_eq!(
            fs::read_to_string(&xml).unwrap(),
            render_task_xml(Path::new(FAKE_EXE), root.project_root()),
            "--force must overwrite the definition with the fresh render"
        );
        let rotated = auth_token(&fs::read_to_string(&auth).unwrap());
        assert_ne!(rotated, seeded, "--force must rotate the token");

        assert_eq!(
            calls,
            [[
                "/Create".to_string(),
                "/TN".to_string(),
                SCHTASKS_TASK_NAME.to_string(),
                "/XML".to_string(),
                xml.display().to_string(),
                "/F".to_string(),
            ]],
            "/F is the only difference from the first-install argv"
        );
        assert_eq!(
            calls[0].last().map(String::as_str),
            Some("/F"),
            "the overwrite flag rides last: {calls:?}"
        );
        assert!(
            !calls.iter().flatten().any(|word| word == "/Run"),
            "--force is still not a start; the logon trigger is: {calls:?}"
        );
    }

    /// `schtasks /Run /TN <name>` and nothing else. What this surfaces is the
    /// scheduler's exit, never the supervised process's — until AILAB-192 a
    /// perfectly successful `/Run` still leaves `status` at `stopped`, which
    /// is why nothing here asserts that `watch` stays up.
    #[test]
    fn schtasks_start_runs_the_task() {
        let mut calls: Vec<Vec<String>> = Vec::new();
        let result = run_schtasks_start_with(&mut |args: &[&str]| {
            calls.push(args.iter().map(|s| s.to_string()).collect());
            Ok(())
        });
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            calls,
            [[
                "/Run".to_string(),
                "/TN".to_string(),
                SCHTASKS_TASK_NAME.to_string()
            ]]
        );
    }

    /// AILAB-185 on the one backend whose bounce genuinely takes two calls:
    /// the Task Scheduler has no `kickstart -k`, so `/End` must precede
    /// `/Run`, in that order. Bounce-only — the argv carries no `/Create` and
    /// no `/XML`, so the definition and the token are left exactly as
    /// `install` wrote them.
    #[test]
    fn schtasks_restart_ends_then_runs() {
        let mut calls: Vec<Vec<String>> = Vec::new();
        let result = run_schtasks_restart_with(&mut |args: &[&str]| {
            calls.push(args.iter().map(|s| s.to_string()).collect());
            Ok(())
        });
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            calls,
            [
                vec![
                    "/End".to_string(),
                    "/TN".to_string(),
                    SCHTASKS_TASK_NAME.to_string()
                ],
                vec![
                    "/Run".to_string(),
                    "/TN".to_string(),
                    SCHTASKS_TASK_NAME.to_string()
                ],
            ]
        );
        assert!(
            !calls.iter().flatten().any(|word| word == "/Create"),
            "restart must never rewrite the definition: {calls:?}"
        );
    }

    /// The `/End` is best-effort, exactly as the Darwin `bootout` under
    /// `--force` is (`launchd_install_with_force_ignores_bootout_failure`):
    /// ending a task that is not running exits non-zero, and on this backend
    /// that is the *common* case until AILAB-192 lands. The `/Run` is the call
    /// that must succeed, and the verb still does.
    #[cfg(unix)]
    #[test]
    fn schtasks_restart_ignores_end_failure() {
        use std::os::unix::process::ExitStatusExt;

        let mut calls: Vec<Vec<String>> = Vec::new();
        let result = run_schtasks_restart_with(&mut |args: &[&str]| {
            calls.push(args.iter().map(|s| s.to_string()).collect());
            if args[0] == "/End" {
                // What the scheduler says for a task that is not running.
                Err(ServiceError::Schtasks {
                    args: args.join(" "),
                    status: ExitStatus::from_raw(1 << 8),
                    stderr: "ERROR: The system cannot find the file specified.".into(),
                })
            } else {
                Ok(())
            }
        });
        assert!(
            result.is_ok(),
            "a failed /End must not fail restart: {result:?}"
        );
        assert_eq!(calls.len(), 2, "/Run must still follow: {calls:?}");
        assert_eq!(calls[1][0], "/Run");
    }

    /// The locked Windows teardown argv, and the blast radius that goes with
    /// it. `/F` here is the *no-prompt* flag, not the install overwrite gate:
    /// `schtasks /Delete` otherwise waits for a keystroke, which a one-shot
    /// verb must never do. Without `--purge` nothing in the daemon home is
    /// touched — `auth.json` included, which is the whole reason the token
    /// lives there rather than beside the definition.
    #[cfg(unix)]
    #[test]
    fn schtasks_uninstall_deletes_task_and_preserves_auth_json() {
        let (home, _project) = installed_home_and_project();
        let daemon_home = home.path().join(".agent");
        let auth = DaemonHome::new(daemon_home.clone()).auth_json();
        fs::write(&auth, "{\"token\":\"deadbeef\"}\n").unwrap();
        let xml = task_xml_path(home.path());
        fs::create_dir_all(xml.parent().unwrap()).unwrap();
        fs::write(&xml, "<Task/>\n").unwrap();

        let mut calls: Vec<Vec<String>> = Vec::new();
        let result = run_schtasks_uninstall_with(
            home.path(),
            &daemon_home,
            false,
            false,
            &mut never_confirm,
            &mut |args: &[&str]| {
                calls.push(args.iter().map(|s| s.to_string()).collect());
                Ok(())
            },
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            calls,
            [[
                "/Delete".to_string(),
                "/TN".to_string(),
                SCHTASKS_TASK_NAME.to_string(),
                "/F".to_string(),
            ]]
        );
        assert!(!xml.exists(), "the task definition must be gone");
        assert!(
            auth.is_file(),
            "an uninstall without --purge must leave the token in place"
        );
        assert!(daemon_home.is_dir(), "the daemon home must survive");
    }

    /// The accept half on the Windows path, run on Linux CI: a typed `y`
    /// takes the daemon home — and with it the token that lived inside it —
    /// while the sibling project store, in its own tree the way a user's repo
    /// is, comes through untouched.
    #[cfg(unix)]
    #[test]
    fn schtasks_purge_accepted_deletes_daemon_home_and_spares_project_store() {
        let (home, project) = installed_home_and_project();
        let daemon_home = home.path().join(".agent");
        let auth = DaemonHome::new(daemon_home.clone()).auth_json();
        fs::write(&auth, "{\"token\":\"deadbeef\"}\n").unwrap();
        let project_store = project.path().join(".agent");
        let xml = task_xml_path(home.path());
        fs::create_dir_all(xml.parent().unwrap()).unwrap();
        fs::write(&xml, "<Task/>\n").unwrap();

        let mut prompts: Vec<String> = Vec::new();
        let result = run_schtasks_uninstall_with(
            home.path(),
            &daemon_home,
            true,
            false,
            &mut |prompt: &str| {
                prompts.push(prompt.to_string());
                Ok(Some("y\n".to_string()))
            },
            &mut |_args: &[&str]| Ok(()),
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(prompts.len(), 1);
        assert!(
            prompts[0].contains(&daemon_home.display().to_string()),
            "the prompt must name the directory it is about to delete: {:?}",
            prompts[0]
        );
        assert!(!daemon_home.exists(), "--purge must delete the daemon home");
        assert!(!auth.exists(), "the token goes with the daemon home");
        assert!(
            project_store.join("AGENT_LEARNINGS.jsonl").is_file(),
            "--purge must never reach a per-project .agent/ store"
        );
        assert!(!xml.exists(), "the task definition must be gone");
    }

    /// Idempotent, like the other two backends' teardown: no definition on
    /// disk is not a failure. The `/Delete` is still issued and its non-zero
    /// exit — what the scheduler says for a task it does not have — is
    /// swallowed, and the report's first line is `(none)`.
    #[cfg(unix)]
    #[test]
    fn schtasks_uninstall_without_xml_is_idempotent() {
        use std::os::unix::process::ExitStatusExt;

        let home = tempfile::tempdir().unwrap();
        let daemon_home = home.path().join(".agent");
        assert!(
            !task_xml_path(home.path()).exists(),
            "precondition: fresh HOME"
        );

        let mut calls: Vec<Vec<String>> = Vec::new();
        let result = run_schtasks_uninstall_with(
            home.path(),
            &daemon_home,
            false,
            false,
            &mut never_confirm,
            &mut |args: &[&str]| {
                calls.push(args.iter().map(|s| s.to_string()).collect());
                Err(ServiceError::Schtasks {
                    args: args.join(" "),
                    status: ExitStatus::from_raw(1 << 8),
                    stderr: "ERROR: The system cannot find the file specified.".into(),
                })
            },
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(calls.len(), 1, "/Delete is best-effort but still issued");
        // `finish_uninstall` prints the report with `println!`, which no unit
        // test can capture; the wording it hands over for "nothing was
        // registered" is asserted against the renderer directly.
        assert!(
            render_uninstall_report(None, &daemon_home, false).starts_with("removed: (none)\n"),
            "an uninstall that removed nothing must say so"
        );
    }

    // ---- `service status` on schtasks (AILAB-203) ---------------------------

    /// The exact argv the report verb hands its `Query` runner on Windows.
    /// `/FO LIST` pins the `Key: Value` block — never the default table, whose
    /// columns are truncated to the console width.
    const EXPECTED_QUERY_ARGV: [&str; 6] =
        ["/Query", "/TN", "dev.dreamd.dreamd", "/FO", "LIST", "/V"];

    /// `schtasks /Query … /FO LIST /V` stdout for a task the scheduler is
    /// currently running. Padded like the real thing.
    const QUERY_RUNNING: &str = "\
Folder: \\
HostName:                             DESKTOP-DREAMD
TaskName:                             \\dev.dreamd.dreamd
Next Run Time:                        N/A
Status:                               Running
Logon Mode:                           Interactive only
";

    /// The same dump for a registered task that is idle — the common answer
    /// until AILAB-192, because `watch` exits 2 the moment the trigger fires.
    const QUERY_READY: &str = "\
TaskName:                             \\dev.dreamd.dreamd
Next Run Time:                        At log on
Status:                               Ready
";

    /// And for one the user has disabled: still installed, still not running.
    const QUERY_DISABLED: &str = "\
TaskName:                             \\dev.dreamd.dreamd
Next Run Time:                        Disabled
Status:                               Disabled
";

    /// A dump with no `Status:` line at all — a truncated or localised view.
    /// Unknown is stopped, never running.
    const QUERY_NO_STATUS: &str = "\
TaskName:                             \\dev.dreamd.dreamd
Next Run Time:                        At log on
";

    /// Every schtasks answer is the same report shape: this backend, no pid
    /// and no start time. The `/Query` view reports a *next* run time rather
    /// than a start one and no process id at all, so `render_report` prints
    /// `-` for both — asserted here so no future parse can quietly invent one.
    fn assert_schtasks_report(report: &ServiceReport, state: ServiceState) {
        assert_eq!(report.backend, ServiceBackend::Schtasks);
        assert_eq!(report.state, state);
        assert_eq!(report.pid, None, "schtasks /Query reports no process id");
        assert_eq!(
            report.active_since, None,
            "schtasks /Query reports a next run time, never a start one"
        );
        assert_eq!(ServiceBackend::Schtasks.label(), "schtasks");
    }

    #[test]
    fn status_schtasks_argv_is_query_list_verbose() {
        let mut calls: Vec<Vec<String>> = Vec::new();
        let report = query_schtasks_status(Path::new("/h"), &mut |args: &[&str]| {
            calls.push(args.iter().map(|s| s.to_string()).collect());
            Ok(QUERY_RUNNING.to_string())
        })
        .unwrap();
        assert_eq!(calls.len(), 1, "exactly one query: {calls:?}");
        assert_eq!(calls[0], EXPECTED_QUERY_ARGV);
        assert_schtasks_report(&report, ServiceState::Running);

        let rendered = render_report(&report, &[]);
        assert!(rendered.contains("service: running\n"), "{rendered}");
        assert!(rendered.contains("backend: schtasks\n"), "{rendered}");
        assert!(rendered.contains("pid: -\n"), "{rendered}");
        assert!(rendered.contains("active_since: -\n"), "{rendered}");
    }

    /// `Running` is the only running answer. `Ready`, `Disabled` and a dump
    /// with no `Status:` line at all are all stopped — never `NotInstalled`,
    /// because the scheduler answered at all, and never `Failed`, which has no
    /// Task Scheduler equivalent.
    #[test]
    fn status_schtasks_ready_disabled_and_missing_status_are_stopped() {
        for fixture in [QUERY_READY, QUERY_DISABLED, QUERY_NO_STATUS] {
            let report = query_schtasks_status(Path::new("/h"), &mut |_args: &[&str]| {
                Ok(fixture.to_string())
            })
            .unwrap();
            assert_schtasks_report(&report, ServiceState::Stopped);
            assert_eq!(
                parse_schtasks_query(fixture),
                ServiceState::Stopped,
                "fixture must parse as stopped: {fixture:?}"
            );
        }
        assert_eq!(parse_schtasks_query(QUERY_RUNNING), ServiceState::Running);
        assert_eq!(
            parse_schtasks_query(""),
            ServiceState::Stopped,
            "empty stdout is stopped, never running"
        );
    }

    /// A non-zero `/Query` exit is what the scheduler says for a task name it
    /// does not have, so it is a state answer rather than a failure — and the
    /// definition on disk is what tells the two states apart. Written but
    /// never registered is `stopped`, the [`query_launchd_status`] shape
    /// verbatim.
    #[cfg(unix)]
    #[test]
    fn status_schtasks_query_error_with_xml_is_stopped() {
        use std::os::unix::process::ExitStatusExt;

        let home = tempfile::tempdir().unwrap();
        let xml = task_xml_path(home.path());
        fs::create_dir_all(xml.parent().unwrap()).unwrap();
        fs::write(&xml, b"sentinel: not a task definition").unwrap();

        let report = query_schtasks_status(home.path(), &mut |args: &[&str]| {
            Err(ServiceError::Schtasks {
                args: args.join(" "),
                status: ExitStatus::from_raw(1 << 8),
                stderr: "ERROR: The system cannot find the file specified.".into(),
            })
        })
        .unwrap();
        assert_schtasks_report(&report, ServiceState::Stopped);
    }

    /// The other half of the same branch: no definition on disk either, so
    /// there is nothing installed to be stopped.
    #[cfg(unix)]
    #[test]
    fn status_schtasks_query_error_without_xml_is_not_installed() {
        use std::os::unix::process::ExitStatusExt;

        let home = tempfile::tempdir().unwrap();
        assert!(
            !task_xml_path(home.path()).exists(),
            "precondition: fresh HOME"
        );

        let mut calls = 0;
        let report = query_schtasks_status(home.path(), &mut |args: &[&str]| {
            calls += 1;
            Err(ServiceError::Schtasks {
                args: args.join(" "),
                status: ExitStatus::from_raw(1 << 8),
                stderr: "ERROR: The system cannot find the file specified.".into(),
            })
        })
        .unwrap();
        assert_eq!(calls, 1);
        assert_schtasks_report(&report, ServiceState::NotInstalled);
    }

    /// A `schtasks` that could not be spawned at all is a genuine failure and
    /// must propagate — it is not evidence about the task's state.
    #[test]
    fn status_schtasks_spawn_failure_propagates() {
        let result = query_schtasks_status(Path::new("/h"), &mut |_args: &[&str]| {
            Err(ServiceError::Spawn {
                program: "schtasks",
                source: std::io::Error::other("ENOENT"),
            })
        });
        assert!(
            matches!(
                result,
                Err(ServiceError::Spawn {
                    program: "schtasks",
                    ..
                })
            ),
            "a schtasks spawn failure is a real error, not a state: {result:?}"
        );
    }

    /// The dispatch half, the twin of `status_systemd_arm_never_resolves_uid`:
    /// the Windows arm reaches only its own runner. `systemctl` and
    /// `launchctl` are `never_query` and the uid resolver panics — there is no
    /// `gui/<uid>` domain on this backend, so `id -u` must never run.
    #[test]
    fn status_schtasks_arm_never_resolves_uid_or_touches_other_runners() {
        let mut calls = 0;
        let report = run_status_with(
            Some(ServiceBackend::Schtasks),
            Path::new("/h"),
            || -> Result<u32, ServiceError> {
                panic!("uid must not be resolved on the schtasks arm")
            },
            &mut never_query,
            &mut never_query,
            &mut |_args: &[&str]| {
                calls += 1;
                Ok(QUERY_RUNNING.to_string())
            },
        )
        .unwrap();
        assert_eq!(calls, 1, "the query runner must be called exactly once");
        assert_schtasks_report(&report, ServiceState::Running);
    }

    // ---- refusal and failure copy (AILAB-203) -------------------------------

    /// The no-backend refusal is reached on Linux without a user manager too —
    /// containers, most CI runners — so its copy must keep pointing at
    /// foreground `dreamd watch` and must name neither of the other two
    /// backends' machinery. `no_systemd_message_points_at_watch` pins what the
    /// copy says; this pins what it must not.
    #[test]
    fn no_systemd_message_names_no_other_backend() {
        let msg = ServiceError::NoSystemd.to_string();
        assert!(
            !msg.contains("LaunchAgent"),
            "never send a Linux user at the macOS path: {msg}"
        );
        assert!(
            !msg.to_ascii_lowercase().contains("schtasks"),
            "never send a Linux user at the Windows path: {msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn display_covers_windows_variants() {
        use std::os::unix::process::ExitStatusExt;

        let exists = ServiceError::TaskExists(PathBuf::from(
            "/h/AppData/Roaming/dreamd/dev.dreamd.dreamd.xml",
        ));
        let msg = exists.to_string();
        assert_eq!(
            msg,
            "scheduled-task XML /h/AppData/Roaming/dreamd/dev.dreamd.dreamd.xml already exists; \
             rerun `dreamd service install --force` to overwrite it."
        );
        assert!(
            msg.contains("--force"),
            "the refusal must name the way out: {msg}"
        );
        assert!(
            !msg.contains("LaunchAgent"),
            "the Windows gate must not borrow the Darwin copy: {msg}"
        );

        let schtasks = ServiceError::Schtasks {
            args: "/Create /TN dev.dreamd.dreamd /XML C:\\Users\\u\\task.xml".into(),
            status: ExitStatus::from_raw(1 << 8),
            stderr: "ERROR: Cannot create a file when that file already exists.\n".into(),
        };
        assert_eq!(
            schtasks.to_string(),
            "schtasks /Create /TN dev.dreamd.dreamd /XML C:\\Users\\u\\task.xml \
             failed (exit status: 1): ERROR: Cannot create a file when that file already exists."
        );

        let icacls = ServiceError::Icacls {
            args: format!("C:\\Users\\u\\.agent\\auth.json /inheritance:r /grant:r {FAKE_SID}:F"),
            status: ExitStatus::from_raw(5 << 8),
            stderr: "auth.json: Access is denied.\n".into(),
        };
        assert_eq!(
            icacls.to_string(),
            format!(
                "icacls C:\\Users\\u\\.agent\\auth.json /inheritance:r /grant:r {FAKE_SID}:F \
                 failed (exit status: 5): auth.json: Access is denied."
            )
        );

        let sid = ServiceError::Sid("no S-1- SID in `whoami /user` output \"\"".into());
        assert_eq!(
            sid.to_string(),
            "could not determine the current SID via `whoami /user`: \
             no S-1- SID in `whoami /user` output \"\""
        );
    }

    /// The ACL is the last thing `install` does before it registers the task,
    /// and its failure is [`ServiceError::Icacls`] rather than an I/O error —
    /// the file *was* written. It stays on disk for the same reason the unit
    /// stays on disk when `systemctl` fails: the user can inspect it and rerun.
    /// `never_schtasks` pins the other half, that the scheduler was never
    /// reached, because the ACL runs before `/Create`.
    #[cfg(unix)]
    #[test]
    fn schtasks_install_keeps_auth_json_when_icacls_fails() {
        use std::os::unix::process::ExitStatusExt;

        let (_project, cwd, home) = fake_project_and_home();
        let auth = DaemonHome::new(home.path().join(".agent")).auth_json();

        let result = run_schtasks_install_with(
            &cwd,
            home.path(),
            false,
            fake_exe,
            fake_sid,
            &mut never_schtasks,
            &mut |args: &[&str]| {
                Err(ServiceError::Icacls {
                    args: args.join(" "),
                    status: ExitStatus::from_raw(5 << 8),
                    stderr: "auth.json: Access is denied.".into(),
                })
            },
        );
        assert!(
            matches!(result, Err(ServiceError::Icacls { .. })),
            "expected Icacls, got {result:?}"
        );
        assert!(
            auth.is_file(),
            "the token must stay on disk after an icacls failure"
        );
        auth_token(&fs::read_to_string(&auth).unwrap());
        assert!(
            task_xml_path(home.path()).is_file(),
            "the definition must stay on disk too"
        );
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("/inheritance:r"), "{msg}");
        assert!(msg.contains("Access is denied"), "{msg}");
    }
}
