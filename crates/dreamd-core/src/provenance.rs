//! Provenance edge recording (BZR-155) in the `provenance/1.0` format that
//! `docs/provenance.md` specifies (BZR-154).
//!
//! One append-only file: `<project>/.agent/.dreamd/provenance/ledger.jsonl`.
//! Each line is one edge, compact JSON with keys in the order
//! `schema_version`, `kind`, `from`, `to`, and one trailing `\n`.
//!
//! Every edge goes through one [`Ledger`] per process and project (BZR-1236).
//! `WriterLock::acquire` builds it, after cutting a torn final line, and the
//! lock token carries it. Its callers:
//!
//! - the dream cycle and `dreamd forget`: their plan lists the
//!   `lesson_citation` and `recurrence` edges ([`lesson_edges`]), the redo WAL
//!   carries them, and `wal::roll_forward` appends them after the renames;
//! - the indexer's persist step, after `writer.commit()` and before the
//!   watermark (`index_doc` edges for every id that commit covered).
//!
//! The appender opens the file with `O_APPEND` on its first edge and
//! serializes appends behind a mutex, so the coordinator and the indexer can
//! append at the same time. It reads the file's triples once, at that first
//! edge, and keeps them in memory (BZR-1243).
//!
//! The learn path never opens the ledger. The reserved vector kind in the
//! format has no variant here: the format forbids writing it. This module has no
//! signature and no key. `dreamd forget --proof` writes a forget receipt
//! (BZR-151) and does not write the signed membership proof.
//!
//! [`verify`] (BZR-148) is read-only: it recomputes the Merkle root from the
//! good lines and reports orphans, missing edges, and corrupt lines. It does
//! not rewrite the ledger; the format forbids that.

use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use dreamd_protocol::EventId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::episodic::{self, EpisodicError};
use crate::index::RecurrenceSidecar;
use crate::layout::AgentRoot;
use crate::lessons::{self, LessonsFile};

/// The ledger's `schema_version` token. It is not the episodic record token
/// and not the daemon state token.
pub const PROVENANCE_SCHEMA_VERSION: &str = "provenance/1.0";

/// The edge kinds this writer emits. A redo WAL carries them, so they read
/// back too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Event id → `lsn_` + lesson id; the lesson's `citations` names the event.
    LessonCitation,
    /// Event id → the same id as the Tantivy episodic document's `event_id`.
    IndexDoc,
    /// Lesson exemplar id → `skill_action` cluster key. One edge per cluster.
    Recurrence,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::LessonCitation => "lesson_citation",
            Kind::IndexDoc => "index_doc",
            Kind::Recurrence => "recurrence",
        }
    }
}

