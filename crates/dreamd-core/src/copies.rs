//! Copies of one event that `dreamd forget` must find beyond the live log
//! (DRM-168).
//!
//! dreamd keeps an event's bytes in more places than
//! `episodic/AGENT_LEARNINGS.jsonl`:
//!
//! - the decay archive, `.dreamd/snapshots/<date>.jsonl`, once decay moved it
//!   out of the log;
//! - the migrate backup, `episodic/AGENT_LEARNINGS.jsonl.bak`;
//! - memory branch objects, `.dreamd/branches/objects/<id>/`: every dream
//!   cycle's autosnap and every named branch holds a copy of the log and of
//!   `LESSONS.md`.
//!
//! [`find`] lists them, reading only. [`stage_scrub`] stages the forget for
//! them inside the forget's stage: the two JSONL copies are rewritten without
//! the record (the redo WAL renames them in), and every object a ref or a
//! detached `HEAD` names is replaced by a new object with the same forget
//! applied ([`crate::plan::plan_forget`]), the ref rewritten to name it.
//! Objects are content-addressed and immutable, so a scrubbed object has a new
//! id, and the old object is removed after the commit by
//! [`crate::retention::remove_unnamed`]. The switch from old to new object is
//! the ref rename, which the forget's redo WAL commits with the log.
//!
//! An object is rewritten rather than dropped with its refs: an event older
//! than the retained autosnaps is in every one of them, and a named branch
//! holds every other memory it was taken with, so dropping them would delete
//! the restore points along with the one event.
//!
//! [`git_history`] reads the project's git repository, if any, for commits
//! whose dreamd files hold the event. dreamd never rewrites git history;
//! [`rewrite_command`] is the command a user can run.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use dreamd_protocol::EventId;

use crate::autobiography;
use crate::episodic;
use crate::forget::ForgetError;
use crate::layout::AgentRoot;
use crate::lessons;
use crate::plan::{self, LessonsChange};
use crate::snapshot::{self, HEAD_FILE};
use crate::stage::Stage;

/// Path of the live log inside an object, and of `LESSONS.md`.
const OBJECT_LOG: &str = "episodic/AGENT_LEARNINGS.jsonl";
const OBJECT_LESSONS: &str = "semantic/LESSONS.md";

/// Which JSONL copy a [`FileCopy`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// A day of the decay archive, `.dreamd/snapshots/<date>.jsonl`.
    DecayArchive,
    /// `episodic/AGENT_LEARNINGS.jsonl.bak`, written by `dreamd migrate`.
    MigrateBackup,
}

/// A JSONL file outside the live log whose bytes hold the event id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCopy {
    pub path: PathBuf,
    pub kind: FileKind,
}

/// A memory branch object whose files hold the event id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectCopy {
    /// The object id, or `<id>.partial` for an unfinished write.
    pub id: String,
    /// The files under `branches/refs/` whose line 1 names it.
    pub refs: Vec<String>,
    /// `branches/HEAD` is detached at it.
    pub head: bool,
    /// Why it cannot be rewritten: it failed its hash check, could not be
    /// read, or is a `.partial` write. `None` for a sound object.
    pub unreadable: Option<String>,
}

impl ObjectCopy {
    /// A ref or a detached `HEAD` names it.
    pub fn named(&self) -> bool {
        !self.refs.is_empty() || self.head
    }
}

/// Every copy [`find`] saw, in path and id order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreCopies {
    pub files: Vec<FileCopy>,
    pub objects: Vec<ObjectCopy>,
}

impl StoreCopies {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.objects.is_empty()
    }

    pub fn has(&self, kind: FileKind) -> bool {
        self.files.iter().any(|f| f.kind == kind)
    }
}

