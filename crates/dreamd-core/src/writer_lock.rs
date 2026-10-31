//! The project writer lock: `.agent/.dreamd/writer.lock`.
//!
//! Every process that writes a project's store takes this lock first, and holds
//! it for as long as it writes: `dreamd watch` for each project it serves,
//! in-process `dreamd mcp`, `dreamd setup`'s first lesson, and the one-shot CLI
//! writers for one invocation. A second holder is refused with the first
//! holder's pid and [`WriterKind`], which [`WriterLock::acquire`] reads back from
//! the stamp the holder wrote into the lockfile.
//!
//! Acquisition is one sequence, in this order: lock, refuse if a pre-lock
//! dreamd holds Tantivy's own index lock, stamp, open the provenance ledger
//! (cutting a torn final line), reset a stale `in_progress` status when there
//! is no WAL, then WAL recovery, which rolls a committed cycle forward and
//! wipes any staged one. A [`WriterKind::Repair`] writer moves an unreadable
//! WAL aside and recovers again instead of refusing.
//!
//! Only after that may the caller open the index and start the coordinator.
//! `TantivyIndexHandle::open` takes a `&WriterLock`, and
//! `MemoryCoordinator::open` takes the lock's [`Ledger`], which outside this
//! crate only a `WriterLock` hands out. `MemoryCoordinator::open_at` takes
//! neither; it is for tests and cannot run a dream cycle. The token carries
//! the project's one ledger appender ([`WriterLock::ledger`]); a roll-forward
//! records its edges through it.
//!
//! The lock is an advisory `flock` (Unix) / `LockFileEx` (Windows) through
//! [`File::try_lock`]. It belongs to the open file description, so a second
//! `acquire` in the **same** process on the same project is refused exactly as
//! one from another process is. One process takes one token per project and
//! clones it; the OS lock is released when the last clone drops.
//!
//! The lockfile is never deleted, for the reason [`crate::io::ExclusiveLock`]
//! gives. A stamp left behind by a holder that exited is stale and is
//! overwritten by the next holder.

use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::daemon_state::update_daemon_state;
use crate::layout::AgentRoot;
use crate::provenance::Ledger;
use crate::server::INDEX_DIR_NAME;
use crate::wal::{self, WalError};

/// Lockfile name under `.agent/.dreamd/`.
pub const WRITER_LOCK_FILENAME: &str = "writer.lock";

/// Tantivy's own writer lock inside the index directory. A dreamd that predates
/// the project lock holds only this one.
const LEGACY_INDEX_LOCK_FILENAME: &str = ".tantivy-writer.lock";

/// How many times [`WriterLock::acquire`] re-opens the lockfile when the path
/// no longer names the file it locked.
const OPEN_ATTEMPTS: usize = 3;

/// What a lock holder is doing with the project.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriterKind {
    Watch,
    Mcp,
    Setup,
    Dream,
    Forget,
    Archive,
    Checkout,
    Bisect,
    Repair,
}

impl WriterKind {
    /// The stamp and message spelling, e.g. `watch`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Watch => "watch",
            Self::Mcp => "mcp",
            Self::Setup => "setup",
            Self::Dream => "dream",
            Self::Forget => "forget",
            Self::Archive => "archive",
            Self::Checkout => "checkout",
            Self::Bisect => "bisect",
            Self::Repair => "repair",
        }
    }
}

