//! One project's writer runtime: the project writer lock, the Tantivy index,
//! and the coordinator wired to that index.
//!
//! The daemon keeps one [`ProjectResourceMap`](crate::server::ProjectResourceMap)
//! of these for every project other than its boot project, so a project's
//! coordinator and its index open, idle and evict as one entry. In-process
//! `dreamd mcp` opens one for its project and holds it until it exits.
//!
//! [`ProjectRuntime::open`] runs the one acquisition sequence: lock (which
//! resets a stale status and runs WAL recovery), then the index, then the
//! coordinator. Both spawned tasks keep a clone of the lock, so releasing an
//! entry frees the project only after the coordinator has drained and the
//! indexer task is gone.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::layout::AgentRoot;
use crate::server::index_map::{IndexError, IndexHandle};
use crate::server::lifecycle::{Supervisor, COORDINATOR_CHANNEL_CAPACITY};
use crate::server::project_resource_map::ProjectMapResource;
use crate::server::tantivy_handle::{TantivyIndexHandle, DEFAULT_COMMIT_CADENCE};
use crate::writer_lock::{WriterKind, WriterLock, WriterLockError};

/// How long [`ProjectRuntime::open_for_watch`] waits while this process's own
/// `watch` still holds the project writer lock (DRM-50).
pub(crate) const OWN_LOCK_WAIT: Duration = Duration::from_secs(2);

/// The pause between two tries within [`OWN_LOCK_WAIT`].
pub(crate) const OWN_LOCK_RETRY: Duration = Duration::from_millis(10);

/// A project's coordinator and index, opened under one writer lock.
pub struct ProjectRuntime {
    supervisor: Arc<Supervisor>,
    index: Arc<TantivyIndexHandle>,
    last_used: Instant,
    /// Declared last, dropped last.
    lock: WriterLock,
}

impl ProjectRuntime {
    /// acquire → open index → start coordinator wired to that index.
    ///
    /// A refused lock is [`IndexError::WriterLock`] carrying the
    /// [`WriterLockError`] `Display`. A failed recovery keeps its
    /// [`IndexError::Wal`] shape.
    pub fn open(agent_root: &AgentRoot, kind: WriterKind) -> Result<Self, IndexError> {
        let lock = WriterLock::acquire(agent_root, kind).map_err(lock_error)?;
        Self::open_locked(agent_root, lock)
    }

    /// [`Self::open`] for `dreamd watch`'s per-project runtimes
    /// ([`WriterKind::Watch`]).
    ///
    /// When this process's own `watch` holds the writer lock, the holder is a
    /// runtime the daemon released whose tasks have not ended yet: each keeps
    /// a lock clone until the executor polls it (DRM-49 §8 H1). This waits for
    /// them, trying again every [`OWN_LOCK_RETRY`] for at most
    /// [`OWN_LOCK_WAIT`], and yields between tries so they can run. Any other
    /// holder is refused at once, as [`Self::open`] refuses it.
    pub(crate) async fn open_for_watch(agent_root: &AgentRoot) -> Result<Self, IndexError> {
        let deadline = tokio::time::Instant::now() + OWN_LOCK_WAIT;
        let lock = loop {
            match WriterLock::acquire(agent_root, WriterKind::Watch) {
                Ok(lock) => break lock,
                Err(e) if e.held_by_this_watch() && tokio::time::Instant::now() < deadline => {}
                Err(e) => return Err(lock_error(e)),
            }
            tokio::time::sleep(OWN_LOCK_RETRY).await;
        };
        Self::open_locked(agent_root, lock)
    }

    /// Open the index under `lock`, then start the coordinator wired to it.
    fn open_locked(agent_root: &AgentRoot, lock: WriterLock) -> Result<Self, IndexError> {
        let index = Arc::new(TantivyIndexHandle::open(
            agent_root,
            &lock,
            DEFAULT_COMMIT_CADENCE,
        )?);
        let supervisor = Supervisor::start(
            agent_root,
            &lock,
            COORDINATOR_CHANNEL_CAPACITY,
            Some(index.sender()),
        )
        .map_err(|e| IndexError::Supervisor(e.to_string()))?;
        Ok(Self {
            supervisor: Arc::new(supervisor),
            index,
            last_used: Instant::now(),
            lock,
        })
    }

