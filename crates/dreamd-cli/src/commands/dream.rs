//! Handler for `dreamd dream` — runs the deterministic dream cycle.

use std::fmt;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use dreamd_core::{
    consolidation::{run_deterministic_dream_cycle, DreamCycleError},
    decay::{run_decay_pruner, DecayError},
    layout::AgentRoot,
    server::{TantivyIndexHandle, DEFAULT_COMMIT_CADENCE},
};

/// Errors produced by the `dreamd dream` command.
#[derive(Debug)]
pub enum DreamCliError {
    DreamCycle(DreamCycleError),
    Decay(DecayError),
    Index(String),
    Io(std::io::Error),
    /// HTTP 409 from the daemon — a cycle is already running for this project.
    DaemonInProgress,
    /// The daemon proxy failed (transport, or a non-404/409 daemon status).
    DaemonProxy(String),
}

impl fmt::Display for DreamCliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DreamCycle(e) => write!(f, "dream cycle error: {e}"),
            Self::Decay(e) => write!(f, "decay error: {e}"),
            Self::Index(s) => write!(f, "index error: {s}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::DaemonInProgress => {
                write!(f, "a dream cycle is already running for this project (daemon)")
            }
            Self::DaemonProxy(s) => write!(f, "daemon proxy error: {s}"),
        }
    }
}

impl std::error::Error for DreamCliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::DreamCycle(e) => Some(e),
            Self::Decay(e) => Some(e),
            Self::Index(_) | Self::DaemonInProgress | Self::DaemonProxy(_) => None,
            Self::Io(e) => Some(e),
        }
    }
}

impl From<std::io::Error> for DreamCliError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Run the full deterministic dream cycle from the CLI.
pub fn run(
    project_root: &Path,
    out: &mut impl Write,
    no_commit: bool,
) -> Result<(), DreamCliError> {
    // WEG-271 fast-follow: if a daemon owns this project, proxy the cycle to it
    // (one writer). --no-commit always runs in-process (determinism / commit
    // semantics the HTTP API can't express).
    #[cfg(unix)]
    if !no_commit {
        if let Some(decided) = try_proxy_to_daemon(project_root, out)? {
            return Ok(decided); // daemon ran it (409 already surfaced as Err)
        }
        // None → no daemon for this project; fall through to in-process.
    }

    let agent_root = AgentRoot::new(project_root);

    // Clock read for the cycle — both values derive from it. The core threads
    // `now_sec` as a caller-provided parameter (see consolidation.rs: "now_sec
    // is caller-provided for determinism — do not call Utc::now()"); this is the
    // lone CLI-boundary clock read, so SOURCE_DATE_EPOCH is honored here.
    let now_sec = resolve_now_sec()?;
    let cycle_date = chrono::DateTime::from_timestamp(now_sec, 0)
        .expect("valid unix timestamp")
        .format("%Y-%m-%d")
        .to_string();

    // WEG-63 — capture dirty state BEFORE the cycle runs.
    let dirty_at_cycle_start = if no_commit {
        Vec::new()
    } else {
        dreamd_core::autobiography::check_dirty_at_cycle_start(project_root).unwrap_or_default()
    };

    run_deterministic_dream_cycle(&agent_root, now_sec).map_err(DreamCliError::DreamCycle)?;

    let result =
        run_decay_pruner(&agent_root, now_sec, &cycle_date).map_err(DreamCliError::Decay)?;

    // Capture counts before moving result.decayed_ids.
    let decayed_count = result.decayed_ids.len();
    let kept_count = result.kept_count;

    // Tantivy prune — Phase 1 pattern: open fresh handle per call.
    // main() is sync, so spin up a new current-thread runtime.
    // Do NOT use Handle::current().block_on() — panics inside existing runtime.
    if decayed_count > 0 {
        let handle = TantivyIndexHandle::open(&agent_root, DEFAULT_COMMIT_CADENCE)
            .map_err(|e| DreamCliError::Index(e.to_string()))?;
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(handle.prune_decayed_events(result.decayed_ids))
            .map_err(|e| DreamCliError::Index(e.to_string()))?;
    }

    // WEG-63 — autobiography commit. Best-effort; failure does not fail the cycle.
    if !no_commit {
        if let Err(e) = dreamd_core::autobiography::commit_cycle(
            &agent_root,
            &cycle_date,
            &dirty_at_cycle_start,
        ) {
            tracing::error!(
                error = %e,
                "autobiography commit failed (dream cycle still succeeded)"
            );
        }
    }

    writeln!(
        out,
        "dream cycle complete ({decayed_count} events decayed, {kept_count} kept)",
    )?;
    Ok(())
}

