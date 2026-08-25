//! Typed `.agent/.dreamd/state.json` and the single read-modify-write path
//! that owns it (AILAB-167).
//!
//! `state.json` had three writers. Two of them — [`crate::wal`]'s cycle edges
//! and [`crate::autobiography`]'s skip marker — write the *same* live file, and
//! the wal writer rebuilt it from scratch on every call: it read back only
//! `schema_version`, then emitted a fresh four-key object. Every key it did not
//! know about was dropped. `last_autobiography_skip` is exactly such a key, so
//! a skip marker written by the autobiography pass was eaten by the next
//! `begin_cycle` **and** by `commit_cycle`, and `dreamd doctor` stopped being
//! able to explain why the audit-trail commit had not happened. The same
//! from-scratch shape also wrote JSON `null` into `last_dream_cycle_at`
//! whenever the caller had no timestamp to hand, so `begin_cycle` wiped the
//! previous cycle's completion time.
//!
//! This module owns the one typed read-modify-write path, so "not patched"
//! means "preserved": callers name only the fields they are changing and
//! everything else round-trips off disk. It is a third module rather than
//! living in `wal` because [`DaemonState`] needs [`AutobiographySkip`] and both
//! writers need the writer — putting either half in `wal.rs` makes a
//! `wal → autobiography → wal` compile cycle.

use serde::{Deserialize, Serialize};

use crate::io::write_atomic;
use crate::layout::AgentRoot;

/// Schema version for daemon state (`state.json`). Versions independently of
/// the record schema (`dreamd_protocol::RECORD_SCHEMA_VERSION`).
pub const STATE_SCHEMA_VERSION: &str = "1.0";

#[derive(Debug, thiserror::Error)]
pub enum DaemonStateError {
    #[error("state.json I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("state.json parse: {0}")]
    Json(#[from] serde_json::Error),
}

/// State.json field that surfaces in `dreamd doctor`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutobiographySkip {
    pub at: i64,
    pub reason: String,
    pub files: Vec<String>,
}

/// Typed `.agent/.dreamd/state.json`.
///
/// Field DECLARATION ORDER IS LOAD-BEARING: `#[derive(Serialize)]` emits
/// declaration order, and `dream_cycle_snapshot__state_json.snap` pins the
/// BTreeMap-alphabetical order the old `serde_json::json!` writer produced
/// (serde_json has no `preserve_order` feature here). Keep these five
/// alphabetical or the snapshot breaks.
///
/// The container-level `#[serde(default)]` is what lets a *partial* on-disk
/// `state.json` — e.g. `{"schema_version":"1.0","daemon_version":"0.0.0"}`,
/// the exact shape an existing `wal.rs` test writes — deserialize without
/// error instead of failing the read and blocking the write.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonState {
    pub daemon_version: String,
    /// Absent-means-no-skip: serialized only when `Some`, so clearing the
    /// marker omits the key rather than writing JSON `null` (matches the
    /// pre-AILAB-167 `Map::remove` semantic that doctor reads).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_autobiography_skip: Option<AutobiographySkip>,
    pub last_dream_cycle_at: Option<String>,
    pub last_dream_cycle_status: String,
    pub schema_version: String,
}

impl Default for DaemonState {
    fn default() -> Self {
        Self {
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            last_autobiography_skip: None,
            last_dream_cycle_at: None,
            last_dream_cycle_status: "idle".to_string(),
            schema_version: STATE_SCHEMA_VERSION.to_string(),
        }
    }
}

/// Read `state.json`. A missing file is the pre-first-write state and yields
/// [`DaemonState::default`]; an unreadable/unparseable file is an error, never
/// a silent clobber.
pub fn read_daemon_state(agent_root: &AgentRoot) -> Result<DaemonState, DaemonStateError> {
    let path = agent_root.state_json();
    if !path.exists() {
        return Ok(DaemonState::default());
    }
    Ok(serde_json::from_slice(&std::fs::read(&path)?)?)
}

