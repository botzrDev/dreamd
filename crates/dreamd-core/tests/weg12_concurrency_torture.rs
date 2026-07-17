#![cfg(unix)]

//! WEG-12 / DR-110 — concurrency torture test (1000 concurrent appends).
//!
//! Fans out 1000 concurrent `AppendLearning` messages against a live in-process
//! [`MemoryCoordinator`] (the DR-114 actor) and asserts durable DR-103 JSONL
//! integrity: exact line count, every line parseable, all minted ids unique, and
//! no torn tail. Runs both a small and a near-4-KiB "large" payload, each under a
//! 10-second wall-clock budget.
//!
//! Locked to the DIRECT coordinator channel rather than the HTTP learn endpoint:
//! the Supervisor's HTTP ingress uses a `try_send` with a 100 ms timeout and a
//! capacity-256 channel, so a naïve 1000-way HTTP hammer races into `503 Full`
//! and cannot assert "exactly 1000 lines". A direct `mpsc::Sender::send().await`
//! backpressures cleanly while still exercising the full serialize → `\n` →
//! 4 KiB check → `write_all` → `sync_data` durable-write path inside
//! `handle_append`.
//!
//! [`MemoryCoordinator`]: dreamd_core::coordinator::MemoryCoordinator

use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use dreamd_core::coordinator::{MemoryCoordinator, MemoryCoordinatorMsg};
use dreamd_core::episodic::{self, MAX_LEARNING_LINE_BYTES};
use dreamd_protocol::{AgentLearning, EventId};
use tempfile::TempDir;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

/// Concurrent appends per case.
const N: usize = 1000;

/// Canonical all-uppercase Crockford ULID example, used only as a parseable
/// placeholder `id`. The coordinator overwrites `id`, `schema_version`, and
/// `timestamp` on every durable append.
const SAMPLE_ULID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

/// Wall-clock budget for a single 1000-way fan-out.
const BUDGET: Duration = Duration::from_secs(10);

fn placeholder_id() -> EventId {
    EventId::parse(&format!("evt_{SAMPLE_ULID}")).expect("placeholder EventId parses")
}

