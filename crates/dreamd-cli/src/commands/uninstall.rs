//! `dreamd uninstall` — one safe path to leave clean (AILAB-226).
//!
//! Stops local `dreamd mcp` / `dreamd watch` processes, removes the daemon
//! socket, unregisters the cwd project from the daemon registry (reusing
//! `init::uninstall_project` — one implementation of registry removal), and
//! clears the shim caches. Never deletes per-project `.agent/` stores, and
//! there is no `dreamd reset --all` — the destructive store wipe stays a
//! manual, documented step (docs/troubleshooting.md "Full fresh store").
//!
//! Idempotent: a second run with nothing left to remove is `Ok` with benign
//! messaging.

use std::io::Write;
use std::path::{Path, PathBuf};

use super::confirm::{self, ConfirmError};
use super::init;
use super::lifecycle_cleanup::{self, PlannedStop, StopScope, Stopper};

/// Prompt written to stderr after the plan, on a tty, before the answer.
pub const PROMPT: &str = "Proceed with dreamd uninstall? [y/N] ";

/// Failure modes for `dreamd uninstall`. Everything user-fixable is benign
/// messaging, not an error; only real I/O failures (permission denied on a
/// cache tree, unreadable registry) surface here.
#[derive(Debug)]
pub enum UninstallError {
    /// A cache tree or the registry could not be read or removed; the message
    /// names the path.
    Io(std::io::Error),
}

impl From<std::io::Error> for UninstallError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl std::fmt::Display for UninstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for UninstallError {}

/// Everything [`run`] needs, injected by the caller. `cli.rs` resolves the
/// real home-derived paths (socket via the shared `$DREAMD_SOCK` /
/// `DaemonHome::socket_path()` resolver); tests pass temp dirs so the real
/// `~/.agent`, `~/.cache`, and npx cache are never touched.
pub struct UninstallRequest<'a> {
    /// Directory the registry step resolves the project root from.
    pub cwd: &'a Path,
    /// Daemon home (normally `~/.agent`).
    pub daemon_home: &'a Path,
    /// Resolved daemon socket. `None` — no home directory to resolve against,
    /// or a non-Unix target, where the daemon listens on loopback TCP.
    pub socket: Option<&'a Path>,
    /// Native binary cache tree (normally `~/.cache/dreamd-mcp`). `None` — no
    /// home directory.
    pub cache_dir: Option<&'a Path>,
    /// npx cache root (the directory holding per-package cache entries).
    /// `None` — no home directory.
    pub npx_dir: Option<&'a Path>,
    /// Skip all cache clearing.
    pub keep_caches: bool,
    /// Wipe the entire npx cache instead of only dreamd-mcp entries. Loud.
    pub all_npx: bool,
    /// Suppress non-essential stdout.
    pub quiet: bool,
}

