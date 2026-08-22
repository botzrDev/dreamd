//! Atomic file writes for dream-cycle outputs (DR-104 / WEG-8).
//!
//! [`write_atomic`] is the durability primitive the dream cycle leans on when
//! it replaces `LESSONS.md`, `PREFERENCES.md`, `DECISIONS.md`, and similar
//! semantic-layer files. It writes to `<path>.tmp`, `sync_data`s the bytes,
//! renames into place, then `sync_all`s the parent directory so the rename
//! itself survives crash. Windows lands in v0.1.1 — see `docs/windows.md`.
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

/// Replace `path` with `contents` atomically.
///
/// On any failure between `.tmp` creation and `rename` into place, the `.tmp`
/// file is left on disk as a recovery signal for the daemon's startup check.
/// Do not silently remove it — WEG-8 relies on its presence to drive cleanup.
///
/// Returns [`io::ErrorKind::Unsupported`] on Windows; see `docs/windows.md`.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    #[cfg(windows)]
    {
        let _ = (path, contents);
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic write on Windows lands in v0.1.1; see docs/windows.md",
        ));
    }
    #[cfg(not(windows))]
    {
        write_atomic_with_hook(path, contents, || Ok(()))
    }
}

/// Shared implementation behind [`write_atomic`] (and [`episodic::rewrite_atomic`](crate::episodic::rewrite_atomic)).
/// `hook` runs after the `.tmp` file is fully fsynced and before the rename into
/// `path` — the only window any caller can wedge into the atomic write.
/// Production callers pass a no-op hook (or, for the episodic log, a WAL prune
/// intent append — the named `.tmp` provably exists at that point); tests inject
/// a failing hook to assert the destination file is untouched on error and the
/// `.tmp` survives as a recovery signal.
///
/// Returns [`io::ErrorKind::Unsupported`] on Windows (hook ignored), mirroring
/// [`write_atomic`]; Windows durable writes land in v0.1.1 (see `docs/windows.md`).
pub(crate) fn write_atomic_with_hook(
    path: &Path,
    contents: &[u8],
    hook: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    #[cfg(windows)]
    {
        let _ = (path, contents, hook);
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic write on Windows lands in v0.1.1; see docs/windows.md",
        ));
    }
    #[cfg(not(windows))]
    {
        let tmp = path.with_extension("tmp");
        let mut f = File::create(&tmp)?;
        f.write_all(contents)?;
        f.sync_data()?;
        drop(f);
        hook()?;
        fs::rename(&tmp, path)?;
        if let Some(parent) = path.parent() {
            // `std::fs::rename` does not expose the parent-dir fd, and `PathBuf`
            // has no `sync_data` — we must re-open the parent and fsync it so
            // the rename itself is durable across crash. No-op-but-not-error
            // on macOS; correct on Linux.
            File::open(parent)?.sync_all()?;
        }
        Ok(())
    }
}

/// An owned exclusive advisory lock, held for as long as this value lives.
///
/// Dropping the guard releases the lock (`flock(LOCK_UN)` via `nix`'s `Flock`
/// destructor, and again implicitly when the descriptor closes). The lockfile
/// itself is **never unlinked**. `flock` is advisory and attached to the open
/// file description, not to the path: unlinking would let a process that
/// already opened the old inode keep a lock that a later process — which
/// re-`create`s the path and gets a fresh inode — cannot see, so both would
/// enter the critical section at once. A stale zero-byte `*.lock` neighbour is
/// the intended steady state, the same convention the Tantivy index lock
/// follows (`tantivy-lock-file-no-rm-on-startup`).
#[derive(Debug)]
pub struct ExclusiveLock {
    #[cfg(unix)]
    _flock: nix::fcntl::Flock<File>,
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
///
/// Returns [`io::ErrorKind::Unsupported`] on Windows; see `docs/windows.md`.
pub fn lock_exclusive(path: &Path) -> io::Result<ExclusiveLock> {
    // `nix` is a `cfg(unix)` dependency (see dreamd-core/Cargo.toml), so the
    // gate here is `unix` rather than `not(windows)`; Windows still lands in
    // the `Unsupported` arm exactly like `write_atomic`.
    #[cfg(not(unix))]
    {
        let _ = path;
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "advisory file locking on Windows lands in v0.1.1; see docs/windows.md",
        ));
    }
    #[cfg(unix)]
    {
        use nix::fcntl::{Flock, FlockArg};

        // `create(true)` needs `write(true)`; `read(true)` keeps the descriptor
        // usable if a caller ever wants to stamp diagnostics into the lockfile.
        // `truncate(false)` is deliberate: the lockfile's *identity* (its inode)
        // is the lock, and a contender that arrives while the holder is inside
        // the critical section must not have its file mutated out from under it.
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        // Blocking: a concurrent writer should wait its turn, not fail.
        let flock = Flock::lock(file, FlockArg::LockExclusive).map_err(|(_file, errno)| {
            io::Error::new(
                io::Error::from(errno).kind(),
                format!("flock {}: {errno}", path.display()),
            )
        })?;
        Ok(ExclusiveLock { _flock: flock })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{unique_tmpdir, DirGuard};

    #[cfg(not(windows))]
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

    #[cfg(not(windows))]
    #[test]
    fn injected_failure_preserves_original_and_keeps_tmp() {
        let dir = unique_tmpdir("fail");
        let _g = DirGuard(dir.clone());
        let target = dir.join("LESSONS.md");
        fs::write(&target, b"original\n").unwrap();

        let err =
            write_atomic_with_hook(&target, b"unfinished\n", || Err(io::Error::other("boom")))
                .expect_err("hook failure must surface");
        assert_eq!(err.kind(), io::ErrorKind::Other);

        // Destination is byte-identical to its pre-call state.
        assert_eq!(fs::read(&target).unwrap(), b"original\n");
        // `.tmp` is deliberately preserved as a recovery signal (WEG-8).
        let tmp = target.with_extension("tmp");
        assert!(
            tmp.exists(),
            ".tmp must remain after failure as recovery signal"
        );
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

    #[cfg(windows)]
    #[test]
    fn windows_stub_returns_unsupported() {
        let err = write_atomic(Path::new("ignored"), b"data")
            .expect_err("write_atomic must error on Windows in v0.1");
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }
}
