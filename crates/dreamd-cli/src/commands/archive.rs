//! `dreamd archive --force-unpin <id|--all>` — operator safety valve (WEG-134 / DR-116).
//!
//! Clears the sticky `pinned` flag on episodic entries so the next pruning pass
//! (`dreamd dream` decay) can remove them. This is the manual escape hatch for
//! the pin lifecycle: the dream cycle only ever *unions* pins (WEG-426), so an
//! entry pinned by an external writer stays pinned forever unless an operator
//! force-clears it here.
//!
//! Modeled on [`crate::commands::reset`]: a typed error enum, [`AgentRoot::discover`]
//! for root resolution, and injectable `out`/`err` writers for testability. The
//! read → mutate → rewrite path is the inverse of
//! [`dreamd_core::consolidation::apply_pin_unpin`] — force-clear instead of
//! union-set — and reuses the same [`dreamd_core::episodic`] I/O seam
//! ([`read_all`](dreamd_core::episodic::read_all) +
//! [`rewrite_atomic`](dreamd_core::episodic::rewrite_atomic)) so no JSONL I/O is
//! hand-rolled.
//!
//! **Daemon-coexistence guard (correctness, not scope):** a live single-writer
//! daemon holds an open fd on the JSONL and appends by offset. If we
//! `rewrite_atomic` (temp + rename) underneath it, the daemon keeps writing to
//! the old inode and the unpin is silently lost. So we refuse when the daemon
//! socket probes live (`is_daemon_socket_live`) and tell the operator to stop it
//! first — a persisted clear is the only correct implementation of "clears the
//! pin flag".

use std::io::Write;
use std::path::Path;

use dreamd_core::episodic::{self, EpisodicError};
use dreamd_core::server::is_daemon_socket_live;
use dreamd_core::{AgentRoot, LayoutError};

#[derive(Debug)]
pub enum ArchiveError {
    /// No `.agent/` store found walking up from `cwd`.
    NotFound,
    /// The daemon is live; a rewrite underneath it would be lost.
    DaemonRunning,
    /// `archive` invoked without `--force-unpin` (the only op today).
    ForceUnpinRequired,
    /// `--force-unpin` with neither an event id nor `--all`.
    NoTarget,
    /// Both an event id and `--all` were given.
    IdAndAll,
    /// No episodic entry matches the requested id.
    UnknownId(String),
    /// Failure reading or rewriting the episodic log.
    Episodic(EpisodicError),
    /// Failure writing to the `out`/`err` sinks.
    Io(std::io::Error),
}

