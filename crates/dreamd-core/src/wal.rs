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

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::daemon_state::{read_daemon_state, update_daemon_state, DaemonStateError};
use crate::io::write_atomic;
use crate::layout::AgentRoot;

/// Schema version for daemon state (`state.json`). Re-exported from
/// [`crate::daemon_state`], which owns the typed writer (AILAB-167);
/// `crate::wal::STATE_SCHEMA_VERSION` remains a valid path.
pub use crate::daemon_state::STATE_SCHEMA_VERSION;

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

impl From<DaemonStateError> for WalError {
    fn from(e: DaemonStateError) -> Self {
        match e {
            DaemonStateError::Io(e) => WalError::Io(e),
            DaemonStateError::Json(e) => WalError::Json(e),
        }
    }
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
    // Status-only patch, deliberately (AILAB-167): a cycle that is starting has
    // no completion timestamp, and the previous cycle's `last_dream_cycle_at`
    // must survive. The pre-AILAB-167 from-scratch writer wrote JSON `null`
    // here and wiped it on every begin.
    update_daemon_state(agent_root, |s| {
        s.last_dream_cycle_status = "in_progress".to_string()
    })?;
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

/// Which [`WalIntent`] a [`guarded_replace`] records for the file it is
/// replacing.
///
/// A *kind*, not an intent: the `temp_file_path` half is supplied by
/// [`guarded_replace`] from the tmp the atomic write actually created, so a
/// caller has no way to name one (AILAB-164).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardedReplaceKind {
    /// `.agent/semantic/LESSONS.md` and friends.
    ReplaceSemanticMemory,
    /// `.agent/episodic/AGENT_LEARNINGS.jsonl`.
    PruneEpisodicMemory,
}

impl GuardedReplaceKind {
    /// Build the on-disk intent for `tmp`. Deliberately private: this is the
    /// **only** place production code mints a `temp_file_path`, and `tmp` can
    /// only come from [`crate::io::write_atomic_with_hook`]'s hook argument.
    fn into_intent(self, tmp: &Path) -> WalIntent {
        let temp_file_path = tmp.to_string_lossy().into_owned();
        match self {
            Self::ReplaceSemanticMemory => WalIntent::ReplaceSemanticMemory { temp_file_path },
            Self::PruneEpisodicMemory => WalIntent::PruneEpisodicMemory { temp_file_path },
        }
    }
}

/// Replace `dest` with `contents` and record the matching [`WalIntent`] as one
/// operation (AILAB-164).
///
/// Sequence, inside [`crate::io::write_atomic_with_hook`]: write `dest`'s `.tmp`,
/// fsync it, **then** append the intent naming that exact tmp, then rename and
/// fsync the parent dir. The intent therefore never references a tmp that does
/// not exist yet, and never a tmp this call did not write — the two halves used
/// to be separate statements at each call site, each re-deriving
/// `dest.with_extension("tmp")` for itself, and `write_selected_lesson` even ran
/// them in the opposite order (intent, *then* write).
///
/// Follows [`append_intent`]'s no-active-cycle rule: with no WAL on disk the
/// intent append is a no-op and the replace still commits. One-shot CLI rewrites
/// with no open cycle (`dreamd archive`) call
/// [`episodic::rewrite_atomic`](crate::episodic::rewrite_atomic) instead and do
/// not come through here at all.
///
/// Returns [`std::io::ErrorKind::Unsupported`] on Windows, inherited from
/// `write_atomic_with_hook`; see `docs/windows.md`.
pub fn guarded_replace(
    agent_root: &AgentRoot,
    dest: &Path,
    contents: &[u8],
    kind: GuardedReplaceKind,
) -> Result<(), WalError> {
    crate::io::write_atomic_with_hook(dest, contents, |tmp| {
        append_intent(agent_root, kind.into_intent(tmp)).map_err(std::io::Error::other)
    })?;
    Ok(())
}

/// [`guarded_replace`] that crashes in the one window recovery exists for:
/// after the `.tmp` is fsynced and the intent is durably appended, before the
/// rename. Leaves `dest` byte-identical, the `.tmp` on disk, and the WAL naming
/// it — exactly the state a `SIGKILL` there would.
///
/// Test-only, and the only supported way to produce that state: hand-writing WAL
/// JSON tests the recovery *reader* against a fixture nothing in production
/// emits.
#[cfg(test)]
pub(crate) fn guarded_replace_crash_after_intent(
    agent_root: &AgentRoot,
    dest: &Path,
    contents: &[u8],
    kind: GuardedReplaceKind,
) -> Result<(), WalError> {
    crate::io::write_atomic_with_hook(dest, contents, |tmp| {
        append_intent(agent_root, kind.into_intent(tmp)).map_err(std::io::Error::other)?;
        Err(std::io::Error::other("injected crash after intent"))
    })?;
    Ok(())
}

