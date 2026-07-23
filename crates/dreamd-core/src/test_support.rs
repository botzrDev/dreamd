//! Shared helpers for unit tests in `dreamd-core`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Create a unique temporary directory for parallel-safe test isolation.
pub fn unique_tmpdir(label: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "dreamd-test-{}-{}-{}-{}",
        label,
        std::process::id(),
        nanos,
        n,
    ));
    std::fs::create_dir_all(&dir).expect("create unique tmpdir");
    dir
}

/// RAII cleanup guard: removes the temp dir on drop so tests leave no scratch
/// behind even when they panic.
pub struct DirGuard(pub PathBuf);

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
