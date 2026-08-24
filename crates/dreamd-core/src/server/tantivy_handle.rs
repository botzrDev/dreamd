//! Per-project incremental Tantivy indexer (WEG-42 / DR-202).
//!
//! Wires the episodic JSONL at `<agent_root>/.agent/episodic/AGENT_LEARNINGS.jsonl`
//! into a per-project Tantivy index under `<agent_root>/.agent/.dreamd/index/`.
//! On startup, a two-pass replay walks the JSONL: pass 1 builds the final
//! `skill_action → recurrence` cluster counts; pass 2 indexes every event
//! whose `EventId` is strictly greater (string-compared on the lexicographically
//! sortable ULID) than the watermark in `index_progress.json`. From steady
//! state forward, the indexer task receives `IndexerMsg::Append` from the
//! `MemoryCoordinator` (WEG-7) after each successful `sync_data`, batches
//! `add_document` calls, and commits on a wall-clock cadence
//! ([`DEFAULT_COMMIT_CADENCE`], 5 seconds). The watermark is updated on disk
//! after each successful Tantivy commit, never before.
//!
//! **Idempotent-replay prose (WEG-42 Lock 3, verbatim).**
//!
//! > "On crash recovery, the batch since the last committed `last_indexed_id`
//! > will be re-indexed. This is intentional — Tantivy commits are atomic
//! > and `add_document` is idempotent for the same content. The replay
//! > re-does at most one 5-second window."
//!
//! **Lock-file behavior.** The on-disk `.tantivy-writer.lock` /
//! `.tantivy-meta.lock` files persist after SIGKILL but are not the gate —
//! Tantivy 0.26 uses `fs4` advisory flock, which the kernel releases when
//! the holder dies. Do NOT unlink them on startup or recovery. See drift
//! catalog entry `tantivy-lock-file-no-rm-on-startup`.
//!
//! **Coordinator → indexer wiring.** Option B (per the WEG-42 spec): the
//! supervisor opens a `TantivyIndexHandle`, extracts its
//! `mpsc::Sender<IndexerMsg>` via [`TantivyIndexHandle::sender`], and threads
//! that sender into `MemoryCoordinator::open` so each successful append can
//! `try_send` an [`IndexerMsg::Append`]. The coordinator never holds the
//! `IndexWriter`; the writer lives entirely on the indexer task. This keeps
//! each actor with exactly one mutable resource.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dreamd_protocol::{AgentLearning, EventId};
use serde::{Deserialize, Serialize};
use tantivy::directory::MmapDirectory;
use tantivy::{doc, Index, IndexReader, IndexWriter, TantivyDocument};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::index::{
    build_schema, check_manifest_version, ClusterCount, IndexManifest, Layer, ManifestCheckOutcome,
    ManifestVersionError, RecurrenceSidecar, SchemaFields, INDEX_MANIFEST_FILENAME, SCHEMA_VERSION,
};
use crate::io::write_atomic;
use crate::layout::AgentRoot;
use crate::server::index_map::{IndexError, IndexHandle};

/// Config-file parsing for `commit_cadence_seconds` and other runtime
/// settings is deferred to v0.1.1 (natural home: LLM cost cap ticket WEG-140
/// / DR-307). Do not add a config reader in this ticket.
///
/// Wall-clock cadence at which the indexer flushes accumulated `add_document`
/// calls to Tantivy. Production callers pass this value;
/// tests pass shorter durations for observability.
pub const DEFAULT_COMMIT_CADENCE: Duration = Duration::from_secs(5);

/// Writer heap budget, identical to the WEG-24 spike measurement
/// (`tests/bin/tantivy_spike.rs::WRITER_HEAP_BYTES`). No new spike is needed
/// — do not raise without one.
pub const WRITER_HEAP_BYTES: usize = 50_000_000;

/// Default mpsc capacity for the coordinator → indexer hand-off. Sized so a
/// 5-second commit window plus replay headroom fits without blocking the
/// coordinator. On `TrySendError::Full`, the coordinator logs `warn!` and
/// drops the indexer update — the JSONL is the source of truth, so dropping
/// an index message is recoverable by the next startup replay.
pub(crate) const DEFAULT_INDEXER_CHANNEL_CAPACITY: usize = 1024;

/// Relative filename for the indexer's commit watermark, joined under the
/// project's `.dreamd/` directory. WEG-42 owns reads and writes; `dreamd
/// doctor --repair` (AILAB-223) clears it when wiping the rebuildable cache.
pub const INDEX_PROGRESS_FILENAME: &str = "index_progress.json";

/// Relative directory holding the per-project Tantivy index segments.
pub const INDEX_DIR_NAME: &str = "index";

/// Relative filename for the semantic pass's report of lessons it could not
/// index, joined under the project's `.dreamd/` directory. Written by
/// [`index_semantic_lessons`], read by `dreamd doctor` (AILAB-700).
pub const SEMANTIC_PASS_FILENAME: &str = "semantic_pass.json";

/// v0.1 index-vs-JSONL contract surface (WEG-42 / DR-202).
///
/// Compares the JSONL tail against `index_progress.json`. `stale == true` when
/// the episodic log has committed events the index watermark has not caught up
/// to yet — including the normal ≤[`DEFAULT_COMMIT_CADENCE`] window after a
/// live append, channel-saturation drops (`try_send` → `Full`), or a crash
/// between JSONL `sync_data` and the next Tantivy commit. JSONL durability is
/// WAL-backed; index freshness is best-effort and heals on the next
/// `TantivyIndexHandle::open` replay (or when the indexer commits the backlog).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IndexFreshness {
    /// `true` when `jsonl_tail_id` is strictly greater than `last_indexed_id`
    /// (lexicographic `EventId` order), or when JSONL has events but the
    /// watermark is absent.
    pub stale: bool,
    /// `id` of the last well-formed JSONL record, if any.
    pub jsonl_tail_id: Option<String>,
    /// `last_indexed_id` from `index_progress.json`, if present.
    pub last_indexed_id: Option<String>,
    /// Count of JSONL events strictly after the watermark (0 when fresh).
    pub unindexed_count: usize,
}

/// Assess on-disk index freshness for `agent_root` without opening Tantivy.
///
/// Operators and `GET /api/v1/health` use this to detect recall lag relative to
/// the JSONL source of truth. Does not consult the live indexer channel.
pub fn assess_index_freshness(agent_root: &AgentRoot) -> Result<IndexFreshness, IndexError> {
    let progress_path = agent_root.dreamd_dir().join(INDEX_PROGRESS_FILENAME);
    let progress = read_progress(&progress_path)?;
    let watermark = progress.last_indexed_id.as_deref();

    let events = read_jsonl_events(&agent_root.episodic_jsonl())?;
    let jsonl_tail_id = events.last().map(|ev| ev.id.as_str().to_owned());
    let unindexed_count = events
        .iter()
        .filter(|ev| match watermark {
            Some(last) => ev.id.as_str() > last,
            None => true,
        })
        .count();
    let stale = unindexed_count > 0;

    Ok(IndexFreshness {
        stale,
        jsonl_tail_id,
        last_indexed_id: progress.last_indexed_id,
        unindexed_count,
    })
}

/// What the last semantic (LESSONS.md) pass could not index (AILAB-700).
///
/// Lives at `<agent_root>/.agent/.dreamd/semantic_pass.json`. It exists so
/// `dreamd doctor` can report un-indexable lessons without opening the index
/// and without re-resolving LESSONS.md against the episodic log — a second
/// parser for the same rule is exactly the drift this avoids.
///
/// Deliberately carries **no timestamp**: `dream` reads wall-clock, which would
/// make the file non-deterministic for no operator benefit. Doctor reports the
/// current state of the store, not when that state was measured.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticPassRecord {
    /// `lesson.id`s dropped because the exemplar was not in the episodic log.
    pub skipped_lesson_ids: Vec<String>,
    /// The clustering key of the LESSONS.md those lessons came from.
    pub cluster_key: String,
    /// Lessons successfully added as `layer=semantic` documents.
    pub indexed: usize,
}

/// Read the semantic pass report for `agent_root` without opening Tantivy.
///
/// `Ok(None)` when the file is absent — a store that has never dreamed has no
/// report, which is a fact and not a fault. Same shape as
/// [`assess_index_freshness`]: `dreamd doctor` is most valuable when the daemon
/// is down, so this path opens no index and takes no lock.
pub fn read_semantic_pass_record(
    agent_root: &AgentRoot,
) -> Result<Option<SemanticPassRecord>, IndexError> {
    let path = agent_root.dreamd_dir().join(SEMANTIC_PASS_FILENAME);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| IndexError::Other(format!("parse {SEMANTIC_PASS_FILENAME}: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(IndexError::Other(format!(
            "read {SEMANTIC_PASS_FILENAME}: {e}"
        ))),
    }
}

/// Crash-recovery watermark recording the daemon-assigned `EventId` of the
/// most recently committed document. Lives at
/// `<agent_root>/.dreamd/index_progress.json`. Reads on startup;
/// writes after each successful Tantivy commit, never before.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexProgress {
    /// `evt_`-prefixed ULID string of the most recently committed document,
    /// or `None` if no commit has succeeded yet (cold start / empty JSONL).
    pub(crate) last_indexed_id: Option<String>,
}

/// Messages accepted by the indexer task.
///
/// `#[non_exhaustive]` keeps the enum forward-compatible — additional
/// variants (e.g., `Delete`, `Rewrite`) will land in later tickets without
/// breaking exhaustive matches in callers.
#[non_exhaustive]
pub enum IndexerMsg {
    /// Coordinator → indexer hand-off after a durable JSONL append.
    ///
    /// Older docs in a cluster intentionally carry the recurrence value at
    /// their index time, not the live cluster count. This is bounded
    /// staleness — stale rows underweight their cluster, never overweight.
    /// Reconciliation is a dream-cycle concern, not a v0.1 indexer concern.
    Append {
        event_id: EventId,
        learning: AgentLearning,
    },
    /// Drives deterministic flush in tests; production commits run on the
    /// cadence ticker. The `ack` oneshot resolves with `Ok(())` after a
    /// successful Tantivy commit + progress-file update, or `Err(IndexError)`
    /// if either step failed.
    Flush {
        ack: oneshot::Sender<Result<(), IndexError>>,
    },
    /// Dream-cycle hook (WEG-45 / DR-205′): read `semantic/recurrence_counts.json`,
    /// walk the JSONL, delete-and-re-add each event with the authoritative
    /// cluster count, then commit. Resolves after the commit completes.
    ApplyRecurrenceSidecar {
        agent_root: AgentRoot,
        response: oneshot::Sender<Result<(), IndexError>>,
    },
    /// Decay pruner hook (WEG-62 / DR-309): delete decayed event IDs from the index.
    /// Does not touch the JSONL — JSONL rewrite is handled by `run_decay_pruner`.
    PruneDecayedEvents {
        event_ids: Vec<EventId>,
        response: oneshot::Sender<Result<(), IndexError>>,
    },
    /// Dream-cycle hook (DR-211 / AILAB-205): re-read `semantic/LESSONS.md`,
    /// replace every `layer=semantic` document with the file's current lesson
    /// set, then commit. Lets a running daemon pick up the lessons consolidation
    /// just wrote without waiting for a restart. Never touches episodic
    /// documents or the episodic watermark. Resolves after the commit completes.
    IndexSemanticLessons {
        agent_root: AgentRoot,
        response: oneshot::Sender<Result<(), IndexError>>,
    },
}

/// Owning handle for the spawned indexer task. Constructed inside
/// [`TantivyIndexHandle::open`] and held privately. Dropped (and task
/// aborted or drained) when [`TantivyIndexHandle`] is closed or shut down.
pub(crate) struct IndexerHandle {
    tx: mpsc::Sender<IndexerMsg>,
    join: JoinHandle<()>,
}

impl IndexerHandle {
    pub(crate) fn sender(&self) -> mpsc::Sender<IndexerMsg> {
        self.tx.clone()
    }
}

/// Tantivy-backed concrete [`IndexHandle`] for one project root.
pub struct TantivyIndexHandle {
    last_used: Mutex<Instant>,
    indexer: IndexerHandle,
    reader: IndexReader,
}