/// `dreamd uninstall` entry point. Step order: stop servers →
/// remove socket → registry unregister (benign skip outside a project) →
/// cache clears (unless `keep_caches`) → leftover guidance.
pub fn run(
    req: &UninstallRequest<'_>,
    stop: Stopper<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), UninstallError> {
    // 1. Stop local servers (best-effort; no match is success).
    //
    // `spared` — at least one matching process left running, normally because
    // it was not attributable to this invocation's $HOME / native cache
    // (AILAB-584) — gets its own line, pointing at the stderr warnings that
    // name each pid and its reason, and suppresses "no running ... found".
    // See `lifecycle_cleanup::StopReport` for the flags.
    let report = stop(err);
    if !req.quiet {
        if report.matched {
            writeln!(out, "stopped running dreamd mcp/watch process(es)")?;
        }
        if report.spared {
            writeln!(
                out,
                "left dreamd mcp/watch process(es) running — see the warnings above and stop them yourself if they are yours"
            )?;
        } else if !report.matched && report.attempted {
            writeln!(out, "no running dreamd mcp/watch processes found")?;
        }
    }

    // 2. Remove the daemon socket.
    match req.socket {
        Some(socket) => match lifecycle_cleanup::remove_socket(socket) {
            lifecycle_cleanup::SocketRemoval::Removed => {
                if !req.quiet {
                    writeln!(out, "removed daemon socket {}", socket.display())?;
                }
            }
            lifecycle_cleanup::SocketRemoval::NotFound => {
                if !req.quiet {
                    writeln!(out, "no daemon socket at {}", socket.display())?;
                }
            }
            lifecycle_cleanup::SocketRemoval::RefusedLive => {
                // Still answering after the grace window: the stop pass above
                // did not land or spared the daemon (see
                // `lifecycle_cleanup::remove_socket`). The remedy names a
                // scoped stop, never a machine-global `pkill -f 'dreamd watch'`.
                writeln!(
                    err,
                    "dreamd: warning — a daemon is still live on {}; left the socket in place. \
                     Stop that daemon — `dreamd uninstall` under the $HOME it serves, or \
                     `kill <pid>` for a pid named in the warnings above — then re-run to \
                     finish cleanup.",
                    socket.display()
                )?;
            }
            lifecycle_cleanup::SocketRemoval::Failed(e) => {
                // Best-effort: warn with the path and continue — an
                // unremovable socket must not abort the uninstall.
                writeln!(
                    err,
                    "dreamd: warning — could not remove daemon socket {} ({e}); remove it manually",
                    socket.display()
                )?;
            }
        },
        None => {
            if !req.quiet {
                writeln!(
                    out,
                    "daemon socket unresolved (no home directory) — skipping"
                )?;
            }
        }
    }

    // 3. Registry unregister — reuse the `init --uninstall-project`
    // implementation. A cwd outside any project root is a benign skip, not a
    // failure: uninstall must succeed from anywhere.
    if init::find_project_root(req.cwd).is_some() {
        let daemon_home = dreamd_core::DaemonHome::new(req.daemon_home);
        match init::uninstall_project(req.cwd, &daemon_home, req.quiet, out, err) {
            Ok(()) => {}
            // Unreachable — the root was just checked — but treat as the skip.
            Err(init::InitError::NoProjectRoot) => {}
            Err(init::InitError::Io(e)) => return Err(UninstallError::Io(e)),
        }
    } else if !req.quiet {
        writeln!(
            out,
            "no project root here — skipping registry unregister (run from a project to unregister it)"
        )?;
    }

    // 4. Cache clears.
    if req.keep_caches {
        if !req.quiet {
            writeln!(
                out,
                "--keep-caches: leaving the native binary cache and npx cache in place"
            )?;
        }
    } else {
        clear_caches(req, out, err)?;
    }

    // 5. Leftover guidance — what an uninstall deliberately does not touch.
    if !req.quiet {
        writeln!(out)?;
        writeln!(out, "left in place:")?;
        writeln!(
            out,
            "  ~/.agent/registry.toml and ~/.agent/dreamd.log (shared daemon home)"
        )?;
        writeln!(out, "  per-project .agent/ stores (your memory data)")?;
        writeln!(
            out,
            "note: there is no `dreamd reset --all` — for a full fresh store, delete a project's .agent/ and re-run `dreamd init` (see docs/troubleshooting.md, \"Full fresh store\")."
        )?;
    }

    Ok(())
}

