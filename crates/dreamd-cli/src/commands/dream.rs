//! `dreamd dream` — deterministic dream cycle from the CLI.
//!
//! ## Execution modes
//!
//! 1. **Daemon proxy** (default on Unix when `dreamd watch` owns the project):
//!    `POST /api/v1/dream` over the UDS. Returns early on HTTP 409 (cycle in progress).
//! 2. **In-process** (no daemon, or `--no-commit`): runs the full cycle via
//!    [`dreamd_core::dream_cycle::run_in_process`].
//!
//! `--no-commit` is the ONLY flag that skips the proxy. `--no-llm` (AILAB-204)
//! travels *through* it as the `x-dreamd-no-llm: 1` header — skipping the daemon
//! to honor it locally would run a second writer against a JSONL the daemon's
//! coordinator owns.
//!
//! ## WAL protocol
//!
//! The in-process path writes [`dream_in_progress.wal`](dreamd_core::wal::DreamWal) before
//! destructive steps. `--no-commit` skips the git autobiography commit but still runs
//! the full cycle.
//!
//! ## `--dry` contract
//!
//! [`run_dry`] previews the cycle and writes nothing: it prints the `LESSONS.md`
//! this cycle *would* persist (or reports that it would retire the file) and
//! returns. It is the one dream path that **skips the daemon proxy** — the
//! inverse of `--no-llm`, which must travel through it. A `POST /api/v1/dream`
//! is a write by definition, so proxying a dry run would perform the very
//! mutation the flag exists to avoid (AILAB-341). `--no-commit` is meaningless
//! alongside `--dry` and is ignored rather than rejected.

use std::fmt;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use dreamd_core::dream_cycle::{self, DreamCycleError};

/// Errors produced by the `dreamd dream` command.
#[derive(Debug)]
pub enum DreamCliError {
    DreamCycle(DreamCycleError),
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
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::DaemonInProgress => {
                write!(
                    f,
                    "a dream cycle is already running for this project (daemon)"
                )
            }
            Self::DaemonProxy(s) => write!(f, "daemon proxy error: {s}"),
        }
    }
}

impl std::error::Error for DreamCliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::DreamCycle(e) => Some(e),
            Self::DaemonInProgress | Self::DaemonProxy(_) => None,
            Self::Io(e) => Some(e),
        }
    }
}

impl From<std::io::Error> for DreamCliError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Run the full dream cycle from the CLI.
///
/// `no_llm` forces the deterministic lesson body on whichever path runs; it does
/// not change *which* path runs.
pub fn run(
    project_root: &Path,
    out: &mut impl Write,
    no_commit: bool,
    no_llm: bool,
) -> Result<(), DreamCliError> {
    // WEG-271 fast-follow: if a daemon owns this project, proxy the cycle to it
    // (one writer). --no-commit always runs in-process (determinism / commit
    // semantics the HTTP API can't express). `no_llm` is passed INTO the proxy,
    // never AND-ed into this condition.
    #[cfg(unix)]
    if !no_commit {
        if let Some(decided) = try_proxy_to_daemon(project_root, out, no_llm)? {
            return Ok(decided); // daemon ran it (409 already surfaced as Err)
        }
        // None → no daemon for this project; fall through to in-process.
    }

    let now_sec = resolve_now_sec()?;

    // WEG-63 — capture dirty state BEFORE the cycle runs.
    let dirty_at_cycle_start = if no_commit {
        Vec::new()
    } else {
        dreamd_core::autobiography::check_dirty_at_cycle_start(project_root).unwrap_or_default()
    };

    let result = dream_cycle::run_in_process(
        project_root,
        now_sec,
        no_commit,
        no_llm,
        dirty_at_cycle_start,
    )
    .map_err(DreamCliError::DreamCycle)?;

    let decayed_count = result.decay.decayed_ids.len();
    let kept_count = result.decay.kept_count;

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
    no_llm: bool,
) -> Result<Option<()>, DreamCliError> {
    use dreamd_core::client::{proxy_dream_cycle, resolve_daemon_socket, DreamProxyOutcome};

    let Some(sock) = resolve_daemon_socket() else {
        return Ok(None);
    };
    let outcome = tokio::runtime::Runtime::new()?
        .block_on(proxy_dream_cycle(&sock, project_root, no_llm))
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

/// What a `--dry` preview decided the cycle would do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DryOutcome {
    /// A cluster promoted; the would-be `LESSONS.md` was written to `out`.
    WouldWrite,
    /// Nothing promoted; a real cycle would unlink `LESSONS.md` (AILAB-699).
    /// Nothing was written to `out`, and the file on disk is untouched.
    WouldRetire,
}