impl TantivyIndexHandle {
    /// Open (or create) the per-project Tantivy index under
    /// `<agent_root>/.agent/.dreamd/index/`. On first open for a new project,
    /// writes `<agent_root>/.agent/.dreamd/index_manifest.json` with the
    /// binary's current `SCHEMA_VERSION`. Spawns the indexer task on the
    /// current tokio runtime and runs the startup replay before returning.
    pub fn open(agent_root: &AgentRoot, commit_cadence: Duration) -> Result<Self, IndexError> {
        let dreamd_dir = agent_root.dreamd_dir();
        std::fs::create_dir_all(&dreamd_dir).map_err(io_to_index)?;

        let index_dir = dreamd_dir.join(INDEX_DIR_NAME);
        std::fs::create_dir_all(&index_dir).map_err(io_to_index)?;

        let manifest_path = dreamd_dir.join(INDEX_MANIFEST_FILENAME);
        let progress_path = dreamd_dir.join(INDEX_PROGRESS_FILENAME);
        let jsonl_path = agent_root.episodic_jsonl();

        // Index schema bump => rebuild the derived cache from JSONL (ARCHITECTURE.md §4).
        // This is NOT `dreamd migrate` (§7, durable data); the index is a rebuildable cache.
        match check_manifest_version(&manifest_path) {
            Ok(ManifestCheckOutcome::NeedsMigration { from }) => {
                tracing::warn!(
                    from,
                    to = SCHEMA_VERSION,
                    "index schema outdated; rebuilding from JSONL"
                );
                let _ = std::fs::remove_dir_all(&index_dir);
                let _ = std::fs::remove_file(&manifest_path);
                // Reset watermark so replay_two_pass re-indexes the full JSONL, not just the tail.
                let _ = std::fs::remove_file(&progress_path);
                std::fs::create_dir_all(&index_dir).map_err(io_to_index)?;
            }
            Err(ManifestVersionError::TooNew { manifest, binary }) => {
                return Err(IndexError::Other(format!(
                    "index schema {manifest:?} is newer than binary {binary:?}; \
                     upgrade dreamd, or wipe the index dir to rebuild under this binary"
                )));
            }
            _ => {}
        }

        let (schema, fields) = build_schema();
        let index = open_or_create_index(&index_dir, schema, &manifest_path, &progress_path)?;
        write_manifest_if_absent(&manifest_path)?;
        let reader = index.reader().map_err(tantivy_to_index)?;
        let mut writer: IndexWriter<TantivyDocument> =
            index.writer(WRITER_HEAP_BYTES).map_err(tantivy_to_index)?;

        let progress = read_progress(&progress_path)?;
        let last_indexed = progress.last_indexed_id.clone();
        let (mut clusters, to_index) = replay_two_pass(&jsonl_path, last_indexed.as_deref())?;

        let mut latest_committed = last_indexed.clone();
        for event in &to_index {
            let count = *clusters
                .get(&event.skill_action)
                .expect("pass-1 populates every skill_action present in pass-2");
            add_document(&mut writer, &fields, event, count)?;
            latest_committed = Some(event.id.as_str().to_owned());
        }
        // DR-211 / AILAB-205: fold `semantic/LESSONS.md` into the same rebuild,
        // after the episodic replay and before the single commit below. One
        // insertion point covers three recovery paths — cold start, the
        // schema-migration rebuild above, and `dreamd doctor --repair` (which
        // reopens through this same function, so `doctor.rs` needs no change).
        let semantic = index_semantic_lessons(&mut writer, &fields, agent_root)?;

        // The semantic pass has its own reason to commit: after a crash between
        // the LESSONS.md write and the Tantivy commit, the episodic watermark is
        // already current, so `to_index` is empty and the old
        // `!to_index.is_empty()` gate would have skipped the commit and dropped
        // the replayed lessons on the floor.
        let needs_commit = !to_index.is_empty() || semantic.touched;
        if needs_commit {
            writer.commit().map_err(tantivy_to_index)?;
            // Watermark tracks the episodic log only. The semantic pass is a
            // wholesale delete-then-add and is idempotent by construction, so it
            // deliberately has no watermark of its own (AILAB-205 §3 step 3).
            if !to_index.is_empty() {
                write_progress(
                    &progress_path,
                    &IndexProgress {
                        last_indexed_id: latest_committed.clone(),
                    },
                )?;
            }
            // The reader was created before this replay commit and uses the
            // default `ReloadPolicy::OnCommitWithDelay`, so the first query
            // after open would otherwise see a stale (pre-replay) view until
            // the background reload fires. Force a synchronous reload so recall
            // reflects the replayed tail the moment `open()` returns — this is
            // the cross-harness read-after-open case (a fresh process indexing
            // a prior process's append). Steady-state cadence commits from
            // `run_indexer` are still picked up by OnCommitWithDelay. (WEG-264
            // Defect 1.)
            reader.reload().map_err(tantivy_to_index)?;
        }

        // Build steady-state cluster counts: pass-1 already counted the
        // entire JSONL, which is the correct live state going forward.
        // (Replay only re-indexes the tail past last_indexed_id, but the
        // cluster counter must reflect all prior events for new appends.)
        // No additional work needed — `clusters` already holds the full count.
        let _ = &mut clusters;

        let (tx, rx) = mpsc::channel::<IndexerMsg>(DEFAULT_INDEXER_CHANNEL_CAPACITY);
        let join = tokio::spawn(run_indexer(
            writer,
            fields,
            rx,
            commit_cadence,
            clusters,
            progress_path.clone(),
            latest_committed,
        ));

        Ok(Self {
            last_used: Mutex::new(Instant::now()),
            indexer: IndexerHandle { tx, join },
            reader,
        })
    }

    /// Clone the sender for the indexer task. Pass into
    /// `MemoryCoordinator::open` so coordinator appends route to this index.
    pub fn sender(&self) -> mpsc::Sender<IndexerMsg> {
        self.indexer.sender()
    }

    /// Returns a reference to the Tantivy `IndexReader` for this project.
    /// Used by HTTP handlers to execute recall queries (WEG-69).
    pub fn reader(&self) -> &IndexReader {
        &self.reader
    }

    /// Update `last_used` to `Instant::now()`. Called by the supervisor on
    /// each access through `ProjectIndexMap::get_or_open`.
    pub fn touch(&self) {
        *self.last_used.lock().expect("last_used mutex poisoned") = Instant::now();
    }

    /// Apply the on-disk recurrence sidecar (`semantic/recurrence_counts.json`)
    /// to the Tantivy index. For each cluster listed in the sidecar, every
    /// event belonging to that `skill_action` is deleted and re-added with the
    /// authoritative `count` as its `recurrence` FastField value. A single
    /// commit is issued after all clusters are processed.
    ///
    /// Called by the dream cycle after it writes the sidecar (WEG-45 / DR-205′).
    pub async fn apply_recurrence_sidecar(&self, agent_root: &AgentRoot) -> Result<(), IndexError> {
        let (tx, rx) = oneshot::channel();
        self.indexer
            .sender()
            .send(IndexerMsg::ApplyRecurrenceSidecar {
                agent_root: agent_root.clone(),
                response: tx,
            })
            .await
            .map_err(|_| IndexError::ChannelClosed)?;
        rx.await.map_err(|_| IndexError::TaskDropped)?
    }

    /// Remove decayed event IDs from the Tantivy index (WEG-62 / DR-309).
    /// Called by the dream cycle orchestrator after `run_decay_pruner` returns.
    /// No-op if `event_ids` is empty (still sends message to stay on the indexer task).
    pub async fn prune_decayed_events(&self, event_ids: Vec<EventId>) -> Result<(), IndexError> {
        let (tx, rx) = oneshot::channel();
        self.indexer
            .sender()
            .send(IndexerMsg::PruneDecayedEvents {
                event_ids,
                response: tx,
            })
            .await
            .map_err(|_| IndexError::ChannelClosed)?;
        rx.await.map_err(|_| IndexError::TaskDropped)?
    }

    /// Re-index `<agent_root>/.agent/semantic/LESSONS.md` as `layer=semantic`
    /// documents (DR-211 / AILAB-205). Called by the dream cycle after
    /// consolidation rewrites the file so a live daemon serves the new lessons
    /// without a restart.
    ///
    /// Idempotent: the pass deletes every semantic document and re-adds the
    /// file's current lesson set, so running it twice is indistinguishable from
    /// running it once. A missing LESSONS.md is a silent no-op.
    pub async fn index_semantic_lessons(&self, agent_root: AgentRoot) -> Result<(), IndexError> {
        let (tx, rx) = oneshot::channel();
        self.indexer
            .sender()
            .send(IndexerMsg::IndexSemanticLessons {
                agent_root,
                response: tx,
            })
            .await
            .map_err(|_| IndexError::ChannelClosed)?;
        rx.await.map_err(|_| IndexError::TaskDropped)?
    }

    /// Force a Tantivy commit + progress-file update and wait for it.
    ///
    /// Promoted from the test-only helper for AILAB-162: `run_watch`'s
    /// shutdown drain prefers [`Self::shutdown`], but that consumes `self` and
    /// the primary handle is an `Arc` shared with `AppState`. When
    /// `Arc::try_unwrap` cannot recover sole ownership, this is the drain — it
    /// rides the existing [`IndexerMsg::Flush`] path, so there is exactly one
    /// commit implementation (`commit_and_persist`) and no second, shutdown-only
    /// code path that could drift from it.
    ///
    /// Unlike `shutdown` it leaves the indexer task running. That is the point:
    /// a shared handle has other owners, and the process is about to exit
    /// anyway — what must not be lost is the batch already accumulated in the
    /// writer, which this commits.
    pub(crate) async fn flush(&self) -> Result<(), IndexError> {
        let (tx, rx) = oneshot::channel();
        self.sender()
            .send(IndexerMsg::Flush { ack: tx })
            .await
            .map_err(|_| IndexError::ChannelClosed)?;
        rx.await.map_err(|_| IndexError::TaskDropped)?
    }

    /// Async drain path. Drops the indexer sender, awaits task completion
    /// (which performs a final commit before exiting). Preferred over
    /// [`IndexHandle::close`] when called from an async context — sidesteps
    /// the sync/async runtime juggling in `close`.
    pub async fn shutdown(self) -> Result<(), IndexError> {
        let IndexerHandle { tx, join } = self.indexer;
        drop(tx);
        join.await
            .map_err(|e| IndexError::Other(format!("indexer task join: {e}")))?;
        Ok(())
    }
}

impl IndexHandle for TantivyIndexHandle {
    /// Sync close path for [`crate::server::ProjectIndexMap`] eviction.
    ///
    /// When invoked from outside a tokio runtime (typical `Drop` /
    /// supervisor-shutdown timing), `close` builds a fresh current-thread
    /// runtime and drains the indexer task — that path produces the same
    /// final commit as [`Self::shutdown`].
    ///
    /// When invoked from inside a runtime, `close` cannot safely block on
    /// the indexer's `JoinHandle`: this crate's tokio features
    /// (`["sync", "rt", "macros", "time"]`, drift slug
    /// `tokio-feature-split-bin-target-exception`) deliberately exclude
    /// `rt-multi-thread`, so `tokio::task::block_in_place` is unavailable.
    /// In that case `close` drops the sender and `abort`s the task —
    /// the in-flight batch is lost. Async callers should prefer
    /// [`Self::shutdown`] for guaranteed drain.
    fn close(self) -> Result<(), IndexError> {
        let IndexerHandle { tx, join } = self.indexer;
        drop(tx);
        match tokio::runtime::Handle::try_current() {
            Ok(_) => {
                join.abort();
            }
            Err(_) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| IndexError::Other(format!("build runtime for close: {e}")))?;
                rt.block_on(async move {
                    let _ = join.await;
                });
            }
        }
        Ok(())
    }

    fn last_used(&self) -> Instant {
        *self.last_used.lock().expect("last_used mutex poisoned")
    }
}

// Indexer task

async fn run_indexer(
    mut writer: IndexWriter<TantivyDocument>,
    fields: SchemaFields,
    mut rx: mpsc::Receiver<IndexerMsg>,
    commit_cadence: Duration,
    mut clusters: HashMap<String, u32>,
    progress_path: PathBuf,
    mut last_committed_id: Option<String>,
) {
    let mut batch_last_id: Option<String> = None;
    let mut interval = tokio::time::interval(commit_cadence);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Burn the first immediate tick so the cadence is measured from
    // construction time, not zero.
    interval.tick().await;

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Some(IndexerMsg::Append { event_id, learning }) => {
                        let counter = clusters
                            .entry(learning.skill_action.clone())
                            .or_insert(0);
                        *counter += 1;
                        let recurrence = *counter;
                        if let Err(e) = add_document(&mut writer, &fields, &learning, recurrence) {
                            tracing::warn!(error = ?e, "indexer add_document failed");
                            continue;
                        }
                        batch_last_id = Some(event_id.as_str().to_owned());
                    }
                    Some(IndexerMsg::Flush { ack }) => {
                        let result = commit_and_persist(
                            &mut writer,
                            &progress_path,
                            &mut batch_last_id,
                            &mut last_committed_id,
                        );
                        let _ = ack.send(result);
                    }
                    Some(IndexerMsg::ApplyRecurrenceSidecar { agent_root, response }) => {
                        let result = apply_recurrence_sidecar_inner(
                            &mut writer,
                            &fields,
                            &agent_root,
                        );
                        let _ = response.send(result);
                    }
                    Some(IndexerMsg::IndexSemanticLessons { agent_root, response }) => {
                        let result = (|| -> Result<(), IndexError> {
                            let outcome = index_semantic_lessons(&mut writer, &fields, &agent_root)?;
                            // No delete/add was issued (unreadable or malformed
                            // LESSONS.md) — nothing to commit. A *missing* file
                            // does issue a delete: it retires the layer.
                            if outcome.touched {
                                writer.commit().map_err(tantivy_to_index)?;
                            }
                            Ok(())
                        })();
                        let _ = response.send(result);
                    }
                    Some(IndexerMsg::PruneDecayedEvents { event_ids, response }) => {
                        let result = (|| -> Result<(), IndexError> {
                            for id in &event_ids {
                                let term = tantivy::Term::from_field_text(fields.event_id, id.as_str());
                                writer.delete_term(term);
                            }
                            writer.commit().map_err(tantivy_to_index)?;
                            Ok(())
                        })();
                        let _ = response.send(result);
                    }
                    None => {
                        // Channel closed: final flush, then exit.
                        let _ = commit_and_persist(
                            &mut writer,
                            &progress_path,
                            &mut batch_last_id,
                            &mut last_committed_id,
                        );
                        break;
                    }
                }
            }
            _ = interval.tick() => {
                if batch_last_id.is_some() {
                    if let Err(e) = commit_and_persist(
                        &mut writer,
                        &progress_path,
                        &mut batch_last_id,
                        &mut last_committed_id,
                    ) {
                        tracing::warn!(error = ?e, "indexer cadence commit failed");
                    }
                }
            }
        }
    }
}

