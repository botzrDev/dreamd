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
            lifecycle_cleanup::SocketRemoval::RefusedLive => {
                // Still answering after the grace window, so this is not a
                // daemon mid-shutdown — the best-effort stop pass above did not
                // land: no `kill` on the box, the signal was denied, or (the
                // common case now) the daemon was deliberately spared as not
                // attributable to this invocation's $HOME / native cache.
                // Unlinking a live daemon's socket orphans it: it keeps serving
                // while every client resolves a path that is gone.
                //
                // The remedy is deliberately NOT `pkill -f 'dreamd watch'`.
                // This branch is reached precisely when scoping mattered, and
                // that recipe is the machine-global kill AILAB-584 removed —
                // the same commit's README calls it unsafe. Point at the pid
                // the stop pass already named on stderr, or at the daemon's own
                // terminal.
                writeln!(
                    err,
                    "dreamd: warning — a daemon is still live on {}; left the socket in place. \
                     Stop that daemon — `dreamd update` under the $HOME it serves, or \
                     `kill <pid>` for a pid named in the warnings above — then re-run to \
                     finish the update.",
                    socket.display()
                )?;
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
        //
        // `spared` is what keeps this line honest on the AILAB-584 path: a
        // process that matched but was not attributable to this invocation is
        // still running, and reporting it as "nothing was running" contradicted
        // the stderr warning printed seconds earlier in the same output. It is
        // independent of `matched` — one pass can stop this home's `watch` and
        // spare another home's `mcp`.
        //
        // The line points at those stderr warnings rather than restating the
        // reason, because `spared` covers both causes (not attributable, or the
        // signal did not land) and each spared pid carries its own.
        //
        // The mixed line is deliberately the matched line plus a clause, not a
        // rewrite: a mixed sweep is the *ordinary* result on a shared box (any
        // other home's `dreamd mcp` is a spared candidate), so every assertion
        // that pins the matched wording must keep holding when one shows up.
        let step_one = match (report.matched, report.spared, report.attempted) {
            (true, true, _) => {
                "stopped local `dreamd mcp` / `dreamd watch` process(es); others were left \
                 running — see the warnings above"
            }
            (true, false, _) => "stopped local `dreamd mcp` / `dreamd watch` process(es)",
            (false, true, _) => {
                "left `dreamd mcp` / `dreamd watch` process(es) running — see the warnings above \
                 and stop them yourself if they are yours"
            }
            (false, false, true) => "no local `dreamd mcp` / `dreamd watch` processes were running",
            (false, false, false) => {
                "stop local `dreamd mcp` / `dreamd watch` yourself — automatic stop is unix-only in v0.1"
            }
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
