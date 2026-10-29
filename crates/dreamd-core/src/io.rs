//! Atomic file writes (DR-104 / WEG-8).
//!
//! [`write_atomic`] is the durable replace for the index sidecars, `state.json`,
//! the dream cycle's redo WAL, and the one-shot CLI rewrites. It writes to
//! `<path>.tmp`, `sync_data`s the bytes, renames into place, then flushes the
//! parent directory ([`sync_dir`]). A dream cycle's own outputs are staged and
//! committed by the redo WAL instead (`crate::stage`, `crate::wal`).
//! `registry.toml` is the exception: [`replace_renaming`] is a temp-file rename
//! with no parent flush, on every target.
//!
//! The parent flush is the Unix call order. It is not a power-loss proof of
//! the directory entry: on Windows `sync_all` is `FlushFileBuffers`, and a
//! directory handle is opened for write with `FILE_FLAG_BACKUP_SEMANTICS`.
//! Tantivy documents that flush as a no-op on local disks. A failure before
//! the rename leaves `path` untouched. A parent-flush error after the rename
//! is still returned.
//!
//! Single-writer assumption holds: every mutation in v0.1 funnels through the
//! `MemoryCoordinator` (ARCHITECTURE.md "Load-bearing engineering decisions" §1), so
//! a fixed `.tmp` neighbour name is acceptable.
//!
//! [`lock_exclusive`] is the companion primitive for the files the single-writer
//! assumption does *not* cover — `~/.agent/registry.toml` is written by any
//! `dreamd init` or `dreamd ... uninstall` process, so its read-modify-write
//! needs exclusion on top of `write_atomic`'s durability (AILAB-161).

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

/// `FILE_FLAG_BACKUP_SEMANTICS`. Opening a directory on Windows needs it, and
/// [`File::sync_all`](File::sync_all) is `FlushFileBuffers`, which needs write
/// access on that handle.
#[cfg(windows)]
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

/// Flush the directory entry at `dir`.
///
/// Unix opens the directory and `sync_all`s it. Windows opens it for read and
/// write with [`FILE_FLAG_BACKUP_SEMANTICS`] and `sync_all`s that handle. The
/// Windows probe test calls this function, not a lookalike.
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(dir)?;
        return file.sync_all();
    }
    #[cfg(not(windows))]
    {
        File::open(dir)?.sync_all()
    }
}

/// Replace `path` with `contents` atomically.
///
/// On any failure between `.tmp` creation and `rename` into place, the `.tmp`
/// file is left on disk and `path` is untouched. The `.tmp` is not what
/// recovery reads: dream-cycle recovery keys off the WAL and the stage dir.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    write_atomic_with_hook(path, contents, |_| Ok(()))
}

/// The implementation behind [`write_atomic`].
///
/// `hook` runs after the `.tmp` file is fully fsynced and before the rename into
/// `path` — the only window any caller can wedge into the atomic write — and is
/// handed **the `.tmp` path this call actually created**. [`write_atomic`]
/// passes a no-op; tests inject a failing hook to assert the destination file is
/// untouched on error and the `.tmp` is left behind.
pub(crate) fn write_atomic_with_hook(
    path: &Path,
    contents: &[u8],
    hook: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = File::create(&tmp)?;
    f.write_all(contents)?;
    f.sync_data()?;
    drop(f);
    hook(&tmp)?;
    fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            // `std::fs::rename` does not expose the parent-dir fd. Re-open it
            // and flush so the call order matches Unix. On Windows this is
            // [`sync_dir`]: write access plus backup semantics.
            sync_dir(parent)?;
        }
    }
    Ok(())
}

