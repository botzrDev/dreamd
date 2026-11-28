//! Rescan after `dreamd forget`: read every file dreamd keeps under
//! `.agent/` and report each one that still holds the forgotten event
//! (DMD-50).
//!
//! [`crate::forget::cascade`] and the CLI's index pass clear the places
//! they know about, and [`crate::copies::find`] looks for the id again in the
//! copies. This module checks the result independently of that plan. It
//! reads every regular file under `.agent/`, not only the ones the forget
//! wrote, and every stored document of every index segment, deleted ones
//! included. A file counts as clear only when the rescan read it and found
//! neither the event id nor the event's text.
//!
//! The text is the record's `content`, captured before the forget by
//! [`capture`] and held in memory only. It is matched as raw bytes and in
//! its JSON-escaped form. Text shorter than [`MIN_TEXT_BYTES`] is not
//! matched, because a short phrase matches unrelated files; the result says
//! so, and only the id is checked.
//!
//! What this does not do:
//!
//! - it does not read git history (see [`crate::copies::git_history`]),
//!   clones, remotes, MCP client transcripts, model providers, backups, the
//!   daemon home, or freed disk blocks. [`NOT_CHECKED`] names them, and the
//!   receipt lists them every time;
//! - it does not find a paraphrase. A model-written lesson is unlinked by the
//!   forget when its cluster held the event, but text an agent reworded into
//!   another file is not found;
//! - it does not stop the event from coming back. A `git checkout` or a merge
//!   of an older commit can restore the line; nothing records that the id
//!   was forgotten.
//!
//! The provenance ledger keeps the id in its old edges on purpose, so the
//! forget stays traceable. An id found there is listed under
//! [`Verification::kept_by_design`], not as a copy. The ledger's text is
//! still checked: it never holds content, and a match there is a finding.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use dreamd_protocol::EventId;
use serde::Serialize;
use tantivy::index::SegmentReader;
use tantivy::Document;
use tantivy::{Index, TantivyDocument};

use crate::copies::{self, holds};
use crate::episodic;
use crate::layout::AgentRoot;
use crate::server::INDEX_DIR_NAME;

/// Text shorter than this many bytes is not matched, only the id.
pub const MIN_TEXT_BYTES: usize = 16;

/// Places the rescan does not read, in the order the receipt lists them.
pub const NOT_CHECKED: &[&str] = &[
    "git_history_contents",
    "git_reflogs_and_unreachable_objects",
    "git_remotes_and_clones",
    "mcp_client_transcripts",
    "llm_providers",
    "daemon_home",
    "backups_and_sync",
    "freed_disk_blocks",
];

/// Where under `.agent/` a file sits. The order of the variants is the order
/// of every list of findings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Area {
    /// `episodic/AGENT_LEARNINGS.jsonl`.
    LiveLog,
    /// `semantic/LESSONS.md`.
    Lessons,
    /// `semantic/recurrence_counts.json`.
    RecurrenceSidecar,
    /// `.dreamd/snapshots/`, the decay archive.
    DecayArchive,
    /// `episodic/AGENT_LEARNINGS.jsonl.bak`.
    MigrateBackup,
    /// `.dreamd/branches/`: objects, refs and `HEAD`.
    BranchObjects,
    /// `.dreamd/index/`: segment files and their stored documents.
    Index,
    /// `.dreamd/stage/` and the redo WAL, left by a crash.
    StageAndWal,
    /// `.dreamd/provenance/`.
    Ledger,
    /// `working/`, `personal/`, `skills/`, `protocols/`: files a person or an
    /// agent writes. The forget never edits them; the rescan reports them.
    AgentFiles,
    /// Any other file under `.agent/`, such as `.dreamd/agent.log`.
    Other,
}

impl Area {
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
            Self::StageAndWal => "stage_and_wal",
            Self::Ledger => "ledger",
            Self::AgentFiles => "agent_files",
            Self::Other => "other",
        }
    }
}

/// What the rescan found in a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Match {
    /// The file's bytes hold the event id.
    Id,
    /// The file's bytes hold the event's text, raw or JSON-escaped.
    Text,
    /// A stored index document, live or deleted, holds the id or the text.
    IndexDocument,
    /// A segment file that no searchable segment uses. Its stored documents
    /// cannot be read, so it is reported rather than counted as clear.
    UnreferencedSegmentFile,
    /// The file or the index could not be read, so it was not checked.
    Unreadable,
}