/// What [`stage_scrub`] staged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scrub {
    /// The JSONL copies whose rewrite was staged.
    pub files: Vec<FileKind>,
    /// (old object, new object) for every named object that was rewritten.
    pub replaced: Vec<(String, String)>,
    /// Objects to remove after the commit: the replaced ones and every sound
    /// object no ref named that held the event.
    pub to_remove: Vec<String>,
}

/// Whether `haystack` contains `needle`.
pub(crate) fn holds(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack.windows(needle.len()).any(|w| w == needle)
}

/// `AGENT_LEARNINGS.jsonl.bak`, beside the live log.
pub fn migrate_backup_path(agent_root: &AgentRoot) -> PathBuf {
    let mut name = agent_root.episodic_jsonl().into_os_string();
    name.push(".bak");
    PathBuf::from(name)
}

fn read_if_present(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether any regular file under `dir` holds `needle`. Unreadable files
/// count as holding it: a caller then reports the object instead of
/// clearing it.
fn dir_holds(dir: &Path, needle: &[u8]) -> io::Result<bool> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            if dir_holds(&path, needle)? {
                return Ok(true);
            }
        } else if meta.is_file() {
            match fs::read(&path) {
                Ok(bytes) if holds(&bytes, needle) => return Ok(true),
                Ok(_) => {}
                Err(_) => return Ok(true),
            }
        }
    }
    Ok(false)
}

/// Find every copy of `id` in the decay archive, the migrate backup and the
/// branch objects. Reads only.
pub fn find(agent_root: &AgentRoot, id: &EventId) -> Result<StoreCopies, ForgetError> {
    let needle = id.as_str().as_bytes();
    let mut copies = StoreCopies::default();

    let archive_dir = agent_root.snapshots_dir();
    match fs::read_dir(&archive_dir) {
        Ok(entries) => {
            let mut paths: Vec<PathBuf> = entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "jsonl") && p.is_file())
                .collect();
            paths.sort();
            for path in paths {
                if read_if_present(&path)
                    .map_err(ForgetError::Copies)?
                    .is_some_and(|b| holds(&b, needle))
                {
                    copies.files.push(FileCopy {
                        path,
                        kind: FileKind::DecayArchive,
                    });
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(ForgetError::Copies(e)),
    }
    let backup = migrate_backup_path(agent_root);
    if read_if_present(&backup)
        .map_err(ForgetError::Copies)?
        .is_some_and(|b| holds(&b, needle))
    {
        copies.files.push(FileCopy {
            path: backup,
            kind: FileKind::MigrateBackup,
        });
    }

    let refs = snapshot::ref_files(agent_root)?;
    let head = snapshot::head_body(agent_root)?;
    let head_line = head
        .as_deref()
        .and_then(|b| b.lines().next())
        .unwrap_or("")
        .to_string();
    let objects_dir = agent_root.branches_objects_dir();
    let mut candidates: Vec<String> = snapshot::object_ids(agent_root)?;
    if let Ok(entries) = fs::read_dir(&objects_dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.ends_with(".partial") && entry.path().is_dir() {
                    candidates.push(name.to_string());
                }
            }
        }
    }
    candidates.sort();
    for obj in candidates {
        if !dir_holds(&objects_dir.join(&obj), needle).map_err(ForgetError::Copies)? {
            continue;
        }
        let unreadable = if obj.ends_with(".partial") {
            Some("an unfinished object write".to_string())
        } else {
            snapshot::read_object(agent_root, &obj)
                .err()
                .map(|e| e.to_string())
        };
        copies.objects.push(ObjectCopy {
            refs: refs
                .iter()
                .filter(|(_, body)| body.lines().next() == Some(obj.as_str()))
                .map(|(name, _)| name.clone())
                .collect(),
            head: head_line == obj,
            id: obj,
            unreadable,
        });
    }
    Ok(copies)
}

