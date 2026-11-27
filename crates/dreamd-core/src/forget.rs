//! Forget cascade: remove one episodic event, the derived state built from
//! it, and the copies of it dreamd keeps.
//!
//! [`cascade`] plans, stages and commits like a dream cycle but is not one. Its
//! plan ([`crate::plan::plan_forget`]) is the JSONL without the one id, the
//! lesson built from it unlinked (or a deterministic lesson's other citation
//! dropped), and the recurrence sidecar recounted; the commit renames them in
//! and appends ledger edges for the lesson that remains. It never decays other
//! rows, never re-selects a lesson, and never copies the forgotten row into
//! `snapshots/`.
//!
//! The same commit clears the copies dreamd keeps outside the live log
//! ([`crate::copies`]): the decay archive, the migrate backup, and every
//! memory branch object (each autosnap and named branch holds a copy of the
//! log and of `LESSONS.md`). A branch object is rewritten without the event
//! and its refs moved to the new object, rather than dropped, so restore
//! points survive. [`ForgetReport::left`] lists what still holds the id.
//!
//! The JSONL has one writer. `dreamd forget` takes the project writer lock
//! (`WriterLock::acquire`) before it runs [`cascade`] in-process, so no
//! coordinator — a daemon's or an in-process `dreamd mcp`'s — holds the append
//! fd during the rewrite. This module does not touch the index; the CLI
//! refreshes and compacts an existing index afterwards. It does not touch git
//! either: [`crate::copies::git_history`] only counts the commits that hold
//! the id.
//!
//! Existing ledger lines are left in place. A forgotten id's old edge stays in
//! the file, and `provenance::verify` reports it as an orphan.
//!
//! `dreamd forget` is the CLI. It calls [`cascade`] when no daemon is
//! live, or [`preview`] for `--dry-run`. `--proof` writes a forget receipt
//! ([`render_receipt`], `forget-receipt/1.1`). The receipt is unsigned; the
//! signed membership proof in `docs/provenance.md` is still absent.

use std::io;

use dreamd_protocol::EventId;
use serde::Serialize;

use crate::consolidation::ConsolidationError;
use crate::copies::{self, FileKind, GitScan, StoreCopies};
use crate::dream_cycle::{self, DreamCycleError};
use crate::episodic::{self, EpisodicError};
use crate::layout::AgentRoot;
use crate::lessons;
use crate::plan;
use crate::provenance::{Ledger, ProvenanceError};
use crate::retention;
use crate::snapshot::SnapshotError;
use crate::wal::{self, CycleKind, WalError};

/// What [`cascade`] did to `semantic/LESSONS.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LessonEffect {
    /// No lesson file, or the file does not name the id and was not built
    /// from it.
    Untouched,
    /// The lesson is deterministic, and the id was a citation but not the
    /// exemplar (a shape only a hand-edited file has); it was dropped from
    /// `citations` and the file was rewritten.
    CitationDropped,
    /// The id was a `Lesson.id`, so the lesson body is that event's content.
    /// Or the lesson is model-written, and the id was one of its citations or
    /// in its cluster, so the model's prose can hold its text (DRM-114). The
    /// file was unlinked.
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

/// A place in `.agent/` that a forget clears. The order of the variants is
/// the order of every list of places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Place {
    /// `episodic/AGENT_LEARNINGS.jsonl`.
    LiveLog,
    /// `semantic/LESSONS.md`, rewritten or unlinked.
    Lessons,
    /// `semantic/recurrence_counts.json`, recounted without the event. It
    /// holds counts per cluster, never an id or content.
    RecurrenceSidecar,
    /// `.dreamd/snapshots/<date>.jsonl`, where decay archives an event.
    DecayArchive,
    /// `episodic/AGENT_LEARNINGS.jsonl.bak`, written by `dreamd migrate`.
    MigrateBackup,
    /// `.dreamd/branches/objects/`: autosnaps and memory branches.
    BranchObjects,
    /// `.dreamd/index/`. Only the CLI clears it, after the cascade.
    Index,
}

impl Place {
    /// Stable token for CLI output and the forget receipt.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LiveLog => "live_log",
            Self::Lessons => "lessons",
            Self::RecurrenceSidecar => "recurrence_sidecar",
            Self::DecayArchive => "decay_archive",
            Self::MigrateBackup => "migrate_backup",
            Self::BranchObjects => "branch_objects",
            Self::Index => "index",
        }
    }
}

