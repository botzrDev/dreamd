//! `commit_cycle` — git2 autobiography commit for the dream cycle (WEG-63).
//!
//! After every successful dream cycle, this module commits
//! `.agent/semantic/LESSONS.md` and `.agent/episodic/AGENT_LEARNINGS.jsonl` to
//! the user's outer project git repo with a fixed identity
//! (`dreamd <noreply@dreamd.dev>`) and a fixed message shape
//! (`dreamd: cycle YYYY-MM-DD`).
//!
//! Behavior is best-effort: every failure path returns a typed outcome or
//! error, but neither the HTTP handler (WEG-70) nor the CLI (WEG-64) treats
//! autobiography failure as a cycle failure. The cycle's data writes are
//! already on disk by the time this module runs; nothing about the audit-trail
//! commit is load-bearing for correctness.
//!
//! **What holds the commit (BZR-1467).** The check runs at cycle start, not at
//! commit time, and it compares bytes rather than asking git for status. Each
//! tracked file counts as dreamd's when the bytes on disk are bytes a dream
//! cycle wrote:
//! - `LESSONS.md` is dreamd's when it is absent, when it equals HEAD's blob, or
//!   when its SHA-256 equals the one the last cycle recorded;
//! - the log is dreamd's when it is absent, when HEAD's blob is a prefix of it
//!   (later bytes are appends, which come through the coordinator), or when its
//!   first `log_len` bytes hash to what the last cycle recorded;
//! - with no record yet, a file HEAD does not track is dreamd's.
//!
//! The record is `state.json`'s `autobiography_baseline`, which the sequencer
//! updates after every cycle ([`record_baseline`]). A file that was clean at
//! cycle start advances to what the cycle left; a file that was not keeps its
//! old entry, so a hand edit (or `dreamd forget`, `archive --force-unpin`,
//! `memory checkout`) holds the commit on every later cycle until the user
//! commits that file. A store with no record whose files differ from HEAD skips
//! once (`no_baseline`), then compares against the record that cycle wrote. The
//! cycle itself still runs either way; only the commit is skipped, with a WARN
//! naming the files.
//!
//! **What the commit holds.** Exactly the two tracked files, on top of HEAD's
//! tree, built in an in-memory index: the user's other staged files stay staged
//! and stay out of the commit. An absent file is removed from the commit. A file
//! HEAD does not track and `.gitignore` ignores is neither checked nor
//! committed. A cycle that leaves the tree unchanged makes no commit
//! ([`AutobiographyOutcome::Unchanged`]). A project nested below its repo's
//! root checks and commits its own `.agent/` files.

use std::path::{Path, PathBuf};

use git2::{ErrorCode, Repository, Signature};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::daemon_state::{update_daemon_state, DaemonStateError};
use crate::layout::AgentRoot;

const COMMITTER_NAME: &str = "dreamd";
const COMMITTER_EMAIL: &str = "noreply@dreamd.dev";

const LESSONS_PATH: &str = ".agent/semantic/LESSONS.md";
const LOG_PATH: &str = ".agent/episodic/AGENT_LEARNINGS.jsonl";

/// The two files a cycle checks and commits, relative to the project root.
///
/// Each is joined to the project root's path inside the repo: the canonical
/// project root with the canonical repo workdir stripped off. A project nested
/// below its repo's root therefore checks and commits its own store, under
/// paths such as `examples/solo-rust-dev/.agent/semantic/LESSONS.md`.
const TRACKED_PATHS: [&str; 2] = [LESSONS_PATH, LOG_PATH];

/// Why an autobiography check or commit failed.
#[derive(Debug, thiserror::Error)]
pub enum AutobiographyError {
    /// A git2 call failed for a reason other than a missing repo.
    #[error("git: {0}")]
    Git(#[from] git2::Error),
    /// Reading or writing `state.json` failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// `state.json` could not be parsed or serialized.
    #[error("state.json serialize: {0}")]
    Serde(#[from] serde_json::Error),
}

impl From<DaemonStateError> for AutobiographyError {
    fn from(e: DaemonStateError) -> Self {
        match e {
            DaemonStateError::Io(e) => AutobiographyError::Io(e),
            DaemonStateError::Json(e) => AutobiographyError::Serde(e),
        }
    }
}

/// Outcome of an autobiography attempt. `Committed` is the happy path;
/// `Unchanged`, `NoRepo` and `Skipped` make no commit.
#[derive(Debug)]
pub enum AutobiographyOutcome {
    /// Commit succeeded; carries the OID of the new commit.
    Committed(git2::Oid),
    /// The two tracked files already match HEAD, so there was nothing to
    /// commit and no empty commit was made (BZR-1467).
    Unchanged,
    /// No git repo found at/above project root. Doctor surfaces; no WARN.
    NoRepo,
    /// A tracked file held bytes no dream cycle wrote when the cycle started.
    /// The cycle still ran; the skip holds the commit until the user commits
    /// that file. WARN logged, naming the files.
    Skipped(SkipReason),
}

/// Why an autobiography commit was skipped.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// A tracked file changed outside a dream cycle; `files` names them.
    UserDirtyTree { files: Vec<String> },
    /// BZR-1467: no baseline yet, and a tracked file differs from HEAD. The
    /// cycle records one, so the next cycle can tell.
    NoBaseline { files: Vec<String> },
    /// Never constructed: a missing repo is [`AutobiographyOutcome::NoRepo`].
    NoRepo,
}

impl SkipReason {
    /// The `state.json` / HTTP spelling of the reason.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UserDirtyTree { .. } => "user_dirty_tree",
            Self::NoBaseline { .. } => "no_baseline",
            Self::NoRepo => "no_repo",
        }
    }

    /// The repo-relative files the skip names.
    pub fn files(&self) -> &[String] {
        match self {
            Self::UserDirtyTree { files } | Self::NoBaseline { files } => files,
            Self::NoRepo => &[],
        }
    }
}

/// State.json field that surfaces in `dreamd doctor`. Owned by
/// [`crate::daemon_state`] (AILAB-167) so both `state.json` writers can name
/// it without a module cycle; re-exported here for existing call sites.
pub use crate::daemon_state::AutobiographySkip;