/// The files of a sound object with the forget of `id` applied: the log
/// without the record, and `LESSONS.md` as [`plan::plan_forget`] leaves it.
/// The recurrence sidecar and `PREFERENCES.md` keep their bytes: the sidecar
/// holds counts per cluster, no ids and no content, and recounting it at
/// today's clock would change clusters that have nothing to do with `id`.
fn scrubbed_object(
    files: Vec<(String, Vec<u8>)>,
    id: &EventId,
    now_sec: i64,
) -> Result<Vec<(String, Vec<u8>)>, ForgetError> {
    let log = files
        .iter()
        .find(|(p, _)| p == OBJECT_LOG)
        .map(|(_, b)| b.clone())
        .unwrap_or_default();
    let prior_lessons = files
        .iter()
        .find(|(p, _)| p == OBJECT_LESSONS)
        .map(|(_, b)| b.clone());
    let raw = episodic::raw_lines(&log);
    let (plan, _) = plan::plan_forget(&raw, prior_lessons.as_deref(), id, now_sec)?;
    let mut out = Vec::with_capacity(files.len());
    for (path, bytes) in files {
        if path == OBJECT_LOG {
            out.push((path, plan.log.clone().unwrap_or(bytes)));
        } else if path == OBJECT_LESSONS {
            match &plan.lessons {
                LessonsChange::Keep => out.push((path, bytes)),
                LessonsChange::Write(file) => {
                    out.push((path, lessons::render_lessons_file(file).into_bytes()))
                }
                LessonsChange::Unlink => {}
            }
        } else {
            out.push((path, bytes));
        }
    }
    Ok(out)
}

/// Whether the object directory at `path` was last modified more than
/// [`crate::retention::OBJECT_GC_GRACE`] ago. Reads the wall clock, as object
/// collection does, and only for this.
fn older_than_grace(path: &Path) -> bool {
    let Some(cutoff) = std::time::SystemTime::now().checked_sub(crate::retention::OBJECT_GC_GRACE)
    else {
        return false;
    };
    fs::metadata(path)
        .and_then(|m| m.modified())
        .is_ok_and(|mtime| mtime < cutoff)
}

/// The copies [`stage_scrub`] leaves as they are: objects that fail their
/// hash check or are a `.partial` write, and objects no ref names that are
/// younger than the grace period.
pub(crate) fn would_stay(agent_root: &AgentRoot, copies: &StoreCopies) -> StoreCopies {
    StoreCopies {
        files: Vec::new(),
        objects: copies
            .objects
            .iter()
            .filter(|o| {
                o.unreadable.is_some()
                    || (!o.named()
                        && !older_than_grace(&agent_root.branches_objects_dir().join(&o.id)))
            })
            .cloned()
            .collect(),
    }
}

/// `body` with line 1 replaced by `new_id`. Every later line (the time the
/// snapshot was taken) keeps its bytes.
fn renamed_ref_body(body: &str, new_id: &str) -> String {
    match body.split_once('\n') {
        Some((_, rest)) => format!("{new_id}\n{rest}"),
        None => format!("{new_id}\n"),
    }
}

