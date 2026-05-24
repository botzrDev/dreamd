//! Dream cycle write-ahead log (DR-303 / WEG-60).
//!
//! Guarantees `.agent/` is never left in a half-promoted state after a
//! mid-cycle crash. Before any destructive op, the intent is appended to
//! `dream_in_progress.wal`. On startup, if the WAL exists, recovery runs
//! before the daemon serves traffic.
//!
//! v0.1 WAL covers JSONL + LESSONS.md + recurrence sidecar writes only.
//! NOTE: Tantivy index mutations are NOT WAL-protected in v0.1.
//! The divergence-and-doctor model applies: if a cycle crashes after Tantivy
//! commits but before JSONL/LESSONS.md, recall may return stale results until
//! `dreamd doctor --repair` rebuilds the index from JSONL.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::io::write_atomic;
use crate::layout::AgentRoot;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "operation", content = "payload")]
pub enum WalIntent {
    ReplaceSemanticMemory { temp_file_path: String },
    PruneEpisodicMemory { temp_file_path: String },
    Commit,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DreamWal {
    pub schema_version: String,
    pub intents: Vec<WalIntent>,
}

#[derive(Debug, PartialEq)]
pub enum RecoveryOutcome {
    /// No WAL found — nothing to recover.
    Clean,
    /// Incomplete cycle recovered: temp files cleaned up, WAL deleted.
    Recovered { cleaned_files: Vec<PathBuf> },
    /// Cycle had committed but cleanup wasn't finished; state.json updated.
    CommittedButUnclean,
}

#[derive(Debug, thiserror::Error)]
pub enum WalError {
    #[error("WAL I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("WAL parse: {0}")]
    Json(#[from] serde_json::Error),
}

/// Write a fresh WAL and set state.json to "in_progress".
/// `_now_sec` is caller-provided for testability.
pub fn begin_cycle(agent_root: &AgentRoot, _now_sec: i64) -> Result<(), WalError> {
    let wal = DreamWal {
        schema_version: "1.0".to_string(),
        intents: Vec::new(),
    };
    let json = serde_json::to_string_pretty(&wal)?;
    let wal_path = agent_root.wal_path();
    if let Some(parent) = wal_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_atomic(&wal_path, json.as_bytes())?;
    update_state_json(agent_root, "in_progress", None)?;
    Ok(())
}

/// Append one intent to the existing WAL (rewrites atomically).
/// No-ops gracefully if no active cycle (WAL not begun).
pub fn append_intent(agent_root: &AgentRoot, intent: WalIntent) -> Result<(), WalError> {
    let wal_path = agent_root.wal_path();
    if !wal_path.exists() {
        return Ok(());
    }
    let mut wal: DreamWal = serde_json::from_str(&std::fs::read_to_string(&wal_path)?)?;
    wal.intents.push(intent);
    let json = serde_json::to_string_pretty(&wal)?;
    write_atomic(&wal_path, json.as_bytes())?;
    Ok(())
}

/// Append Commit intent, update state.json to "complete", delete WAL.
/// Caller-provided `now_sec`.
pub fn commit_cycle(agent_root: &AgentRoot, now_sec: i64) -> Result<(), WalError> {
    append_intent(agent_root, WalIntent::Commit)?;
    let iso = DateTime::from_timestamp(now_sec, 0)
        .map(|dt: DateTime<Utc>| dt.to_rfc3339())
        .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".to_string());
    update_state_json(agent_root, "complete", Some(&iso))?;
    let wal_path = agent_root.wal_path();
    if wal_path.exists() {
        std::fs::remove_file(&wal_path)?;
    }
    Ok(())
}

/// Check for a stale WAL on startup and run recovery.
/// Returns `Clean` if no WAL found, `Recovered` if incomplete cycle cleaned up.
pub fn recover_if_needed(agent_root: &AgentRoot, _now_sec: i64) -> Result<RecoveryOutcome, WalError> {
    let wal_path = agent_root.wal_path();
    if !wal_path.exists() {
        return Ok(RecoveryOutcome::Clean);
    }

    let wal: DreamWal = serde_json::from_str(&std::fs::read_to_string(&wal_path)?)?;
    let committed = wal.intents.contains(&WalIntent::Commit);

    if committed {
        update_state_json(agent_root, "complete", None)?;
        std::fs::remove_file(&wal_path)?;
        return Ok(RecoveryOutcome::CommittedButUnclean);
    }

    let mut cleaned = Vec::new();
    for intent in &wal.intents {
        let tmp_path = match intent {
            WalIntent::ReplaceSemanticMemory { temp_file_path } => PathBuf::from(temp_file_path),
            WalIntent::PruneEpisodicMemory { temp_file_path } => PathBuf::from(temp_file_path),
            WalIntent::Commit => continue,
        };
        if tmp_path.exists() {
            std::fs::remove_file(&tmp_path)?;
            cleaned.push(tmp_path.clone());
        }
        let tmp_neighbour = tmp_path.with_extension("tmp");
        if tmp_neighbour.exists() {
            std::fs::remove_file(&tmp_neighbour)?;
            cleaned.push(tmp_neighbour);
        }
    }

    std::fs::remove_file(&wal_path)?;
    update_state_json(agent_root, "failed", None)?;
    Ok(RecoveryOutcome::Recovered { cleaned_files: cleaned })
}

/// Read `last_dream_cycle_status` from `state.json`.
/// Returns `"idle"` if the file is absent or the key is missing.
pub fn read_last_cycle_status(agent_root: &AgentRoot) -> Result<String, WalError> {
    let path = agent_root.state_json();
    if !path.exists() {
        return Ok("idle".to_string());
    }
    let bytes = std::fs::read(&path)?;
    let v: serde_json::Value = serde_json::from_slice(&bytes)?;
    Ok(v.get("last_dream_cycle_status")
        .and_then(|s| s.as_str())
        .unwrap_or("idle")
        .to_string())
}

fn read_schema_version(agent_root: &AgentRoot) -> Option<String> {
    let bytes = std::fs::read(agent_root.state_json()).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("schema_version")?.as_str().map(|s| s.to_string())
}

fn update_state_json(
    agent_root: &AgentRoot,
    status: &str,
    cycle_at: Option<&str>,
) -> Result<(), WalError> {
    let daemon_version = env!("CARGO_PKG_VERSION");
    let schema_version = read_schema_version(agent_root).unwrap_or_else(|| "1.0".to_string());
    let state = serde_json::json!({
        "schema_version": schema_version,
        "daemon_version": daemon_version,
        "last_dream_cycle_at": cycle_at,
        "last_dream_cycle_status": status,
    });
    let json = serde_json::to_string_pretty(&state)?;
    write_atomic(&agent_root.state_json(), json.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_tmpdir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "dreamd-wal-{}-{}-{}-{}",
            label,
            std::process::id(),
            nanos,
            n,
        ))
    }

