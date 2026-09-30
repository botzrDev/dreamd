//! Forget cascade (BZR-158): remove one episodic event and the derived state
//! that names it.
//!
//! [`cascade`] runs under the dream-cycle WAL but is not a dream cycle. It
//! rewrites the live JSONL without the one id, drops or unlinks the lesson that
//! cites it, recounts the recurrence sidecar, and appends ledger edges for the
//! lesson that remains. It never decays other rows, never re-selects a lesson,
//! and never copies the forgotten row into `snapshots/`.
//!
//! The JSONL has one writer, so production calls go through
//! [`MemoryCoordinatorMsg::Forget`](crate::coordinator::MemoryCoordinatorMsg::Forget),
//! which reopens the append fd and tells the live indexer to drop the document.
//! This module does not touch the index.
//!
//! Existing ledger lines are left in place. A forgotten id's old edge stays in
//! the file, and `provenance::verify` reports it as an orphan.
//!
//! `dreamd forget` (BZR-151) is the CLI. It calls [`cascade`] when no daemon is
//! live, or [`preview`] for `--dry-run`. `--proof` writes a forget receipt
//! ([`render_receipt`], `forget-receipt/1.0`). The signed membership proof in
//! `docs/provenance.md` is still absent.

use std::io;

use chrono::DateTime;
use dreamd_protocol::EventId;
use serde::Serialize;

use crate::consolidation::{self, ConsolidationError};
use crate::dream_cycle::{self, DreamCycleError};
use crate::episodic::{self, EpisodicError};
use crate::index::RecurrenceSidecar;
use crate::io::write_atomic;
use crate::layout::AgentRoot;
use crate::lessons;
use crate::provenance::{self, ProvenanceError};
use crate::wal::{self, GuardedReplaceKind, WalError};

/// What [`cascade`] did to `semantic/LESSONS.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LessonEffect {
    /// No lesson file, or the file does not name the id.
    Untouched,
    /// The id was a citation but not the exemplar; it was dropped from
    /// `citations` and the file was rewritten.
    CitationDropped,
    /// The id was a `Lesson.id`. The lesson body is that event's content, so
    /// the file was unlinked.
    Unlinked,
}

impl LessonEffect {
    /// Stable token for CLI output and the forget receipt.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Untouched => "untouched",
            Self::CitationDropped => "citation_dropped",
            Self::Unlinked => "unlinked",
        }
    }
}

/// Outcome of [`cascade`] or [`preview`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForgetReport {
    /// `false` when the id was not in the live log. Nothing was written.
    pub removed: bool,
    pub lesson: LessonEffect,
}