/// Preview the dream cycle and write nothing (AILAB-341).
///
/// Writes the `LESSONS.md` bytes a real cycle would persist to `out` when a
/// cluster promotes, so the founder can diff composed prose against the raw
/// JSONL without mutating `.agent/`. Stdout carries only those bytes — the
/// human-facing banner is the caller's, on stderr — so the preview stays
/// pipe-clean. Under `no_llm` the bytes are reproducible and a subsequent real
/// cycle writes exactly them; with a live model the prose is that call's, so a
/// later cycle composes its own.
///
/// Unlike [`run`] this never consults the daemon. `--no-llm` travels *through*
/// the proxy because the daemon is the single writer; `--dry` skips it because
/// there is nothing to write and `POST /api/v1/dream` would write anyway.
/// `no_commit` is not a parameter: with no writes there is no commit to skip.
pub fn run_dry(
    project_root: &Path,
    out: &mut impl Write,
    no_llm: bool,
) -> Result<DryOutcome, DreamCliError> {
    let now_sec = resolve_now_sec()?;
    let preview = dream_cycle::preview_in_process(project_root, now_sec, no_llm)
        .map_err(DreamCliError::DreamCycle)?;

    match preview.lessons_markdown {
        Some(markdown) => {
            out.write_all(markdown.as_bytes())?;
            Ok(DryOutcome::WouldWrite)
        }
        None => Ok(DryOutcome::WouldRetire),
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
    use std::error::Error;
    use std::fs;

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

    #[test]
    fn display_and_source_cover_all_variants() {
        let cycle = DreamCliError::DreamCycle(DreamCycleError::InProgress);
        assert!(cycle.to_string().contains("dream cycle error"));
        assert!(cycle.source().is_some());

        let io = DreamCliError::from(std::io::Error::other("write failed"));
        assert!(io.to_string().contains("I/O error"));
        assert!(io.source().is_some());

        let in_progress = DreamCliError::DaemonInProgress;
        assert!(in_progress.to_string().contains("already running"));
        assert!(in_progress.source().is_none());

        let proxy = DreamCliError::DaemonProxy("timeout".into());
        assert_eq!(proxy.to_string(), "daemon proxy error: timeout");
        assert!(proxy.source().is_none());
    }

    /// AILAB-341: a promoting store previews without touching `.agent/`.
    ///
    /// The clock is the wall clock (no `SOURCE_DATE_EPOCH` here), so the fixture
    /// events are stamped "now" to land inside the 7-day recurrence window.
    #[test]
    fn run_dry_previews_a_promoting_store_without_writing() {
        use chrono::Utc;
        use dreamd_core::consolidation::PROMOTION_THRESHOLD;
        use dreamd_core::layout::AgentRoot;
        use dreamd_protocol::{AgentLearning, EventId};

        let tmp = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(tmp.path());
        fs::create_dir_all(root.episodic_jsonl().parent().unwrap()).unwrap();
        fs::create_dir_all(root.dreamd_dir()).unwrap();

        let now = Utc::now();
        let mut body = String::new();
        for i in 0..PROMOTION_THRESHOLD {
            let event = AgentLearning {
                schema_version: "1.0.0".to_string(),
                id: EventId::parse(&format!("evt_01ARZ3NDEKTSV4RRFFQ69G5F{i:0>2}"))
                    .expect("valid EventId"),
                timestamp: now,
                pain: 5.0,
                importance: 6.0,
                pinned: false,
                skill_action: "rust::error_handling".to_string(),
                source_harness: "test-harness".to_string(),
                content: format!("lesson body {i}"),
            };
            body.push_str(&serde_json::to_string(&event).unwrap());
            body.push('\n');
        }
        fs::write(root.episodic_jsonl(), &body).unwrap();

        let mut out = Vec::new();
        let outcome = run_dry(tmp.path(), &mut out, true).expect("dry run");

        assert_eq!(outcome, DryOutcome::WouldWrite);
        let stdout = String::from_utf8(out).unwrap();
        assert!(
            stdout.contains("prompt_version:"),
            "stdout is the would-be LESSONS.md, frontmatter included; got: {stdout}"
        );
        assert!(stdout.contains("rust::error_handling"), "got: {stdout}");

        assert!(
            !root.lessons_md().exists(),
            "--dry must not create LESSONS.md"
        );
        assert!(
            !root.semantic_dir().join("recurrence_counts.json").exists(),
            "--dry must not write the recurrence sidecar"
        );
        assert!(!root.wal_path().exists(), "--dry must not open a WAL");
        assert_eq!(
            fs::read_to_string(root.episodic_jsonl()).unwrap(),
            body,
            "--dry must not rewrite the episodic log"
        );
    }

    /// Nothing promotes (empty log): `WouldRetire`, and stdout stays empty so a
    /// piped `--dry` yields no bytes rather than an empty frontmatter file.
    #[test]
    fn run_dry_on_an_empty_store_reports_would_retire_with_empty_stdout() {
        let tmp = tempfile::tempdir().unwrap();
        let episodic = tmp.path().join(".agent/episodic");
        fs::create_dir_all(&episodic).unwrap();
        fs::write(episodic.join("AGENT_LEARNINGS.jsonl"), b"").unwrap();

        let mut out = Vec::new();
        let outcome = run_dry(tmp.path(), &mut out, true).expect("dry run");

        assert_eq!(outcome, DryOutcome::WouldRetire);
        assert!(out.is_empty(), "stdout must be empty on a retire preview");
    }

    #[test]
    fn run_no_commit_completes_on_minimal_store() {
        let tmp = tempfile::tempdir().unwrap();
        let episodic = tmp.path().join(".agent/episodic");
        fs::create_dir_all(&episodic).unwrap();
        fs::write(episodic.join("AGENT_LEARNINGS.jsonl"), b"").unwrap();

        let mut out = Vec::new();
        run(tmp.path(), &mut out, true, true).unwrap();
        let stdout = String::from_utf8(out).unwrap();
        assert!(
            stdout.contains("dream cycle complete"),
            "expected completion line, got: {stdout}"
        );
        assert!(stdout.contains("events decayed"));
        assert!(stdout.contains("kept"));
    }
}