/// Step 4: clear the native binary cache tree, then the npx cache — scoped to
/// dreamd-mcp entries by default, the whole cache only behind `--all-npx`
/// (with a loud stderr warning *before* deleting).
fn clear_caches(
    req: &UninstallRequest<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), UninstallError> {
    match req.cache_dir {
        Some(cache_dir) => {
            let removed = lifecycle_cleanup::clear_dir_tree(cache_dir)
                .map_err(|e| UninstallError::Io(lifecycle_cleanup::io_with_path(e, cache_dir)))?;
            if !req.quiet {
                if removed {
                    writeln!(out, "removed native binary cache {}", cache_dir.display())?;
                } else {
                    writeln!(out, "no native binary cache at {}", cache_dir.display())?;
                }
            }
        }
        None => {
            if !req.quiet {
                writeln!(
                    out,
                    "native binary cache unresolved (no home directory) — skipping"
                )?;
            }
        }
    }

    match req.npx_dir {
        Some(npx_dir) => {
            if req.all_npx {
                // Loud warning before deleting: this removes every npx-cached
                // package on the machine, not just dreamd-mcp.
                writeln!(
                    err,
                    "dreamd: warning — --all-npx wipes the ENTIRE npx cache at {} (every cached package, not just dreamd-mcp)",
                    npx_dir.display()
                )?;
                let removed = lifecycle_cleanup::clear_dir_tree(npx_dir)
                    .map_err(|e| UninstallError::Io(lifecycle_cleanup::io_with_path(e, npx_dir)))?;
                if !req.quiet {
                    if removed {
                        writeln!(out, "removed entire npx cache {}", npx_dir.display())?;
                    } else {
                        writeln!(out, "no npx cache at {}", npx_dir.display())?;
                    }
                }
            } else {
                let removed = lifecycle_cleanup::clear_scoped_npx(npx_dir)
                    .map_err(|e| UninstallError::Io(lifecycle_cleanup::io_with_path(e, npx_dir)))?;
                if !req.quiet {
                    if removed.is_empty() {
                        writeln!(
                            out,
                            "no dreamd-mcp entries in npx cache {}",
                            npx_dir.display()
                        )?;
                    } else {
                        writeln!(
                            out,
                            "removed {} dreamd-mcp npx cache entry(ies) under {}",
                            removed.len(),
                            npx_dir.display()
                        )?;
                    }
                }
            }
        }
        None => {
            if !req.quiet {
                writeln!(out, "npx cache unresolved (no home directory) — skipping")?;
            }
        }
    }

    Ok(())
}

/// Why [`run_gated`] did not finish. [`GatedError::NotATty`] and
/// [`GatedError::Declined`] are exit 2. [`GatedError::Reported`] is exit 1
/// and the line is already on `err`.
#[derive(Debug)]
pub enum GatedError {
    NotATty,
    Declined,
    Reported,
}

/// What the plan says it will delete.
#[derive(Debug)]
pub enum DeletePlan {
    /// `--keep-caches`, which wins over `--all-npx`.
    KeepCaches,
    /// Existing paths [`run`] would delete, native cache first.
    Paths(Vec<PathBuf>),
}

/// The preview [`render_plan`] prints. Built with reads only.
#[derive(Debug)]
pub struct UninstallPlan {
    pub stops: Vec<PlannedStop>,
    pub registry_root: Option<String>,
    pub deletes: DeletePlan,
}

/// Read the process table, the registry, and the cache directories. Does not
/// signal, flock, or delete.
pub fn build_plan(
    req: &UninstallRequest<'_>,
    scope: &StopScope<'_>,
) -> Result<UninstallPlan, std::io::Error> {
    plan_with_stops(req, &lifecycle_cleanup::attributable_targets(scope))
}

fn plan_with_stops(
    req: &UninstallRequest<'_>,
    stops: &[PlannedStop],
) -> Result<UninstallPlan, std::io::Error> {
    Ok(UninstallPlan {
        stops: stops.to_vec(),
        registry_root: registry_root(req)?,
        deletes: delete_plan(req)?,
    })
}

/// The plan text: three sections, no blank lines, trailing newline.
pub fn render_plan(plan: &UninstallPlan) -> String {
    let mut out = String::from("will stop:\n");
    if plan.stops.is_empty() {
        out.push_str("  (none)\n");
    } else {
        for stop in &plan.stops {
            out.push_str(&format!("  {} pid {}\n", stop.label, stop.pid));
        }
    }
    out.push_str("will remove registry block:\n");
    if let Some(root) = &plan.registry_root {
        out.push_str("[[projects]]\n");
        out.push_str(&format!("root = \"{root}\"\n"));
    } else {
        out.push_str("  (none)\n");
    }
    out.push_str("will delete:\n");
    match &plan.deletes {
        DeletePlan::KeepCaches => out.push_str("  (none) --keep-caches\n"),
        DeletePlan::Paths(paths) if paths.is_empty() => out.push_str("  (none)\n"),
        DeletePlan::Paths(paths) => {
            for path in paths {
                out.push_str(&format!("  {}\n", path.display()));
            }
        }
    }
    out
}

/// How [`run_gated`] gets its answer: `--yes`, whether stdin is a terminal,
/// and the line reader. One argument, so [`run_gated`] stays within clippy's
/// argument limit.
pub struct Confirmation<'a> {
    /// `--yes`: no prompt, and `read_line` is not called.
    pub yes: bool,
    /// Whether stdin is a terminal.
    pub is_tty: bool,
    /// Reads the answer after the plan and [`PROMPT`].
    pub read_line: &'a mut dyn FnMut() -> Result<String, ConfirmError>,
}

