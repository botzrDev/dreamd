//! `commit_cycle` — git2 autobiography commit for the dream cycle (WEG-63 / DR-311).
//!
//! After every successful dream cycle, this module stages
//! `.agent/semantic/LESSONS.md` and `.agent/episodic/AGENT_LEARNINGS.jsonl`,
//! and commits to the user's outer project git repo with a fixed identity
//! (`dreamd <noreply@dreamd.dev>`) and a fixed message shape
//! (`dreamd: cycle YYYY-MM-DD`).
//!
//! Behavior is best-effort: every failure path returns a typed outcome or
//! error, but neither the HTTP handler (WEG-70) nor the CLI (WEG-64) treats
//! autobiography failure as a cycle failure. The cycle's data writes are
//! already on disk by the time we run; nothing about the audit-trail commit
//! is load-bearing for correctness.
//!
//! Dirty-tree skip semantic (founder decision B11, 2026-05-10):
//! the check fires at CYCLE START, not at commit time. If the user has
//! uncommitted edits on either of the two tracked files when the cycle begins,
//! the cycle still RUNS (and overwrites the user's edits) but the autobiography
//! commit is skipped. The structured WARN log makes the overwrite explicit.

use std::path::{Path, PathBuf};

use git2::{ErrorCode, Repository, Signature, Status};
use serde::{Deserialize, Serialize};

use crate::layout::AgentRoot;

const COMMITTER_NAME: &str = "dreamd";
const COMMITTER_EMAIL: &str = "noreply@dreamd.dev";

/// Paths checked for dirty state and staged at commit time. Relative to
/// the project root (which is the git repo workdir).
const TRACKED_PATHS: &[&str] = &[
    ".agent/semantic/LESSONS.md",
    ".agent/episodic/AGENT_LEARNINGS.jsonl",
];

