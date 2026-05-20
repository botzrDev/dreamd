//! Writer-process lifecycle (WEG-21 / DR-118).
//!
//! Two layers live here:
//!
//! 1. [`detach_double_fork`] — the Unix `fork() → setsid() → fork()` daemon
//!    pattern. The first fork escapes the parent's process group; `setsid`
//!    makes the child a session leader; the second fork ensures the final
//!    process is not a session leader and therefore can never acquire a
//!    controlling terminal. Called only in the production `run()` path —
//!    tests exercise the supervisor without detachment.
//!
//! 2. [`Supervisor`] — owns the [`MemoryCoordinator`] task handle plus all
//!    senders into the actor channel. Shutdown-drain contract (decision
//!    2026-05-14, option a): the lifecycle layer guarantees no append is in
//!    flight when `Shutdown` is sent. We do this by dropping every sender
//!    before sending `Shutdown` on the supervisor's retained sender.
//!    `Shutdown` is terminal — any further send on a closed channel returns
//!    a typed `SendError`, never a silent drop. `MemoryCoordinator::run()`
//!    is NOT modified for this ticket; the invariant lives entirely in the
//!    supervisor.

use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::coordinator::{MemoryCoordinator, MemoryCoordinatorMsg};
use crate::index::{check_manifest_version, ManifestCheckOutcome, INDEX_MANIFEST_FILENAME};
use crate::layout::{AgentRoot, DaemonHome};
use crate::server::tantivy_handle::IndexerMsg;

/// Default bound for the coordinator mpsc channel (architecture decision C14).
/// Revisit after load-testing in DR-208 / DR-808.
pub const COORDINATOR_CHANNEL_CAPACITY: usize = 256;

/// Timeout for non-blocking coordinator sends from API handlers.
/// On expiry the caller returns 503 Service Unavailable + Retry-After: 1.
pub const COORDINATOR_SEND_TIMEOUT: Duration = Duration::from_millis(100);

/// Error returned by [`Supervisor::try_send`] when the coordinator channel
/// is full or has closed. Pattern-matched by Axum handlers (WEG-67+) to
/// produce the correct HTTP response.
#[derive(Debug, PartialEq, Eq)]
pub enum CoordinatorSendError {
    /// Channel buffer was full when the timeout expired. → 503 Retry-After:1
    Full,
    /// Coordinator task has exited; daemon is shutting down. → 503 no retry
    Closed,
}

