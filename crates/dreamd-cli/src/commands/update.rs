//! `dreamd update` — one safe path to land on `latest` (AILAB-226, DRM-152).
//!
//! The Rust side never downloads anything — download + sha verification live
//! only in the Node shim (`packages/dreamd-mcp`). This command:
//!
//! - stops this `$HOME`'s local `dreamd mcp` / `dreamd watch` servers and
//!   removes the daemon socket;
//! - keeps this version's cached binary (`~/.cache/dreamd-mcp/<VERSION>/`) and
//!   removes every older and newer one, which the shim downloads again if a
//!   spawn needs it;
//! - never contacts the network, so it cannot say whether a newer release
//!   exists. It points the user at floating `npx -y dreamd-mcp`.
//!
//! The npx cache itself is deliberately untouched. For a bare `dreamd-mcp`,
//! npx revalidates the package's registry metadata on every run
//! (`libnpmexec` resolves it with `preferOnline`) and installs a newer
//! version into its cache when one is published. A `dreamd-mcp` installed
//! globally or in the project's `node_modules` runs instead, and a pinned
//! `dreamd-mcp@<version>` stays on that version.
//!
//! Before/after version lines use `commands::version::VERSION_SHORT` — the
//! running binary's version cannot change in-process, so "after" reports the
//! same binary.

use std::io::Write;
use std::path::Path;

use super::lifecycle_cleanup::{self, Stopper};
use super::service::InstalledService;
use super::version::VERSION_SHORT;

/// The cache directory this binary's own version uses: the shim keys
/// `~/.cache/dreamd-mcp/<VERSION>/` by its package version, which
/// `version-consistency.test.js` keeps equal to the crate's.
const KEEP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Failure modes for `dreamd update`. Only real I/O failures (e.g. permission
/// denied on the cache tree) are errors; everything else is guidance.
#[derive(Debug)]
pub enum UpdateError {
    /// A cached version could not be removed; the message names the path.
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
    /// Resolved daemon socket. `None` — no home directory to resolve against,
    /// or a non-Unix target, where the daemon listens on loopback TCP.
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
    /// The installed `dreamd service`, read by the caller with
    /// [`super::service::read_installed_service`]. `None`: none installed,
    /// or no home directory. Its cached binary is kept, and a binary it names
    /// that is gone is reported (DRM-164).
    pub service: Option<&'a InstalledService>,
}

impl UpdateRequest<'_> {
    /// How the user runs dreamd, for the commands `update` prints: a
    /// cargo-installed binary is `dreamd`, anything else the floating shim.
    fn command_prefix(&self) -> &'static str {
        if self.cargo_install_hint.is_some() {
            "dreamd"
        } else {
            "npx -y dreamd-mcp"
        }
    }

    /// The cached version directory the installed service's binary lives
    /// in, when it lives in the shim cache: the first path component below
    /// `cache_dir`. `None` for a service outside the cache, or no service.
    fn service_cached_version(&self) -> Option<String> {
        let service = self.service?;
        let rel = service.exe.strip_prefix(self.cache_dir?).ok()?;
        match rel.components().next()? {
            std::path::Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        }
    }
}

/// Step 2 of the restart contract: the harness spawned the old binary, and
/// only a reload makes it spawn the server again.
const RELOAD_STEP: &str =
    "reload your MCP harness (Claude Code, Cursor, …) so it drops the old binary";
/// Step 3: floating npx only. `update` cannot tell whether a newer release
/// exists, so it promises no new binary (DRM-152).
const RERUN_STEP: &str =
    "re-run `npx -y dreamd-mcp`: floating npx runs the newest published dreamd-mcp, and the shim downloads a binary only for a version it has not cached";
/// Step 4: a hard version pin never picks up `latest`.
const PIN_STEP: &str =
    "an MCP config that pins `dreamd-mcp@<version>` stays on that version. Use `npx -y dreamd-mcp` to follow new releases";
/// The whole contract on one line, for `--quiet`.
const COMPRESSED_NEXT: &str =
    "next: reload your MCP harness, then run `npx -y dreamd-mcp` (floating)";

/// Print the labeled restart contract. `step_one` states what happened (or
/// would happen) to local `dreamd mcp` / `dreamd watch` processes; steps 2
/// to 4 are fixed.
fn write_contract(out: &mut dyn Write, header: &str, step_one: &str) -> std::io::Result<()> {
    writeln!(out, "{header}")?;
    writeln!(out, "  1. {step_one}")?;
    writeln!(out, "  2. {RELOAD_STEP}")?;
    writeln!(out, "  3. {RERUN_STEP}")?;
    writeln!(out, "  4. {PIN_STEP}")
}

/// Remove every entry directly under `cache_dir` whose file name is not in
/// `keep`, and return the removed names, sorted (DRM-152).
///
/// A missing `cache_dir` is an empty one. No symlink is followed: an entry
/// that is a symlink is removed as a link. A name that is not UTF-8 is
/// reported lossily.
fn remove_other_versions(cache_dir: &Path, keep: &[&str]) -> std::io::Result<Vec<String>> {
    let entries = match std::fs::read_dir(cache_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut removed = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if keep.iter().any(|k| name == *k) {
            continue;
        }
        let path = entry.path();
        // `file_type` does not follow a symlink, so a link is removed as one.
        let removal = if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        removal.map_err(|e| lifecycle_cleanup::io_with_path(e, &path))?;
        removed.push(name.to_string_lossy().into_owned());
    }
    removed.sort();
    Ok(removed)
}

