#![cfg(unix)]

//! Red tests for the store defects found by the architecture audit.
//!
//! Each test reproduces one defect through today's public API. Each is
//! ignored until the ticket named in its `ignore` reason lands, and each must
//! fail under `--ignored` before that fix. The fixing ticket un-ignores its
//! test; it does not weaken the assertion.
//!
//! `#![cfg(unix)]` because opening an index handle writes its manifest with
//! `io::write_atomic`, which is `Unsupported` off Unix.

use std::fs;
use std::path::Path;

use chrono::DateTime;
use dreamd_core::coordinator::{MemoryCoordinator, MemoryCoordinatorMsg};
use dreamd_core::index::build_schema;
use dreamd_core::invariants::check_store;
use dreamd_core::layout::AgentRoot;
use dreamd_core::salience::salience;
use dreamd_core::server::{TantivyIndexHandle, DEFAULT_COMMIT_CADENCE, INDEX_PROGRESS_FILENAME};
use dreamd_core::writer_lock::{WriterKind, WriterLock};
use dreamd_protocol::{AgentLearning, EventId};
use tokio::sync::{mpsc, oneshot};

/// Fixed clock for fixtures the coordinator does not restamp.
const NOW_SEC: i64 = 1_780_000_000;
/// The coordinator overwrites this on every append.
const PLACEHOLDER_ID: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV";

/// `pain` and `importance` 6.0, so no salience product collapses to zero.
fn ev(id: &str, skill: &str, content: &str, ts: i64) -> AgentLearning {
    AgentLearning {
        schema_version: "1.0.0".to_string(),
        id: EventId::parse(id).expect("synthesize EventId"),
        timestamp: DateTime::from_timestamp(ts, 0).expect("valid timestamp"),
        pain: 6.0,
        importance: 6.0,
        pinned: false,
        skill_action: skill.to_string(),
        source_harness: "claude-code".to_string(),
        content: content.to_string(),
    }
}

fn line(ev: &AgentLearning) -> String {
    let mut s = serde_json::to_string(ev).expect("serialize learning");
    s.push('\n');
    s
}

async fn append(tx: &mpsc::Sender<MemoryCoordinatorMsg>, learning: AgentLearning) {
    let (response_tx, response_rx) = oneshot::channel();
    tx.send(MemoryCoordinatorMsg::AppendLearning {
        learning,
        client_dedup_key: None,
        response_tx,
    })
    .await
    .expect("coordinator inbox open");
    response_rx
        .await
        .expect("coordinator replied")
        .expect("append succeeded");
}

async fn shutdown(tx: &mpsc::Sender<MemoryCoordinatorMsg>) {
    let (response_tx, response_rx) = oneshot::channel();
    tx.send(MemoryCoordinatorMsg::Shutdown { response_tx })
        .await
        .expect("coordinator inbox open");
    response_rx
        .await
        .expect("coordinator acknowledged shutdown");
}

/// Opens the way a writer does after `dreamd init`: `.agent/` exists, then the
/// project writer lock, then the index. The handle's indexer task keeps the
/// token, so the project stays locked until the handle is shut down.
fn open(root: &AgentRoot) -> TantivyIndexHandle {
    fs::create_dir_all(root.agent_dir()).unwrap();
    let lock = WriterLock::acquire(root, WriterKind::Watch).expect("project writer lock");
    TantivyIndexHandle::open(root, &lock, DEFAULT_COMMIT_CADENCE).expect("open index handle")
}

/// Three fresh `rust::eh` events, enough to promote, with `"x_note":"keep"`
/// on the first record and `mid_line` (if any) between the first and second.
fn seed_promotable_log(root: &AgentRoot, mid_line: Option<&str>) {
    fs::create_dir_all(root.episodic_dir()).unwrap();
    fs::create_dir_all(root.semantic_dir()).unwrap();
    let events = [
        ev(
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FA0",
            "rust::eh",
            "unwrap panicked in the handler",
            NOW_SEC - 3 * 3600,
        ),
        ev(
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FA1",
            "rust::eh",
            "unwrap panicked in the parser",
            NOW_SEC - 2 * 3600,
        ),
        ev(
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FA2",
            "rust::eh",
            "unwrap panicked in the loader",
            NOW_SEC - 3600,
        ),
    ];
    let mut first = serde_json::to_value(&events[0]).unwrap();
    first["x_note"] = serde_json::json!("keep");
    let mut body = serde_json::to_string(&first).unwrap();
    body.push('\n');
    if let Some(mid) = mid_line {
        body.push_str(mid);
        body.push('\n');
    }
    body.push_str(&line(&events[1]));
    body.push_str(&line(&events[2]));
    fs::write(root.episodic_jsonl(), body).unwrap();
}

