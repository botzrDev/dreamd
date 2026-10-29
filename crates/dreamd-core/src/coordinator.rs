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

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use dreamd_protocol::{AgentLearning, EventId, RECORD_SCHEMA_VERSION};
use lru::LruCache;
use tokio::sync::{mpsc, oneshot};

use crate::consolidation::{LessonBodySource, SelectedLesson};
use crate::decay::DecayResult;
use crate::dream_cycle::{self, DreamCycleError};
use crate::episodic::{self, EpisodicError, RawLine};
use crate::id_minter::IdMinter;
use crate::layout::AgentRoot;
use crate::provenance::Ledger;
use crate::server::index_freshness::AppendMark;

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
    /// A dream cycle could not be prepared or committed
    /// ([`MemoryCoordinatorMsg::PrepareCycle`] /
    /// [`MemoryCoordinatorMsg::CommitCycle`], WEG-271). Carries the underlying
    /// error's display string — caller maps to HTTP 500.
    #[error("dream cycle: {0}")]
    DreamCycle(String),
    /// `PrepareCycle` refused: another cycle's ticket is alive, or `state.json`
    /// says `in_progress`. Caller maps to HTTP 409.
    #[error("dream cycle in progress")]
    CycleInProgress,
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
    /// Start a dream cycle (BZR-1241): read the log once, select the lesson, and
    /// keep both until `CommitCycle`. The caller composes the body with no actor
    /// involved, so appends keep landing during the model call. Refused with
    /// [`CoordinatorError::CycleInProgress`] while another cycle's ticket is alive
    /// or `state.json` says `in_progress`.
    ///
    /// `now_sec` is supplied by the caller (the HTTP handler) so the cycle stays
    /// wall-clock-free and deterministic in tests. The commit plans with the
    /// same value, so one cycle has one clock.
    PrepareCycle {
        now_sec: i64,
        response_tx: oneshot::Sender<Result<PreparedCycle, CoordinatorError>>,
    },
    /// Finish the cycle `ticket` names: plan over the lines Prepare read plus
    /// everything appended since, stage, commit, then reopen the append fd on
    /// every exit path (WEG-271).
    ///
    /// The commit can replace `AGENT_LEARNINGS.jsonl` by rename (the plan's
    /// pins or decay prune), which orphans the coordinator's long-lived fd.
    /// Committing in the actor lets it reopen `self.file` so subsequent appends
    /// land on the live inode.
    CommitCycle {
        ticket: CycleTicket,
        cycle_date: String,
        body: LessonBodySource,
        response_tx: oneshot::Sender<Result<DecayResult, CoordinatorError>>,
    },
    /// Gracefully drain the channel and exit the run loop.
    Shutdown { response_tx: oneshot::Sender<()> },
}

/// What `PrepareCycle` returns.
#[derive(Debug)]
pub struct PreparedCycle {
    /// The lesson to compose for. `None`: nothing promotes.
    pub selected: Option<SelectedLesson>,
    /// Hand it back in `CommitCycle`. Dropping it abandons the cycle.
    pub ticket: CycleTicket,
}

/// A prepared cycle that is still wanted. The coordinator keeps a `Weak` of
/// the lease: while a ticket is alive a second `PrepareCycle` is refused, and
/// once it drops (a crashed or cancelled caller) the next one goes ahead.
#[derive(Debug)]
pub struct CycleTicket {
    id: u64,
    _lease: Arc<()>,
}

/// The cycle between `PrepareCycle` and `CommitCycle`.
struct OpenCycle {
    id: u64,
    lease: Weak<()>,
    now_sec: i64,
    raw: Vec<RawLine>,
    composed_for: Option<String>,
    /// The identity and `outside_changes` the lines were read under. `None`
    /// when the read did not match `identity` (BZR-1241).
    read_under: Option<(LogIdentity, u64)>,
}

/// Single mutable owner of `AGENT_LEARNINGS.jsonl`.
pub struct MemoryCoordinator {
    file: File,
    /// Path to `AGENT_LEARNINGS.jsonl`. Needed to reopen `file` after a dream
    /// cycle, or anything else, replaces the file (WEG-271).
    jsonl_path: PathBuf,
    /// Issues this coordinator's event ids; seeded from the log at open.
    minter: IdMinter,
    /// Device and inode on Unix, volume serial and file index on Windows, plus
    /// the length of the file `file` writes, as of its last write. Compared
    /// with the path around every append.
    identity: LogIdentity,
    /// The project root this coordinator owns. The dream cycle (consolidation +
    /// decay) runs against this root. Distinct from `agent_root_key`, which is
    /// canonicalized for idempotency keying — do NOT use the key for the cycle.
    agent_root: AgentRoot,
    /// The project writer lock's ledger appender. `None` from `open_at`, which
    /// holds no lock; such a coordinator refuses to run a dream cycle.
    ledger: Option<Ledger>,
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
    indexer_tx: Option<mpsc::Sender<crate::server::tantivy_handle::IndexerMsg>>,
    /// Set when the log changed outside this coordinator; the next append
    /// sends `IndexerMsg::Resync` after its own `Append` (BZR-1236). The send
    /// itself runs only when an indexer is wired.
    resync_pending: bool,
    /// The dream cycle between `PrepareCycle` and `CommitCycle` (BZR-1241).
    open_cycle: Option<OpenCycle>,
    /// The id the next `PrepareCycle` gives its ticket.
    next_cycle_id: u64,
    /// How many times the log changed outside this coordinator. A cycle whose
    /// lines were read under an older count re-reads the log at commit.
    outside_changes: u64,
    /// The log's largest id and this coordinator's append count, shared with
    /// its `Supervisor` for `/health` (BZR-1243).
    append_mark: Arc<Mutex<AppendMark>>,
}