impl Match {
    /// Stable token for CLI output and the forget receipt.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Text => "text",
            Self::IndexDocument => "index_document",
            Self::UnreferencedSegmentFile => "unreferenced_segment_file",
            Self::Unreadable => "unreadable",
        }
    }
}

/// One file the rescan did not find clear. `path` is relative to the
/// project root and `/`-joined. Holds no content.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Finding {
    pub area: Area,
    pub path: String,
    #[serde(rename = "match")]
    pub matched: Match,
}

/// Whether the rescan also matched the event's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TextCheck {
    /// The text was matched, raw and JSON-escaped.
    Checked,
    /// The text is shorter than [`MIN_TEXT_BYTES`]; only the id was matched.
    TooShort,
    /// No copy of the record was found before the forget, so there was no
    /// text to match; only the id was matched.
    NotCaptured,
}

impl TextCheck {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Checked => "checked",
            Self::TooShort => "too_short",
            Self::NotCaptured => "not_captured",
        }
    }
}

/// The overall result. Git history and the places in [`NOT_CHECKED`] never
/// change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The rescan read every file under `.agent/` and every stored index
    /// document and found no copy, apart from the ledger's ids.
    CompleteInStore,
    /// At least one file under `.agent/` still holds the id or the text, or
    /// could not be read.
    Partial,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CompleteInStore => "complete_in_store",
            Self::Partial => "partial",
        }
    }
}

/// What [`rescan`] read and found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verification {
    pub status: Status,
    /// Regular files read under `.agent/`.
    pub files_read: usize,
    /// Stored index documents read, deleted ones included. `None` when there
    /// is no index directory.
    pub index_documents_read: Option<usize>,
    pub text: TextCheck,
    /// Every file that still holds the id or the text, or was not checked, in
    /// [`Area`] and path order.
    pub still_present: Vec<Finding>,
    /// Ledger files that hold the id. The ledger keeps ids, never text, so
    /// the forget stays traceable.
    pub kept_by_design: Vec<Finding>,
}

/// The record's content, from the first copy that holds `id`: the live log,
/// then the decay archive, the migrate backup, then the branch objects.
/// Call it before the forget. Reads only; returns `None` when no copy holds
/// a record with that id.
pub fn capture(agent_root: &AgentRoot, id: &EventId) -> Option<String> {
    let from = |bytes: &[u8]| {
        episodic::raw_lines(bytes)
            .into_iter()
            .filter_map(|l| l.parsed)
            .find(|ev| ev.id == *id)
            .map(|ev| ev.content)
    };
    let mut jsonl: Vec<PathBuf> = vec![agent_root.episodic_jsonl()];
    if let Ok(entries) = fs::read_dir(agent_root.snapshots_dir()) {
        let mut days: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect();
        days.sort();
        jsonl.extend(days);
    }
    jsonl.push(copies::migrate_backup_path(agent_root));
    if let Ok(entries) = fs::read_dir(agent_root.branches_objects_dir()) {
        let mut objects: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path().join("episodic/AGENT_LEARNINGS.jsonl"))
            .collect();
        objects.sort();
        jsonl.extend(objects);
    }
    jsonl
        .iter()
        .filter_map(|p| fs::read(p).ok())
        .find_map(|bytes| from(&bytes))
}

/// The byte strings the rescan looks for.
struct Needles {
    id: Vec<u8>,
    text: Vec<Vec<u8>>,
    check: TextCheck,
}

impl Needles {
    fn new(id: &EventId, content: Option<&str>) -> Self {
        let (text, check) = match content {
            None => (Vec::new(), TextCheck::NotCaptured),
            Some(c) if c.len() < MIN_TEXT_BYTES => (Vec::new(), TextCheck::TooShort),
            Some(c) => {
                let mut text = vec![c.as_bytes().to_vec()];
                // JSON-escaped, without the quotes: how a JSONL line or a
                // stored document serialized as JSON holds it.
                let escaped = serde_json::to_string(c).expect("a string serializes");
                let escaped = escaped.as_bytes()[1..escaped.len() - 1].to_vec();
                if escaped != text[0] {
                    text.push(escaped);
                }
                (text, TextCheck::Checked)
            }
        };
        Self {
            id: id.as_str().as_bytes().to_vec(),
            text,
            check,
        }
    }

