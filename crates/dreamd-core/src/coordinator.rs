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
#[cfg(unix)]
use std::io::Write;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use dreamd_protocol::{AgentLearning, EventId, RECORD_SCHEMA_VERSION};
use lru::LruCache;
use tokio::sync::{mpsc, oneshot};

use crate::episodic::{self, EpisodicError};
use crate::id_minter::IdMinter;
use crate::layout::AgentRoot;

/// Hard cap on a single serialized JSONL line — the on-disk line-format limit
/// now lives in [`episodic`](crate::episodic). Re-exported so existing callers
/// (`crate::coordinator::MAX_LEARNING_LINE_BYTES`) and the HTTP 413 mapping keep
/// resolving unchanged.
pub use crate::episodic::MAX_LEARNING_LINE_BYTES;

const IDEMPOTENCY_CAPACITY: usize = 1024;

/// Cached outcome for an idempotent retry — both fields are returned verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
struct IdempotencyEntry {
    id: EventId,
    timestamp: DateTime<Utc>,
}

/// The outcome of a successful coordinator append. Returned via the oneshot
/// channel so the HTTP handler can include `deduplicated` in the response body.
#[derive(Debug, PartialEq, Eq)]
pub struct AppendOutcome {
    pub id: EventId,
    /// Daemon-minted UTC timestamp stamped at durable write (or cached on dedup hit).
    pub timestamp: DateTime<Utc>,
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

impl From<EpisodicError> for CoordinatorError {
    fn from(e: EpisodicError) -> Self {
        match e {
            EpisodicError::Io(io) => CoordinatorError::Io(io),
            EpisodicError::Serialize(je) => CoordinatorError::Serialize(je),
            EpisodicError::PayloadTooLarge { size, max } => {
                CoordinatorError::PayloadTooLarge { size, max }
            }
        }
    }
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
        /// Force the deterministic lesson body even when credentials exist
        /// (AILAB-204). Set from the `x-dreamd-no-llm: 1` request header.
        no_llm: bool,
        /// Per-call consent to include `.agent/personal/` in the composition
        /// prompt (AILAB-199). Set from the `x-dreamd-share-personal: 1` request
        /// header. A required field rather than a defaulted one: a new message
        /// site that forgets it must fail to compile, not quietly disclose.
        share_personal: bool,
        response_tx: oneshot::Sender<Result<crate::decay::DecayResult, CoordinatorError>>,
    },
    /// Gracefully drain the channel and exit the run loop.
    Shutdown { response_tx: oneshot::Sender<()> },
}

/// Single mutable owner of `AGENT_LEARNINGS.jsonl`.
pub struct MemoryCoordinator {
    file: File,
    /// Path to `AGENT_LEARNINGS.jsonl`. Needed to reopen `file` after a dream
    /// cycle, or anything else, replaces the file (WEG-271).
    jsonl_path: PathBuf,
    /// Issues this coordinator's event ids; seeded from the log at open.
    minter: IdMinter,
    /// Device, inode and length of the file `file` writes, as of its last
    /// write. Compared with the path around every append.
    #[cfg(unix)]
    identity: LogIdentity,
    /// The project root this coordinator owns. The dream cycle (consolidation +
    /// decay) runs against this root. Distinct from `agent_root_key`, which is
    /// canonicalized for idempotency keying — do NOT use the key for the cycle.
    agent_root: AgentRoot,
    rx: mpsc::Receiver<MemoryCoordinatorMsg>,
    /// Canonicalized AgentRoot path — half of the idempotency LRU key.
    /// Falls back to the un-canonicalized path if canonicalization fails
    /// (e.g., in tests against not-yet-created scratch dirs).
    agent_root_key: PathBuf,
    idempotency: LruCache<(PathBuf, String), IdempotencyEntry>,
    /// WEG-42: optional hand-off to the per-project Tantivy indexer task.
    /// `None` means "no indexer wired, append only" (existing tests). The
    /// hand-off awaits `send`, so a full channel backpressures this actor
    /// rather than shedding an update; when the channel is closed we log
    /// `warn!` once and drop the sender.
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
        // Recovery rewrites from offset 0, which `O_APPEND` would send to the
        // end, so it runs on its own plain fd, closed at the end of this block.
        // `set_len` alone would work on either.
        let records = {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(jsonl_path)?;
            episodic::recover(&mut file, jsonl_path)?
        };
        let minter = IdMinter::seeded(records.iter().map(|r| &r.id), SystemTime::now());
        let file = open_append(jsonl_path)?;
        #[cfg(unix)]
        let identity = LogIdentity::of(&file.metadata()?);