impl MemoryCoordinator {
    /// Open the JSONL at the given path, run malformed-tail-skip recovery,
    /// and return a coordinator ready to receive on `rx`.
    ///
    /// Recovery: any trailing bytes that do not deserialize as a complete
    /// `AgentLearning` are truncated. The last fully parseable line is the
    /// recovery point. Subsequent appends start from there.
    ///
    /// `ledger` is the project writer lock's appender (`WriterLock::ledger`);
    /// the dream cycle records its edges through it.
    pub fn open(
        agent_root: &AgentRoot,
        ledger: Ledger,
        rx: mpsc::Receiver<MemoryCoordinatorMsg>,
        indexer_tx: Option<mpsc::Sender<crate::server::tantivy_handle::IndexerMsg>>,
    ) -> std::io::Result<Self> {
        let path = agent_root.episodic_jsonl();
        let mut coordinator = Self::open_at(&path, agent_root.project_root(), rx, indexer_tx)?;
        coordinator.ledger = Some(ledger);
        Ok(coordinator)
    }

    /// Lower-level constructor used by tests that need to point at a
    /// scratch path independent of the layout module.
    pub fn open_at(
        jsonl_path: &Path,
        agent_root: &Path,
        rx: mpsc::Receiver<MemoryCoordinatorMsg>,
        indexer_tx: Option<mpsc::Sender<crate::server::tantivy_handle::IndexerMsg>>,
    ) -> std::io::Result<Self> {
        if let Some(parent) = jsonl_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Recovery runs on its own plain read/write fd, closed at the end of
        // this block, before the append fd opens. It only truncates a torn
        // fragment or ends a final record with `\n`.
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
        let append_mark = Arc::new(Mutex::new(AppendMark {
            tail_id: records.iter().map(|r| &r.id).max().cloned(),
            appended: 0,
        }));
        let file = open_append(jsonl_path)?;
        let identity = LogIdentity::of(jsonl_path)?;

        let agent_root_key =
            std::fs::canonicalize(agent_root).unwrap_or_else(|_| agent_root.to_path_buf());
        let cap = NonZeroUsize::new(IDEMPOTENCY_CAPACITY).expect("non-zero capacity");

        Ok(Self {
            file,
            jsonl_path: jsonl_path.to_path_buf(),
            minter,
            identity,
            agent_root: AgentRoot::new(agent_root),
            ledger: None,
            rx,
            agent_root_key,
            idempotency: LruCache::new(cap),
            indexer_tx,
            resync_pending: false,
            open_cycle: None,
            next_cycle_id: 0,
            outside_changes: 0,
            append_mark,
        })
    }