/// Errors from appending to or verifying the provenance ledger.
#[derive(Debug, thiserror::Error)]
pub enum ProvenanceError {
    /// Opening, reading, writing or syncing the ledger failed.
    #[error("provenance ledger io: {0}")]
    Io(#[from] io::Error),
    /// An edge could not be serialized as a ledger line.
    #[error("provenance ledger encode: {0}")]
    Encode(#[from] serde_json::Error),
    /// `verify` could not read the episodic log it checks edges against.
    #[error("provenance verify: episodic log: {0}")]
    Episodic(#[from] EpisodicError),
}

/// Wire shape of one line. Field order is the key order on the wire.
#[derive(Serialize)]
struct EdgeLine<'a> {
    schema_version: &'static str,
    kind: Kind,
    from: &'a str,
    to: &'a str,
}

/// Read side for the duplicate check. `kind` stays a string so a line of a
/// kind this writer does not emit still round-trips as a triple.
#[derive(Deserialize)]
struct ExistingLine {
    kind: String,
    from: String,
    to: String,
}

/// The project's provenance appender. `WriterLock::acquire` builds the only
/// one; every edge this process writes goes through it.
#[derive(Clone, Debug)]
pub struct Ledger {
    inner: Arc<LedgerInner>,
}

#[derive(Debug)]
struct LedgerInner {
    path: PathBuf,
    /// Opened on the first non-empty append.
    open: Mutex<Option<Open>>,
}

/// The appender's fd and every triple the file holds.
#[derive(Debug)]
struct Open {
    /// Opened with `O_APPEND`.
    file: File,
    /// [`edge_key`] of every line in the file: those read at the open and
    /// those this appender wrote since.
    seen: HashSet<[u8; 32]>,
}

/// SHA-256 of `kind ‖ 0x00 ‖ from ‖ 0x00 ‖ to`: one triple in 32 bytes.
fn edge_key(kind: &str, from: &str, to: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_bytes());
    hasher.update([0u8]);
    hasher.update(from.as_bytes());
    hasher.update([0u8]);
    hasher.update(to.as_bytes());
    hasher.finalize().into()
}

impl Ledger {
    /// Cut a torn final line, then return an appender that has not opened the
    /// file. Called only by `WriterLock::acquire`.
    ///
    /// A missing file is left missing. Any other failure, such as
    /// `provenance/` being a regular file, is a `warn!`: the appender is still
    /// returned, and its first append reports the error.
    pub(crate) fn open(path: PathBuf) -> Self {
        if let Err(e) = cut_torn_tail(&path) {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not check the provenance ledger for a torn final line"
            );
        }
        Self {
            inner: Arc::new(LedgerInner {
                path,
                open: Mutex::new(None),
            }),
        }
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Append `edges`, skipping any `(kind, from, to)` already in the file or
    /// earlier in `edges`. An empty slice writes nothing and creates nothing.
    ///
    /// The file's triples are read once, when the first non-empty append opens
    /// it, and kept: this appender is the only writer while its process holds
    /// the project writer lock (BZR-1243). An edge counts as held only after
    /// its line is written and synced, so a failed append leaves its edges for
    /// the caller's retry.
    ///
    /// Never truncates: complete lines are never rewritten or reordered.
    pub fn append(&self, edges: &[(Kind, String, String)]) -> Result<(), ProvenanceError> {
        if edges.is_empty() {
            return Ok(());
        }
        let mut slot = self.inner.open.lock().expect("ledger mutex poisoned");
        if slot.is_none() {
            *slot = Some(self.open_and_load()?);
        }
        let open = slot.as_mut().expect("opened above");

        let mut chosen: HashSet<[u8; 32]> = HashSet::new();
        let mut out = Vec::new();
        for (kind, from, to) in edges {
            let key = edge_key(kind.as_str(), from, to);
            if open.seen.contains(&key) || !chosen.insert(key) {
                continue;
            }
            serde_json::to_writer(
                &mut out,
                &EdgeLine {
                    schema_version: PROVENANCE_SCHEMA_VERSION,
                    kind: *kind,
                    from,
                    to,
                },
            )?;
            out.push(b'\n');
        }

        #[cfg(test)]
        if self
            .inner
            .path
            .with_file_name("failpoint-ledger-write")
            .exists()
        {
            return Err(io::Error::other("failpoint-ledger-write").into());
        }
        if !out.is_empty() {
            open.file.write_all(&out)?;
            open.file.sync_data()?;
        }
        open.seen.extend(chosen);
        Ok(())
    }

    /// Open the file for append and read every triple it holds, through the
    /// same fd. `O_APPEND` still sends every write to the end.
    fn open_and_load(&self) -> Result<Open, ProvenanceError> {
        if let Some(parent) = self.inner.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&self.inner.path)?;
        file.seek(SeekFrom::Start(0))?;
        let mut existing = Vec::new();
        file.read_to_end(&mut existing)?;
        let seen = existing
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .filter_map(|line| serde_json::from_slice::<ExistingLine>(line).ok())
            .map(|l| edge_key(&l.kind, &l.from, &l.to))
            .collect();
        Ok(Open { file, seen })
    }
}

