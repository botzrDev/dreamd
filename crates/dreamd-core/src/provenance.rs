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

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

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
}
