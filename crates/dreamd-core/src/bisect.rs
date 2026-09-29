//! `dreamd memory bisect` — binary search over the `snap-*` timeline (BZR-153).
//!
//! The `branches/1.0` format (`docs/branching.md`) has no parent pointer: a ref
//! is an object id plus a UTC timestamp. The search range is therefore every
//! `snap-*` ref whose timestamp string lies between a good endpoint and a bad
//! endpoint. Zero-padded `YYYY-MM-DDTHH:MM:SSZ` strings sort in time order, so
//! timestamps are compared as strings and never parsed.
//!
//! Each step checks out a ref through [`snapshot::checkout`], which refuses
//! while the daemon socket exists. State is one JSON object at
//! `branches/bisect`, beside `HEAD`. It is not a ref and never appears in
//! [`snapshot::list_branches`]. It is written only after a checkout succeeds.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::layout::AgentRoot;
use crate::snapshot::{self, SnapshotError, AUTOSNAP_REF_PREFIX};

/// Filename of the bisect state inside `branches/`.
pub const BISECT_STATE_FILE: &str = "bisect";

#[derive(Debug, thiserror::Error)]
pub enum BisectError {
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("ref {path}: {reason}")]
    BadRef { path: PathBuf, reason: String },
    #[error("endpoint {0:?} is neither an existing ref nor an object id some ref names")]
    UnknownEndpoint(String),
    #[error("good and bad are the same ref {0:?}")]
    SameEndpoint(String),
    #[error(
        "good ref {good:?} ({good_ts}) must be strictly earlier than bad ref {bad:?} ({bad_ts})"
    )]
    OutOfOrder {
        good: String,
        good_ts: String,
        bad: String,
        bad_ts: String,
    },
    #[error("no bisect in progress; run `dreamd memory bisect start` first")]
    NotInProgress,
    #[error("bisect state {path}: {reason}")]
    BadState { path: PathBuf, reason: String },
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> BisectError + '_ {
    move |source| BisectError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// What a bisect step did. Both carry the ref name and the object id on the
/// ref's first line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BisectOutcome {
    /// `name` is checked out; test it and mark it good or bad.
    Checkout { name: String, id: String },
    /// `name` is the first bad ref. No state file remains.
    Found { name: String, id: String },
}

#[derive(Debug, Serialize, Deserialize)]
struct BisectState {
    good_idx: usize,
    bad_idx: usize,
    current_idx: usize,
    steps: Vec<String>,
}

/// One parsed ref file.
#[derive(Debug, Clone)]
struct RefEntry {
    name: String,
    id: String,
    timestamp: String,
}

/// `branches/bisect`. Not under `refs/`.
pub fn state_path(agent_root: &AgentRoot) -> PathBuf {
    agent_root.branches_dir().join(BISECT_STATE_FILE)
}

fn is_object_id(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn read_ref(refs: &Path, name: &str) -> Result<RefEntry, BisectError> {
    let path = refs.join(name);
    let body = match fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(SnapshotError::BranchNotFound(name.to_string()).into())
        }
        Err(e) => return Err(io_err(&path)(e)),
    };
    let mut lines = body.lines();
    let id = lines.next().unwrap_or("");
    if !is_object_id(id) {
        return Err(BisectError::BadRef {
            path,
            reason: "first line is not a 64-hex object id".to_string(),
        });
    }
    let timestamp = lines.next().unwrap_or("");
    if timestamp.is_empty() {
        return Err(BisectError::BadRef {
            path,
            reason: "second line (timestamp) is empty".to_string(),
        });
    }
    Ok(RefEntry {
        name: name.to_string(),
        id: id.to_string(),
        timestamp: timestamp.to_string(),
    })
}

/// Every well-named ref file, parsed. Temp files and names outside the
/// grammar are skipped. A missing `refs/` is empty.
fn read_all_refs(refs: &Path) -> Result<Vec<RefEntry>, BisectError> {
    let entries = match fs::read_dir(refs) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(refs)(e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_err(refs))?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !snapshot::is_valid_branch_name(&name) || !entry.path().is_file() {
            continue;
        }
        out.push(read_ref(refs, &name)?);
    }
    Ok(out)
}

/// An existing ref by name, else the earliest-timestamp ref naming a 64-hex id.
fn resolve_endpoint(refs: &Path, endpoint: &str) -> Result<RefEntry, BisectError> {
    if snapshot::is_valid_branch_name(endpoint) && refs.join(endpoint).is_file() {
        return read_ref(refs, endpoint);
    }
    if is_object_id(endpoint) {
        return read_all_refs(refs)?
            .into_iter()
            .filter(|r| r.id == endpoint)
            .min_by(|a, b| {
                a.timestamp
                    .cmp(&b.timestamp)
                    .then_with(|| a.name.cmp(&b.name))
            })
            .ok_or_else(|| BisectError::UnknownEndpoint(endpoint.to_string()));
    }
    Err(BisectError::UnknownEndpoint(endpoint.to_string()))
}