impl From<std::io::Error> for ArchiveError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<EpisodicError> for ArchiveError {
    fn from(e: EpisodicError) -> Self {
        Self::Episodic(e)
    }
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "no .agent/ directory found"),
            Self::DaemonRunning => write!(f, "daemon is running; stop it first"),
            Self::ForceUnpinRequired => {
                write!(
                    f,
                    "archive requires --force-unpin (the only archive op today)"
                )
            }
            Self::NoTarget => write!(f, "specify an event id or --all"),
            Self::IdAndAll => write!(f, "cannot combine an id with --all"),
            Self::UnknownId(id) => write!(f, "no entry with id {id}"),
            Self::Episodic(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ArchiveError {}

/// `dreamd archive --force-unpin <EVENT_ID | --all>` entry point.
///
/// Validates the flag combination, refuses if a daemon is holding the log, then
/// clears `pinned` on the targeted episodic entries via a single atomic rewrite.
/// One `tracing::warn!` is emitted per cleared id (audit trail). Prints a
/// `N entr{y,ies} unpinned` summary to `out`; `--all` with nothing pinned is a
/// no-op success (`0 entries unpinned`, no rewrite).
///
/// `socket` is the resolved daemon UDS path (`None` when the home directory
/// cannot be resolved — treated as "no daemon", matching `dreamd status`).
pub fn run(
    cwd: &Path,
    socket: Option<&Path>,
    force_unpin: bool,
    id: Option<&str>,
    all: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), ArchiveError> {
    // `--force-unpin` is the only archive operation today.
    if !force_unpin {
        writeln!(
            err,
            "dreamd: error — archive requires --force-unpin (the only archive operation today)"
        )?;
        return Err(ArchiveError::ForceUnpinRequired);
    }

    // Target must be exactly one of <EVENT_ID> or --all.
    match (id, all) {
        (Some(_), true) => {
            writeln!(err, "dreamd: error — cannot combine an id with --all")?;
            return Err(ArchiveError::IdAndAll);
        }
        (None, false) => {
            writeln!(err, "dreamd: error — specify an event id or --all")?;
            return Err(ArchiveError::NoTarget);
        }
        _ => {}
    }

    let root = match AgentRoot::discover(cwd) {
        Ok(r) => r,
        Err(LayoutError::NotFound) => {
            writeln!(
                err,
                "dreamd: error — no .agent/ directory found. Run `dreamd init` first."
            )?;
            return Err(ArchiveError::NotFound);
        }
    };

    // Refuse while a live daemon holds the log: its offset-append writes would
    // survive our rename and clobber the unpin. Stop the daemon, then retry.
    if let Some(sock) = socket {
        if is_daemon_socket_live(sock) {
            writeln!(
                err,
                "dreamd: error — daemon is running; stop it first — dreamd cannot safely \
                 rewrite the log while the daemon holds it."
            )?;
            return Err(ArchiveError::DaemonRunning);
        }
    }

    let jsonl = root.episodic_jsonl();
    let mut events = episodic::read_all(&jsonl)?;

    // Collect the ids we actually flip true → false so the audit log and the
    // summary count only real state changes (idempotent clear of an already
    // unpinned entry is a no-op, not an error).
    let mut cleared: Vec<String> = Vec::new();

    if let Some(target) = id {
        match events.iter_mut().find(|e| e.id.as_str() == target) {
            None => {
                writeln!(err, "dreamd: error — no entry with id {target}")?;
                return Err(ArchiveError::UnknownId(target.to_string()));
            }
            Some(entry) => {
                if entry.pinned {
                    entry.pinned = false;
                    cleared.push(target.to_string());
                }
            }
        }
    } else {
        // `all == true` here (validated above).
        for entry in events.iter_mut() {
            if entry.pinned {
                entry.pinned = false;
                cleared.push(entry.id.as_str().to_string());
            }
        }
    }

    // Audit trail: one WARN per cleared id.
    for cleared_id in &cleared {
        tracing::warn!(event_id = %cleared_id, "archive: force-unpinned episodic entry");
    }

    // Only touch the file when something changed. No-op hook: a guarded one-shot
    // CLI rewrite has no open WAL (that hook is for the daemon dream cycle).
    if !cleared.is_empty() {
        episodic::rewrite_atomic(&jsonl, &events, || Ok(()))?;
    }

    let n = cleared.len();
    writeln!(out, "{n} entr{} unpinned", if n == 1 { "y" } else { "ies" })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use dreamd_core::episodic::rewrite_atomic;
    use dreamd_protocol::{AgentLearning, EventId};
    use std::fs;
    use std::path::PathBuf;

    // Fixed young timestamp (2026-07-03Z) — irrelevant to unpin, kept realistic.
    const NOW_SEC: i64 = 1751500800;

    /// Distinct valid `evt_` ids for fixtures (26-char Crockford ULID tail).
    fn evt_id(suffix: char) -> String {
        format!("evt_01ARZ3NDEKTSV4RRFFQ69G5FA{suffix}")
    }

    fn learning(suffix: char, pinned: bool) -> AgentLearning {
        AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: EventId::parse(&evt_id(suffix)).expect("valid EventId"),
            timestamp: DateTime::<Utc>::from_timestamp(NOW_SEC, 0).expect("valid ts"),
            pain: 5.0,
            importance: 6.0,
            pinned,
            skill_action: "rust::archive".to_string(),
            source_harness: "test-harness".to_string(),
            content: format!("entry {suffix}"),
        }
    }

    /// Scaffold a `.agent/` root under a fresh tempdir and seed the episodic log
    /// with `records`. Returns the root dir (for `cwd`) and the JSONL path.
    fn seed(records: &[AgentLearning]) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(tmp.path());
        let jsonl = root.episodic_jsonl();
        fs::create_dir_all(jsonl.parent().unwrap()).unwrap();
        rewrite_atomic(&jsonl, records, || Ok(())).unwrap();
        (tmp, jsonl)
    }

    fn pinned_of(jsonl: &Path, suffix: char) -> bool {
        let target = evt_id(suffix);
        episodic::read_all(jsonl)
            .unwrap()
            .into_iter()
            .find(|e| e.id.as_str() == target)
            .expect("entry present")
            .pinned
    }

    #[test]
    fn display_covers_all_variants() {
        assert_eq!(
            ArchiveError::NotFound.to_string(),
            "no .agent/ directory found"
        );
        assert_eq!(
            ArchiveError::DaemonRunning.to_string(),
            "daemon is running; stop it first"
        );
        assert_eq!(
            ArchiveError::ForceUnpinRequired.to_string(),
            "archive requires --force-unpin (the only archive op today)"
        );
        assert_eq!(
            ArchiveError::NoTarget.to_string(),
            "specify an event id or --all"
        );
        assert_eq!(
            ArchiveError::IdAndAll.to_string(),
            "cannot combine an id with --all"
        );
        assert_eq!(
            ArchiveError::UnknownId("evt_x".to_string()).to_string(),
            "no entry with id evt_x"
        );
        let io = ArchiveError::from(std::io::Error::other("disk full"));
        assert_eq!(io.to_string(), "disk full");
    }

    #[test]
    fn single_id_clears_only_that_pin() {
        let (tmp, jsonl) = seed(&[learning('A', true), learning('B', true)]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        run(
            tmp.path(),
            None,
            true,
            Some(&evt_id('A')),
            false,
            &mut out,
            &mut err,
        )
        .unwrap();

        assert!(!pinned_of(&jsonl, 'A'), "target A must be unpinned");
        assert!(pinned_of(&jsonl, 'B'), "B must be left pinned");
        assert!(String::from_utf8(out).unwrap().contains("1 entry unpinned"));
        assert!(err.is_empty());
    }

    #[test]
    fn all_clears_every_pin() {
        let (tmp, jsonl) = seed(&[
            learning('A', true),
            learning('B', false),
            learning('C', true),
        ]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        run(tmp.path(), None, true, None, true, &mut out, &mut err).unwrap();

        assert!(!pinned_of(&jsonl, 'A'));
        assert!(!pinned_of(&jsonl, 'B'));
        assert!(!pinned_of(&jsonl, 'C'));
        // Only the two that were pinned count as unpinned.
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("2 entries unpinned"));
        assert!(err.is_empty());
    }

    #[test]
    fn all_with_nothing_pinned_is_noop_success() {
        let (tmp, _jsonl) = seed(&[learning('A', false), learning('B', false)]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        run(tmp.path(), None, true, None, true, &mut out, &mut err).unwrap();

        assert!(String::from_utf8(out)
            .unwrap()
            .contains("0 entries unpinned"));
        assert!(err.is_empty());
    }

    #[test]
    fn unknown_id_errors_and_leaves_file_unchanged() {
        let (tmp, jsonl) = seed(&[learning('A', true)]);
        let before = fs::read(&jsonl).unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run(
            tmp.path(),
            None,
            true,
            Some(&evt_id('Z')),
            false,
            &mut out,
            &mut err,
        );

        assert!(matches!(result, Err(ArchiveError::UnknownId(_))));
        assert_eq!(fs::read(&jsonl).unwrap(), before, "file must be untouched");
        assert!(out.is_empty());
        let stderr = String::from_utf8(err).unwrap();
        assert!(stderr.contains("no entry with id"));
    }

    #[test]
    fn no_target_errors() {
        let (tmp, _jsonl) = seed(&[learning('A', true)]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run(tmp.path(), None, true, None, false, &mut out, &mut err);

        assert!(matches!(result, Err(ArchiveError::NoTarget)));
        assert!(out.is_empty());
        assert!(String::from_utf8(err)
            .unwrap()
            .contains("specify an event id or --all"));
    }

    #[test]
    fn id_and_all_errors() {
        let (tmp, _jsonl) = seed(&[learning('A', true)]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run(
            tmp.path(),
            None,
            true,
            Some(&evt_id('A')),
            true,
            &mut out,
            &mut err,
        );

        assert!(matches!(result, Err(ArchiveError::IdAndAll)));
        assert!(out.is_empty());
        assert!(String::from_utf8(err)
            .unwrap()
            .contains("cannot combine an id with --all"));
    }

    #[test]
    fn force_unpin_absent_errors() {
        let (tmp, _jsonl) = seed(&[learning('A', true)]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run(
            tmp.path(),
            None,
            false,
            Some(&evt_id('A')),
            false,
            &mut out,
            &mut err,
        );

        assert!(matches!(result, Err(ArchiveError::ForceUnpinRequired)));
        assert!(out.is_empty());
        assert!(String::from_utf8(err).unwrap().contains("--force-unpin"));
    }

    #[test]
    fn missing_agent_dir_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run(
            tmp.path(),
            None,
            true,
            Some(&evt_id('A')),
            false,
            &mut out,
            &mut err,
        );
        assert!(matches!(result, Err(ArchiveError::NotFound)));
        assert!(out.is_empty());
        let stderr = String::from_utf8(err).unwrap();
        assert!(stderr.contains("no .agent/ directory found"));
        assert!(stderr.contains("dreamd init"));
    }
}