/// What the last cycle left in the tracked files (BZR-1467); owned by
/// [`crate::daemon_state`] like [`AutobiographySkip`].
pub use crate::daemon_state::AutobiographyBaseline;

/// What the cycle-start check saw (BZR-1467).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartCheck {
    /// Repo-relative `LESSONS.md` when its bytes are not a dream cycle's.
    pub lessons: Option<PathBuf>,
    /// Repo-relative episodic log when its bytes are not a dream cycle's.
    pub log: Option<PathBuf>,
    /// `state.json` held a baseline when the check ran.
    pub had_baseline: bool,
}

impl StartCheck {
    /// The files that hold the commit, `LESSONS.md` first, as git names them.
    pub fn files(&self) -> Vec<String> {
        self.lessons
            .iter()
            .chain(self.log.iter())
            .map(|p| p.display().to_string())
            .collect()
    }

    /// Neither tracked file holds bytes outside a dream cycle's.
    pub fn is_clean(&self) -> bool {
        self.lessons.is_none() && self.log.is_none()
    }
}

/// Lowercase hex SHA-256.
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// D1: `LESSONS.md` is dreamd's when it equals HEAD's blob or the recorded
/// hash; with no record, when HEAD does not track it.
fn lessons_is_dreamds(
    bytes: &[u8],
    head: Option<&[u8]>,
    baseline: Option<&AutobiographyBaseline>,
) -> bool {
    if head == Some(bytes) {
        return true;
    }
    match baseline {
        Some(b) => b.lessons_sha256.as_deref() == Some(sha256_hex(bytes).as_str()),
        None => head.is_none(),
    }
}

/// D1: the log is dreamd's when HEAD's blob is a prefix of it, or its first
/// `log_len` bytes match the record; with no record, when HEAD does not track
/// it. Bytes after either prefix are appends.
fn log_is_dreamds(
    bytes: &[u8],
    head: Option<&[u8]>,
    baseline: Option<&AutobiographyBaseline>,
) -> bool {
    if head.is_some_and(|h| bytes.starts_with(h)) {
        return true;
    }
    match baseline {
        Some(b) => usize::try_from(b.log_len)
            .ok()
            .and_then(|n| bytes.get(..n))
            .is_some_and(|prefix| sha256_hex(prefix) == b.log_sha256),
        None => head.is_none(),
    }
}

/// The repo, its canonical workdir, and the project root's path inside it.
struct RepoView {
    repo: Repository,
    workdir: PathBuf,
    prefix: PathBuf,
}

/// `Ok(None)` when no repo is found at or above `project_root`.
fn open_repo(project_root: &Path) -> Result<Option<RepoView>, AutobiographyError> {
    let repo = match Repository::discover(project_root) {
        Ok(r) => r,
        Err(e) if e.code() == ErrorCode::NotFound => return Ok(None),
        Err(e) => return Err(AutobiographyError::Git(e)),
    };
    let workdir = repo
        .workdir()
        .ok_or_else(|| git2::Error::from_str("bare repository has no workdir"))?
        .canonicalize()?;
    let prefix = project_root
        .canonicalize()?
        .strip_prefix(&workdir)
        .map_err(|_| git2::Error::from_str("project root is outside the repository workdir"))?
        .to_path_buf();
    Ok(Some(RepoView {
        repo,
        workdir,
        prefix,
    }))
}

/// HEAD's commit; `None` on an unborn branch.
fn head_commit(repo: &Repository) -> Result<Option<git2::Commit<'_>>, AutobiographyError> {
    match repo.head() {
        Ok(head) => Ok(Some(head.peel_to_commit()?)),
        Err(e) if matches!(e.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => Ok(None),
        Err(e) => Err(AutobiographyError::Git(e)),
    }
}

/// The bytes HEAD's tree holds at `rel`; `None` when it holds nothing there.
fn head_blob(
    repo: &Repository,
    tree: Option<&git2::Tree<'_>>,
    rel: &Path,
) -> Result<Option<Vec<u8>>, AutobiographyError> {
    let Some(tree) = tree else {
        return Ok(None);
    };
    let entry = match tree.get_path(rel) {
        Ok(entry) => entry,
        Err(e) if e.code() == ErrorCode::NotFound => return Ok(None),
        Err(e) => return Err(AutobiographyError::Git(e)),
    };
    Ok(Some(repo.find_blob(entry.id())?.content().to_vec()))
}

type Rule = fn(&[u8], Option<&[u8]>, Option<&AutobiographyBaseline>) -> bool;

/// Check one tracked path; `Some(rel)` when its bytes are not a dream cycle's.
fn check_path(
    view: &RepoView,
    head_tree: Option<&git2::Tree<'_>>,
    path: &str,
    rule: Rule,
    baseline: Option<&AutobiographyBaseline>,
) -> Result<Option<PathBuf>, AutobiographyError> {
    let rel = view.prefix.join(path);
    let bytes = match std::fs::read(view.workdir.join(&rel)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(AutobiographyError::Io(e)),
    };
    let head = head_blob(&view.repo, head_tree, &rel)?;
    if head.is_none() && view.repo.is_path_ignored(&rel)? {
        return Ok(None);
    }
    Ok((!rule(&bytes, head.as_deref(), baseline)).then_some(rel))
}

/// Check whether either tracked file holds bytes no dream cycle wrote. Called
/// at cycle start, before the cycle runs, with `state.json`'s baseline.
///
/// No repo at or above `project_root` is a clean start (capability absence).
pub fn check_dirty_at_cycle_start(
    project_root: &Path,
    baseline: Option<&AutobiographyBaseline>,
) -> Result<StartCheck, AutobiographyError> {
    let mut start = StartCheck {
        had_baseline: baseline.is_some(),
        ..StartCheck::default()
    };
    let Some(view) = open_repo(project_root)? else {
        return Ok(start);
    };
    let head = head_commit(&view.repo)?;
    let head_tree = head.as_ref().map(|c| c.tree()).transpose()?;
    start.lessons = check_path(
        &view,
        head_tree.as_ref(),
        LESSONS_PATH,
        lessons_is_dreamds,
        baseline,
    )?;
    start.log = check_path(
        &view,
        head_tree.as_ref(),
        LOG_PATH,
        log_is_dreamds,
        baseline,
    )?;
    Ok(start)
}

