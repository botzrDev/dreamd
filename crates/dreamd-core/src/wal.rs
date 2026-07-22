//! Dream cycle write-ahead log (DR-303 / WEG-60).
//!
//! Guarantees `.agent/` is never left in a half-promoted state after a
//! mid-cycle crash. Before any destructive op, the intent is appended to
//! `dream_in_progress.wal`. On startup, if the WAL exists, recovery runs
//! before the daemon serves traffic.
//!
//! v0.1 WAL covers JSONL + LESSONS.md + recurrence sidecar writes only.
//! Tantivy index mutations are NOT WAL-protected in v0.1 — see
//! [`ARCHITECTURE.md`](../../ARCHITECTURE.md) §4 (index freshness contract) and
//! [`assess_index_freshness`](crate::server::tantivy_handle::assess_index_freshness).

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::io::write_atomic;
use crate::layout::AgentRoot;

/// Schema version for daemon state (`state.json`). Versions independently of
/// the record schema (`dreamd_protocol::RECORD_SCHEMA_VERSION`).
pub const STATE_SCHEMA_VERSION: &str = "1.0";

/// Destructive dream-cycle step recorded before it executes.
///
/// On crash recovery without [`WalIntent::Commit`], each non-commit intent's
/// temp file is deleted and the WAL is removed so `.agent/` stays pre-cycle.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "operation", content = "payload")]
pub enum WalIntent {
    /// About to promote a temp LESSONS.md (or semantic memory file) into place.
    ReplaceSemanticMemory { temp_file_path: String },
    /// About to replace episodic JSONL with a pruned temp copy.
    PruneEpisodicMemory { temp_file_path: String },
    /// All destructive steps succeeded; recovery treats the cycle as committed.
    Commit,
}

/// On-disk write-ahead log for an in-flight dream cycle.
///
/// Serialized to `.agent/.dreamd/dream_in_progress.wal`. Each [`WalIntent`]
/// records a destructive step before it executes so crash recovery can unwind
/// partial work. Deleted after a successful [`commit_cycle`](crate::wal::commit_cycle).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DreamWal {
    /// Daemon state schema version (independent of episodic `schema_version`).
    pub schema_version: String,
    /// Ordered list of intents recorded this cycle.
    pub intents: Vec<WalIntent>,
}

/// Result of scanning / applying `dream_in_progress.wal` on startup.
#[derive(Debug, PartialEq)]
pub enum RecoveryOutcome {
    /// No WAL found — nothing to recover.
    Clean,
    /// Incomplete cycle (no Commit): temp files removed, WAL deleted.
    Recovered { cleaned_files: Vec<PathBuf> },
    /// Commit was recorded but post-commit cleanup did not finish; repair state.json.
    CommittedButUnclean,
}