/// Confirm, then [`run`].
///
/// `stops` is the process list from [`lifecycle_cleanup::attributable_targets`]
/// for this invocation. It is passed in so the stop closure can borrow the
/// same scope after the list is already owned.
///
/// `is_tty` is false when stdin is not a terminal: the refusal is one line,
/// stdout stays empty, and `read_line` is not called. On a tty the plan is
/// written first (even when `quiet`), then [`PROMPT`], then `read_line`.
/// `--yes` does not call `read_line`. The plan is written for `--yes` only
/// when `quiet` is false. After `y`, `Y`, or `--yes`, [`run`] is the body.
#[allow(clippy::too_many_arguments)] // CLI entry: request, stops, flags, prompt, stopper, out, err
pub fn run_gated(
    req: &UninstallRequest<'_>,
    stops: &[PlannedStop],
    confirmation: Confirmation<'_>,
    stop: Stopper<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), GatedError> {
    let Confirmation {
        yes,
        is_tty,
        read_line,
    } = confirmation;
    let plan = match plan_with_stops(req, stops) {
        Ok(plan) => plan,
        Err(error) => return Err(report(err, &error)),
    };
    let rendered = render_plan(&plan);

    if !yes {
        if !is_tty {
            return Err(report(err, &ConfirmError::NotATty).not_tty());
        }
        if let Err(error) = write!(out, "{rendered}") {
            return Err(report(err, &error));
        }
        if let Err(error) = write!(err, "{PROMPT}") {
            return Err(report(err, &error));
        }
        if let Err(error) = err.flush() {
            return Err(report(err, &error));
        }
        let line = match read_line() {
            Ok(line) => line,
            Err(error) => return Err(report(err, &error)),
        };
        let mut once = Some(line);
        if let Err(error) = confirm::confirm_destructive(false, PROMPT, &mut |_| Ok(once.take())) {
            return Err(match error {
                ConfirmError::NotATty => report(err, &ConfirmError::NotATty).not_tty(),
                ConfirmError::Declined => report(err, &ConfirmError::Declined).declined(),
                ConfirmError::Io(source) => report(err, &ConfirmError::Io(source)),
            });
        }
    } else if !req.quiet {
        if let Err(error) = write!(out, "{rendered}") {
            return Err(report(err, &error));
        }
    }

    match run(req, stop, out, err) {
        Ok(()) => Ok(()),
        Err(UninstallError::Io(error)) => Err(report(err, &error)),
    }
}

fn report(err: &mut dyn Write, error: &impl std::fmt::Display) -> GatedError {
    let _ = writeln!(err, "dreamd: error — {error}");
    GatedError::Reported
}

impl GatedError {
    fn not_tty(self) -> Self {
        match self {
            Self::Reported => Self::NotATty,
            other => other,
        }
    }

    fn declined(self) -> Self {
        match self {
            Self::Reported => Self::Declined,
            other => other,
        }
    }
}

fn registry_root(req: &UninstallRequest<'_>) -> Result<Option<String>, std::io::Error> {
    let Some(root) = init::find_project_root(req.cwd) else {
        return Ok(None);
    };
    let registry = dreamd_core::DaemonHome::new(req.daemon_home).registry_toml();
    Ok(dreamd_core::registry::resolve_project(&registry, &root)?.map(|entry| entry.root))
}

fn delete_plan(req: &UninstallRequest<'_>) -> Result<DeletePlan, std::io::Error> {
    if req.keep_caches {
        return Ok(DeletePlan::KeepCaches);
    }
    let mut paths = Vec::new();
    if let Some(cache) = req.cache_dir {
        if cache.is_dir() {
            paths.push(cache.to_path_buf());
        }
    }
    if let Some(npx) = req.npx_dir {
        if req.all_npx {
            if npx.exists() {
                paths.push(npx.to_path_buf());
            }
        } else {
            paths.extend(lifecycle_cleanup::dreamd_mcp_npx_entries(npx)?);
        }
    }
    Ok(DeletePlan::Paths(paths))
}
