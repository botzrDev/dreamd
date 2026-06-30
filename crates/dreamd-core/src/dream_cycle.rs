//! Dream cycle orchestration — single owner of the full phase graph.
//!
//! Phase order (load-bearing):
//!   1. Consolidation — cluster → LESSONS.md → pin/unpin
//!   2. Decay — archive + JSONL prune
//!   3. Index — recurrence sidecar apply → prune decayed docs
//!   4. Autobiography — best-effort git commit
//!
//! [`run_filesystem_phases`] opens and commits the single WAL envelope that
//! spans consolidation + decay (one cycle = one atomic region).
//!
//! Entry points (`dreamd dream`, `POST /api/v1/dream`) are thin adapters over
//! this module. The coordinator actor calls [`run_filesystem_phases`] only so it
//! can reopen its long-lived append fd after atomic renames (WEG-271).

use std::path::{Path, PathBuf};

use crate::autobiography::{self, AutobiographyOutcome};
use crate::consolidation::{self, DreamCycleError as ConsolidationError};
use crate::decay::{self, DecayError, DecayResult};
use crate::layout::AgentRoot;
use crate::wal::{self, WalError};

/// Unified error type for dream-cycle orchestration.
#[derive(Debug, thiserror::Error)]
pub enum DreamCycleError {
    #[error("consolidation: {0}")]
    Consolidation(#[from] ConsolidationError),
    #[error("decay: {0}")]
    Decay(#[from] DecayError),
    #[error("WAL: {0}")]
    Wal(#[from] WalError),
    #[cfg(unix)]
    #[error("index: {0}")]
    Index(#[from] crate::server::index_map::IndexError),
    #[error("dream cycle already in progress")]
    InProgress,
}

/// Outcome of a full dream cycle (filesystem + post phases).
#[derive(Debug)]
pub struct DreamCycleResult {
    pub decay: DecayResult,
    #[cfg(unix)]
    pub autobiography: Option<AutobiographyOutcome>,
    #[cfg(not(unix))]
    pub autobiography: Option<()>,
}

/// How post-filesystem index updates reach the Tantivy indexer task.
#[cfg(unix)]
pub enum IndexBackend {
    /// Reuse the daemon's live indexer channel (HTTP path).
    Sender(tokio::sync::mpsc::Sender<crate::server::tantivy_handle::IndexerMsg>),
    /// Open a fresh handle for this operation (CLI in-process path).
    FreshHandle,
    /// Skip index mutations (tests, or no index wired).
    Skip,
}

/// Options for post-filesystem phases (index + autobiography).
pub struct PostPhaseOptions<'a> {
    pub agent_root: &'a AgentRoot,
    pub project_root: &'a Path,
    pub cycle_date: &'a str,
    pub decay_result: &'a DecayResult,
    pub dirty_at_cycle_start: &'a [PathBuf],
    pub commit_autobiography: bool,
    #[cfg(unix)]
    pub index: IndexBackend,
}

/// Derive `YYYY-MM-DD` from a caller-supplied unix timestamp.
#[must_use]
pub fn cycle_date_from_now_sec(now_sec: i64) -> String {
    chrono::DateTime::from_timestamp(now_sec, 0)
        .unwrap_or(chrono::DateTime::UNIX_EPOCH)
        .format("%Y-%m-%d")
        .to_string()
}

/// Reject concurrent cycles (HTTP 409 guard).
pub fn ensure_not_in_progress(agent_root: &AgentRoot) -> Result<(), DreamCycleError> {
    match wal::read_last_cycle_status(agent_root)? {
        status if status == "in_progress" => Err(DreamCycleError::InProgress),
        _ => Ok(()),
    }
}

/// Filesystem phases: consolidation then decay.
///
/// Opens one WAL envelope before consolidation and commits it after decay so the
/// full cycle is a single atomic region (ARCH-2). Called by the coordinator
/// actor, which must reopen its append fd afterward.
pub fn run_filesystem_phases(
    agent_root: &AgentRoot,
    now_sec: i64,
    cycle_date: &str,
) -> Result<DecayResult, DreamCycleError> {
    // ARCH-2: ONE WAL envelope spans BOTH filesystem phases. begin_cycle guards
    // `.agent/` existence (WEG-281) and sets state=in_progress; commit_cycle sets
    // state=complete and deletes the WAL. A phase error short-circuits via `?`
    // BEFORE commit, leaving an uncommitted WAL for next-startup recovery
    // (state=failed) — so no half-finished cycle is ever recorded "complete".
    wal::begin_cycle(agent_root, now_sec)?;
    consolidation::run_deterministic_dream_cycle(agent_root, now_sec)?;
    let decay = decay::run_decay_pruner(agent_root, now_sec, cycle_date)?;
    wal::commit_cycle(agent_root, now_sec)?;
    Ok(decay)
}

/// Index + autobiography phases. Async because Tantivy ops run on the indexer task.
#[cfg(unix)]
pub async fn run_post_phases(
    opts: PostPhaseOptions<'_>,
) -> Result<Option<AutobiographyOutcome>, DreamCycleError> {
    run_index_phases(opts.agent_root, opts.decay_result, opts.index).await?;

    if !opts.commit_autobiography {
        return Ok(None);
    }

    match autobiography::commit_cycle(opts.agent_root, opts.cycle_date, opts.dirty_at_cycle_start) {
        Ok(outcome) => Ok(Some(outcome)),
        Err(e) => {
            tracing::error!(
                error = %e,
                "autobiography commit failed (dream cycle still succeeded)"
            );
            Ok(None)
        }
    }
}

#[cfg(not(unix))]
pub async fn run_post_phases(opts: PostPhaseOptions<'_>) -> Result<Option<()>, DreamCycleError> {
    let _ = opts;
    Ok(None)
}

/// Full in-process cycle for the CLI (`--no-commit` or no daemon).
pub fn run_in_process(
    project_root: &Path,
    now_sec: i64,
    no_commit: bool,
    dirty_at_cycle_start: Vec<PathBuf>,
) -> Result<DreamCycleResult, DreamCycleError> {
    let agent_root = AgentRoot::new(project_root);
    let cycle_date = cycle_date_from_now_sec(now_sec);

    let decay = run_filesystem_phases(&agent_root, now_sec, &cycle_date)?;

    #[cfg(unix)]
    {
        let autobiography = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime for dream cycle index phases")
            .block_on(run_post_phases(PostPhaseOptions {
                agent_root: &agent_root,
                project_root,
                cycle_date: &cycle_date,
                decay_result: &decay,
                dirty_at_cycle_start: &dirty_at_cycle_start,
                commit_autobiography: !no_commit,
                index: IndexBackend::FreshHandle,
            }))?;

        Ok(DreamCycleResult {
            decay,
            autobiography,
        })
    }

    #[cfg(not(unix))]
    {
        let _ = (no_commit, dirty_at_cycle_start, project_root);
        Ok(DreamCycleResult {
            decay,
            autobiography: None,
        })
    }
}

#[cfg(unix)]
async fn run_index_phases(
    agent_root: &AgentRoot,
    decay_result: &DecayResult,
    index: IndexBackend,
) -> Result<(), DreamCycleError> {
    match index {
        IndexBackend::Skip => Ok(()),
        IndexBackend::Sender(sender) => {
            apply_recurrence_sidecar_if_present(agent_root, &sender).await?;
            if !decay_result.decayed_ids.is_empty() {
                prune_decayed_events(&sender, decay_result.decayed_ids.clone()).await?;
            }
            Ok(())
        }
        IndexBackend::FreshHandle => {
            use crate::server::{TantivyIndexHandle, DEFAULT_COMMIT_CADENCE};

            let handle = TantivyIndexHandle::open(agent_root, DEFAULT_COMMIT_CADENCE)?;
            let sidecar_path = agent_root.semantic_dir().join("recurrence_counts.json");
            if sidecar_path.exists() {
                handle.apply_recurrence_sidecar(agent_root).await?;
            }
            if !decay_result.decayed_ids.is_empty() {
                handle
                    .prune_decayed_events(decay_result.decayed_ids.clone())
                    .await?;
            }
            Ok(())
        }
    }
}

#[cfg(unix)]
async fn apply_recurrence_sidecar_if_present(
    agent_root: &AgentRoot,
    sender: &tokio::sync::mpsc::Sender<crate::server::tantivy_handle::IndexerMsg>,
) -> Result<(), DreamCycleError> {
    use crate::server::tantivy_handle::IndexerMsg;
    use tokio::sync::oneshot;

    let sidecar_path = agent_root.semantic_dir().join("recurrence_counts.json");
    if !sidecar_path.exists() {
        return Ok(());
    }

    let (tx, rx) = oneshot::channel();
    sender
        .send(IndexerMsg::ApplyRecurrenceSidecar {
            agent_root: agent_root.clone(),
            response: tx,
        })
        .await
        .map_err(|_| crate::server::index_map::IndexError("indexer channel closed".to_string()))?;
    rx.await.map_err(|_| {
        crate::server::index_map::IndexError("indexer dropped response".to_string())
    })??;
    Ok(())
}

#[cfg(unix)]
async fn prune_decayed_events(
    sender: &tokio::sync::mpsc::Sender<crate::server::tantivy_handle::IndexerMsg>,
    event_ids: Vec<dreamd_protocol::EventId>,
) -> Result<(), DreamCycleError> {
    use crate::server::tantivy_handle::IndexerMsg;
    use tokio::sync::oneshot;

    let (tx, rx) = oneshot::channel();
    sender
        .send(IndexerMsg::PruneDecayedEvents {
            event_ids,
            response: tx,
        })
        .await
        .map_err(|_| crate::server::index_map::IndexError("indexer channel closed".to_string()))?;
    rx.await.map_err(|_| {
        crate::server::index_map::IndexError("indexer dropped response".to_string())
    })??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const NOW_SEC: i64 = 1_747_137_600;

    fn fixture_jsonl() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/dream-cycle-snapshot/AGENT_LEARNINGS.jsonl")
    }

    fn scaffold_fixture() -> (tempfile::TempDir, AgentRoot) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = AgentRoot::new(dir.path());
        let episodic_dir = root.episodic_jsonl().parent().unwrap().to_owned();
        fs::create_dir_all(&episodic_dir).unwrap();
        fs::copy(fixture_jsonl(), root.episodic_jsonl()).expect("copy fixture");
        fs::create_dir_all(root.dreamd_dir()).unwrap();
        (dir, root)
    }

    #[test]
    fn filesystem_phases_produces_lessons_on_fixture() {
        let (_dir, root) = scaffold_fixture();
        let cycle_date = cycle_date_from_now_sec(NOW_SEC);

        run_filesystem_phases(&root, NOW_SEC, &cycle_date).expect("filesystem phases");

        assert!(root.lessons_md().exists(), "LESSONS.md must exist");
        let sidecar = root.semantic_dir().join("recurrence_counts.json");
        assert!(sidecar.exists(), "recurrence_counts.json must exist");
    }

    #[test]
    fn in_process_full_cycle_on_fixture() {
        let (_dir, root) = scaffold_fixture();
        let project_root = root.project_root().to_path_buf();

        let result = run_in_process(&project_root, NOW_SEC, true, Vec::new())
            .expect("full in-process cycle");

        assert!(root.lessons_md().exists());
        assert!(!result.decay.decayed_ids.is_empty() || result.decay.kept_count > 0);
    }

    #[test]
    fn single_wal_envelope_committed_and_cleaned_after_full_cycle() {
        let (_dir, root) = scaffold_fixture();
        run_filesystem_phases(&root, NOW_SEC, &cycle_date_from_now_sec(NOW_SEC))
            .expect("filesystem phases");
        assert!(
            !root.wal_path().exists(),
            "single envelope committed + WAL cleaned"
        );
        assert_eq!(
            crate::wal::read_last_cycle_status(&root).unwrap(),
            "complete",
            "exactly one transition to complete, at the end"
        );
    }

    #[test]
    fn ensure_not_in_progress_rejects_active_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        fs::create_dir_all(root.dreamd_dir()).unwrap();
        wal::begin_cycle(&root, NOW_SEC).unwrap();

        let err = ensure_not_in_progress(&root).unwrap_err();
        assert!(matches!(err, DreamCycleError::InProgress));
    }