/// Read `path`, or `None` when it does not exist.
fn read_if_present(path: &Path) -> Result<Option<Vec<u8>>, std::io::Error> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Read what is on disk now (`agent_root.lessons_md()`, `agent_root.episodic_jsonl()`).
/// Absent `LESSONS.md` → `None`; absent log → `log_len` 0 and the hash of no bytes.
pub fn capture_baseline(
    agent_root: &AgentRoot,
) -> Result<AutobiographyBaseline, AutobiographyError> {
    let lessons_sha256 = read_if_present(&agent_root.lessons_md())?.map(|b| sha256_hex(&b));
    let log = read_if_present(&agent_root.episodic_jsonl())?.unwrap_or_default();
    Ok(AutobiographyBaseline {
        lessons_sha256,
        log_len: log.len() as u64,
        log_sha256: sha256_hex(&log),
    })
}

/// D2: clean files advance to `captured`; a file dirty at start keeps `prev`'s entry.
/// With no `prev`, everything advances (D3).
pub fn next_baseline(
    prev: Option<&AutobiographyBaseline>,
    mut captured: AutobiographyBaseline,
    start: &StartCheck,
) -> AutobiographyBaseline {
    if let Some(prev) = prev {
        if start.lessons.is_some() {
            captured.lessons_sha256 = prev.lessons_sha256.clone();
        }
        if start.log.is_some() {
            captured.log_len = prev.log_len;
            captured.log_sha256 = prev.log_sha256.clone();
        }
    }
    captured
}

/// Capture, apply [`next_baseline`], and write it through `update_daemon_state`.
pub fn record_baseline(
    agent_root: &AgentRoot,
    prev: Option<&AutobiographyBaseline>,
    start: &StartCheck,
) -> Result<(), AutobiographyError> {
    let next = next_baseline(prev, capture_baseline(agent_root)?, start);
    update_daemon_state(agent_root, |s| s.autobiography_baseline = Some(next))?;
    Ok(())
}

/// The one text for the WARN and the CLI (D9). `files` joined with ", ".
pub fn skip_message(reason: &str, files: &[String]) -> String {
    let names = files.join(", ");
    match reason {
        "user_dirty_tree" => format!(
            "no git commit for this cycle: {names} changed outside a dream cycle. \
             Commit those changes with git, and the next cycle commits again."
        ),
        "no_baseline" => format!(
            "no git commit for this cycle: {names} differ from HEAD, and dreamd has no \
             record yet of what its last cycle wrote. The next cycle commits."
        ),
        other => format!("no git commit for this cycle ({other}): {names}"),
    }
}

/// The autobiography outcome as `POST /api/v1/dream` reports it and the CLI prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AutobiographyReport {
    /// A commit was made; `commit` is its id.
    Committed { commit: String },
    /// Nothing to commit; no commit was made.
    Unchanged,
    /// No git repo at or above the project root.
    NoRepo,
    /// The commit was held; `reason` is [`SkipReason::as_str`].
    Skipped { reason: String, files: Vec<String> },
}

impl From<&AutobiographyOutcome> for AutobiographyReport {
    fn from(outcome: &AutobiographyOutcome) -> Self {
        match outcome {
            AutobiographyOutcome::Committed(oid) => Self::Committed {
                commit: oid.to_string(),
            },
            AutobiographyOutcome::Unchanged => Self::Unchanged,
            AutobiographyOutcome::NoRepo => Self::NoRepo,
            AutobiographyOutcome::Skipped(reason) => Self::Skipped {
                reason: reason.as_str().to_string(),
                files: reason.files().to_vec(),
            },
        }
    }
}

impl AutobiographyReport {
    /// The line `dreamd dream` prints: [`skip_message`] for a skip, else nothing.
    pub fn notice(&self) -> Option<String> {
        match self {
            Self::Skipped { reason, files } => Some(skip_message(reason, files)),
            _ => None,
        }
    }
}

/// Run the autobiography commit. Called after the dream cycle's data writes
/// have completed. `start` is the result of [`check_dirty_at_cycle_start`],
/// taken before the cycle ran, so the hold is "cycle-start state", not
/// "commit-time state".
pub fn commit_cycle(
    agent_root: &AgentRoot,
    cycle_date: &str,
    start: &StartCheck,
) -> Result<AutobiographyOutcome, AutobiographyError> {
    // 1. Discover repo.
    let Some(view) = open_repo(agent_root.project_root())? else {
        write_skip_marker(
            agent_root,
            AutobiographySkip {
                at: now_seconds(),
                reason: "no_repo".to_string(),
                files: Vec::new(),
            },
        )?;
        return Ok(AutobiographyOutcome::NoRepo);
    };
    let repo = &view.repo;

    // 2. Hold the commit when a tracked file held bytes no cycle wrote at start.
    if !start.is_clean() {
        let files = start.files();
        let reason = if start.had_baseline {
            SkipReason::UserDirtyTree {
                files: files.clone(),
            }
        } else {
            SkipReason::NoBaseline {
                files: files.clone(),
            }
        };
        tracing::warn!(
            dreamd_event = "autobiography_skipped",
            reason = reason.as_str(),
            files = ?files,
            "{}",
            skip_message(reason.as_str(), &files),
        );
        write_skip_marker(
            agent_root,
            AutobiographySkip {
                at: now_seconds(),
                reason: reason.as_str().to_string(),
                files,
            },
        )?;
        return Ok(AutobiographyOutcome::Skipped(reason));
    }

    // 3. The commit's tree: HEAD's tree plus the two tracked paths, in an
    //    in-memory index, so the user's other staged files stay out of it.
    let head = head_commit(repo)?;
    let head_tree = head.as_ref().map(|c| c.tree()).transpose()?;
    let mut tree_index = git2::Index::new()?;
    if let Some(t) = &head_tree {
        tree_index.read_tree(t)?;
    }
    let mut repo_index = repo.index()?;
    for p in TRACKED_PATHS {
        let rel = view.prefix.join(p);
        let in_head = head_tree.as_ref().is_some_and(|t| t.get_path(&rel).is_ok());
        if view.workdir.join(&rel).is_file() {
            if !in_head && repo.is_path_ignored(&rel)? {
                continue;
            }
            repo_index.add_path(&rel)?;
            let entry = repo_index
                .get_path(&rel, 0)
                .ok_or_else(|| git2::Error::from_str("staged path missing from the index"))?;
            tree_index.add(&entry)?;
        } else {
            repo_index.remove_path(&rel)?;
            tree_index.remove_path(&rel)?;
        }
    }
    let tree_id = tree_index.write_tree_to(repo)?;
    let unchanged = match &head_tree {
        Some(t) => t.id() == tree_id,
        None => tree_index.is_empty(),
    };

    // 4. No empty commits (D7).
    if unchanged {
        repo_index.write()?;
        clear_skip_marker(agent_root)?;
        return Ok(AutobiographyOutcome::Unchanged);
    }

    // 5. Commit on top of HEAD (no parent on an unborn branch).
    let tree = repo.find_tree(tree_id)?;
    let sig = Signature::now(COMMITTER_NAME, COMMITTER_EMAIL)?;
    let msg = format!("dreamd: cycle {cycle_date}");
    let parents: Vec<&git2::Commit<'_>> = head.iter().collect();
    let oid = repo.commit(Some("HEAD"), &sig, &sig, &msg, &tree, &parents)?;
    repo_index.write()?;

    // 6. Clear any prior skip marker — this commit went through cleanly.
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
    update_daemon_state(agent_root, |s| s.last_autobiography_skip = Some(skip))?;
    Ok(())
}