#[derive(Debug, thiserror::Error)]
pub enum WalError {
    #[error("WAL I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("WAL parse: {0}")]
    Json(#[from] serde_json::Error),
    #[error("no .agent/ store at {0} — refusing to scaffold a phantom store; run `dreamd init`")]
    NoAgentStore(PathBuf),
}

/// Write a fresh WAL and set state.json to "in_progress".
/// `_now_sec` is caller-provided for testability.
pub fn begin_cycle(agent_root: &AgentRoot, _now_sec: i64) -> Result<(), WalError> {
    // WEG-281 — never conjure a store. begin_cycle may create the `.dreamd/`
    // subdir, but only under an EXISTING `.agent/`. If `.agent/` is absent the
    // caller resolved the wrong root (or skipped discovery); error rather than
    // create_dir_all a phantom empty store. Last line of defense behind the
    // cli.rs discover gate.
    if !agent_root.agent_dir().exists() {
        return Err(WalError::NoAgentStore(agent_root.agent_dir()));
    }
    let wal = DreamWal {
        schema_version: STATE_SCHEMA_VERSION.to_string(),
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
pub fn recover_if_needed(
    agent_root: &AgentRoot,
    _now_sec: i64,
) -> Result<RecoveryOutcome, WalError> {
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
    Ok(RecoveryOutcome::Recovered {
        cleaned_files: cleaned,
    })
}

/// Check for a stale WAL on daemon startup and run recovery with logging.
///
/// Called from `run_watch` and lazy per-project open before any store access.
pub fn recover_on_startup(agent_root: &AgentRoot) -> Result<RecoveryOutcome, WalError> {
    let now_sec = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let outcome = recover_if_needed(agent_root, now_sec)?;
    match &outcome {
        RecoveryOutcome::Clean => {}
        RecoveryOutcome::Recovered { cleaned_files } => {
            tracing::info!(
                count = cleaned_files.len(),
                "recovered incomplete dream cycle"
            );
        }
        RecoveryOutcome::CommittedButUnclean => {
            tracing::info!("dream cycle committed; finished WAL cleanup on startup");
        }
    }
    Ok(outcome)
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

/// Read `last_dream_cycle_at` from `state.json`.
/// Returns `None` if the file is absent, the key is missing, or the value is JSON null.
pub fn read_cycle_started_at(agent_root: &AgentRoot) -> Result<Option<String>, WalError> {
    let path = agent_root.state_json();
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path)?;
    let v: serde_json::Value = serde_json::from_slice(&bytes)?;
    Ok(v.get("last_dream_cycle_at")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string()))
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
    let schema_version =
        read_schema_version(agent_root).unwrap_or_else(|| STATE_SCHEMA_VERSION.to_string());
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

        assert!(
            !root.wal_path().exists(),
            "WAL must be deleted after commit_cycle"
        );

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
    fn read_last_cycle_status_returns_idle_when_file_missing() {
        let (root, _g) = setup_root("idle-default");
        // state.json does not exist yet
        assert!(!root.state_json().exists());
        let status = read_last_cycle_status(&root).unwrap();
        assert_eq!(status, "idle");
    }

    #[test]
    fn read_last_cycle_status_returns_idle_when_key_missing() {
        let (root, _g) = setup_root("idle-key-missing");
        // state.json exists but has no last_dream_cycle_status key.
        let state = serde_json::json!({"schema_version": "1.0", "daemon_version": "0.0.0"});
        fs::write(
            root.state_json(),
            serde_json::to_string_pretty(&state).unwrap(),
        )
        .unwrap();
        let status = read_last_cycle_status(&root).unwrap();
        assert_eq!(status, "idle");
    }

    #[test]
    fn read_cycle_started_at_returns_none_when_file_missing() {
        let (root, _g) = setup_root("cycle-started-missing");
        assert!(!root.state_json().exists());
        let started = read_cycle_started_at(&root).unwrap();
        assert_eq!(started, None);
    }

    #[test]
    fn read_cycle_started_at_returns_some_after_commit() {
        let (root, _g) = setup_root("cycle-started-set");
        begin_cycle(&root, NOW_SEC).unwrap();
        commit_cycle(&root, NOW_SEC).unwrap();
        let started = read_cycle_started_at(&root).unwrap();
        assert!(started.is_some(), "must return Some after commit");
        assert!(
            started.as_ref().unwrap().contains('T'),
            "expected ISO date with time, got {:?}",
            started
        );
    }

    #[test]
    fn begin_cycle_refuses_missing_agent_store() {
        // A bare tmpdir with no `.agent/` — begin_cycle must error rather than
        // scaffold a phantom store. Do NOT use `setup_root` (it pre-creates
        // `.dreamd/`); we need the un-set-up dir.
        let dir = unique_tmpdir("no-agent-store");
        fs::create_dir_all(&dir).unwrap();
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);

        let err = begin_cycle(&root, NOW_SEC).unwrap_err();
        assert!(
            matches!(err, WalError::NoAgentStore(_)),
            "expected NoAgentStore, got {err:?}",
        );
        assert!(
            !dir.join(".agent").exists(),
            ".agent/ must not be created when begin_cycle refuses",
        );
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
