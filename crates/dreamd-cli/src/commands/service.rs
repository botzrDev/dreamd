//! `dreamd service install` / `dreamd service start` — Linux systemd **user**
//! unit for the daemon (AILAB-190).
//!
//! `install` writes `~/.config/systemd/user/dreamd.service` and runs
//! `systemctl --user daemon-reload` + `enable --now`; `start` runs
//! `systemctl --user start dreamd.service`. The unit is `Type=simple`: systemd
//! itself supervises a **foreground** `dreamd watch` (which already handles
//! SIGTERM, WEG-268) and restarts it on failure. That choice shapes everything
//! else in this module:
//!
//! - **`detach_double_fork` is not this unit's call site — do not call it.**
//!   Under `Type=simple` systemd tracks the process it spawned; a double-fork
//!   would leave systemd watching the intermediate parent while the real
//!   daemon is reparented to init and escapes supervision entirely. The helper
//!   keeps its zero production call sites (ARCHITECTURE.md §8.1).
//! - **`WorkingDirectory=` is the [`AgentRoot`] project root discovered from
//!   `cwd` at install time.** `run_watch` does `AgentRoot::discover(cwd)` and
//!   exits 2 without a project root, so a unit with no working directory would
//!   never start. Reinstalling from another project overwrites the unit.
//! - **No `Environment=` line is ever emitted — in particular never an empty
//!   HOME environment line** (AILAB-584): an empty `HOME` makes every derived
//!   path (`~/.agent`, the cache dir, the npx dir) relative. systemd --user
//!   already provides `HOME` and `%h`.
//! - **Both verbs are one-shots and stay console-only** (AILAB-184): the
//!   tracing file layer truncates `~/.agent/dreamd.log`, so `wants_daemon_log`
//!   in `cli.rs` never grants it to `service`. The supervised `watch` still
//!   gets the file layer, and its stderr additionally lands in the user
//!   journal (JSON when stderr is not a TTY).
//!
//! Hosts without a reachable systemd --user manager (non-Linux, containers,
//! most CI runners) get [`ServiceError::NoSystemd`] and are pointed at
//! foreground `dreamd watch`. The probe is `/run/systemd/system` — systemd's
//! documented "booted with systemd" check — and never spawns `systemctl`.
//! The `systemctl` runner itself is injected ([`Systemctl`]), so unit tests
//! pass a recording closure and can never spawn it either.

use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use dreamd_core::io::write_atomic;
use dreamd_core::{AgentRoot, LayoutError};

/// Name of the user unit written by `install` and started by `start`.
pub(crate) const UNIT_NAME: &str = "dreamd.service";

/// How `install` / `start` reach systemd. Production passes [`systemctl_user`];
/// tests pass a recording closure, so no unit test can ever spawn `systemctl`
/// (this dev box has a live user manager; GHA runners mostly do not).
type Systemctl<'a> = &'a mut dyn FnMut(&[&str]) -> Result<(), ServiceError>;

#[derive(Debug)]
pub enum ServiceError {
    /// No systemd --user on this host (non-Linux, or `/run/systemd/system` absent).
    NoSystemd,
    /// cwd is not inside a dreamd project (`AgentRoot::discover` failed).
    NoProjectRoot,
    /// `std::env::current_exe()` / canonicalize failed.
    CurrentExe(std::io::Error),
    /// Creating the unit directory or writing the unit file failed.
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
    /// `systemctl` could not be spawned at all.
    Spawn(std::io::Error),
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
            Self::Spawn(e) => write!(f, "could not run systemctl: {e}"),
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

/// Path of the user unit under `home`: `<home>/.config/systemd/user/dreamd.service`.
pub(crate) fn unit_path(home: &Path) -> PathBuf {
    home.join(".config")
        .join("systemd")
        .join("user")
        .join(UNIT_NAME)
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

/// Render + write the unit file (creates parents). Returns the path written.
/// Never spawns anything — this is the unit-testable half of `install`.
pub(crate) fn write_user_unit(
    home: &Path,
    exe: &Path,
    project_root: &Path,
) -> Result<PathBuf, ServiceError> {
    let path = unit_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ServiceError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    write_atomic(&path, render_user_unit(exe, project_root).as_bytes()).map_err(|source| {
        ServiceError::Io {
            path: path.clone(),
            source,
        }
    })?;
    Ok(path)
}

/// Absolute, symlink-resolved path of this binary for `ExecStart=`.
fn current_exe_canonical() -> Result<PathBuf, ServiceError> {
    std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(ServiceError::CurrentExe)
}

/// Full install: probe → discover project root → current_exe → write → systemctl.
pub fn run_install(cwd: &Path, home: &Path) -> Result<(), ServiceError> {
    run_install_with(
        cwd,
        home,
        systemd_user_available(),
        current_exe_canonical,
        &mut systemctl_user,
    )
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

/// `dreamd service start`: probe, then `systemctl --user start dreamd.service`.
pub fn run_start() -> Result<(), ServiceError> {
    run_start_with(systemd_user_available(), &mut systemctl_user)
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

/// Run `systemctl --user <args>` with captured output. A spawn failure is
/// [`ServiceError::Spawn`]; a non-zero exit is [`ServiceError::Systemctl`]
/// carrying the subcommand words and systemd's stderr. This is the only
/// place in the module that spawns a process.
fn systemctl_user(args: &[&str]) -> Result<(), ServiceError> {
    let output = std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(ServiceError::Spawn)?;
    if output.status.success() {
        return Ok(());
    }
    Err(ServiceError::Systemctl {
        args: args.join(" "),
        status: output.status,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // No test here can spawn `systemctl`: every call into `run_*_with` passes
    // either a panicking runner (paths that must refuse first) or a recording
    // closure (paths that must reach systemd). This host may well have a live
    // user manager, and a test must never enable a real unit under the
    // developer's HOME.

    const FAKE_EXE: &str = "/opt/dreamd/bin/dreamd";

    fn fake_exe() -> Result<PathBuf, ServiceError> {
        Ok(PathBuf::from(FAKE_EXE))
    }

    fn never_systemctl(args: &[&str]) -> Result<(), ServiceError> {
        panic!("systemctl must not be reached, got {args:?}")
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
        let spawn = ServiceError::Spawn(std::io::Error::other("ENOENT"));
        assert_eq!(spawn.to_string(), "could not run systemctl: ENOENT");
    }
}