/// Stage the forget of `id` for `copies` in `stage`:
///
/// - each JSONL copy is staged without the record ([`episodic::patch_lines`],
///   which keeps every other line's bytes); a copy left empty is unlinked;
/// - each sound object a ref or a detached `HEAD` names is written again
///   with the forget applied, as a new object outside the WAL (as an autosnap
///   object is), and every ref file and a detached `HEAD` that named it are
///   staged to name the new one;
/// - an object nobody names is not rewritten; it is listed for removal once
///   it is older than the grace period object collection uses.
///
/// An object whose plan fails is left alone and not listed; the check after
/// the commit reports it.
pub(crate) fn stage_scrub(
    stage: &mut Stage,
    copies: &StoreCopies,
    id: &EventId,
    now_sec: i64,
) -> Result<Scrub, ForgetError> {
    let agent_root = stage.agent_root().clone();
    let drops = HashSet::from([id.clone()]);
    let mut scrub = Scrub::default();

    for copy in &copies.files {
        let raw = episodic::read_raw(&copy.path)?;
        let patched = episodic::patch_lines(&raw, &HashMap::new(), &drops)?;
        if patched.is_empty() {
            stage.unlink(&copy.path).map_err(ForgetError::Copies)?;
        } else {
            stage
                .put(&copy.path, &patched)
                .map_err(ForgetError::Copies)?;
        }
        if !scrub.files.contains(&copy.kind) {
            scrub.files.push(copy.kind);
        }
    }

    for obj in &copies.objects {
        if obj.unreadable.is_some() {
            continue;
        }
        if !obj.named() {
            // `memory branch` writes its object before its ref, without the
            // writer lock: an object younger than the grace period may be
            // about to be named. Leave it, as object collection does; the
            // search after the commit reports it.
            if older_than_grace(&agent_root.branches_objects_dir().join(&obj.id)) {
                scrub.to_remove.push(obj.id.clone());
            }
            continue;
        }
        let files = snapshot::read_object(&agent_root, &obj.id)?;
        let Ok(files) = scrubbed_object(files, id, now_sec) else {
            continue;
        };
        let new_id = snapshot::put_object(&agent_root, &files)?;
        if new_id == obj.id {
            continue;
        }
        scrub.replaced.push((obj.id.clone(), new_id));
        scrub.to_remove.push(obj.id.clone());
    }

    if !scrub.replaced.is_empty() {
        let new_of: HashMap<&str, &str> = scrub
            .replaced
            .iter()
            .map(|(old, new)| (old.as_str(), new.as_str()))
            .collect();
        let refs_dir = agent_root.branches_refs_dir();
        for (name, body) in snapshot::ref_files(&agent_root)? {
            if let Some(new_id) = body.lines().next().and_then(|l| new_of.get(l)) {
                stage
                    .put(
                        &refs_dir.join(&name),
                        renamed_ref_body(&body, new_id).as_bytes(),
                    )
                    .map_err(ForgetError::Copies)?;
            }
        }
        if let Some(body) = snapshot::head_body(&agent_root)? {
            if let Some(new_id) = body.lines().next().and_then(|l| new_of.get(l)) {
                stage
                    .put(
                        &agent_root.branches_dir().join(HEAD_FILE),
                        renamed_ref_body(&body, new_id).as_bytes(),
                    )
                    .map_err(ForgetError::Copies)?;
            }
        }
    }
    Ok(scrub)
}

/// The project's git history as it bears on one event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitScan {
    /// No git repository at or above the project root.
    NoRepo,
    /// The repository was read. `commits` is 0 when no commit holds the id.
    Scanned(GitHistory),
    /// The repository could not be read; nothing is known about it.
    Failed(String),
}

/// Commits whose dreamd files hold an event id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitHistory {
    /// Commits reachable from `HEAD` or any ref (branches, tags, remote
    /// branches) whose episodic log or `LESSONS.md` holds the id.
    pub commits: usize,
    /// The oldest of them (a parent comes before its children), full hex id.
    pub first: Option<String>,
    /// The newest of them, full hex id.
    pub last: Option<String>,
    /// The repository paths that hold the id in at least one of them.
    pub paths: Vec<String>,
    /// The repository's working directory, where a rewrite would run. Not
    /// written to the receipt.
    pub workdir: PathBuf,
}

