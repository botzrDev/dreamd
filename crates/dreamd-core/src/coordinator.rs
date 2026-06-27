//! `MemoryCoordinator` — the single mutable owner of the episodic JSONL.
//!
//! Per DR-103 / DR-114, all mutations to `AGENT_LEARNINGS.jsonl` flow through
//! one tokio task. The "Mutex" of DR-103 is the actor itself: `&mut self` on
//! [`MemoryCoordinator::run`] is the exclusivity guarantee — we do NOT wrap
//! the file in a `Mutex`, because there is no other handle to it.
//!
//! WEG-7 layers the durability protocol on the WEG-16 skeleton:
//!   - daemon-minted `EventId` (ULID, `evt_` + 26 Crockford chars),
//!   - 4 KiB hard reject for serialized lines (no sidecar in v0.1),
//!   - idempotency LRU keyed by (canonical AgentRoot path, client_dedup_key),
//!   - malformed-tail-skip startup recovery so a torn final line does not
//!     poison subsequent appends.
//!
//! The file writes are blocking `std::io` calls; that's intentional for v0.1.
//! The actor model already serializes mutations, so blocking the runtime task
//! is acceptable until benchmarks demand otherwise. Do NOT swap in `tokio::fs`
//! or `spawn_blocking` without re-reading `docs/architecture/durability.md`.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use dreamd_protocol::{AgentLearning, EventId, RECORD_SCHEMA_VERSION};
use lru::LruCache;
use tokio::sync::{mpsc, oneshot};
use ulid::Ulid;

use crate::layout::AgentRoot;

/// Hard cap on a single serialized JSONL line (including the trailing `\n`).
/// Anything larger is rejected at the coordinator boundary; the HTTP handler
/// maps the error to 413 Payload Too Large. Sidecar storage is deferred to
/// v0.1.1.
pub const MAX_LEARNING_LINE_BYTES: usize = 4096;

const IDEMPOTENCY_CAPACITY: usize = 1024;

/// The outcome of a successful coordinator append. Returned via the oneshot
/// channel so the HTTP handler can include `deduplicated` in the response body.
#[derive(Debug, PartialEq, Eq)]
pub struct AppendOutcome {
    pub id: EventId,
    /// `true` when the idempotency LRU returned a cached `EventId` instead of
    /// performing a new write.
    pub deduplicated: bool,
}

/// Errors surfaced by the coordinator back to API handlers.
#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialize error: {0}")]
    Serialize(#[from] serde_json::Error),
    /// Serialized line exceeds [`MAX_LEARNING_LINE_BYTES`] — caller should
    /// map to HTTP 413.
    #[error("payload too large: {size} bytes exceeds {max} byte limit")]
    PayloadTooLarge { size: usize, max: usize },
    /// Consolidation or decay failed during a [`MemoryCoordinatorMsg::RunDreamCycle`]
    /// (WEG-271). Carries the underlying error's display string — caller maps
    /// to HTTP 500.
    #[error("dream cycle: {0}")]
    DreamCycle(String),
}

/// Messages accepted by the coordinator actor.
///
/// `#[non_exhaustive]` keeps the enum forward-compatible: WEG-50 / later
/// tickets will add `Query`, etc. without breaking downstream `match` arms.
#[derive(Debug)]
#[non_exhaustive]
pub enum MemoryCoordinatorMsg {
    /// Append a learning to the JSONL. The coordinator mints the `EventId`
    /// (any `id` on the inbound `learning` is overwritten) and the response
    /// fires after `sync_data` returns — i.e., the bytes are durable on
    /// disk. The returned `EventId` is the daemon-assigned id.
    ///
    /// `client_dedup_key` enables idempotent retries: if a previous request
    /// with the same key (under the same AgentRoot) succeeded, the cached
    /// `EventId` is returned without performing a second write.
    AppendLearning {
        learning: AgentLearning,
        client_dedup_key: Option<String>,
        response_tx: oneshot::Sender<Result<AppendOutcome, CoordinatorError>>,
    },
    /// Run a deterministic dream cycle (consolidation + decay) against the
    /// coordinator's own project root, then reopen the append fd (WEG-271).
    ///
    /// Both consolidation and decay can replace `AGENT_LEARNINGS.jsonl` by
    /// atomic rename, which orphans the coordinator's long-lived fd. Routing
    /// the cycle through the actor lets the handler reopen `self.file` so
    /// subsequent appends land on the live inode. `now_sec` / `cycle_date` are
    /// supplied by the caller (the HTTP handler) so the cycle stays
    /// wall-clock-free and deterministic in tests.
    RunDreamCycle {
        now_sec: i64,
        cycle_date: String,
        response_tx: oneshot::Sender<Result<crate::decay::DecayResult, CoordinatorError>>,
    },
    /// Gracefully drain the channel and exit the run loop.
    Shutdown { response_tx: oneshot::Sender<()> },
}

