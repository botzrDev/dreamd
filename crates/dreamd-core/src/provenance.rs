//! Provenance edge recording (BZR-155) in the `provenance/1.0` format that
//! `docs/provenance.md` specifies (BZR-154).
//!
//! One append-only file: `<project>/.agent/.dreamd/provenance/ledger.jsonl`.
//! Each line is one edge, compact JSON with keys in the order
//! `schema_version`, `kind`, `from`, `to`, and one trailing `\n`.
//!
//! Three writers call [`append_edges`]:
//!
//! - the dream cycle, through [`record_cycle`], after decay and before
//!   `wal::commit_cycle` (`lesson_citation` and `recurrence` edges);
//! - the indexer actor's `commit_and_persist`, after `writer.commit()` and
//!   before the watermark (`index_doc` edges for a live batch);
//! - `TantivyIndexHandle::open`, after the replay `writer.commit()` and before
//!   `write_progress` (`index_doc` edges for ids replay re-indexed). Startup
//!   replay does not go through `commit_and_persist`.
//!
//! The learn path never opens the ledger. The reserved vector kind in the
//! format has no variant here: a writer MUST NOT emit it. There is no proof,
//! signature, or key in this module (proofs are BZR-151).
//!
//! [`verify`] (BZR-148) is read-only: it recomputes the Merkle root from the
//! good lines and reports orphans, missing edges, and corrupt lines. It does
//! not rewrite the ledger; the format forbids that.