/// Truncate `path` to just after its last `\n` when it does not end in one.
fn cut_torn_tail(path: &Path) -> io::Result<()> {
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if bytes.last().is_none_or(|b| *b == b'\n') {
        return Ok(());
    }
    let keep = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    file.set_len(keep as u64)?;
    file.sync_data()?;
    tracing::warn!(
        path = %path.display(),
        bytes = bytes.len() - keep,
        "cut a torn final line from the provenance ledger"
    );
    Ok(())
}

/// The `lesson_citation` and `recurrence` edges of `file`: one citation edge
/// per `citations` id, in order, to `lsn_<first lesson id>`, then one
/// `recurrence` edge, exemplar → `cluster_key`, when `cluster_listed` (the
/// recurrence sidecar lists `cluster_key`). A file with no lesson has no edges.
///
/// The one edge rule: a cycle's or a forget's plan lists these edges for the
/// commit to append, and [`verify`] expects them of the live files.
#[must_use]
pub fn lesson_edges(file: &LessonsFile, cluster_listed: bool) -> Vec<(Kind, String, String)> {
    let Some(exemplar) = file.lessons.first().map(|l| l.id.clone()) else {
        return Vec::new();
    };
    let lesson_doc = format!("lsn_{exemplar}");
    let mut edges: Vec<(Kind, String, String)> = file
        .citations
        .iter()
        .map(|id| (Kind::LessonCitation, id.clone(), lesson_doc.clone()))
        .collect();
    if cluster_listed {
        edges.push((Kind::Recurrence, exemplar, file.cluster_key.clone()));
    }
    edges
}

/// Result of [`verify`]. `root` is the Merkle root recomputed from the good
/// lines; nothing on disk stores a root to compare it against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvenanceReport {
    pub root: String,
    pub orphans: Vec<EdgeFault>,
    pub missing: Vec<EdgeFault>,
    pub corrupt_lines: Vec<u64>,
    pub skipped_unknown_kind: u64,
}

/// One edge [`verify`] flagged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeFault {
    /// 1-based ledger line; 0 when the edge was expected and is absent.
    pub line: u64,
    pub kind: String,
    pub from: String,
    pub to: String,
}

/// Full wire shape for the checker: all four fields must be strings.
#[derive(Deserialize)]
struct VerifyLine {
    schema_version: String,
    kind: String,
    from: String,
    to: String,
}

enum LineClass {
    Good,
    Corrupt,
    UnknownKind,
}

fn is_event_id(s: &str) -> bool {
    EventId::parse(s).is_ok()
}

fn classify(line: &VerifyLine) -> LineClass {
    if line.schema_version != PROVENANCE_SCHEMA_VERSION {
        return LineClass::Corrupt;
    }
    let ok = match line.kind.as_str() {
        "lesson_citation" => {
            is_event_id(&line.from) && line.to.strip_prefix("lsn_").is_some_and(is_event_id)
        }
        "index_doc" => is_event_id(&line.from) && line.to == line.from,
        "recurrence" => {
            is_event_id(&line.from)
                && !line.to.is_empty()
                && !line.to.chars().any(char::is_whitespace)
        }
        // Reserved: the format forbids writing it, so its presence is corruption.
        "embedding" => false,
        _ => return LineClass::UnknownKind,
    };
    if ok {
        LineClass::Good
    } else {
        LineClass::Corrupt
    }
}