/// Single mutable owner of `AGENT_LEARNINGS.jsonl`.
pub struct MemoryCoordinator {
    file: File,
    /// Path to `AGENT_LEARNINGS.jsonl`. Needed to reopen `file` after a dream
    /// cycle replaces the file by atomic rename (WEG-271) — otherwise the fd
    /// would point at an orphaned inode and subsequent appends would be lost.
    jsonl_path: PathBuf,
    /// The project root this coordinator owns. The dream cycle (consolidation +
    /// decay) runs against this root. Distinct from `agent_root_key`, which is
    /// canonicalized for idempotency keying — do NOT use the key for the cycle.
    agent_root: AgentRoot,
    rx: mpsc::Receiver<MemoryCoordinatorMsg>,
    /// Canonicalized AgentRoot path — half of the idempotency LRU key.
    /// Falls back to the un-canonicalized path if canonicalization fails
    /// (e.g., in tests against not-yet-created scratch dirs).
    agent_root_key: PathBuf,
    idempotency: LruCache<(PathBuf, String), EventId>,
    /// WEG-42: optional hand-off to the per-project Tantivy indexer task.
    /// `None` means "no indexer wired, append only" (existing tests). On
    /// `TrySendError::Full` we log `warn!` and drop the update; on
    /// `TrySendError::Closed` we log `warn!` once and drop the sender.
    #[cfg(unix)]
    indexer_tx: Option<mpsc::Sender<crate::server::tantivy_handle::IndexerMsg>>,
}

impl MemoryCoordinator {
    /// Open the JSONL at the given path, run malformed-tail-skip recovery,
    /// and return a coordinator ready to receive on `rx`.
    ///
    /// Recovery: any trailing bytes that do not deserialize as a complete
    /// `AgentLearning` are truncated. The last fully parseable line is the
    /// recovery point. Subsequent appends start from there.
    pub fn open(
        agent_root: &AgentRoot,
        rx: mpsc::Receiver<MemoryCoordinatorMsg>,
        #[cfg(unix)] indexer_tx: Option<mpsc::Sender<crate::server::tantivy_handle::IndexerMsg>>,
    ) -> std::io::Result<Self> {
        let path = agent_root.episodic_jsonl();
        Self::open_at(
            &path,
            agent_root.project_root(),
            rx,
            #[cfg(unix)]
            indexer_tx,
        )
    }

    /// Lower-level constructor used by tests that need to point at a
    /// scratch path independent of the layout module.
    pub fn open_at(
        jsonl_path: &Path,
        agent_root: &Path,
        rx: mpsc::Receiver<MemoryCoordinatorMsg>,
        #[cfg(unix)] indexer_tx: Option<mpsc::Sender<crate::server::tantivy_handle::IndexerMsg>>,
    ) -> std::io::Result<Self> {
        if let Some(parent) = jsonl_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(jsonl_path)?;

        truncate_malformed_tail(&mut file)?;
        // Seek to end for subsequent appends. We deliberately do NOT open
        // with `.append(true)` because the recovery path needs `set_len`
        // and explicit positioning.
        file.seek(SeekFrom::End(0))?;

        let agent_root_key =
            std::fs::canonicalize(agent_root).unwrap_or_else(|_| agent_root.to_path_buf());
        let cap = NonZeroUsize::new(IDEMPOTENCY_CAPACITY).expect("non-zero capacity");

        Ok(Self {
            file,
            jsonl_path: jsonl_path.to_path_buf(),
            agent_root: AgentRoot::new(agent_root),
            rx,
            agent_root_key,
            idempotency: LruCache::new(cap),
            #[cfg(unix)]
            indexer_tx,
        })
    }

    /// Run the coordinator loop until `Shutdown` is received (or the
    /// channel closes).
    pub async fn run(mut self) {
        while let Some(msg) = self.rx.recv().await {
            match msg {
                MemoryCoordinatorMsg::AppendLearning {
                    learning,
                    client_dedup_key,
                    response_tx,
                } => {
                    let result = self.handle_append(learning, client_dedup_key);
                    let _ = response_tx.send(result);
                }
                MemoryCoordinatorMsg::RunDreamCycle {
                    now_sec,
                    cycle_date,
                    response_tx,
                } => {
                    let result = self.handle_run_dream_cycle(now_sec, &cycle_date);
                    let _ = response_tx.send(result);
                }
                MemoryCoordinatorMsg::Shutdown { response_tx } => {
                    self.rx.close();
                    while self.rx.recv().await.is_some() {}
                    let _ = response_tx.send(());
                    break;
                }
            }
        }
    }