        let agent_root_key =
            std::fs::canonicalize(agent_root).unwrap_or_else(|_| agent_root.to_path_buf());
        let cap = NonZeroUsize::new(IDEMPOTENCY_CAPACITY).expect("non-zero capacity");

        Ok(Self {
            file,
            jsonl_path: jsonl_path.to_path_buf(),
            minter,
            #[cfg(unix)]
            identity,
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
                    let result = self.handle_append(learning, client_dedup_key).await;
                    let _ = response_tx.send(result);
                }
                MemoryCoordinatorMsg::RunDreamCycle {
                    now_sec,
                    cycle_date,
                    no_llm,
                    share_personal,
                    response_tx,
                } => {
                    // AILAB-204: awaited here, in the actor loop, so the cycle
                    // (LLM call included) stays inside the coordinator's single
                    // -writer window. Appends queue behind it; they are not
                    // raced by a second writer.
                    let result = self
                        .handle_run_dream_cycle(now_sec, &cycle_date, no_llm, share_personal)
                        .await;
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

    async fn handle_append(
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
                    id: cached.id.clone(),
                    timestamp: cached.timestamp,
                    deduplicated: true,
                });
            }
        }

        // A `git checkout` or `merge` under a live daemon replaces the log.
        // Write to the file at the path, not the replaced one.
        #[cfg(unix)]
        self.ensure_live_log()?;

        // One clock read stamps the id and `timestamp`.
        let now = SystemTime::now();
        let event_id = self.minter.next_at(now);
        learning.id = event_id.clone();

        // Server-stamp schema_version on every durable write. Closes the HTTP
        // client-trust gap (WEG-275): post_learn never re-stamped, so a client
        // could persist any value. Both ingress paths route through here.
        learning.schema_version = RECORD_SCHEMA_VERSION.to_string();

        // Server-stamp timestamp — same "daemon owns durable fields" rule as `id`.
        let stamped: DateTime<Utc> = now.into();
        learning.timestamp = stamped;

        // skill_action is NOT re-validated here (ARCHITECTURE.md §9): ingress
        // (`LearnIngress`) is the sole gate; on-disk String accepts legacy
        // dotted keys that the dream cycle treats as opaque single segments.

        // Write protocol (WEG-7 AC): serialize → ensure trailing \n → 4 KiB
        // check → write_all → sync_data. Delegated to the single durable-write
        // seam `episodic::append` (WEG-378); `PayloadTooLarge` maps to HTTP 413
        // via `From<EpisodicError>`. LRU insert stays below — only after a
        // durable write succeeds.
        let written = episodic::append(&mut self.file, &learning)?;
        #[cfg(unix)]
        self.confirm_landed(&learning, written)?;

        // Cache insert ONLY after durable write succeeds. Insert-before
        // would poison the cache on write failure.
        if let Some(key) = client_dedup_key {
            let full_key = (self.agent_root_key.clone(), key);
            self.idempotency.put(
                full_key,
                IdempotencyEntry {
                    id: event_id.clone(),
                    timestamp: stamped,
                },
            );
        }

        // WEG-42 hand-off to the indexer task. The send is AWAITED: a full
        // channel backpressures this actor rather than shedding the message.
        // Upstream, in-flight learns queue in the 256-slot coordinator channel
        // and simply wait — there is no per-request timeout — so a brief park
        // shows up as latency; only once that inbox is full too does the
        // `Supervisor`'s separate 100 ms send timeout start returning HTTP 503,
        // and only on the HTTP ingress (the in-process `dreamd mcp` path sends
        // on this channel without that timeout, so it waits). Shedding is NOT
        // replay-healed: startup replay filters `id > last_indexed_id`, a
        // watermark rather than a contiguous prefix, so a gap followed by any
        // indexed event is skipped forever. A `Closed` channel is still logged
        // once and the sender dropped so we stop retrying.
        #[cfg(unix)]
        self.route_to_indexer(&event_id, &learning).await;

        Ok(AppendOutcome {
            id: event_id,
            timestamp: stamped,
            deduplicated: false,
        })
    }

    /// WEG-271: run the deterministic dream cycle, then reopen the append fd.
    ///
    /// `run_deterministic_dream_cycle` (on cluster promotion) and
    /// `run_decay_pruner` (on prune) each replace `AGENT_LEARNINGS.jsonl` by
    /// atomic rename, leaving `self.file` pointing at an orphaned inode. It
    /// reopens on every exit path, error included, so the next append lands on
    /// the live inode whether or not either step rewrote the file and whether
    /// or not the cycle succeeded.
    /// The cycle writes a well-formed file via atomic rename, so no
    /// `episodic::recover` scan is needed on reopen.
    async fn handle_run_dream_cycle(
        &mut self,
        now_sec: i64,
        cycle_date: &str,
        no_llm: bool,
        share_personal: bool,
    ) -> Result<crate::decay::DecayResult, CoordinatorError> {
        let result = crate::dream_cycle::run_filesystem_phases(
            &self.agent_root,
            now_sec,
            cycle_date,
            no_llm,
            share_personal,
        )
        .await
        .map_err(|e| CoordinatorError::DreamCycle(e.to_string()));
        // Every exit path reopens: a phase can fail after the pin pass or decay
        // has already replaced the log by rename.
        let reopened = self.reopen();
        match (result, reopened) {
            (Ok(decay), Ok(())) => Ok(decay),
            (Ok(_), Err(e)) => Err(e.into()),
            (Err(e), reopened) => {
                if let Err(re) = reopened {
                    tracing::warn!(
                        error = %re,
                        "could not reopen the episodic log after a failed dream cycle"
                    );
                }
                Err(e)
            }
        }
    }

    /// Point `file` at whatever is at `jsonl_path` now.
    fn reopen(&mut self) -> std::io::Result<()> {
        self.file = open_append(&self.jsonl_path)?;
        #[cfg(unix)]
        {
            self.identity = LogIdentity::of(&self.file.metadata()?);
        }
        Ok(())
    }

    /// Before a write: if the file at the path is not the one `file` last
    /// wrote, reopen it.
    #[cfg(unix)]
    fn ensure_live_log(&mut self) -> Result<(), CoordinatorError> {
        if LogIdentity::at(&self.jsonl_path)? != Some(self.identity) {
            self.reopen_after_external_change()?;
        }
        Ok(())
    }

    /// After a write of `written` bytes: if the path no longer shows that
    /// write, reopen it, and append `learning` again unless the file now at the
    /// path already holds its id.
    #[cfg(unix)]
    fn confirm_landed(
        &mut self,
        learning: &AgentLearning,
        written: usize,
    ) -> Result<(), CoordinatorError> {
        self.identity.len += written as u64;
        if LogIdentity::at(&self.jsonl_path)? == Some(self.identity) {
            return Ok(());
        }
        let records = self.reopen_after_external_change()?;
        if !records.iter().any(|r| r.id == learning.id) {
            let written = episodic::append(&mut self.file, learning)?;
            self.identity.len += written as u64;
        }
        Ok(())
    }

    /// Something other than this coordinator replaced or edited the log.
    /// Reopen it, end a foreign final line that lacks `\n`, and raise the
    /// minter's floor to the file's largest id. Returns the file's records.
    #[cfg(unix)]
    fn reopen_after_external_change(&mut self) -> Result<Vec<AgentLearning>, CoordinatorError> {
        use std::os::unix::fs::FileExt;

        self.reopen()?;
        let len = self.file.metadata()?.len();
        if len > 0 {
            let mut last = [0u8; 1];
            self.file.read_exact_at(&mut last, len - 1)?;
            if last[0] != b'\n' {
                // End the foreign line, so the next append starts its own line
                // instead of extending that one.
                self.file.write_all(b"\n")?;
                self.file.sync_data()?;
            }
        }
        let records = episodic::read_all(&self.jsonl_path)?;
        self.minter
            .observe(records.iter().map(|r| &r.id), SystemTime::now());
        self.identity = LogIdentity::of(&self.file.metadata()?);
        tracing::warn!(
            path = %self.jsonl_path.display(),
            "episodic log changed outside the coordinator; reopened it"
        );
        // BZR-1236 queues `IndexerMsg::Resync` here.
        Ok(records)
    }

    /// Hand one durable append to the indexer task, **awaiting** the send.
    ///
    /// The await is the point. A bounded channel that is momentarily full must
    /// block this actor, not shed the message: startup `replay_two_pass` keeps
    /// events whose `id > last_indexed_id`, and that watermark is the newest
    /// committed id, not a contiguous prefix. Shed event N, index N+1, and N is
    /// skipped forever. Blocking here stops the coordinator reading its own
    /// inbox, so ingress degrades honestly instead of returning 201 for a record
    /// recall will never see: the in-flight learns already queued in the 256-slot
    /// coordinator channel just *wait* (no per-request timeout), and only
    /// overflow past that inbox meets the `Supervisor`'s separate 100 ms send
    /// timeout and becomes an HTTP 503. Sustained overload therefore surfaces as
    /// 503, a brief park as latency — and only on the HTTP ingress: the
    /// in-process `dreamd mcp` path sends on this channel without that timeout,
    /// so it waits.
    ///
    /// The append is acked once the indexer *accepts* the message; the Tantivy
    /// commit still lands on the usual 5 s cadence. A closed channel is terminal
    /// for live routing: warn once, drop the sender, and let the next startup
    /// replay pick the tail up.
    #[cfg(unix)]
    async fn route_to_indexer(&mut self, event_id: &EventId, learning: &AgentLearning) {
        let Some(tx) = self.indexer_tx.as_ref() else {
            return;
        };
        let msg = crate::server::tantivy_handle::IndexerMsg::Append {
            event_id: event_id.clone(),
            learning: learning.clone(),
        };
        if tx.send(msg).await.is_err() {
            tracing::warn!(
                "indexer channel closed; dropping sender — subsequent appends will not be indexed live"
            );
            self.indexer_tx = None;
        }
    }
}