/// Merkle root over the given leaves (already unique and byte-sorted), per
/// `docs/provenance.md`: leaf = SHA-256(id), parent = SHA-256(left || right)
/// over raw bytes, odd node promoted unchanged, empty = SHA-256("").
fn merkle_root<'a>(leaves: impl Iterator<Item = &'a str>) -> [u8; 32] {
    let mut level: Vec<[u8; 32]> = leaves
        .map(|id| Sha256::digest(id.as_bytes()).into())
        .collect();
    if level.is_empty() {
        return Sha256::digest([]).into();
    }
    while level.len() > 1 {
        level = level
            .chunks(2)
            .map(|pair| match pair {
                [l, r] => {
                    let mut h = Sha256::new();
                    h.update(l);
                    h.update(r);
                    h.finalize().into()
                }
                [odd] => *odd,
                _ => unreachable!("chunks(2) yields one or two nodes"),
            })
            .collect();
    }
    level[0]
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Check the ledger against the live memory files. Read-only: it never
/// writes the ledger or `LESSONS.md` and never repairs a line.
///
/// A missing ledger is `Ok` with the empty-ledger root. Corrupt lines
/// (unparseable, wrong `schema_version`, `embedding`, or a known kind with a
/// bad shape) and unknown kinds are left out of the root. An orphan is a good
/// edge whose `from` is absent from the live JSONL; orphans stay in the root.
/// An event with no `index_doc` edge is not reported: edges land after the
/// index commit, and the next coverage pass backfills any a crash skipped.
pub fn verify(agent_root: &AgentRoot) -> Result<ProvenanceReport, ProvenanceError> {
    let bytes = match fs::read(agent_root.provenance_ledger()) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };

    let mut good: Vec<(u64, VerifyLine)> = Vec::new();
    let mut corrupt_lines = Vec::new();
    let mut skipped_unknown_kind = 0u64;
    for (idx, raw) in bytes.split(|b| *b == b'\n').enumerate() {
        if raw.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let line_no = idx as u64 + 1;
        let Ok(line) = serde_json::from_slice::<VerifyLine>(raw) else {
            corrupt_lines.push(line_no);
            continue;
        };
        match classify(&line) {
            LineClass::Good => good.push((line_no, line)),
            LineClass::Corrupt => corrupt_lines.push(line_no),
            LineClass::UnknownKind => skipped_unknown_kind += 1,
        }
    }

    let leaves: BTreeSet<&str> = good.iter().map(|(_, l)| l.from.as_str()).collect();
    let root = to_hex(&merkle_root(leaves.into_iter()));

    let live: HashSet<String> = episodic::read_all(&agent_root.episodic_jsonl())?
        .into_iter()
        .map(|ev| ev.id.as_str().to_owned())
        .collect();
    let orphans = good
        .iter()
        .filter(|(_, l)| !live.contains(&l.from))
        .map(|(n, l)| EdgeFault {
            line: *n,
            kind: l.kind.clone(),
            from: l.from.clone(),
            to: l.to.clone(),
        })
        .collect();

    let present: HashSet<(&str, &str, &str)> = good
        .iter()
        .map(|(_, l)| (l.kind.as_str(), l.from.as_str(), l.to.as_str()))
        .collect();
    let mut expected: Vec<(Kind, String, String)> = Vec::new();
    if let Ok(file) = lessons::read_lessons_file(&agent_root.lessons_md()) {
        let sidecar_path = agent_root.semantic_dir().join("recurrence_counts.json");
        let sidecar = fs::read_to_string(&sidecar_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<RecurrenceSidecar>(&raw).ok());
        let listed = sidecar.is_some_and(|s| {
            s.clusters
                .iter()
                .any(|c| c.skill_action == file.cluster_key)
        });
        expected = lesson_edges(&file, listed);
    }
    let missing = expected
        .into_iter()
        .filter(|(k, f, t)| !present.contains(&(k.as_str(), f.as_str(), t.as_str())))
        .map(|(k, from, to)| EdgeFault {
            line: 0,
            kind: k.as_str().to_owned(),
            from,
            to,
        })
        .collect();

    Ok(ProvenanceReport {
        root,
        orphans,
        missing,
        corrupt_lines,
        skipped_unknown_kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::ClusterCount;
    use crate::lessons::{Lesson, LessonsFile};

    const EVT_A: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const EVT_B: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FAW";

    fn ledger_lines(path: &Path) -> Vec<String> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn line_bytes_are_exact() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(dir.path().join("provenance/ledger.jsonl"));
        ledger
            .append(&[(Kind::LessonCitation, EVT_A.into(), format!("lsn_{EVT_A}"))])
            .unwrap();
        assert_eq!(
            fs::read(ledger.path()).unwrap(),
            b"{\"schema_version\":\"provenance/1.0\",\"kind\":\"lesson_citation\",\"from\":\"evt_01ARZ3NDEKTSV4RRFFQ69G5FAV\",\"to\":\"lsn_evt_01ARZ3NDEKTSV4RRFFQ69G5FAV\"}\n"
        );
    }

    #[test]
    fn duplicate_triple_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(dir.path().join("ledger.jsonl"));
        let edge = (Kind::IndexDoc, EVT_A.to_owned(), EVT_A.to_owned());
        ledger.append(std::slice::from_ref(&edge)).unwrap();
        ledger.append(&[edge.clone(), edge]).unwrap();
        assert_eq!(ledger_lines(ledger.path()).len(), 1);
    }

    #[test]
    fn same_ids_different_kind_is_a_new_edge() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(dir.path().join("ledger.jsonl"));
        ledger
            .append(&[(Kind::IndexDoc, EVT_A.into(), EVT_A.into())])
            .unwrap();
        ledger
            .append(&[(Kind::LessonCitation, EVT_A.into(), EVT_A.into())])
            .unwrap();
        assert_eq!(ledger_lines(ledger.path()).len(), 2);
    }

    /// A second appender on the same path (a later process) sees the first
    /// one's lines in its dedupe.
    #[test]
    fn duplicate_triple_from_an_earlier_appender_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let edge = (Kind::IndexDoc, EVT_A.to_owned(), EVT_A.to_owned());
        Ledger::open(path.clone())
            .append(std::slice::from_ref(&edge))
            .unwrap();
        Ledger::open(path.clone()).append(&[edge]).unwrap();
        assert_eq!(ledger_lines(&path).len(), 1);
    }

    /// BZR-1243: the appender reads the file once. A truncation behind its
    /// back is not seen until a new appender opens the file.
    #[test]
    fn the_ledger_file_is_read_once_per_appender() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let edge = (Kind::IndexDoc, EVT_A.to_owned(), EVT_A.to_owned());
        let ledger = Ledger::open(path.clone());
        ledger.append(std::slice::from_ref(&edge)).unwrap();
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(0)
            .unwrap();

        ledger.append(std::slice::from_ref(&edge)).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "",
            "the held set still has E"
        );

        Ledger::open(path.clone()).append(&[edge]).unwrap();
        assert_eq!(ledger_lines(&path).len(), 1);
    }

    /// BZR-1243: an edge is held only once its line is written and synced.
    #[test]
    fn a_failed_write_does_not_mark_its_edges_seen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let failpoint = dir.path().join("failpoint-ledger-write");
        let edge = (Kind::IndexDoc, EVT_A.to_owned(), EVT_A.to_owned());
        let ledger = Ledger::open(path.clone());

        fs::write(&failpoint, b"").unwrap();
        assert!(ledger.append(std::slice::from_ref(&edge)).is_err());

        fs::remove_file(&failpoint).unwrap();
        ledger.append(std::slice::from_ref(&edge)).unwrap();
        assert_eq!(
            ledger_lines(&path),
            vec![format!(
                "{{\"schema_version\":\"provenance/1.0\",\"kind\":\"index_doc\",\"from\":\"{EVT_A}\",\"to\":\"{EVT_A}\"}}"
            )]
        );
    }

    #[test]
    fn empty_append_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(dir.path().join("provenance/ledger.jsonl"));
        ledger.append(&[]).unwrap();
        assert!(!dir.path().join("provenance").exists());
    }

    /// Appends from several threads at once all land, each as one whole line:
    /// the one shared appender serializes them, so no two writers land on one
    /// offset and no tail repair cuts another's line (BZR-1236).
    #[test]
    fn ledger_appends_from_many_threads_keep_every_line() {
        use std::sync::Barrier;
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(dir.path().join("provenance/ledger.jsonl"));
        let barrier = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let ledger = ledger.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    for i in 0..100 {
                        let id = format!("evt_01ARZ3NDEKTSV4RRFFQ69G{t}{i:03}");
                        ledger.append(&[(Kind::IndexDoc, id.clone(), id)]).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let bytes = fs::read(ledger.path()).unwrap();
        let lines: Vec<&[u8]> = bytes
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .collect();
        let valid = lines
            .iter()
            .filter(|l| {
                serde_json::from_slice::<VerifyLine>(l)
                    .is_ok_and(|v| matches!(classify(&v), LineClass::Good))
            })
            .count();
        assert_eq!(
            (lines.len(), valid),
            (800, 800),
            "8 threads x 100 distinct edges must leave 800 valid lines"
        );
    }

    /// A one-lesson file filed under `EVT_A`.
    fn lessons_file(cluster_key: &str, citations: &[&str]) -> LessonsFile {
        LessonsFile {
            last_updated: chrono::DateTime::from_timestamp(1_747_137_600, 0).unwrap(),
            prompt_version: "deterministic".into(),
            cluster_key: cluster_key.into(),
            citations: citations.iter().map(|s| (*s).to_owned()).collect(),
            lessons: vec![Lesson {
                id: EVT_A.into(),
                content: "prefer typed errors".into(),
                pinned: false,
            }],
        }
    }

    fn write_lessons(root: &AgentRoot, cluster_key: &str, citations: &[&str]) {
        fs::create_dir_all(root.semantic_dir()).unwrap();
        lessons::write_lessons_file(&root.lessons_md(), &lessons_file(cluster_key, citations))
            .unwrap();
    }

    fn write_sidecar(root: &AgentRoot, keys: &[&str]) {
        let sidecar = RecurrenceSidecar {
            // verify reads only `clusters`.
            schema_version: String::new(),
            clusters: keys
                .iter()
                .map(|k| ClusterCount {
                    skill_action: (*k).to_owned(),
                    count: 3,
                })
                .collect(),
        };
        fs::write(
            root.semantic_dir().join("recurrence_counts.json"),
            serde_json::to_string(&sidecar).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn lesson_edges_lists_citations_then_one_recurrence_edge() {
        let key = "rust::error_handling::axum_rejection";
        let file = lessons_file(key, &[EVT_A, EVT_B]);

        let lsn = format!("lsn_{EVT_A}");
        assert_eq!(
            lesson_edges(&file, true),
            vec![
                (Kind::LessonCitation, EVT_A.to_owned(), lsn.clone()),
                (Kind::LessonCitation, EVT_B.to_owned(), lsn),
                (Kind::Recurrence, EVT_A.to_owned(), key.to_owned()),
            ]
        );
    }

    #[test]
    fn lesson_edges_without_a_listed_cluster_has_no_recurrence_edge() {
        let file = lessons_file("rust::errors", &[EVT_A, EVT_B]);

        let lsn = format!("lsn_{EVT_A}");
        assert_eq!(
            lesson_edges(&file, false),
            vec![
                (Kind::LessonCitation, EVT_A.to_owned(), lsn.clone()),
                (Kind::LessonCitation, EVT_B.to_owned(), lsn),
            ]
        );
    }

    #[test]
    fn lesson_edges_of_a_file_without_lessons_is_empty() {
        let file = LessonsFile {
            lessons: Vec::new(),
            ..lessons_file("rust::errors", &[EVT_A])
        };

        assert_eq!(lesson_edges(&file, true), Vec::new());
    }

    // ── verify (BZR-148) ───────────────────────────────────────────────────

    fn sha256(bytes: &[u8]) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        Sha256::digest(bytes).into()
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn edge(kind: &str, from: &str, to: &str) -> String {
        format!(
            r#"{{"schema_version":"provenance/1.0","kind":"{kind}","from":"{from}","to":"{to}"}}"#
        )
    }

    fn write_ledger(root: &AgentRoot, lines: &[String]) {
        let path = root.provenance_ledger();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut body = lines.join("\n");
        body.push('\n');
        fs::write(path, body).unwrap();
    }

    fn write_events(root: &AgentRoot, ids: &[&str]) {
        let path = root.episodic_jsonl();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: String = ids
            .iter()
            .map(|id| {
                format!(
                    r#"{{"schema_version":"1.0.0","id":"{id}","timestamp":"2025-05-13T12:00:00Z","pain":5.0,"importance":5.0,"pinned":false,"skill_action":"rust::errors","source_harness":"test","content":"c"}}"#
                ) + "\n"
            })
            .collect();
        fs::write(path, body).unwrap();
    }

    fn verify_ledger(lines: &[String], events: &[&str]) -> ProvenanceReport {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        write_events(&root, events);
        write_ledger(&root, lines);
        verify(&root).unwrap()
    }

    #[test]
    fn verify_missing_ledger_is_ok_with_empty_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        let report = verify(&root).unwrap();
        assert_eq!(report.root, hex(&sha256(b"")));
        assert!(report.orphans.is_empty());
        assert!(report.missing.is_empty());
        assert!(report.corrupt_lines.is_empty());
        assert_eq!(report.skipped_unknown_kind, 0);
        assert!(
            !root.provenance_ledger().exists(),
            "verify must not create the ledger"
        );
    }

    #[test]
    fn verify_two_leaves_root_is_hash_of_concatenated_leaf_hashes() {
        assert!(EVT_A < EVT_B);
        let lsn = format!("lsn_{EVT_A}");
        // Written B first: leaf order is by sort, not by line order.
        let report = verify_ledger(
            &[
                edge("lesson_citation", EVT_B, &lsn),
                edge("lesson_citation", EVT_A, &lsn),
            ],
            &[EVT_A, EVT_B],
        );
        let mut cat = sha256(EVT_A.as_bytes()).to_vec();
        cat.extend_from_slice(&sha256(EVT_B.as_bytes()));
        assert_eq!(report.root, hex(&sha256(&cat)));
        assert!(report.orphans.is_empty());
        assert!(report.corrupt_lines.is_empty());
    }

    #[test]
    fn verify_edge_absent_from_jsonl_is_an_orphan_and_stays_in_root() {
        let lsn = format!("lsn_{EVT_A}");
        let lines = [
            edge("lesson_citation", EVT_A, &lsn),
            edge("lesson_citation", EVT_B, &lsn),
        ];
        let report = verify_ledger(&lines, &[EVT_A]);
        assert_eq!(
            report.orphans,
            vec![EdgeFault {
                line: 2,
                kind: "lesson_citation".into(),
                from: EVT_B.into(),
                to: lsn.clone(),
            }]
        );
        let with_both = verify_ledger(&lines, &[EVT_A, EVT_B]);
        assert!(with_both.orphans.is_empty());
        assert_eq!(report.root, with_both.root);
    }

    #[test]
    fn verify_uncited_citation_is_missing_not_orphan() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        let key = "rust::errors";
        write_lessons(&root, key, &[EVT_A, EVT_B]);
        write_sidecar(&root, &[key]);
        write_events(&root, &[EVT_A, EVT_B]);
        let lsn = format!("lsn_{EVT_A}");
        write_ledger(
            &root,
            &[
                edge("lesson_citation", EVT_A, &lsn),
                edge("recurrence", EVT_A, key),
            ],
        );

        let report = verify(&root).unwrap();
        assert_eq!(
            report.missing,
            vec![EdgeFault {
                line: 0,
                kind: "lesson_citation".into(),
                from: EVT_B.into(),
                to: lsn,
            }]
        );
        assert!(report.orphans.is_empty());
    }

    #[test]
    fn verify_missing_recurrence_only_when_sidecar_names_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        let key = "rust::errors";
        write_lessons(&root, key, &[EVT_A]);
        write_events(&root, &[EVT_A]);
        write_ledger(
            &root,
            &[edge("lesson_citation", EVT_A, &format!("lsn_{EVT_A}"))],
        );

        // No sidecar: no recurrence edge is expected.
        assert!(verify(&root).unwrap().missing.is_empty());
        // Unparseable sidecar: still none.
        fs::write(
            root.semantic_dir().join("recurrence_counts.json"),
            "{not json",
        )
        .unwrap();
        assert!(verify(&root).unwrap().missing.is_empty());
        // Sidecar names the key: one missing recurrence edge.
        write_sidecar(&root, &[key]);
        let missing = verify(&root).unwrap().missing;
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].kind, "recurrence");
        assert_eq!(missing[0].from, EVT_A);
        assert_eq!(missing[0].to, key);
    }

    #[test]
    fn verify_non_json_line_is_corrupt_and_left_out_of_root() {
        let a = edge("index_doc", EVT_A, EVT_A);
        let b = edge("index_doc", EVT_B, EVT_B);
        let report = verify_ledger(&[a.clone(), "not json".into(), b.clone()], &[EVT_A, EVT_B]);
        assert_eq!(report.corrupt_lines, vec![2]);
        assert_eq!(report.root, verify_ledger(&[a, b], &[EVT_A, EVT_B]).root);
    }

    #[test]
    fn verify_embedding_is_corrupt_and_unknown_kind_is_skipped() {
        let a = edge("index_doc", EVT_A, EVT_A);
        let baseline = verify_ledger(std::slice::from_ref(&a), &[EVT_A, EVT_B]);

        let report = verify_ledger(
            &[a.clone(), edge("embedding", EVT_B, EVT_B)],
            &[EVT_A, EVT_B],
        );
        assert_eq!(report.corrupt_lines, vec![2]);
        assert_eq!(report.root, baseline.root);

        let report = verify_ledger(&[a, edge("other", EVT_B, "x")], &[EVT_A, EVT_B]);
        assert!(report.corrupt_lines.is_empty());
        assert_eq!(report.skipped_unknown_kind, 1);
        assert_eq!(report.root, baseline.root);
    }

    #[test]
    fn verify_index_doc_with_different_to_is_corrupt() {
        let report = verify_ledger(&[edge("index_doc", EVT_A, EVT_B)], &[EVT_A, EVT_B]);
        assert_eq!(report.corrupt_lines, vec![1]);
        assert_eq!(report.root, hex(&sha256(b"")));
    }

    #[test]
    fn verify_wrong_schema_version_and_bad_lsn_are_corrupt() {
        let wrong_version = edge("index_doc", EVT_A, EVT_A).replace("provenance/1.0", "1.0.0");
        let bad_lsn = edge("lesson_citation", EVT_A, "lsn_nope");
        let report = verify_ledger(&[wrong_version, bad_lsn], &[EVT_A]);
        assert_eq!(report.corrupt_lines, vec![1, 2]);
    }

    #[test]
    fn verify_does_not_write_the_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        write_lessons(&root, "rust::errors", &[EVT_A, EVT_B]);
        write_ledger(&root, &["{\"torn".into()]);
        // Strip the trailing newline so the file ends in a torn tail.
        let path = root.provenance_ledger();
        let mut bytes = fs::read(&path).unwrap();
        bytes.pop();
        fs::write(&path, &bytes).unwrap();

        let report = verify(&root).unwrap();
        assert_eq!(report.corrupt_lines, vec![1]);
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}
