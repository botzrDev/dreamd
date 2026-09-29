//! Compare two snapshot objects (BZR-156).
//!
//! An object is `branches/objects/<id>/` in the `branches/1.0` format
//! (`docs/branching.md`, written by [`crate::snapshot`]). This module only
//! reads objects; it never writes one and never takes a snapshot. There is no
//! CLI here: `dreamd memory` is BZR-150's command.
//!
//! Events are compared by `AgentLearning.id`. Salience is the query-time
//! formula without the BM25 term, scored with one `now_sec` for both sides,
//! so an unchanged record whose cluster count is unchanged has delta 0.
//! `LESSONS.md`, `PREFERENCES.md`, and `recurrence_counts.json` are compared
//! as raw bytes.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use dreamd_protocol::AgentLearning;

use crate::episodic::{self, EpisodicError};
use crate::index::RecurrenceSidecar;
use crate::layout::AgentRoot;
use crate::salience::salience;

const EPISODIC_PATH: &str = "episodic/AGENT_LEARNINGS.jsonl";
const LESSONS_PATH: &str = "semantic/LESSONS.md";
const RECURRENCE_PATH: &str = "semantic/recurrence_counts.json";
const PREFERENCES_PATH: &str = "personal/PREFERENCES.md";

#[derive(Debug, thiserror::Error)]
pub enum DiffError {
    #[error("object id {0:?} is not 64 lowercase hex characters")]
    InvalidId(String),
    #[error("object {0} not found")]
    MissingObject(String),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path}: {source}")]
    Episodic {
        path: PathBuf,
        #[source]
        source: EpisodicError,
    },
    #[error("{path}: invalid recurrence sidecar: {source}")]
    Sidecar {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// How one non-event file changed between the two objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChange {
    Same,
    Added,
    Removed,
    Modified,
}

/// One event present on both sides whose salience differs.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SalienceChange {
    pub id: String,
    pub from: f64,
    pub to: f64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct MemoryDiff {
    pub events_added: Vec<String>,
    pub events_removed: Vec<String>,
    pub events_salience_changed: Vec<SalienceChange>,
    pub lessons: FileChange,
    pub preferences: FileChange,
    pub recurrence: FileChange,
}

/// Diff object `from_id` against object `to_id` under
/// `agent_root.branches_objects_dir()`. `now_sec` is the clock for both
/// sides' salience; the caller supplies it.
pub fn diff_objects(
    agent_root: &AgentRoot,
    from_id: &str,
    to_id: &str,
    now_sec: i64,
) -> Result<MemoryDiff, DiffError> {
    let from_dir = object_dir(agent_root, from_id)?;
    let to_dir = object_dir(agent_root, to_id)?;

    let from_events = load_events(&from_dir)?;
    let to_events = load_events(&to_dir)?;
    let from_counts = load_counts(&from_dir)?;
    let to_counts = load_counts(&to_dir)?;

    let events_added = to_events
        .keys()
        .filter(|id| !from_events.contains_key(*id))
        .cloned()
        .collect();
    let events_removed = from_events
        .keys()
        .filter(|id| !to_events.contains_key(*id))
        .cloned()
        .collect();
    let events_salience_changed = from_events
        .iter()
        .filter_map(|(id, from_ev)| {
            let to_ev = to_events.get(id)?;
            let from = score(from_ev, &from_counts, now_sec);
            let to = score(to_ev, &to_counts, now_sec);
            (from != to).then(|| SalienceChange {
                id: id.clone(),
                from,
                to,
            })
        })
        .collect();

    Ok(MemoryDiff {
        events_added,
        events_removed,
        events_salience_changed,
        lessons: compare_file(&from_dir, &to_dir, LESSONS_PATH)?,
        preferences: compare_file(&from_dir, &to_dir, PREFERENCES_PATH)?,
        recurrence: compare_file(&from_dir, &to_dir, RECURRENCE_PATH)?,
    })
}

fn object_dir(agent_root: &AgentRoot, id: &str) -> Result<PathBuf, DiffError> {
    let valid = id.len() == 64 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !valid {
        return Err(DiffError::InvalidId(id.to_string()));
    }
    let dir = agent_root.branches_objects_dir().join(id);
    if !dir.is_dir() {
        return Err(DiffError::MissingObject(id.to_string()));
    }
    Ok(dir)
}

/// Events keyed by id, sorted. A repeated id keeps the later record.
fn load_events(dir: &Path) -> Result<BTreeMap<String, AgentLearning>, DiffError> {
    let path = dir.join(EPISODIC_PATH);
    let records = episodic::read_all(&path).map_err(|source| DiffError::Episodic {
        path: path.clone(),
        source,
    })?;
    Ok(records
        .into_iter()
        .map(|ev| (ev.id.as_str().to_string(), ev))
        .collect())
}