/// Append Commit intent, update state.json to "complete", delete WAL.
/// Caller-provided `now_sec`.
pub fn commit_cycle(agent_root: &AgentRoot, now_sec: i64) -> Result<(), WalError> {
    append_intent(agent_root, WalIntent::Commit)?;
    let iso = DateTime::from_timestamp(now_sec, 0)
        .map(|dt: DateTime<Utc>| dt.to_rfc3339())
        .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".to_string());
    update_daemon_state(agent_root, |s| {
        s.last_dream_cycle_status = "complete".to_string();
        s.last_dream_cycle_at = Some(iso);
    })?;
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
        update_daemon_state(agent_root, |s| {
            s.last_dream_cycle_status = "complete".to_string()
        })?;
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
    update_daemon_state(agent_root, |s| {
        s.last_dream_cycle_status = "failed".to_string()
    })?;
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
///
/// Returns `"idle"` if the file is absent or the key is missing — the same two
/// defaults the hand-rolled `serde_json::Value` walk this replaced produced, now
/// inherited from [`DaemonState`](crate::daemon_state::DaemonState)`::default`
/// and its container-level `#[serde(default)]` (AILAB-164 over AILAB-167).
pub fn read_last_cycle_status(agent_root: &AgentRoot) -> Result<String, WalError> {
    Ok(read_daemon_state(agent_root)?.last_dream_cycle_status)
}

/// Read `last_dream_cycle_at` from `state.json`.
///
/// Returns `None` if the file is absent, the key is missing, or the value is
/// JSON null. Reads through the same typed state as
/// [`read_last_cycle_status`]; the writer stays
/// [`update_daemon_state`](crate::daemon_state::update_daemon_state).
pub fn read_cycle_started_at(agent_root: &AgentRoot) -> Result<Option<String>, WalError> {
    Ok(read_daemon_state(agent_root)?.last_dream_cycle_at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_state::{read_daemon_state, AutobiographySkip};
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

    /// Read back the WAL on disk and return the `temp_file_path` of its single
    /// non-`Commit` intent. Pattern-binds by shorthand rather than reaching for
    /// the field, so no test *names* a `temp_file_path` value either.
    fn only_intent_tmp(root: &AgentRoot) -> PathBuf {
        let wal: DreamWal =
            serde_json::from_str(&fs::read_to_string(root.wal_path()).unwrap()).unwrap();
        let paths: Vec<PathBuf> = wal
            .intents
            .iter()
            .filter_map(|i| match i {
                WalIntent::ReplaceSemanticMemory { temp_file_path }
                | WalIntent::PruneEpisodicMemory { temp_file_path } => {
                    Some(PathBuf::from(temp_file_path))
                }
                WalIntent::Commit => None,
            })
            .collect();
        assert_eq!(paths.len(), 1, "expected exactly one replace intent");
        paths.into_iter().next().unwrap()
    }

    /// The seam's happy path: `dest` gets the new bytes, no `.tmp` is left, and
    /// the recorded intent names the tmp the write actually created — not a path
    /// the caller re-derived (AILAB-164).
    #[test]
    fn guarded_replace_records_the_tmp_it_wrote_then_renames_it() {
        let (root, _g) = setup_root("guarded-happy");
        let dest = root.lessons_md();
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, b"old lesson\n").unwrap();

        begin_cycle(&root, NOW_SEC).unwrap();
        guarded_replace(
            &root,
            &dest,
            b"new lesson\n",
            GuardedReplaceKind::ReplaceSemanticMemory,
        )
        .unwrap();

        assert_eq!(fs::read(&dest).unwrap(), b"new lesson\n");
        let tmp = dest.with_extension("tmp");
        assert!(!tmp.exists(), "happy path must leave no stray .tmp");
        assert_eq!(
            only_intent_tmp(&root),
            tmp,
            "intent must name the tmp the write created"
        );
    }

    /// `append_intent`'s no-active-cycle rule survives the wrapper: with no WAL
    /// on disk the replace still commits (AILAB-164 §1.6).
    #[test]
    fn guarded_replace_still_writes_with_no_open_wal() {
        let (root, _g) = setup_root("guarded-no-wal");
        let dest = root.lessons_md();
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        assert!(!root.wal_path().exists(), "no cycle is open");

        guarded_replace(
            &root,
            &dest,
            b"unguarded\n",
            GuardedReplaceKind::ReplaceSemanticMemory,
        )
        .unwrap();

        assert_eq!(fs::read(&dest).unwrap(), b"unguarded\n");
        assert!(!root.wal_path().exists(), "no WAL must be conjured");
    }

    /// Crash in the window recovery exists for — intent durable, rename not yet
    /// done — driven through the real seam rather than a hand-written WAL
    /// fixture, then unwound by `recover_if_needed`.
    #[test]
    fn recover_incomplete_deletes_tmp_and_marks_failed() {
        let (root, _g) = setup_root("recovery");
        let dest = root.episodic_jsonl();
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, b"pre-cycle record\n").unwrap();

        begin_cycle(&root, NOW_SEC).unwrap();
        let err = guarded_replace_crash_after_intent(
            &root,
            &dest,
            b"pruned record\n",
            GuardedReplaceKind::PruneEpisodicMemory,
        )
        .expect_err("injected crash must surface");
        assert!(
            matches!(err, WalError::Io(_)),
            "crash surfaces as I/O, got {err:?}"
        );

        // State at the crash: dest untouched, tmp on disk, WAL naming that tmp.
        let tmp_path = dest.with_extension("tmp");
        assert_eq!(fs::read(&dest).unwrap(), b"pre-cycle record\n");
        assert!(
            tmp_path.exists(),
            ".tmp must survive as the recovery signal"
        );
        assert_eq!(only_intent_tmp(&root), tmp_path);

        let outcome = recover_if_needed(&root, NOW_SEC).unwrap();

        assert!(!tmp_path.exists(), ".tmp file must be deleted by recovery");
        assert!(!root.wal_path().exists(), "WAL must be deleted by recovery");
        assert_eq!(
            fs::read(&dest).unwrap(),
            b"pre-cycle record\n",
            "recovery must leave the destination pre-cycle"
        );

        let state: serde_json::Value =
            serde_json::from_slice(&fs::read(root.state_json()).unwrap()).unwrap();
        assert_eq!(state["last_dream_cycle_status"], "failed");

        assert!(
            matches!(outcome, RecoveryOutcome::Recovered { .. }),
            "outcome must be Recovered"
        );
    }

    fn skip_fixture() -> AutobiographySkip {
        AutobiographySkip {
            at: 42,
            reason: "user_dirty_tree".to_string(),
            files: vec![".agent/semantic/LESSONS.md".to_string()],
        }
    }

    #[test]
    fn skip_marker_survives_begin_cycle() {
        let (root, _g) = setup_root("skip-survives-begin");
        update_daemon_state(&root, |s| s.last_autobiography_skip = Some(skip_fixture())).unwrap();

        begin_cycle(&root, NOW_SEC).unwrap();

        let state = read_daemon_state(&root).unwrap();
        let skip = state
            .last_autobiography_skip
            .expect("begin_cycle must not eat last_autobiography_skip");
        assert_eq!(skip.reason, "user_dirty_tree");
        assert_eq!(state.last_dream_cycle_status, "in_progress");
    }

    /// AILAB-167 AC: a full cycle is begin **and** commit; both edges wrote
    /// state.json from scratch before the typed RMW writer landed.
    #[test]
    fn skip_marker_survives_begin_and_commit() {
        let (root, _g) = setup_root("skip-survives-commit");
        update_daemon_state(&root, |s| s.last_autobiography_skip = Some(skip_fixture())).unwrap();

        begin_cycle(&root, NOW_SEC).unwrap();
        commit_cycle(&root, NOW_SEC).unwrap();

        let state = read_daemon_state(&root).unwrap();
        let skip = state
            .last_autobiography_skip
            .expect("commit_cycle must not eat last_autobiography_skip");
        assert_eq!(skip.reason, "user_dirty_tree");
        assert_eq!(state.last_dream_cycle_status, "complete");
        let at = state
            .last_dream_cycle_at
            .expect("commit_cycle must stamp last_dream_cycle_at");
        assert!(at.contains('T'), "expected ISO date with time, got {at:?}");
    }

    #[test]
    fn last_dream_cycle_at_survives_begin_cycle() {
        let (root, _g) = setup_root("cycle-at-survives-begin");
        begin_cycle(&root, NOW_SEC).unwrap();
        commit_cycle(&root, NOW_SEC).unwrap();

        let committed_at = read_daemon_state(&root)
            .unwrap()
            .last_dream_cycle_at
            .expect("commit_cycle must stamp last_dream_cycle_at");

        begin_cycle(&root, NOW_SEC).unwrap();

        let state = read_daemon_state(&root).unwrap();
        assert_eq!(
            state.last_dream_cycle_at,
            Some(committed_at),
            "begin_cycle must preserve the prior cycle's completion timestamp",
        );
        assert_eq!(state.last_dream_cycle_status, "in_progress");
    }
}