/// Attempt to proxy the cycle to a running daemon over the UDS.
///
/// `Ok(Some(()))` → the daemon ran it (caller returns success). `Ok(None)` →
/// no daemon owns this project (socket unresolved, unreachable, or 404
/// not-registered); the caller runs the in-process cycle. `Err(..)` → a 409
/// (cycle already running) or a genuine proxy failure; the caller must NOT run
/// in-process (that would race the coordinator).
#[cfg(unix)]
fn try_proxy_to_daemon(
    project_root: &Path,
    out: &mut impl Write,
) -> Result<Option<()>, DreamCliError> {
    use dreamd_core::client::{proxy_dream_cycle, resolve_daemon_socket, DreamProxyOutcome};

    let Some(sock) = resolve_daemon_socket() else {
        return Ok(None);
    };
    let outcome = tokio::runtime::Runtime::new()?
        .block_on(proxy_dream_cycle(&sock, project_root))
        .map_err(|e| DreamCliError::DaemonProxy(e.to_string()))?;
    match outcome {
        DreamProxyOutcome::Ran => {
            writeln!(out, "dream cycle complete (via daemon)")?;
            Ok(Some(()))
        }
        DreamProxyOutcome::InProgress => Err(DreamCliError::DaemonInProgress),
        DreamProxyOutcome::NotReachable | DreamProxyOutcome::ProjectNotRegistered => Ok(None),
    }
}

/// Resolve the dream-cycle clock.
///
/// `SOURCE_DATE_EPOCH` (the reproducible-builds convention, an integer unix
/// timestamp) pins the clock so deterministic fixtures regenerate
/// byte-identically; absent → the wall clock. Reading an env var rather than
/// adding a flag keeps the `dreamd dream` clap surface — and its byte-locked
/// help snapshot — unchanged.
fn resolve_now_sec() -> Result<i64, DreamCliError> {
    match std::env::var("SOURCE_DATE_EPOCH") {
        Ok(raw) => parse_epoch_override(&raw),
        Err(_) => Ok(SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64),
    }
}

/// Parse a `SOURCE_DATE_EPOCH` value into a unix-second count.
///
/// Surrounding whitespace is tolerated; anything else fails with
/// [`std::io::ErrorKind::InvalidInput`] rather than silently falling back to
/// the wall clock (a malformed pin would defeat the determinism it requests).
fn parse_epoch_override(raw: &str) -> Result<i64, DreamCliError> {
    raw.trim().parse::<i64>().map_err(|_| {
        DreamCliError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "SOURCE_DATE_EPOCH must be an integer unix timestamp",
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_epoch_override_accepts_integer() {
        assert_eq!(parse_epoch_override("1748520000").unwrap(), 1_748_520_000);
    }

    #[test]
    fn parse_epoch_override_trims_whitespace() {
        assert_eq!(
            parse_epoch_override("  1748520000\n").unwrap(),
            1_748_520_000
        );
    }

    #[test]
    fn parse_epoch_override_rejects_non_integer() {
        let err = parse_epoch_override("not-a-number").unwrap_err();
        match err {
            DreamCliError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput),
            other => panic!("expected Io(InvalidInput), got {other:?}"),
        }
    }
}