fn midpoint(good_idx: usize, bad_idx: usize) -> usize {
    good_idx + (bad_idx - good_idx) / 2
}

fn write_state(agent_root: &AgentRoot, state: &BisectState) -> Result<(), BisectError> {
    let dest = state_path(agent_root);
    let dir = agent_root.branches_dir();
    let bytes = serde_json::to_vec(state).map_err(SnapshotError::from)?;
    // Leading dot: the temp name is never mistaken for the state file.
    let tmp = dir.join(format!(".{BISECT_STATE_FILE}.tmp"));
    fs::write(&tmp, bytes).map_err(io_err(&tmp))?;
    fs::rename(&tmp, &dest).map_err(io_err(&dest))
}

fn read_state(agent_root: &AgentRoot) -> Result<BisectState, BisectError> {
    let path = state_path(agent_root);
    let raw = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(BisectError::NotInProgress),
        Err(e) => return Err(io_err(&path)(e)),
    };
    let bad = |reason: String| BisectError::BadState {
        path: path.clone(),
        reason,
    };
    let state: BisectState = serde_json::from_slice(&raw).map_err(|e| bad(e.to_string()))?;
    let n = state.steps.len();
    if !(state.good_idx < state.current_idx
        && state.current_idx < state.bad_idx
        && state.bad_idx < n)
    {
        return Err(bad(format!(
            "indices good={} current={} bad={} do not fit {} steps",
            state.good_idx, state.current_idx, state.bad_idx, n
        )));
    }
    Ok(state)
}

/// Error unless a readable, well-formed bisect state file exists.
pub fn ensure_in_progress(agent_root: &AgentRoot) -> Result<(), BisectError> {
    read_state(agent_root).map(|_| ())
}

/// Start a bisect between `good` and `bad` (ref names, or object ids some ref
/// names). Adjacent endpoints return [`BisectOutcome::Found`] with no checkout
/// and no state file. Otherwise the midpoint is checked out and, only after
/// that succeeds, `branches/bisect` is written.
pub fn bisect_start(
    agent_root: &AgentRoot,
    good: &str,
    bad: &str,
    daemon_socket: Option<&Path>,
) -> Result<BisectOutcome, BisectError> {
    let refs = agent_root.branches_refs_dir();
    let good = resolve_endpoint(&refs, good)?;
    let bad = resolve_endpoint(&refs, bad)?;
    if good.name == bad.name {
        return Err(BisectError::SameEndpoint(good.name));
    }
    if good.timestamp >= bad.timestamp {
        return Err(BisectError::OutOfOrder {
            good: good.name,
            good_ts: good.timestamp,
            bad: bad.name,
            bad_ts: bad.timestamp,
        });
    }

    let mut steps: Vec<RefEntry> = read_all_refs(&refs)?
        .into_iter()
        .filter(|r| {
            r.name.starts_with(AUTOSNAP_REF_PREFIX)
                && r.name != good.name
                && r.name != bad.name
                && r.timestamp >= good.timestamp
                && r.timestamp <= bad.timestamp
        })
        .collect();
    steps.push(good.clone());
    steps.push(bad.clone());
    steps.sort_by(|a, b| {
        a.timestamp
            .cmp(&b.timestamp)
            .then_with(|| a.name.cmp(&b.name))
    });
    // Both ends were pushed and good sorts strictly before bad.
    let g = steps.iter().position(|r| r.name == good.name).unwrap_or(0);
    let b = steps
        .iter()
        .position(|r| r.name == bad.name)
        .unwrap_or(steps.len() - 1);
    let steps: Vec<String> = steps[g..=b].iter().map(|r| r.name.clone()).collect();

    let bad_idx = steps.len() - 1;
    if bad_idx == 1 {
        return Ok(BisectOutcome::Found {
            name: bad.name,
            id: bad.id,
        });
    }
    let current_idx = midpoint(0, bad_idx);
    let name = steps[current_idx].clone();
    let id = snapshot::checkout(agent_root, &name, daemon_socket)?;
    write_state(
        agent_root,
        &BisectState {
            good_idx: 0,
            bad_idx,
            current_idx,
            steps,
        },
    )?;
    Ok(BisectOutcome::Checkout { name, id })
}