#[derive(Debug, thiserror::Error)]
pub enum AutobiographyError {
    #[error("git: {0}")]
    Git(#[from] git2::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("state.json serialize: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Outcome of an autobiography attempt. `Committed` is the happy path;
/// `NoRepo` and `Skipped` are capability absences / safety holds.
#[derive(Debug)]
pub enum AutobiographyOutcome {
    /// Commit succeeded; carries the OID of the new commit.
    Committed(git2::Oid),
    /// No git repo found at/above project root. Doctor surfaces; no WARN.
    NoRepo,
    /// User had in-flight edits on tracked files at cycle start. Cycle still
    /// ran (and overwrote the edits); the skip prevents commit. WARN logged.
    Skipped(SkipReason),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    UserDirtyTree { files: Vec<String> },
    NoRepo,
}

/// State.json field that surfaces in `dreamd doctor`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutobiographySkip {
    pub at: i64,
    pub reason: String,
    pub files: Vec<String>,
}

/// Check if any of the two tracked paths have unstaged or staged user edits
/// relative to HEAD. Called at CYCLE START, before the cycle runs.
///
/// Returns the subset of tracked paths that are dirty. Empty Vec → clean.
/// Returns `Ok(Vec::new())` if no git repo is found (capability absence).
pub fn check_dirty_at_cycle_start(
    project_root: &Path,
) -> Result<Vec<PathBuf>, AutobiographyError> {
    let repo = match Repository::discover(project_root) {
        Ok(r) => r,
        Err(e) if e.code() == ErrorCode::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(AutobiographyError::Git(e)),
    };

    let user_dirty = Status::WT_MODIFIED
        | Status::INDEX_MODIFIED
        | Status::WT_NEW
        | Status::INDEX_NEW;

    let mut dirty = Vec::new();
    for p in TRACKED_PATHS {
        let path = Path::new(p);
        match repo.status_file(path) {
            Ok(status) if status.intersects(user_dirty) => dirty.push(path.to_path_buf()),
            Ok(_) => {}
            Err(e) if e.code() == ErrorCode::NotFound => {}
            Err(e) => return Err(AutobiographyError::Git(e)),
        }
    }
    Ok(dirty)
}

/// Run the autobiography commit. Called AFTER the dream cycle's data writes
/// have completed. `dirty_at_cycle_start` is the result of an earlier call
/// to `check_dirty_at_cycle_start` — caller-provided so the dirty-tree
/// semantic is "cycle-start state," not "commit-time state."
pub fn commit_cycle(
    agent_root: &AgentRoot,
    cycle_date: &str,
    dirty_at_cycle_start: &[PathBuf],
) -> Result<AutobiographyOutcome, AutobiographyError> {
    let project_root = agent_root.project_root();

    // 1. Discover repo.
    let repo = match Repository::discover(project_root) {
        Ok(r) => r,
        Err(e) if e.code() == ErrorCode::NotFound => {
            write_skip_marker(
                agent_root,
                AutobiographySkip {
                    at: now_seconds(),
                    reason: "no_repo".to_string(),
                    files: Vec::new(),
                },
            )?;
            return Ok(AutobiographyOutcome::NoRepo);
        }
        Err(e) => return Err(AutobiographyError::Git(e)),
    };

    // 2. Dirty-tree gate (cycle-start state, not commit-time state).
    if !dirty_at_cycle_start.is_empty() {
        let files: Vec<String> = dirty_at_cycle_start
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        let files_str = files.join(", ");
        tracing::warn!(
            dreamd_event = "autobiography_skipped",
            reason = "user_dirty_tree",
            files = ?files,
            "skipping autobiography commit: user edits detected on {} at cycle start \
             (cycle ran and overwrote them; your edits are now part of the file)",
            files_str,
        );
        write_skip_marker(
            agent_root,
            AutobiographySkip {
                at: now_seconds(),
                reason: "user_dirty_tree".to_string(),
                files: files.clone(),
            },
        )?;
        return Ok(AutobiographyOutcome::Skipped(SkipReason::UserDirtyTree { files }));
    }

    // 3. Stage tracked paths.
    let mut index = repo.index()?;
    for p in TRACKED_PATHS {
        index.add_path(Path::new(p))?;
    }
    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;

    // 4. Build the commit. Handle initial-commit (UnbornBranch) case.
    let sig = Signature::now(COMMITTER_NAME, COMMITTER_EMAIL)?;
    let msg = format!("dreamd: cycle {cycle_date}");

    let parents: Vec<git2::Commit> = match repo.head() {
        Ok(head) => vec![head.peel_to_commit()?],
        Err(e) if e.code() == ErrorCode::UnbornBranch => Vec::new(),
        Err(e) => return Err(AutobiographyError::Git(e)),
    };
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();

    let oid = repo.commit(Some("HEAD"), &sig, &sig, &msg, &tree, &parent_refs)?;
    index.write()?;

    // 5. Clear any prior skip marker — we just committed cleanly.
    clear_skip_marker(agent_root)?;

    Ok(AutobiographyOutcome::Committed(oid))
}

/// Read `last_autobiography_skip` from state.json. Returns `None` if the file
/// is absent, unparseable, or the field is missing.
pub fn read_last_skip(agent_root: &AgentRoot) -> Option<AutobiographySkip> {
    let path = agent_root.state_json();
    let text = std::fs::read_to_string(&path).ok()?;
    let state: serde_json::Value = serde_json::from_str(&text).ok()?;
    let skip = state.get("last_autobiography_skip")?;
    serde_json::from_value(skip.clone()).ok()
}

fn write_skip_marker(
    agent_root: &AgentRoot,
    skip: AutobiographySkip,
) -> Result<(), AutobiographyError> {
    let path = agent_root.state_json();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut state: serde_json::Value = if path.exists() {
        serde_json::from_str(&std::fs::read_to_string(&path)?)?
    } else {
        serde_json::json!({})
    };
    state["last_autobiography_skip"] = serde_json::to_value(&skip)?;
    let bytes = serde_json::to_vec_pretty(&state)?;
    crate::io::write_atomic(&path, &bytes)?;
    Ok(())
}

fn clear_skip_marker(agent_root: &AgentRoot) -> Result<(), AutobiographyError> {
    let path = agent_root.state_json();
    if !path.exists() {
        return Ok(());
    }
    let mut state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    if let Some(obj) = state.as_object_mut() {
        obj.remove("last_autobiography_skip");
    }
    let bytes = serde_json::to_vec_pretty(&state)?;
    crate::io::write_atomic(&path, &bytes)?;
    Ok(())
}

/// Current unix-second timestamp for skip markers. This is a leaf call site
/// where caller-injection adds no value — the skip is happening now by definition.
fn now_seconds() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    fn init_repo_with_files(dir: &TempDir) -> Repository {
        let repo = Repository::init(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join(".agent/semantic")).unwrap();
        std::fs::create_dir_all(dir.path().join(".agent/episodic")).unwrap();
        std::fs::write(
            dir.path().join(".agent/semantic/LESSONS.md"),
            b"initial lessons",
        )
        .unwrap();
        std::fs::write(
            dir.path().join(".agent/episodic/AGENT_LEARNINGS.jsonl"),
            b"{}",
        )
        .unwrap();
        repo
    }