fn run_cycle(project: &Path) {
    let root = AgentRoot::new(project);
    dreamd_core::dream_cycle::run_in_process(project, NOW_SEC, true, true, false)
        .expect("dream cycle");
    assert!(
        root.lessons_md().exists(),
        "precondition: the cycle promoted rust::eh and wrote LESSONS.md"
    );
}

/// A `git checkout` replaces the log by rename. Appends after that must land
/// in the file at the path, not in the replaced inode.
#[tokio::test]
#[ignore = "BZR-1233: append fd orphaned by inode swap"]
async fn appends_survive_an_inode_swap_of_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let root = AgentRoot::new(dir.path());
    let jsonl = root.episodic_jsonl();
    let (tx, rx) = mpsc::channel(8);
    let coordinator = MemoryCoordinator::open_at(&jsonl, dir.path(), rx, None).unwrap();
    let task = tokio::spawn(coordinator.run());

    append(
        &tx,
        ev(PLACEHOLDER_ID, "git::checkout", "first marker", NOW_SEC),
    )
    .await;
    let sibling = jsonl.with_extension("checkout");
    fs::copy(&jsonl, &sibling).unwrap();
    fs::rename(&sibling, &jsonl).unwrap();
    append(
        &tx,
        ev(PLACEHOLDER_ID, "git::checkout", "second marker", NOW_SEC),
    )
    .await;
    shutdown(&tx).await;
    task.await.unwrap();

    let body = fs::read_to_string(&jsonl).unwrap();
    assert!(
        body.contains("second marker"),
        "an append after the rename must land in the file at the path; file holds:\n{body}"
    );
}

/// Losing `index_progress.json` makes the next open replay the whole log. The
/// replay must not add a second document for an event already indexed.
#[tokio::test]
#[ignore = "BZR-1236: replay re-adds without delete_term"]
async fn replay_after_lost_watermark_indexes_each_event_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = AgentRoot::new(dir.path());
    fs::create_dir_all(root.episodic_dir()).unwrap();
    fs::write(
        root.episodic_jsonl(),
        line(&ev(
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FA0",
            "rust::eh",
            "indexed exactly once",
            NOW_SEC,
        )),
    )
    .unwrap();

    open(&root).shutdown().await.unwrap();
    fs::remove_file(root.dreamd_dir().join(INDEX_PROGRESS_FILENAME)).unwrap();
    let h = open(&root);

    let violations = check_store(&root, Some(h.reader())).unwrap();
    assert!(
        violations.is_empty(),
        "replay without a watermark must leave one document per event; got {violations:?}"
    );
    h.shutdown().await.unwrap();
}

/// Three live appends under one key must all carry that key's subtree count,
/// the same value a replay of the same log would give them.
#[tokio::test]
#[ignore = "BZR-1242: recurrence depends on the path that indexed it"]
async fn live_appended_docs_carry_the_subtree_recurrence() {
    let dir = tempfile::tempdir().unwrap();
    let root = AgentRoot::new(dir.path());
    let h = open(&root);
    let (tx, rx) = mpsc::channel(8);
    let coordinator =
        MemoryCoordinator::open_at(&root.episodic_jsonl(), dir.path(), rx, Some(h.sender()))
            .unwrap();
    let task = tokio::spawn(coordinator.run());

    for n in 1..=3 {
        append(
            &tx,
            ev(
                PLACEHOLDER_ID,
                "rust::eh::unwrap",
                &format!("quokka {n}"),
                NOW_SEC,
            ),
        )
        .await;
    }
    shutdown(&tx).await;
    task.await.unwrap();
    h.shutdown().await.unwrap();

    let h = open(&root);
    let (_, fields) = build_schema();
    let now = chrono::Utc::now().timestamp();
    let results = dreamd_core::recall(h.reader(), &fields, "quokka", 10, None, now).unwrap();
    let recurrences: Vec<u64> = results.iter().map(|r| r.recurrence).collect();
    assert_eq!(results.len(), 3, "precondition: all three appends indexed");
    assert!(
        results.iter().all(|r| r.recurrence == 3),
        "every doc under rust::eh::unwrap must carry recurrence 3; got {recurrences:?}"
    );
    h.shutdown().await.unwrap();
}

