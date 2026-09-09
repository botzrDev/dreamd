//! `dreamd service install` / `start` / `restart` / `status` — the per-user
//! service that supervises the daemon: a Linux systemd **user** unit
//! (AILAB-190) or a macOS **LaunchAgent** (AILAB-169). One module, one backend
//! picked at runtime by [`detect_backend`].
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
//! locale-bound); on macOS `launchctl print gui/<uid>/dev.dreamd.dreamd`.
//! Daemon-level UDS liveness stays `dreamd status` (WEG-103); the two verbs
//! answer different questions and are deliberately separate.
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
//!   Under `Type=simple` systemd tracks the process it spawned, and launchd
//!   tracks the `ProgramArguments` process the same way; a double-fork would
//!   leave the supervisor watching the intermediate parent while the real
//!   daemon is reparented to init and escapes supervision entirely. The helper
//!   keeps its zero production call sites (ARCHITECTURE.md §8.1).
//! - **`WorkingDirectory` (unit line and plist key alike) is the [`AgentRoot`]
//!   project root discovered from `cwd` at install time.** `run_watch` does
//!   `AgentRoot::discover(cwd)` and exits 2 without a project root, so a
//!   service with no working directory would never start. Reinstalling from
//!   another project retargets it: silently on Linux (the unit is simply
//!   overwritten, no `--force` gate), and on macOS only with `--force` — the
//!   flag is the confirmation (no stdin prompt, same shape as
//!   `reset workspace --yes`); without it an existing plist is
//!   [`ServiceError::PlistExists`], exit 2, and nothing is rewritten.
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
//! - **All four verbs are one-shots and stay console-only** (AILAB-184): the
//!   tracing file layer truncates `~/.agent/dreamd.log`, so `wants_daemon_log`
//!   in `cli.rs` never grants it to `service`. The supervised `watch` still
//!   gets the file layer, and its stderr additionally lands in the user
//!   journal on Linux (JSON when stderr is not a TTY).
//!
//! Hosts with neither backend (Linux without a reachable systemd --user
//! manager: containers, most CI runners) get [`ServiceError::NoSystemd`] and
//! are pointed at foreground `dreamd watch`. The probe is `/run/systemd/system`
//! — systemd's documented "booted with systemd" check — and never spawns
//! `systemctl`; macOS is picked by `cfg!(target_os = "macos")` and never
//! probed. Every process runner is injected — [`Systemctl`] / [`Launchctl`]
//! for the verbs that only need an exit status, the stdout-returning
//! [`Query`] for the one that reads a report — so unit tests pass a recording
//! closure and can never spawn either supervisor; that is how Linux CI
//! exercises the macOS path. The uid for the `gui/<uid>` domain is injected
//! too (production reads `id -u` stdout; no FFI).

use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use dreamd_core::io::write_atomic;
use dreamd_core::{AgentRoot, LayoutError};

/// Name of the user unit written by `install` and started by `start`.
pub(crate) const UNIT_NAME: &str = "dreamd.service";

/// launchd label of the LaunchAgent written by `install` on macOS; also the
/// plist's basename (`<label>.plist`) and the service name under `gui/<uid>/`.
pub(crate) const LAUNCHD_LABEL: &str = "dev.dreamd.dreamd";

/// Number of trailing `~/.agent/dreamd.log` lines `service status` echoes
/// (Linear AILAB-178). `dreamd status` keeps its own 5 (`status::LOG_TAIL_LINES`).
pub(crate) const STATUS_LOG_TAIL_LINES: usize = 10;

/// How `install` / `start` / `restart` reach systemd. Production passes
/// [`systemctl_user`]; tests pass a recording closure, so no unit test can ever
/// spawn `systemctl` (this dev box has a live user manager; GHA runners mostly
/// do not).
type Systemctl<'a> = &'a mut dyn FnMut(&[&str]) -> Result<(), ServiceError>;

/// How `install` / `start` / `restart` reach launchd. Same shape as [`Systemctl`]:
/// production passes [`launchctl`]; tests pass a recording or panicking
/// closure, so no unit test can ever bootstrap a real LaunchAgent (GHA
/// `macos-latest` has a live `launchctl` under the runner user).
type Launchctl<'a> = &'a mut dyn FnMut(&[&str]) -> Result<(), ServiceError>;

/// How `status` reads the supervisor: the same injection shape as
/// [`Systemctl`] / [`Launchctl`], but the runner yields the captured stdout of
/// a successful call. Production passes [`systemctl_user_query`] /
/// [`launchctl_query`]; tests pass a closure answering with a fixture, so no
/// unit test can ever query a live supervisor.
type Query<'a> = &'a mut dyn FnMut(&[&str]) -> Result<String, ServiceError>;

/// Which OS supervisor `install` / `start` / `restart` / `status` talk to on
/// this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServiceBackend {
    /// Linux systemd `--user` unit (`systemctl --user`).
    Systemd,
    /// macOS LaunchAgent (`launchctl` in the `gui/<uid>` domain).
    Launchd,
}