/// Mark the checked-out step good (`mark_good`) or bad. When the range closes,
/// delete the state file and return [`BisectOutcome::Found`] without another
/// checkout. Otherwise check out the new midpoint and then rewrite the state;
/// a failed checkout leaves the previous state file as it was.
pub fn bisect_mark(
    agent_root: &AgentRoot,
    mark_good: bool,
    daemon_socket: Option<&Path>,
) -> Result<BisectOutcome, BisectError> {
    let mut state = read_state(agent_root)?;
    if mark_good {
        state.good_idx = state.current_idx;
    } else {
        state.bad_idx = state.current_idx;
    }

    if state.bad_idx == state.good_idx + 1 {
        let found = read_ref(&agent_root.branches_refs_dir(), &state.steps[state.bad_idx])?;
        let path = state_path(agent_root);
        fs::remove_file(&path).map_err(io_err(&path))?;
        return Ok(BisectOutcome::Found {
            name: found.name,
            id: found.id,
        });
    }

    state.current_idx = midpoint(state.good_idx, state.bad_idx);
    let name = state.steps[state.current_idx].clone();
    let id = snapshot::checkout(agent_root, &name, daemon_socket)?;
    write_state(agent_root, &state)?;
    Ok(BisectOutcome::Checkout { name, id })
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-09-29T14:03:00Z
    const NOW: i64 = 1_790_690_580;

    fn scaffold() -> (tempfile::TempDir, AgentRoot) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = AgentRoot::new(dir.path());
        fs::create_dir_all(root.episodic_jsonl().parent().unwrap()).unwrap();
        (dir, root)
    }

    /// One autosnap per offset, each over distinct JSONL bytes. Returns
    /// `(ref name, object id)` in timeline order.
    fn snaps(root: &AgentRoot, offsets: &[i64]) -> Vec<(String, String)> {
        offsets
            .iter()
            .map(|&off| {
                fs::write(root.episodic_jsonl(), format!("{{\"x\":{off}}}\n")).unwrap();
                let id = snapshot::create_autosnap(root, NOW + off).unwrap();
                (snapshot::autosnap_ref_name(NOW + off).unwrap(), id)
            })
            .collect()
    }

    fn head(root: &AgentRoot) -> PathBuf {
        root.branches_dir().join(snapshot::HEAD_FILE)
    }

    #[test]
    fn start_checks_out_middle_and_mark_good_finds_third() {
        let (_dir, root) = scaffold();
        let s = snaps(&root, &[0, 60, 120]);

        let out = bisect_start(&root, &s[0].0, &s[2].0, None).unwrap();
        assert_eq!(
            out,
            BisectOutcome::Checkout {
                name: s[1].0.clone(),
                id: s[1].1.clone()
            }
        );
        assert!(state_path(&root).is_file());
        assert_eq!(
            fs::read_to_string(root.episodic_jsonl()).unwrap(),
            "{\"x\":60}\n"
        );
        let names: Vec<_> = snapshot::list_branches(&root)
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert!(!names.iter().any(|n| n == "bisect"), "{names:?}");
        assert_eq!(names.len(), 3);

        let out = bisect_mark(&root, true, None).unwrap();
        assert_eq!(
            out,
            BisectOutcome::Found {
                name: s[2].0.clone(),
                id: s[2].1.clone()
            }
        );
        assert!(!state_path(&root).exists());
    }

    #[test]
    fn mark_bad_then_good_finds_third_of_five() {
        let (_dir, root) = scaffold();
        let s = snaps(&root, &[0, 60, 120, 180, 240]);

        let out = bisect_start(&root, &s[0].0, &s[4].0, None).unwrap();
        assert!(matches!(out, BisectOutcome::Checkout { ref name, .. } if *name == s[2].0));
        let out = bisect_mark(&root, false, None).unwrap();
        assert!(matches!(out, BisectOutcome::Checkout { ref name, .. } if *name == s[1].0));
        assert!(state_path(&root).is_file());
        let out = bisect_mark(&root, true, None).unwrap();
        assert_eq!(
            out,
            BisectOutcome::Found {
                name: s[2].0.clone(),
                id: s[2].1.clone()
            }
        );
        assert!(!state_path(&root).exists());
    }

    #[test]
    fn adjacent_endpoints_found_without_state_or_head() {
        let (_dir, root) = scaffold();
        let s = snaps(&root, &[0, 60]);

        let out = bisect_start(&root, &s[0].0, &s[1].0, None).unwrap();
        assert_eq!(
            out,
            BisectOutcome::Found {
                name: s[1].0.clone(),
                id: s[1].1.clone()
            }
        );
        assert!(!state_path(&root).exists());
        assert!(!head(&root).exists());
        // No checkout: the live file is still the last one written.
        assert_eq!(
            fs::read_to_string(root.episodic_jsonl()).unwrap(),
            "{\"x\":60}\n"
        );
    }

    #[test]
    fn object_id_endpoint_resolves_to_its_ref() {
        let (_dir, root) = scaffold();
        let s = snaps(&root, &[0, 60, 120]);

        let out = bisect_start(&root, &s[0].1, &s[2].1, None).unwrap();
        assert!(matches!(out, BisectOutcome::Checkout { ref name, .. } if *name == s[1].0));
    }

    #[test]
    fn good_after_bad_is_an_error() {
        let (_dir, root) = scaffold();
        let s = snaps(&root, &[0, 60, 120]);

        let err = bisect_start(&root, &s[2].0, &s[0].0, None).unwrap_err();
        assert!(matches!(err, BisectError::OutOfOrder { .. }), "{err}");
        assert!(!state_path(&root).exists());
        assert!(!head(&root).exists());
    }

    #[test]
    fn same_ref_both_ends_is_an_error() {
        let (_dir, root) = scaffold();
        let s = snaps(&root, &[0]);
        let err = bisect_start(&root, &s[0].0, &s[0].0, None).unwrap_err();
        assert!(matches!(err, BisectError::SameEndpoint(_)), "{err}");
    }

    #[test]
    fn unknown_object_id_is_an_error() {
        let (_dir, root) = scaffold();
        let s = snaps(&root, &[0, 60]);

        let err = bisect_start(&root, &"a".repeat(64), &s[1].0, None).unwrap_err();
        assert!(matches!(err, BisectError::UnknownEndpoint(_)), "{err}");
        let err = bisect_start(&root, "NOT A REF", &s[1].0, None).unwrap_err();
        assert!(matches!(err, BisectError::UnknownEndpoint(_)), "{err}");
    }

    #[test]
    fn existing_socket_refuses_and_writes_no_state() {
        let (dir, root) = scaffold();
        let s = snaps(&root, &[0, 60, 120]);
        let sock = dir.path().join("dreamd.sock");
        fs::write(&sock, b"").unwrap();

        let err = bisect_start(&root, &s[0].0, &s[2].0, Some(&sock)).unwrap_err();
        assert!(
            matches!(err, BisectError::Snapshot(SnapshotError::DaemonRunning(_))),
            "{err}"
        );
        assert!(!state_path(&root).exists());
        assert!(!head(&root).exists());
    }

    #[test]
    fn failed_mark_checkout_leaves_previous_state() {
        let (dir, root) = scaffold();
        let s = snaps(&root, &[0, 60, 120, 180, 240]);
        bisect_start(&root, &s[0].0, &s[4].0, None).unwrap();
        let before = fs::read(state_path(&root)).unwrap();

        let sock = dir.path().join("dreamd.sock");
        fs::write(&sock, b"").unwrap();
        let err = bisect_mark(&root, false, Some(&sock)).unwrap_err();
        assert!(
            matches!(err, BisectError::Snapshot(SnapshotError::DaemonRunning(_))),
            "{err}"
        );
        assert_eq!(fs::read(state_path(&root)).unwrap(), before);
    }

    #[test]
    fn mark_without_state_is_not_in_progress_and_bad_state_stays() {
        let (_dir, root) = scaffold();
        assert!(matches!(
            bisect_mark(&root, true, None),
            Err(BisectError::NotInProgress)
        ));

        fs::create_dir_all(root.branches_dir()).unwrap();
        fs::write(state_path(&root), b"not json").unwrap();
        assert!(matches!(
            bisect_mark(&root, true, None),
            Err(BisectError::BadState { .. })
        ));
        assert_eq!(fs::read(state_path(&root)).unwrap(), b"not json");
    }

    #[test]
    fn named_branch_endpoint_sorts_before_a_same_second_snap() {
        let (_dir, root) = scaffold();
        let s = snaps(&root, &[0, 60]);
        fs::write(root.episodic_jsonl(), b"{\"x\":\"named\"}\n").unwrap();
        // create_branch also autosnaps at NOW+120; `exp` sorts before that
        // `snap-*` name, so the slice ends at `exp` and drops the autosnap.
        snapshot::create_branch(&root, "exp", NOW + 120).unwrap();

        let out = bisect_start(&root, &s[0].0, "exp", None).unwrap();
        assert!(
            matches!(out, BisectOutcome::Checkout { ref name, .. } if *name == s[1].0),
            "{out:?}"
        );
        let out = bisect_mark(&root, false, None).unwrap();
        assert!(
            matches!(out, BisectOutcome::Found { ref name, .. } if *name == s[1].0),
            "{out:?}"
        );
    }
}