/// Read-modify-write `state.json` through [`DaemonState`].
///
/// `f` patches only the fields it names; every other field round-trips from
/// disk. `daemon_version` is restamped on every write (pre-AILAB-167 wal
/// behavior). Durability is `crate::io::write_atomic`.
pub fn update_daemon_state(
    agent_root: &AgentRoot,
    f: impl FnOnce(&mut DaemonState),
) -> Result<(), DaemonStateError> {
    let path = agent_root.state_json();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut state = read_daemon_state(agent_root)?;
    f(&mut state);
    state.daemon_version = env!("CARGO_PKG_VERSION").to_string();
    let json = serde_json::to_string_pretty(&state)?;
    write_atomic(&path, json.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{unique_tmpdir, DirGuard};
    use std::fs;

    fn setup_root(label: &str) -> (AgentRoot, DirGuard) {
        let dir = unique_tmpdir(label);
        fs::create_dir_all(&dir).unwrap();
        let root = AgentRoot::new(&dir);
        fs::create_dir_all(root.dreamd_dir()).unwrap();
        let guard = DirGuard(dir);
        (root, guard)
    }

    fn skip_fixture() -> AutobiographySkip {
        AutobiographySkip {
            at: 42,
            reason: "user_dirty_tree".to_string(),
            files: vec![".agent/semantic/LESSONS.md".to_string()],
        }
    }

    #[test]
    fn read_daemon_state_defaults_when_file_missing() {
        let (root, _g) = setup_root("state-missing");
        assert!(!root.state_json().exists());

        let state = read_daemon_state(&root).unwrap();
        assert_eq!(state.last_dream_cycle_status, "idle");
        assert_eq!(state.last_dream_cycle_at, None);
        assert!(state.last_autobiography_skip.is_none());
        assert_eq!(state.schema_version, STATE_SCHEMA_VERSION);
    }

    #[test]
    fn update_preserves_unpatched_fields() {
        let (root, _g) = setup_root("state-preserve");

        update_daemon_state(&root, |s| {
            s.last_autobiography_skip = Some(skip_fixture());
        })
        .unwrap();

        // A second writer that names only the status must not eat the skip.
        update_daemon_state(&root, |s| {
            s.last_dream_cycle_status = "in_progress".to_string();
        })
        .unwrap();

        let state = read_daemon_state(&root).unwrap();
        assert_eq!(state.last_dream_cycle_status, "in_progress");
        let skip = state
            .last_autobiography_skip
            .expect("skip marker must survive an unrelated patch");
        assert_eq!(skip.reason, "user_dirty_tree");
        assert_eq!(skip.at, 42);
        assert_eq!(skip.files, vec![".agent/semantic/LESSONS.md".to_string()]);
    }

    #[test]
    fn skip_none_omits_the_key() {
        let (root, _g) = setup_root("state-skip-omitted");

        update_daemon_state(&root, |s| {
            s.last_autobiography_skip = Some(skip_fixture());
        })
        .unwrap();
        update_daemon_state(&root, |s| s.last_autobiography_skip = None).unwrap();

        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(root.state_json()).unwrap()).unwrap();
        assert!(
            raw.get("last_autobiography_skip").is_none(),
            "clearing the marker must OMIT the key, not write JSON null: {raw}",
        );
    }

    #[test]
    fn unparseable_state_json_is_an_error() {
        let (root, _g) = setup_root("state-unparseable");
        let path = root.state_json();
        fs::write(&path, b"{not json").unwrap();

        let err = update_daemon_state(&root, |s| {
            s.last_dream_cycle_status = "complete".to_string();
        });
        assert!(err.is_err(), "unparseable state.json must be an error");

        assert_eq!(
            fs::read(&path).unwrap(),
            b"{not json",
            "a failed read must never clobber the file",
        );
    }
}