    struct DirGuard(PathBuf);
    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn setup_root(label: &str) -> (AgentRoot, DirGuard) {
        let dir = unique_tmpdir(label);
        fs::create_dir_all(&dir).unwrap();
        let root = AgentRoot::new(&dir);
        fs::create_dir_all(root.dreamd_dir()).unwrap();
        let guard = DirGuard(dir);
        (root, guard)
    }

    const NOW_SEC: i64 = 1747137600; // 2026-05-13T12:00:00Z

    #[test]
    fn wal_begin_creates_file() {
        let (root, _g) = setup_root("begin");
        begin_cycle(&root, NOW_SEC).unwrap();

        let wal_path = root.wal_path();
        assert!(wal_path.exists(), "WAL file must exist after begin_cycle");

        let wal: DreamWal = serde_json::from_str(&fs::read_to_string(&wal_path).unwrap()).unwrap();
        assert_eq!(wal.schema_version, "1.0");
        assert!(wal.intents.is_empty(), "WAL must start with empty intents");

        let state: serde_json::Value =
            serde_json::from_slice(&fs::read(root.state_json()).unwrap()).unwrap();
        assert_eq!(state["last_dream_cycle_status"], "in_progress");
    }

    #[test]
    fn wal_commit_removes_wal_and_updates_state() {
        let (root, _g) = setup_root("commit");
        begin_cycle(&root, NOW_SEC).unwrap();
        commit_cycle(&root, NOW_SEC).unwrap();

        assert!(!root.wal_path().exists(), "WAL must be deleted after commit_cycle");

        let state: serde_json::Value =
            serde_json::from_slice(&fs::read(root.state_json()).unwrap()).unwrap();
        assert_eq!(state["last_dream_cycle_status"], "complete");
        assert!(
            state["last_dream_cycle_at"].is_string(),
            "last_dream_cycle_at must be set after commit"
        );
    }

    #[test]
    fn recover_clean_when_no_wal() {
        let (root, _g) = setup_root("clean");
        let outcome = recover_if_needed(&root, NOW_SEC).unwrap();
        assert_eq!(outcome, RecoveryOutcome::Clean);
    }

    #[test]
    fn recover_incomplete_deletes_tmp_and_marks_failed() {
        let (root, _g) = setup_root("recovery");

        // Create the tmp file that the WAL intent references.
        let tmp_path = root.episodic_jsonl().with_extension("tmp");
        fs::create_dir_all(tmp_path.parent().unwrap()).unwrap();
        fs::write(&tmp_path, b"partial write\n").unwrap();

        // Write a WAL with a PruneEpisodicMemory intent but no Commit.
        let wal = DreamWal {
            schema_version: "1.0".to_string(),
            intents: vec![WalIntent::PruneEpisodicMemory {
                temp_file_path: tmp_path.to_string_lossy().into_owned(),
            }],
        };
        let wal_json = serde_json::to_string_pretty(&wal).unwrap();
        fs::write(root.wal_path(), wal_json.as_bytes()).unwrap();

        let outcome = recover_if_needed(&root, NOW_SEC).unwrap();

        assert!(!tmp_path.exists(), ".tmp file must be deleted by recovery");
        assert!(!root.wal_path().exists(), "WAL must be deleted by recovery");

        let state: serde_json::Value =
            serde_json::from_slice(&fs::read(root.state_json()).unwrap()).unwrap();
        assert_eq!(state["last_dream_cycle_status"], "failed");

        assert!(
            matches!(outcome, RecoveryOutcome::Recovered { .. }),
            "outcome must be Recovered"
        );
    }
}
