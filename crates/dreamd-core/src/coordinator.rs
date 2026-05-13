//! `MemoryCoordinator` — the single mutable owner of the episodic JSONL.
//!
//! Per DR-103 / DR-114, all mutations to `AGENT_LEARNINGS.jsonl` flow through
//! one tokio task. The "Mutex" of DR-103 is the actor itself: `&mut self` on
//! [`MemoryCoordinator::run`] is the exclusivity guarantee — we do NOT wrap
//! the file in a `Mutex`, because there is no other handle to it.
//!
//! WEG-16 lands the skeleton (channel topology + `AppendLearning` durability
//! path). WEG-7 will add the sidecar idempotency cache, ULID generation, and
//! the 4 KiB envelope cap on top of this actor.
//!
//! The file writes are blocking `std::io` calls; that's intentional for v0.1.
//! The actor model already serializes mutations, so blocking the runtime task
//! is acceptable until benchmarks demand otherwise. Do NOT swap in `tokio::fs`
//! or `spawn_blocking` without re-reading the durability ADR.

use std::io::Write;

use dreamd_protocol::AgentLearning;
use tokio::sync::{mpsc, oneshot};

/// Errors surfaced by the coordinator back to API handlers.
#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialize error: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Messages accepted by the coordinator actor.
///
/// `#[non_exhaustive]` keeps the enum forward-compatible: WEG-7 / WEG-50 /
/// later tickets will add `Query`, `RunDreamCycle`, etc. without breaking
/// downstream `match` arms.
#[derive(Debug)]
#[non_exhaustive]
pub enum MemoryCoordinatorMsg {
    /// Append a learning to the JSONL. The response fires after `sync_data`
    /// returns — i.e., the bytes are durable on disk.
    AppendLearning {
        learning: AgentLearning,
        response_tx: oneshot::Sender<Result<(), CoordinatorError>>,
    },
    /// Gracefully drain the channel and exit the run loop.
    Shutdown { response_tx: oneshot::Sender<()> },
}

/// Single mutable owner of `AGENT_LEARNINGS.jsonl`.
pub struct MemoryCoordinator {
    file: std::fs::File,
    rx: mpsc::Receiver<MemoryCoordinatorMsg>,
}

impl MemoryCoordinator {
    /// Construct a coordinator. The caller is responsible for opening `file`
    /// in append mode against the right path (see `dreamd-core::layout`).
    pub fn new(file: std::fs::File, rx: mpsc::Receiver<MemoryCoordinatorMsg>) -> Self {
        Self { file, rx }
    }

    /// Run the coordinator loop until `Shutdown` is received (or the channel
    /// closes). `&mut self` is the exclusivity guarantee — there is no other
    /// handle to the file, so no `Mutex` is required.
    pub async fn run(mut self) {
        while let Some(msg) = self.rx.recv().await {
            match msg {
                MemoryCoordinatorMsg::AppendLearning {
                    learning,
                    response_tx,
                } => {
                    let result = self.append_learning(&learning);
                    // Receiver may have been dropped; that's fine, we still
                    // performed the durable write.
                    let _ = response_tx.send(result);
                }
                MemoryCoordinatorMsg::Shutdown { response_tx } => {
                    // Drain any remaining messages without acting on them.
                    // The channel close ensures no new senders can enqueue
                    // after this point only if all senders are dropped, but
                    // the explicit drain matches the spec.
                    self.rx.close();
                    while self.rx.recv().await.is_some() {}
                    let _ = response_tx.send(());
                    break;
                }
            }
        }
    }

    fn append_learning(&mut self, learning: &AgentLearning) -> Result<(), CoordinatorError> {
        let mut line = serde_json::to_string(learning)?;
        if !line.ends_with('\n') {
            line.push('\n');
        }
        self.file.write_all(line.as_bytes())?;
        self.file.sync_data()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use std::fs::OpenOptions;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_tmp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("dreamd-coord-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn sample_learning() -> AgentLearning {
        AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: "evt_test_0001".to_string(),
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

    #[tokio::test]
    async fn append_learning_round_trip_then_shutdown() {
        let dir = unique_tmp_dir("append");
        let path = dir.join("AGENT_LEARNINGS.jsonl");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("open jsonl");

        let (tx, rx) = mpsc::channel::<MemoryCoordinatorMsg>(8);
        let coordinator = MemoryCoordinator::new(file, rx);
        let handle = tokio::spawn(coordinator.run());

        let learning = sample_learning();
        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(MemoryCoordinatorMsg::AppendLearning {
            learning: learning.clone(),
            response_tx: resp_tx,
        })
        .await
        .expect("send append");

        let append_result = resp_rx.await.expect("oneshot recv");
        assert!(append_result.is_ok(), "append failed: {append_result:?}");

        // Read the JSONL and assert the round-trip.
        let raw = std::fs::read_to_string(&path).expect("read jsonl");
        let line = raw.lines().next().expect("at least one line");
        let decoded: AgentLearning = serde_json::from_str(line).expect("deserialize");
        assert_eq!(decoded, learning);

        // Shutdown
        let (sh_tx, sh_rx) = oneshot::channel();
        tx.send(MemoryCoordinatorMsg::Shutdown { response_tx: sh_tx })
            .await
            .expect("send shutdown");
        sh_rx.await.expect("shutdown ack");
        handle.await.expect("coordinator joined");

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }
}