    fn handle_append(
        &mut self,
        mut learning: AgentLearning,
        client_dedup_key: Option<String>,
    ) -> Result<AppendOutcome, CoordinatorError> {
        // Idempotency lookup BEFORE any write. A hit short-circuits to the
        // cached EventId — no second line on disk.
        if let Some(key) = client_dedup_key.as_deref() {
            let full_key = (self.agent_root_key.clone(), key.to_owned());
            if let Some(cached) = self.idempotency.get(&full_key) {
                return Ok(AppendOutcome {
                    id: cached.clone(),
                    deduplicated: true,
                });
            }
        }

        // Daemon-mint the EventId. The `ulid` crate emits canonical 26-char
        // uppercase Crockford base32, so `EventId::parse` always succeeds.
        let minted_raw = format!("evt_{}", Ulid::new());
        let event_id =
            EventId::parse(&minted_raw).expect("freshly minted ULID always parses as EventId");
        learning.id = event_id.clone();

        // Server-stamp schema_version on every durable write. Closes the HTTP
        // client-trust gap (WEG-275): post_learn never re-stamped, so a client
        // could persist any value. Both ingress paths route through here.
        learning.schema_version = RECORD_SCHEMA_VERSION.to_string();

        // Write protocol (WEG-7 AC): serialize → ensure trailing \n →
        // 4 KiB check → write_all → sync_data → LRU insert.
        let mut line = serde_json::to_string(&learning)?;
        if !line.ends_with('\n') {
            line.push('\n');
        }
        let size = line.len();
        if size > MAX_LEARNING_LINE_BYTES {
            return Err(CoordinatorError::PayloadTooLarge {
                size,
                max: MAX_LEARNING_LINE_BYTES,
            });
        }
        self.file.write_all(line.as_bytes())?;
        self.file.sync_data()?;

        // Cache insert ONLY after durable write succeeds. Insert-before
        // would poison the cache on write failure.
        if let Some(key) = client_dedup_key {
            let full_key = (self.agent_root_key.clone(), key);
            self.idempotency.put(full_key, event_id.clone());
        }

        // WEG-42 hand-off to the indexer task. Best-effort non-blocking
        // try_send: `Full` is logged and dropped (next-startup replay
        // covers the loss); `Closed` is logged once and the sender is
        // dropped so we stop retrying.
        #[cfg(unix)]
        self.try_route_to_indexer(&event_id, &learning);

        Ok(AppendOutcome {
            id: event_id,
            deduplicated: false,
        })
    }

    /// WEG-271: run the deterministic dream cycle, then reopen the append fd.
    ///
    /// `run_deterministic_dream_cycle` (on cluster promotion) and
    /// `run_decay_pruner` (on prune) each replace `AGENT_LEARNINGS.jsonl` by
    /// atomic rename, leaving `self.file` pointing at an orphaned inode. We
    /// reopen unconditionally after both run so the next append lands on the
    /// live inode regardless of whether either step actually rewrote the file.
    /// The cycle writes a well-formed file via atomic rename, so no
    /// `truncate_malformed_tail` scan is needed on reopen.
    fn handle_run_dream_cycle(
        &mut self,
        now_sec: i64,
        cycle_date: &str,
    ) -> Result<crate::decay::DecayResult, CoordinatorError> {
        let decay =
            crate::dream_cycle::run_filesystem_phases(&self.agent_root, now_sec, cycle_date)
                .map_err(|e| CoordinatorError::DreamCycle(e.to_string()))?;

        self.file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.jsonl_path)?;
        self.file.seek(SeekFrom::End(0))?;
        Ok(decay)
    }

    #[cfg(unix)]
    fn try_route_to_indexer(&mut self, event_id: &EventId, learning: &AgentLearning) {
        use tokio::sync::mpsc::error::TrySendError;
        let Some(tx) = self.indexer_tx.as_ref() else {
            return;
        };
        let msg = crate::server::tantivy_handle::IndexerMsg::Append {
            event_id: event_id.clone(),
            learning: learning.clone(),
        };
        match tx.try_send(msg) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                tracing::warn!(
                    "indexer channel full; dropping IndexerMsg::Append (recoverable via next-startup replay)"
                );
            }
            Err(TrySendError::Closed(_)) => {
                tracing::warn!(
                    "indexer channel closed; dropping sender — subsequent appends will not be indexed live"
                );
                self.indexer_tx = None;
            }
        }
    }
}

