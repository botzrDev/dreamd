//! `dreamd update` — one safe path to land on `latest` (AILAB-226).
//!
//! The Rust side never downloads anything — download + sha verification live
//! only in the Node shim (`packages/dreamd-mcp`). This command stops local
//! servers, removes the daemon socket, clears the *native binary* cache so a
//! stuck binary can be replaced, and points the user at `npx -y dreamd-mcp`.
//! The npx cache itself is deliberately untouched: floating npx re-resolves
//! the package side on its own.
//!
//! Before/after version lines use `commands::version::VERSION_SHORT` — the
//! running binary's version cannot change in-process, so "after" reports the
//! same binary plus what was cleared.

use std::io::Write;
use std::path::Path;

use super::lifecycle_cleanup::{self, Stopper};
use super::version::VERSION_SHORT;

/// Failure modes for `dreamd update`. Only real I/O failures (e.g. permission
/// denied on the cache tree) are errors; everything else is guidance.
#[derive(Debug)]
pub enum UpdateError {
    Io(std::io::Error),
}

impl From<std::io::Error> for UpdateError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for UpdateError {}

/// Everything [`run`] needs, injected by the caller. `cli.rs` resolves the
/// real home-derived paths (socket via the shared `$DREAMD_SOCK` /
/// `DaemonHome::socket_path()` resolver); tests pass temp dirs.
pub struct UpdateRequest<'a> {
    /// Resolved daemon socket. `None` — no home directory to resolve against.
    pub socket: Option<&'a Path>,
    /// Native binary cache tree (normally `~/.cache/dreamd-mcp`). `None` — no
    /// home directory.
    pub cache_dir: Option<&'a Path>,
    /// Print the plan without mutating anything.
    pub dry_run: bool,
    /// Suppress non-essential stdout (before/after version lines and the
    /// compressed restart guidance stay).
    pub quiet: bool,
    /// `--restart`: the explicit form of the stop step. Update always stops
    /// local servers (AILAB-226); this only makes the stop loud in the output.
    pub restart: bool,
    /// Best-effort cargo-install detection done by the caller (`DREAMD_BIN`
    /// set, or the running executable living under `.cargo/bin`). `Some`
    /// carries a short human-readable reason for the guidance line.
    pub cargo_install_hint: Option<String>,
}

/// Step 2 of the restart contract: the harness holds the old binary open, so
/// clearing the cache alone is not enough.
const RELOAD_STEP: &str =
    "reload your MCP harness (Claude Code, Cursor, …) so it drops the old binary";
/// Step 3: floating npx only — a hard version pin never picks up `latest`.
const RERUN_STEP: &str =
    "re-run `npx -y dreamd-mcp` — the floating spawn re-resolves `latest` and fetches the new binary";
/// The whole contract on one line, for `--quiet`.
const COMPRESSED_NEXT: &str =
    "next: reload your MCP harness, then run `npx -y dreamd-mcp` (floating)";

/// Print the labeled restart contract. `step_one` states what happened (or
/// would happen) to local `dreamd mcp` / `dreamd watch` processes; steps 2 and
/// 3 are fixed.
fn write_contract(out: &mut dyn Write, header: &str, step_one: &str) -> std::io::Result<()> {
    writeln!(out, "{header}")?;
    writeln!(out, "  1. {step_one}")?;
    writeln!(out, "  2. {RELOAD_STEP}")?;
    writeln!(out, "  3. {RERUN_STEP}")
}

/// `dreamd update` entry point. Order: before-version → (unless dry-run)
/// stop servers → remove socket → clear native cache → restart contract →
/// after-version.
pub fn run(
    req: &UpdateRequest<'_>,
    stop: Stopper<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), UpdateError> {
    // Before line — printed even under --quiet: the version IS the output.
    writeln!(out, "current: {VERSION_SHORT}")?;

    if req.dry_run {
        if !req.quiet {
            match req.cache_dir {
                Some(cache_dir) => writeln!(
                    out,
                    "dry-run: would stop dreamd mcp/watch processes, remove the daemon socket, and clear {}",
                    cache_dir.display()
                )?,
                None => writeln!(
                    out,
                    "dry-run: would stop dreamd mcp/watch processes and remove the daemon socket (cache location unresolved — no home directory)"
                )?,
            }
            if let Some(hint) = &req.cargo_install_hint {
                writeln!(
                    out,
                    "note: this dreamd binary looks cargo-installed ({hint}); clearing the cache does not replace it — rebuild with `cargo install --path crates/dreamd-cli`"
                )?;
            }
            if req.restart {
                writeln!(
                    out,
                    "dry-run: --restart requested — the stop below would run; nothing is stopped now"
                )?;
            }
            write_contract(
                out,
                "restart contract (dry-run — nothing changes):",
                "would stop local `dreamd mcp` / `dreamd watch` processes if running",
            )?;
        } else {
            writeln!(out, "{COMPRESSED_NEXT}")?;
        }
        writeln!(out, "after: {VERSION_SHORT} (no changes)")?;
        return Ok(());
    }

    // Stop servers + remove the socket so a stuck binary/cache can be replaced.
    // The stop always runs (AILAB-226); `--restart` only announces it — after
    // the fact, and only when a stop was actually attempted, so the louder
    // line can never contradict step 1 of the contract below.
    let report = stop(err);
    if req.restart && !req.quiet && report.attempted {
        writeln!(
            out,
            "restart: --restart — ran the explicit stop pass for local dreamd mcp/watch"
        )?;
    }

    if let Some(socket) = req.socket {
        match lifecycle_cleanup::remove_socket(socket) {
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
            lifecycle_cleanup::SocketRemoval::Failed(e) => {
                // Best-effort: warn with the path and continue — an
                // unremovable socket must not abort the update.
                writeln!(
                    err,
                    "dreamd: warning — could not remove daemon socket {} ({e}); remove it manually",
                    socket.display()
                )?;
            }
        }
    }

    // Clear the native binary cache. The npx cache is NOT touched on update.
    let mut cache_cleared = false;
    match req.cache_dir {
        Some(cache_dir) => {
            cache_cleared = lifecycle_cleanup::clear_dir_tree(cache_dir)
                .map_err(|e| UpdateError::Io(lifecycle_cleanup::io_with_path(e, cache_dir)))?;
            if !req.quiet {
                if cache_cleared {
                    writeln!(out, "cleared native binary cache {}", cache_dir.display())?;
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

    if !req.quiet {
        if let Some(hint) = &req.cargo_install_hint {
            writeln!(
                out,
                "note: this dreamd binary looks cargo-installed ({hint}); clearing the cache does not replace it — rebuild with `cargo install --path crates/dreamd-cli`"
            )?;
        }
        // Step 1 reports the stop that already ran above. An unattempted stop
        // (non-unix) must hand the job back to the user, never claim success.
        let step_one = if report.matched {
            "stopped local `dreamd mcp` / `dreamd watch` process(es)"
        } else if report.attempted {
            "no local `dreamd mcp` / `dreamd watch` processes were running"
        } else {
            "stop local `dreamd mcp` / `dreamd watch` yourself — automatic stop is unix-only in v0.1"
        };
        write_contract(out, "restart contract:", step_one)?;
    } else {
        writeln!(out, "{COMPRESSED_NEXT}")?;
    }

    // After line — the running binary cannot change in-process.
    writeln!(
        out,
        "after: {VERSION_SHORT} (running binary unchanged until reinstall; native cache {})",
        if cache_cleared {
            "cleared"
        } else {
            "already clean"
        }
    )?;

    Ok(())
}