use std::collections::{BTreeSet, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use dreamd_protocol::EventId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::episodic::{self, EpisodicError};
use crate::index::RecurrenceSidecar;
use crate::layout::AgentRoot;
use crate::lessons;

/// The ledger's `schema_version` token. It is not the episodic record token
/// and not the daemon state token.
pub const PROVENANCE_SCHEMA_VERSION: &str = "provenance/1.0";

/// The edge kinds this writer emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
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

#[derive(Debug, thiserror::Error)]
pub enum ProvenanceError {
    #[error("provenance ledger io: {0}")]
    Io(#[from] io::Error),
    #[error("provenance ledger encode: {0}")]
    Encode(#[from] serde_json::Error),
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

/// Append `edges` to the ledger at `ledger`, skipping any `(kind, from, to)`
/// already present (in the file or earlier in `edges`).
///
/// A torn tail (non-empty file that does not end in `\n`) is truncated to the
/// last `\n` first. Complete lines are never rewritten or reordered. An empty
/// `edges` slice writes nothing and does not create the file.
pub fn append_edges(
    ledger: &Path,
    edges: &[(Kind, String, String)],
) -> Result<(), ProvenanceError> {
    if edges.is_empty() {
        return Ok(());
    }
    if let Some(parent) = ledger.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(ledger)?;

    let mut existing = Vec::new();
    file.read_to_end(&mut existing)?;
    if existing.last().is_some_and(|b| *b != b'\n') {
        let keep = existing
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(0, |i| i + 1);
        file.set_len(keep as u64)?;
        existing.truncate(keep);
    }

    let mut seen: HashSet<(String, String, String)> = existing
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice::<ExistingLine>(line).ok())
        .map(|l| (l.kind, l.from, l.to))
        .collect();

    let mut out = Vec::new();
    for (kind, from, to) in edges {
        if !seen.insert((kind.as_str().to_owned(), from.clone(), to.clone())) {
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

    if !out.is_empty() {
        file.seek(SeekFrom::End(0))?;
        file.write_all(&out)?;
    }
    file.sync_data()?;
    Ok(())
}

/// Record the dream cycle's `lesson_citation` and `recurrence` edges from the
/// `LESSONS.md` and `semantic/recurrence_counts.json` this cycle left on disk.
///
/// A missing or unreadable `LESSONS.md` writes nothing and returns `Ok`: the
/// ledger must not fail a cycle that promoted nothing. A missing or malformed
/// sidecar drops the `recurrence` edge only.
pub fn record_cycle(agent_root: &AgentRoot) -> Result<(), ProvenanceError> {
    let Ok(file) = lessons::read_lessons_file(&agent_root.lessons_md()) else {
        return Ok(());
    };
    let Some(exemplar) = file.lessons.first().map(|l| l.id.clone()) else {
        return Ok(());
    };
    let lesson_doc = format!("lsn_{exemplar}");

    let mut edges: Vec<(Kind, String, String)> = file
        .citations
        .iter()
        .map(|id| (Kind::LessonCitation, id.clone(), lesson_doc.clone()))
        .collect();

    let sidecar_path = agent_root.semantic_dir().join("recurrence_counts.json");
    if let Ok(raw) = fs::read_to_string(&sidecar_path) {
        match serde_json::from_str::<RecurrenceSidecar>(&raw) {
            Ok(sidecar) => {
                if sidecar
                    .clusters
                    .iter()
                    .any(|c| c.skill_action == file.cluster_key)
                {
                    edges.push((Kind::Recurrence, exemplar, file.cluster_key.clone()));
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "recurrence_counts.json unparseable; no recurrence edge");
            }
        }
    }

    append_edges(&agent_root.provenance_ledger(), &edges)
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
        // Reserved: a writer MUST NOT emit it, so its presence is corruption.
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
/// An event with no `index_doc` edge is not reported: the index watermark is
/// not a contiguous prefix.
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
        if let Some(exemplar) = file.lessons.first().map(|l| l.id.clone()) {
            let lesson_doc = format!("lsn_{exemplar}");
            expected.extend(
                file.citations
                    .iter()
                    .map(|id| (Kind::LessonCitation, id.clone(), lesson_doc.clone())),
            );
            let sidecar_path = agent_root.semantic_dir().join("recurrence_counts.json");
            let sidecar = fs::read_to_string(&sidecar_path)
                .ok()
                .and_then(|raw| serde_json::from_str::<RecurrenceSidecar>(&raw).ok());
            if sidecar.is_some_and(|s| {
                s.clusters
                    .iter()
                    .any(|c| c.skill_action == file.cluster_key)
            }) {
                expected.push((Kind::Recurrence, exemplar, file.cluster_key.clone()));
            }
        }
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
        let ledger = dir.path().join("provenance/ledger.jsonl");
        append_edges(
            &ledger,
            &[(Kind::LessonCitation, EVT_A.into(), format!("lsn_{EVT_A}"))],
        )
        .unwrap();
        assert_eq!(
            fs::read(&ledger).unwrap(),
            b"{\"schema_version\":\"provenance/1.0\",\"kind\":\"lesson_citation\",\"from\":\"evt_01ARZ3NDEKTSV4RRFFQ69G5FAV\",\"to\":\"lsn_evt_01ARZ3NDEKTSV4RRFFQ69G5FAV\"}\n"
        );
    }

    #[test]
    fn duplicate_triple_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        let edge = (Kind::IndexDoc, EVT_A.to_owned(), EVT_A.to_owned());
        append_edges(&ledger, std::slice::from_ref(&edge)).unwrap();
        append_edges(&ledger, &[edge.clone(), edge]).unwrap();
        assert_eq!(ledger_lines(&ledger).len(), 1);
    }

    #[test]
    fn same_ids_different_kind_is_a_new_edge() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        append_edges(&ledger, &[(Kind::IndexDoc, EVT_A.into(), EVT_A.into())]).unwrap();
        append_edges(
            &ledger,
            &[(Kind::LessonCitation, EVT_A.into(), EVT_A.into())],
        )
        .unwrap();
        assert_eq!(ledger_lines(&ledger).len(), 2);
    }

    #[test]
    fn torn_tail_is_truncated_and_prior_line_kept() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        append_edges(&ledger, &[(Kind::IndexDoc, EVT_A.into(), EVT_A.into())]).unwrap();
        let first = fs::read(&ledger).unwrap();
        let mut torn = first.clone();
        torn.extend_from_slice(b"{\"schema_version\":\"provenance/1.0\",\"kind\":\"ind");
        fs::write(&ledger, &torn).unwrap();

        append_edges(&ledger, &[(Kind::IndexDoc, EVT_B.into(), EVT_B.into())]).unwrap();

        let bytes = fs::read(&ledger).unwrap();
        assert!(bytes.starts_with(&first), "complete line was rewritten");
        let lines = ledger_lines(&ledger);
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains(EVT_B));
        assert!(bytes.ends_with(b"\n"));
    }

    #[test]
    fn torn_only_line_truncates_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        fs::write(&ledger, b"{\"schema_ver").unwrap();
        append_edges(&ledger, &[(Kind::IndexDoc, EVT_A.into(), EVT_A.into())]).unwrap();
        let lines = ledger_lines(&ledger);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("{\"schema_version\":\"provenance/1.0\""));
    }

    fn write_lessons(root: &AgentRoot, cluster_key: &str, citations: &[&str]) {
        fs::create_dir_all(root.semantic_dir()).unwrap();
        let file = LessonsFile {
            last_updated: chrono::DateTime::from_timestamp(1_747_137_600, 0).unwrap(),
            prompt_version: "deterministic".into(),
            cluster_key: cluster_key.into(),
            citations: citations.iter().map(|s| (*s).to_owned()).collect(),
            lessons: vec![Lesson {
                id: EVT_A.into(),
                content: "prefer typed errors".into(),
                pinned: false,
            }],
        };
        lessons::write_lessons_file(&root.lessons_md(), &file).unwrap();
    }

    fn write_sidecar(root: &AgentRoot, keys: &[&str]) {
        let sidecar = RecurrenceSidecar {
            // record_cycle reads only `clusters`.
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
    fn record_cycle_writes_citations_and_one_recurrence_edge() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        let key = "rust::error_handling::axum_rejection";
        write_lessons(&root, key, &[EVT_A, EVT_B]);
        write_sidecar(&root, &[key]);

        record_cycle(&root).unwrap();

        let lsn = format!("lsn_{EVT_A}");
        let expected = vec![
            format!(
                r#"{{"schema_version":"provenance/1.0","kind":"lesson_citation","from":"{EVT_A}","to":"{lsn}"}}"#
            ),
            format!(
                r#"{{"schema_version":"provenance/1.0","kind":"lesson_citation","from":"{EVT_B}","to":"{lsn}"}}"#
            ),
            format!(
                r#"{{"schema_version":"provenance/1.0","kind":"recurrence","from":"{EVT_A}","to":"{key}"}}"#
            ),
        ];
        assert_eq!(ledger_lines(&root.provenance_ledger()), expected);

        // A second cycle over the same files adds nothing.
        record_cycle(&root).unwrap();
        assert_eq!(ledger_lines(&root.provenance_ledger()).len(), 3);
    }

    #[test]
    fn record_cycle_skips_sidecar_rows_for_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        write_lessons(&root, "rust::errors", &[EVT_A, EVT_B]);
        write_sidecar(&root, &["python::imports"]);

        record_cycle(&root).unwrap();

        let lines = ledger_lines(&root.provenance_ledger());
        assert_eq!(lines.len(), 2);
        assert!(lines
            .iter()
            .all(|l| l.contains("\"kind\":\"lesson_citation\"")));
    }

    #[test]
    fn record_cycle_without_lessons_md_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        record_cycle(&root).unwrap();
        assert!(!root.provenance_ledger().exists());
        assert!(!root.provenance_dir().exists());
    }

    // ---- verify (BZR-148) ----

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