/// Walk the JSONL file from the end, find the last byte offset such that
/// every preceding line deserializes cleanly to `AgentLearning`, and
/// truncate any malformed-tail bytes past that point.
///
/// Strategy: read the whole file (JSONL files in v0.1 stay small; a
/// streaming reverse-scan can come later), iterate line-by-line forward,
/// and keep the byte offset just after the last complete-and-parseable
/// line. Truncate to that offset.
///
/// "Complete" means terminated by `\n`. A final partial line without a
/// trailing newline is automatically dropped because the iterator below
/// only yields complete `\n`-terminated segments.
fn truncate_malformed_tail(file: &mut File) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;

    let mut last_good_offset: u64 = 0;
    let mut cursor: usize = 0;
    while cursor < buf.len() {
        // Find next newline.
        let Some(rel_nl) = buf[cursor..].iter().position(|b| *b == b'\n') else {
            // Trailing bytes without a newline — torn write. Drop them.
            break;
        };
        let line_end_exclusive = cursor + rel_nl; // position of '\n'
        let line_slice = &buf[cursor..line_end_exclusive];
        // An empty line (two consecutive newlines) is treated as malformed
        // and stops the scan — torn writes can leave such a gap.
        if line_slice.is_empty() {
            break;
        }
        match serde_json::from_slice::<AgentLearning>(line_slice) {
            Ok(_) => {
                last_good_offset = (line_end_exclusive + 1) as u64; // include \n
                cursor = line_end_exclusive + 1;
            }
            Err(_) => break,
        }
    }

    if (last_good_offset as usize) < buf.len() {
        file.set_len(last_good_offset)?;
        file.sync_data()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    const SAMPLE_ULID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    fn unique_tmp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("dreamd-coord-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn placeholder_id() -> EventId {
        EventId::parse(&format!("evt_{SAMPLE_ULID}")).unwrap()
    }

    fn sample_learning() -> AgentLearning {
        AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: placeholder_id(),
            timestamp: DateTime::parse_from_rfc3339("2026-05-13T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            pain: 6.0,
            importance: 7.0,
            pinned: false,
            skill_action: "rust.cargo.coordinator_test".to_string(),
            source_harness: "test-harness".to_string(),
            content: "coordinator round-trip body".to_string(),
        }
    }

    async fn spawn_coordinator(
        root: &Path,
        jsonl: &Path,
    ) -> (
        mpsc::Sender<MemoryCoordinatorMsg>,
        tokio::task::JoinHandle<()>,
    ) {
        let (tx, rx) = mpsc::channel::<MemoryCoordinatorMsg>(8);
        let coordinator = MemoryCoordinator::open_at(
            jsonl,
            root,
            rx,
            #[cfg(unix)]
            None,
        )
        .expect("open coord");
        let handle = tokio::spawn(coordinator.run());
        (tx, handle)
    }

    async fn shutdown(tx: mpsc::Sender<MemoryCoordinatorMsg>, h: tokio::task::JoinHandle<()>) {
        let (sh_tx, sh_rx) = oneshot::channel();
        tx.send(MemoryCoordinatorMsg::Shutdown { response_tx: sh_tx })
            .await
            .expect("send shutdown");
        sh_rx.await.expect("shutdown ack");
        h.await.expect("coordinator joined");
    }

    #[tokio::test]
    async fn append_learning_mints_id_and_persists_durably() {
        let dir = unique_tmp_dir("append");
        let path = dir.join("AGENT_LEARNINGS.jsonl");
        let (tx, handle) = spawn_coordinator(&dir, &path).await;

        let learning = sample_learning();
        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(MemoryCoordinatorMsg::AppendLearning {
            learning: learning.clone(),
            client_dedup_key: None,
            response_tx: resp_tx,
        })
        .await
        .expect("send append");

        let minted = resp_rx.await.expect("oneshot recv").expect("append ok").id;
        // Coordinator overwrote the placeholder id with a freshly minted one.
        assert_ne!(minted, learning.id);
        assert!(minted.as_str().starts_with("evt_"));
        assert_eq!(minted.as_str().len(), "evt_".len() + 26);

        let raw = std::fs::read_to_string(&path).expect("read jsonl");
        let line = raw.lines().next().expect("at least one line");
        let decoded: AgentLearning = serde_json::from_str(line).expect("deserialize");
        assert_eq!(decoded.id, minted);
        assert_eq!(decoded.content, learning.content);

        shutdown(tx, handle).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn idempotency_lru_short_circuits_on_dedup_key_hit() {
        let dir = unique_tmp_dir("lru");
        let path = dir.join("AGENT_LEARNINGS.jsonl");
        let (tx, handle) = spawn_coordinator(&dir, &path).await;

        let dedup = Some("client-req-42".to_string());

        // First call: writes one line.
        let (r1_tx, r1_rx) = oneshot::channel();
        tx.send(MemoryCoordinatorMsg::AppendLearning {
            learning: sample_learning(),
            client_dedup_key: dedup.clone(),
            response_tx: r1_tx,
        })
        .await
        .unwrap();
        let id1 = r1_rx.await.unwrap().unwrap().id;

        // Second call with same dedup key: must return same id, no new line.
        let (r2_tx, r2_rx) = oneshot::channel();
        tx.send(MemoryCoordinatorMsg::AppendLearning {
            learning: sample_learning(),
            client_dedup_key: dedup,
            response_tx: r2_tx,
        })
        .await
        .unwrap();
        let id2 = r2_rx.await.unwrap().unwrap().id;
        assert_eq!(id1, id2, "dedup hit must return cached EventId");

        shutdown(tx, handle).await;

        // Exactly one line on disk.
        let raw = std::fs::read_to_string(&path).expect("read jsonl");
        let lines: Vec<&str> = raw.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 1, "second call must not produce a second line");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn payload_over_4kib_rejected_with_payload_too_large() {
        let dir = unique_tmp_dir("toobig");
        let path = dir.join("AGENT_LEARNINGS.jsonl");
        let (tx, handle) = spawn_coordinator(&dir, &path).await;

        let mut huge = sample_learning();
        // 5 KiB of content guarantees > 4096 byte serialized line.
        huge.content = "x".repeat(5 * 1024);

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(MemoryCoordinatorMsg::AppendLearning {
            learning: huge,
            client_dedup_key: None,
            response_tx: resp_tx,
        })
        .await
        .unwrap();
        let err = resp_rx
            .await
            .unwrap()
            .expect_err("oversized payload must be rejected");
        match err {
            CoordinatorError::PayloadTooLarge { size, max } => {
                assert!(size > max);
                assert_eq!(max, MAX_LEARNING_LINE_BYTES);
            }
            other => panic!("expected PayloadTooLarge, got {other:?}"),
        }

        shutdown(tx, handle).await;

        // Nothing was written to disk.
        let raw = std::fs::read_to_string(&path).expect("read jsonl");
        assert!(raw.is_empty(), "rejected payload must leave file untouched");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn malformed_tail_skipped_on_startup() {
        let dir = unique_tmp_dir("recover");
        let path = dir.join("AGENT_LEARNINGS.jsonl");

        // Prime the file with one good line plus a torn partial line.
        let good = sample_learning();
        let mut good_line = serde_json::to_string(&good).unwrap();
        good_line.push('\n');
        let partial = "{\"schema_version\":\"1.0.0\",\"id\":\"evt_TRUNCAT"; // no \n
        let mut primed = good_line.as_bytes().to_vec();
        primed.extend_from_slice(partial.as_bytes());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, &primed).unwrap();

        // Construct the coordinator — recovery should truncate the torn tail.
        let (tx, handle) = spawn_coordinator(&dir, &path).await;

        // File on disk now contains only the one good line.
        let after = std::fs::read_to_string(&path).expect("read jsonl");
        let lines: Vec<&str> = after.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 1, "torn tail must be truncated");
        let decoded: AgentLearning = serde_json::from_str(lines[0]).expect("recovered line parses");
        assert_eq!(decoded.content, good.content);

        // Subsequent append lands on a clean boundary.
        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(MemoryCoordinatorMsg::AppendLearning {
            learning: sample_learning(),
            client_dedup_key: None,
            response_tx: resp_tx,
        })
        .await
        .unwrap();
        let _new_id = resp_rx.await.unwrap().expect("append after recovery").id;

        shutdown(tx, handle).await;

        let after2 = std::fs::read_to_string(&path).expect("re-read jsonl");
        let lines2: Vec<&str> = after2.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            lines2.len(),
            2,
            "one recovered + one new = two complete lines"
        );
        for line in &lines2 {
            let _: AgentLearning = serde_json::from_str(line).expect("each line parses cleanly");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