/// Replace `path` by writing a sibling temp file and renaming it over `path`.
///
/// This is the writer for `registry.toml` only. It does not flush the parent
/// directory. [`write_atomic`] does. Production registry writes on Unix still
/// use [`write_atomic`]; Windows registry writes use this function.
///
/// Compiled into Unix test builds so the rename can be checked without a
/// Windows host. Production Unix registry writes still use [`write_atomic`].
#[cfg(any(test, not(unix)))]
pub(crate) fn replace_renaming(path: &Path, contents: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(contents)?;
        f.sync_data()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// An owned exclusive advisory lock, held for as long as this value lives.
///
/// Dropping the guard unlocks and closes the file. The lockfile itself is
/// **never unlinked**. The lock is advisory and attached to the open file,
/// not to the path: unlinking would let a process that already opened the old
/// file keep a lock that a later process — which re-creates the path and gets
/// a fresh file — cannot see, so both would enter the critical section at
/// once. A stale zero-byte `*.lock` neighbour is the intended steady state,
/// the same convention the Tantivy index lock follows
/// (`tantivy-lock-file-no-rm-on-startup`).
///
/// Unix takes `flock`; Windows takes `LockFileEx`. Both go through
/// [`File::lock`](File::lock), which has been in std since 1.89.
#[derive(Debug)]
pub struct ExclusiveLock {
    file: File,
}

impl Drop for ExclusiveLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Take a blocking exclusive advisory lock on `path`, creating the lockfile if
/// it is absent.
///
/// Callers use this to serialize a read-modify-write over a *neighbouring*
/// file (e.g. `registry.toml.lock` guarding `registry.toml`): [`write_atomic`]
/// gives durability, not mutual exclusion, so two concurrent RMW cycles both
/// read the same snapshot and the later `rename` silently drops the earlier
/// writer's change.
///
/// Bind the result to a **named** variable — `let _guard = lock_exclusive(..)?`.
/// A bare `let _ = ...` drops the guard immediately and silently disables the
/// lock.
///
/// The lockfile is never removed; see [`ExclusiveLock`] for why.
pub fn lock_exclusive(path: &Path) -> io::Result<ExclusiveLock> {
    // `create(true)` needs `write(true)`; `read(true)` keeps the descriptor
    // usable if a caller ever wants to stamp diagnostics into the lockfile.
    // `truncate(false)` is deliberate: the lockfile's identity is the lock,
    // and a contender that arrives while the holder is inside the critical
    // section must not have its file mutated out from under it.
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    // Blocking: a concurrent writer should wait its turn, not fail.
    file.lock()
        .map_err(|e| io::Error::new(e.kind(), format!("lock {}: {e}", path.display())))?;
    Ok(ExclusiveLock { file })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{unique_tmpdir, DirGuard};

    #[test]
    fn happy_path_replaces_target_and_removes_tmp() {
        let dir = unique_tmpdir("happy");
        let _g = DirGuard(dir.clone());
        let target = dir.join("LESSONS.md");
        fs::write(&target, b"old\n").unwrap();

        write_atomic(&target, b"new content\n").expect("write_atomic ok");

        assert_eq!(fs::read(&target).unwrap(), b"new content\n");
        // `path.with_extension("tmp")` on `"LESSONS.md"` -> `"LESSONS.tmp"`.
        let tmp = target.with_extension("tmp");
        assert!(!tmp.exists(), "stray .tmp left behind after happy path");
    }

    #[test]
    fn injected_failure_preserves_original_and_keeps_tmp() {
        let dir = unique_tmpdir("fail");
        let _g = DirGuard(dir.clone());
        let target = dir.join("LESSONS.md");
        fs::write(&target, b"original\n").unwrap();

        let err = write_atomic_with_hook(&target, b"unfinished\n", |tmp| {
            // The hook is handed the tmp the write just fsynced, not a path it
            // re-derived (AILAB-164).
            assert_eq!(tmp, target.with_extension("tmp"));
            assert!(tmp.exists(), "hook must run after the .tmp is on disk");
            Err(io::Error::other("boom"))
        })
        .expect_err("hook failure must surface");
        assert_eq!(err.kind(), io::ErrorKind::Other);

        // Destination is byte-identical to its pre-call state.
        assert_eq!(fs::read(&target).unwrap(), b"original\n");
        // `.tmp` is left behind; the destination never saw it (WEG-8).
        let tmp = target.with_extension("tmp");
        assert!(tmp.exists(), ".tmp must remain after failure");
    }

    /// Two threads contending on ONE lock path must not overlap inside the
    /// critical section. Each thread stamps a shared cell with its own id,
    /// sleeps while holding the guard, then checks the stamp is still its own;
    /// without mutual exclusion the other thread overwrites it mid-sleep.
    #[cfg(unix)]
    #[test]
    fn exclusive_lock_serializes_two_threads() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Barrier};
        use std::time::Duration;

        let dir = unique_tmpdir("flock");
        let _g = DirGuard(dir.clone());
        let lock_path = dir.join("registry.toml.lock");

        let stamp = Arc::new(AtomicUsize::new(0));
        let overlaps = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(2));

        let handles: Vec<_> = (1..=2usize)
            .map(|id| {
                let lock_path = lock_path.clone();
                let stamp = Arc::clone(&stamp);
                let overlaps = Arc::clone(&overlaps);
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    let _guard = lock_exclusive(&lock_path).expect("acquire exclusive lock");
                    stamp.store(id, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(150));
                    // Assert on the parent thread instead of panicking here, so
                    // the failure message is the assertion below rather than a
                    // join error.
                    if stamp.load(Ordering::SeqCst) != id {
                        overlaps.fetch_add(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("lock contender thread joined");
        }

        assert_eq!(
            overlaps.load(Ordering::SeqCst),
            0,
            "critical sections overlapped - lock_exclusive did not serialize"
        );
        // Guards are dropped; the lockfile itself is deliberately never
        // unlinked (see `ExclusiveLock`).
        assert!(
            lock_path.exists(),
            "lockfile must survive guard drop, never unlinked"
        );
    }

    #[test]
    fn replace_renaming_replaces_the_target_and_removes_the_temp() {
        let dir = unique_tmpdir("replace-renaming");
        let _g = DirGuard(dir.clone());
        let target = dir.join("registry.toml");
        fs::write(&target, b"old\n").unwrap();

        replace_renaming(&target, b"projects = []\n").unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"projects = []\n");
        assert!(!dir.join("registry.tmp").exists());
    }

    /// The production parent open: write access and backup semantics, then
    /// `sync_all`. A lookalike open would not prove the replace path.
    #[cfg(windows)]
    #[test]
    fn windows_parent_flush_uses_production_open() {
        let dir = unique_tmpdir("win-flush");
        let _g = DirGuard(dir.clone());
        sync_dir(&dir).expect("production parent flush");
        let target = dir.join("LESSONS.md");
        fs::write(&target, b"old\n").unwrap();
        write_atomic(&target, b"new\n").expect("write_atomic");
        assert_eq!(fs::read(&target).unwrap(), b"new\n");
    }
}