    /// The project's coordinator.
    pub fn supervisor(&self) -> &Arc<Supervisor> {
        &self.supervisor
    }

    /// The project's index; the coordinator's appends go to its indexer.
    pub fn index(&self) -> &Arc<TantivyIndexHandle> {
        &self.index
    }

    /// The token this runtime holds.
    pub fn writer_lock(&self) -> &WriterLock {
        &self.lock
    }

    /// Drain the coordinator, then commit the index, then release the lock.
    /// The order is `run_watch`'s drain: the coordinator's queued appends reach
    /// the indexer before the final commit.
    pub async fn shutdown(self) {
        crate::server::watch::drain_daemon(self.supervisor, Some(self.index)).await;
        drop(self.lock);
    }
}

/// A refused lock as an [`IndexError`]: a failed recovery keeps its
/// [`IndexError::Wal`] shape, and anything else is [`IndexError::WriterLock`]
/// carrying the [`WriterLockError`] `Display`.
fn lock_error(e: WriterLockError) -> IndexError {
    match e {
        WriterLockError::Recover(e) => IndexError::Wal(e.to_string()),
        other => IndexError::WriterLock(other.to_string()),
    }
}

impl ProjectMapResource for ProjectRuntime {
    type ReleaseError = IndexError;

    fn last_used(&self) -> Instant {
        self.last_used
    }

    /// Drop the supervisor; close the index when this was its last owner. The
    /// tasks' lock clones keep the project held until both tasks end.
    fn release(self) -> Result<(), IndexError> {
        let Self {
            supervisor,
            index,
            lock,
            ..
        } = self;
        drop(supervisor);
        if let Ok(index) = Arc::try_unwrap(index) {
            index.close()?;
        }
        drop(lock);
        Ok(())
    }

    fn touch(&mut self) {
        self.last_used = Instant::now();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::server::project_resource_map::{ProjectResourceMap, ProjectResourceMapConfig};
    use std::time::Duration;

    fn store() -> (tempfile::TempDir, AgentRoot) {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        std::fs::create_dir_all(root.episodic_dir()).unwrap();
        (dir, root)
    }

    #[tokio::test]
    async fn open_holds_the_lock_and_wires_the_coordinator_to_the_index() {
        let (_dir, root) = store();

        let runtime = ProjectRuntime::open(&root, WriterKind::Watch).expect("open runtime");

        assert_eq!(runtime.writer_lock().kind(), WriterKind::Watch);
        let err = match ProjectRuntime::open(&root, WriterKind::Mcp) {
            Ok(_) => panic!("a second runtime on one project must be refused"),
            Err(e) => e,
        };
        assert_eq!(
            err.to_string(),
            format!(
                "project writer lock is held by pid {} (watch)",
                std::process::id()
            )
        );
        assert!(!err.is_retryable());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_frees_the_project() {
        let (_dir, root) = store();
        ProjectRuntime::open(&root, WriterKind::Mcp)
            .expect("first open")
            .shutdown()
            .await;

        let again = ProjectRuntime::open(&root, WriterKind::Watch).expect("reopen after shutdown");
        again.shutdown().await;
    }

    #[tokio::test]
    async fn missing_store_is_refused_without_scaffolding() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());

        let err = match ProjectRuntime::open(&root, WriterKind::Watch) {
            Ok(_) => panic!("a project without .agent/ must be refused"),
            Err(e) => e,
        };

        assert!(matches!(err, IndexError::WriterLock(_)), "got {err:?}");
        assert!(!root.agent_dir().exists());
    }

    #[tokio::test]
    async fn idle_eviction_releases_the_entry() {
        let (_dir, root) = store();
        let mut map = ProjectResourceMap::new(ProjectResourceMapConfig {
            capacity: 10,
            idle_timeout: Duration::ZERO,
        });
        let runtime = ProjectRuntime::open(&root, WriterKind::Watch).expect("open runtime");
        map.insert_or_adopt(root.project_root(), runtime);

        assert_eq!(map.evict_idle(), 1);
        assert!(map.is_empty());
    }
}