impl ServiceBackend {
    /// The word `service status` prints on its `backend:` line.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Systemd => "systemd",
            Self::Launchd => "launchd",
        }
    }
}

/// The supervisor's view of the service, as `service status` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServiceState {
    /// The supervised `watch` is up (`ActiveState=active` / `state = running`).
    Running,
    /// Installed but not running: a loaded, inactive unit; or a plist on disk
    /// that launchd has not loaded, or has loaded and is not running.
    Stopped,
    /// systemd's `ActiveState=failed` — the unit gave up restarting.
    Failed,
    /// No unit at all (`LoadState=not-found`); no plist file and no loaded label.
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
    /// or `0` (systemd's "no main process").
    pub(crate) pid: Option<u32>,
    /// systemd's `ActiveEnterTimestamp`, passed through raw (no duration
    /// math). Always `None` on launchd: `launchctl print` has no start time.
    pub(crate) active_since: Option<String>,
}

#[derive(Debug)]
pub enum ServiceError {
    /// No supported backend: not macOS, and no systemd --user on this host
    /// (`/run/systemd/system` absent).
    NoSystemd,
    /// cwd is not inside a dreamd project (`AgentRoot::discover` failed).
    NoProjectRoot,
    /// macOS: the LaunchAgent plist already exists and `--force` was absent.
    PlistExists(PathBuf),
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
    /// `id -u` ran but its exit status or stdout was unusable (macOS uid lookup).
    Uid(String),
    /// A helper program (`systemctl`, `launchctl`, `id`) could not be spawned at all.
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
            Self::Uid(detail) => {
                write!(f, "could not determine the current uid via `id -u`: {detail}")
            }
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

/// Production backend pick: macOS is always launchd (never probed); anything
/// else is systemd when the user manager probe passes, otherwise `None`,
/// which the verbs map to [`ServiceError::NoSystemd`].
fn detect_backend() -> Option<ServiceBackend> {
    if cfg!(target_os = "macos") {
        Some(ServiceBackend::Launchd)
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
/// `<string>` element. `&` goes first so the entities introduced for the
/// other three are not themselves re-escaped. No systemd `%` doubling here:
/// launchd does no specifier expansion, so a literal `%` stays a literal `%`.
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
/// [`run_launchd_install_with`] with the uid from `id -u`. No backend →
/// [`ServiceError::NoSystemd`], nothing written.
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
/// dreamd.service` (Linux) or `launchctl kickstart -k gui/<uid>/<label>`
/// (macOS). No backend → [`ServiceError::NoSystemd`].
pub fn run_start() -> Result<(), ServiceError> {
    match detect_backend() {
        None => Err(ServiceError::NoSystemd),
        Some(ServiceBackend::Systemd) => run_start_with(true, &mut systemctl_user),
        Some(ServiceBackend::Launchd) => run_launchd_start_with(current_uid()?, &mut launchctl),
    }
}

/// `dreamd service restart` (AILAB-185): pick the backend, then `systemctl
/// --user restart dreamd.service` (Linux) or the same `launchctl kickstart -k
/// gui/<uid>/<label>` argv `start` issues (macOS — `-k` already kills the
/// running instance first, so it *is* the bounce). No backend →
/// [`ServiceError::NoSystemd`]. Bounce-only: the unit / plist is never
/// rewritten, so a binary that moved (an npx cache bump) still needs
/// `dreamd service install` first.
pub fn run_restart() -> Result<(), ServiceError> {
    match detect_backend() {
        None => Err(ServiceError::NoSystemd),
        Some(ServiceBackend::Systemd) => run_restart_with(true, &mut systemctl_user),
        Some(ServiceBackend::Launchd) => run_launchd_restart_with(current_uid()?, &mut launchctl),
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
    )?;
    print!("{}", render_report(&report, log_tail));
    Ok(())
}

/// Same as [`run_status`] with the backend pick, the uid resolver, and both
/// query runners injected. `resolve_uid` is only ever called on the launchd
/// arm: passing `current_uid()?` as a call argument would spawn `id` on Linux
/// too, and ahead of the no-backend refusal — the AILAB-169 shape this
/// deliberately avoids. Tests pass fixtures and panicking runners and never
/// spawn anything.
pub(crate) fn run_status_with(
    backend: Option<ServiceBackend>,
    home: &Path,
    resolve_uid: impl FnOnce() -> Result<u32, ServiceError>,
    systemctl: Query<'_>,
    launchctl: Query<'_>,
) -> Result<ServiceReport, ServiceError> {
    match backend {
        None => Err(ServiceError::NoSystemd),
        Some(ServiceBackend::Systemd) => query_systemd_status(systemctl),
        Some(ServiceBackend::Launchd) => query_launchd_status(home, resolve_uid, launchctl),
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
    // Linux — none is gated on the macOS target.

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
}