    fn has_id(&self, bytes: &[u8]) -> bool {
        holds(bytes, &self.id)
    }

    fn has_text(&self, bytes: &[u8]) -> bool {
        self.text.iter().any(|t| holds(bytes, t))
    }
}

/// The area of `path`, relative to `.agent/`.
fn area_of(rel: &Path) -> Area {
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let part = |i: usize| parts.get(i).map(String::as_str);
    match (part(0), part(1)) {
        (Some("episodic"), Some("AGENT_LEARNINGS.jsonl")) if parts.len() == 2 => Area::LiveLog,
        (Some("episodic"), Some("AGENT_LEARNINGS.jsonl.bak")) if parts.len() == 2 => {
            Area::MigrateBackup
        }
        (Some("semantic"), Some("LESSONS.md")) if parts.len() == 2 => Area::Lessons,
        (Some("semantic"), Some("recurrence_counts.json")) if parts.len() == 2 => {
            Area::RecurrenceSidecar
        }
        (Some("working" | "personal" | "skills" | "protocols"), _) => Area::AgentFiles,
        (Some(".dreamd"), Some("snapshots")) => Area::DecayArchive,
        (Some(".dreamd"), Some("branches")) => Area::BranchObjects,
        (Some(".dreamd"), Some(INDEX_DIR_NAME)) => Area::Index,
        (Some(".dreamd"), Some("stage" | "dream_in_progress.wal")) => Area::StageAndWal,
        (Some(".dreamd"), Some("provenance")) => Area::Ledger,
        _ => Area::Other,
    }
}

/// Every regular file under `dir`, in path order. Symlinks are not
/// followed: a link out of `.agent/` points at a file dreamd does not keep.
fn walk(dir: &Path, out: &mut Vec<PathBuf>, unreadable: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return,
        Err(_) => {
            unreadable.push(dir.to_path_buf());
            return;
        }
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        match fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => walk(&path, out, unreadable),
            Ok(m) if m.is_file() => out.push(path),
            Ok(_) => {}
            Err(_) => unreadable.push(path),
        }
    }
}