/// The append fd. `read` is for the tail check after an outside change; every
/// write goes to the end of the file.
fn open_append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(path)
}

/// What the coordinator expects at `jsonl_path`.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LogIdentity {
    dev: u64,
    ino: u64,
    len: u64,
}

#[cfg(unix)]
impl LogIdentity {
    fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
        }
    }

    /// `None` when nothing is at `path`.
    fn at(path: &Path) -> std::io::Result<Option<Self>> {
        match std::fs::metadata(path) {
            Ok(meta) => Ok(Some(Self::of(&meta))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
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

        let outcome = resp_rx.await.expect("oneshot recv").expect("append ok");
        let minted = outcome.id;
        // Coordinator overwrote the placeholder id with a freshly minted one.
        assert_ne!(minted, learning.id);
        assert!(minted.as_str().starts_with("evt_"));
        assert_eq!(minted.as_str().len(), "evt_".len() + 26);
        assert_ne!(
            outcome.timestamp, learning.timestamp,
            "client-supplied timestamp must be server-stamped"
        );

        let raw = std::fs::read_to_string(&path).expect("read jsonl");
        let line = raw.lines().next().expect("at least one line");
        let decoded: AgentLearning = serde_json::from_str(line).expect("deserialize");
        assert_eq!(decoded.id, minted);
        assert_eq!(decoded.timestamp, outcome.timestamp);
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

    /// Fixed clock for the dream-cycle fixture; the coordinator restamps appends.
    const CYCLE_NOW_SEC: i64 = 1_780_000_000;

    fn learning_with(id: &EventId, skill: &str, content: &str, ts: i64) -> AgentLearning {
        AgentLearning {
            id: id.clone(),
            timestamp: DateTime::from_timestamp(ts, 0).expect("valid timestamp"),
            skill_action: skill.to_string(),
            content: content.to_string(),
            ..sample_learning()
        }
    }

    fn jsonl_line(learning: &AgentLearning) -> String {
        let mut line = serde_json::to_string(learning).expect("serialize learning");
        line.push('\n');
        line
    }

    /// An id `offset` from the wall clock, so the coordinator's own clock read
    /// sits on the other side of it.
    fn id_from_now(offset: std::time::Duration, ahead: bool) -> EventId {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis() as u64;
        let delta = offset.as_millis() as u64;
        let ms = if ahead {
            now_ms + delta
        } else {
            now_ms - delta
        };
        EventId::parse(&format!("evt_{}", ulid::Ulid::from_parts(ms, 0))).unwrap()
    }

    fn open_coordinator(
        root: &AgentRoot,
    ) -> (MemoryCoordinator, mpsc::Sender<MemoryCoordinatorMsg>) {
        let (tx, rx) = mpsc::channel::<MemoryCoordinatorMsg>(8);
        let c = MemoryCoordinator::open_at(
            &root.episodic_jsonl(),
            root.project_root(),
            rx,
            #[cfg(unix)]
            None,
        )
        .expect("open coord");
        (c, tx)
    }

    async fn append_via(
        tx: &mpsc::Sender<MemoryCoordinatorMsg>,
        learning: AgentLearning,
    ) -> EventId {
        let (response_tx, response_rx) = oneshot::channel();
        tx.send(MemoryCoordinatorMsg::AppendLearning {
            learning,
            client_dedup_key: None,
            response_tx,
        })
        .await
        .expect("send append");
        response_rx
            .await
            .expect("oneshot recv")
            .expect("append ok")
            .id
    }

    /// Replace the log the way `git checkout` does: write a sibling, rename it over.
    #[cfg(unix)]
    fn replace_by_rename(jsonl: &Path, bytes: &[u8]) {
        let sibling = jsonl.with_extension("checkout");
        std::fs::write(&sibling, bytes).unwrap();
        std::fs::rename(&sibling, jsonl).unwrap();
    }

    #[cfg(unix)]
    fn ino(meta: &std::fs::Metadata) -> u64 {
        use std::os::unix::fs::MetadataExt;
        meta.ino()
    }

    #[test]
    fn append_fd_writes_after_bytes_another_handle_appended() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        let (mut c, _tx) = open_coordinator(&root);

        let x = learning_with(
            &id_from_now(std::time::Duration::from_secs(60), false),
            "rust::other_handle",
            "written by another handle",
            CYCLE_NOW_SEC,
        );
        let mut other = OpenOptions::new()
            .append(true)
            .open(root.episodic_jsonl())
            .unwrap();
        other.write_all(jsonl_line(&x).as_bytes()).unwrap();
        drop(other);

        let y = learning_with(
            &placeholder_id(),
            "rust::coordinator",
            "written by the coordinator fd",
            CYCLE_NOW_SEC,
        );
        episodic::append(&mut c.file, &y).unwrap();

        let body = std::fs::read_to_string(root.episodic_jsonl()).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "both lines must be in the file; file holds:\n{body}"
        );
        for line in lines {
            let _: AgentLearning = serde_json::from_str(line).expect("each line parses");
        }
    }

    #[tokio::test]
    async fn minted_id_is_above_a_log_id_ahead_of_the_clock() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        std::fs::create_dir_all(root.episodic_dir()).unwrap();
        let seeded = id_from_now(std::time::Duration::from_secs(3600), true);
        std::fs::write(
            root.episodic_jsonl(),
            jsonl_line(&learning_with(
                &seeded,
                "rust::seed",
                "an hour ahead",
                CYCLE_NOW_SEC,
            )),
        )
        .unwrap();

        let (tx, handle) = spawn_coordinator(root.project_root(), &root.episodic_jsonl()).await;
        let minted = append_via(&tx, sample_learning()).await;
        shutdown(tx, handle).await;

        assert!(
            minted > seeded,
            "{minted} must sort above the seeded {seeded}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_cycle_reopens_the_append_fd() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        std::fs::create_dir_all(root.episodic_dir()).unwrap();
        std::fs::create_dir_all(root.semantic_dir()).unwrap();
        let mut body = String::new();
        for (tail, hours, content) in [
            ("FA0", 3, "unwrap panicked in the handler"),
            ("FA1", 2, "unwrap panicked in the parser"),
            ("FA2", 1, "unwrap panicked in the loader"),
        ] {
            let id = EventId::parse(&format!("evt_01ARZ3NDEKTSV4RRFFQ69G5{tail}")).unwrap();
            body.push_str(&jsonl_line(&learning_with(
                &id,
                "rust::eh",
                content,
                CYCLE_NOW_SEC - hours * 3600,
            )));
        }
        std::fs::write(root.episodic_jsonl(), body).unwrap();
        // A regular file where the ledger's directory goes: `record_cycle` fails
        // after the pin pass has already renamed the log.
        std::fs::create_dir_all(root.dreamd_dir()).unwrap();
        std::fs::write(root.provenance_dir(), b"not a directory").unwrap();

        let (mut c, _tx) = open_coordinator(&root);
        let before = ino(&c.file.metadata().unwrap());

        let result = c
            .handle_run_dream_cycle(CYCLE_NOW_SEC, "2026-05-28", true, false)
            .await;
        assert!(
            result.is_err(),
            "precondition: the cycle fails; got {result:?}"
        );
        let at_path = ino(&std::fs::metadata(root.episodic_jsonl()).unwrap());
        assert_ne!(
            at_path, before,
            "precondition: the pin pass renamed the log"
        );

        assert_eq!(
            ino(&c.file.metadata().unwrap()),
            at_path,
            "a failed cycle must still point the append fd at the file at the path"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn outside_replace_with_a_torn_tail_gets_a_newline_first() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        let jsonl = root.episodic_jsonl();
        let (tx, handle) = spawn_coordinator(root.project_root(), &jsonl).await;

        let valid = learning_with(&placeholder_id(), "git::merge", "merged in", CYCLE_NOW_SEC);
        let mut replacement = jsonl_line(&valid);
        replacement.push_str("{\"partial");
        replace_by_rename(&jsonl, replacement.as_bytes());

        let minted = append_via(&tx, sample_learning()).await;
        shutdown(tx, handle).await;

        let ids: Vec<EventId> = episodic::read_all(&jsonl)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, vec![valid.id, minted]);
        let body = std::fs::read_to_string(&jsonl).unwrap();
        assert!(body.contains("{\"partial\n"), "file holds:\n{body}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn outside_replace_raises_the_minter_floor() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        let jsonl = root.episodic_jsonl();
        let (tx, handle) = spawn_coordinator(root.project_root(), &jsonl).await;

        let ahead = id_from_now(std::time::Duration::from_secs(3600), true);
        replace_by_rename(
            &jsonl,
            jsonl_line(&learning_with(
                &ahead,
                "git::merge",
                "an hour ahead",
                CYCLE_NOW_SEC,
            ))
            .as_bytes(),
        );

        let minted = append_via(&tx, sample_learning()).await;
        shutdown(tx, handle).await;

        assert!(
            minted > ahead,
            "{minted} must sort above the merged-in {ahead}"
        );
    }

    /// Seed one record, open, write `a` straight through the fd, then replace
    /// the path with the seed bytes plus `extra`, and confirm.
    #[cfg(unix)]
    fn confirm_after_replace(include_a: bool) -> usize {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        let jsonl = root.episodic_jsonl();
        std::fs::create_dir_all(root.episodic_dir()).unwrap();
        let seed = learning_with(
            &id_from_now(std::time::Duration::from_secs(60), false),
            "rust::seed",
            "already there",
            CYCLE_NOW_SEC,
        );
        std::fs::write(&jsonl, jsonl_line(&seed)).unwrap();
        let (mut c, _tx) = open_coordinator(&root);

        let saved = std::fs::read(&jsonl).unwrap();
        let a = learning_with(
            &placeholder_id(),
            "rust::confirm",
            "acked append",
            CYCLE_NOW_SEC,
        );
        let w = episodic::append(&mut c.file, &a).unwrap();
        let mut replacement = saved;
        if include_a {
            replacement.extend_from_slice(jsonl_line(&a).as_bytes());
        }
        replace_by_rename(&jsonl, &replacement);
        c.confirm_landed(&a, w).unwrap();

        episodic::read_all(&jsonl)
            .unwrap()
            .iter()
            .filter(|r| r.id == a.id)
            .count()
    }

    #[cfg(unix)]
    #[test]
    fn replace_after_the_write_appends_the_line_again() {
        assert_eq!(confirm_after_replace(false), 1);
    }

    #[cfg(unix)]
    #[test]
    fn replace_after_the_write_does_not_duplicate_a_held_line() {
        assert_eq!(confirm_after_replace(true), 1);
    }
}