    /// The mark this coordinator keeps, for its `Supervisor` to read
    /// (BZR-1243). Taken before the coordinator is spawned.
    pub fn append_mark_handle(&self) -> Arc<Mutex<AppendMark>> {
        Arc::clone(&self.append_mark)
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
                MemoryCoordinatorMsg::PrepareCycle {
                    now_sec,
                    response_tx,
                } => {
                    // A dropped receiver hands the ticket back here, and
                    // dropping it frees the next PrepareCycle.
                    let _ = response_tx.send(self.handle_prepare_cycle(now_sec));
                }
                MemoryCoordinatorMsg::CommitCycle {
                    ticket,
                    cycle_date,
                    body,
                    response_tx,
                } => {
                    let result = self.handle_commit_cycle(ticket, &cycle_date, body);
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
        self.confirm_landed(&learning, written)?;
        // The line is durable. Ids are strictly increasing, so it is the
        // largest. Counted before the indexer can see it, so `/health` never
        // reads more committed appends than appends.
        {
            let mut mark = self.append_mark.lock().expect("append mark poisoned");
            mark.appended += 1;
            mark.tail_id = Some(event_id.clone());
        }

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
        // on this channel without that timeout, so it waits). A shed append
        // would stay out of recall until the next coverage pass (the next
        // open, or a resync), so backpressure is still what keeps live recall
        // whole. A `Closed` channel is still logged once and the sender
        // dropped so we stop retrying.
        self.route_to_indexer(&event_id, &learning).await;
        // The log changed outside this coordinator: it may hold ids the index
        // has never seen. Queued behind this append's `Append`.
        if std::mem::take(&mut self.resync_pending) {
            self.send_resync().await;
        }

        Ok(AppendOutcome {
            id: event_id,
            timestamp: stamped,
            deduplicated: false,
        })
    }

    /// BZR-1241: read the log once, select the lesson, and keep both as the
    /// open cycle until `CommitCycle`. No model call: the caller composes.
    ///
    /// Refused while another cycle's ticket is alive, and while `state.json`
    /// says `in_progress` (a failed cycle keeps that until the next lock
    /// acquisition, and a pending WAL comes with it).
    fn handle_prepare_cycle(&mut self, now_sec: i64) -> Result<PreparedCycle, CoordinatorError> {
        if self.ledger.is_none() {
            return Err(CoordinatorError::DreamCycle(
                "this coordinator was opened without the project writer lock".to_string(),
            ));
        }
        if self
            .open_cycle
            .as_ref()
            .is_some_and(|open| open.lease.strong_count() > 0)
        {
            return Err(CoordinatorError::CycleInProgress);
        }
        match dream_cycle::ensure_not_in_progress(&self.agent_root) {
            Ok(()) => {}
            Err(DreamCycleError::InProgress) => return Err(CoordinatorError::CycleInProgress),
            Err(e) => return Err(CoordinatorError::DreamCycle(e.to_string())),
        }

        self.ensure_live_log()?;
        let bytes = self.read_log()?;
        // Commit reads only the bytes appended after these, so they must be
        // exactly the file `file` writes, up to a line boundary.
        let read_under = (bytes.len() as u64 == self.identity.len
            && bytes.last().is_none_or(|b| *b == b'\n'))
        .then_some((self.identity, self.outside_changes));
        let raw = episodic::raw_lines(&bytes);
        let selected = dream_cycle::select(&raw, now_sec);

        let id = self.next_cycle_id;
        self.next_cycle_id += 1;
        let lease = Arc::new(());
        self.open_cycle = Some(OpenCycle {
            id,
            lease: Arc::downgrade(&lease),
            now_sec,
            raw,
            composed_for: selected.as_ref().map(|s| s.cluster_key.clone()),
            read_under,
        });
        Ok(PreparedCycle {
            selected,
            ticket: CycleTicket { id, _lease: lease },
        })
    }

    /// WEG-271 / BZR-1241: commit the cycle `ticket` names, then reopen the
    /// append fd.
    ///
    /// The plan covers the lines Prepare read plus every line appended since.
    /// If the log changed outside dreamd during the window, the whole log is
    /// read again and the body is the deterministic one. The plan rewrites the
    /// log when it pins a cited record (on cluster promotion) or prunes one,
    /// and the commit renames the staged log over `AGENT_LEARNINGS.jsonl`,
    /// leaving `self.file` pointing at an orphaned inode. It reopens on every
    /// exit path past the ticket check, error included, so the next append
    /// lands on the live inode whether or not the commit replaced the file and
    /// whether or not the cycle succeeded. The staged log is a well-formed
    /// file, so no `episodic::recover` scan is needed on reopen.
    fn handle_commit_cycle(
        &mut self,
        ticket: CycleTicket,
        cycle_date: &str,
        body: LessonBodySource,
    ) -> Result<DecayResult, CoordinatorError> {
        let Some(ledger) = self.ledger.clone() else {
            return Err(CoordinatorError::DreamCycle(
                "this coordinator was opened without the project writer lock".to_string(),
            ));
        };
        let Some(open) = self.open_cycle.take_if(|open| open.id == ticket.id) else {
            return Err(CoordinatorError::DreamCycle(
                "no prepared dream cycle for this ticket".to_string(),
            ));
        };
        let result = self.commit_open_cycle(open, &ledger, cycle_date, body);
        // Every exit path reopens: the commit can fail after its rename has
        // already replaced the log.
        let reopened = self.reopen();
        drop(ticket);
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

    /// The commit itself, without the reopen.
    fn commit_open_cycle(
        &mut self,
        mut open: OpenCycle,
        ledger: &Ledger,
        cycle_date: &str,
        body: LessonBodySource,
    ) -> Result<DecayResult, CoordinatorError> {
        let (raw, body) = match self.log_since(&mut open)? {
            Some(raw) => (raw, body),
            None => {
                tracing::warn!(
                    path = %self.jsonl_path.display(),
                    "the episodic log changed outside dreamd during the model call; \
                     writing the deterministic body"
                );
                // Handle the change the way an append would before reading:
                // end a foreign final line, raise the minter's floor, and
                // flag the resync.
                self.ensure_live_log()?;
                let bytes = self.read_log()?;
                (episodic::raw_lines(&bytes), LessonBodySource::Deterministic)
            }
        };
        let decay = dream_cycle::commit(
            &self.agent_root,
            ledger,
            open.now_sec,
            cycle_date,
            &raw,
            open.composed_for.as_deref(),
            body,
        )
        .map_err(|e| CoordinatorError::DreamCycle(e.to_string()))?;
        // The log the commit wrote is `raw` without the decayed records: the
        // plan drops by id and keeps every other record's id (`patch_lines`).
        let decayed: HashSet<&EventId> = decay.decayed_ids.iter().collect();
        let tail_id = raw
            .iter()
            .filter_map(|line| line.parsed.as_ref())
            .map(|record| &record.id)
            .filter(|id| !decayed.contains(id))
            .max()
            .cloned();
        self.append_mark
            .lock()
            .expect("append mark poisoned")
            .tail_id = tail_id;
        Ok(decay)
    }

    /// The whole log as of now, from `open`'s lines plus the bytes this
    /// coordinator appended since Prepare read them. `None` when the log
    /// changed any other way in between: an outside change was handled, the
    /// path holds another file or another length, the file is not the one
    /// Prepare read, or Prepare's read did not match what `file` wrote.
    fn log_since(&self, open: &mut OpenCycle) -> std::io::Result<Option<Vec<RawLine>>> {
        let Some((at, changes)) = open.read_under else {
            return Ok(None);
        };
        if changes != self.outside_changes
            || LogIdentity::at(&self.jsonl_path)? != Some(self.identity)
            || !self.identity.same_file(&at)
            || self.identity.len < at.len
        {
            return Ok(None);
        }
        let mut tail = vec![0u8; (self.identity.len - at.len) as usize];
        read_exact_at(&self.file, &mut tail, at.len)?;
        // Prepare's bytes end at a line boundary, so the two line lists
        // concatenate to the lines of the whole file.
        let mut raw = std::mem::take(&mut open.raw);
        raw.extend(episodic::raw_lines(&tail));
        Ok(Some(raw))
    }

    /// The bytes at `jsonl_path`; an absent file is empty.
    fn read_log(&self) -> std::io::Result<Vec<u8>> {
        match std::fs::read(&self.jsonl_path) {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Point `file` at whatever is at `jsonl_path` now.
    fn reopen(&mut self) -> std::io::Result<()> {
        self.file = open_append(&self.jsonl_path)?;
        self.identity = LogIdentity::of(&self.jsonl_path)?;
        Ok(())
    }

    /// Before a write: if the file at the path is not the one `file` last
    /// wrote, reopen it.
    fn ensure_live_log(&mut self) -> Result<(), CoordinatorError> {
        if LogIdentity::at(&self.jsonl_path)? != Some(self.identity) {
            self.reopen_after_external_change()?;
        }
        Ok(())
    }

    /// After a write of `written` bytes: if the path no longer shows that
    /// write, reopen it, and append `learning` again unless the file now at the
    /// path already holds its id.
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
    fn reopen_after_external_change(&mut self) -> Result<Vec<AgentLearning>, CoordinatorError> {
        self.reopen()?;
        let len = self.file.metadata()?.len();
        if len > 0 {
            let mut last = [0u8; 1];
            read_exact_at(&self.file, &mut last, len - 1)?;
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
        // The tail is the log as this coordinator last read it; `appended`
        // counts its own appends and stays (BZR-1243).
        self.append_mark
            .lock()
            .expect("append mark poisoned")
            .tail_id = records.iter().map(|r| &r.id).max().cloned();
        self.identity = LogIdentity::of(&self.jsonl_path)?;
        tracing::warn!(
            path = %self.jsonl_path.display(),
            "episodic log changed outside the coordinator; reopened it"
        );
        // This function is sync; `handle_append` sends the resync.
        self.resync_pending = true;
        self.outside_changes += 1;
        Ok(records)
    }

    /// Hand one durable append to the indexer task, **awaiting** the send.
    ///
    /// The await is the point. A bounded channel that is momentarily full must
    /// block this actor, not shed the message: a shed append would stay out of
    /// recall until the next coverage pass (the next open, or a resync), while
    /// later appends answer queries. Blocking here stops the coordinator
    /// reading its own inbox, so ingress degrades honestly instead of returning
    /// 201 for a record recall does not see: the in-flight learns already
    /// queued in the 256-slot coordinator channel just *wait* (no per-request
    /// timeout), and only overflow past that inbox meets the `Supervisor`'s
    /// separate 100 ms send timeout and becomes an HTTP 503. Sustained overload therefore surfaces as
    /// 503, a brief park as latency — and only on the HTTP ingress: the
    /// in-process `dreamd mcp` path sends on this channel without that timeout,
    /// so it waits.
    ///
    /// The append is acked once the indexer *accepts* the message; the Tantivy
    /// commit still lands on the usual 5 s cadence. A closed channel is terminal
    /// for live routing: warn once, drop the sender, and let the next open's
    /// coverage pass pick the append up.
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

    /// Ask the indexer to run its coverage pass, **awaiting** the send for the
    /// same reason [`Self::route_to_indexer`] does. A closed channel follows
    /// that function's rule.
    async fn send_resync(&mut self) {
        let Some(tx) = self.indexer_tx.as_ref() else {
            return;
        };
        let msg = crate::server::tantivy_handle::IndexerMsg::Resync { ack: None };
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

/// What the coordinator expects at `jsonl_path`. The indexer's tick compares
/// the same file and its length.
///
/// Unix stores the device and inode. Windows stores the volume serial and file
/// index from the `file-id` crate (`GetFileInformationByHandle` stays in that
/// crate). `extra` is the high 64 bits of a 128-bit Windows file id, and 0
/// everywhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LogIdentity {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    extra: u64,
    pub(crate) len: u64,
}

impl LogIdentity {
    /// The file at `path`, including its length.
    fn of(path: &Path) -> std::io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        Self::from_meta(path, &meta)
    }

    /// `None` when nothing is at `path`.
    pub(crate) fn at(path: &Path) -> std::io::Result<Option<Self>> {
        match std::fs::metadata(path) {
            Ok(meta) => Ok(Some(Self::from_meta(path, &meta)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Same file, ignoring length.
    pub(crate) fn same_file(&self, other: &Self) -> bool {
        self.dev == other.dev && self.ino == other.ino && self.extra == other.extra
    }

    #[cfg(unix)]
    fn from_meta(_path: &Path, meta: &std::fs::Metadata) -> std::io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            dev: meta.dev(),
            ino: meta.ino(),
            extra: 0,
            len: meta.len(),
        })
    }

    #[cfg(windows)]
    fn from_meta(path: &Path, meta: &std::fs::Metadata) -> std::io::Result<Self> {
        let id = file_id::get_file_id(path).map_err(std::io::Error::other)?;
        let (dev, ino, extra) = match id {
            file_id::FileId::LowRes {
                volume_serial_number,
                file_index,
            } => (u64::from(volume_serial_number), file_index, 0),
            file_id::FileId::HighRes {
                volume_serial_number,
                file_id,
            } => (volume_serial_number, file_id as u64, (file_id >> 64) as u64),
            file_id::FileId::Inode { .. } => {
                return Err(std::io::Error::other(
                    "file-id returned an inode id on Windows",
                ));
            }
        };
        Ok(Self {
            dev,
            ino,
            extra,
            len: meta.len(),
        })
    }
}

/// Read `buf.len()` bytes at `offset`. Unix uses `read_exact_at`. Windows uses
/// `seek_read`, which does not promise a full buffer.
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let n = file.seek_read(buf, offset)?;
        if n == buf.len() {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "short read of the episodic log",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer_lock::{WriterKind, WriterLock};
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
        let coordinator = MemoryCoordinator::open_at(jsonl, root, rx, None).expect("open coord");
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

    /// Open the way a writer does: `.agent/`, the project writer lock, then
    /// the coordinator with the lock's ledger. The lock is returned so it
    /// outlives the test.
    fn open_coordinator(
        root: &AgentRoot,
    ) -> (
        MemoryCoordinator,
        mpsc::Sender<MemoryCoordinatorMsg>,
        WriterLock,
    ) {
        std::fs::create_dir_all(root.episodic_dir()).expect("create .agent/");
        let lock = WriterLock::acquire(root, WriterKind::Watch).expect("project writer lock");
        let (tx, rx) = mpsc::channel::<MemoryCoordinatorMsg>(8);
        let mut c =
            MemoryCoordinator::open_at(&root.episodic_jsonl(), root.project_root(), rx, None)
                .expect("open coord");
        c.ledger = Some(lock.ledger().clone());
        (c, tx, lock)
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
        let (mut c, _tx, _lock) = open_coordinator(&root);

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

    #[tokio::test]
    async fn minted_id_is_above_a_terminated_final_record() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        std::fs::create_dir_all(root.episodic_dir()).unwrap();
        let seeded = id_from_now(std::time::Duration::from_secs(3600), true);
        let line = jsonl_line(&learning_with(
            &seeded,
            "rust::seed",
            "an hour ahead, no newline",
            CYCLE_NOW_SEC,
        ));
        std::fs::write(root.episodic_jsonl(), line.trim_end_matches('\n')).unwrap();

        let (tx, handle) = spawn_coordinator(root.project_root(), &root.episodic_jsonl()).await;
        let minted = append_via(&tx, sample_learning()).await;
        shutdown(tx, handle).await;

        assert!(
            minted > seeded,
            "{minted} must sort above the seeded {seeded}"
        );
        let records = episodic::read_all(&root.episodic_jsonl()).unwrap();
        let ids: Vec<&EventId> = records.iter().map(|r| &r.id).collect();
        assert_eq!(ids, vec![&seeded, &minted], "both records are in the log");
    }

    #[cfg(unix)]
    #[test]
    fn failed_cycle_reopens_the_append_fd() {
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
        // A regular file where the ledger's directory goes: the commit's ledger
        // append fails after the rename of the log.
        std::fs::create_dir_all(root.dreamd_dir()).unwrap();
        std::fs::write(root.provenance_dir(), b"not a directory").unwrap();

        let (mut c, _tx, _lock) = open_coordinator(&root);
        let before = ino(&c.file.metadata().unwrap());

        let prepared = c.handle_prepare_cycle(CYCLE_NOW_SEC).expect("prepare");
        let result = c.handle_commit_cycle(
            prepared.ticket,
            "2026-05-28",
            LessonBodySource::Deterministic,
        );
        assert!(
            result.is_err(),
            "precondition: the cycle fails; got {result:?}"
        );
        let at_path = ino(&std::fs::metadata(root.episodic_jsonl()).unwrap());
        assert_ne!(at_path, before, "precondition: the commit renamed the log");

        assert_eq!(
            ino(&c.file.metadata().unwrap()),
            at_path,
            "a failed cycle must still point the append fd at the file at the path"
        );
    }

    /// BZR-1243: the mark follows the log the coordinator read and wrote.
    #[cfg(unix)]
    #[tokio::test]
    async fn append_mark_tracks_open_appends_and_cycle_commits() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        std::fs::create_dir_all(root.episodic_dir()).unwrap();
        let id =
            |tail: &str| EventId::parse(&format!("evt_01ARZ3NDEKTSV4RRFFQ69G5{tail}")).unwrap();
        let (a, b, c) = (id("FA0"), id("FA1"), id("FA2"));
        // Out of id order; B is pinned and survives the cycle's decay.
        let mut body = String::new();
        for (record, pinned) in [(&a, false), (&c, false), (&b, true)] {
            let learning = AgentLearning {
                pinned,
                ..learning_with(record, "rust::eh", "mark fixture", CYCLE_NOW_SEC)
            };
            body.push_str(&jsonl_line(&learning));
        }
        std::fs::write(root.episodic_jsonl(), body).unwrap();

        let (mut coord, _tx, _lock) = open_coordinator(&root);
        let mark = coord.append_mark_handle();
        let read = || mark.lock().unwrap().clone();
        assert_eq!(
            read(),
            AppendMark {
                tail_id: Some(c.clone()),
                appended: 0
            }
        );

        let first = coord
            .handle_append(sample_learning(), Some("key".to_string()))
            .await
            .expect("append");
        assert!(!first.deduplicated);
        assert_eq!(
            read(),
            AppendMark {
                tail_id: Some(first.id.clone()),
                appended: 1
            }
        );

        let repeat = coord
            .handle_append(sample_learning(), Some("key".to_string()))
            .await
            .expect("dedup append");
        assert!(repeat.deduplicated, "precondition: a dedup-key hit");
        assert_eq!(
            read(),
            AppendMark {
                tail_id: Some(first.id.clone()),
                appended: 1
            },
            "a dedup-key hit is not an append"
        );

        // Ten years on: every unpinned record decays, and nothing promotes.
        let later = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 10 * 365 * 86_400;
        let prepared = coord.handle_prepare_cycle(later).expect("prepare");
        assert!(
            prepared.selected.is_none(),
            "precondition: nothing promotes"
        );
        let decay = coord
            .handle_commit_cycle(prepared.ticket, CYCLE_DATE, LessonBodySource::Deterministic)
            .expect("commit");
        assert_eq!(decay.kept_count, 1, "precondition: only B is left");
        assert_eq!(
            read(),
            AppendMark {
                tail_id: Some(b.clone()),
                appended: 1
            }
        );
    }

    /// BZR-1243: a re-read after an outside change moves the tail to the
    /// file's largest id and leaves `appended` alone.
    #[cfg(unix)]
    #[tokio::test]
    async fn append_mark_follows_a_reread_after_an_outside_change() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        std::fs::create_dir_all(root.episodic_dir()).unwrap();
        let (a, b) = (eh_id("FA0"), eh_id("FA1"));
        let line = |id: &EventId| {
            jsonl_line(&learning_with(
                id,
                "rust::eh",
                "mark fixture",
                CYCLE_NOW_SEC,
            ))
        };
        std::fs::write(root.episodic_jsonl(), line(&a) + &line(&b)).unwrap();

        let (c, tx, _lock) = open_coordinator(&root);
        let mark = c.append_mark_handle();
        let read = || mark.lock().unwrap().clone();
        let handle = tokio::spawn(c.run());
        let minted = append_via(&tx, sample_learning()).await;
        assert_eq!(
            read(),
            AppendMark {
                tail_id: Some(minted),
                appended: 1
            },
            "precondition: one append"
        );

        // A `git checkout` to an older log that holds only A.
        replace_by_rename(&root.episodic_jsonl(), line(&a).as_bytes());
        let (response_tx, response_rx) = oneshot::channel();
        tx.send(MemoryCoordinatorMsg::PrepareCycle {
            now_sec: CYCLE_NOW_SEC,
            response_tx,
        })
        .await
        .unwrap();
        drop(response_rx.await.unwrap().expect("prepare"));

        assert_eq!(
            read(),
            AppendMark {
                tail_id: Some(a),
                appended: 1
            },
            "the tail is the re-read log's largest id"
        );
        shutdown(tx, handle).await;
    }

    // ── BZR-1241: PrepareCycle / CommitCycle ─────────────────────────────────

    #[cfg(unix)]
    const CYCLE_DATE: &str = "2026-05-28";

    #[cfg(unix)]
    fn eh_id(tail: &str) -> EventId {
        EventId::parse(&format!("evt_01ARZ3NDEKTSV4RRFFQ69G5{tail}")).unwrap()
    }

    /// Three `rust::eh` records from the hour before [`CYCLE_NOW_SEC`]: the
    /// cluster promotes and FA2, the newest, is its exemplar. Returns the ids
    /// in log order.
    #[cfg(unix)]
    fn seed_eh_store(root: &AgentRoot) -> Vec<EventId> {
        std::fs::create_dir_all(root.episodic_dir()).unwrap();
        let mut body = String::new();
        let mut ids = Vec::new();
        for (tail, hours, content) in [
            ("FA0", 3, "unwrap panicked in the handler"),
            ("FA1", 2, "unwrap panicked in the parser"),
            ("FA2", 1, "unwrap panicked in the loader"),
        ] {
            let id = eh_id(tail);
            body.push_str(&jsonl_line(&learning_with(
                &id,
                "rust::eh",
                content,
                CYCLE_NOW_SEC - hours * 3600,
            )));
            ids.push(id);
        }
        std::fs::write(root.episodic_jsonl(), body).unwrap();
        ids
    }

    #[cfg(unix)]
    fn llm_body(content: &str, citations: &[&EventId]) -> LessonBodySource {
        LessonBodySource::Llm {
            content: content.to_string(),
            prompt_version: crate::llm::VERSIONED_PROMPT_ID.to_string(),
            citations: citations.iter().map(|id| id.as_str().to_string()).collect(),
        }
    }

    /// A record `handle_append` will stamp with the wall clock, which is after
    /// [`CYCLE_NOW_SEC`], so it counts in the cycle's windows at age 0.
    #[cfg(unix)]
    fn appended(skill: &str, content: &str, pain: f32, importance: f32) -> AgentLearning {
        AgentLearning {
            pain,
            importance,
            ..learning_with(&placeholder_id(), skill, content, CYCLE_NOW_SEC)
        }
    }

    #[cfg(unix)]
    fn read_lessons(root: &AgentRoot) -> crate::lessons::LessonsFile {
        crate::lessons::read_lessons_file(&root.lessons_md()).expect("LESSONS.md reads")
    }

    #[cfg(unix)]
    fn is_pinned(root: &AgentRoot, id: &EventId) -> bool {
        episodic::read_all(&root.episodic_jsonl())
            .unwrap()
            .iter()
            .find(|r| r.id == *id)
            .unwrap_or_else(|| panic!("{id} is in the log"))
            .pinned
    }

    /// Every path under `.agent/`, relative to it, with its bytes (`None` for a
    /// directory). The writer lock's stamp names its holder, so it is left out.
    #[cfg(unix)]
    fn agent_tree(root: &AgentRoot) -> std::collections::BTreeMap<PathBuf, Option<Vec<u8>>> {
        fn walk(
            base: &Path,
            dir: &Path,
            out: &mut std::collections::BTreeMap<PathBuf, Option<Vec<u8>>>,
        ) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                let rel = path.strip_prefix(base).unwrap().to_path_buf();
                if path.is_dir() {
                    out.insert(rel, None);
                    walk(base, &path, out);
                } else {
                    out.insert(rel, Some(std::fs::read(&path).unwrap()));
                }
            }
        }
        let base = root.agent_dir();
        let mut out = std::collections::BTreeMap::new();
        walk(&base, &base, &mut out);
        out.remove(Path::new(".dreamd/writer.lock"));
        out
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prepare_then_commit_matches_the_in_process_cycle() {
        let actor_dir = tempfile::tempdir().unwrap();
        let actor_root = AgentRoot::new(actor_dir.path());
        seed_eh_store(&actor_root);
        let in_process_dir = tempfile::tempdir().unwrap();
        let in_process_root = AgentRoot::new(in_process_dir.path());
        seed_eh_store(&in_process_root);

        let (mut c, _tx, _lock) = open_coordinator(&actor_root);
        let prepared = c.handle_prepare_cycle(CYCLE_NOW_SEC).expect("prepare");
        assert!(
            prepared.selected.is_some(),
            "precondition: the store promotes"
        );
        c.handle_commit_cycle(prepared.ticket, CYCLE_DATE, LessonBodySource::Deterministic)
            .expect("commit");

        let lock = WriterLock::acquire(&in_process_root, WriterKind::Dream).unwrap();
        crate::dream_cycle::run_filesystem_phases(
            &in_process_root,
            lock.ledger(),
            CYCLE_NOW_SEC,
            CYCLE_DATE,
            true,
            false,
        )
        .await
        .expect("in-process cycle");

        let actor = agent_tree(&actor_root);
        assert!(
            actor.contains_key(Path::new("semantic/LESSONS.md")),
            "precondition: the cycle wrote a lesson; tree: {:?}",
            actor.keys().collect::<Vec<_>>()
        );
        assert_eq!(actor, agent_tree(&in_process_root));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn appends_land_between_prepare_and_commit() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        let ids = seed_eh_store(&root);
        let (mut c, _tx, _lock) = open_coordinator(&root);

        let prepared = c.handle_prepare_cycle(CYCLE_NOW_SEC).expect("prepare");
        assert_eq!(
            prepared.selected.as_ref().map(|s| s.cluster_key.as_str()),
            Some("rust::eh"),
            "precondition: the store promotes rust::eh"
        );
        // While the model "runs": one record in the selected cluster (pain 1,
        // so FA2 stays the exemplar) and one elsewhere.
        c.handle_append(appended("rust::eh", "unwrap in the cache", 1.0, 7.0), None)
            .await
            .expect("append in the cluster");
        c.handle_append(appended("go::fmt", "gofmt on save", 6.0, 7.0), None)
            .await
            .expect("append elsewhere");
        let log = std::fs::read(root.episodic_jsonl()).unwrap();
        let appended_lines: Vec<u8> = episodic::raw_lines(&log)[3..]
            .iter()
            .flat_map(|line| line.bytes.clone())
            .collect();

        c.handle_commit_cycle(
            prepared.ticket,
            CYCLE_DATE,
            llm_body("Return `?` instead of unwrapping.", &[&ids[0], &ids[1]]),
        )
        .expect("commit");

        let lessons = read_lessons(&root);
        assert_eq!(lessons.prompt_version, crate::llm::VERSIONED_PROMPT_ID);
        assert_eq!(
            lessons.lessons[0].content,
            "Return `?` instead of unwrapping."
        );
        let after = std::fs::read(root.episodic_jsonl()).unwrap();
        assert!(
            after.ends_with(&appended_lines),
            "the new log must end with the two appended lines; log:\n{}",
            String::from_utf8_lossy(&after)
        );
        assert!(is_pinned(&root, &ids[0]), "cited record FA0 is pinned");
        assert!(is_pinned(&root, &ids[1]), "cited record FA1 is pinned");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_second_prepare_while_a_ticket_is_live_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        seed_eh_store(&root);
        let (c, tx, _lock) = open_coordinator(&root);

        // Both are queued before the actor reads its inbox.
        let (first_tx, first_rx) = oneshot::channel();
        let (second_tx, second_rx) = oneshot::channel();
        for response_tx in [first_tx, second_tx] {
            tx.send(MemoryCoordinatorMsg::PrepareCycle {
                now_sec: CYCLE_NOW_SEC,
                response_tx,
            })
            .await
            .unwrap();
        }
        let handle = tokio::spawn(c.run());

        let first = first_rx.await.unwrap();
        let second = second_rx.await.unwrap();
        assert!(first.is_ok(), "the first prepare gets a ticket: {first:?}");
        assert!(
            matches!(second, Err(CoordinatorError::CycleInProgress)),
            "the second prepare is refused while the first ticket is alive: {second:?}"
        );
        drop(first);
        shutdown(tx, handle).await;
    }

    #[cfg(unix)]
    #[test]
    fn dropping_the_ticket_frees_the_next_prepare() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        seed_eh_store(&root);
        let (mut c, _tx, _lock) = open_coordinator(&root);

        let first = c
            .handle_prepare_cycle(CYCLE_NOW_SEC)
            .expect("first prepare");
        drop(first);
        let second = c.handle_prepare_cycle(CYCLE_NOW_SEC);
        assert!(
            second.is_ok(),
            "a dropped ticket frees the next prepare: {second:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn prepare_refuses_while_state_json_says_in_progress() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        seed_eh_store(&root);
        let (mut c, _tx, _lock) = open_coordinator(&root);
        // Written while the lock is held, so nothing resets it.
        std::fs::write(
            root.state_json(),
            b"{\"schema_version\":\"1.0\",\"last_dream_cycle_status\":\"in_progress\"}\n",
        )
        .unwrap();

        let result = c.handle_prepare_cycle(CYCLE_NOW_SEC);
        assert!(
            matches!(result, Err(CoordinatorError::CycleInProgress)),
            "got {result:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn commit_after_an_outside_replace_writes_the_deterministic_body() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        let ids = seed_eh_store(&root);
        let (mut c, _tx, _lock) = open_coordinator(&root);

        let prepared = c.handle_prepare_cycle(CYCLE_NOW_SEC).expect("prepare");
        // A `git checkout` while the model runs: the same records plus one
        // more in the selected cluster.
        let extra = eh_id("FA3");
        let mut replacement = std::fs::read(root.episodic_jsonl()).unwrap();
        replacement.extend_from_slice(
            jsonl_line(&learning_with(
                &extra,
                "rust::eh",
                "unwrap panicked in the merge",
                CYCLE_NOW_SEC - 4 * 3600,
            ))
            .as_bytes(),
        );
        replace_by_rename(&root.episodic_jsonl(), &replacement);

        // The body would pass the gate: same cluster, cited ids still members.
        c.handle_commit_cycle(
            prepared.ticket,
            CYCLE_DATE,
            llm_body("Composed before the checkout.", &[&ids[0], &ids[1]]),
        )
        .expect("commit");

        assert_eq!(
            read_lessons(&root).prompt_version,
            crate::consolidation::DETERMINISTIC_PROMPT_ID
        );
        let after: Vec<EventId> = episodic::read_all(&root.episodic_jsonl())
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert!(
            after.contains(&extra),
            "the new log holds the replacement's extra record: {after:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn commit_after_an_in_place_rewrite_and_an_append_writes_the_deterministic_body() {
        use std::io::Write;
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        let ids = seed_eh_store(&root);
        let (mut c, _tx, _lock) = open_coordinator(&root);
        let path = root.episodic_jsonl();

        let prepared = c.handle_prepare_cycle(CYCLE_NOW_SEC).expect("prepare");
        let before = std::fs::metadata(&path).unwrap();
        // While the model runs, an outside writer rewrites the log in place:
        // FA0 is gone and FX1 and FX2 are new, so the file is longer than
        // what Prepare read.
        let x1 = eh_id("FX1");
        let x2 = eh_id("FX2");
        let mut rewrite = String::new();
        for (id, content, hours) in [
            (&ids[1], "unwrap panicked in the parser", 2),
            (&ids[2], "unwrap panicked in the loader", 1),
            (&x1, "outside one, a longer line than the others", 1),
            (&x2, "outside two", 1),
        ] {
            rewrite.push_str(&jsonl_line(&learning_with(
                id,
                "rust::eh",
                content,
                CYCLE_NOW_SEC - hours * 3600,
            )));
        }
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&path)
                .unwrap();
            f.write_all(rewrite.as_bytes()).unwrap();
            f.sync_all().unwrap();
        }
        let rewritten = std::fs::metadata(&path).unwrap();
        assert_eq!(
            (rewritten.dev(), rewritten.ino()),
            (before.dev(), before.ino()),
            "precondition: the rewrite keeps the inode"
        );
        assert!(
            rewritten.len() > before.len(),
            "precondition: the rewrite is longer than what Prepare read"
        );

        // An append handles the outside change before the commit, so the
        // path, the inode and the length all match the actor's fd again. Only
        // the outside-change count tells Commit the prepared lines are stale.
        let added = c
            .handle_append(appended("go::fmt", "gofmt on save", 6.0, 7.0), None)
            .await
            .expect("append")
            .id;
        c.handle_commit_cycle(
            prepared.ticket,
            CYCLE_DATE,
            llm_body("Composed before the rewrite.", &[&ids[1], &ids[2]]),
        )
        .expect("commit");

        let after: Vec<EventId> = episodic::read_all(&path)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(
            after,
            vec![ids[1].clone(), ids[2].clone(), x1, x2, added],
            "the commit plans over the log as it is now"
        );
        assert_eq!(
            read_lessons(&root).prompt_version,
            crate::consolidation::DETERMINISTIC_PROMPT_ID
        );
    }

    #[cfg(unix)]
    #[test]
    fn commit_with_another_ticket_is_refused_and_keeps_the_open_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        seed_eh_store(&root);
        let (mut c, _tx, _lock) = open_coordinator(&root);

        let prepared = c.handle_prepare_cycle(CYCLE_NOW_SEC).expect("prepare");
        let other = CycleTicket {
            id: prepared.ticket.id + 1,
            _lease: Arc::new(()),
        };
        let refused = c.handle_commit_cycle(other, CYCLE_DATE, LessonBodySource::Deterministic);
        assert!(
            refused.is_err(),
            "another ticket must not commit this cycle: {refused:?}"
        );

        let committed =
            c.handle_commit_cycle(prepared.ticket, CYCLE_DATE, LessonBodySource::Deterministic);
        assert!(
            committed.is_ok(),
            "the real ticket still commits: {committed:?}"
        );
        assert!(root.lessons_md().exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_autosnap_holds_records_appended_during_the_compose() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        seed_eh_store(&root);
        let (mut c, _tx, _lock) = open_coordinator(&root);

        let prepared = c.handle_prepare_cycle(CYCLE_NOW_SEC).expect("prepare");
        let x = c
            .handle_append(appended("go::fmt", "landed mid-compose", 6.0, 7.0), None)
            .await
            .expect("append")
            .id;
        c.handle_commit_cycle(prepared.ticket, CYCLE_DATE, LessonBodySource::Deterministic)
            .expect("commit");

        let objects: Vec<PathBuf> = std::fs::read_dir(root.branches_objects_dir())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(objects.len(), 1, "one autosnap object: {objects:?}");
        let snap_log =
            std::fs::read_to_string(objects[0].join("episodic/AGENT_LEARNINGS.jsonl")).unwrap();
        assert!(
            snap_log.contains(x.as_str()),
            "the autosnap's log holds {x}; it holds:\n{snap_log}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_does_not_wait_behind_a_prepared_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        seed_eh_store(&root);
        let (c, tx, _lock) = open_coordinator(&root);
        let handle = tokio::spawn(c.run());

        let (prepare_tx, prepare_rx) = oneshot::channel();
        tx.send(MemoryCoordinatorMsg::PrepareCycle {
            now_sec: CYCLE_NOW_SEC,
            response_tx: prepare_tx,
        })
        .await
        .unwrap();
        let prepared = prepare_rx.await.unwrap().expect("prepare");

        let (sh_tx, sh_rx) = oneshot::channel();
        tx.send(MemoryCoordinatorMsg::Shutdown { response_tx: sh_tx })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), sh_rx)
            .await
            .expect("Shutdown acks within 1 s while a cycle is prepared")
            .expect("shutdown ack");

        let (commit_tx, _commit_rx) = oneshot::channel();
        let sent = tx
            .send(MemoryCoordinatorMsg::CommitCycle {
                ticket: prepared.ticket,
                cycle_date: CYCLE_DATE.to_string(),
                body: LessonBodySource::Deterministic,
                response_tx: commit_tx,
            })
            .await;
        assert!(sent.is_err(), "a commit after shutdown cannot be sent");
        handle.await.expect("coordinator joined");

        assert_ne!(
            crate::wal::read_last_cycle_status(&root).unwrap(),
            "in_progress"
        );
        let stage_root = root.dreamd_dir().join("stage");
        assert!(
            !stage_root.exists() || std::fs::read_dir(&stage_root).unwrap().next().is_none(),
            "no stage was opened"
        );
    }

    #[cfg(unix)]
    #[test]
    fn prepare_without_the_writer_lock_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        seed_eh_store(&root);
        let (_tx, rx) = mpsc::channel::<MemoryCoordinatorMsg>(8);
        let mut c =
            MemoryCoordinator::open_at(&root.episodic_jsonl(), root.project_root(), rx, None)
                .expect("open coord");

        let result = c.handle_prepare_cycle(CYCLE_NOW_SEC);
        assert!(result.is_err(), "got {result:?}");
        assert!(c.open_cycle.is_none(), "a refused prepare keeps no cycle");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn commit_gates_the_body_against_the_commit_selection() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        let a_ids = seed_eh_store(&root);
        let (mut c, _tx, _lock) = open_coordinator(&root);

        let prepared = c.handle_prepare_cycle(CYCLE_NOW_SEC).expect("prepare");
        assert_eq!(
            prepared.selected.as_ref().map(|s| s.cluster_key.as_str()),
            Some("rust::eh"),
            "precondition: A is the top cluster at prepare"
        );
        // While the model runs, cluster B lands with a larger salience sum.
        let mut b_ids = Vec::new();
        for content in ["open failed", "read failed", "write failed", "seek failed"] {
            let id = c
                .handle_append(appended("py::io", content, 9.0, 9.0), None)
                .await
                .expect("append")
                .id;
            b_ids.push(id);
        }
        let log = std::fs::read(root.episodic_jsonl()).unwrap();
        let commit_selection = crate::consolidation::select_lesson(
            &crate::consolidation::compute_promoted_clusters(
                &crate::plan::records(&episodic::raw_lines(&log)),
                CYCLE_NOW_SEC,
            ),
            CYCLE_NOW_SEC,
        )
        .expect("the log promotes");
        assert_eq!(
            commit_selection.cluster_key, "py::io",
            "precondition: B is the top cluster at commit"
        );

        c.handle_commit_cycle(
            prepared.ticket,
            CYCLE_DATE,
            llm_body("A lesson about rust::eh.", &[&a_ids[0], &a_ids[1]]),
        )
        .expect("commit");

        let lessons = read_lessons(&root);
        assert_eq!(lessons.cluster_key, "py::io");
        assert_eq!(
            lessons.prompt_version,
            crate::consolidation::DETERMINISTIC_PROMPT_ID
        );
        assert_eq!(
            lessons.citations,
            vec![commit_selection.exemplar_id.clone()]
        );
        assert!(
            b_ids
                .iter()
                .any(|id| id.as_str() == commit_selection.exemplar_id),
            "B's exemplar is one of B's records"
        );
        for id in &a_ids {
            assert!(!is_pinned(&root, id), "{id} (A) must not be pinned");
        }
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
        let (mut c, _tx, _lock) = open_coordinator(&root);

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