fn commit_and_persist(
    writer: &mut IndexWriter<TantivyDocument>,
    progress_path: &Path,
    batch_last_id: &mut Option<String>,
    last_committed_id: &mut Option<String>,
) -> Result<(), IndexError> {
    let Some(new_last) = batch_last_id.take() else {
        return Ok(());
    };
    // Write protocol (WEG-42): Tantivy commit first, then watermark on disk.
    // If we crash after commit but before write_progress, the next startup
    // replay re-indexes at most one 5-second window (idempotent). If we wrote
    // the watermark first and then crashed, those events would be silently
    // skipped on recovery -- silent data loss.
    writer.commit().map_err(tantivy_to_index)?;
    *last_committed_id = Some(new_last);
    write_progress(
        progress_path,
        &IndexProgress {
            last_indexed_id: last_committed_id.clone(),
        },
    )?;
    Ok(())
}

/// Implements the delete-and-re-add recurrence update triggered by
/// [`IndexerMsg::ApplyRecurrenceSidecar`] (WEG-45 / DR-205′).
///
/// Algorithm:
/// 1. Read `<agent_root>/.agent/semantic/recurrence_counts.json`.
/// 2. Parse every line of the JSONL into a per-`skill_action` bucket.
/// 3. For each cluster in the sidecar: delete every matching event by its
///    `event_id` term, then re-add the event document with the sidecar's
///    authoritative `count` as the `recurrence` FastField value.
/// 4. Commit once after all clusters are processed.
fn apply_recurrence_sidecar_inner(
    writer: &mut IndexWriter<TantivyDocument>,
    fields: &SchemaFields,
    agent_root: &AgentRoot,
) -> Result<(), IndexError> {
    // 1. Read and parse the sidecar.
    let sidecar_path = agent_root.semantic_dir().join("recurrence_counts.json");
    let sidecar_json = std::fs::read_to_string(&sidecar_path)
        .map_err(|e| IndexError::Other(format!("read recurrence_counts.json: {e}")))?;
    let sidecar: RecurrenceSidecar = serde_json::from_str(&sidecar_json)
        .map_err(|e| IndexError::Other(format!("parse recurrence_counts.json: {e}")))?;

    // 2. Walk the JSONL (shared episodic scan, WEG-378) and bucket by skill_action.
    let jsonl_path = agent_root.episodic_jsonl();
    let events = read_jsonl_events(&jsonl_path)?;
    if events.is_empty() {
        // Nothing indexed yet — sidecar application is a no-op.
        return Ok(());
    }
    let mut by_skill: HashMap<String, Vec<AgentLearning>> = HashMap::new();
    for learning in events {
        by_skill
            .entry(learning.skill_action.clone())
            .or_default()
            .push(learning);
    }

    // 3. Delete-and-re-add for each cluster listed in the sidecar.
    for ClusterCount {
        skill_action,
        count,
    } in &sidecar.clusters
    {
        let events = match by_skill.get(skill_action) {
            Some(v) => v,
            None => continue,
        };
        for event in events {
            // Delete the existing document by its exact event_id term.
            let id_str = event.id.as_str().to_string();
            let term = tantivy::Term::from_field_text(fields.event_id, &id_str);
            writer.delete_term(term);
            // Re-add with the sidecar-authoritative recurrence count.
            add_document(writer, fields, event, *count)?;
        }
    }

    // 4. Commit once after all clusters.
    writer.commit().map_err(tantivy_to_index)?;
    Ok(())
}

/// What one semantic (LESSONS.md) indexing pass did.
///
/// `touched` records whether the pass issued any delete/add operation, so the
/// caller knows a commit is required. It is `true` from the wholesale delete
/// onward — even when every lesson was skipped, because the delete alone
/// changes the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct SemanticPassOutcome {
    /// Lessons added as `layer=semantic` documents.
    pub(crate) indexed: usize,
    /// Lessons dropped because their exemplar event was not in the episodic log.
    pub(crate) skipped: usize,
    /// `true` once the pass has mutated the writer (delete and/or add).
    pub(crate) touched: bool,
}

/// Identity token for a lesson document: the exemplar's id in a distinct
/// namespace (DR-211 / AILAB-205).
///
/// Load-bearing, not cosmetic. Both existing delete sites key on the raw
/// `event_id` term — the decay pruner and the recurrence sidecar — so a lesson
/// carrying its exemplar's id verbatim would be deleted alongside the event,
/// and the sidecar would re-add only the episodic document. A separate
/// namespace keeps both paths correct without touching either. It also keeps
/// lessons out of decay entirely: the pruner's candidate ids come from the
/// JSONL, which holds no `lsn_` records.
fn semantic_event_id(lesson_id: &str) -> String {
    format!("lsn_{lesson_id}")
}

/// Count the episodic events belonging to `cluster_key`, using the same prefix
/// semantics `consolidation` promotes with.
///
/// `compute_promoted_clusters` promotes at the *deepest* prefix that met the
/// threshold, so member events routinely carry longer leaf keys — three events
/// under `rust::eh::unwrap` and two under `rust::eh::expect` can promote as
/// `rust::eh`. An exact-match count would report 0 members for exactly those
/// clusters, and `recurrence` feeds the salience product.
fn cluster_member_count(events: &[AgentLearning], cluster_key: &str) -> u64 {
    let child_prefix = format!("{cluster_key}::");
    events
        .iter()
        .filter(|ev| ev.skill_action == cluster_key || ev.skill_action.starts_with(&child_prefix))
        .count() as u64
}

/// Index `<agent_root>/.agent/semantic/LESSONS.md` as `layer=semantic`
/// documents (DR-211 / AILAB-205).
///
/// Wholesale replace: delete every semantic document, then add the file's
/// current lesson set. LESSONS.md is rewritten in full each dream cycle, so the
/// index mirrors that — a lesson dropped between cycles disappears, and a
/// cluster that stops recurring retires structurally with no expiry logic.
///
/// Does **not** commit. The caller owns the commit so this pass can share the
/// episodic replay's single commit inside [`TantivyIndexHandle::open`].
///
/// Exemplar lookup is the live episodic log only; a miss skips the lesson with
/// a `warn!` (AILAB-205 rev 3 cut the snapshot fallback — `apply_pin_unpin`
/// pins every cited exemplar and `should_decay` short-circuits on `pinned`, so
/// a cited exemplar does not age out). A lesson is never indexed with defaulted
/// pain/importance: that document would score exactly 0.0 and could never rank.
///
/// Tolerate-and-report, in the posture `episodic::read_all` uses — but the two
/// failure modes are deliberately **not** symmetric (AILAB-699):
///
/// * **Missing** LESSONS.md is a *fact*, not a failure: either the store has
///   never dreamed, or a no-promotion cycle retired the file. Both mean zero
///   lessons, so the pass deletes the semantic layer and reports `touched`,
///   silently and with no log line.
/// * **Unreadable or malformed** LESSONS.md is a failure of unknown extent: the
///   lessons may still be there behind a torn write. It logs at `warn!` and
///   returns untouched, so a corrupt file can never wipe the layer. The parse
///   deliberately happens **before** the delete for the same reason.
///
/// Every path that resolves lessons also writes a [`SemanticPassRecord`] to
/// `.dreamd/semantic_pass.json` so `dreamd doctor` can name the skipped lessons
/// (AILAB-700). The write lives here, not at the call sites, because both
/// consumers — the [`TantivyIndexHandle::open`] rebuild and the indexer task's
/// `IndexSemanticLessons` handler — go through this one function. The malformed
/// path deliberately writes nothing: it did not touch the index, so the prior
/// record still describes it.
fn index_semantic_lessons(
    writer: &mut IndexWriter<TantivyDocument>,
    fields: &SchemaFields,
    agent_root: &AgentRoot,
) -> Result<SemanticPassOutcome, IndexError> {
    let lessons_path = agent_root.lessons_md();
    let lessons_file = match crate::lessons::read_lessons_file(&lessons_path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // An absent LESSONS.md means "zero lessons", not "skip the pass"
            // (AILAB-699). A no-promotion dream cycle *unlinks* the file — that
            // unlink is the whole retirement mechanism, and without this delete
            // the retired lesson keeps answering recall out of the live index
            // until the daemon restarts. Same STRING exact-match term as the
            // wholesale replace below, so it hits every semantic document and
            // zero episodic ones.
            //
            // `touched: true` is what makes the caller commit. On a store that
            // has never dreamed this costs one delete of an empty layer plus a
            // commit; that is the accepted price of not needing a "did a cycle
            // just remove this file?" signal threaded down here.
            let term = tantivy::Term::from_field_text(fields.layer, Layer::Semantic.as_str());
            writer.delete_term(term);
            // A retired file has no un-indexable lessons, so clear any prior
            // report (AILAB-700) — otherwise doctor keeps naming lessons that
            // no longer exist.
            write_semantic_pass_record(agent_root, &SemanticPassRecord::default());
            return Ok(SemanticPassOutcome {
                indexed: 0,
                skipped: 0,
                touched: true,
            });
        }
        Err(e) => {
            tracing::warn!(
                path = %lessons_path.display(),
                error = %e,
                "LESSONS.md unreadable; leaving the index untouched"
            );
            // Deliberately no report write: the index was not touched, so the
            // prior record is still the truth about what is in it.
            return Ok(SemanticPassOutcome::default());
        }
    };

    // Only read the episodic log once we know there is a LESSONS.md to index,
    // so a project that has never dreamed pays no extra I/O here.
    let events = read_jsonl_events(&agent_root.episodic_jsonl())?;
    let exemplars: HashMap<&str, &AgentLearning> =
        events.iter().map(|ev| (ev.id.as_str(), ev)).collect();
    let member_count = cluster_member_count(&events, &lessons_file.cluster_key);

    // `layer` is STRING (raw-tokenized), so this exact-match term hits every
    // semantic document and zero episodic ones. Deleting by the file's
    // clustering key instead would take every episodic event in it as well.
    let term = tantivy::Term::from_field_text(fields.layer, Layer::Semantic.as_str());
    writer.delete_term(term);

    let mut outcome = SemanticPassOutcome {
        indexed: 0,
        skipped: 0,
        touched: true,
    };
    // Kept local rather than on `SemanticPassOutcome`: that type is `Copy` and
    // built with struct-literal syntax at every return, and a `Vec` field would
    // break both (AILAB-700).
    let mut skipped_lesson_ids: Vec<String> = Vec::new();
    for lesson in &lessons_file.lessons {
        let Some(exemplar) = exemplars.get(lesson.id.as_str()) else {
            tracing::warn!(
                lesson_id = %lesson.id,
                cluster_key = %lessons_file.cluster_key,
                path = %lessons_path.display(),
                "lesson exemplar is not in the episodic log; skipping the lesson \
                 (indexing it without the exemplar's pain/importance would produce \
                 a document that scores 0.0 and can never rank)"
            );
            outcome.skipped += 1;
            skipped_lesson_ids.push(lesson.id.clone());
            continue;
        };
        add_semantic_document(
            writer,
            fields,
            &lessons_file,
            lesson,
            exemplar,
            member_count,
        )?;
        outcome.indexed += 1;
    }

    // Written on every full pass, including the zero-skip one: without that,
    // a store whose exemplars were restored would keep reporting yesterday's
    // skips forever (AILAB-700).
    write_semantic_pass_record(
        agent_root,
        &SemanticPassRecord {
            skipped_lesson_ids,
            cluster_key: lessons_file.cluster_key.clone(),
            indexed: outcome.indexed,
        },
    );

    Ok(outcome)
}

/// Map one [`crate::lessons::Lesson`] onto a `layer=semantic` Tantivy document.
///
/// `pain` and `importance` are inherited from the exemplar event because a
/// lesson has none of its own: `collector::recall` reads both fast fields with
/// `unwrap_or(0.0)` and the salience product multiplies by each, so a lesson
/// indexed without them would score exactly 0.0 and never surface.
///
/// `timestamp_sec` is the file's `last_updated` — the consolidation time — and
/// deliberately **not** the exemplar's timestamp. Salience decays as
/// `exp(-age_days/14)`, which asks "how stale is this claim?"; for an event that
/// is when it fired, but for a lesson it is when consolidation last re-affirmed
/// it. A lesson distilled today from a 90-day-old exemplar would otherwise score
/// `exp(-6.43) ~= 0.0016` and rank two orders of magnitude below any fresh
/// event — indexed, matching, and permanently buried.
///
/// `source_harness` is the literal `"dreamd"` rather than the exemplar's
/// harness — a promoted cluster spans harnesses, so attributing the synthesized
/// lesson to one contributor would misreport provenance. The exemplar (and its
/// harness) stay one hop away through `event_id`.
fn add_semantic_document(
    writer: &mut IndexWriter<TantivyDocument>,
    fields: &SchemaFields,
    lessons_file: &crate::lessons::LessonsFile,
    lesson: &crate::lessons::Lesson,
    exemplar: &AgentLearning,
    member_count: u64,
) -> Result<(), IndexError> {
    // Consolidation time, not the exemplar's timestamp — see the fn docs.
    let last_updated_sec = lessons_file.last_updated.timestamp() as u64;
    let doc = doc!(
        fields.content => lesson.content.clone(),
        fields.timestamp_sec => last_updated_sec,
        fields.pain => exemplar.pain as f64,
        fields.importance => exemplar.importance as f64,
        fields.recurrence => member_count,
        fields.layer => Layer::Semantic.as_str().to_string(),
        fields.last_updated_sec => last_updated_sec,
        fields.cited_event_count => member_count,
        fields.event_id => semantic_event_id(&lesson.id),
        fields.skill_action => lessons_file.cluster_key.clone(),
        fields.source_harness => SEMANTIC_SOURCE_HARNESS.to_string(),
    );
    writer.add_document(doc).map_err(tantivy_to_index)?;
    Ok(())
}

