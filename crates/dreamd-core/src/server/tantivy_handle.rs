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
//! `send` (awaited) an [`IndexerMsg::Append`]. The coordinator never holds the
//! `IndexWriter`; the writer lives entirely on the indexer task. This keeps
//! each actor with exactly one mutable resource.
//!
//! **Module split (BZR-170).** This file keeps the open/read handle:
//! [`TantivyIndexHandle::open`] (manifest check, replay, reader), `flush`,
//! `shutdown`, and `close`. The writer-owning task and its message enum live in
//! [`crate::server::indexer_actor`]; the on-disk watermark and
//! [`assess_index_freshness`] live in [`crate::server::index_freshness`]. Both
//! are re-exported here so existing `tantivy_handle::…` imports still resolve.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dreamd_protocol::{AgentLearning, EventId};
use tantivy::directory::MmapDirectory;
use tantivy::{Index, IndexReader, IndexWriter, TantivyDocument};
use tokio::sync::{mpsc, oneshot};

use crate::index::{
    build_schema, check_manifest_version, IndexManifest, ManifestCheckOutcome,
    ManifestVersionError, INDEX_MANIFEST_FILENAME, SCHEMA_VERSION,
};
use crate::io::write_atomic;
use crate::layout::AgentRoot;
use crate::server::index_freshness::{
    read_jsonl_events, read_progress, write_progress, IndexProgress,
};
use crate::server::index_map::{
    io_to_index, tantivy_io_to_index, tantivy_to_index, IndexError, IndexHandle,
};
use crate::server::indexer_actor::{
    add_document, index_semantic_lessons, run_indexer, IndexerHandle,
    DEFAULT_INDEXER_CHANNEL_CAPACITY,
};

// BZR-170: freshness and the indexer actor moved to sibling modules. Re-export
// their public surface so `crate::server::tantivy_handle::…` paths (including
// `memory_store.rs` and the `server/mod.rs` list) keep compiling unchanged.
pub use crate::server::index_freshness::{
    assess_index_freshness, IndexFreshness, INDEX_PROGRESS_FILENAME,
};
pub use crate::server::indexer_actor::{
    read_semantic_pass_record, IndexerMsg, SemanticPassRecord, SEMANTIC_PASS_FILENAME,
};

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

/// Relative directory holding the per-project Tantivy index segments.
pub const INDEX_DIR_NAME: &str = "index";

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

// Replay (two-pass)

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