#[derive(Debug, thiserror::Error)]
pub enum ForgetError {
    #[error("dream cycle already in progress")]
    InProgress,
    #[error("episodic log: {0}")]
    Episodic(#[from] EpisodicError),
    #[error("WAL: {0}")]
    Wal(#[from] WalError),
    #[error("lessons: {0}")]
    Lessons(io::Error),
    #[error("recurrence sidecar: {0}")]
    Sidecar(#[from] ConsolidationError),
    #[error("provenance: {0}")]
    Provenance(#[from] ProvenanceError),
}

impl From<DreamCycleError> for ForgetError {
    fn from(e: DreamCycleError) -> Self {
        match e {
            DreamCycleError::InProgress => ForgetError::InProgress,
            DreamCycleError::Wal(w) => ForgetError::Wal(w),
            // `ensure_not_in_progress` produces only the two arms above.
            other => ForgetError::Wal(WalError::Io(io::Error::other(other.to_string()))),
        }
    }
}

/// Remove `id` from the live episodic log and cascade to the lesson file, the
/// recurrence sidecar, and the provenance ledger.
///
/// A missing id returns `removed: false` without opening a WAL. A present id is
/// removed even when `pinned` is set. `now_sec` is caller-provided; this
/// function reads no clock.
///
/// A failure after `wal::begin_cycle` returns the error and leaves the WAL for
/// startup recovery. The caller must reopen any append fd it holds on the
/// JSONL, because the rewrite replaces the file by rename.
///
/// Returns [`io::ErrorKind::Unsupported`] (wrapped) on Windows, inherited from
/// `write_atomic`.
pub fn cascade(
    agent_root: &AgentRoot,
    id: &EventId,
    now_sec: i64,
) -> Result<ForgetReport, ForgetError> {
    let jsonl = agent_root.episodic_jsonl();
    let events = episodic::read_all(&jsonl)?;
    if !events.iter().any(|ev| ev.id == *id) {
        return Ok(ForgetReport {
            removed: false,
            lesson: LessonEffect::Untouched,
        });
    }

    dream_cycle::ensure_not_in_progress(agent_root)?;
    wal::begin_cycle(agent_root, now_sec)?;

    // Every other row, in read order, pinned or not.
    let remaining: Vec<_> = events.into_iter().filter(|ev| ev.id != *id).collect();
    episodic::rewrite_guarded(agent_root, &jsonl, &remaining)?;

    let lesson = cascade_lessons(agent_root, id.as_str(), now_sec)?;

    if remaining.is_empty() {
        // `run_cluster_engine` returns early on an empty log without touching
        // the sidecar; a stale count for the forgotten event would survive.
        let sidecar = RecurrenceSidecar {
            schema_version: "1.0".to_string(),
            clusters: vec![],
        };
        std::fs::create_dir_all(agent_root.semantic_dir()).map_err(ConsolidationError::Io)?;
        let json = serde_json::to_string_pretty(&sidecar)
            .map_err(|e| ConsolidationError::Json { line: 0, source: e })?;
        write_atomic(
            &agent_root.semantic_dir().join("recurrence_counts.json"),
            json.as_bytes(),
        )
        .map_err(ConsolidationError::Io)?;
    } else {
        consolidation::run_cluster_engine(agent_root, now_sec)?;
    }

    provenance::record_cycle(agent_root)?;
    wal::commit_cycle(agent_root, now_sec)?;

    Ok(ForgetReport {
        removed: true,
        lesson,
    })
}

/// Drop `id` from `LESSONS.md`, or unlink the file when `id` is a lesson's
/// exemplar. Only this file; no lesson is re-selected.
fn cascade_lessons(
    agent_root: &AgentRoot,
    id: &str,
    now_sec: i64,
) -> Result<LessonEffect, ForgetError> {
    let path = agent_root.lessons_md();
    let mut file = match lessons::read_lessons_file(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(LessonEffect::Untouched),
        Err(e) => return Err(ForgetError::Lessons(e)),
    };

    match classify_lessons(&file, id) {
        LessonEffect::Untouched => return Ok(LessonEffect::Untouched),
        LessonEffect::Unlinked => {
            // The body is the forgotten event's content; there is nothing to keep.
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(ForgetError::Lessons(e)),
            }
            return Ok(LessonEffect::Unlinked);
        }
        LessonEffect::CitationDropped => {}
    }

    file.citations.retain(|c| c != id);
    file.last_updated = DateTime::from_timestamp(now_sec, 0).unwrap_or(DateTime::UNIX_EPOCH);
    wal::guarded_replace(
        agent_root,
        &path,
        lessons::render_lessons_file(&file).as_bytes(),
        GuardedReplaceKind::ReplaceSemanticMemory,
    )?;
    Ok(LessonEffect::CitationDropped)
}

/// What forgetting `id` does to `file`. Shared by [`cascade_lessons`] and
/// [`preview`] so the two cannot drift: the exemplar unlinks, a citation drops,
/// anything else is untouched.
fn classify_lessons(file: &lessons::LessonsFile, id: &str) -> LessonEffect {
    if file.lessons.iter().any(|l| l.id == id) {
        LessonEffect::Unlinked
    } else if file.citations.iter().any(|c| c == id) {
        LessonEffect::CitationDropped
    } else {
        LessonEffect::Untouched
    }
}

/// Report what [`cascade`] would do to `id`, writing nothing.
///
/// Same decision order as `cascade`: a missing id returns `removed: false`
/// without checking the dream-cycle status; a present id refuses with
/// [`ForgetError::InProgress`] before reading `LESSONS.md`. Opens no WAL and
/// reads no clock.
pub fn preview(agent_root: &AgentRoot, id: &EventId) -> Result<ForgetReport, ForgetError> {
    let events = episodic::read_all(&agent_root.episodic_jsonl())?;
    if !events.iter().any(|ev| ev.id == *id) {
        return Ok(ForgetReport {
            removed: false,
            lesson: LessonEffect::Untouched,
        });
    }

    dream_cycle::ensure_not_in_progress(agent_root)?;

    let lesson = match lessons::read_lessons_file(&agent_root.lessons_md()) {
        Ok(file) => classify_lessons(&file, id.as_str()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => LessonEffect::Untouched,
        Err(e) => return Err(ForgetError::Lessons(e)),
    };
    Ok(ForgetReport {
        removed: true,
        lesson,
    })
}

/// Schema token of the file `dreamd forget --proof` writes.
pub const FORGET_RECEIPT_SCHEMA: &str = "forget-receipt/1.0";

#[derive(Serialize)]
struct Receipt<'a> {
    schema_version: &'static str,
    event_id: &'a str,
    removed: bool,
    lesson: &'static str,
    root: &'a str,
    orphans: Vec<ReceiptOrphan<'a>>,
}

#[derive(Serialize)]
struct ReceiptOrphan<'a> {
    line: u64,
    kind: &'a str,
    from: &'a str,
    to: &'a str,
}

/// Render the `forget-receipt/1.0` file: one compact JSON object and a
/// trailing `\n`.
///
/// Keys are `schema_version`, `event_id`, `removed` (always `true`), `lesson`,
/// `root` (the recomputed Merkle root from `provenance::verify`), and
/// `orphans` — the orphan edges whose `from` is `event_id`, in report order.
/// This is a receipt, not the signed membership proof in `docs/provenance.md`:
/// it carries no inclusion path and no signature.
pub fn render_receipt(
    event_id: &EventId,
    lesson: LessonEffect,
    report: &crate::provenance::ProvenanceReport,
) -> String {
    let id = event_id.as_str();
    let receipt = Receipt {
        schema_version: FORGET_RECEIPT_SCHEMA,
        event_id: id,
        removed: true,
        lesson: lesson.as_str(),
        root: &report.root,
        orphans: report
            .orphans
            .iter()
            .filter(|o| o.from == id)
            .map(|o| ReceiptOrphan {
                line: o.line,
                kind: &o.kind,
                from: &o.from,
                to: &o.to,
            })
            .collect(),
    };
    // Plain strings, integers, and a bool: serialization cannot fail.
    let mut out = serde_json::to_string(&receipt).expect("receipt serializes");
    out.push('\n');
    out
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;

    use dreamd_protocol::AgentLearning;

    use crate::daemon_state::update_daemon_state;
    use crate::lessons::{read_lessons_file, write_lessons_file, Lesson, LessonsFile};
    use crate::test_support::{unique_tmpdir, DirGuard};

    // 2026-07-03Z; event timestamps match so nothing is aged.
    const NOW_SEC: i64 = 1_751_500_800;
    const CLUSTER: &str = "rust::forget";

    fn eid(suffix: char) -> EventId {
        EventId::parse(&format!("evt_01ARZ3NDEKTSV4RRFFQ69G5FA{suffix}")).unwrap()
    }

    fn learning(suffix: char, content: &str, pinned: bool) -> AgentLearning {
        AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: eid(suffix),
            timestamp: DateTime::from_timestamp(NOW_SEC, 0).unwrap(),
            pain: 6.0,
            importance: 7.0,
            pinned,
            skill_action: CLUSTER.to_string(),
            source_harness: "test-harness".to_string(),
            content: content.to_string(),
        }
    }

    fn setup(label: &str, rows: &[AgentLearning]) -> (AgentRoot, DirGuard) {
        let dir = unique_tmpdir(label);
        let root = AgentRoot::new(&dir);
        fs::create_dir_all(root.episodic_dir()).unwrap();
        fs::create_dir_all(root.semantic_dir()).unwrap();
        episodic::rewrite_atomic(&root.episodic_jsonl(), rows).unwrap();
        (root, DirGuard(dir))
    }

    fn seed_lessons(root: &AgentRoot, exemplar: char, content: &str, citations: &[char]) {
        let file = LessonsFile {
            last_updated: DateTime::from_timestamp(NOW_SEC - 60, 0).unwrap(),
            prompt_version: "deterministic".to_string(),
            cluster_key: CLUSTER.to_string(),
            citations: citations.iter().map(|c| eid(*c).to_string()).collect(),
            lessons: vec![Lesson {
                id: eid(exemplar).to_string(),
                content: content.to_string(),
                pinned: false,
            }],
        };
        write_lessons_file(&root.lessons_md(), &file).unwrap();
    }

    fn ids(root: &AgentRoot) -> Vec<String> {
        episodic::read_all(&root.episodic_jsonl())
            .unwrap()
            .into_iter()
            .map(|ev| ev.id.to_string())
            .collect()
    }

    #[test]
    fn missing_id_is_not_removed_and_opens_no_wal() {
        let (root, _g) = setup("forget-missing", &[learning('A', "alpha", false)]);
        let before = fs::read(root.episodic_jsonl()).unwrap();

        let report = cascade(&root, &eid('Z'), NOW_SEC).unwrap();

        assert_eq!(
            report,
            ForgetReport {
                removed: false,
                lesson: LessonEffect::Untouched
            }
        );
        assert!(!root.wal_path().exists());
        assert!(!root.state_json().exists(), "begin_cycle must not run");
        assert_eq!(fs::read(root.episodic_jsonl()).unwrap(), before);
    }

    #[test]
    fn present_id_is_removed_and_neighbor_kept() {
        let (root, _g) = setup(
            "forget-present",
            &[learning('A', "alpha", false), learning('B', "beta", false)],
        );

        let report = cascade(&root, &eid('A'), NOW_SEC).unwrap();

        assert!(report.removed);
        assert_eq!(report.lesson, LessonEffect::Untouched);
        assert_eq!(ids(&root), vec![eid('B').to_string()]);
        assert!(!root.wal_path().exists());
        assert_eq!(wal::read_last_cycle_status(&root).unwrap(), "complete");
        assert!(
            !root.snapshots_dir().exists(),
            "the forgotten row must not be archived"
        );
    }

    #[test]
    fn pinned_id_is_removed() {
        let (root, _g) = setup(
            "forget-pinned",
            &[learning('A', "alpha", true), learning('B', "beta", false)],
        );

        let report = cascade(&root, &eid('A'), NOW_SEC).unwrap();

        assert!(report.removed);
        assert_eq!(ids(&root), vec![eid('B').to_string()]);
    }

    #[test]
    fn last_row_removed_writes_empty_sidecar() {
        let (root, _g) = setup("forget-last", &[learning('A', "alpha", false)]);
        let sidecar = root.semantic_dir().join("recurrence_counts.json");
        fs::write(
            &sidecar,
            r#"{"schema_version":"1.0","clusters":[{"skill_action":"rust::forget","count":3}]}"#,
        )
        .unwrap();

        cascade(&root, &eid('A'), NOW_SEC).unwrap();

        assert!(ids(&root).is_empty());
        let parsed: RecurrenceSidecar =
            serde_json::from_str(&fs::read_to_string(&sidecar).unwrap()).unwrap();
        assert_eq!(parsed.schema_version, "1.0");
        assert!(parsed.clusters.is_empty());
    }

    #[test]
    fn citation_that_is_not_the_exemplar_is_dropped() {
        let (root, _g) = setup(
            "forget-citation",
            &[
                learning('A', "alpha", false),
                learning('B', "beta body", false),
            ],
        );
        seed_lessons(&root, 'B', "beta body", &['B', 'A']);

        let report = cascade(&root, &eid('A'), NOW_SEC).unwrap();

        assert_eq!(report.lesson, LessonEffect::CitationDropped);
        let file = read_lessons_file(&root.lessons_md()).unwrap();
        assert_eq!(file.citations, vec![eid('B').to_string()]);
        assert_eq!(file.lessons.len(), 1);
        assert!(file.lessons[0].content.contains("beta body"));
        assert_eq!(file.last_updated.timestamp(), NOW_SEC);
    }

    #[test]
    fn exemplar_match_unlinks_lessons_md() {
        let (root, _g) = setup(
            "forget-exemplar",
            &[learning('A', "alpha", false), learning('B', "beta", false)],
        );
        seed_lessons(&root, 'A', "alpha", &['A']);

        let report = cascade(&root, &eid('A'), NOW_SEC).unwrap();

        assert_eq!(report.lesson, LessonEffect::Unlinked);
        assert!(!root.lessons_md().exists());
    }

    #[test]
    fn existing_ledger_line_for_removed_id_is_kept() {
        let (root, _g) = setup(
            "forget-ledger",
            &[learning('A', "alpha", false), learning('B', "beta", false)],
        );
        seed_lessons(&root, 'B', "beta", &['B', 'A']);
        let line = format!(
            "{{\"schema_version\":\"provenance/1.0\",\"kind\":\"lesson_citation\",\"from\":\"{}\",\"to\":\"lsn_{}\"}}\n",
            eid('A'),
            eid('B')
        );
        fs::create_dir_all(root.provenance_dir()).unwrap();
        fs::write(root.provenance_ledger(), &line).unwrap();

        cascade(&root, &eid('A'), NOW_SEC).unwrap();

        let ledger = fs::read_to_string(root.provenance_ledger()).unwrap();
        assert!(
            ledger.starts_with(&line),
            "old line must stay first: {ledger}"
        );
    }

    #[test]
    fn in_progress_status_refuses_before_any_rewrite() {
        let (root, _g) = setup(
            "forget-inprogress",
            &[learning('A', "alpha", false), learning('B', "beta", false)],
        );
        update_daemon_state(&root, |s| {
            s.last_dream_cycle_status = "in_progress".to_string()
        })
        .unwrap();
        let before = fs::read(root.episodic_jsonl()).unwrap();

        let err = cascade(&root, &eid('A'), NOW_SEC).unwrap_err();

        assert!(matches!(err, ForgetError::InProgress), "{err}");
        assert_eq!(fs::read(root.episodic_jsonl()).unwrap(), before);
        assert!(!root.wal_path().exists());
    }

    #[test]
    fn crash_after_intent_recovers_to_the_pre_forget_log() {
        let (root, _g) = setup(
            "forget-crash",
            &[learning('A', "alpha", false), learning('B', "beta", false)],
        );
        let jsonl = root.episodic_jsonl();
        let before = fs::read(&jsonl).unwrap();
        let mut rest = serde_json::to_string(&learning('B', "beta", false)).unwrap();
        rest.push('\n');

        wal::begin_cycle(&root, NOW_SEC).unwrap();
        let err = wal::guarded_replace_crash_after_intent(
            &root,
            &jsonl,
            rest.as_bytes(),
            GuardedReplaceKind::PruneEpisodicMemory,
        );
        assert!(err.is_err());
        let tmp = jsonl.with_extension("tmp");
        assert!(tmp.exists());

        wal::recover_if_needed(&root, NOW_SEC).unwrap();

        assert!(ids(&root).contains(&eid('A').to_string()));
        assert_eq!(fs::read(&jsonl).unwrap(), before);
        assert!(!tmp.exists());
        assert!(!root.wal_path().exists());
    }

    #[test]
    fn preview_of_missing_id_writes_nothing() {
        let (root, _g) = setup("forget-preview-missing", &[learning('A', "alpha", false)]);
        let before = fs::read(root.episodic_jsonl()).unwrap();

        let report = preview(&root, &eid('Z')).unwrap();

        assert_eq!(
            report,
            ForgetReport {
                removed: false,
                lesson: LessonEffect::Untouched
            }
        );
        assert!(!root.wal_path().exists());
        assert_eq!(fs::read(root.episodic_jsonl()).unwrap(), before);
    }

    #[test]
    fn preview_of_cited_non_exemplar_reports_citation_dropped_and_writes_nothing() {
        let (root, _g) = setup(
            "forget-preview-citation",
            &[learning('A', "alpha", false), learning('B', "beta", false)],
        );
        seed_lessons(&root, 'B', "beta", &['B', 'A']);
        let log_before = fs::read(root.episodic_jsonl()).unwrap();
        let lessons_before = fs::read(root.lessons_md()).unwrap();

        let report = preview(&root, &eid('A')).unwrap();

        assert_eq!(
            report,
            ForgetReport {
                removed: true,
                lesson: LessonEffect::CitationDropped
            }
        );
        assert_eq!(fs::read(root.lessons_md()).unwrap(), lessons_before);
        assert_eq!(fs::read(root.episodic_jsonl()).unwrap(), log_before);
        assert!(!root.wal_path().exists());
    }

    #[test]
    fn preview_of_present_id_while_in_progress_refuses() {
        let (root, _g) = setup(
            "forget-preview-inprogress",
            &[learning('A', "alpha", false)],
        );
        update_daemon_state(&root, |s| {
            s.last_dream_cycle_status = "in_progress".to_string()
        })
        .unwrap();

        let err = preview(&root, &eid('A')).unwrap_err();

        assert!(matches!(err, ForgetError::InProgress), "{err}");
        assert!(!root.wal_path().exists());
    }

    #[test]
    fn render_receipt_keeps_only_this_ids_orphans() {
        use crate::provenance::{EdgeFault, ProvenanceReport};

        let report = ProvenanceReport {
            root: "ab".repeat(32),
            orphans: vec![
                EdgeFault {
                    line: 1,
                    kind: "lesson_citation".to_string(),
                    from: eid('A').to_string(),
                    to: format!("lsn_{}", eid('B')),
                },
                EdgeFault {
                    line: 2,
                    kind: "index_doc".to_string(),
                    from: eid('C').to_string(),
                    to: "idx".to_string(),
                },
            ],
            missing: vec![],
            corrupt_lines: vec![],
            skipped_unknown_kind: 0,
        };

        let text = render_receipt(&eid('A'), LessonEffect::CitationDropped, &report);

        assert!(text.ends_with('\n') && !text.ends_with("\n\n"));
        assert!(!text.contains("\"path\""), "{text}");
        assert!(!text.contains("\"signature\""), "{text}");
        assert!(text.starts_with("{\"schema_version\":\"forget-receipt/1.0\",\"event_id\""));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["schema_version"], FORGET_RECEIPT_SCHEMA);
        assert_eq!(v["event_id"], eid('A').as_str());
        assert_eq!(v["removed"], true);
        assert_eq!(v["lesson"], "citation_dropped");
        assert_eq!(v["root"], report.root.as_str());
        let orphans = v["orphans"].as_array().unwrap();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0]["line"], 1);
        assert_eq!(orphans[0]["kind"], "lesson_citation");
        assert_eq!(orphans[0]["from"], eid('A').as_str());
    }
}
