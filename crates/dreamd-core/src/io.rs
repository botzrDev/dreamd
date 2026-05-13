//! Atomic file writes for dream-cycle outputs (DR-104 / WEG-8).
//!
//! [`write_atomic`] is the durability primitive the dream cycle leans on when
//! it replaces `LESSONS.md`, `PREFERENCES.md`, `DECISIONS.md`, and similar
//! semantic-layer files. It writes to `<path>.tmp`, `sync_data`s the bytes,
//! renames into place, then `sync_all`s the parent directory so the rename
//! itself survives crash. Windows lands in v0.1.1 — see `docs/windows.md`.
//!
//! Single-writer assumption holds: every mutation in v0.1 funnels through the
//! `MemoryCoordinator` (CLAUDE.md "Load-bearing engineering decisions" §1), so
//! a fixed `.tmp` neighbour name is acceptable.

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

/// Shared implementation behind [`write_atomic`]. `hook` runs after the `.tmp`
/// file is fully fsynced and before the rename into `path` — the only window
/// any caller can wedge into the atomic write. Production callers pass a
/// no-op hook; tests inject a failing hook to assert the destination file is
/// untouched on error and the `.tmp` survives as a recovery signal.
#[cfg(not(windows))]
pub(crate) fn write_atomic_with_hook(
    path: &Path,
    contents: &[u8],
    hook: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_tmpdir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "dreamd-io-{}-{}-{}-{}",
            label,
            std::process::id(),
            nanos,
            n,
        ));
        fs::create_dir_all(&dir).expect("create unique tmpdir");
        dir
    }

    struct DirGuard(PathBuf);
    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

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

        let err = write_atomic_with_hook(&target, b"unfinished\n", || {
            Err(io::Error::other("boom"))
        })
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

    #[cfg(windows)]
    #[test]
    fn windows_stub_returns_unsupported() {
        let err = write_atomic(Path::new("ignored"), b"data")
            .expect_err("write_atomic must error on Windows in v0.1");
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }
}