// Manifest persistence

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
/// to one variant: [`IndexError::SchemaIncompatible`], which only
/// `tantivy_to_index` mints, and only from `TantivyError::SchemaError`
/// (BZR-170). No rendered string is searched. That keeps
/// `tantivy_io_to_index`'s [`IndexError::TantivyDirectory`] out — its payload
/// embeds the *directory path*, so a store living under a path containing
/// "schema" must never wipe — and keeps any other [`IndexError::Tantivy`]
/// payload out even when it happens to mention a schema.
///
/// Deliberately does not try to catch `TantivyError::IncompatibleIndex` (a
/// tantivy index-*format* mismatch). That variant renders through
/// `Incompatibility`'s hand-written `Debug`, which emits `"Library version: N,
/// index version: M. …"` — containing neither "incompatible" nor "schema". The
/// `|| msg.contains("incompatible")` alternative that used to sit here
/// therefore never matched it and only widened the false-positive surface. A
/// format mismatch stays a loud startup error, not a silent wipe.
fn is_schema_incompatible(err: &IndexError) -> bool {
    matches!(err, IndexError::SchemaIncompatible(_))
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    // AILAB-192: the four coordinator↔indexer tests below are the only readers,
    // and each drives `MemoryCoordinator::open_at`'s `#[cfg(unix)]` `indexer_tx`
    // parameter. Gated together with them so the import does not go unused
    // off-target; the module's other 35 tests are portable and stay ungated.
    #[cfg(unix)]
    use crate::coordinator::{MemoryCoordinator, MemoryCoordinatorMsg};
    use crate::index::{Layer, SchemaFields};
    use crate::server::index_map::{ProjectIndexMap, ProjectIndexMapConfig};
    use crate::server::indexer_actor::{
        cluster_member_count, semantic_event_id, write_semantic_pass_record, SemanticPassOutcome,
    };
    use crate::test_support::{unique_tmpdir, DirGuard};
    use chrono::{DateTime, Utc};
    use std::io::Write;
    use std::path::PathBuf;
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

    // Unix-only: passes `Some(idx_tx)` to `MemoryCoordinator::open_at`, whose
    // `indexer_tx` parameter is `#[cfg(unix)]` (AILAB-192).
    #[cfg(unix)]
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

    /// A full indexer channel must BLOCK the coordinator, not shed the message.
    ///
    /// Shedding is unrecoverable: `replay_two_pass` keeps `id > last_indexed_id`,
    /// and that watermark is the newest *committed* id, not a contiguous prefix
    /// — so a dropped event followed by any indexed event is skipped forever.
    ///
    /// The setup makes the backpressure deterministic without a sleep. The
    /// consumer is gated shut, so the capacity-1 indexer channel stays full. The
    /// coordinator inbox is *also* capacity 1, but be precise about what that
    /// buys: the inbox permit is released at the coordinator's `recv`, strictly
    /// *before* `route_to_indexer` runs, so `coord_tx.send(second)` returning
    /// proves only that the first message was dequeued. On the current-thread
    /// runtime `#[tokio::test]` gives us, the coordinator then runs on to its
    /// first await — the indexer `send`, which has no room — before this task is
    /// polled again; under `flavor = "multi_thread"` that last step would be a
    /// race. The count assertion, not the timing, is what makes the test sound:
    /// opening the gate must let both appends through, for three
    /// `IndexerMsg::Append`s total (the filler plus both appends). A
    /// drop-on-`Full` regression loses one or both and fails the count.
    ///
    /// Unix-only: `MemoryCoordinator::open_at`'s `indexer_tx` parameter is
    /// `#[cfg(unix)]` (AILAB-192).
    #[cfg(unix)]
    #[tokio::test]
    async fn coordinator_blocks_then_delivers_when_indexer_channel_full() {
        let dir = unique_tmpdir("coord-full");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        std::fs::create_dir_all(agent_root.episodic_dir()).unwrap();

        // Capacity 1, pre-filled: the coordinator's next `send` finds it full.
        let (idx_tx, mut idx_rx) = mpsc::channel::<IndexerMsg>(1);
        idx_tx
            .send(IndexerMsg::Append {
                event_id: make_event_id('Z'),
                learning: sample_learning(make_event_id('Z'), "rust.test", "filler"),
            })
            .await
            .unwrap();

        // Consumer, held shut by `gate_rx`. Every message it eventually sees is
        // relayed onto `seen_rx` — the relay is roomier than the traffic, so the
        // consumer itself never blocks. Holding an undrained receiver here
        // instead would deadlock the coordinator, which is exactly the point.
        let (gate_tx, gate_rx) = oneshot::channel::<()>();
        let (seen_tx, mut seen_rx) = mpsc::channel::<IndexerMsg>(16);
        let consumer = tokio::spawn(async move {
            let _ = gate_rx.await;
            while let Some(msg) = idx_rx.recv().await {
                if seen_tx.send(msg).await.is_err() {
                    break;
                }
            }
        });

        // Capacity 1 so the second `send` below proves the coordinator is parked.
        let (coord_tx, coord_rx) = mpsc::channel::<MemoryCoordinatorMsg>(1);
        let coord = MemoryCoordinator::open_at(
            &agent_root.episodic_jsonl(),
            agent_root.project_root(),
            coord_rx,
            Some(idx_tx),
        )
        .expect("open coord");
        let coord_handle = tokio::spawn(coord.run());

        let (r1_tx, r1_rx) = oneshot::channel();
        coord_tx
            .send(MemoryCoordinatorMsg::AppendLearning {
                learning: sample_learning(
                    make_event_id('0'),
                    "rust.test",
                    "first through a full channel",
                ),
                client_dedup_key: None,
                response_tx: r1_tx,
            })
            .await
            .expect("send first append");

        // Returns once the coordinator has dequeued the first message (the inbox
        // permit frees at `recv`); on this current-thread runtime it then runs on
        // into the parked `route_to_indexer` send before we are polled again.
        let (r2_tx, r2_rx) = oneshot::channel();
        coord_tx
            .send(MemoryCoordinatorMsg::AppendLearning {
                learning: sample_learning(
                    make_event_id('1'),
                    "rust.test",
                    "second through a full channel",
                ),
                client_dedup_key: None,
                response_tx: r2_tx,
            })
            .await
            .expect("send second append");

        // Release the consumer; the parked send can now make progress.
        gate_tx.send(()).expect("open consumer gate");

        // 5s bounds turn a backpressure regression that deadlocks into a fast
        // failure instead of a hung suite.
        let first = tokio::time::timeout(Duration::from_secs(5), r1_rx)
            .await
            .expect("first append must not hang behind a full indexer channel")
            .expect("recv first resp")
            .expect("first append ok");
        let second = tokio::time::timeout(Duration::from_secs(5), r2_rx)
            .await
            .expect("second append must not hang behind a full indexer channel")
            .expect("recv second resp")
            .expect("second append ok");

        let (sh_tx, sh_rx) = oneshot::channel();
        coord_tx
            .send(MemoryCoordinatorMsg::Shutdown { response_tx: sh_tx })
            .await
            .unwrap();
        sh_rx.await.unwrap();
        coord_handle.await.unwrap();
        // Coordinator dropped => its `idx_tx` clone dropped => consumer exits.
        consumer.await.expect("consumer joined");

        let mut appends = 0usize;
        while let Some(msg) = seen_rx.recv().await {
            match msg {
                IndexerMsg::Append { .. } => appends += 1,
                _ => panic!("coordinator must emit only IndexerMsg::Append"),
            }
        }
        assert_eq!(
            appends, 3,
            "1 pre-filled filler + 2 coordinator appends must all be delivered; \
             a full channel backpressures the coordinator, it never sheds"
        );

        // Both appends are durable on disk, not just in the indexer channel.
        let raw = std::fs::read_to_string(agent_root.episodic_jsonl()).expect("read jsonl");
        let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 2, "both appends must be durable in the JSONL");
        assert!(
            raw.contains(first.id.as_str()),
            "first minted id missing from JSONL"
        );
        assert!(
            raw.contains(second.id.as_str()),
            "second minted id missing from JSONL"
        );
    }

    // Unix-only: `MemoryCoordinator::open_at`'s `indexer_tx` parameter is
    // `#[cfg(unix)]` (AILAB-192).
    #[cfg(unix)]
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

    /// `IndexHandle::close` from inside a tokio runtime takes the abort branch
    /// (no `block_in_place` without `rt-multi-thread`). It must return `Ok`
    /// promptly rather than block on the indexer's `JoinHandle`. Called
    /// directly, not only as a side effect of `ProjectIndexMap::close_all`.
    #[tokio::test]
    async fn close_inside_runtime_aborts_indexer_and_returns_ok() {
        let dir = unique_tmpdir("close-in-rt");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        std::fs::create_dir_all(agent_root.episodic_dir()).unwrap();

        let handle = TantivyIndexHandle::open(&agent_root, Duration::from_secs(60)).expect("open");
        let sender = handle.sender();
        assert!(tokio::runtime::Handle::try_current().is_ok());

        IndexHandle::close(handle).expect("close inside runtime returns Ok");

        // The aborted task drops its receiver once the runtime polls it.
        tokio::time::timeout(Duration::from_secs(5), sender.closed())
            .await
            .expect("indexer task must be gone after close");
    }

    // v0.1 index-vs-JSONL contract — replay healing. The `assess_index_freshness`
    // cases moved to `index_freshness::tests` with the function (BZR-170).

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

    /// An append that met a full indexer channel is still searchable after the
    /// ordinary flush — no restart, no startup replay.
    ///
    /// A capacity-1 channel sits in front of the real [`TantivyIndexHandle`]
    /// sender, pre-filled so there is no room for the coordinator's `send`. The
    /// forwarder that relays into the live indexer is held shut by `gate_rx` so
    /// the channel *stays* full until we say otherwise — an ungated forwarder
    /// would drain the filler before the coordinator ever sent, and the
    /// coordinator would never meet a full channel at all. The coordinator inbox
    /// is capacity 1 for the same reason as
    /// [`coordinator_blocks_then_delivers_when_indexer_channel_full`]: the
    /// second `coord_tx.send` cannot return until the coordinator has dequeued
    /// the append. Only then is the gate opened.
    ///
    /// Because the coordinator awaits instead of shedding, the marker token
    /// reaches Tantivy through the normal path and recall finds it. Under a
    /// drop-on-`Full` regression the token never reaches the indexer and the
    /// recall assertion fails. Startup replay is deliberately not exercised —
    /// it only ever heals the tail after `last_indexed_id`.
    ///
    /// Unix-only: `MemoryCoordinator::open_at`'s `indexer_tx` parameter is
    /// `#[cfg(unix)]` (AILAB-192).
    #[cfg(unix)]
    #[tokio::test]
    async fn full_indexer_channel_still_reaches_recall_after_flush() {
        let dir = unique_tmpdir("chan-full-recall");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        std::fs::create_dir_all(agent_root.episodic_dir()).unwrap();

        let handle =
            TantivyIndexHandle::open(&agent_root, Duration::from_secs(60)).expect("open index");
        let real_tx = handle.sender();

        // Capacity 1, pre-filled — the coordinator's `send` blocks for a slot.
        let (idx_tx, mut idx_rx) = mpsc::channel::<IndexerMsg>(1);
        idx_tx
            .send(IndexerMsg::Append {
                event_id: make_event_id('Z'),
                learning: sample_learning(make_event_id('Z'), "rust.test", "filler"),
            })
            .await
            .unwrap();

        // Forward everything the coordinator queues into the real indexer, FIFO
        // — but not before `gate_tx` fires. Ungated, this would empty the
        // capacity-1 channel immediately and the coordinator would never meet a
        // full one, leaving the test vacuous.
        let (gate_tx, gate_rx) = oneshot::channel::<()>();
        let forwarder = tokio::spawn(async move {
            let _ = gate_rx.await;
            while let Some(msg) = idx_rx.recv().await {
                if real_tx.send(msg).await.is_err() {
                    break;
                }
            }
        });

        // Capacity 1 so the `Shutdown` send below cannot return until the
        // coordinator has dequeued the append.
        let (coord_tx, coord_rx) = mpsc::channel::<MemoryCoordinatorMsg>(1);
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

        // The inbox permit frees at the coordinator's `recv`, so this returning
        // proves only that the append was dequeued; on the current-thread test
        // runtime the coordinator then runs on to its first await — the indexer
        // `send`, which has no room — before this task is polled again. The
        // recall assertion below, not this timing, is what makes the test sound:
        // the point of the gate is that the filler is still occupying the
        // channel's one slot when the coordinator routes.
        let (sh_tx, sh_rx) = oneshot::channel();
        coord_tx
            .send(MemoryCoordinatorMsg::Shutdown { response_tx: sh_tx })
            .await
            .unwrap();

        // Release the forwarder; the parked send can now make progress.
        gate_tx.send(()).expect("open forwarder gate");

        tokio::time::timeout(Duration::from_secs(5), resp_rx)
            .await
            .expect("append must not hang behind a full indexer channel")
            .expect("recv")
            .expect("append must succeed when the indexer channel is full");

        tokio::time::timeout(Duration::from_secs(5), sh_rx)
            .await
            .expect("shutdown ack must not hang behind a full indexer channel")
            .unwrap();
        coord_handle.await.unwrap();
        // Coordinator dropped => `idx_tx` dropped => the forwarder drains and
        // exits, so the real indexer has both messages before we flush.
        forwarder.await.expect("forwarder joined");

        flush(&handle).await.expect("flush");
        handle.reader().reload().expect("reload after flush");

        let (_, fields) = build_schema();
        let now_sec = chrono::Utc::now().timestamp();
        let hits = crate::recall(handle.reader(), &fields, unique, 5, None, now_sec)
            .expect("recall after flush");
        assert!(
            !hits.is_empty(),
            "an append queued behind a full indexer channel must be recallable \
             after the ordinary flush, with no restart replay"
        );

        let fresh = assess_index_freshness(&agent_root).expect("assess after flush");
        assert!(
            !fresh.stale,
            "backpressured append must leave the index fresh: {fresh:?}"
        );

        handle.shutdown().await.expect("shutdown");
    }

    /// `is_schema_incompatible` gates a `remove_dir_all` of the index cache, so
    /// it must fire on a real tantivy schema mismatch and on nothing else.
    /// The false-positive cases below are the reason it matches the typed
    /// `SchemaIncompatible` variant rather than any substring.
    #[test]
    fn schema_incompat_matches_tantivy_schema_error_only() {
        // The real trigger: `tantivy_to_index` maps `TantivyError::SchemaError`
        // onto the typed variant (BZR-170).
        let schema_err = tantivy_to_index(tantivy::TantivyError::SchemaError(
            "field 'skill_action' not found".to_string(),
        ));
        assert!(
            matches!(schema_err, IndexError::SchemaIncompatible(_)),
            "a tantivy SchemaError must map to SchemaIncompatible: {schema_err:?}"
        );
        assert!(
            is_schema_incompatible(&schema_err),
            "a tantivy SchemaError must trigger the rebuild: {schema_err:?}"
        );

        // The gate matches the variant, not a rendering: a `Tantivy` payload
        // that merely reads like a schema error must not wipe (BZR-170).
        let lookalike = IndexError::Tantivy("Schema error: 'x' not found".to_string());
        assert!(
            !is_schema_incompatible(&lookalike),
            "only the SchemaIncompatible variant may trigger a wipe: {lookalike:?}"
        );

        // Every other tantivy error stays `Tantivy`.
        let other = tantivy_to_index(tantivy::TantivyError::InvalidArgument(
            "schema mentioned in passing".to_string(),
        ));
        assert!(
            matches!(other, IndexError::Tantivy(_)),
            "a non-schema tantivy error must stay Tantivy: {other:?}"
        );
        assert!(!is_schema_incompatible(&other));

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
