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
use tantivy::{doc, Index, IndexWriter, TantivyDocument};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::index::{build_schema, IndexManifest, Layer, SchemaFields, INDEX_MANIFEST_FILENAME};
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
/// project's `.dreamd/` directory. WEG-42 owns reads and writes; no other
/// ticket touches this file.
pub(crate) const INDEX_PROGRESS_FILENAME: &str = "index_progress.json";

/// Relative directory holding the per-project Tantivy index segments.
pub(crate) const INDEX_DIR_NAME: &str = "index";

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
        write_manifest_if_absent(&manifest_path)?;

        let progress_path = dreamd_dir.join(INDEX_PROGRESS_FILENAME);
        let jsonl_path = agent_root.episodic_jsonl();

        let (schema, fields) = build_schema();
        let mmap_dir = MmapDirectory::open(&index_dir).map_err(tantivy_io_to_index)?;
        let index = Index::open_or_create(mmap_dir, schema).map_err(tantivy_to_index)?;
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
        if !to_index.is_empty() {
            writer.commit().map_err(tantivy_to_index)?;
            write_progress(
                &progress_path,
                &IndexProgress {
                    last_indexed_id: latest_committed.clone(),
                },
            )?;
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
        })
    }

    /// Clone the sender for the indexer task. Pass into
    /// `MemoryCoordinator::open` so coordinator appends route to this index.
    pub fn sender(&self) -> mpsc::Sender<IndexerMsg> {
        self.indexer.sender()
    }

    /// Update `last_used` to `Instant::now()`. Called by the supervisor on
    /// each access through `ProjectIndexMap::get_or_open`.
    pub fn touch(&self) {
        *self.last_used.lock().expect("last_used mutex poisoned") = Instant::now();
    }

    /// Async drain path. Drops the indexer sender, awaits task completion
    /// (which performs a final commit before exiting). Preferred over
    /// [`IndexHandle::close`] when called from an async context — sidesteps
    /// the sync/async runtime juggling in `close`.
    pub async fn shutdown(self) -> Result<(), IndexError> {
        let IndexerHandle { tx, join } = self.indexer;
        drop(tx);
        join.await
            .map_err(|e| IndexError(format!("indexer task join: {e}")))?;
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
                    .map_err(|e| IndexError(format!("build runtime for close: {e}")))?;
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

// ---------------------------------------------------------------------------
// Indexer task
// ---------------------------------------------------------------------------

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

/// Map an [`AgentLearning`] onto a Tantivy document and add it to the writer.
/// `layer` is always [`Layer::Episodic`] in v0.1; semantic indexing is WEG-136.
fn add_document(
    writer: &mut IndexWriter<TantivyDocument>,
    fields: &SchemaFields,
    learning: &AgentLearning,
    recurrence: u32,
) -> Result<(), IndexError> {
    let ts = learning.timestamp.timestamp() as u64;
    let layer_str = Layer::Episodic.as_str().to_string();
    let doc = doc!(
        fields.content => learning.content.clone(),
        fields.timestamp_sec => ts,
        fields.pain => learning.pain as f64,
        fields.importance => learning.importance as f64,
        fields.recurrence => recurrence as u64,
        fields.layer => layer_str,
        fields.last_updated_sec => ts,
        fields.cited_event_count => 0u64,
    );
    writer.add_document(doc).map_err(tantivy_to_index)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Replay (two-pass)
// ---------------------------------------------------------------------------

fn replay_two_pass(
    jsonl_path: &Path,
    last_indexed_id: Option<&str>,
) -> Result<(HashMap<String, u32>, Vec<AgentLearning>), IndexError> {
    let bytes = match std::fs::read(jsonl_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((HashMap::new(), Vec::new()));
        }
        Err(e) => return Err(IndexError(format!("read jsonl: {e}"))),
    };

    // Pass 1: build final cluster counts by walking every well-formed line.
    let pass1 = parse_jsonl_until_torn(&bytes);
    let mut clusters: HashMap<String, u32> = HashMap::new();
    for ev in &pass1 {
        *clusters.entry(ev.skill_action.clone()).or_insert(0) += 1;
    }

    // Pass 2: filter to events whose id is strictly greater than the watermark.
    // EventId does not implement `Ord`; compare via `as_str()` (ULIDs are
    // lexicographically sortable in their canonical Crockford base32 form).
    // Separate `parse_jsonl_until_torn` call rather than re-filtering pass-1's
    // Vec so we get an owned iterator without cloning the full collection.
    let to_index: Vec<AgentLearning> = parse_jsonl_until_torn(&bytes)
        .into_iter()
        .filter(|ev| match last_indexed_id {
            Some(last) => ev.id.as_str() > last,
            None => true,
        })
        .collect();

    Ok((clusters, to_index))
}

/// Walk JSONL bytes line-by-line and return every cleanly-parseable record
/// up to the first torn-write halt signal (blank line or unparseable record).
/// Mirrors the convention in `coordinator::truncate_malformed_tail` — see
/// drift catalog entry `torn-write-blank-line-signal`.
fn parse_jsonl_until_torn(bytes: &[u8]) -> Vec<AgentLearning> {
    let mut out = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let Some(rel_nl) = bytes[cursor..].iter().position(|b| *b == b'\n') else {
            break;
        };
        let line_end = cursor + rel_nl;
        let slice = &bytes[cursor..line_end];
        if slice.is_empty() {
            break;
        }
        match serde_json::from_slice::<AgentLearning>(slice) {
            Ok(ev) => out.push(ev),
            Err(_) => break,
        }
        cursor = line_end + 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Progress + manifest persistence
// ---------------------------------------------------------------------------

fn read_progress(path: &Path) -> Result<IndexProgress, IndexError> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| IndexError(format!("parse {INDEX_PROGRESS_FILENAME}: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(IndexProgress::default()),
        Err(e) => Err(IndexError(format!("read {INDEX_PROGRESS_FILENAME}: {e}"))),
    }
}

fn write_progress(path: &Path, progress: &IndexProgress) -> Result<(), IndexError> {
    let bytes = serde_json::to_vec(progress)
        .map_err(|e| IndexError(format!("serialize {INDEX_PROGRESS_FILENAME}: {e}")))?;
    write_atomic(path, &bytes)
        .map_err(|e| IndexError(format!("write {INDEX_PROGRESS_FILENAME}: {e}")))?;
    Ok(())
}

fn write_manifest_if_absent(path: &Path) -> Result<(), IndexError> {
    if path.exists() {
        return Ok(());
    }
    let manifest = IndexManifest::current();
    let bytes = serde_json::to_vec(&manifest)
        .map_err(|e| IndexError(format!("serialize {INDEX_MANIFEST_FILENAME}: {e}")))?;
    write_atomic(path, &bytes)
        .map_err(|e| IndexError(format!("write {INDEX_MANIFEST_FILENAME}: {e}")))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn io_to_index(e: std::io::Error) -> IndexError {
    IndexError(format!("io: {e}"))
}

fn tantivy_to_index<E: std::fmt::Display>(e: E) -> IndexError {
    IndexError(format!("tantivy: {e}"))
}

fn tantivy_io_to_index(e: tantivy::directory::error::OpenDirectoryError) -> IndexError {
    IndexError(format!("tantivy directory: {e}"))
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::{MemoryCoordinator, MemoryCoordinatorMsg};
    use crate::server::index_map::{ProjectIndexMap, ProjectIndexMapConfig};
    use chrono::{DateTime, Utc};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tantivy::query::AllQuery;
    use tantivy::ReloadPolicy;

    const SAMPLE_ULID_BASE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA";

    fn unique_tmpdir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "dreamd-tantivy-{}-{}-{}-{}",
            label,
            std::process::id(),
            nanos,
            n,
        ));
        std::fs::create_dir_all(&dir).expect("create tmpdir");
        dir
    }

    /// RAII cleanup guard: removes the temp dir on drop so tests leave no
    /// scratch behind even when they panic. Preferred over manual cleanup at
    /// the end of each test because cleanup still runs on panic / assertion
    /// failure.
    struct DirGuard(PathBuf);
    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

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
            schema_version: "1.0".to_string(),
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
        let (tx, rx) = oneshot::channel();
        handle
            .sender()
            .send(IndexerMsg::Flush { ack: tx })
            .await
            .expect("send flush");
        rx.await.expect("flush ack")
    }

    // -----------------------------------------------------------------------
    // AC-1
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // AC-2 — replay two-pass + AC-2 watermark gating
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // AC-3 — steady-state recurrence + bounded-staleness
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // AC-5 — progress file written after commit only
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // AC-7 — manifest written on first open only
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // AC-9 — commit cadence drives a tick (short cadence)
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // AC-16 — replay halts on blank line / unparseable record
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn replay_halts_on_blank_line_in_jsonl() {
        let dir = unique_tmpdir("replay-blank");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        let jsonl_path = agent_root.episodic_jsonl();
        std::fs::create_dir_all(jsonl_path.parent().unwrap()).unwrap();

        let good1 = sample_learning(make_event_id('0'), "rust.test", "first");
        let good2 = sample_learning(make_event_id('1'), "rust.test", "after blank");
        let mut bytes = serde_json::to_string(&good1).unwrap();
        bytes.push('\n');
        bytes.push('\n'); // blank line — torn-write halt signal
        let mut tail = serde_json::to_string(&good2).unwrap();
        tail.push('\n');
        bytes.push_str(&tail);
        std::fs::write(&jsonl_path, bytes.as_bytes()).unwrap();

        let handle = TantivyIndexHandle::open(&agent_root, Duration::from_secs(60)).expect("open");
        handle.shutdown().await.expect("shutdown");

        assert_eq!(
            count_docs(&agent_root),
            1,
            "only the pre-blank record is indexed"
        );
    }

    #[tokio::test]
    async fn replay_halts_on_unparseable_record() {
        let dir = unique_tmpdir("replay-bad");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        let jsonl_path = agent_root.episodic_jsonl();
        std::fs::create_dir_all(jsonl_path.parent().unwrap()).unwrap();

        let good1 = sample_learning(make_event_id('0'), "rust.test", "first");
        let mut bytes = serde_json::to_string(&good1).unwrap();
        bytes.push('\n');
        bytes.push_str("{not json}\n"); // unparseable — halt
        let good2 = sample_learning(make_event_id('1'), "rust.test", "unreachable");
        bytes.push_str(&serde_json::to_string(&good2).unwrap());
        bytes.push('\n');
        std::fs::write(&jsonl_path, bytes.as_bytes()).unwrap();

        let handle = TantivyIndexHandle::open(&agent_root, Duration::from_secs(60)).expect("open");
        handle.shutdown().await.expect("shutdown");

        assert_eq!(
            count_docs(&agent_root),
            1,
            "only the pre-corrupt record is indexed"
        );
    }

    // -----------------------------------------------------------------------
    // AC-23 — coordinator routes appends to indexer sender
    // -----------------------------------------------------------------------

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
            .expect("append must succeed even when indexer channel is full");
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
        let id1 = r1_rx.await.unwrap().unwrap();

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
        let id2 = r2_rx.await.unwrap().unwrap();
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

    // -----------------------------------------------------------------------
    // AC-24 — shutdown flushes final batch to disk
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // AC-11 — trait conformance + ProjectIndexMap integration
    // -----------------------------------------------------------------------

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
}
