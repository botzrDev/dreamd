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
    /// Suppress non-essential stdout (before/after version lines stay).
    pub quiet: bool,
    /// Best-effort cargo-install detection done by the caller (`DREAMD_BIN`
    /// set, or the running executable living under `.cargo/bin`). `Some`
    /// carries a short human-readable reason for the guidance line.
    pub cargo_install_hint: Option<String>,
}

/// `dreamd update` entry point. Order: before-version → (unless dry-run)
/// stop servers → remove socket → clear native cache → guidance → after-version.
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
            writeln!(
                out,
                "dry-run: then re-run `npx -y dreamd-mcp` to fetch the latest native binary"
            )?;
            if let Some(hint) = &req.cargo_install_hint {
                writeln!(
                    out,
                    "note: this dreamd binary looks cargo-installed ({hint}); clearing the cache does not replace it — rebuild with `cargo install --path crates/dreamd-cli`"
                )?;
            }
        }
        writeln!(out, "after: {VERSION_SHORT} (no changes)")?;
        return Ok(());
    }

    // Stop servers + remove the socket so a stuck binary/cache can be replaced.
    let report = stop(err);
    if !req.quiet {
        if report.matched {
            writeln!(out, "stopped running dreamd mcp/watch process(es)")?;
        } else if report.attempted {
            writeln!(out, "no running dreamd mcp/watch processes found")?;
        }
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
        writeln!(
            out,
            "next: run `npx -y dreamd-mcp` — the shim downloads and verifies the latest native binary"
        )?;
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