/// `source_harness` stamped on every lesson document. The dream cycle authored
/// it, not any one harness.
const SEMANTIC_SOURCE_HARNESS: &str = "dreamd";

/// Map an [`AgentLearning`] onto a Tantivy document and add it to the writer.
/// `layer` is always [`Layer::Episodic`] in v0.1; semantic indexing is WEG-136.
/// `event_id` is stored as `STRING | STORED` for targeted delete-and-re-add
/// during recurrence sidecar application (WEG-45 / DR-205′).
fn add_document(
    writer: &mut IndexWriter<TantivyDocument>,
    fields: &SchemaFields,
    learning: &AgentLearning,
    recurrence: u32,
) -> Result<(), IndexError> {
    let ts = learning.timestamp.timestamp() as u64;
    let layer_str = Layer::Episodic.as_str().to_string();
    let id_str = learning.id.as_str().to_string();
    let doc = doc!(
        fields.content => learning.content.clone(),
        fields.timestamp_sec => ts,
        fields.pain => learning.pain as f64,
        fields.importance => learning.importance as f64,
        fields.recurrence => recurrence as u64,
        fields.layer => layer_str,
        fields.last_updated_sec => ts,
        fields.cited_event_count => 0u64,
        fields.event_id => id_str,
        fields.skill_action => learning.skill_action.clone(),
        fields.source_harness => learning.source_harness.clone(),
    );
    writer.add_document(doc).map_err(tantivy_to_index)?;
    Ok(())
}

// Replay (two-pass)

fn read_jsonl_events(jsonl_path: &Path) -> Result<Vec<AgentLearning>, IndexError> {
    crate::episodic::read_all(jsonl_path).map_err(|e| IndexError::Other(format!("read jsonl: {e}")))
}

fn replay_two_pass(
    jsonl_path: &Path,
    last_indexed_id: Option<&str>,
) -> Result<(HashMap<String, u32>, Vec<AgentLearning>), IndexError> {
    // One owned read via the shared episodic scan (WEG-378): pass 1 counts
    // clusters over `&events`, pass 2 filters the same Vec by watermark.
    let events = read_jsonl_events(jsonl_path)?;

    // Pass 1: build final cluster counts by walking every well-formed line.
    let mut clusters: HashMap<String, u32> = HashMap::new();
    for ev in &events {
        *clusters.entry(ev.skill_action.clone()).or_insert(0) += 1;
    }

    // Pass 2: filter to events whose id is strictly greater than the watermark.
    // EventId does not implement `Ord`; compare via `as_str()` (ULIDs are
    // lexicographically sortable in their canonical Crockford base32 form).
    let to_index: Vec<AgentLearning> = events
        .into_iter()
        .filter(|ev| match last_indexed_id {
            Some(last) => ev.id.as_str() > last,
            None => true,
        })
        .collect();

    Ok((clusters, to_index))
}

// Progress + manifest persistence

fn read_progress(path: &Path) -> Result<IndexProgress, IndexError> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| IndexError::Other(format!("parse {INDEX_PROGRESS_FILENAME}: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(IndexProgress::default()),
        Err(e) => Err(IndexError::Other(format!(
            "read {INDEX_PROGRESS_FILENAME}: {e}"
        ))),
    }
}

fn write_progress(path: &Path, progress: &IndexProgress) -> Result<(), IndexError> {
    let bytes = serde_json::to_vec(progress)
        .map_err(|e| IndexError::Other(format!("serialize {INDEX_PROGRESS_FILENAME}: {e}")))?;
    write_atomic(path, &bytes)
        .map_err(|e| IndexError::Other(format!("write {INDEX_PROGRESS_FILENAME}: {e}")))?;
    Ok(())
}

/// Record what the semantic pass could not index, for `dreamd doctor`
/// (AILAB-700).
///
/// Best-effort by design, unlike [`write_progress`]: the watermark is
/// crash-recovery state, this is a diagnostic. A store whose `.dreamd/` is
/// read-only must still open its index and serve recall, so a failure here
/// logs and returns rather than failing the pass — doctor then reports the
/// previous pass, which is a stale hint, not a wrong index.
fn write_semantic_pass_record(agent_root: &AgentRoot, record: &SemanticPassRecord) {
    let dir = agent_root.dreamd_dir();
    let path = dir.join(SEMANTIC_PASS_FILENAME);
    let written = serde_json::to_vec(record)
        .map_err(|e| format!("serialize {SEMANTIC_PASS_FILENAME}: {e}"))
        .and_then(|bytes| {
            std::fs::create_dir_all(&dir)
                .and_then(|()| write_atomic(&path, &bytes))
                .map_err(|e| format!("write {SEMANTIC_PASS_FILENAME}: {e}"))
        });
    if let Err(e) = written {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "could not record the semantic pass report; `dreamd doctor` will \
             report the previous pass until the next one lands"
        );
    }
}

fn write_manifest_if_absent(path: &Path) -> Result<(), IndexError> {
    if path.exists() {
        return Ok(());
    }
    let manifest = IndexManifest::current();
    let bytes = serde_json::to_vec(&manifest)
        .map_err(|e| IndexError::Other(format!("serialize {INDEX_MANIFEST_FILENAME}: {e}")))?;
    write_atomic(path, &bytes)
        .map_err(|e| IndexError::Other(format!("write {INDEX_MANIFEST_FILENAME}: {e}")))?;
    Ok(())
}

/// Open or create the on-disk Tantivy index. If the directory holds a stale
/// schema with no manifest (or a mismatch open_or_create cannot reconcile),
/// wipe the index cache once and retry — same JSONL replay path as the
/// manifest-driven self-heal rebuild, not `dreamd migrate`.
fn open_or_create_index(
    index_dir: &Path,
    schema: tantivy::schema::Schema,
    manifest_path: &Path,
    progress_path: &Path,
) -> Result<Index, IndexError> {
    match try_open_or_create(index_dir, schema.clone()) {
        Ok(index) => Ok(index),
        Err(e) if is_schema_incompatible(&e) => {
            tracing::warn!(
                error = %e,
                "index schema incompatible with binary; rebuilding from JSONL"
            );
            let _ = std::fs::remove_dir_all(index_dir);
            let _ = std::fs::remove_file(manifest_path);
            let _ = std::fs::remove_file(progress_path);
            std::fs::create_dir_all(index_dir).map_err(io_to_index)?;
            try_open_or_create(index_dir, schema)
        }
        Err(e) => Err(e),
    }
}

fn try_open_or_create(
    index_dir: &Path,
    schema: tantivy::schema::Schema,
) -> Result<Index, IndexError> {
    let mmap_dir = MmapDirectory::open(index_dir).map_err(tantivy_io_to_index)?;
    Index::open_or_create(mmap_dir, schema).map_err(tantivy_to_index)
}

/// Does this open failure mean the on-disk index was written under a different
/// schema and must be rebuilt? Gates a `remove_dir_all`, so it is narrowed
/// twice. First by variant: only [`IndexError::Tantivy`] can qualify, which
/// excludes `tantivy_io_to_index`'s [`IndexError::TantivyDirectory`] outright —
/// that payload embeds the *directory path*, so a store living under a path
/// containing "schema" must never reach the substring test. Then by substring:
/// the exact rendering tantivy gives `TantivyError::SchemaError` (`"Schema
/// error: '{0}'"`) rather than a bare `"schema"`, since a `Tantivy` payload may
/// mention a schema without being a schema mismatch.
///
/// Deliberately does not try to catch `TantivyError::IncompatibleIndex` (a
/// tantivy index-*format* mismatch). That variant renders through
/// `Incompatibility`'s hand-written `Debug`, which emits `"Library version: N,
/// index version: M. …"` — containing neither "incompatible" nor "schema". The
/// `|| msg.contains("incompatible")` alternative that used to sit here
/// therefore never matched it and only widened the false-positive surface. A
/// format mismatch stays a loud startup error, not a silent wipe.
fn is_schema_incompatible(err: &IndexError) -> bool {
    match err {
        IndexError::Tantivy(s) => s.to_ascii_lowercase().contains("schema error:"),
        _ => false,
    }
}

// Error helpers

fn io_to_index(e: std::io::Error) -> IndexError {
    IndexError::Io(format!("{e}"))
}

fn tantivy_to_index<E: std::fmt::Display>(e: E) -> IndexError {
    IndexError::Tantivy(format!("{e}"))
}