/// Failure modes surfaced by the `server::run` entry point.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("UDS bind failed: {0}")]
    UdsBind(#[from] crate::server::uds::UdsBindError),
    #[error("coordinator open failed: {0}")]
    Coordinator(std::io::Error),
    #[error("double-fork failed: {0}")]
    Fork(String),
    #[error("other I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("index manifest check failed: {0}")]
    ManifestCheck(#[from] crate::index::ManifestVersionError),
}

/// Wiring config for the writer-process. `agent_root` selects the per-project
/// JSONL; `daemon_home` selects the per-user UDS path. The pair lets
/// `npx dreamd-mcp` resolve both without a separate config file.
#[derive(Clone)]
pub struct ServerConfig {
    pub agent_root: AgentRoot,
    pub daemon_home: DaemonHome,
    /// Channel buffer between the UDS accept loop and the coordinator. A
    /// small bound is enough — a single client RTT writes one message.
    pub coordinator_channel_capacity: usize,
    /// WEG-42 indexer hand-off. When `Some`, every successful coordinator
    /// append `try_send`s an [`IndexerMsg::Append`] on this channel. The
    /// caller (`server::run` entry point) constructs a `TantivyIndexHandle`,
    /// extracts its sender via [`crate::server::TantivyIndexHandle::sender`],
    /// and threads it here.
    pub indexer_tx: Option<mpsc::Sender<IndexerMsg>>,
}

impl ServerConfig {
    pub fn new(agent_root: AgentRoot, daemon_home: DaemonHome) -> Self {
        Self {
            agent_root,
            daemon_home,
            coordinator_channel_capacity: COORDINATOR_CHANNEL_CAPACITY,
            indexer_tx: None,
        }
    }

    pub fn socket_path(&self) -> PathBuf {
        self.daemon_home.socket_path()
    }
}

/// Supervisor: owns the coordinator task + the canonical sender into its
/// channel. Lifecycle order (assembled by `Supervisor::start`):
///
///   1. Open `AGENT_LEARNINGS.jsonl` through `MemoryCoordinator::open`.
///   2. Spawn the coordinator task.
///   3. Retain one sender on the supervisor itself.
///
/// On shutdown, the supervisor must be the LAST owner of any sender. After
/// dropping its `tx`, sending `Shutdown` on a retained handle requires
/// constructing it before the drop — see [`Supervisor::shutdown`].
pub struct Supervisor {
    /// Primary sender retained by the supervisor. `clone()` to hand out
    /// per-connection senders; drop them all before [`Supervisor::shutdown`].
    tx: mpsc::Sender<MemoryCoordinatorMsg>,
    handle: tokio::task::JoinHandle<()>,
}

impl Supervisor {
    /// Boot a coordinator under `agent_root`, return a supervisor that owns
    /// the sole sender. The coordinator runs on the current tokio runtime.
    ///
    /// WEG-49 (DR-210): before any coordinator state is opened, read the
    /// per-project index manifest and compare its `schema_version` to the
    /// binary's [`crate::index::SCHEMA_VERSION`]. A manifest newer than the
    /// binary aborts startup via [`ServerError::ManifestCheck`]; older or
    /// absent manifests log a `tracing::warn!` and proceed.
    pub fn start(
        agent_root: &AgentRoot,
        channel_capacity: usize,
        indexer_tx: Option<mpsc::Sender<IndexerMsg>>,
    ) -> Result<Self, ServerError> {
        let manifest_path = agent_root.dreamd_dir().join(INDEX_MANIFEST_FILENAME);
        match check_manifest_version(&manifest_path)? {
            ManifestCheckOutcome::Absent => {
                tracing::warn!(
                    path = ?manifest_path,
                    "no index manifest found; treating project as unindexed"
                );
            }
            ManifestCheckOutcome::Current => {}
            ManifestCheckOutcome::NeedsMigration { from } => {
                tracing::warn!(
                    from,
                    binary = crate::index::SCHEMA_VERSION,
                    "index schema predates binary; run `dreamd migrate` to upgrade"
                );
            }
        }

        let (tx, rx) = mpsc::channel::<MemoryCoordinatorMsg>(channel_capacity);
        let coordinator = MemoryCoordinator::open(agent_root, rx, indexer_tx)
            .map_err(ServerError::Coordinator)?;
        let handle = tokio::spawn(coordinator.run());
        Ok(Self { tx, handle })
    }

    /// Clone a sender for a UDS client connection. The supervisor retains its
    /// own copy on `self.tx`; the shutdown drain depends on that retention.
    pub fn sender(&self) -> mpsc::Sender<MemoryCoordinatorMsg> {
        self.tx.clone()
    }

    /// Non-blocking coordinator send with a [`COORDINATOR_SEND_TIMEOUT`] timeout.
    ///
    /// Returns `Ok(())` on success. Returns `Err(CoordinatorSendError::Full)` if
    /// the channel buffer is still full after the timeout; returns
    /// `Err(CoordinatorSendError::Closed)` if the coordinator has exited.
    ///
    /// Axum handlers (WEG-67+) call this instead of cloning `tx` directly —
    /// the method is the single enforcement point for the backpressure contract.
    pub async fn try_send(
        &self,
        msg: MemoryCoordinatorMsg,
    ) -> Result<(), CoordinatorSendError> {
        match tokio::time::timeout(COORDINATOR_SEND_TIMEOUT, self.tx.send(msg)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(CoordinatorSendError::Closed),
            Err(_elapsed) => Err(CoordinatorSendError::Full),
        }
    }

    /// Test-only constructor: returns a `Supervisor` whose channel has capacity 1
    /// and whose receiver is handed back to the caller. The coordinator task is
    /// replaced with a no-op handle. Use this to test `try_send` behaviour
    /// without starting a real `MemoryCoordinator`.
    #[cfg(test)]
    pub fn for_backpressure_test() -> (Self, mpsc::Receiver<MemoryCoordinatorMsg>) {
        let (tx, rx) = mpsc::channel::<MemoryCoordinatorMsg>(1);
        let handle = tokio::spawn(async {});
        (Self { tx, handle }, rx)
    }

    /// Drain and shut down the coordinator cleanly.
    ///
    /// Contract: the caller MUST drop every supervisor-issued sender clone
    /// before calling `shutdown` — typically by joining all client tasks
    /// first. The supervisor's own `tx` is consumed here; combined with the
    /// caller's drops, the coordinator's `rx.recv().await` returns `None`
    /// only AFTER our `Shutdown` lands, so the actor processes every queued
    /// append before exiting.
    ///
    /// Implementation detail: we send `Shutdown` BEFORE the final drop of
    /// `self.tx` because `Shutdown` itself rides over the channel. Once
    /// `Shutdown` is consumed inside `MemoryCoordinator::run`, it closes
    /// its `rx` and breaks the loop. Subsequent sends on cloned senders
    /// would return a typed `mpsc::error::SendError` — see
    /// [`SupervisorSendError`].
    pub async fn shutdown(self) {
        let (sh_tx, sh_rx) = oneshot::channel();
        // If the coordinator already exited (e.g., panicked), the send
        // returns an error; swallow it because there's nothing to drain.
        let _ = self
            .tx
            .send(MemoryCoordinatorMsg::Shutdown { response_tx: sh_tx })
            .await;
        let _ = sh_rx.await;
        drop(self.tx);
        let _ = self.handle.await;
    }

    /// Test-only helper: synchronously wait on the join handle without
    /// triggering shutdown. Useful for tests that drive shutdown via
    /// dropped senders + an external `Shutdown` send.
    #[cfg(test)]
    pub async fn join_after_shutdown_elsewhere(self) {
        drop(self.tx);
        let _ = self.handle.await;
    }
}

/// Newtype wrapper around `tokio::sync::mpsc::error::SendError` so the
/// supervisor's contract surface is independent of the coordinator's
/// internal message type. Callers can match on this to distinguish "channel
/// closed because Shutdown landed" from other I/O failures.
#[derive(Debug, thiserror::Error)]
#[error("memory coordinator channel closed (post-Shutdown send is rejected)")]
pub struct SupervisorSendError;

impl<T> From<mpsc::error::SendError<T>> for SupervisorSendError {
    fn from(_: mpsc::error::SendError<T>) -> Self {
        Self
    }
}

/// Run the Unix double-fork dance and return whether the caller is the
/// final detached writer-process (`Ok(true)`) or the original launcher that
/// should exit immediately (`Ok(false)`).
///
/// Pattern:
///   * First `fork()`: parent exits with status `0` so the shell prompt
///     returns; the child continues.
///   * `setsid()`: detach from the controlling TTY by becoming a session
///     leader.
///   * Second `fork()`: the session leader exits, leaving the final child
///     as a non-session-leader that can never re-acquire a TTY.
///
/// **Must be called before any tokio runtime is constructed.** The
/// intermediate session-leader process exits via `std::process::exit(0)`,
/// which skips Drop handlers — any live runtime worker threads would be
/// torn down without orderly shutdown.
///
/// Unix-only. Windows is DR-121 / WEG-135.
#[cfg(unix)]
pub fn detach_double_fork() -> Result<bool, ServerError> {
    use nix::unistd::{fork, setsid, ForkResult};

    // SAFETY: nix::unistd::fork is `unsafe` because async-signal-safety
    // demands the child do nothing but exec/exit until it has reset
    // signal handlers and threads. We satisfy that here: the child path
    // does only `setsid()` and a second `fork()`, both async-signal-safe,
    // before returning into the supervisor.
    //
    // Workspace forbids `unsafe_code`, so we wrap the unsafe call in an
    // `unsafe { ... }` block that is scoped to this function only. To keep
    // the workspace-wide forbid in force everywhere else, we localize the
    // override here. NOTE: if `unsafe_code = "forbid"` is ever escalated
    // without an explicit allow on this fn, the build will fail loudly —
    // exactly what we want.
    #[allow(unsafe_code)]
    let first = unsafe { fork() }.map_err(|e| ServerError::Fork(format!("first fork: {e}")))?;
    match first {
        ForkResult::Parent { .. } => return Ok(false),
        ForkResult::Child => {}
    }

    setsid().map_err(|e| ServerError::Fork(format!("setsid: {e}")))?;

    #[allow(unsafe_code)]
    let second = unsafe { fork() }.map_err(|e| ServerError::Fork(format!("second fork: {e}")))?;
    match second {
        ForkResult::Parent { .. } => {
            // Session leader exits; let the kernel reap the final child via
            // its grandparent (init / launchd / systemd).
            std::process::exit(0);
        }
        ForkResult::Child => Ok(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::MemoryCoordinatorMsg;
    use chrono::{DateTime, Utc};
    use dreamd_protocol::{AgentLearning, EventId};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::oneshot;

    const SAMPLE_ULID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    fn unique_tmp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("dreamd-supervisor-{tag}-{nanos}"));
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
            timestamp: DateTime::parse_from_rfc3339("2026-05-14T08:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            pain: 5.0,
            importance: 5.0,
            pinned: false,
            skill_action: "rust.supervisor.test".to_string(),
            source_harness: "test-harness".to_string(),
            content: "supervisor drain body".to_string(),
        }
    }

    #[tokio::test]
    async fn shutdown_drain_persists_in_flight_append_before_exit() {
        // AC: AppendLearning + Shutdown in tight sequence — the append must
        // land on disk and the coordinator must exit cleanly. We force the
        // drain path by queuing the AppendLearning, immediately driving
        // shutdown WITHOUT awaiting the append's oneshot first, and only
        // then reading the append response. If the channel were empty when
        // Shutdown was sent (the old test's bug), this would not test drain.
        let dir = unique_tmp_dir("drain");
        let agent_root = AgentRoot::new(&dir);
        std::fs::create_dir_all(agent_root.episodic_dir()).unwrap();

        let supervisor = Supervisor::start(&agent_root, 8, None).expect("start supervisor");
        let client_tx = supervisor.sender();

        // Queue an append from a "client" sender, then drop it so the
        // supervisor's tx is the only remaining sender. The supervisor's
        // own retained tx is what `shutdown()` uses to enqueue Shutdown —
        // guaranteed to land AFTER our AppendLearning in FIFO order.
        let (resp_tx, resp_rx) = oneshot::channel();
        client_tx
            .send(MemoryCoordinatorMsg::AppendLearning {
                learning: sample_learning(),
                client_dedup_key: None,
                response_tx: resp_tx,
            })
            .await
            .expect("send append");
        drop(client_tx);

        // Critical: do NOT await `resp_rx` here. Drive Shutdown immediately
        // so AppendLearning and Shutdown are both queued, with AppendLearning
        // first. This is the drain invariant under test.
        supervisor.shutdown().await;

        // After shutdown returns, the coordinator has processed both
        // messages and exited. The append's oneshot must already be
        // resolved with Ok — proving the queued message drained before the
        // Shutdown arm broke the loop.
        let minted = resp_rx
            .await
            .expect("append oneshot must be resolved by drain")
            .expect("append must succeed under drain");
        assert!(minted.as_str().starts_with("evt_"));

        // File on disk has exactly one line.
        let raw = std::fs::read_to_string(agent_root.episodic_jsonl()).expect("read jsonl");
        let lines: Vec<&str> = raw.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            lines.len(),
            1,
            "drained append must land on disk before coordinator exit"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn send_on_closed_channel_returns_typed_send_error() {
        // AC: post-Shutdown send returns SendError, not a silent drop.
        let dir = unique_tmp_dir("closed");
        let agent_root = AgentRoot::new(&dir);
        std::fs::create_dir_all(agent_root.episodic_dir()).unwrap();

        let supervisor = Supervisor::start(&agent_root, 4, None).expect("start supervisor");
        let stale_tx = supervisor.sender();
        supervisor.shutdown().await;

        // After shutdown, the coordinator task has exited; rx is dropped.
        let (resp_tx, _resp_rx) = oneshot::channel();
        let send_result = stale_tx
            .send(MemoryCoordinatorMsg::AppendLearning {
                learning: sample_learning(),
                client_dedup_key: None,
                response_tx: resp_tx,
            })
            .await;

        // Map into the supervisor's typed error so the contract matches the
        // wire-facing surface.
        let typed: Result<(), SupervisorSendError> = send_result.map_err(SupervisorSendError::from);
        assert!(
            matches!(typed, Err(SupervisorSendError)),
            "post-Shutdown send must surface a typed SendError, not silent drop"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn try_send_full_channel_returns_full() {
        // Capacity-1 channel, receiver held but never polled → sends block.
        let (supervisor, _rx) = Supervisor::for_backpressure_test();
        // Pre-fill synchronously (no scheduler yield) so the channel is at
        // capacity before try_send is called.
        let (resp_tx, _) = oneshot::channel();
        supervisor
            .sender()
            .try_send(MemoryCoordinatorMsg::AppendLearning {
                learning: sample_learning(),
                client_dedup_key: None,
                response_tx: resp_tx,
            })
            .expect("pre-fill must succeed on empty channel");
        // Channel is now full; _rx is never polled, so the 100ms timeout fires.
        let (resp_tx2, _) = oneshot::channel();
        let result = supervisor
            .try_send(MemoryCoordinatorMsg::AppendLearning {
                learning: sample_learning(),
                client_dedup_key: None,
                response_tx: resp_tx2,
            })
            .await;
        assert_eq!(result, Err(CoordinatorSendError::Full));
    }

    #[tokio::test]
    async fn try_send_closed_channel_returns_closed() {
        // Drop the receiver to simulate coordinator exit; send must return
        // immediately with Closed (no timeout needed).
        let (supervisor, rx) = Supervisor::for_backpressure_test();
        drop(rx);
        let (resp_tx, _) = oneshot::channel();
        let result = supervisor
            .try_send(MemoryCoordinatorMsg::AppendLearning {
                learning: sample_learning(),
                client_dedup_key: None,
                response_tx: resp_tx,
            })
            .await;
        assert_eq!(result, Err(CoordinatorSendError::Closed));
    }

    #[test]
    fn coordinator_channel_capacity_constant_is_256() {
        assert_eq!(COORDINATOR_CHANNEL_CAPACITY, 256);
    }
}