/// Outcome of [`cascade`] or [`preview`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgetReport {
    /// The id was in the live log: [`cascade`] removed it, or [`preview`]
    /// would.
    pub removed: bool,
    pub lesson: LessonEffect,
    /// The places in `.agent/` that [`cascade`] cleared, or that [`preview`]
    /// would clear, in [`Place`] order. Never [`Place::Index`]. Empty when
    /// nothing was written.
    pub cleared: Vec<Place>,
    /// The copies outside the live log that held the id before the forget:
    /// what [`cascade`] rewrote or removed, or what [`preview`] would.
    pub copies: StoreCopies,
    /// The copies that still hold the id: found again after [`cascade`]
    /// committed, or the ones [`preview`] would leave.
    pub left: StoreCopies,
}

impl ForgetReport {
    fn untouched(left: StoreCopies) -> Self {
        Self {
            removed: false,
            lesson: LessonEffect::Untouched,
            cleared: Vec::new(),
            copies: left.clone(),
            left,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ForgetError {
    /// `state.json` says a dream cycle is in progress.
    #[error("dream cycle already in progress")]
    InProgress,
    /// Reading or patching the episodic log failed.
    #[error("episodic log: {0}")]
    Episodic(#[from] EpisodicError),
    /// Opening the stage, staging the plan, or committing the redo WAL failed.
    #[error("WAL: {0}")]
    Wal(#[from] WalError),
    /// `LESSONS.md` could not be read, was not UTF-8, or did not parse.
    #[error("lessons: {0}")]
    Lessons(io::Error),
    /// Serializing the recounted recurrence sidecar failed.
    #[error("recurrence sidecar: {0}")]
    Sidecar(#[from] ConsolidationError),
    /// A provenance ledger operation failed.
    #[error("provenance: {0}")]
    Provenance(#[from] ProvenanceError),
    /// Reading or staging a copy outside the live log (the decay archive, the
    /// migrate backup) failed.
    #[error("copies: {0}")]
    Copies(io::Error),
    /// Reading or writing a memory branch object or ref failed.
    #[error("branches: {0}")]
    Branches(#[from] SnapshotError),
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

/// The places `report` says were (or would be) cleared, given the copies
/// found before and after.
fn cleared_places(
    removed: bool,
    lesson: LessonEffect,
    before: &StoreCopies,
    after: &StoreCopies,
) -> Vec<Place> {
    let mut places = Vec::new();
    if removed {
        places.push(Place::LiveLog);
        if lesson != LessonEffect::Untouched {
            places.push(Place::Lessons);
        }
        places.push(Place::RecurrenceSidecar);
    }
    for (kind, place) in [
        (FileKind::DecayArchive, Place::DecayArchive),
        (FileKind::MigrateBackup, Place::MigrateBackup),
    ] {
        if before.has(kind) && !after.has(kind) {
            places.push(place);
        }
    }
    if before
        .objects
        .iter()
        .any(|o| !after.objects.iter().any(|a| a.id == o.id))
    {
        places.push(Place::BranchObjects);
    }
    places
}

/// Remove `id` from the live episodic log and cascade to the lesson file, the
/// recurrence sidecar, the provenance ledger, and the copies dreamd keeps
/// outside the live log ([`crate::copies`]): the decay archive, the migrate
/// backup, and every memory branch object (autosnaps and named branches).
///
/// An id in neither the live log nor a copy it can rewrite returns
/// `removed: false` with nothing cleared, without opening a stage. A present
/// id is removed even when `pinned` is set. An id found only in copies (for
/// example, one decay already archived) has its copies cleared and
/// `removed: false`. `now_sec` is caller-provided. The wall clock is read
/// only to tell whether an object no ref names is past
/// [`crate::retention::OBJECT_GC_GRACE`].
///
/// Every live file, ref and `HEAD` change commits in one redo WAL. Each
/// rewritten branch object is written as a new object before the commit, and
/// the objects it replaces are removed after it. The copies are then looked
/// for again, and whatever still holds the id is in [`ForgetReport::left`].
///
/// A failure before the commit point removes the stage and sets `state.json`
/// to `failed` before the error returns (BZR-1532). A failure after it is
/// retried once; if the retry fails too, the redo WAL stays, and the next
/// writer to take the project lock rolls it forward. The caller must reopen
/// any append fd it holds on the JSONL, because the commit replaces the file
/// by rename.
///
/// `ledger` is the project writer lock's appender (`WriterLock::ledger`).
pub fn cascade(
    agent_root: &AgentRoot,
    ledger: &Ledger,
    id: &EventId,
    now_sec: i64,
) -> Result<ForgetReport, ForgetError> {
    let jsonl = agent_root.episodic_jsonl();
    let raw = episodic::read_raw(&jsonl)?;
    let in_log = raw
        .iter()
        .filter_map(|line| line.parsed.as_ref())
        .any(|ev| ev.id == *id);
    let before = copies::find(agent_root, id)?;
    if !in_log && copies::would_stay(agent_root, &before) == before {
        return Ok(ForgetReport::untouched(before));
    }

    dream_cycle::ensure_not_in_progress(agent_root)?;
    let (lesson, scrub) = wal::stage_and_commit(
        agent_root,
        CycleKind::Forget,
        now_sec,
        ledger,
        |stage| -> Result<(LessonEffect, copies::Scrub), ForgetError> {
            let mut lesson = LessonEffect::Untouched;
            if in_log {
                // The log was read above; this is the one read of the prior
                // lesson file.
                let prior = stage
                    .read(&agent_root.lessons_md())
                    .map_err(ForgetError::Lessons)?;
                let (plan, effect) = plan::plan_forget(&raw, prior.as_deref(), id, now_sec)?;
                stage
                    .apply_plan(&plan, &dream_cycle::cycle_date_from_now_sec(now_sec))
                    .map_err(WalError::Io)?;
                lesson = effect;
            }
            // The commit appends the plan's ledger edges after the renames.
            let scrub = copies::stage_scrub(stage, &before, id, now_sec)?;
            Ok((lesson, scrub))
        },
    )?;

    // After the commit no ref names a replaced object, so removing it cannot
    // take a restore point with it. A failure here leaves the object on disk,
    // and the search below reports it.
    if let Err(e) = retention::remove_unnamed(agent_root, &scrub.to_remove) {
        tracing::warn!(error = %e, "forget: a branch object that held the event was not removed");
    }
    let left = copies::find(agent_root, id).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "forget: could not look for copies after the commit");
        before.clone()
    });

    Ok(ForgetReport {
        removed: in_log,
        lesson,
        cleared: cleared_places(in_log, lesson, &before, &left),
        copies: before,
        left,
    })
}

/// Report what [`cascade`] would do to `id`, writing nothing.
///
/// Same decision order as `cascade`: an id in neither the live log nor a
/// copy it can rewrite returns `removed: false` without checking the
/// dream-cycle status; otherwise it refuses with [`ForgetError::InProgress`]
/// before reading `LESSONS.md`. Opens no stage. Reads the wall clock only for
/// the grace check that [`cascade`] makes.
pub fn preview(agent_root: &AgentRoot, id: &EventId) -> Result<ForgetReport, ForgetError> {
    let events = episodic::read_all(&agent_root.episodic_jsonl())?;
    let in_log = events.iter().any(|ev| ev.id == *id);
    let before = copies::find(agent_root, id)?;
    if !in_log && copies::would_stay(agent_root, &before) == before {
        return Ok(ForgetReport::untouched(before));
    }

    dream_cycle::ensure_not_in_progress(agent_root)?;

    let lesson = if in_log {
        let skill_action = events
            .iter()
            .find(|ev| ev.id == *id)
            .map(|ev| ev.skill_action.as_str());
        match lessons::read_lessons_file(&agent_root.lessons_md()) {
            Ok(file) => plan::classify_lessons(&file, id.as_str(), skill_action),
            Err(e) if e.kind() == io::ErrorKind::NotFound => LessonEffect::Untouched,
            Err(e) => return Err(ForgetError::Lessons(e)),
        }
    } else {
        LessonEffect::Untouched
    };
    let left = copies::would_stay(agent_root, &before);
    Ok(ForgetReport {
        removed: in_log,
        lesson,
        cleared: cleared_places(in_log, lesson, &before, &left),
        copies: before,
        left,
    })
}

/// Schema token of the file `dreamd forget --proof` writes.
pub const FORGET_RECEIPT_SCHEMA: &str = "forget-receipt/1.1";

/// What happened to the project's search index, which the CLI refreshes and
/// compacts after [`cascade`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexOutcome {
    /// There is no index directory.
    Absent,
    /// The event's document was pruned, and no segment holds a deleted
    /// document's bytes.
    Cleared,
    /// The prune or the re-index failed: the index may still return the event.
    RefreshFailed,
    /// The prune worked, but the merge failed or left a segment holding
    /// deleted documents: their bytes are still in the segment files.
    MergePending,
    /// The merge worked, but some of the files it replaced could not be
    /// removed.
    FilesNotRemoved,
}

/// A place where a copy of the event is still kept after the forget.
/// Serialized into the receipt with a `place` tag. Holds ids, commit ids,
/// paths and counts; never the memory's content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "place", rename_all = "snake_case")]
pub enum Remaining {
    /// Commits in the project's git repository whose dreamd files hold the
    /// id. dreamd never rewrites git history.
    GitHistory {
        commits: usize,
        first: String,
        last: String,
        paths: Vec<String>,
    },
    /// The git repository could not be read, so git history was not checked.
    #[serde(rename = "git_history")]
    GitUnchecked { unchecked: String },
    /// A branch object the forget could not rewrite.
    BranchObject {
        object: String,
        refs: Vec<String>,
        head: bool,
        reason: &'static str,
    },
    /// A decay archive day that still holds the id.
    DecayArchive { file: String },
    /// The migrate backup still holds the id.
    MigrateBackup { file: String },
    /// The search index: `reason` is `refresh_failed`, `merge_pending` or
    /// `files_not_removed`.
    Index { reason: &'static str },
}

/// `path` relative to the project root, `/`-joined.
fn project_relative(agent_root: &AgentRoot, path: &std::path::Path) -> String {
    path.strip_prefix(agent_root.project_root())
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Everything [`cascade`], the CLI's index pass and the git scan found: the
/// places cleared and the places a copy remains, as the CLI prints them and
/// the receipt states them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgetOutcome {
    pub cleared: Vec<Place>,
    pub remaining: Vec<Remaining>,
}

impl ForgetOutcome {
    pub fn new(
        agent_root: &AgentRoot,
        report: &ForgetReport,
        index: IndexOutcome,
        git: &GitScan,
    ) -> Self {
        let mut cleared = report.cleared.clone();
        let mut remaining = Vec::new();
        match git {
            GitScan::Scanned(history) if history.commits > 0 => {
                remaining.push(Remaining::GitHistory {
                    commits: history.commits,
                    first: history.first.clone().unwrap_or_default(),
                    last: history.last.clone().unwrap_or_default(),
                    paths: history.paths.clone(),
                })
            }
            GitScan::Failed(e) => remaining.push(Remaining::GitUnchecked {
                unchecked: e.clone(),
            }),
            GitScan::Scanned(_) | GitScan::NoRepo => {}
        }
        for obj in &report.left.objects {
            remaining.push(Remaining::BranchObject {
                object: obj.id.clone(),
                refs: obj.refs.clone(),
                head: obj.head,
                reason: if obj.id.ends_with(".partial") {
                    "unfinished_write"
                } else if obj.unreadable.is_some() {
                    "failed_hash_check"
                } else {
                    "not_cleared"
                },
            });
        }
        for file in &report.left.files {
            let rel = project_relative(agent_root, &file.path);
            remaining.push(match file.kind {
                FileKind::DecayArchive => Remaining::DecayArchive { file: rel },
                FileKind::MigrateBackup => Remaining::MigrateBackup { file: rel },
            });
        }
        match index {
            IndexOutcome::Absent => {}
            IndexOutcome::Cleared => cleared.push(Place::Index),
            IndexOutcome::RefreshFailed => remaining.push(Remaining::Index {
                reason: "refresh_failed",
            }),
            IndexOutcome::MergePending => remaining.push(Remaining::Index {
                reason: "merge_pending",
            }),
            IndexOutcome::FilesNotRemoved => remaining.push(Remaining::Index {
                reason: "files_not_removed",
            }),
        }
        Self { cleared, remaining }
    }
}

#[derive(Serialize)]
struct Receipt<'a> {
    schema_version: &'static str,
    event_id: &'a str,
    removed: bool,
    lesson: &'static str,
    root: &'a str,
    orphans: Vec<ReceiptOrphan<'a>>,
    cleared: Vec<&'static str>,
    remaining: &'a [Remaining],
}

#[derive(Serialize)]
struct ReceiptOrphan<'a> {
    line: u64,
    kind: &'a str,
    from: &'a str,
    to: &'a str,
}

/// Render the `forget-receipt/1.1` file: one compact JSON object and a
/// trailing `\n`.
///
/// The 1.0 keys come first, with their 1.0 meaning: `schema_version`,
/// `event_id`, `removed`, `lesson`, `root` (the recomputed Merkle root from
/// `provenance::verify`), and `orphans` (the orphan edges whose `from` is
/// `event_id`, in report order). `removed` is `false` only when the id was
/// not in the live log and only copies of it were cleared.
///
/// 1.1 adds `cleared`, the [`Place`] tokens of every place the forget
/// cleared, and `remaining`, every place a copy is still kept
/// ([`Remaining`]). The receipt never holds the memory's content. It is
/// unsigned: it carries no inclusion path and no signature.
pub fn render_receipt(
    event_id: &EventId,
    report: &ForgetReport,
    outcome: &ForgetOutcome,
    ledger: &crate::provenance::ProvenanceReport,
) -> String {
    let id = event_id.as_str();
    let receipt = Receipt {
        schema_version: FORGET_RECEIPT_SCHEMA,
        event_id: id,
        removed: report.removed,
        lesson: report.lesson.as_str(),
        root: &ledger.root,
        orphans: ledger
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
        cleared: outcome.cleared.iter().map(|p| p.as_str()).collect(),
        remaining: &outcome.remaining,
    };
    // Plain strings, integers, and bools: serialization cannot fail.
    let mut out = serde_json::to_string(&receipt).expect("receipt serializes");
    out.push('\n');
    out
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;

    use chrono::DateTime;
    use dreamd_protocol::AgentLearning;

    use crate::daemon_state::update_daemon_state;
    use crate::index::RecurrenceSidecar;
    use crate::lessons::{read_lessons_file, write_lessons_file, Lesson, LessonsFile};
    use crate::test_support::{unique_tmpdir, DirGuard};
    use crate::writer_lock::{WriterKind, WriterLock};

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

    /// A store holding `rows`, and the project writer lock on it. The lock is
    /// taken here, before a test writes any state of its own, so taking it
    /// recovers or resets nothing a test set up.
    fn setup(label: &str, rows: &[AgentLearning]) -> (AgentRoot, WriterLock, DirGuard) {
        let dir = unique_tmpdir(label);
        let root = AgentRoot::new(&dir);
        fs::create_dir_all(root.episodic_dir()).unwrap();
        fs::create_dir_all(root.semantic_dir()).unwrap();
        let bytes: String = rows
            .iter()
            .map(|r| format!("{}\n", serde_json::to_string(r).unwrap()))
            .collect();
        episodic::rewrite_atomic(&root.episodic_jsonl(), bytes.as_bytes()).unwrap();
        let lock = WriterLock::acquire(&root, WriterKind::Forget).unwrap();
        (root, lock, DirGuard(dir))
    }

    fn seed_lessons(root: &AgentRoot, exemplar: char, content: &str, citations: &[char]) {
        let file = LessonsFile {
            last_updated: DateTime::from_timestamp(NOW_SEC - 60, 0).unwrap(),
            prompt_version: crate::consolidation::DETERMINISTIC_PROMPT_ID.to_string(),
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
        let (root, lock, _g) = setup("forget-missing", &[learning('A', "alpha", false)]);
        let before = fs::read(root.episodic_jsonl()).unwrap();

        let report = cascade(&root, lock.ledger(), &eid('Z'), NOW_SEC).unwrap();

        assert_eq!(
            (report.removed, report.lesson),
            (false, LessonEffect::Untouched)
        );
        assert!(!root.wal_path().exists());
        assert!(!root.state_json().exists(), "begin_cycle must not run");
        assert_eq!(fs::read(root.episodic_jsonl()).unwrap(), before);
    }

    #[test]
    fn present_id_is_removed_and_neighbor_kept() {
        let (root, lock, _g) = setup(
            "forget-present",
            &[learning('A', "alpha", false), learning('B', "beta", false)],
        );

        let report = cascade(&root, lock.ledger(), &eid('A'), NOW_SEC).unwrap();

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
        let (root, lock, _g) = setup(
            "forget-pinned",
            &[learning('A', "alpha", true), learning('B', "beta", false)],
        );

        let report = cascade(&root, lock.ledger(), &eid('A'), NOW_SEC).unwrap();

        assert!(report.removed);
        assert_eq!(ids(&root), vec![eid('B').to_string()]);
    }

    #[test]
    fn last_row_removed_writes_empty_sidecar() {
        let (root, lock, _g) = setup("forget-last", &[learning('A', "alpha", false)]);
        let sidecar = root.semantic_dir().join("recurrence_counts.json");
        fs::write(
            &sidecar,
            r#"{"schema_version":"1.0","clusters":[{"skill_action":"rust::forget","count":3}]}"#,
        )
        .unwrap();

        cascade(&root, lock.ledger(), &eid('A'), NOW_SEC).unwrap();

        assert!(ids(&root).is_empty());
        let parsed: RecurrenceSidecar =
            serde_json::from_str(&fs::read_to_string(&sidecar).unwrap()).unwrap();
        assert_eq!(parsed.schema_version, "1.0");
        assert!(parsed.clusters.is_empty());
    }

    #[test]
    fn citation_that_is_not_the_exemplar_is_dropped() {
        let (root, lock, _g) = setup(
            "forget-citation",
            &[
                learning('A', "alpha", false),
                learning('B', "beta body", false),
            ],
        );
        seed_lessons(&root, 'B', "beta body", &['B', 'A']);

        let report = cascade(&root, lock.ledger(), &eid('A'), NOW_SEC).unwrap();

        assert_eq!(report.lesson, LessonEffect::CitationDropped);
        let file = read_lessons_file(&root.lessons_md()).unwrap();
        assert_eq!(file.citations, vec![eid('B').to_string()]);
        assert_eq!(file.lessons.len(), 1);
        assert!(file.lessons[0].content.contains("beta body"));
        assert_eq!(file.last_updated.timestamp(), NOW_SEC);
    }

    #[test]
    fn exemplar_match_unlinks_lessons_md() {
        let (root, lock, _g) = setup(
            "forget-exemplar",
            &[learning('A', "alpha", false), learning('B', "beta", false)],
        );
        seed_lessons(&root, 'A', "alpha", &['A']);

        let report = cascade(&root, lock.ledger(), &eid('A'), NOW_SEC).unwrap();

        assert_eq!(report.lesson, LessonEffect::Unlinked);
        assert!(!root.lessons_md().exists());
    }

    /// DRM-114 test 3: `preview` and `cascade` agree that a model-written
    /// lesson goes with any event of its cluster, cited or not.
    #[test]
    fn preview_and_cascade_agree_on_a_model_lesson() {
        let (root, lock, _g) = setup(
            "forget-model-lesson",
            &[
                learning('A', "alpha", false),
                learning('B', "beta", false),
                learning('C', "gamma, which the model saw", false),
            ],
        );
        let file = LessonsFile {
            last_updated: DateTime::from_timestamp(NOW_SEC - 60, 0).unwrap(),
            prompt_version: "lesson-v1".to_string(),
            cluster_key: CLUSTER.to_string(),
            citations: vec![eid('B').to_string(), eid('A').to_string()],
            lessons: vec![Lesson {
                id: eid('B').to_string(),
                content: "prose over alpha, beta and gamma".to_string(),
                pinned: false,
            }],
        };
        write_lessons_file(&root.lessons_md(), &file).unwrap();
        let log_before = fs::read(root.episodic_jsonl()).unwrap();
        let lessons_before = fs::read(root.lessons_md()).unwrap();

        for id in ['A', 'C'] {
            let report = preview(&root, &eid(id)).unwrap();
            assert_eq!(
                (report.removed, report.lesson),
                (true, LessonEffect::Unlinked),
                "preview of {id}"
            );
            assert_eq!(fs::read(root.lessons_md()).unwrap(), lessons_before);
            assert_eq!(fs::read(root.episodic_jsonl()).unwrap(), log_before);
            assert!(!root.wal_path().exists());
        }

        let report = cascade(&root, lock.ledger(), &eid('C'), NOW_SEC).unwrap();
        assert_eq!(report.lesson, LessonEffect::Unlinked);
        assert!(!root.lessons_md().exists(), "LESSONS.md is gone");
    }

    #[test]
    fn existing_ledger_line_for_removed_id_is_kept() {
        let (root, lock, _g) = setup(
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

        cascade(&root, lock.ledger(), &eid('A'), NOW_SEC).unwrap();

        let ledger = fs::read_to_string(root.provenance_ledger()).unwrap();
        assert!(
            ledger.starts_with(&line),
            "old line must stay first: {ledger}"
        );
    }

    #[test]
    fn in_progress_status_refuses_before_any_rewrite() {
        let (root, lock, _g) = setup(
            "forget-inprogress",
            &[learning('A', "alpha", false), learning('B', "beta", false)],
        );
        update_daemon_state(&root, |s| {
            s.last_dream_cycle_status = "in_progress".to_string()
        })
        .unwrap();
        let before = fs::read(root.episodic_jsonl()).unwrap();

        let err = cascade(&root, lock.ledger(), &eid('A'), NOW_SEC).unwrap_err();

        assert!(matches!(err, ForgetError::InProgress), "{err}");
        assert_eq!(fs::read(root.episodic_jsonl()).unwrap(), before);
        assert!(!root.wal_path().exists());
    }

    #[test]
    fn preview_of_missing_id_writes_nothing() {
        let (root, _lock, _g) = setup("forget-preview-missing", &[learning('A', "alpha", false)]);
        let before = fs::read(root.episodic_jsonl()).unwrap();

        let report = preview(&root, &eid('Z')).unwrap();

        assert_eq!(
            (report.removed, report.lesson),
            (false, LessonEffect::Untouched)
        );
        assert!(!root.wal_path().exists());
        assert_eq!(fs::read(root.episodic_jsonl()).unwrap(), before);
    }

    #[test]
    fn preview_of_cited_non_exemplar_reports_citation_dropped_and_writes_nothing() {
        let (root, _lock, _g) = setup(
            "forget-preview-citation",
            &[learning('A', "alpha", false), learning('B', "beta", false)],
        );
        seed_lessons(&root, 'B', "beta", &['B', 'A']);
        let log_before = fs::read(root.episodic_jsonl()).unwrap();
        let lessons_before = fs::read(root.lessons_md()).unwrap();

        let report = preview(&root, &eid('A')).unwrap();

        assert_eq!(
            (report.removed, report.lesson),
            (true, LessonEffect::CitationDropped)
        );
        assert_eq!(fs::read(root.lessons_md()).unwrap(), lessons_before);
        assert_eq!(fs::read(root.episodic_jsonl()).unwrap(), log_before);
        assert!(!root.wal_path().exists());
    }

    #[test]
    fn preview_of_present_id_while_in_progress_refuses() {
        let (root, _lock, _g) = setup(
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
        use crate::copies::GitHistory;
        use crate::provenance::{EdgeFault, ProvenanceReport};

        let ledger = ProvenanceReport {
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
        let report = ForgetReport {
            removed: true,
            lesson: LessonEffect::CitationDropped,
            cleared: vec![Place::LiveLog, Place::Lessons, Place::RecurrenceSidecar],
            copies: StoreCopies::default(),
            left: StoreCopies::default(),
        };
        let root = AgentRoot::new(std::path::Path::new("/p"));
        let outcome = ForgetOutcome::new(&root, &report, IndexOutcome::Absent, &GitScan::NoRepo);

        let text = render_receipt(&eid('A'), &report, &outcome, &ledger);

        assert!(text.ends_with('\n') && !text.ends_with("\n\n"));
        assert!(!text.contains("\"signature\""), "{text}");
        assert!(text.starts_with("{\"schema_version\":\"forget-receipt/1.1\",\"event_id\""));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["schema_version"], FORGET_RECEIPT_SCHEMA);
        assert_eq!(v["event_id"], eid('A').as_str());
        assert_eq!(v["removed"], true);
        assert_eq!(v["lesson"], "citation_dropped");
        assert_eq!(v["root"], ledger.root.as_str());
        let orphans = v["orphans"].as_array().unwrap();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0]["line"], 1);
        assert_eq!(orphans[0]["kind"], "lesson_citation");
        assert_eq!(orphans[0]["from"], eid('A').as_str());
        assert_eq!(
            v["cleared"],
            serde_json::json!(["live_log", "lessons", "recurrence_sidecar"])
        );
        assert_eq!(v["remaining"], serde_json::json!([]));

        // With an index and a git history that still holds the id.
        let git = GitScan::Scanned(GitHistory {
            commits: 2,
            first: Some("1".repeat(40)),
            last: Some("2".repeat(40)),
            paths: vec![".agent/episodic/AGENT_LEARNINGS.jsonl".to_string()],
            workdir: std::path::PathBuf::from("/home/someone/project"),
        });
        let outcome = ForgetOutcome::new(&root, &report, IndexOutcome::Cleared, &git);
        let v: serde_json::Value =
            serde_json::from_str(&render_receipt(&eid('A'), &report, &outcome, &ledger)).unwrap();
        assert_eq!(
            v["cleared"],
            serde_json::json!(["live_log", "lessons", "recurrence_sidecar", "index"])
        );
        assert!(!text_of(&v).contains("someone"), "the workdir stays out");
        assert_eq!(
            v["remaining"],
            serde_json::json!([{
                "place": "git_history",
                "commits": 2,
                "first": "1".repeat(40),
                "last": "2".repeat(40),
                "paths": [".agent/episodic/AGENT_LEARNINGS.jsonl"],
            }])
        );
    }

    fn text_of(v: &serde_json::Value) -> String {
        v.to_string()
    }

    /// The 1.0 keys keep their order and come before the 1.1 keys.
    #[test]
    fn receipt_1_1_keeps_the_1_0_keys_first() {
        use crate::provenance::ProvenanceReport;

        let ledger = ProvenanceReport {
            root: "cd".repeat(32),
            orphans: vec![],
            missing: vec![],
            corrupt_lines: vec![],
            skipped_unknown_kind: 0,
        };
        let report = ForgetReport::untouched(StoreCopies::default());
        let root = AgentRoot::new(std::path::Path::new("/p"));
        let outcome = ForgetOutcome::new(
            &root,
            &report,
            IndexOutcome::MergePending,
            &GitScan::Failed("no".to_string()),
        );

        let text = render_receipt(&eid('A'), &report, &outcome, &ledger);

        let keys: Vec<&str> = [
            "schema_version",
            "event_id",
            "removed",
            "lesson",
            "root",
            "orphans",
            "cleared",
            "remaining",
        ]
        .into_iter()
        .collect();
        let mut at = 0;
        for key in keys {
            let pos = text[at..]
                .find(&format!("\"{key}\":"))
                .unwrap_or_else(|| panic!("{key} missing or out of order: {text}"));
            at += pos;
        }
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            v["remaining"],
            serde_json::json!([
                {"place": "git_history", "unchecked": "no"},
                {"place": "index", "reason": "merge_pending"},
            ])
        );
    }

    /// A store whose live log holds A (with `MARK`) and B, `LESSONS.md` built
    /// from A, and one autosnap taken of it.
    fn store_with_autosnap(label: &str) -> (AgentRoot, WriterLock, DirGuard, String) {
        let (root, lock, g) = setup(
            label,
            &[learning('A', MARK, false), learning('B', "beta", false)],
        );
        seed_lessons(&root, 'A', MARK, &['A']);
        let snap = crate::snapshot::create_autosnap(&root, NOW_SEC - 120).unwrap();
        (root, lock, g, snap)
    }

    const MARK: &str = "forgetmarkerz8";

    fn files_holding(dir: &std::path::Path, needle: &str, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files_holding(&path, needle, out);
            } else if copies::holds(&fs::read(&path).unwrap(), needle.as_bytes()) {
                out.push(path);
            }
        }
    }

    fn holding(root: &AgentRoot, needle: &str) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        files_holding(&root.agent_dir(), needle, &mut out);
        out.retain(|p| !p.starts_with(root.dreamd_dir().join("provenance")));
        out
    }

    #[test]
    fn cascade_clears_the_autosnap_that_holds_the_event() {
        let (root, lock, _g, snap) = store_with_autosnap("forget-autosnap");

        let report = cascade(&root, lock.ledger(), &eid('A'), NOW_SEC).unwrap();

        assert!(report.removed);
        assert_eq!(
            report.cleared,
            vec![
                Place::LiveLog,
                Place::Lessons,
                Place::RecurrenceSidecar,
                Place::BranchObjects
            ]
        );
        assert_eq!(report.copies.objects.len(), 1);
        assert_eq!(report.copies.objects[0].id, snap);
        assert!(report.left.is_empty(), "{:?}", report.left);
        assert!(
            holding(&root, MARK).is_empty(),
            "{:?}",
            holding(&root, MARK)
        );
        assert!(holding(&root, eid('A').as_str()).is_empty());
        assert!(!root.branches_objects_dir().join(&snap).exists());
        // The autosnap ref still exists and names an object that keeps B.
        let refs = crate::snapshot::list_branches(&root).unwrap();
        assert_eq!(refs.len(), 1, "{refs:?}");
        assert!(!holding(&root, eid('B').as_str()).is_empty());
        assert!(!root.wal_path().exists());
    }

    #[test]
    fn an_id_only_in_the_decay_archive_is_cleared_there() {
        let (root, lock, _g) = setup("forget-archived", &[learning('B', "beta", false)]);
        fs::create_dir_all(root.snapshots_dir()).unwrap();
        let archived = format!(
            "{}\n",
            serde_json::to_string(&learning('A', MARK, false)).unwrap()
        );
        fs::write(root.snapshot_file("2025-06-01"), archived).unwrap();

        let report = cascade(&root, lock.ledger(), &eid('A'), NOW_SEC).unwrap();

        assert!(!report.removed);
        assert_eq!(report.cleared, vec![Place::DecayArchive]);
        assert!(holding(&root, MARK).is_empty());
        assert_eq!(ids(&root), vec![eid('B').to_string()]);
    }

    #[test]
    fn preview_lists_the_copies_and_writes_nothing() {
        let (root, _lock, _g, snap) = store_with_autosnap("forget-preview-copies");
        let before = holding(&root, MARK);
        let objects_before: Vec<_> = fs::read_dir(root.branches_objects_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();

        let report = preview(&root, &eid('A')).unwrap();

        assert!(report.removed);
        assert_eq!(
            report.cleared,
            vec![
                Place::LiveLog,
                Place::Lessons,
                Place::RecurrenceSidecar,
                Place::BranchObjects
            ]
        );
        assert_eq!(report.copies.objects[0].id, snap);
        assert!(report.left.is_empty());
        assert_eq!(holding(&root, MARK), before);
        let objects_after: Vec<_> = fs::read_dir(root.branches_objects_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(objects_after, objects_before);
        assert!(!root.wal_path().exists());
    }

    /// BZR-1234: the forget rewrite keeps every other line's bytes, including
    /// unknown members and lines the reader skips.
    #[test]
    fn cascade_keeps_unknown_fields_and_skipped_lines() {
        const SKIPPED: &str = "{\"not\":\"a learning\"}\n";
        let (root, lock, _g) = setup("forget-bytes", &[]);
        let a_json = serde_json::to_string(&learning('A', "alpha", false)).unwrap();
        let a_line = format!("{{\"x_note\":\"keep\",{}\n", &a_json[1..]);
        let b_line = format!(
            "{}\n",
            serde_json::to_string(&learning('B', "beta", false)).unwrap()
        );
        fs::write(root.episodic_jsonl(), format!("{a_line}{SKIPPED}{b_line}")).unwrap();

        let report = cascade(&root, lock.ledger(), &eid('B'), NOW_SEC).unwrap();

        assert!(report.removed);
        assert_eq!(
            fs::read_to_string(root.episodic_jsonl()).unwrap(),
            format!("{a_line}{SKIPPED}")
        );
    }
}