    #[test]
    fn seam_crash_after_consolidation_recovers_not_clean() {
        // Old two-envelope bug: run_deterministic_dream_cycle committed its OWN WAL,
        // so a crash after consolidation but before decay left NO WAL → recovery
        // returned Clean and state read "complete" (a half-cycle silently recorded
        // as success). With the single hoisted envelope the orchestrator's WAL stays
        // open across the seam, so the same crash is recoverable.
        let (_dir, root) = scaffold_fixture();

        // Orchestrator opens the one envelope...
        wal::begin_cycle(&root, NOW_SEC).unwrap();
        // ...consolidation runs and records its intents but no longer commits.
        consolidation::run_deterministic_dream_cycle(&root, NOW_SEC).unwrap();

        // Simulate a crash in the seam: decay + commit never run.
        assert!(root.wal_path().exists(), "WAL must remain open in the seam");
        let wal: crate::wal::DreamWal =
            serde_json::from_str(&std::fs::read_to_string(root.wal_path()).unwrap()).unwrap();
        assert!(
            !wal.intents.contains(&crate::wal::WalIntent::Commit),
            "no Commit intent — the cycle did not finish"
        );

        // Recovery must NOT report Clean, and state must NOT be left "complete".
        let outcome = crate::wal::recover_if_needed(&root, NOW_SEC).unwrap();
        assert!(
            matches!(outcome, crate::wal::RecoveryOutcome::Recovered { .. }),
            "seam crash must be recovered, not silently Clean"
        );
        assert_eq!(crate::wal::read_last_cycle_status(&root).unwrap(), "failed");
    }
}
