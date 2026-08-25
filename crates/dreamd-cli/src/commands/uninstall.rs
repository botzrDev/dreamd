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
use std::path::Path;

use super::init;
use super::lifecycle_cleanup::{self, Stopper};

/// Failure modes for `dreamd uninstall`. Everything user-fixable is benign
/// messaging, not an error; only real I/O failures (permission denied on a
/// cache tree, unreadable registry) surface here.
#[derive(Debug)]
pub enum UninstallError {
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
    /// Resolved daemon socket. `None` — no home directory to resolve against.
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

/// `dreamd uninstall` entry point. Step order per spec: stop servers →
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
    // name each pid and its reason. Without it this printed "no running dreamd
    // mcp/watch processes found" directly above the stderr warning naming the
    // pid that is still running, which is the one scenario the scoping fix
    // exists for. `matched` and `spared` are independent: one pass can stop
    // ours and spare another's.
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
                // Still answering after the grace window, so this is not a
                // daemon mid-shutdown — the best-effort stop pass above did not
                // land: no `kill` on the box, the signal was denied, or (the
                // common case now) the daemon was deliberately spared as not
                // attributable to this invocation's $HOME / native cache.
                // Unlinking a live daemon's socket orphans it: it keeps serving
                // while every client resolves a path that is gone.
                //
                // The remedy is deliberately NOT `pkill -f 'dreamd watch'`:
                // this branch is reached precisely when scoping mattered, and
                // that recipe is the machine-global kill AILAB-584 removed.
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
        match init::uninstall_project(req.cwd, req.daemon_home, req.quiet, out, err) {
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

    // 5. Leftover guidance — what an uninstall deliberately does NOT touch.
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
                // Loud warning BEFORE deleting: this removes every npx-cached
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