fn learning(content: String) -> AgentLearning {
    AgentLearning {
        schema_version: "1.0.0".to_string(),
        id: placeholder_id(),
        timestamp: DateTime::parse_from_rfc3339("2026-05-13T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
        pain: 5.0,
        importance: 5.0,
        pinned: false,
        skill_action: "rust::concurrency::torture".to_string(),
        source_harness: "weg12-torture".to_string(),
        content,
    }
}

async fn spawn_coord(
    jsonl: &Path,
    root: &Path,
) -> (
    mpsc::Sender<MemoryCoordinatorMsg>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = mpsc::channel(256);
    let coord = MemoryCoordinator::open_at(jsonl, root, rx, None).expect("open coordinator");
    let handle = tokio::spawn(coord.run());
    (tx, handle)
}

/// Fan out `N` concurrent appends of `content`, awaiting every durable ack.
/// Asserts each append was a fresh write (never a dedup hit) and that all `N`
/// minted `EventId`s are unique. Returns the fan-out wall-clock elapsed.
async fn fanout(tx: &mpsc::Sender<MemoryCoordinatorMsg>, content: String) -> Duration {
    let start = Instant::now();
    let mut set = JoinSet::new();
    for _ in 0..N {
        let tx = tx.clone();
        let learning = learning(content.clone());
        set.spawn(async move {
            let (resp_tx, resp_rx) = oneshot::channel();
            tx.send(MemoryCoordinatorMsg::AppendLearning {
                learning,
                // Critical: a shared dedup key would collapse the line count via
                // the idempotency LRU. None guarantees 1000 distinct writes.
                client_dedup_key: None,
                response_tx: resp_tx,
            })
            .await
            .expect("send append");
            let outcome = resp_rx.await.expect("oneshot recv").expect("append ok");
            assert!(!outcome.deduplicated, "a None dedup key must never dedup");
            outcome.id
        });
    }

    let mut ids = HashSet::new();
    while let Some(res) = set.join_next().await {
        let id = res.expect("task joined");
        ids.insert(id.as_str().to_owned());
    }
    assert_eq!(ids.len(), N, "all minted EventIds must be unique");
    start.elapsed()
}

async fn shutdown(tx: mpsc::Sender<MemoryCoordinatorMsg>, handle: tokio::task::JoinHandle<()>) {
    let (sh_tx, sh_rx) = oneshot::channel();
    tx.send(MemoryCoordinatorMsg::Shutdown { response_tx: sh_tx })
        .await
        .expect("send shutdown");
    sh_rx.await.expect("shutdown ack");
    handle.await.expect("coordinator task joined");
}

/// Durable-integrity assertions (WEG-378 conventions): trailing `\n`, zero torn
/// bytes, zero malformed lines, exactly `N` valid records, and `N` unique ids.
/// Uses raw-byte checks plus `episodic::{assess_log_health, read_all}` — never
/// `str::lines()` alone, which drops the trailing-newline signal.
fn assert_jsonl_intact(path: &Path) {
    let raw = std::fs::read(path).expect("read jsonl");
    assert!(!raw.is_empty(), "file must not be empty");
    assert_eq!(*raw.last().unwrap(), b'\n', "file must end with a newline");

    let health = episodic::assess_log_health(path).expect("assess health");
    assert_eq!(health.valid_record_count, N, "exactly N durable records");
    assert_eq!(health.torn_tail_bytes, 0, "no torn tail");
    assert_eq!(health.malformed_line_count, 0, "no malformed lines");

    let records = episodic::read_all(path).expect("read_all");
    assert_eq!(records.len(), N, "read_all returns N records");
    let unique: HashSet<_> = records.iter().map(|r| r.id.as_str().to_owned()).collect();
    assert_eq!(unique.len(), N, "all persisted ids must be unique");
}

/// Build a "large" content body so the serialized line (including the trailing
/// `\n`) is just under `MAX_LEARNING_LINE_BYTES` and comfortably over 3 KiB.
/// Sized dynamically from the empty-content overhead, leaving headroom below the
/// cap because the coordinator overwrites `timestamp` with `Utc::now()`, whose
/// RFC3339 form can be longer than our fixed placeholder timestamp.
fn large_content() -> String {
    let overhead = serde_json::to_string(&learning(String::new()))
        .expect("serialize probe")
        .len();
    // Target serialized-line length (incl. trailing '\n'), well below the 4096
    // cap so the real, longer minted timestamp still fits.
    const TARGET_LINE: usize = 4050;
    let content_len = TARGET_LINE - 1 - overhead; // -1 for the trailing '\n'
    "x".repeat(content_len)
}

#[tokio::test]
async fn concurrent_appends_small_payload_are_durable() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    let jsonl = root.join("AGENT_LEARNINGS.jsonl");

    let (tx, handle) = spawn_coord(&jsonl, root).await;
    let elapsed = fanout(&tx, "weg12 torture: small payload body".to_string()).await;
    shutdown(tx, handle).await;

    assert_jsonl_intact(&jsonl);
    assert!(
        elapsed < BUDGET,
        "small-payload fan-out took {elapsed:?}, over the {BUDGET:?} budget"
    );
    println!("small-payload: {N} concurrent appends in {elapsed:?}");
}

#[tokio::test]
async fn concurrent_appends_large_payload_are_durable() {
    let content = large_content();

    // Sizing guard: the measured serialized line + '\n' must fit the 4 KiB cap
    // and still be a meaningful near-cap "large" case (> 3000 bytes).
    let serialized_len = serde_json::to_string(&learning(content.clone()))
        .expect("serialize large")
        .len();
    let line_len = serialized_len + 1; // trailing '\n'
    assert!(
        line_len <= MAX_LEARNING_LINE_BYTES,
        "large line {line_len} exceeds MAX_LEARNING_LINE_BYTES {MAX_LEARNING_LINE_BYTES}"
    );
    assert!(
        line_len > 3000,
        "large line {line_len} should exceed 3000 to exercise a near-cap payload"
    );
    println!("large-payload: serialized line = {line_len} bytes (cap {MAX_LEARNING_LINE_BYTES})");

    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    let jsonl = root.join("AGENT_LEARNINGS.jsonl");

    let (tx, handle) = spawn_coord(&jsonl, root).await;
    let elapsed = fanout(&tx, content).await;
    shutdown(tx, handle).await;

    assert_jsonl_intact(&jsonl);
    assert!(
        elapsed < BUDGET,
        "large-payload fan-out took {elapsed:?}, over the {BUDGET:?} budget"
    );
    println!("large-payload: {N} concurrent appends in {elapsed:?}");
}