    fn make_initial_commit(repo: &Repository, dir: &TempDir) -> git2::Oid {
        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_path(Path::new(".agent/semantic/LESSONS.md"))
            .unwrap();
        index
            .add_path(Path::new(".agent/episodic/AGENT_LEARNINGS.jsonl"))
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let _ = dir; // kept alive
        repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
            .unwrap()
    }

    #[test]
    fn commit_cycle_happy_path() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_files(&dir);
        make_initial_commit(&repo, &dir);

        // Update LESSONS.md so there's a real change to commit.
        std::fs::write(
            dir.path().join(".agent/semantic/LESSONS.md"),
            b"updated lessons",
        )
        .unwrap();

        let agent_root = AgentRoot::new(dir.path());
        let outcome = commit_cycle(&agent_root, "2026-05-26", &[]).unwrap();

        match outcome {
            AutobiographyOutcome::Committed(oid) => {
                let commit = repo.find_commit(oid).unwrap();
                assert_eq!(commit.committer().name().unwrap(), "dreamd");
                assert_eq!(commit.committer().email().unwrap(), "noreply@dreamd.dev");
                assert_eq!(commit.message().unwrap(), "dreamd: cycle 2026-05-26");
                assert_eq!(commit.parent_count(), 1);
            }
            other => panic!("expected Committed, got {other:?}"),
        }
    }

    #[test]
    fn commit_cycle_no_repo() {
        let dir = TempDir::new().unwrap();
        // No git init — not a git repo.
        let agent_root = AgentRoot::new(dir.path());

        let outcome = commit_cycle(&agent_root, "2026-05-26", &[]).unwrap();
        assert!(
            matches!(outcome, AutobiographyOutcome::NoRepo),
            "expected NoRepo"
        );

        // state.json should have skip marker.
        let state_json = std::fs::read_to_string(agent_root.state_json()).unwrap();
        let state: serde_json::Value = serde_json::from_str(&state_json).unwrap();
        assert_eq!(
            state["last_autobiography_skip"]["reason"].as_str().unwrap(),
            "no_repo"
        );
    }

    #[test]
    fn commit_cycle_skips_dirty_tree() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_files(&dir);
        make_initial_commit(&repo, &dir);

        // Modify LESSONS.md without staging — WT_MODIFIED.
        std::fs::write(
            dir.path().join(".agent/semantic/LESSONS.md"),
            b"user edit",
        )
        .unwrap();

        let dirty = check_dirty_at_cycle_start(dir.path()).unwrap();
        assert!(
            !dirty.is_empty(),
            "modified LESSONS.md must appear as dirty"
        );

        let agent_root = AgentRoot::new(dir.path());
        let outcome = commit_cycle(&agent_root, "2026-05-26", &dirty).unwrap();

        assert!(
            matches!(outcome, AutobiographyOutcome::Skipped(SkipReason::UserDirtyTree { .. })),
            "expected Skipped(UserDirtyTree)"
        );

        // Verify the commit was NOT made — HEAD still points to initial.
        let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(
            head_commit.message().unwrap(),
            "initial",
            "no new commit must be created on dirty-tree skip"
        );

        // state.json must have skip marker.
        let state_json = std::fs::read_to_string(agent_root.state_json()).unwrap();
        let state: serde_json::Value = serde_json::from_str(&state_json).unwrap();
        assert_eq!(
            state["last_autobiography_skip"]["reason"].as_str().unwrap(),
            "user_dirty_tree"
        );
    }

    #[test]
    fn commit_cycle_initial_commit() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_files(&dir);
        // No commits yet — UnbornBranch.

        let agent_root = AgentRoot::new(dir.path());
        let outcome = commit_cycle(&agent_root, "2026-05-26", &[]).unwrap();

        match outcome {
            AutobiographyOutcome::Committed(oid) => {
                let commit = repo.find_commit(oid).unwrap();
                assert_eq!(commit.parent_count(), 0, "initial commit must have no parents");
                assert_eq!(commit.message().unwrap(), "dreamd: cycle 2026-05-26");
                assert_eq!(commit.committer().name().unwrap(), "dreamd");
            }
            other => panic!("expected Committed, got {other:?}"),
        }
    }

    #[test]
    fn commit_cycle_clears_prior_skip_marker() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_files(&dir);
        make_initial_commit(&repo, &dir);

        // Pre-write a skip marker.
        let agent_root = AgentRoot::new(dir.path());
        write_skip_marker(
            &agent_root,
            AutobiographySkip {
                at: 0,
                reason: "user_dirty_tree".to_string(),
                files: vec![".agent/semantic/LESSONS.md".to_string()],
            },
        )
        .unwrap();
        assert!(agent_root.state_json().exists());

        // Run a clean commit cycle.
        let outcome = commit_cycle(&agent_root, "2026-05-26", &[]).unwrap();
        assert!(matches!(outcome, AutobiographyOutcome::Committed(_)));

        // Skip marker must be absent.
        let state_json = std::fs::read_to_string(agent_root.state_json()).unwrap();
        let state: serde_json::Value = serde_json::from_str(&state_json).unwrap();
        assert!(
            state.get("last_autobiography_skip").is_none(),
            "skip marker must be cleared after clean commit"
        );
    }

    #[test]
    fn check_dirty_at_cycle_start_clean_tree() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_files(&dir);
        make_initial_commit(&repo, &dir);

        let dirty = check_dirty_at_cycle_start(dir.path()).unwrap();
        assert!(dirty.is_empty(), "clean tree must return empty dirty list");
    }

    #[test]
    fn check_dirty_at_cycle_start_unstaged_edits() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_files(&dir);
        make_initial_commit(&repo, &dir);

        // Modify LESSONS.md on disk without staging.
        std::fs::write(
            dir.path().join(".agent/semantic/LESSONS.md"),
            b"user edit",
        )
        .unwrap();

        let dirty = check_dirty_at_cycle_start(dir.path()).unwrap();
        assert!(
            dirty.iter().any(|p| p.ends_with("LESSONS.md")),
            "unstaged LESSONS.md must be in dirty list"
        );
        let _ = repo; // ensure repo stays alive
    }

    #[test]
    fn check_dirty_at_cycle_start_staged_edits() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_files(&dir);
        make_initial_commit(&repo, &dir);

        // Modify and stage LESSONS.md.
        std::fs::write(
            dir.path().join(".agent/semantic/LESSONS.md"),
            b"staged edit",
        )
        .unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_path(Path::new(".agent/semantic/LESSONS.md"))
            .unwrap();
        index.write().unwrap();

        let dirty = check_dirty_at_cycle_start(dir.path()).unwrap();
        assert!(
            dirty.iter().any(|p| p.ends_with("LESSONS.md")),
            "staged LESSONS.md must be in dirty list"
        );
    }
}