fn tantivy_io_to_index(e: tantivy::directory::error::OpenDirectoryError) -> IndexError {
    IndexError::TantivyDirectory(format!("{e}"))
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::{MemoryCoordinator, MemoryCoordinatorMsg};
    use crate::server::index_map::{ProjectIndexMap, ProjectIndexMapConfig};
    use crate::test_support::{unique_tmpdir, DirGuard};
    use chrono::{DateTime, Utc};
    use std::io::Write;
    use tantivy::query::AllQuery;
    use tantivy::ReloadPolicy;

    const SAMPLE_ULID_BASE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA";

    fn make_event_id(suffix_char: char) -> EventId {
        // 26-char Crockford ULID, last char varies for ordering tests.
        let raw = format!("evt_{SAMPLE_ULID_BASE}{suffix_char}");
        EventId::parse(&raw).expect("synthesize EventId")
    }

    /// Build a minimal `AgentLearning` with `pain=5, importance=6, pinned=false`.
    /// Fixed neutral values make test score assertions predictable without
    /// coupling tests to the salience formula constants.
    fn sample_learning(id: EventId, skill: &str, content: &str) -> AgentLearning {
        AgentLearning {
            schema_version: "1.0.0".to_string(),
            id,
            timestamp: DateTime::parse_from_rfc3339("2026-05-14T08:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            pain: 5.0,
            importance: 6.0,
            pinned: false,
            skill_action: skill.to_string(),
            source_harness: "test-harness".to_string(),
            content: content.to_string(),
        }
    }

    fn prime_jsonl(dir: &Path, learnings: &[AgentLearning]) -> PathBuf {
        let agent_root = AgentRoot::new(dir);
        let path = agent_root.episodic_jsonl();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut bytes = Vec::new();
        for l in learnings {
            let mut line = serde_json::to_string(l).unwrap();
            line.push('\n');
            bytes.extend_from_slice(line.as_bytes());
        }
        std::fs::write(&path, &bytes).unwrap();
        path
    }

    /// Open a fresh Tantivy reader at the on-disk index dir and pull every
    /// document's `recurrence` value via FastFields. Returns one `u64` per
    /// indexed document, in segment order.
    fn read_recurrence_values(agent_root: &AgentRoot) -> Vec<u64> {
        let index_dir = agent_root.dreamd_dir().join(INDEX_DIR_NAME);
        let index = Index::open_in_dir(&index_dir).expect("open index for read");
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .expect("reader builder");
        reader.reload().expect("reload reader");
        let searcher = reader.searcher();
        let mut out = Vec::new();
        for segment_reader in searcher.segment_readers() {
            let column = segment_reader
                .fast_fields()
                .u64("recurrence")
                .expect("recurrence fast column");
            for doc_id in 0..segment_reader.num_docs() {
                if let Some(v) = column.first(doc_id) {
                    out.push(v);
                }
            }
        }
        out
    }

    fn count_docs(agent_root: &AgentRoot) -> usize {
        let index_dir = agent_root.dreamd_dir().join(INDEX_DIR_NAME);
        let index = Index::open_in_dir(&index_dir).expect("open index for count");
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .expect("reader builder");
        reader.reload().expect("reload reader");
        let searcher = reader.searcher();
        searcher
            .search(&AllQuery, &tantivy::collector::Count)
            .expect("count")
    }

    async fn flush(handle: &TantivyIndexHandle) -> Result<(), IndexError> {
        // Delegates to the promoted `pub(crate)` method (AILAB-162) so tests
        // and the shutdown drain exercise the same flush path.
        handle.flush().await
    }

    // AC-1

    #[tokio::test]
    async fn lock_files_present_post_sigkill_do_not_block_open() {
        let dir = unique_tmpdir("locks");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        let index_dir = agent_root.dreamd_dir().join(INDEX_DIR_NAME);
        std::fs::create_dir_all(&index_dir).unwrap();
        // Prime the directory with plain (non-flocked) lock-named files.
        std::fs::write(index_dir.join(".tantivy-writer.lock"), b"").unwrap();
        std::fs::write(index_dir.join(".tantivy-meta.lock"), b"").unwrap();

        let handle = TantivyIndexHandle::open(&agent_root, DEFAULT_COMMIT_CADENCE)
            .expect("open succeeds despite leftover lock files");
        handle.shutdown().await.expect("shutdown");

        // Files were NOT unlinked by open.
        assert!(index_dir.join(".tantivy-writer.lock").exists());
        assert!(index_dir.join(".tantivy-meta.lock").exists());
    }

    // AC-2 — replay two-pass + AC-2 watermark gating

    #[tokio::test]
    async fn replay_two_pass_assigns_final_cluster_count() {
        let dir = unique_tmpdir("replay-cluster");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);

        // 3 rust.test events interleaved with 1 python.pytest event.
        let learnings = vec![
            sample_learning(make_event_id('0'), "rust.test", "first rust"),
            sample_learning(make_event_id('1'), "python.pytest", "py one"),
            sample_learning(make_event_id('2'), "rust.test", "second rust"),
            sample_learning(make_event_id('3'), "rust.test", "third rust"),
        ];
        prime_jsonl(&dir, &learnings);

        let handle = TantivyIndexHandle::open(&agent_root, DEFAULT_COMMIT_CADENCE).expect("open");
        handle.shutdown().await.expect("shutdown");

        let values = read_recurrence_values(&agent_root);
        assert_eq!(values.len(), 4, "all four events indexed");
        // Three values are 3 (the rust.test cluster), one is 1 (python.pytest).
        let threes = values.iter().filter(|&&v| v == 3).count();
        let ones = values.iter().filter(|&&v| v == 1).count();
        assert_eq!(threes, 3, "rust cluster docs each carry final count 3");
        assert_eq!(ones, 1, "lone python.pytest doc carries count 1");
    }

    #[tokio::test]
    async fn replay_two_pass_skips_events_at_or_below_last_indexed_id() {
        let dir = unique_tmpdir("replay-watermark");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);

        let id_a = make_event_id('A');
        let id_b = make_event_id('B');
        let id_c = make_event_id('C');
        let learnings = vec![
            sample_learning(id_a.clone(), "rust.test", "alpha"),
            sample_learning(id_b.clone(), "rust.test", "beta"),
            sample_learning(id_c.clone(), "rust.test", "gamma"),
        ];
        prime_jsonl(&dir, &learnings);

        // Pre-seed the watermark at B so only C should be (re-)indexed.
        let dreamd_dir = agent_root.dreamd_dir();
        std::fs::create_dir_all(&dreamd_dir).unwrap();
        let progress = IndexProgress {
            last_indexed_id: Some(id_b.as_str().to_owned()),
        };
        write_progress(&dreamd_dir.join(INDEX_PROGRESS_FILENAME), &progress).unwrap();

        let handle = TantivyIndexHandle::open(&agent_root, DEFAULT_COMMIT_CADENCE).expect("open");
        handle.shutdown().await.expect("shutdown");

        let values = read_recurrence_values(&agent_root);
        // Only the C event is indexed — its recurrence is 3 (cluster count
        // from pass-1 over the entire JSONL).
        assert_eq!(values, vec![3]);
    }

    // AC-3 — steady-state recurrence + bounded-staleness

    #[tokio::test]
    async fn steady_state_increments_counter_per_skill_action() {
        let dir = unique_tmpdir("steady-counter");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        // No JSONL primed — start clean.
        std::fs::create_dir_all(agent_root.episodic_dir()).unwrap();

        let handle = TantivyIndexHandle::open(&agent_root, Duration::from_secs(60)).expect("open");
        let tx = handle.sender();

        for c in ['0', '1', '2'] {
            let id = make_event_id(c);
            let learning = sample_learning(id.clone(), "rust.test", "body");
            tx.send(IndexerMsg::Append {
                event_id: id,
                learning,
            })
            .await
            .expect("send append");
        }
        flush(&handle).await.expect("flush ok");
        drop(tx);
        handle.shutdown().await.expect("shutdown");

        let values = read_recurrence_values(&agent_root);
        let mut sorted = values.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            vec![1, 2, 3],
            "steady-state appends carry incrementing recurrence"
        );
    }

    #[tokio::test]
    async fn old_docs_not_rewritten_on_new_append_in_cluster() {
        let dir = unique_tmpdir("staleness");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        std::fs::create_dir_all(agent_root.episodic_dir()).unwrap();

        let handle = TantivyIndexHandle::open(&agent_root, Duration::from_secs(60)).expect("open");
        let tx = handle.sender();

        // First append: doc gets recurrence=1.
        let id1 = make_event_id('A');
        tx.send(IndexerMsg::Append {
            event_id: id1.clone(),
            learning: sample_learning(id1, "rust.test", "first"),
        })
        .await
        .unwrap();
        flush(&handle).await.expect("flush 1");

        // Second append: doc gets recurrence=2. First doc must NOT be
        // rewritten to recurrence=2.
        let id2 = make_event_id('B');
        tx.send(IndexerMsg::Append {
            event_id: id2.clone(),
            learning: sample_learning(id2, "rust.test", "second"),
        })
        .await
        .unwrap();
        flush(&handle).await.expect("flush 2");
        drop(tx);
        handle.shutdown().await.expect("shutdown");

        let mut values = read_recurrence_values(&agent_root);
        values.sort_unstable();
        assert_eq!(
            values,
            vec![1, 2],
            "older doc keeps its index-time recurrence (bounded staleness)"
        );
    }

    // AC-5 — progress file written after commit only

    #[tokio::test]
    async fn progress_file_written_after_commit_only() {
        let dir = unique_tmpdir("progress-after");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        std::fs::create_dir_all(agent_root.episodic_dir()).unwrap();

        let progress_path = agent_root.dreamd_dir().join(INDEX_PROGRESS_FILENAME);

        // Long cadence so the cadence ticker won't fire during the test.
        let handle = TantivyIndexHandle::open(&agent_root, Duration::from_secs(60)).expect("open");
        // Empty JSONL, no replay events committed → no progress file yet.
        assert!(
            !progress_path.exists(),
            "no progress file before any commit"
        );

        let id = make_event_id('0');
        handle
            .sender()
            .send(IndexerMsg::Append {
                event_id: id.clone(),
                learning: sample_learning(id, "rust.test", "x"),
            })
            .await
            .unwrap();
        // Still no commit — the file should not exist yet.
        assert!(
            !progress_path.exists(),
            "no progress file before flush/commit"
        );

        flush(&handle).await.expect("flush");
        assert!(
            progress_path.exists(),
            "progress file appears after successful commit"
        );

        handle.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn progress_file_records_last_indexed_id() {
        let dir = unique_tmpdir("progress-record");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        std::fs::create_dir_all(agent_root.episodic_dir()).unwrap();

        let handle = TantivyIndexHandle::open(&agent_root, Duration::from_secs(60)).expect("open");
        let id = make_event_id('Z');
        handle
            .sender()
            .send(IndexerMsg::Append {
                event_id: id.clone(),
                learning: sample_learning(id.clone(), "rust.test", "body"),
            })
            .await
            .unwrap();
        flush(&handle).await.expect("flush");
        handle.shutdown().await.expect("shutdown");

        let bytes = std::fs::read(agent_root.dreamd_dir().join(INDEX_PROGRESS_FILENAME))
            .expect("progress file");
        let progress: IndexProgress = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(progress.last_indexed_id.as_deref(), Some(id.as_str()));
    }

    // AC-7 — manifest written on first open only

    #[tokio::test]
    async fn manifest_written_on_first_open_only() {
        let dir = unique_tmpdir("manifest-once");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        std::fs::create_dir_all(agent_root.episodic_dir()).unwrap();
        let manifest_path = agent_root.dreamd_dir().join(INDEX_MANIFEST_FILENAME);

        let h1 =
            TantivyIndexHandle::open(&agent_root, Duration::from_secs(60)).expect("first open");
        h1.shutdown().await.expect("shutdown 1");
        let mtime1 = std::fs::metadata(&manifest_path)
            .expect("manifest present after first open")
            .modified()
            .expect("modified time");

        // Sleep enough that any rewrite would have a distinguishably-newer mtime.
        std::thread::sleep(Duration::from_millis(50));
        let h2 =
            TantivyIndexHandle::open(&agent_root, Duration::from_secs(60)).expect("second open");
        h2.shutdown().await.expect("shutdown 2");
        let mtime2 = std::fs::metadata(&manifest_path)
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(mtime1, mtime2, "manifest must not be rewritten on re-open");
    }

    // AC-9 — commit cadence drives a tick (short cadence)

    #[tokio::test]
    async fn commit_cadence_constructor_arg_drives_tick() {
        let dir = unique_tmpdir("cadence");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        std::fs::create_dir_all(agent_root.episodic_dir()).unwrap();
        let progress_path = agent_root.dreamd_dir().join(INDEX_PROGRESS_FILENAME);

        let handle =
            TantivyIndexHandle::open(&agent_root, Duration::from_millis(50)).expect("open");
        let id = make_event_id('Q');
        handle
            .sender()
            .send(IndexerMsg::Append {
                event_id: id.clone(),
                learning: sample_learning(id, "rust.test", "tick"),
            })
            .await
            .unwrap();

        // Wait until the cadence tick fires and the progress file appears.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if progress_path.exists() {
                break;
            }
            if Instant::now() >= deadline {
                panic!("cadence tick did not produce a commit within 5s");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        handle.shutdown().await.expect("shutdown");
    }

    // AC-16 — replay skips mid-file blank/corrupt lines (SPEC §88 / WEG-427)

    #[tokio::test]
    async fn replay_skips_blank_midfile_line_in_jsonl() {
        let dir = unique_tmpdir("replay-blank");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        let jsonl_path = agent_root.episodic_jsonl();
        std::fs::create_dir_all(jsonl_path.parent().unwrap()).unwrap();

        let good1 = sample_learning(make_event_id('0'), "rust.test", "first");
        let good2 = sample_learning(make_event_id('1'), "rust.test", "after blank");
        let mut bytes = serde_json::to_string(&good1).unwrap();
        bytes.push('\n');
        bytes.push('\n'); // mid-file blank — skipped
        let mut tail = serde_json::to_string(&good2).unwrap();
        tail.push('\n');
        bytes.push_str(&tail);
        std::fs::write(&jsonl_path, bytes.as_bytes()).unwrap();

        let handle = TantivyIndexHandle::open(&agent_root, Duration::from_secs(60)).expect("open");
        handle.shutdown().await.expect("shutdown");

        assert_eq!(
            count_docs(&agent_root),
            2,
            "both valid records indexed; mid-file blank skipped"
        );
    }

    #[tokio::test]
    async fn replay_skips_unparseable_midfile_record() {
        let dir = unique_tmpdir("replay-bad");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        let jsonl_path = agent_root.episodic_jsonl();
        std::fs::create_dir_all(jsonl_path.parent().unwrap()).unwrap();

        let good1 = sample_learning(make_event_id('0'), "rust.test", "first");
        let mut bytes = serde_json::to_string(&good1).unwrap();
        bytes.push('\n');
        bytes.push_str("{not json}\n"); // unparseable mid-file — skipped
        let good2 = sample_learning(make_event_id('1'), "rust.test", "after bad");
        bytes.push_str(&serde_json::to_string(&good2).unwrap());
        bytes.push('\n');
        std::fs::write(&jsonl_path, bytes.as_bytes()).unwrap();

        let handle = TantivyIndexHandle::open(&agent_root, Duration::from_secs(60)).expect("open");
        handle.shutdown().await.expect("shutdown");

        assert_eq!(
            count_docs(&agent_root),
            2,
            "both valid records indexed; mid-file corrupt line skipped"
        );
    }

    // AC-23 — coordinator routes appends to indexer sender

    #[tokio::test]
    async fn coordinator_routes_appends_to_indexer_sender() {
        let dir = unique_tmpdir("coord-routes");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        std::fs::create_dir_all(agent_root.episodic_dir()).unwrap();

        let (idx_tx, mut idx_rx) = mpsc::channel::<IndexerMsg>(8);
        let (coord_tx, coord_rx) = mpsc::channel::<MemoryCoordinatorMsg>(8);
        let coord = MemoryCoordinator::open_at(
            &agent_root.episodic_jsonl(),
            agent_root.project_root(),
            coord_rx,
            Some(idx_tx),
        )
        .expect("open coord");
        let coord_handle = tokio::spawn(coord.run());

        let (resp_tx, resp_rx) = oneshot::channel();
        coord_tx
            .send(MemoryCoordinatorMsg::AppendLearning {
                learning: sample_learning(make_event_id('0'), "rust.test", "routed"),
                client_dedup_key: None,
                response_tx: resp_tx,
            })
            .await
            .expect("send append");
        let _ = resp_rx.await.expect("recv").expect("append ok");

        // Exactly one IndexerMsg::Append should have been delivered.
        let msg = tokio::time::timeout(Duration::from_secs(2), idx_rx.recv())
            .await
            .expect("indexer msg in time")
            .expect("some msg");
        match msg {
            IndexerMsg::Append { .. } => {}
            _ => panic!("expected Append"),
        }

        // Cleanly shut down the coordinator.
        let (sh_tx, sh_rx) = oneshot::channel();
        coord_tx
            .send(MemoryCoordinatorMsg::Shutdown { response_tx: sh_tx })
            .await
            .unwrap();
        sh_rx.await.unwrap();
        coord_handle.await.unwrap();
    }

    #[tokio::test]
    async fn coordinator_continues_when_indexer_channel_full() {
        let dir = unique_tmpdir("coord-full");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        std::fs::create_dir_all(agent_root.episodic_dir()).unwrap();

        // Capacity 1; pre-fill it so the next try_send returns Full.
        let (idx_tx, _idx_rx) = mpsc::channel::<IndexerMsg>(1);
        idx_tx
            .send(IndexerMsg::Append {
                event_id: make_event_id('Z'),
                learning: sample_learning(make_event_id('Z'), "rust.test", "filler"),
            })
            .await
            .unwrap();

        let (coord_tx, coord_rx) = mpsc::channel::<MemoryCoordinatorMsg>(8);
        let coord = MemoryCoordinator::open_at(
            &agent_root.episodic_jsonl(),
            agent_root.project_root(),
            coord_rx,
            Some(idx_tx),
        )
        .expect("open coord");
        let coord_handle = tokio::spawn(coord.run());

        let (resp_tx, resp_rx) = oneshot::channel();
        coord_tx
            .send(MemoryCoordinatorMsg::AppendLearning {
                learning: sample_learning(make_event_id('0'), "rust.test", "should still succeed"),
                client_dedup_key: None,
                response_tx: resp_tx,
            })
            .await
            .expect("send append");

        let minted = resp_rx
            .await
            .expect("recv resp")
            .expect("append must succeed even when indexer channel is full")
            .id;
        assert!(minted.as_str().starts_with("evt_"));

        let (sh_tx, sh_rx) = oneshot::channel();
        coord_tx
            .send(MemoryCoordinatorMsg::Shutdown { response_tx: sh_tx })
            .await
            .unwrap();
        sh_rx.await.unwrap();
        coord_handle.await.unwrap();
    }

    #[tokio::test]
    async fn idempotency_hit_does_not_emit_indexer_append() {
        let dir = unique_tmpdir("coord-dedup");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        std::fs::create_dir_all(agent_root.episodic_dir()).unwrap();

        let (idx_tx, mut idx_rx) = mpsc::channel::<IndexerMsg>(8);
        let (coord_tx, coord_rx) = mpsc::channel::<MemoryCoordinatorMsg>(8);
        let coord = MemoryCoordinator::open_at(
            &agent_root.episodic_jsonl(),
            agent_root.project_root(),
            coord_rx,
            Some(idx_tx),
        )
        .expect("open coord");
        let coord_handle = tokio::spawn(coord.run());

        // First append with a dedup key.
        let dedup = Some("req-1".to_string());
        let (r1_tx, r1_rx) = oneshot::channel();
        coord_tx
            .send(MemoryCoordinatorMsg::AppendLearning {
                learning: sample_learning(make_event_id('0'), "rust.test", "first"),
                client_dedup_key: dedup.clone(),
                response_tx: r1_tx,
            })
            .await
            .unwrap();
        let id1 = r1_rx.await.unwrap().unwrap().id;

        // Second append with same key — idempotency hit, no new write.
        let (r2_tx, r2_rx) = oneshot::channel();
        coord_tx
            .send(MemoryCoordinatorMsg::AppendLearning {
                learning: sample_learning(make_event_id('1'), "rust.test", "second"),
                client_dedup_key: dedup,
                response_tx: r2_tx,
            })
            .await
            .unwrap();
        let id2 = r2_rx.await.unwrap().unwrap().id;
        assert_eq!(id1, id2, "dedup hit returns cached id");

        // Drain indexer rx and assert exactly one IndexerMsg::Append.
        let first = tokio::time::timeout(Duration::from_secs(2), idx_rx.recv())
            .await
            .expect("first msg in time")
            .expect("some msg");
        assert!(matches!(first, IndexerMsg::Append { .. }));
        let none = tokio::time::timeout(Duration::from_millis(200), idx_rx.recv()).await;
        assert!(
            none.is_err(),
            "no second IndexerMsg should be emitted on dedup hit"
        );

        let (sh_tx, sh_rx) = oneshot::channel();
        coord_tx
            .send(MemoryCoordinatorMsg::Shutdown { response_tx: sh_tx })
            .await
            .unwrap();
        sh_rx.await.unwrap();
        coord_handle.await.unwrap();
    }

    // AC-24 — shutdown flushes final batch to disk

    #[tokio::test]
    async fn shutdown_flushes_final_batch_to_disk() {
        let dir = unique_tmpdir("shutdown-flush");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        std::fs::create_dir_all(agent_root.episodic_dir()).unwrap();

        // 60s cadence — only the shutdown-drain path can produce the commit.
        let handle = TantivyIndexHandle::open(&agent_root, Duration::from_secs(60)).expect("open");
        let tx = handle.sender();
        for c in ['0', '1', '2'] {
            let id = make_event_id(c);
            tx.send(IndexerMsg::Append {
                event_id: id.clone(),
                learning: sample_learning(id, "rust.test", "body"),
            })
            .await
            .unwrap();
        }
        drop(tx);
        handle.shutdown().await.expect("shutdown");

        assert_eq!(
            count_docs(&agent_root),
            3,
            "drained appends must land on disk before indexer task exits"
        );
    }

    // WEG-69 — reader() accessor

    #[tokio::test]
    async fn tantivy_handle_reader_is_accessible() {
        let dir = unique_tmpdir("reader-access");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        std::fs::create_dir_all(agent_root.episodic_dir()).unwrap();

        let handle =
            TantivyIndexHandle::open(&agent_root, DEFAULT_COMMIT_CADENCE).expect("open handle");
        // Confirm reader() returns without panic and produces a usable searcher.
        let _searcher = handle.reader().searcher();
        handle.shutdown().await.expect("shutdown");
    }

    // WEG-264 Defect 1 — fresh handle's reader reflects the open-time replay

    /// Regression: a process that opens a `TantivyIndexHandle` and is the
    /// first to index a JSONL record must see that record on its *first* query
    /// through `reader()`. Before the WEG-264 fix the reader was created
    /// pre-commit with `ReloadPolicy::OnCommitWithDelay` and never reloaded on
    /// the production path, so the open-time replay commit was invisible until
    /// some later process re-opened the index. This reproduces the
    /// cross-harness demo where agent #2 is the first process to index agent
    /// #1's append.
    #[tokio::test]
    async fn fresh_handle_reader_sees_replayed_events_without_reload() {
        let dir = unique_tmpdir("fresh-reader-replay");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);

        // Simulate a prior process's durable append already on disk.
        prime_jsonl(
            &dir,
            &[sample_learning(
                make_event_id('0'),
                "rust.build.zlorp-aarch64",
                "pass --features ring-prebuilt on aarch64",
            )],
        );

        // A *new* process opens the handle and queries immediately — no
        // explicit flush, no manual reader.reload().
        let handle = TantivyIndexHandle::open(&agent_root, DEFAULT_COMMIT_CADENCE).expect("open");
        let count = handle
            .reader()
            .searcher()
            .search(&AllQuery, &tantivy::collector::Count)
            .expect("count");
        assert_eq!(
            count, 1,
            "freshly opened reader must reflect the open-time replay commit"
        );

        handle.shutdown().await.expect("shutdown");
    }

    // WEG-45 — apply_recurrence_sidecar (delete-and-re-add)

    /// Verify that `apply_recurrence_sidecar` updates the `recurrence` FastField
    /// for all documents in a cluster from their original values to the sidecar's
    /// authoritative count.
    ///
    /// Setup:
    ///   3 "deploy" events with recurrence 1, 2, 3 from the steady-state counter.
    /// After sidecar with count=42:
    ///   All 3 events should carry recurrence=42 (old docs deleted, new added).
    #[tokio::test]
    async fn apply_recurrence_sidecar_updates_fastfield() {
        use crate::index::RecurrenceSidecar;

        let dir = unique_tmpdir("sidecar");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);

        // No JSONL primed — start clean, then append via steady-state path.
        std::fs::create_dir_all(agent_root.episodic_dir()).unwrap();
        // The sidecar path is under semantic/; create the dir explicitly.
        std::fs::create_dir_all(agent_root.semantic_dir()).unwrap();

        let handle = TantivyIndexHandle::open(&agent_root, Duration::from_secs(60)).expect("open");
        let tx = handle.sender();

        // Append 3 "deploy" events — steady-state counter gives recurrence 1, 2, 3.
        // We also write them to the JSONL so the sidecar handler can re-parse.
        let mut learnings = Vec::new();
        for c in ['0', '1', '2'] {
            let id = make_event_id(c);
            let learning = sample_learning(id.clone(), "deploy", "deploy body");
            learnings.push(learning.clone());
            tx.send(IndexerMsg::Append {
                event_id: id,
                learning,
            })
            .await
            .expect("send append");
        }
        flush(&handle).await.expect("flush after appends");

        // Write the JSONL so apply_recurrence_sidecar_inner can re-read events.
        prime_jsonl(&dir, &learnings);

        // Verify pre-sidecar recurrence values are {1, 2, 3}.
        let values_before = read_recurrence_values(&agent_root);
        let mut sorted_before = values_before.clone();
        sorted_before.sort_unstable();
        assert_eq!(
            sorted_before,
            vec![1, 2, 3],
            "pre-sidecar: steady-state counter produces 1,2,3"
        );

        // Write the sidecar: count=42 for "deploy".
        let sidecar = RecurrenceSidecar {
            schema_version: "1.0".to_string(),
            clusters: vec![crate::index::ClusterCount {
                skill_action: "deploy".to_string(),
                count: 42,
            }],
        };
        let sidecar_json = serde_json::to_string(&sidecar).unwrap();
        let sidecar_path = agent_root.semantic_dir().join("recurrence_counts.json");
        std::fs::write(&sidecar_path, sidecar_json.as_bytes()).unwrap();

        // Apply the sidecar.
        handle
            .apply_recurrence_sidecar(&agent_root)
            .await
            .expect("apply_recurrence_sidecar");

        drop(tx);
        handle.shutdown().await.expect("shutdown");

        // Read recurrence values after: Tantivy delete-and-re-add leaves the
        // old (soft-deleted) docs in their segments AND adds new docs. The
        // `read_recurrence_values` helper walks all segment readers and returns
        // ALL stored FastField values — including soft-deleted rows. After the
        // sidecar commit, we expect 3 new docs with recurrence=42. The old
        // soft-deleted docs still occupy slots with values 1, 2, or 3 until
        // compaction. We assert that 42 appears at least 3 times (the new
        // docs) and that no value other than 1, 2, 3, or 42 is present.
        let values_after = read_recurrence_values(&agent_root);
        let count_42 = values_after.iter().filter(|&&v| v == 42).count();
        assert!(
            count_42 >= 3,
            "expected at least 3 docs with recurrence=42 after sidecar, got: {values_after:?}"
        );
        assert!(
            values_after.iter().all(|&v| matches!(v, 1 | 2 | 3 | 42)),
            "unexpected recurrence values after sidecar: {values_after:?}"
        );
    }

    // WEG-62 — prune_decayed_events removes docs from index

    #[tokio::test]
    async fn prune_decayed_events_removes_docs_from_index() {
        let dir = unique_tmpdir("prune-decay");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        std::fs::create_dir_all(agent_root.episodic_dir()).unwrap();

        let handle = TantivyIndexHandle::open(&agent_root, Duration::from_secs(60)).expect("open");
        let tx = handle.sender();

        // Append 5 events.
        let ids: Vec<EventId> = vec!['0', '1', '2', '3', '4']
            .into_iter()
            .map(make_event_id)
            .collect();
        for id in &ids {
            let learning = sample_learning(id.clone(), "rust.test", "body");
            tx.send(IndexerMsg::Append {
                event_id: id.clone(),
                learning,
            })
            .await
            .expect("send append");
        }
        flush(&handle).await.expect("flush");

        // Reload reader and confirm 5 docs.
        handle.reader().reload().expect("reload reader");
        assert_eq!(
            handle
                .reader()
                .searcher()
                .search(&AllQuery, &tantivy::collector::Count)
                .expect("count"),
            5,
            "5 docs after append"
        );

        // Prune 3 of the 5 events.
        let to_prune = ids[..3].to_vec();
        handle
            .prune_decayed_events(to_prune)
            .await
            .expect("prune_decayed_events");

        // Reload reader and confirm 2 docs remain.
        handle.reader().reload().expect("reload after prune");
        assert_eq!(
            handle
                .reader()
                .searcher()
                .search(&AllQuery, &tantivy::collector::Count)
                .expect("count after prune"),
            2,
            "2 docs remain after pruning 3"
        );

        drop(tx);
        handle.shutdown().await.expect("shutdown");
    }

    // Schema migration — self-healing rebuild surfaces provenance anchors

    #[tokio::test]
    async fn schema_migration_rebuilds_index_and_surfaces_anchors() {
        use crate::index::{IndexManifest, SCHEMA_VERSION};

        let dir = unique_tmpdir("schema-migrate");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);

        let id = make_event_id('M');
        let learning = AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: id.clone(),
            timestamp: DateTime::parse_from_rfc3339("2026-05-14T08:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            pain: 7.0,
            importance: 8.0,
            pinned: false,
            skill_action: "rust::migration::anchor_test".to_string(),
            source_harness: "cursor".to_string(),
            content: "migration anchor marker token".to_string(),
        };
        prime_jsonl(&dir, &[learning]);

        let dreamd_dir = agent_root.dreamd_dir();
        std::fs::create_dir_all(&dreamd_dir).unwrap();
        std::fs::write(
            dreamd_dir.join(INDEX_MANIFEST_FILENAME),
            r#"{"schema_version":"index/1.2"}"#,
        )
        .unwrap();

        let handle = TantivyIndexHandle::open(&agent_root, DEFAULT_COMMIT_CADENCE)
            .expect("open after migration");

        let (_, fields) = build_schema();
        let now_sec = chrono::Utc::now().timestamp();
        let results = crate::recall(handle.reader(), &fields, "anchor marker", 5, None, now_sec)
            .expect("recall after migration rebuild");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].skill_action, "rust::migration::anchor_test");
        assert_eq!(results[0].source_harness, "cursor");

        let manifest_text =
            std::fs::read_to_string(dreamd_dir.join(INDEX_MANIFEST_FILENAME)).expect("manifest");
        let manifest: IndexManifest = serde_json::from_str(&manifest_text).expect("parse manifest");
        assert_eq!(manifest.schema_version, SCHEMA_VERSION);

        handle.shutdown().await.expect("shutdown");
    }

    // AC-11 — trait conformance + ProjectIndexMap integration

    #[test]
    fn tantivy_handle_satisfies_index_handle_trait() {
        fn assert_index_handle<H: IndexHandle>() {}
        assert_index_handle::<TantivyIndexHandle>();
    }

    #[tokio::test]
    async fn tantivy_handle_works_inside_project_index_map() {
        let dir_a = unique_tmpdir("map-a");
        let _ga = DirGuard(dir_a.clone());
        let dir_b = unique_tmpdir("map-b");
        let _gb = DirGuard(dir_b.clone());
        let root_a = AgentRoot::new(&dir_a);
        let root_b = AgentRoot::new(&dir_b);
        std::fs::create_dir_all(root_a.episodic_dir()).unwrap();
        std::fs::create_dir_all(root_b.episodic_dir()).unwrap();

        let mut map: ProjectIndexMap<TantivyIndexHandle> =
            ProjectIndexMap::new(ProjectIndexMapConfig {
                capacity: 2,
                idle_timeout: Duration::from_secs(3600),
            });

        let h_a_present = {
            let h = map
                .get_or_open(root_a.project_root(), |_| {
                    TantivyIndexHandle::open(&root_a, Duration::from_secs(60))
                })
                .expect("open A");
            h.touch();
            true
        };
        assert!(h_a_present);

        let _ = map
            .get_or_open(root_b.project_root(), |_| {
                TantivyIndexHandle::open(&root_b, Duration::from_secs(60))
            })
            .expect("open B");
        assert_eq!(map.len(), 2, "below capacity, no eviction");

        // close_all drives `IndexHandle::close` on each handle. Inside the
        // runtime this aborts indexer tasks (see close() docs); we are
        // verifying that `ProjectIndexMap<H>` composes with the concrete
        // handle type, not the drain guarantee.
        map.close_all();
        assert_eq!(map.len(), 0);
    }

    // v0.1 index-vs-JSONL contract — assess + replay healing

    #[test]
    fn assess_index_freshness_ok_when_watermark_matches_tail() {
        let dir = unique_tmpdir("fresh-ok");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        let id = make_event_id('A');
        prime_jsonl(&dir, &[sample_learning(id.clone(), "rust.test", "one")]);
        let progress_path = agent_root.dreamd_dir().join(INDEX_PROGRESS_FILENAME);
        std::fs::create_dir_all(agent_root.dreamd_dir()).unwrap();
        write_progress(
            &progress_path,
            &IndexProgress {
                last_indexed_id: Some(id.as_str().to_owned()),
            },
        )
        .unwrap();

        let report = assess_index_freshness(&agent_root).expect("assess");
        assert!(!report.stale, "watermark at tail must be fresh: {report:?}");
        assert_eq!(report.unindexed_count, 0);
    }

    #[test]
    fn assess_index_freshness_stale_when_jsonl_ahead_of_watermark() {
        let dir = unique_tmpdir("fresh-stale");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        let id_a = make_event_id('A');
        let id_b = make_event_id('B');
        prime_jsonl(
            &dir,
            &[
                sample_learning(id_a.clone(), "rust.test", "older"),
                sample_learning(id_b.clone(), "rust.test", "newer"),
            ],
        );
        let progress_path = agent_root.dreamd_dir().join(INDEX_PROGRESS_FILENAME);
        std::fs::create_dir_all(agent_root.dreamd_dir()).unwrap();
        write_progress(
            &progress_path,
            &IndexProgress {
                last_indexed_id: Some(id_a.as_str().to_owned()),
            },
        )
        .unwrap();

        let report = assess_index_freshness(&agent_root).expect("assess");
        assert!(report.stale, "jsonl tail ahead of watermark: {report:?}");
        assert_eq!(report.unindexed_count, 1);
        assert_eq!(report.jsonl_tail_id.as_deref(), Some(id_b.as_str()));
    }

    #[tokio::test]
    async fn startup_replay_heals_jsonl_index_divergence() {
        let dir = unique_tmpdir("replay-heal");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        let id_a = make_event_id('A');
        let id_b = make_event_id('B');

        // Index event A through the normal open path.
        prime_jsonl(&dir, &[sample_learning(id_a.clone(), "rust.test", "first")]);
        let handle =
            TantivyIndexHandle::open(&agent_root, Duration::from_secs(60)).expect("open baseline");
        handle.shutdown().await.expect("shutdown baseline");

        // Simulate crash after JSONL sync_data but before indexer caught up:
        // append B directly to the episodic log while watermark still at A.
        let jsonl_path = agent_root.episodic_jsonl();
        let mut line =
            serde_json::to_string(&sample_learning(id_b.clone(), "rust.test", "second")).unwrap();
        line.push('\n');
        std::fs::OpenOptions::new()
            .append(true)
            .open(&jsonl_path)
            .unwrap()
            .write_all(line.as_bytes())
            .unwrap();

        let before = assess_index_freshness(&agent_root).expect("assess before");
        assert!(before.stale, "pre-replay must be stale: {before:?}");
        assert_eq!(before.unindexed_count, 1);

        let handle2 =
            TantivyIndexHandle::open(&agent_root, Duration::from_secs(60)).expect("open replay");
        handle2.shutdown().await.expect("shutdown replay");

        let after = assess_index_freshness(&agent_root).expect("assess after");
        assert!(!after.stale, "post-replay must be fresh: {after:?}");
        assert_eq!(
            count_docs(&agent_root),
            2,
            "A from first open + B from replay"
        );
    }

    #[tokio::test]
    async fn channel_saturation_stale_recall_until_restart_replay() {
        let dir = unique_tmpdir("chan-full-recall");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        std::fs::create_dir_all(agent_root.episodic_dir()).unwrap();

        let handle =
            TantivyIndexHandle::open(&agent_root, Duration::from_secs(60)).expect("open index");
        let real_tx = handle.sender();

        // Capacity 1, pre-filled — coordinator try_send will drop the next Append.
        let (idx_tx, _idx_rx) = mpsc::channel::<IndexerMsg>(1);
        idx_tx
            .send(IndexerMsg::Append {
                event_id: make_event_id('Z'),
                learning: sample_learning(make_event_id('Z'), "rust.test", "filler"),
            })
            .await
            .unwrap();

        let (coord_tx, coord_rx) = mpsc::channel::<MemoryCoordinatorMsg>(8);
        let coord = MemoryCoordinator::open_at(
            &agent_root.episodic_jsonl(),
            agent_root.project_root(),
            coord_rx,
            Some(idx_tx),
        )
        .expect("open coord");
        let coord_handle = tokio::spawn(coord.run());

        let unique = "channel_saturation_marker_token";
        let (resp_tx, resp_rx) = oneshot::channel();
        coord_tx
            .send(MemoryCoordinatorMsg::AppendLearning {
                learning: sample_learning(make_event_id('0'), "rust.test", unique),
                client_dedup_key: None,
                response_tx: resp_tx,
            })
            .await
            .expect("send append");
        resp_rx
            .await
            .expect("recv")
            .expect("append must succeed when indexer channel is full");

        let (sh_tx, sh_rx) = oneshot::channel();
        coord_tx
            .send(MemoryCoordinatorMsg::Shutdown { response_tx: sh_tx })
            .await
            .unwrap();
        sh_rx.await.unwrap();
        coord_handle.await.unwrap();

        let stale = assess_index_freshness(&agent_root).expect("assess");
        assert!(stale.stale, "dropped indexer msg must leave index stale");

        let (_, fields) = build_schema();
        let now_sec = chrono::Utc::now().timestamp();
        let misses = crate::recall(handle.reader(), &fields, unique, 5, None, now_sec)
            .expect("recall before replay");
        assert!(
            misses.is_empty(),
            "recall must miss unindexed append after channel saturation"
        );

        drop(real_tx);
        handle.shutdown().await.expect("shutdown first handle");

        let handle2 =
            TantivyIndexHandle::open(&agent_root, Duration::from_secs(60)).expect("reopen replay");
        handle2.reader().reload().expect("reload after replay");
        let hits = crate::recall(handle2.reader(), &fields, unique, 5, None, now_sec)
            .expect("recall after replay");
        assert!(
            !hits.is_empty(),
            "startup replay must make saturated append searchable"
        );

        let fresh = assess_index_freshness(&agent_root).expect("assess after");
        assert!(!fresh.stale, "replay must heal freshness: {fresh:?}");

        handle2.shutdown().await.expect("shutdown");
    }

    /// `is_schema_incompatible` gates a `remove_dir_all` of the index cache, so
    /// it must fire on a real tantivy schema mismatch and on nothing else.
    /// The false-positive cases below are the reason it matches
    /// `"schema error:"` rather than a bare `"schema"` substring.
    #[test]
    fn schema_incompat_matches_tantivy_schema_error_only() {
        // The real trigger: tantivy renders SchemaError as "Schema error: '..'",
        // wrapped by `tantivy_to_index`.
        let schema_err = tantivy_to_index(tantivy::TantivyError::SchemaError(
            "field 'skill_action' not found".to_string(),
        ));
        assert!(
            is_schema_incompatible(&schema_err),
            "a tantivy SchemaError must trigger the rebuild: {schema_err:?}"
        );

        // A store whose path merely contains "schema" must NOT wipe the index.
        // `tantivy_io_to_index` embeds the directory path in its message.
        let path_err = IndexError::TantivyDirectory(
            "Failed to open the directory: \
             '/home/dev/schema-tools/.agent/.dreamd/index'"
                .to_string(),
        );
        assert!(
            !is_schema_incompatible(&path_err),
            "a path containing 'schema' must not trigger a wipe: {path_err:?}"
        );

        // A tantivy index-FORMAT mismatch renders through Incompatibility's
        // Debug and contains neither "incompatible" nor "schema error:". It
        // stays a loud error rather than a silent rebuild.
        let format_err = IndexError::Tantivy(
            "Library version: 6, index version: 5. Change tantivy to a \
             version compatible with index format 5 (e.g. 0.21.x) and rebuild \
             your project."
                .to_string(),
        );
        assert!(
            !is_schema_incompatible(&format_err),
            "an index-format mismatch must not be treated as a schema rebuild: {format_err:?}"
        );

        // Plain io failures must never wipe a healthy index.
        let io_err = io_to_index(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "permission denied",
        ));
        assert!(
            !is_schema_incompatible(&io_err),
            "a permission error must not trigger a wipe: {io_err:?}"
        );
    }

    // DR-211 / AILAB-205 — semantic (LESSONS.md) pass, pure-helper unit level.
    // End-to-end recall behavior lives in tests/semantic_indexing.rs.

    /// In-RAM writer, so these cases exercise the pass without a 50 MB
    /// on-disk writer heap or a tokio task.
    fn ram_writer() -> (Index, IndexWriter<TantivyDocument>, SchemaFields) {
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema);
        let writer = index.writer(15_000_000).expect("create writer");
        (index, writer, fields)
    }

    fn write_lessons(
        agent_root: &AgentRoot,
        cluster_key: &str,
        lessons: Vec<crate::lessons::Lesson>,
    ) {
        std::fs::create_dir_all(agent_root.semantic_dir()).unwrap();
        crate::lessons::write_lessons_file(
            &agent_root.lessons_md(),
            &crate::lessons::LessonsFile {
                last_updated: DateTime::parse_from_rfc3339("2026-05-20T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
                prompt_version: "dream-cycle/v1.1@2026-05-13".to_string(),
                cluster_key: cluster_key.to_string(),
                citations: Vec::new(),
                lessons,
            },
        )
        .expect("write LESSONS.md");
    }

    fn lesson(id: &str, content: &str) -> crate::lessons::Lesson {
        crate::lessons::Lesson {
            id: id.to_string(),
            content: content.to_string(),
            pinned: false,
        }
    }

    #[test]
    fn cluster_member_count_counts_children_by_prefix() {
        // consolidation promotes at the deepest prefix that met the threshold,
        // so leaf events must count toward the promoted parent.
        let events = vec![
            sample_learning(make_event_id('0'), "rust::eh", "a"),
            sample_learning(make_event_id('1'), "rust::eh::unwrap", "b"),
            sample_learning(make_event_id('2'), "rust::eh::expect", "c"),
            // Sibling with a shared textual prefix but a different segment —
            // must NOT count.
            sample_learning(make_event_id('3'), "rust::ehlers", "d"),
            sample_learning(make_event_id('4'), "python::pytest", "e"),
        ];
        assert_eq!(cluster_member_count(&events, "rust::eh"), 3);
        assert_eq!(cluster_member_count(&events, "rust::eh::unwrap"), 1);
        assert_eq!(cluster_member_count(&events, "python::pytest"), 1);
        assert_eq!(cluster_member_count(&events, "go::testing"), 0);
    }

    #[test]
    fn semantic_event_id_namespaces_the_exemplar_id() {
        let raw = make_event_id('0');
        let id = semantic_event_id(raw.as_str());
        assert!(
            id.starts_with("lsn_"),
            "lesson ids carry the lsn_ namespace"
        );
        assert_ne!(
            id,
            raw.as_str(),
            "a lesson must never carry its exemplar's raw id — both delete \
             paths key on event_id"
        );
        assert!(
            id.ends_with(raw.as_str()),
            "provenance stays one hop away: the exemplar id is recoverable"
        );
    }

    #[test]
    fn semantic_pass_clears_the_layer_when_lessons_md_is_missing() {
        let dir = unique_tmpdir("sem-missing");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        let (_index, mut writer, fields) = ram_writer();

        let outcome = index_semantic_lessons(&mut writer, &fields, &agent_root).expect("pass ok");
        assert_eq!(
            outcome,
            SemanticPassOutcome {
                indexed: 0,
                skipped: 0,
                touched: true,
            },
            "AILAB-699: an absent LESSONS.md means zero lessons, so the pass \
             deletes the semantic layer and reports touched — otherwise a \
             retired lesson keeps answering recall until restart. On a store \
             that never dreamed this is an empty-layer delete plus a commit."
        );
    }

    /// Commit `writer`, then count the live documents carrying `layer`.
    ///
    /// `layer` is STRING (raw-tokenized), so a `TermQuery` over it is the same
    /// exact match the pass deletes with.
    fn count_layer(
        index: &Index,
        writer: &mut IndexWriter<TantivyDocument>,
        fields: &SchemaFields,
        layer: Layer,
    ) -> usize {
        writer.commit().unwrap();
        let reader = index.reader().unwrap();
        reader.reload().unwrap();
        let term = tantivy::Term::from_field_text(fields.layer, layer.as_str());
        let query = tantivy::query::TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic);
        reader
            .searcher()
            .search(&query, &tantivy::collector::Count)
            .unwrap()
    }

    #[test]
    fn semantic_pass_retires_an_indexed_lesson_when_lessons_md_is_unlinked() {
        let dir = unique_tmpdir("sem-retire");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        let exemplar = make_event_id('0');
        let mate = make_event_id('1');
        // Both episodic events sit in the lesson's cluster: if retirement
        // deleted by cluster_key instead of layer it would take them too.
        let events = [
            sample_learning(exemplar.clone(), "rust::eh", "unwrap panicked"),
            sample_learning(mate.clone(), "rust::eh::unwrap", "and again"),
        ];
        prime_jsonl(&dir, &events);
        write_lessons(
            &agent_root,
            "rust::eh",
            vec![lesson(exemplar.as_str(), "prefer ? over unwrap")],
        );
        let (index, mut writer, fields) = ram_writer();
        for event in &events {
            add_document(&mut writer, &fields, event, 1).expect("index episodic event");
        }

        index_semantic_lessons(&mut writer, &fields, &agent_root).expect("first pass ok");
        assert_eq!(
            count_layer(&index, &mut writer, &fields, Layer::Semantic),
            1,
            "the lesson is indexed before retirement"
        );

        // The no-promotion dream cycle's unlink.
        std::fs::remove_file(agent_root.lessons_md()).expect("unlink LESSONS.md");

        let outcome = index_semantic_lessons(&mut writer, &fields, &agent_root).expect("pass ok");
        assert!(outcome.touched, "the retiring pass must force a commit");

        assert_eq!(
            count_layer(&index, &mut writer, &fields, Layer::Semantic),
            0,
            "unlinking LESSONS.md must retire the lesson from the live index; \
             leaving it is the AILAB-205 hole AILAB-699 closes"
        );
        assert_eq!(
            count_layer(&index, &mut writer, &fields, Layer::Episodic),
            2,
            "episodic cluster mates must survive the layer delete"
        );
    }

    #[test]
    fn semantic_pass_is_untouched_when_lessons_md_is_malformed() {
        let dir = unique_tmpdir("sem-malformed");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        std::fs::create_dir_all(agent_root.semantic_dir()).unwrap();
        std::fs::write(agent_root.lessons_md(), b"not frontmatter\n").unwrap();
        let (_index, mut writer, fields) = ram_writer();

        let outcome = index_semantic_lessons(&mut writer, &fields, &agent_root).expect("pass ok");
        assert!(
            !outcome.touched,
            "a malformed LESSONS.md must not delete the already-indexed lessons"
        );
        assert_eq!(outcome.indexed, 0);
        assert_eq!(outcome.skipped, 0);
    }

    #[test]
    fn semantic_pass_skips_a_lesson_whose_exemplar_is_absent() {
        let dir = unique_tmpdir("sem-no-exemplar");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        let present = make_event_id('0');
        prime_jsonl(
            &dir,
            &[sample_learning(
                present.clone(),
                "rust::eh",
                "borrow checker",
            )],
        );
        write_lessons(
            &agent_root,
            "rust::eh",
            vec![
                lesson(present.as_str(), "prefer ? over unwrap"),
                lesson("evt_01ARZ3NDEKTSV4RRFFQ69G5FZZ", "orphaned lesson"),
            ],
        );
        let (_index, mut writer, fields) = ram_writer();

        let outcome = index_semantic_lessons(&mut writer, &fields, &agent_root).expect("pass ok");
        assert_eq!(outcome.indexed, 1);
        assert_eq!(
            outcome.skipped, 1,
            "a lesson with no exemplar has no pain/importance to inherit and \
             must be skipped, not indexed at score 0.0"
        );
        assert!(outcome.touched);
    }

    /// Commit `writer` and pull `timestamp_sec` off every document in an in-RAM
    /// index, via the same fast field the salience collector reads.
    fn read_timestamps(index: &Index, writer: &mut IndexWriter<TantivyDocument>) -> Vec<u64> {
        writer.commit().unwrap();
        let reader = index.reader().unwrap();
        reader.reload().unwrap();
        let searcher = reader.searcher();
        let mut out = Vec::new();
        for segment in searcher.segment_readers() {
            let ts = segment
                .fast_fields()
                .u64(crate::index::TIMESTAMP_SEC_FIELD)
                .unwrap()
                .first_or_default_col(0);
            for doc in segment.doc_ids_alive() {
                out.push(ts.get_val(doc));
            }
        }
        out
    }

    #[test]
    fn semantic_doc_timestamp_is_the_consolidation_time() {
        let dir = unique_tmpdir("sem-timestamp");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        let id = make_event_id('0');
        prime_jsonl(&dir, &[sample_learning(id.clone(), "rust::eh", "exemplar")]);
        // `write_lessons` stamps last_updated = 2026-05-20T00:00:00Z, six days
        // after `sample_learning`'s 2026-05-14T08:00:00Z timestamp.
        write_lessons(
            &agent_root,
            "rust::eh",
            vec![lesson(id.as_str(), "prefer ? over unwrap")],
        );
        let exemplar_sec = DateTime::parse_from_rfc3339("2026-05-14T08:00:00Z")
            .unwrap()
            .timestamp() as u64;
        let consolidated_sec = DateTime::parse_from_rfc3339("2026-05-20T00:00:00Z")
            .unwrap()
            .timestamp() as u64;
        let (index, mut writer, fields) = ram_writer();

        index_semantic_lessons(&mut writer, &fields, &agent_root).expect("pass ok");

        let timestamps = read_timestamps(&index, &mut writer);
        assert_eq!(timestamps.len(), 1);
        assert_eq!(
            timestamps[0], consolidated_sec,
            "recency asks how stale the claim is; for a lesson that is when \
             consolidation last re-affirmed it"
        );
        assert_ne!(
            timestamps[0], exemplar_sec,
            "inheriting the exemplar's timestamp buries the lesson under \
             exp(-age/14)"
        );
    }

    #[test]
    fn semantic_pass_reports_indexed_count_for_a_full_file() {
        let dir = unique_tmpdir("sem-full");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        let a = make_event_id('0');
        let b = make_event_id('1');
        prime_jsonl(
            &dir,
            &[
                sample_learning(a.clone(), "rust::eh", "one"),
                sample_learning(b.clone(), "rust::eh::unwrap", "two"),
            ],
        );
        write_lessons(
            &agent_root,
            "rust::eh",
            vec![
                lesson(a.as_str(), "lesson one"),
                lesson(b.as_str(), "lesson two"),
            ],
        );
        let (_index, mut writer, fields) = ram_writer();

        let outcome = index_semantic_lessons(&mut writer, &fields, &agent_root).expect("pass ok");
        assert_eq!(
            outcome,
            SemanticPassOutcome {
                indexed: 2,
                skipped: 0,
                touched: true
            }
        );
    }

    // AILAB-700 — the `.dreamd/semantic_pass.json` channel from the pass to
    // `dreamd doctor`.

    /// A `SemanticPassRecord` naming one skipped lesson, for the tests that
    /// need a *prior* report on disk to prove a path did or did not clear it.
    fn stale_record() -> SemanticPassRecord {
        SemanticPassRecord {
            skipped_lesson_ids: vec!["evt_01ARZ3NDEKTSV4RRFFQ69G5FZZ".to_string()],
            cluster_key: "rust::eh".to_string(),
            indexed: 3,
        }
    }

    #[test]
    fn semantic_pass_records_the_skipped_lesson_for_doctor() {
        let dir = unique_tmpdir("sem-sidecar-skip");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        let present = make_event_id('0');
        let orphan = "evt_01ARZ3NDEKTSV4RRFFQ69G5FZZ";
        prime_jsonl(
            &dir,
            &[sample_learning(
                present.clone(),
                "rust::eh",
                "borrow checker",
            )],
        );
        write_lessons(
            &agent_root,
            "rust::eh",
            vec![
                lesson(present.as_str(), "prefer ? over unwrap"),
                lesson(orphan, "orphaned lesson"),
            ],
        );
        let (_index, mut writer, fields) = ram_writer();

        index_semantic_lessons(&mut writer, &fields, &agent_root).expect("pass ok");

        let record = read_semantic_pass_record(&agent_root)
            .expect("read sidecar")
            .expect("a pass over a real LESSONS.md must leave a report");
        assert_eq!(
            record,
            SemanticPassRecord {
                skipped_lesson_ids: vec![orphan.to_string()],
                cluster_key: "rust::eh".to_string(),
                indexed: 1,
            },
            "doctor needs the lesson id and its cluster to name the condition; \
             the count alone cannot be acted on"
        );
    }

    #[test]
    fn semantic_pass_clears_the_record_when_every_exemplar_resolves() {
        let dir = unique_tmpdir("sem-sidecar-clean");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        let a = make_event_id('0');
        let b = make_event_id('1');
        prime_jsonl(
            &dir,
            &[
                sample_learning(a.clone(), "rust::eh", "one"),
                sample_learning(b.clone(), "rust::eh::unwrap", "two"),
            ],
        );
        write_lessons(
            &agent_root,
            "rust::eh",
            vec![
                lesson(a.as_str(), "lesson one"),
                lesson(b.as_str(), "lesson two"),
            ],
        );
        // A prior pass reported a skip; this one must overwrite it.
        write_semantic_pass_record(&agent_root, &stale_record());
        let (_index, mut writer, fields) = ram_writer();

        index_semantic_lessons(&mut writer, &fields, &agent_root).expect("pass ok");

        let record = read_semantic_pass_record(&agent_root)
            .expect("read sidecar")
            .expect("report present");
        assert!(
            record.skipped_lesson_ids.is_empty(),
            "writing on the zero-skip path is load-bearing: a restored exemplar \
             must stop doctor reporting yesterday's skip, forever otherwise; \
             got: {record:?}"
        );
        assert_eq!(record.indexed, 2);
    }

    #[test]
    fn semantic_pass_clears_the_record_when_lessons_md_is_missing() {
        let dir = unique_tmpdir("sem-sidecar-missing");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        write_semantic_pass_record(&agent_root, &stale_record());
        let (_index, mut writer, fields) = ram_writer();

        index_semantic_lessons(&mut writer, &fields, &agent_root).expect("pass ok");

        let record = read_semantic_pass_record(&agent_root)
            .expect("read sidecar")
            .expect("report present");
        assert_eq!(
            record,
            SemanticPassRecord::default(),
            "AILAB-699 retirement removes the lessons themselves, so there is \
             nothing left that could not be indexed"
        );
    }

    #[test]
    fn semantic_pass_leaves_the_record_alone_when_lessons_md_is_malformed() {
        let dir = unique_tmpdir("sem-sidecar-malformed");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        std::fs::create_dir_all(agent_root.semantic_dir()).unwrap();
        std::fs::write(agent_root.lessons_md(), b"not frontmatter\n").unwrap();
        write_semantic_pass_record(&agent_root, &stale_record());
        let (_index, mut writer, fields) = ram_writer();

        index_semantic_lessons(&mut writer, &fields, &agent_root).expect("pass ok");

        let record = read_semantic_pass_record(&agent_root)
            .expect("read sidecar")
            .expect("report present");
        assert_eq!(
            record,
            stale_record(),
            "a malformed file leaves the index untouched, so the prior report \
             is still the truth about what is in it — overwriting it here would \
             claim a clean pass that never happened"
        );
    }
}