impl fmt::Display for WriterKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The stamp a holder writes into the lockfile: `{"pid":…,"kind":"…"}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub pid: u32,
    pub kind: WriterKind,
}

/// Failure to take the project writer lock.
#[derive(Debug, thiserror::Error)]
pub enum WriterLockError {
    /// Another open of the lockfile holds it. `holder` is its stamp, or `None`
    /// when the stamp could not be read (expected on Windows, which refuses
    /// reads of a locked range).
    #[error("{}", busy_message(.holder))]
    Busy { holder: Option<Holder> },
    /// No project lock is held, but the index writer lock is: a dreamd from
    /// before the project lock is writing this store.
    #[error(
        "the index writer lock is held by a dreamd process that predates the \
         project writer lock (1.0.x); stop it first"
    )]
    LegacyWriter,
    /// `.agent/` does not exist. Nothing was created.
    #[error(
        "no .agent/ store at {} — refusing to scaffold a phantom store; run `dreamd init`",
        .0.display()
    )]
    NoAgentStore(PathBuf),
    /// Creating `.agent/.dreamd/`, or opening, locking or stamping the
    /// lockfile, failed.
    #[error("project writer lock I/O: {0}")]
    Io(#[from] io::Error),
    /// The lock was taken, but resetting a stale status or WAL recovery failed.
    /// The lock was released.
    #[error("wal recovery: {0}")]
    Recover(#[from] WalError),
}

fn busy_message(holder: &Option<Holder>) -> String {
    match holder {
        Some(h) => format!("project writer lock is held by pid {} ({})", h.pid, h.kind),
        None => "project writer lock is held by another process".to_string(),
    }
}

#[derive(Debug)]
struct Inner {
    file: File,
    /// Canonicalized `.agent/` (raw if canonicalize failed).
    agent_dir: PathBuf,
    kind: WriterKind,
    ledger: Ledger,
    /// Set when this acquire quarantined an unreadable WAL. Repair only.
    quarantined: Option<PathBuf>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// One token per (process, project). Clones share one locked file; the OS lock
/// is released when the last clone drops.
#[derive(Clone, Debug)]
pub struct WriterLock {
    inner: Arc<Inner>,
}

impl WriterLock {
    /// Take the project writer lock for `root`, then bring the store to a
    /// pre- or post-cycle state: reset a stale `in_progress` status when no WAL
    /// exists, then WAL recovery, which rolls a committed cycle forward and
    /// wipes any staged one.
    ///
    /// [`WriterKind::Repair`] is the one caller that, on
    /// [`WalError::UnknownFormat`], [`WalError::BadRedoPath`] or
    /// [`WalError::Json`], keeps the lock and quarantines the WAL instead of
    /// returning the error. Every other kind still refuses. [`WalError::Io`]
    /// and [`WalError::Provenance`] refuse for repair too.
    ///
    /// Creates `.agent/.dreamd/` and the lockfile when they are absent, but
    /// never `.agent/` itself. Never creates, truncates or deletes the Tantivy
    /// index lock it probes.
    pub fn acquire(root: &AgentRoot, kind: WriterKind) -> Result<Self, WriterLockError> {
        let agent_dir = root.agent_dir();
        if !agent_dir.exists() {
            return Err(WriterLockError::NoAgentStore(agent_dir));
        }
        let dreamd_dir = root.dreamd_dir();
        fs::create_dir_all(&dreamd_dir)?;
        let path = dreamd_dir.join(WRITER_LOCK_FILENAME);

        let file = lock_file(&path)?;
        probe_legacy_writer(
            &dreamd_dir
                .join(INDEX_DIR_NAME)
                .join(LEGACY_INDEX_LOCK_FILENAME),
        )?;
        write_stamp(
            &file,
            Holder {
                pid: std::process::id(),
                kind,
            },
        )?;
        let ledger = Ledger::open(root.provenance_ledger());

        if !root.wal_path().exists() && wal::read_last_cycle_status(root)? == "in_progress" {
            update_daemon_state(root, |s| s.last_dream_cycle_status = "failed".to_string())
                .map_err(WalError::from)?;
            tracing::warn!(
                project = %root.project_root().display(),
                "state.json said a dream cycle was in progress, but there is no WAL; \
                 reset the status to failed"
            );
        }
        let quarantined = match wal::recover_on_startup(root, &ledger) {
            Ok(_) => None,
            Err(err) if kind == WriterKind::Repair && wal::is_unreadable_wal(&err) => {
                let sidecar = wal::quarantine_unreadable(root)?;
                wal::recover_on_startup(root, &ledger)?;
                sidecar
            }
            Err(err) => return Err(err.into()),
        };

        Ok(Self {
            inner: Arc::new(Inner {
                file,
                agent_dir: canonical_or_raw(&agent_dir),
                kind,
                ledger,
                quarantined,
            }),
        })
    }

    /// Same project? Compares canonicalized `.agent/` dirs (raw paths if
    /// canonicalize fails).
    pub fn covers(&self, root: &AgentRoot) -> bool {
        canonical_or_raw(&root.agent_dir()) == self.inner.agent_dir
    }

    pub fn kind(&self) -> WriterKind {
        self.inner.kind
    }

    /// The project's provenance appender. Every clone of this token shares it.
    pub fn ledger(&self) -> &Ledger {
        &self.inner.ledger
    }

    /// The sidecar this acquire moved an unreadable WAL to, if it did.
    pub fn quarantined_wal(&self) -> Option<&Path> {
        self.inner.quarantined.as_deref()
    }
}

fn canonical_or_raw(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Open the lockfile and take it, or report who holds it.
fn lock_file(path: &Path) -> Result<File, WriterLockError> {
    for _ in 0..OPEN_ATTEMPTS {
        // The `io::lock_exclusive` options: the lockfile's identity is the
        // lock, so a contender must not truncate a holder's stamp.
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(WriterLockError::Busy {
                    holder: read_stamp(path),
                });
            }
            Err(TryLockError::Error(e)) => return Err(WriterLockError::Io(e)),
        }
        if still_names(path, &file)? {
            return Ok(file);
        }
        // The path was replaced or removed between the open and the lock: the
        // lock is on a file nobody else will open. Drop it and open again.
    }
    Err(WriterLockError::Io(io::Error::other(format!(
        "{} kept changing while it was being locked",
        path.display()
    ))))
}

/// Does `path` still name the file `file` locked?
#[cfg(unix)]
fn still_names(path: &Path, file: &File) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let held = file.metadata()?;
    match fs::metadata(path) {
        Ok(on_disk) => Ok(held.dev() == on_disk.dev() && held.ino() == on_disk.ino()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(windows)]
fn still_names(path: &Path, file: &File) -> io::Result<bool> {
    let held = same_file::Handle::from_file(file.try_clone()?)?;
    match same_file::Handle::from_path(path) {
        Ok(on_disk) => Ok(held == on_disk),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// The holder's stamp, or `None` if it cannot be read or parsed.
fn read_stamp(path: &Path) -> Option<Holder> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(text.trim_end()).ok()
}

fn write_stamp(file: &File, holder: Holder) -> Result<(), WriterLockError> {
    let mut stamp = serde_json::to_string(&holder).map_err(io::Error::other)?;
    stamp.push('\n');
    file.set_len(0)?;
    let mut f = file;
    f.seek(SeekFrom::Start(0))?;
    f.write_all(stamp.as_bytes())?;
    Ok(())
}

/// Refuse when a pre-lock dreamd holds the Tantivy writer lock. Opens the file
/// only if it exists, and only to try the lock; a lock taken here is released
/// by dropping the file.
fn probe_legacy_writer(path: &Path) -> Result<(), WriterLockError> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(WriterLockError::Io(e)),
    };
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(WriterLockError::LegacyWriter),
        Err(TryLockError::Error(e)) => Err(WriterLockError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_state::read_daemon_state;
    use crate::stage::arm_crash;
    use crate::test_support::{unique_tmpdir, DirGuard};
    use crate::wal::{CycleKind, DreamWal, RecoveryOutcome, WalIntent};

    fn store(label: &str) -> (AgentRoot, DirGuard) {
        let dir = unique_tmpdir(label);
        let root = AgentRoot::new(&dir);
        fs::create_dir_all(root.episodic_dir()).unwrap();
        (root, DirGuard(dir))
    }

    fn stamp_on_disk(root: &AgentRoot) -> String {
        fs::read_to_string(root.dreamd_dir().join(WRITER_LOCK_FILENAME)).unwrap()
    }

    fn write_status(root: &AgentRoot, status: &str) {
        update_daemon_state(root, |s| s.last_dream_cycle_status = status.to_string()).unwrap();
    }

    fn status(root: &AgentRoot) -> String {
        read_daemon_state(root).unwrap().last_dream_cycle_status
    }

    #[test]
    fn missing_agent_dir_is_refused_and_nothing_is_created() {
        let dir = unique_tmpdir("wl-no-store");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);

        let err = WriterLock::acquire(&root, WriterKind::Dream).unwrap_err();

        assert!(
            matches!(&err, WriterLockError::NoAgentStore(p) if *p == root.agent_dir()),
            "got {err:?}"
        );
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            0,
            "acquire must not scaffold anything under the project root"
        );
    }

    #[test]
    fn stamp_names_this_pid_and_kind() {
        let (root, _g) = store("wl-stamp");

        let lock = WriterLock::acquire(&root, WriterKind::Forget).unwrap();

        let holder: Holder = serde_json::from_str(stamp_on_disk(&root).trim_end()).unwrap();
        assert_eq!(
            holder,
            Holder {
                pid: std::process::id(),
                kind: WriterKind::Forget
            }
        );
        assert_eq!(lock.kind(), WriterKind::Forget);
        assert!(lock.covers(&root));
    }

    #[test]
    fn second_acquire_is_refused_until_every_clone_drops() {
        let (root, _g) = store("wl-busy");
        let first = WriterLock::acquire(&root, WriterKind::Watch).unwrap();
        let clone = first.clone();
        let expected = Holder {
            pid: std::process::id(),
            kind: WriterKind::Watch,
        };

        let busy = WriterLock::acquire(&root, WriterKind::Dream).unwrap_err();
        assert!(
            matches!(busy, WriterLockError::Busy { holder: Some(h) } if h == expected),
            "got {busy:?}"
        );
        assert_eq!(
            busy.to_string(),
            format!(
                "project writer lock is held by pid {} (watch)",
                expected.pid
            )
        );

        drop(first);
        let busy = WriterLock::acquire(&root, WriterKind::Dream).unwrap_err();
        assert!(
            matches!(busy, WriterLockError::Busy { holder: Some(h) } if h == expected),
            "a live clone must keep the lock held; got {busy:?}"
        );

        drop(clone);
        let again = WriterLock::acquire(&root, WriterKind::Dream).unwrap();
        assert_eq!(again.kind(), WriterKind::Dream);
    }

    #[test]
    fn stale_stamp_from_a_dropped_holder_is_overwritten() {
        let (root, _g) = store("wl-stale-stamp");
        drop(WriterLock::acquire(&root, WriterKind::Checkout).unwrap());
        // A longer stamp than the next one, so a missing truncate would show.
        fs::write(
            root.dreamd_dir().join(WRITER_LOCK_FILENAME),
            "{\"pid\":4294967295,\"kind\":\"checkout\"}\ntrailing bytes\n",
        )
        .unwrap();

        let _lock = WriterLock::acquire(&root, WriterKind::Mcp).unwrap();

        assert_eq!(
            stamp_on_disk(&root),
            format!("{{\"pid\":{},\"kind\":\"mcp\"}}\n", std::process::id())
        );
    }

    #[test]
    fn stale_in_progress_without_a_wal_becomes_failed() {
        let (root, _g) = store("wl-stale-status");
        write_status(&root, "in_progress");
        assert!(!root.wal_path().exists());

        let _lock = WriterLock::acquire(&root, WriterKind::Dream).unwrap();

        assert_eq!(status(&root), "failed");
    }

    /// A crash right after the redo WAL's write: the next `acquire` finishes
    /// the cycle.
    #[test]
    fn acquire_rolls_a_committed_cycle_forward() {
        const X: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let (root, _g) = store("wl-roll-forward");
        let lessons = crate::lessons::render_lessons_file(&crate::lessons::LessonsFile {
            last_updated: chrono::DateTime::from_timestamp(1_747_137_600, 0).unwrap(),
            prompt_version: "deterministic-only".to_string(),
            cluster_key: "rust::eh".to_string(),
            citations: vec![X.to_string()],
            lessons: vec![crate::lessons::Lesson {
                id: X.to_string(),
                content: "handle errors".to_string(),
                pinned: false,
            }],
        });
        let sidecar =
            br#"{"schema_version":"1.0","clusters":[{"skill_action":"rust::eh","count":3}]}"#;
        let jsonl = root.episodic_jsonl();
        let sidecar_path = root.semantic_dir().join("recurrence_counts.json");
        {
            let lock = WriterLock::acquire(&root, WriterKind::Dream).unwrap();
            let mut stage = wal::begin_cycle(&root, CycleKind::Dream, 0).unwrap();
            stage.put(&jsonl, b"staged log\n").unwrap();
            stage.put(&root.lessons_md(), lessons.as_bytes()).unwrap();
            stage.put(&sidecar_path, sidecar).unwrap();
            stage.record_edges(&[
                (
                    crate::provenance::Kind::LessonCitation,
                    X.to_string(),
                    format!("lsn_{X}"),
                ),
                (
                    crate::provenance::Kind::Recurrence,
                    X.to_string(),
                    "rust::eh".to_string(),
                ),
            ]);
            // Point 0 is the stage-dir fsync; point 1 follows the WAL write.
            arm_crash(Some(1));
            let result = wal::commit_cycle(stage, lock.ledger(), 0);
            arm_crash(None);
            result.unwrap_err();
        }
        assert!(root.wal_path().exists());
        assert!(!jsonl.exists(), "the crash came before any rename");
        assert_eq!(status(&root), "in_progress");

        let _lock = WriterLock::acquire(&root, WriterKind::Watch).unwrap();

        assert_eq!(fs::read(&jsonl).unwrap(), b"staged log\n");
        assert_eq!(fs::read_to_string(root.lessons_md()).unwrap(), lessons);
        assert_eq!(fs::read(&sidecar_path).unwrap(), sidecar);
        let ledger = fs::read_to_string(root.provenance_ledger()).unwrap();
        assert!(
            ledger.contains("\"lesson_citation\"") && ledger.contains("\"recurrence\""),
            "the roll-forward must record the cycle's edges: {ledger}"
        );
        assert!(!root.wal_path().exists(), "the WAL must be gone");
        assert!(!root.stage_dir().exists(), "the stage must be wiped");
        assert_eq!(status(&root), "complete");
    }

    /// A hand-written legacy WAL with no `Commit`: the temp goes, the
    /// destination stays, status `failed`, as before 1.2.0.
    #[test]
    fn acquire_keeps_a_legacy_wal_meaning() {
        let (root, _g) = store("wl-legacy-wal");
        let jsonl = root.episodic_jsonl();
        fs::write(&jsonl, b"pre-cycle\n").unwrap();
        let tmp = jsonl.with_extension("tmp");
        fs::write(&tmp, b"partial\n").unwrap();
        let wal = DreamWal {
            schema_version: "1.0".to_string(),
            intents: vec![WalIntent::PruneEpisodicMemory {
                temp_file_path: tmp.to_string_lossy().into_owned(),
            }],
        };
        fs::create_dir_all(root.dreamd_dir()).unwrap();
        fs::write(root.wal_path(), serde_json::to_string_pretty(&wal).unwrap()).unwrap();
        write_status(&root, "in_progress");

        let _lock = WriterLock::acquire(&root, WriterKind::Watch).unwrap();

        assert!(!root.wal_path().exists(), "the WAL must be gone");
        assert!(!tmp.exists(), "the temp file must be gone");
        assert_eq!(fs::read(&jsonl).unwrap(), b"pre-cycle\n");
        assert_eq!(status(&root), "failed");
        // Recovery ran exactly once: a second acquire finds a clean store.
        drop(_lock);
        let lock = WriterLock::acquire(&root, WriterKind::Watch).unwrap();
        assert_eq!(
            wal::recover_if_needed(&root, lock.ledger()).unwrap(),
            RecoveryOutcome::Clean
        );
    }

    /// Taking the lock cuts a final ledger line with no `\n`, keeps every
    /// complete line, and leaves a missing ledger missing.
    #[test]
    fn acquire_cuts_a_torn_ledger_tail() {
        const LINE: &[u8] = b"{\"schema_version\":\"provenance/1.0\",\"kind\":\"index_doc\",\"from\":\"evt_01ARZ3NDEKTSV4RRFFQ69G5FAV\",\"to\":\"evt_01ARZ3NDEKTSV4RRFFQ69G5FAV\"}\n";
        const FRAGMENT: &[u8] = b"{\"schema_version\":\"provenance/1.0\",\"kind\":\"ind";

        let (root, _g) = store("wl-ledger-absent");
        drop(WriterLock::acquire(&root, WriterKind::Watch).unwrap());
        assert!(
            !root.provenance_dir().exists(),
            "acquire must not create the ledger"
        );

        for (label, before, after) in [
            (
                "wl-ledger-line-and-fragment",
                [LINE, FRAGMENT].concat(),
                LINE.to_vec(),
            ),
            ("wl-ledger-fragment-only", FRAGMENT.to_vec(), Vec::new()),
        ] {
            let (root, _g) = store(label);
            fs::create_dir_all(root.provenance_dir()).unwrap();
            fs::write(root.provenance_ledger(), &before).unwrap();

            let lock = WriterLock::acquire(&root, WriterKind::Watch).unwrap();

            assert_eq!(
                fs::read(root.provenance_ledger()).unwrap(),
                after,
                "{label}"
            );
            assert_eq!(lock.ledger().path(), root.provenance_ledger());
        }
    }

    /// A pre-lock dreamd holds only Tantivy's writer lock, and its index open
    /// wiped an outdated index before taking that lock. The probe refuses
    /// before anything is wiped, so a second writer cannot replace the first
    /// one's index underneath it.
    #[cfg(unix)]
    #[tokio::test]
    async fn legacy_index_writer_is_refused_before_the_index_is_wiped() {
        use crate::index::INDEX_MANIFEST_FILENAME;
        use crate::server::{TantivyIndexHandle, DEFAULT_COMMIT_CADENCE};

        let (root, _g) = store("wl-legacy");
        // One indexed event, so the index holds segment files a rebuild renames.
        let event = dreamd_protocol::AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: dreamd_protocol::EventId::parse("evt_01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
            timestamp: chrono::DateTime::from_timestamp(1_780_000_000, 0).unwrap(),
            pain: 6.0,
            importance: 6.0,
            pinned: false,
            skill_action: "rust::eh".to_string(),
            source_harness: "claude-code".to_string(),
            content: "indexed before the old writer took the lock".to_string(),
        };
        fs::write(
            root.episodic_jsonl(),
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();
        {
            let lock = WriterLock::acquire(&root, WriterKind::Watch).unwrap();
            TantivyIndexHandle::open(&root, &lock, DEFAULT_COMMIT_CADENCE)
                .unwrap()
                .shutdown()
                .await
                .unwrap();
        }
        let index_dir = root.dreamd_dir().join(INDEX_DIR_NAME);
        let held = File::open(index_dir.join(LEGACY_INDEX_LOCK_FILENAME)).unwrap();
        held.try_lock().unwrap();
        let manifest = root.dreamd_dir().join(INDEX_MANIFEST_FILENAME);
        fs::write(&manifest, br#"{"schema_version":"index/1.0"}"#).unwrap();
        let entries = |dir: &Path| {
            let mut names: Vec<_> = fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            names.sort();
            names
        };
        let index_before = entries(&index_dir);
        let manifest_before = fs::read(&manifest).unwrap();

        let result = WriterLock::acquire(&root, WriterKind::Dream);
        // What a writer does with the token: open the index.
        if let Ok(lock) = &result {
            if let Ok(h) = TantivyIndexHandle::open(&root, lock, DEFAULT_COMMIT_CADENCE) {
                h.shutdown().await.unwrap();
            }
        }

        assert_eq!(
            entries(&index_dir),
            index_before,
            "the index dir must be untouched while an old writer holds it"
        );
        assert_eq!(fs::read(&manifest).unwrap(), manifest_before);
        let err = result.unwrap_err();
        assert!(matches!(err, WriterLockError::LegacyWriter), "got {err:?}");
        assert_eq!(
            err.to_string(),
            "the index writer lock is held by a dreamd process that predates the project \
             writer lock (1.0.x); stop it first"
        );
    }
}