/// `skill_action` → sidecar count. Absent sidecar is empty; invalid JSON errors.
fn load_counts(dir: &Path) -> Result<HashMap<String, u32>, DiffError> {
    let path = dir.join(RECURRENCE_PATH);
    let Some(bytes) = read_optional(&path)? else {
        return Ok(HashMap::new());
    };
    let sidecar: RecurrenceSidecar =
        serde_json::from_slice(&bytes).map_err(|source| DiffError::Sidecar { path, source })?;
    Ok(sidecar
        .clusters
        .into_iter()
        .map(|c| (c.skill_action, c.count))
        .collect())
}

fn score(ev: &AgentLearning, counts: &HashMap<String, u32>, now_sec: i64) -> f64 {
    let recurrence = counts.get(&ev.skill_action).copied().unwrap_or(0);
    salience(
        now_sec,
        ev.timestamp.timestamp(),
        f64::from(ev.pain),
        f64::from(ev.importance),
        u64::from(recurrence),
    )
}

fn compare_file(from_dir: &Path, to_dir: &Path, rel: &str) -> Result<FileChange, DiffError> {
    let from = read_optional(&from_dir.join(rel))?;
    let to = read_optional(&to_dir.join(rel))?;
    Ok(match (from, to) {
        (None, None) => FileChange::Same,
        (None, Some(_)) => FileChange::Added,
        (Some(_), None) => FileChange::Removed,
        (Some(a), Some(b)) if a == b => FileChange::Same,
        (Some(_), Some(_)) => FileChange::Modified,
    })
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, DiffError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(DiffError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_790_000_000;
    const TS: i64 = NOW - 3 * 86_400;
    const ID_A: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FAA";
    const ID_B: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FAB";
    const ID_C: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FAC";

    fn obj_id(n: u8) -> String {
        format!("{n:02x}").repeat(32)
    }

    fn line(id: &str, skill_action: &str) -> String {
        let ts = chrono::DateTime::from_timestamp(TS, 0)
            .unwrap()
            .to_rfc3339();
        format!(
            "{{\"schema_version\":\"1.0.0\",\"id\":\"{id}\",\"timestamp\":\"{ts}\",\
             \"pain\":6.0,\"importance\":7.0,\"pinned\":false,\
             \"skill_action\":\"{skill_action}\",\"source_harness\":\"test\",\
             \"content\":\"c\"}}\n"
        )
    }

    fn sidecar(skill_action: &str, count: u32) -> String {
        format!(
            "{{\"schema_version\":\"1.0\",\"clusters\":[{{\"skill_action\":\"{skill_action}\",\"count\":{count}}}]}}"
        )
    }

    /// Write an object directory holding only the given (`path`, bytes) files.
    fn write_object(root: &AgentRoot, id: &str, files: &[(&str, &str)]) {
        let dir = root.branches_objects_dir().join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("manifest.json"), b"{}\n").unwrap();
        for (rel, body) in files {
            let path = dir.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
    }

    fn root() -> (tempfile::TempDir, AgentRoot) {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        (dir, root)
    }

    #[test]
    fn added_and_removed_ids_are_not_salience_rows() {
        let (_d, root) = root();
        let from = format!("{}{}", line(ID_A, "rust::a"), line(ID_B, "rust::a"));
        let to = format!("{}{}", line(ID_B, "rust::a"), line(ID_C, "rust::a"));
        write_object(&root, &obj_id(1), &[(EPISODIC_PATH, &from)]);
        write_object(&root, &obj_id(2), &[(EPISODIC_PATH, &to)]);

        let diff = diff_objects(&root, &obj_id(1), &obj_id(2), NOW).unwrap();
        assert_eq!(diff.events_added, vec![ID_C.to_string()]);
        assert_eq!(diff.events_removed, vec![ID_A.to_string()]);
        assert!(diff.events_salience_changed.is_empty());
    }

    #[test]
    fn higher_sidecar_count_on_to_is_one_salience_row() {
        let (_d, root) = root();
        let jsonl = format!("{}{}", line(ID_A, "rust::a"), line(ID_B, "rust::b"));
        write_object(
            &root,
            &obj_id(1),
            &[
                (EPISODIC_PATH, &jsonl),
                (RECURRENCE_PATH, &sidecar("rust::a", 1)),
            ],
        );
        write_object(
            &root,
            &obj_id(2),
            &[
                (EPISODIC_PATH, &jsonl),
                (RECURRENCE_PATH, &sidecar("rust::a", 4)),
            ],
        );

        let diff = diff_objects(&root, &obj_id(1), &obj_id(2), NOW).unwrap();
        let expected = SalienceChange {
            id: ID_A.to_string(),
            from: salience(NOW, TS, 6.0, 7.0, 1),
            to: salience(NOW, TS, 6.0, 7.0, 4),
        };
        assert!(expected.to > expected.from);
        assert_eq!(diff.events_salience_changed, vec![expected]);
        assert!(diff.events_added.is_empty());
        assert!(diff.events_removed.is_empty());
        assert_eq!(diff.recurrence, FileChange::Modified);
    }

    #[test]
    fn repeated_id_keeps_the_later_record() {
        let (_d, root) = root();
        let from = format!("{}{}", line(ID_A, "rust::a"), line(ID_A, "rust::b"));
        let to = line(ID_A, "rust::b");
        let counts = sidecar("rust::b", 3);
        write_object(
            &root,
            &obj_id(1),
            &[(EPISODIC_PATH, &from), (RECURRENCE_PATH, &counts)],
        );
        write_object(
            &root,
            &obj_id(2),
            &[(EPISODIC_PATH, &to), (RECURRENCE_PATH, &counts)],
        );

        let diff = diff_objects(&root, &obj_id(1), &obj_id(2), NOW).unwrap();
        assert!(diff.events_salience_changed.is_empty());
    }

    #[test]
    fn identical_objects_are_empty_and_same() {
        let (_d, root) = root();
        let files: &[(&str, &str)] = &[
            (EPISODIC_PATH, &line(ID_A, "rust::a")),
            (LESSONS_PATH, "# lessons\n"),
            (PREFERENCES_PATH, "prefs\n"),
            (RECURRENCE_PATH, &sidecar("rust::a", 2)),
        ];
        write_object(&root, &obj_id(1), files);
        write_object(&root, &obj_id(2), files);

        let diff = diff_objects(&root, &obj_id(1), &obj_id(2), NOW).unwrap();
        assert!(diff.events_added.is_empty());
        assert!(diff.events_removed.is_empty());
        assert!(diff.events_salience_changed.is_empty());
        assert_eq!(diff.lessons, FileChange::Same);
        assert_eq!(diff.preferences, FileChange::Same);
        assert_eq!(diff.recurrence, FileChange::Same);
    }

    #[test]
    fn same_id_on_both_sides_is_the_identical_case() {
        let (_d, root) = root();
        write_object(
            &root,
            &obj_id(1),
            &[
                (EPISODIC_PATH, &line(ID_A, "rust::a")),
                (LESSONS_PATH, "x\n"),
            ],
        );

        let diff = diff_objects(&root, &obj_id(1), &obj_id(1), NOW).unwrap();
        assert!(diff.events_added.is_empty());
        assert!(diff.events_removed.is_empty());
        assert!(diff.events_salience_changed.is_empty());
        assert_eq!(diff.lessons, FileChange::Same);
        assert_eq!(diff.preferences, FileChange::Same);
        assert_eq!(diff.recurrence, FileChange::Same);
    }

    #[test]
    fn lessons_added_and_modified() {
        let (_d, root) = root();
        write_object(&root, &obj_id(1), &[]);
        write_object(&root, &obj_id(2), &[(LESSONS_PATH, "one\n")]);
        write_object(&root, &obj_id(3), &[(LESSONS_PATH, "two\n")]);

        let added = diff_objects(&root, &obj_id(1), &obj_id(2), NOW).unwrap();
        assert_eq!(added.lessons, FileChange::Added);
        assert_eq!(added.preferences, FileChange::Same);

        let removed = diff_objects(&root, &obj_id(2), &obj_id(1), NOW).unwrap();
        assert_eq!(removed.lessons, FileChange::Removed);

        let modified = diff_objects(&root, &obj_id(2), &obj_id(3), NOW).unwrap();
        assert_eq!(modified.lessons, FileChange::Modified);
    }

    #[test]
    fn missing_object_is_an_error() {
        let (_d, root) = root();
        write_object(&root, &obj_id(1), &[]);
        let err = diff_objects(&root, &obj_id(1), &obj_id(9), NOW).unwrap_err();
        assert!(matches!(err, DiffError::MissingObject(id) if id == obj_id(9)));
    }

    #[test]
    fn malformed_id_is_an_error() {
        let (_d, root) = root();
        write_object(&root, &obj_id(1), &[]);
        let upper = obj_id(1).to_uppercase().replace('0', "A");
        for bad in ["abc", upper.as_str(), "../objects"] {
            let err = diff_objects(&root, &obj_id(1), bad, NOW).unwrap_err();
            assert!(matches!(err, DiffError::InvalidId(_)), "{bad}");
        }
    }

    #[test]
    fn invalid_sidecar_json_is_an_error() {
        let (_d, root) = root();
        write_object(&root, &obj_id(1), &[]);
        write_object(&root, &obj_id(2), &[(RECURRENCE_PATH, "not json")]);
        let err = diff_objects(&root, &obj_id(1), &obj_id(2), NOW).unwrap_err();
        assert!(matches!(err, DiffError::Sidecar { .. }));
    }
}