/// `path` relative to `base`, `/`-joined.
fn repo_path(base: &Path, path: &Path) -> String {
    base.join(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Read the project's git repository for commits whose
/// `.agent/episodic/AGENT_LEARNINGS.jsonl` or `.agent/semantic/LESSONS.md`
/// holds `id`. Walks every commit reachable from `HEAD` and from every ref.
/// Reads only. Reflogs and unreachable objects are not walked.
pub fn git_history(agent_root: &AgentRoot, id: &EventId) -> GitScan {
    match scan_git(agent_root, id) {
        Ok(Some(history)) => GitScan::Scanned(history),
        Ok(None) => GitScan::NoRepo,
        Err(e) => GitScan::Failed(e),
    }
}

fn scan_git(agent_root: &AgentRoot, id: &EventId) -> Result<Option<GitHistory>, String> {
    let view = match autobiography::open_repo(agent_root.project_root()) {
        Ok(Some(view)) => view,
        Ok(None) => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let repo = &view.repo;
    let project = agent_root.project_root();
    let rels: Vec<String> = [agent_root.episodic_jsonl(), agent_root.lessons_md()]
        .iter()
        .map(|p| repo_path(&view.prefix, p.strip_prefix(project).unwrap_or(p)))
        .collect();
    let git = |e: git2::Error| e.to_string();

    let mut walk = repo.revwalk().map_err(git)?;
    // Parents before children, so the first and last commits that hold the
    // id are the oldest and newest by ancestry. Commit times have second
    // resolution and can tie.
    walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::TIME | git2::Sort::REVERSE)
        .map_err(git)?;
    match repo.head() {
        Ok(head) => {
            if let Ok(commit) = head.peel_to_commit() {
                walk.push(commit.id()).map_err(git)?;
            }
        }
        Err(e)
            if matches!(
                e.code(),
                git2::ErrorCode::UnbornBranch | git2::ErrorCode::NotFound
            ) => {}
        Err(e) => return Err(e.to_string()),
    }
    for reference in repo.references().map_err(git)? {
        let reference = reference.map_err(git)?;
        if let Ok(commit) = reference.peel_to_commit() {
            walk.push(commit.id()).map_err(git)?;
        }
    }

    let needle = id.as_str().as_bytes();
    let mut blobs: HashMap<git2::Oid, bool> = HashMap::new();
    let mut seen_paths = vec![false; rels.len()];
    let mut history = GitHistory {
        workdir: view.workdir.clone(),
        ..GitHistory::default()
    };
    let mut first: Option<git2::Oid> = None;
    let mut last: Option<git2::Oid> = None;
    for oid in walk {
        let oid = oid.map_err(git)?;
        let commit = repo.find_commit(oid).map_err(git)?;
        let tree = commit.tree().map_err(git)?;
        let mut held = false;
        for (i, rel) in rels.iter().enumerate() {
            let entry = match tree.get_path(Path::new(rel)) {
                Ok(entry) => entry,
                Err(e) if e.code() == git2::ErrorCode::NotFound => continue,
                Err(e) => return Err(e.to_string()),
            };
            let blob = entry.id();
            let blob_holds = *blobs.entry(blob).or_insert_with(|| {
                repo.find_blob(blob)
                    .map(|b| holds(b.content(), needle))
                    .unwrap_or(false)
            });
            if blob_holds {
                held = true;
                seen_paths[i] = true;
            }
        }
        if held {
            history.commits += 1;
            first.get_or_insert(oid);
            last = Some(oid);
        }
    }
    history.first = first.map(|oid| oid.to_string());
    history.last = last.map(|oid| oid.to_string());
    history.paths = rels
        .into_iter()
        .zip(seen_paths)
        .filter(|(_, seen)| *seen)
        .map(|(rel, _)| rel)
        .collect();
    Ok(Some(history))
}

/// A `git filter-repo` command that removes `id` from every commit in the
/// repository, for a POSIX shell. dreamd never runs it.
///
/// Its blob callback empties every rendered `LESSONS.md` that holds the id,
/// as `dreamd forget` unlinks the live file when a lesson was built from the
/// event, and drops every line holding the record's `"id":"<id>"` from every
/// other file, which in dreamd's files is the record's own log line.
pub fn rewrite_command(id: &EventId) -> String {
    let id = id.as_str();
    format!(
        "git filter-repo --blob-callback 'blob.data = (b\"\" if blob.data.startswith(b\"---\\nlast_updated:\") \
         else b\"\".join(l for l in blob.data.splitlines(True) if b\"\\\"id\\\":\\\"{id}\\\"\" not in l)) \
         if b\"{id}\" in blob.data else blob.data'"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::lessons::{Lesson, LessonsFile};
    use crate::provenance::Ledger;
    use crate::wal::{self, CycleKind};
    use chrono::{DateTime, Utc};
    use dreamd_protocol::AgentLearning;

    // 2026-09-29T14:03:00Z
    const NOW: i64 = 1_790_690_580;
    const A: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FA1";
    const B: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FA2";
    const MARK: &str = "marker-q7wz";

    fn record(id: &str, content: &str) -> String {
        let ev = AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: EventId::parse(id).unwrap(),
            timestamp: DateTime::<Utc>::from_timestamp(NOW - 3600, 0).unwrap(),
            pain: 6.0,
            importance: 7.0,
            pinned: false,
            skill_action: "rust::forget".to_string(),
            source_harness: "test".to_string(),
            content: content.to_string(),
        };
        format!("{}\n", serde_json::to_string(&ev).unwrap())
    }

    fn lessons_from(exemplar: &str, content: &str) -> String {
        lessons::render_lessons_file(&LessonsFile {
            last_updated: DateTime::<Utc>::from_timestamp(NOW - 60, 0).unwrap(),
            prompt_version: crate::consolidation::DETERMINISTIC_PROMPT_ID.to_string(),
            cluster_key: "rust::forget".to_string(),
            citations: vec![exemplar.to_string()],
            lessons: vec![Lesson {
                id: exemplar.to_string(),
                content: content.to_string(),
                pinned: false,
            }],
        })
    }

    /// A store whose log holds A (with the marker) and B, and whose
    /// `LESSONS.md` is built from A.
    fn store() -> (tempfile::TempDir, AgentRoot) {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        fs::create_dir_all(root.episodic_dir()).unwrap();
        fs::create_dir_all(root.semantic_dir()).unwrap();
        fs::create_dir_all(root.dreamd_dir()).unwrap();
        let log = format!("{}{}", record(A, MARK), record(B, "kept"));
        fs::write(root.episodic_jsonl(), log).unwrap();
        fs::write(root.lessons_md(), lessons_from(A, MARK)).unwrap();
        (dir, root)
    }

    fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files_under(&path, out);
            } else {
                out.push(path);
            }
        }
    }

    fn branch_files_holding(root: &AgentRoot, needle: &str) -> Vec<PathBuf> {
        let mut files = Vec::new();
        files_under(&root.branches_dir(), &mut files);
        files
            .into_iter()
            .filter(|p| holds(&fs::read(p).unwrap(), needle.as_bytes()))
            .collect()
    }

    /// Stage, commit and remove, as `forget::cascade` does.
    fn scrub(root: &AgentRoot, copies: &StoreCopies) -> Scrub {
        let ledger = Ledger::open(root.provenance_ledger());
        let id = EventId::parse(A).unwrap();
        let out = wal::stage_and_commit(root, CycleKind::Forget, NOW, &ledger, |stage| {
            stage_scrub(stage, copies, &id, NOW)
        })
        .unwrap();
        crate::retention::remove_unnamed(root, &out.to_remove).unwrap();
        out
    }

    #[test]
    fn find_lists_every_object_that_holds_the_id_with_the_refs_naming_it() {
        let (_d, root) = store();
        let snap = snapshot::create_autosnap(&root, NOW).unwrap();
        let named = snapshot::create_branch(&root, "keep", NOW + 60).unwrap();
        // An object taken after A left the log holds nothing of it.
        fs::write(root.episodic_jsonl(), record(B, "kept")).unwrap();
        fs::remove_file(root.lessons_md()).unwrap();
        let clean = snapshot::create_autosnap(&root, NOW + 120).unwrap();

        let copies = find(&root, &EventId::parse(A).unwrap()).unwrap();
        let ids: Vec<&str> = copies.objects.iter().map(|o| o.id.as_str()).collect();
        assert!(ids.contains(&snap.as_str()));
        assert!(ids.contains(&named.as_str()));
        assert!(!ids.contains(&clean.as_str()));
        let keep = copies.objects.iter().find(|o| o.id == named).unwrap();
        assert!(keep.refs.contains(&"keep".to_string()));
        assert!(copies.objects.iter().all(|o| o.unreadable.is_none()));
    }

    #[test]
    fn scrub_rewrites_every_named_object_and_removes_the_old_ones() {
        let (_d, root) = store();
        let snap = snapshot::create_autosnap(&root, NOW).unwrap();
        // Same bytes: the named branch shares the autosnap's object.
        let named = snapshot::create_branch(&root, "keep", NOW + 60).unwrap();
        assert_eq!(snap, named);
        let snap_ref = snapshot::autosnap_ref_name(NOW).unwrap();
        let before = fs::read_to_string(root.branches_refs_dir().join(&snap_ref)).unwrap();

        let copies = find(&root, &EventId::parse(A).unwrap()).unwrap();
        let out = scrub(&root, &copies);

        assert_eq!(out.replaced.len(), 1);
        let (old, new) = &out.replaced[0];
        assert_eq!(old, &snap);
        assert!(!root.branches_objects_dir().join(old).exists());
        assert!(branch_files_holding(&root, MARK).is_empty());
        assert!(branch_files_holding(&root, A).is_empty());

        // Both refs name the new object; the autosnap ref keeps its time line.
        let after = fs::read_to_string(root.branches_refs_dir().join(&snap_ref)).unwrap();
        assert_eq!(after.lines().next(), Some(new.as_str()));
        assert_eq!(after.lines().nth(1), before.lines().nth(1));
        let keep = fs::read_to_string(root.branches_refs_dir().join("keep")).unwrap();
        assert_eq!(keep.lines().next(), Some(new.as_str()));

        // The new object is sound, keeps B, and has no LESSONS.md: the lesson
        // was A's content, so the forget unlinks it there as it does live.
        let files = snapshot::read_object(&root, new).unwrap();
        let log = &files.iter().find(|(p, _)| p == OBJECT_LOG).unwrap().1;
        assert!(holds(log, B.as_bytes()));
        assert!(!files.iter().any(|(p, _)| p == OBJECT_LESSONS));
        // HEAD still names the branch.
        let head = snapshot::head_body(&root).unwrap().unwrap();
        assert_eq!(head.trim_end(), "ref: refs/keep");
    }

    #[test]
    fn scrub_follows_a_detached_head_and_removes_an_object_no_ref_names() {
        let (_d, root) = store();
        let snap = snapshot::create_autosnap(&root, NOW).unwrap();
        fs::write(root.branches_dir().join(HEAD_FILE), format!("{snap}\n")).unwrap();
        fs::remove_file(
            root.branches_refs_dir()
                .join(snapshot::autosnap_ref_name(NOW).unwrap()),
        )
        .unwrap();
        // An orphan: an object no ref names that holds A.
        fs::write(root.episodic_jsonl(), record(A, MARK)).unwrap();
        let orphan = snapshot::create_autosnap(&root, NOW + 60).unwrap();
        fs::remove_file(
            root.branches_refs_dir()
                .join(snapshot::autosnap_ref_name(NOW + 60).unwrap()),
        )
        .unwrap();

        let orphan_dir = root.branches_objects_dir().join(&orphan);
        let old = std::time::SystemTime::now() - 2 * crate::retention::OBJECT_GC_GRACE;
        fs::File::open(&orphan_dir)
            .unwrap()
            .set_modified(old)
            .unwrap();

        let copies = find(&root, &EventId::parse(A).unwrap()).unwrap();
        let out = scrub(&root, &copies);

        let head = snapshot::head_body(&root).unwrap().unwrap();
        let new = &out.replaced[0].1;
        assert_eq!(head.lines().next(), Some(new.as_str()));
        assert!(out.to_remove.contains(&orphan));
        assert!(!root.branches_objects_dir().join(&orphan).exists());
        assert!(branch_files_holding(&root, MARK).is_empty());
    }

    #[test]
    fn a_young_object_no_ref_names_is_left_for_a_ref_about_to_name_it() {
        let (_d, root) = store();
        let young = snapshot::create_autosnap(&root, NOW).unwrap();
        fs::remove_file(
            root.branches_refs_dir()
                .join(snapshot::autosnap_ref_name(NOW).unwrap()),
        )
        .unwrap();

        let copies = find(&root, &EventId::parse(A).unwrap()).unwrap();
        let out = scrub(&root, &copies);

        assert!(out.to_remove.is_empty(), "{out:?}");
        assert!(root.branches_objects_dir().join(&young).exists());
        let left = find(&root, &EventId::parse(A).unwrap()).unwrap();
        assert_eq!(left.objects.len(), 1);
        assert!(!left.objects[0].named());
    }

    #[test]
    fn a_corrupt_object_is_reported_and_left_alone() {
        let (_d, root) = store();
        let snap = snapshot::create_autosnap(&root, NOW).unwrap();
        let log = root.branches_objects_dir().join(&snap).join(OBJECT_LOG);
        let mut bytes = fs::read(&log).unwrap();
        bytes.extend_from_slice(b"tampered\n");
        fs::write(&log, bytes).unwrap();

        let copies = find(&root, &EventId::parse(A).unwrap()).unwrap();
        assert_eq!(copies.objects.len(), 1);
        assert!(copies.objects[0].unreadable.is_some());
        let out = scrub(&root, &copies);
        assert!(out.replaced.is_empty() && out.to_remove.is_empty());
        assert!(root.branches_objects_dir().join(&snap).exists());
    }

    #[test]
    fn scrub_rewrites_the_decay_archive_and_the_migrate_backup() {
        let (_d, root) = store();
        fs::create_dir_all(root.snapshots_dir()).unwrap();
        let archive = root.snapshot_file("2026-06-01");
        fs::write(
            &archive,
            format!("{}{}", record(A, MARK), record(B, "kept")),
        )
        .unwrap();
        // A day that held only A is unlinked rather than left empty.
        let only_a = root.snapshot_file("2026-06-02");
        fs::write(&only_a, record(A, MARK)).unwrap();
        let backup = migrate_backup_path(&root);
        fs::write(&backup, record(A, MARK)).unwrap();

        let copies = find(&root, &EventId::parse(A).unwrap()).unwrap();
        assert!(copies.has(FileKind::DecayArchive) && copies.has(FileKind::MigrateBackup));
        let out = scrub(&root, &copies);

        assert_eq!(
            out.files,
            vec![FileKind::DecayArchive, FileKind::MigrateBackup]
        );
        assert_eq!(fs::read_to_string(&archive).unwrap(), record(B, "kept"));
        assert!(!only_a.exists());
        assert!(!backup.exists());
        assert!(find(&root, &EventId::parse(A).unwrap()).unwrap().is_empty());
    }

    /// Prints the command for a by-hand check: `cargo test -p dreamd-core
    /// --lib copies::tests::print_rewrite_command -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn print_rewrite_command() {
        println!("{}", rewrite_command(&EventId::parse(A).unwrap()));
    }

    #[test]
    fn rewrite_command_names_the_id_and_no_content() {
        let id = EventId::parse(A).unwrap();
        let cmd = rewrite_command(&id);
        assert!(cmd.starts_with("git filter-repo --blob-callback '"));
        assert!(cmd.ends_with('\''));
        assert_eq!(cmd.matches('\'').count(), 2, "one single-quoted argument");
        assert!(cmd.contains(A));
    }
}