/// Report the installed `dreamd service` (DRM-164), after the cache step (or
/// in its place under `dry_run`). Prints nothing when no service is installed
/// or it already runs this version's binary.
///
/// - Its binary is in another cached version: that version was kept (or would
///   be), and the line names the commands that move the service to this one.
/// - Its binary is gone: the service cannot start, and the line names the
///   same commands. `update` did not remove it, because it keeps the version
///   a service names; something else did, or an older `update` did.
fn write_service_report(
    req: &UpdateRequest<'_>,
    dry_run: bool,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let Some(service) = req.service else {
        return Ok(());
    };
    if let Some(report) = service.missing_binary_report(req.command_prefix()) {
        return writeln!(out, "service: {report}");
    }
    let fix = service.fix_hint(req.command_prefix());
    match req.service_cached_version() {
        Some(version) if version != KEEP_VERSION => {
            let verb = if dry_run { "would keep" } else { "kept" };
            writeln!(
                out,
                "service: {verb} the cached dreamd {version}, because the dreamd service ({}) runs it. To move the service to {KEEP_VERSION}, {fix}",
                service.file.display()
            )
        }
        _ => Ok(()),
    }
}

/// `dreamd update` entry point. Order: before-version → (unless dry-run)
/// stop servers → remove socket → remove other cached versions → restart
/// contract → after-version.
pub fn run(
    req: &UpdateRequest<'_>,
    stop: Stopper<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), UpdateError> {
    // Before line — printed even under --quiet: the version is the output.
    writeln!(out, "current: {VERSION_SHORT}")?;

    if req.dry_run {
        if !req.quiet {
            match req.cache_dir {
                Some(cache_dir) => writeln!(
                    out,
                    "dry-run: would stop dreamd mcp/watch processes, remove the daemon socket, and remove cached versions other than {KEEP_VERSION} from {}",
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
                    "note: this dreamd binary looks cargo-installed ({hint}); `update` does not replace it — rebuild with `cargo install --path crates/dreamd-cli`"
                )?;
            }
            write_service_report(req, true, out)?;
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
                // Still answering after the grace window: the stop pass above
                // did not land or spared the daemon (see
                // `lifecycle_cleanup::remove_socket`). The remedy names a
                // scoped stop, never a machine-global `pkill -f 'dreamd watch'`
                // (`packages/dreamd-mcp/README.md` warns against it).
                writeln!(
                    err,
                    "dreamd: warning — a daemon is still live on {}; left the socket in place. \
                     Stop that daemon — `dreamd update` under the $HOME it serves, or \
                     `kill <pid>` for a pid named in the warnings above — then re-run to \
                     finish the update.",
                    socket.display()
                )?;
            }
            lifecycle_cleanup::SocketRemoval::NotASocket => {
                // DRM-99: a mistyped `DREAMD_SOCK` must not cost the user a file.
                writeln!(
                    err,
                    "dreamd: warning — {} is not a unix socket; left it in place. \
                     Check `DREAMD_SOCK`.",
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

    // Keep this version's cached binary and remove the others (DRM-152). The
    // npx cache is not touched on update.
    match req.cache_dir {
        Some(cache_dir) => {
            // DRM-164: the version an installed service runs stays too, so
            // the service still starts after the update.
            let service_version = req.service_cached_version();
            let mut keep = vec![KEEP_VERSION];
            keep.extend(service_version.as_deref());
            let removed = remove_other_versions(cache_dir, &keep).map_err(UpdateError::Io)?;
            if !req.quiet {
                let kept = cache_dir.join(KEEP_VERSION);
                if kept.is_dir() {
                    writeln!(
                        out,
                        "kept the cached dreamd {KEEP_VERSION} in {}",
                        kept.display()
                    )?;
                }
                if removed.is_empty() {
                    writeln!(out, "no other cached versions in {}", cache_dir.display())?;
                } else {
                    writeln!(
                        out,
                        "removed {} other cached version(s): {}",
                        removed.len(),
                        removed.join(", ")
                    )?;
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

    // Printed even under --quiet: a service left on an old or missing
    // binary is the one thing here the user has to act on (DRM-164).
    write_service_report(req, false, out)?;

    if !req.quiet {
        if let Some(hint) = &req.cargo_install_hint {
            writeln!(
                out,
                "note: this dreamd binary looks cargo-installed ({hint}); `update` does not replace it — rebuild with `cargo install --path crates/dreamd-cli`"
            )?;
        }
        // Step 1 reports the stop that already ran above. An unattempted stop
        // (non-unix) must hand the job back to the user, never claim success.
        //
        // A spared process is still running, so it must never read as "nothing
        // was running" (see `lifecycle_cleanup::StopReport`). The line points
        // at the stderr warnings rather than restating the reason, because
        // `spared` covers both causes (not attributable, or the signal did not
        // land) and each spared pid carries its own.
        //
        // The mixed line is the matched line plus a clause: a mixed sweep is
        // the *ordinary* result on a shared box (any other home's `dreamd mcp`
        // is a spared candidate), so the matched wording holds either way.
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
                "stop local `dreamd mcp` / `dreamd watch` yourself — this command does not stop them on Windows"
            }
        };
        write_contract(out, "restart contract:", step_one)?;
    } else {
        writeln!(out, "{COMPRESSED_NEXT}")?;
    }

    // After line — the running binary cannot change in-process, and update
    // cannot know which version the next spawn resolves.
    writeln!(
        out,
        "after: {VERSION_SHORT} (this process keeps its version; the next spawn runs the version npx resolves)"
    )?;

    Ok(())
}