/// The pin pass must leave a record it does not change byte-identical,
/// including fields this binary does not know.
#[test]
#[ignore = "BZR-1234: pin pass drops x_ fields"]
fn pin_pass_keeps_unknown_fields() {
    let dir = tempfile::tempdir().unwrap();
    let root = AgentRoot::new(dir.path());
    seed_promotable_log(&root, None);

    run_cycle(dir.path());

    let body = fs::read_to_string(root.episodic_jsonl()).unwrap();
    assert!(
        body.contains(r#""x_note":"keep""#),
        "the pin pass must keep unknown fields; file holds:\n{body}"
    );
}

/// The pin pass must keep a line the reader skips, not delete it.
#[test]
#[ignore = "BZR-1234: pin pass drops skipped lines"]
fn pin_pass_keeps_skipped_lines() {
    const SKIPPED: &str = r#"{"not":"a learning"}"#;
    let dir = tempfile::tempdir().unwrap();
    let root = AgentRoot::new(dir.path());
    seed_promotable_log(&root, Some(SKIPPED));

    run_cycle(dir.path());

    let body = fs::read_to_string(root.episodic_jsonl()).unwrap();
    assert!(
        body.lines().any(|l| l == SKIPPED),
        "the pin pass must keep a skipped line; file holds:\n{body}"
    );
}

/// Open-time recovery must not remove a line the reader merely skips. A
/// lowercase id is a valid ULID this reader does not accept.
#[test]
#[ignore = "BZR-1235: recovery excises SPEC-skippable lines"]
fn open_keeps_a_record_this_reader_skips() {
    const UPPER: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FA1";
    const LOWER: &str = "evt_01arz3ndektsv4rrffq69g5fa1";
    let dir = tempfile::tempdir().unwrap();
    let root = AgentRoot::new(dir.path());
    fs::create_dir_all(root.episodic_dir()).unwrap();
    let lower_line = line(&ev(UPPER, "rust::eh", "lowercase id", NOW_SEC)).replace(UPPER, LOWER);
    let body = [
        line(&ev(
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FA0",
            "rust::eh",
            "before",
            NOW_SEC,
        )),
        lower_line.clone(),
        line(&ev(
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FA2",
            "rust::eh",
            "after",
            NOW_SEC,
        )),
    ]
    .concat();
    fs::write(root.episodic_jsonl(), body).unwrap();

    let (_tx, rx) = mpsc::channel(1);
    drop(MemoryCoordinator::open_at(&root.episodic_jsonl(), dir.path(), rx, None).unwrap());

    let after = fs::read_to_string(root.episodic_jsonl()).unwrap();
    assert!(
        after.contains(&lower_line),
        "open must keep the lowercase-id record; file holds:\n{after}"
    );
}

/// A final record that parses but lacks its `\n` is complete. Recovery must
/// add the newline, not truncate the record.
#[test]
#[ignore = "BZR-1235: valid unterminated final record truncated"]
fn open_keeps_a_valid_final_record_without_newline() {
    let dir = tempfile::tempdir().unwrap();
    let root = AgentRoot::new(dir.path());
    fs::create_dir_all(root.episodic_dir()).unwrap();
    let a = line(&ev(
        "evt_01ARZ3NDEKTSV4RRFFQ69G5FA0",
        "rust::eh",
        "terminated",
        NOW_SEC,
    ));
    let b = serde_json::to_string(&ev(
        "evt_01ARZ3NDEKTSV4RRFFQ69G5FA1",
        "rust::eh",
        "unterminated",
        NOW_SEC,
    ))
    .unwrap();
    fs::write(root.episodic_jsonl(), format!("{a}{b}")).unwrap();

    let (_tx, rx) = mpsc::channel(1);
    drop(MemoryCoordinator::open_at(&root.episodic_jsonl(), dir.path(), rx, None).unwrap());

    let records = dreamd_core::episodic::read_all(&root.episodic_jsonl()).unwrap();
    let bytes = fs::read(root.episodic_jsonl()).unwrap();
    assert_eq!(
        records.len(),
        2,
        "open must keep the valid unterminated record; file holds:\n{}",
        String::from_utf8_lossy(&bytes)
    );
    assert!(bytes.ends_with(b"\n"), "the log must end with a newline");
}

/// Recency is at most 1.0, so with pain and importance at the 10.0 ceiling and
/// no recurrence, salience is at most 1.0 whatever the timestamp.
#[test]
#[ignore = "BZR-1237: salience unbounded for future timestamps"]
fn future_timestamp_cannot_raise_salience_above_one() {
    let now = NOW_SEC;
    let s = salience(now, now + 365 * 86_400, 10.0, 10.0, 0);
    assert!(
        s <= 1.0,
        "a timestamp a year ahead must not raise salience above 1.0; got {s}"
    );
}