fn clear_skip_marker(agent_root: &AgentRoot) -> Result<(), AutobiographyError> {
    // Early return preserved: clearing a marker that was never written must not
    // create `state.json`.
    if !agent_root.state_json().exists() {
        return Ok(());
    }
    update_daemon_state(agent_root, |s| s.last_autobiography_skip = None)?;
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
    use git2::Status;
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

    // ── BZR-1467 helpers ────────────────────────────────────────────────────

    /// Two log lines, both unpinned.
    const TWO_LINES: &[u8] = b"{\"id\":\"a\",\"pinned\":false}\n{\"id\":\"b\",\"pinned\":false}\n";

    /// The user's identity: a temp HOME has no git config to read one from.
    fn user_sig() -> Signature<'static> {
        Signature::now("user", "user@example.com").unwrap()
    }

    /// Write `rel` under `root`, creating its parents.
    fn put(root: &Path, rel: &str, bytes: &[u8]) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// Append `bytes` to `root/rel`.
    fn append(root: &Path, rel: &str, bytes: &[u8]) {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(root.join(rel))
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    /// Commit `paths` as the user through the repo index, the way `git add`
    /// and `git commit` would: an absent path is removed.
    fn user_commit(repo: &Repository, paths: &[&str], msg: &str) -> git2::Oid {
        let workdir = repo.workdir().unwrap().to_path_buf();
        let mut index = repo.index().unwrap();
        for p in paths {
            if workdir.join(p).exists() {
                index.add_path(Path::new(p)).unwrap();
            } else {
                index.remove_path(Path::new(p)).unwrap();
            }
        }
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let parent = repo.head().ok().map(|h| h.peel_to_commit().unwrap());
        let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
        let sig = user_sig();
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parents)
            .unwrap()
    }

    fn head_message(repo: &Repository) -> String {
        repo.head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .message()
            .unwrap()
            .to_string()
    }

    /// The blob at `path` in commit `oid`'s tree, or `None`.
    fn blob_at(repo: &Repository, oid: git2::Oid, path: &str) -> Option<Vec<u8>> {
        let tree = repo.find_commit(oid).unwrap().tree().unwrap();
        let entry = tree.get_path(Path::new(path)).ok()?;
        Some(repo.find_blob(entry.id()).unwrap().content().to_vec())
    }

    fn committed(outcome: AutobiographyOutcome) -> git2::Oid {
        match outcome {
            AutobiographyOutcome::Committed(oid) => oid,
            other => panic!("expected Committed, got {other:?}"),
        }
    }

    fn state_json(root: &AgentRoot) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(root.state_json()).unwrap()).unwrap()
    }

    fn baseline_of(lessons: Option<&[u8]>, log: &[u8]) -> AutobiographyBaseline {
        AutobiographyBaseline {
            lessons_sha256: lessons.map(sha256_hex),
            log_len: log.len() as u64,
            log_sha256: sha256_hex(log),
        }
    }

    // ── Existing tests, on the new signatures (test 18) ─────────────────────

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
        let outcome = commit_cycle(&agent_root, "2026-05-26", &StartCheck::default()).unwrap();

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

        let outcome = commit_cycle(&agent_root, "2026-05-26", &StartCheck::default()).unwrap();
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
        let agent_root = AgentRoot::new(dir.path());
        let baseline = capture_baseline(&agent_root).unwrap();

        // Modify LESSONS.md without staging: bytes no cycle wrote.
        std::fs::write(dir.path().join(".agent/semantic/LESSONS.md"), b"user edit").unwrap();

        let start = check_dirty_at_cycle_start(dir.path(), Some(&baseline)).unwrap();
        assert!(
            start.lessons.is_some(),
            "modified LESSONS.md must appear as dirty"
        );

        let outcome = commit_cycle(&agent_root, "2026-05-26", &start).unwrap();

        assert!(
            matches!(
                outcome,
                AutobiographyOutcome::Skipped(SkipReason::UserDirtyTree { .. })
            ),
            "expected Skipped(UserDirtyTree)"
        );

        // Verify the commit was not made — HEAD still points to initial.
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
        let outcome = commit_cycle(&agent_root, "2026-05-26", &StartCheck::default()).unwrap();

        match outcome {
            AutobiographyOutcome::Committed(oid) => {
                let commit = repo.find_commit(oid).unwrap();
                assert_eq!(
                    commit.parent_count(),
                    0,
                    "initial commit must have no parents"
                );
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
        // A change to commit: an unchanged tree is `Unchanged` (BZR-1467 D7),
        // which `unchanged_tree_makes_no_commit_and_clears_the_marker` covers.
        std::fs::write(
            dir.path().join(".agent/semantic/LESSONS.md"),
            b"cycle lessons",
        )
        .unwrap();

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
        let outcome = commit_cycle(&agent_root, "2026-05-26", &StartCheck::default()).unwrap();
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

        let start = check_dirty_at_cycle_start(dir.path(), None).unwrap();
        assert!(start.is_clean(), "clean tree must return a clean start");
    }

    #[test]
    fn check_dirty_at_cycle_start_unstaged_edits() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_files(&dir);
        make_initial_commit(&repo, &dir);

        // Modify LESSONS.md on disk without staging.
        std::fs::write(dir.path().join(".agent/semantic/LESSONS.md"), b"user edit").unwrap();

        let start = check_dirty_at_cycle_start(dir.path(), None).unwrap();
        assert!(start.lessons.is_some(), "unstaged LESSONS.md must be dirty");
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

        let start = check_dirty_at_cycle_start(dir.path(), None).unwrap();
        assert!(start.lessons.is_some(), "staged LESSONS.md must be dirty");
    }

    // ── BZR-1467 ────────────────────────────────────────────────────────────

    /// Test 1: the two content rules, alone.
    #[test]
    fn rules_pure() {
        let base = baseline_of(Some(b"L1"), b"line1\n");

        // LESSONS.md
        assert!(lessons_is_dreamds(b"L", Some(b"L"), None), "HEAD equal");
        assert!(
            lessons_is_dreamds(b"L", Some(b"L"), Some(&base)),
            "HEAD equal, baseline unequal"
        );
        assert!(
            lessons_is_dreamds(b"L1", Some(b"other"), Some(&base)),
            "baseline hash equal"
        );
        assert!(
            !lessons_is_dreamds(b"edit", Some(b"other"), Some(&base)),
            "baseline hash unequal"
        );
        assert!(lessons_is_dreamds(b"L", None, None), "fresh, no baseline");
        assert!(
            !lessons_is_dreamds(b"edit", Some(b"L"), None),
            "in HEAD, no baseline"
        );
        assert!(
            !lessons_is_dreamds(b"L", None, Some(&baseline_of(None, b""))),
            "the baseline recorded no file"
        );

        // The episodic log
        assert!(log_is_dreamds(b"a\nb\n", Some(b"a\n"), None), "HEAD prefix");
        assert!(log_is_dreamds(b"a\n", Some(b"a\n"), None), "HEAD equal");
        assert!(
            !log_is_dreamds(b"x\nb\n", Some(b"a\n"), None),
            "not a HEAD prefix, no baseline"
        );
        assert!(log_is_dreamds(b"a\n", None, None), "fresh, no baseline");
        assert!(
            log_is_dreamds(b"line1\nline2\n", Some(b"zz"), Some(&base)),
            "baseline prefix match"
        );
        assert!(
            log_is_dreamds(b"line1\n", Some(b"zz"), Some(&base)),
            "baseline exact match"
        );
        assert!(
            !log_is_dreamds(b"LINE1\nline2\n", Some(b"zz"), Some(&base)),
            "baseline prefix mismatch"
        );
        assert!(
            !log_is_dreamds(b"line", Some(b"zz"), Some(&base)),
            "file shorter than log_len"
        );
        assert!(
            log_is_dreamds(b"anything", Some(b"zz"), Some(&baseline_of(None, b""))),
            "an empty-log baseline accepts any log"
        );
    }

    /// Test 2: a fresh store's untracked files are dreamd's.
    #[test]
    fn check_fresh_store_is_clean() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        put(dir.path(), "README", b"readme\n");
        user_commit(&repo, &["README"], "readme");
        put(dir.path(), LESSONS_PATH, b"lessons\n");
        put(dir.path(), LOG_PATH, TWO_LINES);
        assert_eq!(
            check_dirty_at_cycle_start(dir.path(), None).unwrap(),
            StartCheck::default()
        );

        let unborn = TempDir::new().unwrap();
        Repository::init(unborn.path()).unwrap();
        put(unborn.path(), LOG_PATH, TWO_LINES);
        assert_eq!(
            check_dirty_at_cycle_start(unborn.path(), None).unwrap(),
            StartCheck::default()
        );
    }

    /// Test 3: appends after the user's commit are dreamd's, with no baseline.
    #[test]
    fn check_appends_are_clean_without_a_baseline() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        put(dir.path(), LESSONS_PATH, b"lessons\n");
        put(dir.path(), LOG_PATH, TWO_LINES);
        user_commit(&repo, &TRACKED_PATHS, "initial");
        append(dir.path(), LOG_PATH, b"{\"id\":\"c\",\"pinned\":false}\n");

        assert!(check_dirty_at_cycle_start(dir.path(), None)
            .unwrap()
            .is_clean());
    }

    /// Test 4: a cycle's own rewrite is dreamd's once its baseline is recorded.
    #[test]
    fn check_cycle_rewrite_is_clean_against_its_baseline() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        put(dir.path(), LESSONS_PATH, b"lessons\n");
        put(dir.path(), LOG_PATH, TWO_LINES);
        user_commit(&repo, &TRACKED_PATHS, "initial");

        // What a cycle writes: a new LESSONS.md and a pin flip.
        put(dir.path(), LESSONS_PATH, b"cycle lessons\n");
        put(
            dir.path(),
            LOG_PATH,
            b"{\"id\":\"a\",\"pinned\":true}\n{\"id\":\"b\",\"pinned\":false}\n",
        );
        let baseline = capture_baseline(&AgentRoot::new(dir.path())).unwrap();
        append(dir.path(), LOG_PATH, b"{\"id\":\"c\",\"pinned\":false}\n");

        assert_eq!(
            check_dirty_at_cycle_start(dir.path(), Some(&baseline)).unwrap(),
            StartCheck {
                had_baseline: true,
                ..StartCheck::default()
            }
        );
        let without = check_dirty_at_cycle_start(dir.path(), None).unwrap();
        assert_eq!(without.lessons.as_deref(), Some(Path::new(LESSONS_PATH)));
        assert_eq!(without.log.as_deref(), Some(Path::new(LOG_PATH)));
        assert_eq!(without.files(), vec![LESSONS_PATH, LOG_PATH]);
        assert!(!without.had_baseline);
    }

    /// Test 5: a hand edit is not dreamd's, even with a baseline, and only the
    /// edited file is named.
    #[test]
    fn check_hand_edits_are_dirty_against_a_baseline() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        put(dir.path(), LESSONS_PATH, b"lessons\n");
        put(dir.path(), LOG_PATH, TWO_LINES);
        user_commit(&repo, &TRACKED_PATHS, "initial");
        let baseline = capture_baseline(&AgentRoot::new(dir.path())).unwrap();

        put(dir.path(), LESSONS_PATH, b"my edit\n");
        let start = check_dirty_at_cycle_start(dir.path(), Some(&baseline)).unwrap();
        assert_eq!(start.lessons.as_deref(), Some(Path::new(LESSONS_PATH)));
        assert_eq!(start.log, None);

        put(dir.path(), LESSONS_PATH, b"lessons\n");
        put(
            dir.path(),
            LOG_PATH,
            b"{\"id\":\"A\",\"pinned\":false}\n{\"id\":\"b\",\"pinned\":false}\n",
        );
        let start = check_dirty_at_cycle_start(dir.path(), Some(&baseline)).unwrap();
        assert_eq!(start.lessons, None);
        assert_eq!(start.log.as_deref(), Some(Path::new(LOG_PATH)));
    }

    /// Test 6: the skip reason follows whether a baseline existed.
    #[test]
    fn commit_cycle_skip_reason_follows_the_baseline() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        put(dir.path(), LESSONS_PATH, b"lessons\n");
        put(dir.path(), LOG_PATH, TWO_LINES);
        user_commit(&repo, &TRACKED_PATHS, "initial");
        let root = AgentRoot::new(dir.path());
        let baseline = capture_baseline(&root).unwrap();
        put(dir.path(), LESSONS_PATH, b"my edit\n");

        let start = check_dirty_at_cycle_start(dir.path(), Some(&baseline)).unwrap();
        match commit_cycle(&root, "2026-11-03", &start).unwrap() {
            AutobiographyOutcome::Skipped(SkipReason::UserDirtyTree { files }) => {
                assert_eq!(files, vec![LESSONS_PATH]);
            }
            other => panic!("expected Skipped(UserDirtyTree), got {other:?}"),
        }
        let state = state_json(&root);
        assert_eq!(
            state["last_autobiography_skip"]["reason"],
            "user_dirty_tree"
        );
        assert_eq!(
            state["last_autobiography_skip"]["files"],
            serde_json::json!([LESSONS_PATH])
        );
        assert_eq!(head_message(&repo), "initial");

        let start = check_dirty_at_cycle_start(dir.path(), None).unwrap();
        assert!(!start.had_baseline);
        match commit_cycle(&root, "2026-11-03", &start).unwrap() {
            AutobiographyOutcome::Skipped(SkipReason::NoBaseline { files }) => {
                assert_eq!(files, vec![LESSONS_PATH]);
            }
            other => panic!("expected Skipped(NoBaseline), got {other:?}"),
        }
        let state = state_json(&root);
        assert_eq!(state["last_autobiography_skip"]["reason"], "no_baseline");
        assert_eq!(
            state["last_autobiography_skip"]["files"],
            serde_json::json!([LESSONS_PATH])
        );
        assert_eq!(head_message(&repo), "initial");
    }

    /// Test 7: an ignored store HEAD does not track is neither checked nor
    /// committed (D6), and the unchanged tree makes no commit (D7).
    #[test]
    fn ignored_untracked_store_is_not_checked_or_committed() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        put(dir.path(), ".gitignore", b".agent/\n");
        user_commit(&repo, &[".gitignore"], "ignore the store");
        put(dir.path(), LESSONS_PATH, b"lessons\n");
        put(dir.path(), LOG_PATH, TWO_LINES);
        let root = AgentRoot::new(dir.path());

        let matches_nothing = AutobiographyBaseline {
            lessons_sha256: Some("00".to_string()),
            log_len: 3,
            log_sha256: "00".to_string(),
        };
        assert_eq!(
            check_dirty_at_cycle_start(dir.path(), Some(&matches_nothing)).unwrap(),
            StartCheck {
                had_baseline: true,
                ..StartCheck::default()
            }
        );
        let outcome = commit_cycle(&root, "2026-11-03", &StartCheck::default()).unwrap();
        assert!(
            matches!(outcome, AutobiographyOutcome::Unchanged),
            "got {outcome:?}"
        );
        assert_eq!(head_message(&repo), "ignore the store");

        let unborn = TempDir::new().unwrap();
        let unborn_repo = Repository::init(unborn.path()).unwrap();
        put(unborn.path(), ".gitignore", b".agent/\n");
        put(unborn.path(), LESSONS_PATH, b"lessons\n");
        put(unborn.path(), LOG_PATH, TWO_LINES);
        let outcome = commit_cycle(
            &AgentRoot::new(unborn.path()),
            "2026-11-03",
            &StartCheck::default(),
        )
        .unwrap();
        assert!(
            matches!(outcome, AutobiographyOutcome::Unchanged),
            "got {outcome:?}"
        );
        assert!(
            unborn_repo.head().is_err(),
            "no commit on the unborn branch"
        );
    }

    /// Test 8: a path HEAD already tracks is committed even when ignored.
    #[test]
    fn ignored_but_tracked_store_is_committed() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        put(dir.path(), LESSONS_PATH, b"lessons\n");
        put(dir.path(), LOG_PATH, TWO_LINES);
        user_commit(&repo, &TRACKED_PATHS, "initial");
        put(dir.path(), ".gitignore", b".agent/\n");
        append(dir.path(), LOG_PATH, b"{\"id\":\"c\",\"pinned\":false}\n");

        let oid = committed(
            commit_cycle(
                &AgentRoot::new(dir.path()),
                "2026-11-03",
                &StartCheck::default(),
            )
            .unwrap(),
        );
        assert_eq!(
            blob_at(&repo, oid, LOG_PATH).unwrap(),
            std::fs::read(dir.path().join(LOG_PATH)).unwrap()
        );
    }

    /// Test 9: a store with no `LESSONS.md` commits its log, and a missing
    /// `LESSONS.md` is never dirty.
    #[test]
    fn missing_lessons_md_commits_the_log_and_is_not_dirty() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        put(dir.path(), "README", b"readme\n");
        user_commit(&repo, &["README"], "readme");
        put(dir.path(), LOG_PATH, TWO_LINES);

        let oid = committed(
            commit_cycle(
                &AgentRoot::new(dir.path()),
                "2026-11-03",
                &StartCheck::default(),
            )
            .unwrap(),
        );
        assert_eq!(blob_at(&repo, oid, LOG_PATH).as_deref(), Some(TWO_LINES));
        assert!(blob_at(&repo, oid, "README").is_some());
        assert!(blob_at(&repo, oid, LESSONS_PATH).is_none());
        assert_eq!(repo.find_commit(oid).unwrap().parent_count(), 1);

        let other = TempDir::new().unwrap();
        let other_repo = Repository::init(other.path()).unwrap();
        put(other.path(), LESSONS_PATH, b"lessons\n");
        put(other.path(), LOG_PATH, TWO_LINES);
        user_commit(&other_repo, &TRACKED_PATHS, "initial");
        let baseline = capture_baseline(&AgentRoot::new(other.path())).unwrap();
        std::fs::remove_file(other.path().join(LESSONS_PATH)).unwrap();
        assert!(check_dirty_at_cycle_start(other.path(), None)
            .unwrap()
            .is_clean());
        assert!(check_dirty_at_cycle_start(other.path(), Some(&baseline))
            .unwrap()
            .is_clean());
    }

    /// Test 10: a retired `LESSONS.md` leaves the commit and the index.
    #[test]
    fn retired_lessons_md_leaves_the_commit() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        put(dir.path(), LESSONS_PATH, b"lessons\n");
        put(dir.path(), LOG_PATH, TWO_LINES);
        user_commit(&repo, &TRACKED_PATHS, "initial");
        std::fs::remove_file(dir.path().join(LESSONS_PATH)).unwrap();

        let oid = committed(
            commit_cycle(
                &AgentRoot::new(dir.path()),
                "2026-11-03",
                &StartCheck::default(),
            )
            .unwrap(),
        );
        assert!(blob_at(&repo, oid, LESSONS_PATH).is_none());
        assert_eq!(blob_at(&repo, oid, LOG_PATH).as_deref(), Some(TWO_LINES));
        match repo.status_file(Path::new(LESSONS_PATH)) {
            Ok(status) => assert_eq!(status, Status::CURRENT),
            Err(e) => assert_eq!(e.code(), ErrorCode::NotFound),
        }
    }

    /// Test 11: the user's staged files stay staged and out of the commit.
    #[test]
    fn staged_user_files_stay_out_of_the_commit() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        put(dir.path(), LESSONS_PATH, b"lessons\n");
        put(dir.path(), LOG_PATH, TWO_LINES);
        user_commit(&repo, &TRACKED_PATHS, "initial");
        put(dir.path(), "notes.txt", b"notes\n");
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("notes.txt")).unwrap();
        index.write().unwrap();
        append(dir.path(), LOG_PATH, b"{\"id\":\"c\",\"pinned\":false}\n");

        let oid = committed(
            commit_cycle(
                &AgentRoot::new(dir.path()),
                "2026-11-03",
                &StartCheck::default(),
            )
            .unwrap(),
        );
        assert!(blob_at(&repo, oid, "notes.txt").is_none());
        assert_eq!(
            blob_at(&repo, oid, LOG_PATH).unwrap(),
            std::fs::read(dir.path().join(LOG_PATH)).unwrap()
        );
        assert_eq!(
            repo.status_file(Path::new("notes.txt")).unwrap(),
            Status::INDEX_NEW
        );
        for p in TRACKED_PATHS {
            assert_eq!(
                repo.status_file(Path::new(p)).unwrap(),
                Status::CURRENT,
                "{p}"
            );
        }
    }

    /// Test 12: an unchanged tree makes no commit and clears the marker (D7).
    #[test]
    fn unchanged_tree_makes_no_commit_and_clears_the_marker() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_files(&dir);
        make_initial_commit(&repo, &dir);
        let root = AgentRoot::new(dir.path());
        write_skip_marker(
            &root,
            AutobiographySkip {
                at: 0,
                reason: "user_dirty_tree".to_string(),
                files: vec![LESSONS_PATH.to_string()],
            },
        )
        .unwrap();

        let outcome = commit_cycle(&root, "2026-11-03", &StartCheck::default()).unwrap();
        assert!(
            matches!(outcome, AutobiographyOutcome::Unchanged),
            "got {outcome:?}"
        );
        assert_eq!(head_message(&repo), "initial");
        assert!(state_json(&root).get("last_autobiography_skip").is_none());
    }

    /// Test 13: on an unborn branch the commit has no parent and holds only the
    /// store's files.
    #[test]
    fn unborn_branch_commits_only_the_store() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo_with_files(&dir);
        put(dir.path(), "other.txt", b"other\n");

        let oid = committed(
            commit_cycle(
                &AgentRoot::new(dir.path()),
                "2026-11-03",
                &StartCheck::default(),
            )
            .unwrap(),
        );
        assert_eq!(repo.find_commit(oid).unwrap().parent_count(), 0);
        assert!(blob_at(&repo, oid, LOG_PATH).is_some());
        assert!(blob_at(&repo, oid, "other.txt").is_none());
    }

    /// Test 14: a nested project checks and commits its own store (D8).
    #[test]
    fn nested_project_checks_and_commits_its_own_store() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        put(dir.path(), LOG_PATH, TWO_LINES);
        user_commit(&repo, &[LOG_PATH], "root store");
        // A hand edit of the root store, which the nested project must not see.
        put(
            dir.path(),
            LOG_PATH,
            b"{\"id\":\"A\",\"pinned\":false}\n{\"id\":\"b\",\"pinned\":false}\n",
        );

        let project = dir.path().join("sub/proj");
        put(&project, LOG_PATH, b"{\"id\":\"n\",\"pinned\":false}\n");
        let nested_log = format!("sub/proj/{LOG_PATH}");

        assert!(check_dirty_at_cycle_start(&project, None)
            .unwrap()
            .is_clean());
        let root = AgentRoot::new(&project);
        let oid = committed(commit_cycle(&root, "2026-11-03", &StartCheck::default()).unwrap());
        assert_eq!(
            blob_at(&repo, oid, &nested_log).as_deref(),
            Some(&b"{\"id\":\"n\",\"pinned\":false}\n"[..])
        );
        assert_eq!(blob_at(&repo, oid, LOG_PATH).as_deref(), Some(TWO_LINES));

        let baseline = capture_baseline(&root).unwrap();
        put(&project, LOG_PATH, b"{\"id\":\"N\",\"pinned\":false}\n");
        let start = check_dirty_at_cycle_start(&project, Some(&baseline)).unwrap();
        assert_eq!(start.log.as_deref(), Some(Path::new(&nested_log)));
        assert_eq!(start.lessons, None);
    }

    /// Test 15: the notice text and the report's wire form, byte for byte.
    #[test]
    fn skip_message_and_report_exact_strings() {
        let files = vec!["a/LESSONS.md".to_string(), "b/log.jsonl".to_string()];
        assert_eq!(
            skip_message("user_dirty_tree", &files),
            "no git commit for this cycle: a/LESSONS.md, b/log.jsonl changed outside a dream \
             cycle. Commit those changes with git, and the next cycle commits again."
        );
        assert_eq!(
            skip_message("no_baseline", &files),
            "no git commit for this cycle: a/LESSONS.md, b/log.jsonl differ from HEAD, and \
             dreamd has no record yet of what its last cycle wrote. The next cycle commits."
        );
        assert_eq!(
            skip_message("mystery", &files),
            "no git commit for this cycle (mystery): a/LESSONS.md, b/log.jsonl"
        );

        let sha = "0123456789abcdef0123456789abcdef01234567".to_string();
        let cases = [
            (
                AutobiographyReport::Committed {
                    commit: sha.clone(),
                },
                format!("{{\"outcome\":\"committed\",\"commit\":\"{sha}\"}}"),
            ),
            (
                AutobiographyReport::Unchanged,
                "{\"outcome\":\"unchanged\"}".to_string(),
            ),
            (
                AutobiographyReport::NoRepo,
                "{\"outcome\":\"no_repo\"}".to_string(),
            ),
            (
                AutobiographyReport::Skipped {
                    reason: "user_dirty_tree".to_string(),
                    files: files.clone(),
                },
                "{\"outcome\":\"skipped\",\"reason\":\"user_dirty_tree\",\
                 \"files\":[\"a/LESSONS.md\",\"b/log.jsonl\"]}"
                    .to_string(),
            ),
        ];
        for (report, json) in cases {
            assert_eq!(serde_json::to_string(&report).unwrap(), json);
            assert_eq!(
                serde_json::from_str::<AutobiographyReport>(&json).unwrap(),
                report
            );
            match &report {
                AutobiographyReport::Skipped { reason, files } => {
                    assert_eq!(report.notice(), Some(skip_message(reason, files)));
                }
                _ => assert_eq!(report.notice(), None, "{report:?}"),
            }
        }
    }

    /// Test 16: the baseline advances per file (D2, §11 H1).
    #[test]
    fn next_baseline_holds_only_the_dirty_file() {
        let prev = baseline_of(Some(b"old lessons"), b"old log\n");
        let captured = baseline_of(Some(b"new lessons"), b"new log\n");
        let dirty_lessons = StartCheck {
            lessons: Some(PathBuf::from(LESSONS_PATH)),
            log: None,
            had_baseline: true,
        };
        let dirty_log = StartCheck {
            lessons: None,
            log: Some(PathBuf::from(LOG_PATH)),
            had_baseline: true,
        };

        assert_eq!(
            next_baseline(Some(&prev), captured.clone(), &StartCheck::default()),
            captured,
            "a clean start advances everything"
        );

        let next = next_baseline(Some(&prev), captured.clone(), &dirty_lessons);
        assert_eq!(next.lessons_sha256, prev.lessons_sha256);
        assert_eq!(
            (next.log_len, &next.log_sha256),
            (captured.log_len, &captured.log_sha256)
        );

        let next = next_baseline(Some(&prev), captured.clone(), &dirty_log);
        assert_eq!(next.lessons_sha256, captured.lessons_sha256);
        assert_eq!(
            (next.log_len, &next.log_sha256),
            (prev.log_len, &prev.log_sha256)
        );

        assert_eq!(
            next_baseline(None, captured.clone(), &dirty_lessons),
            captured,
            "with no prev, everything advances"
        );
    }

    /// Test 17: `capture_baseline` hashes what is on disk.
    #[test]
    fn capture_baseline_hashes_the_files_on_disk() {
        let dir = TempDir::new().unwrap();
        let root = AgentRoot::new(dir.path());
        assert_eq!(
            capture_baseline(&root).unwrap(),
            AutobiographyBaseline {
                lessons_sha256: None,
                log_len: 0,
                log_sha256: sha256_hex(b""),
            }
        );

        put(dir.path(), LESSONS_PATH, b"lessons\n");
        put(dir.path(), LOG_PATH, TWO_LINES);
        assert_eq!(
            capture_baseline(&root).unwrap(),
            AutobiographyBaseline {
                lessons_sha256: Some(sha256_hex(b"lessons\n")),
                log_len: TWO_LINES.len() as u64,
                log_sha256: sha256_hex(TWO_LINES),
            }
        );

        assert_eq!(dir.path().join(TRACKED_PATHS[0]), root.lessons_md());
        assert_eq!(dir.path().join(TRACKED_PATHS[1]), root.episodic_jsonl());
    }
}