/// `path` relative to the project root, `/`-joined.
fn relative(agent_root: &AgentRoot, path: &Path) -> String {
    path.strip_prefix(agent_root.project_root())
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// What reading the index's stored documents found.
struct IndexRead {
    documents: usize,
    /// Segment ids (`uuid_string`) of the documents that held a needle.
    holding: BTreeSet<String>,
    /// Every searchable segment's id.
    segments: BTreeSet<String>,
}

/// Read every stored document of every searchable segment, deleted ones
/// included, and note the segments whose documents hold the id or the text.
fn read_index(dir: &Path, needles: &Needles) -> tantivy::Result<IndexRead> {
    let index = Index::open_in_dir(dir)?;
    let schema = index.schema();
    let mut read = IndexRead {
        documents: 0,
        holding: BTreeSet::new(),
        segments: BTreeSet::new(),
    };
    for segment in index.searchable_segments()? {
        let seg_id = segment.id().uuid_string();
        read.segments.insert(seg_id.clone());
        let reader = SegmentReader::open(&segment)?;
        let store = reader.get_store_reader(1)?;
        // `None`: every document in the store, deleted ones included.
        for doc in store.iter::<TantivyDocument>(None) {
            let json = doc?.to_json(&schema);
            read.documents += 1;
            if needles.has_id(json.as_bytes()) || needles.has_text(json.as_bytes()) {
                read.holding.insert(seg_id.clone());
            }
        }
    }
    Ok(read)
}

/// The segment id a Tantivy file name starts with: 32 lowercase hex digits
/// before the first `.`.
fn segment_of(file_name: &str) -> Option<&str> {
    let (stem, _) = file_name.split_once('.')?;
    (stem.len() == 32 && stem.bytes().all(|b| b.is_ascii_hexdigit())).then_some(stem)
}

/// Rescan the store for `id` after a forget. `content` is what [`capture`]
/// returned before it. Reads only. Takes no lock: the caller holds the
/// project writer lock, and has shut down every index writer it opened.
pub fn rescan(agent_root: &AgentRoot, id: &EventId, content: Option<&str>) -> Verification {
    let needles = Needles::new(id, content);
    let agent_dir = agent_root.agent_dir();
    let mut files = Vec::new();
    let mut unreadable = Vec::new();
    walk(&agent_dir, &mut files, &mut unreadable);

    let mut still_present = BTreeSet::new();
    let mut kept_by_design = BTreeSet::new();
    let finding = |path: &Path, matched: Match| Finding {
        area: area_of(path.strip_prefix(&agent_dir).unwrap_or(path)),
        path: relative(agent_root, path),
        matched,
    };
    for path in &unreadable {
        still_present.insert(finding(path, Match::Unreadable));
    }

    let mut files_read = 0;
    let mut index_files = Vec::new();
    for path in &files {
        let f = finding(path, Match::Id);
        if f.area == Area::Index {
            index_files.push(path.clone());
        }
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(_) => {
                still_present.insert(Finding {
                    matched: Match::Unreadable,
                    ..f
                });
                continue;
            }
        };
        files_read += 1;
        if needles.has_text(&bytes) {
            still_present.insert(Finding {
                matched: Match::Text,
                ..f.clone()
            });
        }
        if needles.has_id(&bytes) {
            if f.area == Area::Ledger {
                kept_by_design.insert(f);
            } else {
                still_present.insert(f);
            }
        }
    }

    // Segment files are compressed, so their bytes alone do not show what
    // they hold: read the stored documents too.
    let index_dir = agent_root.dreamd_dir().join(INDEX_DIR_NAME);
    let index_documents_read = if index_dir.is_dir() {
        match read_index(&index_dir, &needles) {
            Ok(read) => {
                for path in &index_files {
                    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    let Some(seg) = segment_of(name) else {
                        continue;
                    };
                    if read.holding.contains(seg) && name.ends_with(".store") {
                        still_present.insert(finding(path, Match::IndexDocument));
                    } else if !read.segments.contains(seg) {
                        still_present.insert(finding(path, Match::UnreferencedSegmentFile));
                    }
                }
                Some(read.documents)
            }
            Err(e) => {
                tracing::warn!(error = %e, "forget: could not read the index's stored documents");
                still_present.insert(finding(&index_dir, Match::Unreadable));
                Some(0)
            }
        }
    } else {
        None
    };

    let still_present: Vec<Finding> = still_present.into_iter().collect();
    Verification {
        status: if still_present.is_empty() {
            Status::CompleteInStore
        } else {
            Status::Partial
        },
        files_read,
        index_documents_read,
        text: needles.check,
        still_present,
        kept_by_design: kept_by_design.into_iter().collect(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    use chrono::DateTime;
    use dreamd_protocol::AgentLearning;

    use crate::test_support::{unique_tmpdir, DirGuard};

    const NOW_SEC: i64 = 1_751_500_800;
    const TEXT: &str = "verified removal marker \"quoted\" text";

    fn eid(suffix: char) -> EventId {
        EventId::parse(&format!("evt_01ARZ3NDEKTSV4RRFFQ69G5FA{suffix}")).unwrap()
    }

    fn learning(suffix: char, content: &str) -> AgentLearning {
        AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: eid(suffix),
            timestamp: DateTime::from_timestamp(NOW_SEC, 0).unwrap(),
            pain: 6.0,
            importance: 7.0,
            pinned: false,
            skill_action: "rust::forget".to_string(),
            source_harness: "test-harness".to_string(),
            content: content.to_string(),
        }
    }

    fn line(ev: &AgentLearning) -> String {
        format!("{}\n", serde_json::to_string(ev).unwrap())
    }

    fn store(label: &str) -> (AgentRoot, DirGuard) {
        let dir = unique_tmpdir(label);
        let guard = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);
        for sub in root.subdirs() {
            fs::create_dir_all(sub).unwrap();
        }
        fs::create_dir_all(root.dreamd_dir()).unwrap();
        (root, guard)
    }

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn a_clean_store_is_complete_and_lists_ledger_ids_as_kept() {
        let (root, _g) = store("verify-clean");
        write(
            &root.episodic_jsonl(),
            &line(&learning('B', "another memory entirely")),
        );
        write(
            &root.provenance_ledger(),
            &format!(
                "{{\"kind\":\"index_doc\",\"from\":\"{0}\",\"to\":\"{0}\"}}\n",
                eid('A')
            ),
        );
        let v = rescan(&root, &eid('A'), Some(TEXT));
        assert_eq!(v.status, Status::CompleteInStore, "{v:?}");
        assert_eq!(v.text, TextCheck::Checked);
        assert!(v.still_present.is_empty());
        assert_eq!(v.kept_by_design.len(), 1);
        assert_eq!(v.kept_by_design[0].area, Area::Ledger);
        assert_eq!(v.index_documents_read, None);
        assert!(v.files_read >= 2);
    }

    /// One copy seeded in every area the rescan reads, each found and
    /// reported under its own area.
    #[test]
    fn every_seeded_copy_is_reported_in_its_area() {
        let (root, _g) = store("verify-every");
        let ev = learning('A', TEXT);
        let escaped_line = line(&ev);
        let dreamd = root.dreamd_dir();
        let seeds: Vec<(PathBuf, String, Area, Match)> = vec![
            (
                root.episodic_jsonl(),
                escaped_line.clone(),
                Area::LiveLog,
                Match::Id,
            ),
            (
                root.lessons_md(),
                format!("# Lessons\n\n{TEXT}\n"),
                Area::Lessons,
                Match::Text,
            ),
            (
                root.snapshot_file("2025-06-01"),
                escaped_line.clone(),
                Area::DecayArchive,
                Match::Id,
            ),
            (
                copies::migrate_backup_path(&root),
                escaped_line.clone(),
                Area::MigrateBackup,
                Match::Id,
            ),
            (
                root.branches_objects_dir()
                    .join("0123abcd/episodic/AGENT_LEARNINGS.jsonl"),
                escaped_line.clone(),
                Area::BranchObjects,
                Match::Id,
            ),
            (
                root.stage_dir().join("episodic/AGENT_LEARNINGS.jsonl"),
                escaped_line.clone(),
                Area::StageAndWal,
                Match::Id,
            ),
            (
                root.workspace_md(),
                format!("notes: {TEXT}\n"),
                Area::AgentFiles,
                Match::Text,
            ),
            (
                dreamd.join("agent.log"),
                format!("append {}\n", eid('A')),
                Area::Other,
                Match::Id,
            ),
            (
                root.provenance_ledger(),
                format!("{{\"leaked\":\"{TEXT}\"}}\n"),
                Area::Ledger,
                Match::Text,
            ),
        ];
        for (path, body, _, _) in &seeds {
            write(path, body);
        }
        let v = rescan(&root, &eid('A'), Some(TEXT));
        assert_eq!(v.status, Status::Partial);
        for (path, _, area, matched) in &seeds {
            let rel = relative(&root, path);
            assert!(
                v.still_present
                    .iter()
                    .any(|f| f.path == rel && f.area == *area && f.matched == *matched),
                "{rel} not reported as {area:?}/{matched:?}: {:#?}",
                v.still_present
            );
        }
        // The findings hold paths and tokens, never the text.
        let json = serde_json::to_string(&v).unwrap();
        assert!(!json.contains("verified removal marker"), "{json}");
    }

    #[test]
    fn a_line_holding_both_is_reported_once_per_match() {
        let (root, _g) = store("verify-both");
        write(&root.episodic_jsonl(), &line(&learning('A', TEXT)));
        let v = rescan(&root, &eid('A'), Some(TEXT));
        let live: Vec<Match> = v
            .still_present
            .iter()
            .filter(|f| f.area == Area::LiveLog)
            .map(|f| f.matched)
            .collect();
        assert_eq!(live, vec![Match::Id, Match::Text]);
    }

    #[test]
    fn another_event_that_repeats_the_text_is_reported_by_text() {
        let (root, _g) = store("verify-repeat");
        write(&root.episodic_jsonl(), &line(&learning('B', TEXT)));
        let v = rescan(&root, &eid('A'), Some(TEXT));
        assert_eq!(v.status, Status::Partial);
        assert_eq!(v.still_present.len(), 1);
        assert_eq!(v.still_present[0].matched, Match::Text);
    }

    #[test]
    fn short_text_is_not_matched_and_says_so() {
        let (root, _g) = store("verify-short");
        write(&root.workspace_md(), "use cargo\n");
        let v = rescan(&root, &eid('A'), Some("use cargo"));
        assert_eq!(v.text, TextCheck::TooShort);
        assert_eq!(v.status, Status::CompleteInStore);
        let v = rescan(&root, &eid('A'), None);
        assert_eq!(v.text, TextCheck::NotCaptured);
    }

    #[test]
    fn capture_reads_the_live_log_then_the_copies() {
        let (root, _g) = store("verify-capture");
        assert_eq!(capture(&root, &eid('A')), None);
        write(
            &root.snapshot_file("2025-06-01"),
            &line(&learning('A', "archived")),
        );
        assert_eq!(capture(&root, &eid('A')).as_deref(), Some("archived"));
        write(&root.episodic_jsonl(), &line(&learning('A', "live")));
        assert_eq!(capture(&root, &eid('A')).as_deref(), Some("live"));
    }

    /// A deleted document stays in its segment's store until a merge. The
    /// rescan reads the store, deleted documents included, so it finds the
    /// event there, and finds the index clear once the merge ran.
    #[test]
    fn a_deleted_index_document_is_found_until_the_merge() {
        use crate::server::{compact_deletes, TantivyIndexHandle, DEFAULT_COMMIT_CADENCE};
        use crate::writer_lock::{WriterKind, WriterLock};
        use tantivy::index::SegmentId;
        use tantivy::indexer::NoMergePolicy;
        use tantivy::IndexWriter;

        let (root, _g) = store("verify-index");
        // 61 events in one segment, as in the Nov 26 sandbox.
        let mut rows = vec![learning('A', TEXT)];
        for (i, c) in "BCDEFGHJKMNPQRSTVWXYZ0123456789".chars().enumerate() {
            rows.push(learning(
                c,
                &format!("kept row number {i} about cargo builds"),
            ));
            rows.push(AgentLearning {
                id: EventId::parse(&format!("evt_01ARZ3NDEKTSV4RRFFQ69G5FB{c}")).unwrap(),
                ..learning(c, &format!("second kept row {i} about tests"))
            });
        }
        let bytes: String = rows.iter().map(line).collect();
        episodic::rewrite_atomic(&root.episodic_jsonl(), bytes.as_bytes()).unwrap();
        let lock = WriterLock::acquire(&root, WriterKind::Forget).unwrap();
        let index_dir = root.dreamd_dir().join(INDEX_DIR_NAME);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let handle = TantivyIndexHandle::open(&root, &lock, DEFAULT_COMMIT_CADENCE).unwrap();
            handle.shutdown().await.unwrap();
        });
        // One segment, so the deleted document shares it with live ones.
        {
            let index = Index::open_in_dir(&index_dir).unwrap();
            let ids: Vec<SegmentId> = index.searchable_segment_ids().unwrap();
            let mut writer: IndexWriter<TantivyDocument> =
                index.writer_with_num_threads(1, 15_000_000).unwrap();
            writer.set_merge_policy(Box::new(NoMergePolicy));
            if ids.len() > 1 {
                writer.merge(&ids).wait().unwrap();
            }
            writer.garbage_collect_files().wait().unwrap();
            writer.wait_merging_threads().unwrap();
        }
        rt.block_on(async {
            let handle = TantivyIndexHandle::open(&root, &lock, DEFAULT_COMMIT_CADENCE).unwrap();
            handle.prune_decayed_events(vec![eid('A')]).await.unwrap();
            handle.shutdown().await.unwrap();
        });

        let index_findings = |v: &Verification| -> Vec<Finding> {
            v.still_present
                .iter()
                .filter(|f| f.area == Area::Index)
                .cloned()
                .collect()
        };
        let v = rescan(&root, &eid('A'), Some(TEXT));
        assert!(v.index_documents_read.unwrap() >= rows.len(), "{v:?}");
        assert!(
            index_findings(&v)
                .iter()
                .any(|f| f.matched == Match::IndexDocument && f.path.ends_with(".store")),
            "the deleted document was not found: {:#?}",
            v.still_present
        );

        let out = compact_deletes(&root, &lock).unwrap();
        assert!(out.is_clean(), "{out:?}");
        let v = rescan(&root, &eid('A'), Some(TEXT));
        assert_eq!(index_findings(&v), Vec::<Finding>::new());
        assert!(v.index_documents_read.unwrap() >= rows.len() - 1);
    }

    #[test]
    fn segment_names_are_recognized() {
        assert_eq!(
            segment_of("a5c4dfcbdfe645089129e308e26d5523.store"),
            Some("a5c4dfcbdfe645089129e308e26d5523")
        );
        assert_eq!(
            segment_of("a5c4dfcbdfe645089129e308e26d5523.12.del"),
            Some("a5c4dfcbdfe645089129e308e26d5523")
        );
        assert_eq!(segment_of("meta.json"), None);
        assert_eq!(segment_of(".managed.json"), None);
    }
}
