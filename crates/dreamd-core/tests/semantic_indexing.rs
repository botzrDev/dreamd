#![cfg(unix)]

//! DR-211 / AILAB-205 — `semantic/LESSONS.md` → Tantivy, end to end.
//!
//! `#![cfg(unix)]` because `dreamd_core::server` (and therefore
//! `TantivyIndexHandle`) is unix-gated in `lib.rs`.
//!
//! Every case drives the real seam: `TantivyIndexHandle::open` runs the
//! semantic pass as part of its rebuild, and `index_semantic_lessons` re-runs it
//! on the live indexer task the way the dream cycle does. Pure-helper coverage
//! (prefix counting, the `lsn_` identity shape, the pass outcome) lives in
//! `server::tantivy_handle`'s unit tests.
//!
//! Test names carry their `§3 step 5` case number so the spec-to-suite mapping
//! is readable straight off the `cargo test` output.
//!
//! Three cases are regression guards and must fail against the naive designs:
//!   * [`case_04_semantic_pass_preserves_every_episodic_event_in_the_cluster`]
//!     fails if the pass deletes by `skill_action`/cluster key.
//!   * [`case_05_decay_prune_of_the_exemplar_leaves_the_lesson_indexed`] fails
//!     if a lesson carries its exemplar's raw `event_id`.
//!   * [`case_09_lesson_recency_is_consolidation_time_not_exemplar_age`] fails
//!     if `timestamp_sec` is inherited from the exemplar (observed ratio
//!     0.00161 against a fresh event, versus 1.0 once corrected).

use chrono::{DateTime, Utc};
use dreamd_core::collector::RecallResult;
use dreamd_core::index::{build_schema, Layer};
use dreamd_core::layout::AgentRoot;
use dreamd_core::lessons::{write_lessons_file, Lesson, LessonsFile};
use dreamd_core::server::tantivy_handle::{TantivyIndexHandle, DEFAULT_COMMIT_CADENCE};
use dreamd_protocol::{AgentLearning, EventId};

/// Fixed query-time clock — no wall-clock reads, same invariant as `recall`.
const NOW_SEC: i64 = 1_780_000_000;
const DAY_SECS: i64 = 86_400;
/// 25 Crockford chars; each id appends one more for a canonical 26-char ULID.
const ULID_BASE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA";

fn event_id(suffix: char) -> EventId {
    EventId::parse(&format!("evt_{ULID_BASE}{suffix}")).expect("synthesize EventId")
}

fn at(ts_sec: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(ts_sec, 0).expect("valid timestamp")
}

/// `pain = 7`, `importance = 8` — both non-zero so the salience product cannot
/// collapse for a reason unrelated to the case under test.
fn learning(id: EventId, skill: &str, content: &str, ts_sec: i64) -> AgentLearning {
    AgentLearning {
        schema_version: "1.0.0".to_string(),
        id,
        timestamp: at(ts_sec),
        pain: 7.0,
        importance: 8.0,
        pinned: false,
        skill_action: skill.to_string(),
        source_harness: "claude-code".to_string(),
        content: content.to_string(),
    }
}

fn scaffold() -> (tempfile::TempDir, AgentRoot) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = AgentRoot::new(dir.path());
    std::fs::create_dir_all(root.episodic_dir()).expect("episodic dir");
    std::fs::create_dir_all(root.semantic_dir()).expect("semantic dir");
    std::fs::create_dir_all(root.dreamd_dir()).expect("dreamd dir");
    (dir, root)
}

fn write_jsonl(root: &AgentRoot, events: &[AgentLearning]) {
    let mut body = String::new();
    for ev in events {
        body.push_str(&serde_json::to_string(ev).expect("serialize learning"));
        body.push('\n');
    }
    std::fs::write(root.episodic_jsonl(), body).expect("write jsonl");
}

fn lesson(id: &EventId, content: &str) -> Lesson {
    Lesson {
        id: id.as_str().to_string(),
        content: content.to_string(),
        pinned: false,
    }
}

fn write_lessons(root: &AgentRoot, cluster_key: &str, lessons: Vec<Lesson>) {
    write_lessons_file(
        &root.lessons_md(),
        &LessonsFile {
            last_updated: at(NOW_SEC),
            prompt_version: "dream-cycle/v1.1@2026-05-13".to_string(),
            cluster_key: cluster_key.to_string(),
            lessons,
        },
    )
    .expect("write LESSONS.md");
}

fn open(root: &AgentRoot) -> TantivyIndexHandle {
    TantivyIndexHandle::open(root, DEFAULT_COMMIT_CADENCE).expect("open index handle")
}

/// Reload the handle's reader, then run a salience-scored recall against it.
fn query(handle: &TantivyIndexHandle, q: &str, layer: Option<Layer>) -> Vec<RecallResult> {
    handle.reader().reload().expect("reload reader");
    let (_, fields) = build_schema();
    dreamd_core::recall(handle.reader(), &fields, q, 10, layer, NOW_SEC).expect("recall")
}

// Case 1 — the fact-6 guard.

#[tokio::test]
async fn case_01_lesson_is_recalled_as_semantic_with_a_non_zero_score() {
    let (_dir, root) = scaffold();
    let exemplar = event_id('0');
    write_jsonl(
        &root,
        &[learning(
            exemplar.clone(),
            "rust::eh",
            "unwrap panicked in the request handler",
            NOW_SEC - DAY_SECS,
        )],
    );
    write_lessons(
        &root,
        "rust::eh",
        vec![lesson(
            &exemplar,
            "Propagate with the question mark operator outside tests.",
        )],
    );

    let handle = open(&root);
    let results = query(&handle, "propagate", None);

    assert_eq!(
        results.len(),
        1,
        "the lesson must be recallable: {results:?}"
    );
    let hit = &results[0];
    assert_eq!(hit.layer, Layer::Semantic);
    assert_eq!(hit.source_harness, "dreamd", "the dream cycle authored it");
    assert_eq!(hit.skill_action, "rust::eh");
    assert!(
        hit.event_id.starts_with("lsn_"),
        "lesson identity is namespaced: {}",
        hit.event_id
    );
    // The whole point of inheriting pain/importance from the exemplar: without
    // them the salience product is exactly 0.0 and the document can never rank.
    assert_eq!(hit.pain, 7.0, "pain inherited from the exemplar");
    assert_eq!(
        hit.importance, 8.0,
        "importance inherited from the exemplar"
    );
    assert!(
        hit.score > 0.0,
        "a lesson must score strictly above zero, got {}",
        hit.score
    );

    handle.shutdown().await.expect("shutdown");
}

// Case 2 — one ranked result set.

#[tokio::test]
async fn case_02_episodic_and_semantic_results_rank_together() {
    let (_dir, root) = scaffold();
    let exemplar = event_id('0');
    write_jsonl(
        &root,
        &[learning(
            exemplar.clone(),
            "rust::tantivy",
            "tantivy commit stalled the whole request",
            NOW_SEC - DAY_SECS,
        )],
    );
    write_lessons(
        &root,
        "rust::tantivy",
        vec![lesson(
            &exemplar,
            "Never hold a stdout lock across a tantivy commit.",
        )],
    );

    let handle = open(&root);
    let results = query(&handle, "tantivy", None);

    assert_eq!(
        results.len(),
        2,
        "both layers answer one query: {results:?}"
    );
    assert!(
        results.iter().any(|r| r.layer == Layer::Episodic),
        "episodic document missing from the ranked set"
    );
    assert!(
        results.iter().any(|r| r.layer == Layer::Semantic),
        "semantic document missing from the ranked set"
    );
    assert!(
        results[0].score >= results[1].score,
        "results must be ordered by descending score: {results:?}"
    );
    assert!(results.iter().all(|r| r.score > 0.0));

    handle.shutdown().await.expect("shutdown");
}

// Case 3 — the layer filter still filters.

#[tokio::test]
async fn case_03_episodic_layer_filter_still_excludes_lessons() {
    let (_dir, root) = scaffold();
    let exemplar = event_id('0');
    write_jsonl(
        &root,
        &[learning(
            exemplar.clone(),
            "rust::tantivy",
            "tantivy commit stalled the whole request",
            NOW_SEC - DAY_SECS,
        )],
    );
    write_lessons(
        &root,
        "rust::tantivy",
        vec![lesson(
            &exemplar,
            "Never hold a stdout lock across a tantivy commit.",
        )],
    );

    let handle = open(&root);
    let results = query(&handle, "tantivy", Some(Layer::Episodic));

    assert_eq!(results.len(), 1, "layer filter must drop the lesson");
    assert_eq!(results[0].layer, Layer::Episodic);
    assert!(results[0].content.contains("stalled"));

    handle.shutdown().await.expect("shutdown");
}

// Case 4 — regression: the pass must not delete by cluster key.

#[tokio::test]
async fn case_04_semantic_pass_preserves_every_episodic_event_in_the_cluster() {
    let (_dir, root) = scaffold();
    let events: Vec<AgentLearning> = ['0', '1', '2']
        .into_iter()
        .map(|c| {
            learning(
                event_id(c),
                "rust::foo",
                "widget assembly failed again",
                NOW_SEC - DAY_SECS,
            )
        })
        .collect();
    write_jsonl(&root, &events);
    // Same cluster key as every episodic event above — deleting by skill_action
    // here would take all three with it.
    write_lessons(
        &root,
        "rust::foo",
        vec![lesson(
            &events[0].id,
            "Assemble the widget before the gasket.",
        )],
    );

    let handle = open(&root);
    assert_eq!(
        query(&handle, "widget", Some(Layer::Episodic)).len(),
        3,
        "all three episodic events survive the first pass"
    );

    // Re-run the pass exactly as the dream cycle does after a rewrite.
    handle
        .index_semantic_lessons(root.clone())
        .await
        .expect("re-index lessons");

    assert_eq!(
        query(&handle, "widget", Some(Layer::Episodic)).len(),
        3,
        "re-running the semantic pass must not delete episodic events \
         that share the lesson's cluster key"
    );
    assert_eq!(
        query(&handle, "gasket", Some(Layer::Semantic)).len(),
        1,
        "exactly one lesson after the re-run — no duplicate"
    );

    handle.shutdown().await.expect("shutdown");
}

// Case 5 — regression: the lsn_ namespace keeps decay off the lesson.

#[tokio::test]
async fn case_05_decay_prune_of_the_exemplar_leaves_the_lesson_indexed() {
    let (_dir, root) = scaffold();
    let exemplar = event_id('0');
    write_jsonl(
        &root,
        &[learning(
            exemplar.clone(),
            "rust::eh",
            "gasket torque drifted overnight",
            NOW_SEC - DAY_SECS,
        )],
    );
    write_lessons(
        &root,
        "rust::eh",
        vec![lesson(&exemplar, "Re-torque the gasket after every cycle.")],
    );

    let handle = open(&root);
    assert_eq!(query(&handle, "gasket", None).len(), 2, "event + lesson");

    // The pruner deletes by the raw event_id term. A lesson carrying that same
    // id would silently vanish here.
    handle
        .prune_decayed_events(vec![exemplar.clone()])
        .await
        .expect("prune decayed events");

    let after = query(&handle, "gasket", None);
    assert_eq!(
        after.len(),
        1,
        "only the episodic event is pruned: {after:?}"
    );
    assert_eq!(after[0].layer, Layer::Semantic);
    assert_eq!(after[0].event_id, format!("lsn_{}", exemplar.as_str()));

    handle.shutdown().await.expect("shutdown");
}

// Case 6 — wholesale replace across cycles.

#[tokio::test]
async fn case_06_second_cycle_replaces_the_previous_lesson_set() {
    let (_dir, root) = scaffold();
    let first = event_id('0');
    let second = event_id('1');
    write_jsonl(
        &root,
        &[
            learning(first.clone(), "rust::eh", "first event", NOW_SEC - DAY_SECS),
            learning(
                second.clone(),
                "rust::eh",
                "second event",
                NOW_SEC - DAY_SECS,
            ),
        ],
    );
    write_lessons(
        &root,
        "rust::eh",
        vec![lesson(&first, "Cycle one lesson about the widget.")],
    );

    let handle = open(&root);
    assert_eq!(query(&handle, "widget", Some(Layer::Semantic)).len(), 1);

    // Second dream cycle rewrites the file with a different lesson set.
    write_lessons(
        &root,
        "rust::eh",
        vec![lesson(&second, "Cycle two lesson about the gasket.")],
    );
    handle
        .index_semantic_lessons(root.clone())
        .await
        .expect("re-index lessons");

    assert!(
        query(&handle, "widget", Some(Layer::Semantic)).is_empty(),
        "the prior cycle's lesson must not linger"
    );
    let now_indexed = query(&handle, "lesson", Some(Layer::Semantic));
    assert_eq!(
        now_indexed.len(),
        1,
        "exactly the new set — no duplicates, no orphans: {now_indexed:?}"
    );
    assert!(now_indexed[0].content.contains("gasket"));
    assert_eq!(now_indexed[0].event_id, format!("lsn_{}", second.as_str()));

    handle.shutdown().await.expect("shutdown");
}

// Case 7 — crash recovery. This is what the commit gate in `open` exists for.

#[tokio::test]
async fn case_07_reopen_after_index_wipe_recovers_lessons_with_a_current_watermark() {
    let (_dir, root) = scaffold();
    let exemplar = event_id('0');
    write_jsonl(
        &root,
        &[learning(
            exemplar.clone(),
            "rust::eh",
            "gasket torque drifted overnight",
            NOW_SEC - DAY_SECS,
        )],
    );

    // First open commits the episodic event and advances the watermark.
    let handle = open(&root);
    handle.shutdown().await.expect("shutdown");

    // Consolidation writes LESSONS.md, then the process dies before the index
    // commit; the rebuildable index cache is wiped, the watermark is not.
    write_lessons(
        &root,
        "rust::eh",
        vec![lesson(&exemplar, "Re-torque the gasket after every cycle.")],
    );
    let index_dir = root.dreamd_dir().join("index");
    std::fs::remove_dir_all(&index_dir).expect("wipe index dir");
    assert!(
        root.dreamd_dir().join("index_progress.json").exists(),
        "the watermark lives outside the index dir and survives the wipe — \
         so the replay has zero episodic events to index"
    );

    let handle = open(&root);
    let results = query(&handle, "torque", Some(Layer::Semantic));
    assert_eq!(
        results.len(),
        1,
        "reopen must index LESSONS.md even though the episodic replay was \
         empty: {results:?}"
    );
    assert!(results[0].score > 0.0);

    handle.shutdown().await.expect("shutdown");
}

// Case 8 — pre-first-dream state.

#[tokio::test]
async fn case_08_missing_lessons_file_leaves_episodic_recall_intact() {
    let (_dir, root) = scaffold();
    write_jsonl(
        &root,
        &[learning(
            event_id('0'),
            "rust::eh",
            "gasket torque drifted overnight",
            NOW_SEC - DAY_SECS,
        )],
    );
    assert!(!root.lessons_md().exists());

    let handle = open(&root);

    let all = query(&handle, "gasket", None);
    assert_eq!(all.len(), 1, "episodic recall is unaffected");
    assert_eq!(all[0].layer, Layer::Episodic);
    assert!(
        query(&handle, "gasket", Some(Layer::Semantic)).is_empty(),
        "no semantic documents exist before the first dream cycle"
    );

    handle.shutdown().await.expect("shutdown");
}

// Case 9 — recency is consolidation time, not the exemplar's age.

#[tokio::test]
async fn case_09_lesson_recency_is_consolidation_time_not_exemplar_age() {
    let (_dir, root) = scaffold();
    let aged_exemplar = event_id('0');
    let fresh = event_id('1');
    // Identical text, cluster, pain and importance across all three documents,
    // so BM25 and every salience factor except `exp(-age_days/14)` cancels out
    // of the ratios below. What is left measures recency and nothing else.
    const TEXT: &str = "gasket torque drifted overnight";
    write_jsonl(
        &root,
        &[
            learning(
                aged_exemplar.clone(),
                "rust::eh",
                TEXT,
                NOW_SEC - 90 * DAY_SECS,
            ),
            learning(fresh.clone(), "rust::eh", TEXT, NOW_SEC),
        ],
    );
    // `write_lessons` stamps `last_updated = NOW_SEC`: consolidation re-affirmed
    // this lesson just now, from a 90-day-old exemplar.
    write_lessons(&root, "rust::eh", vec![lesson(&aged_exemplar, TEXT)]);

    let handle = open(&root);
    let results = query(&handle, "gasket", None);
    assert_eq!(results.len(), 3, "two events + one lesson: {results:?}");

    let lesson_hit = results
        .iter()
        .find(|r| r.layer == Layer::Semantic)
        .expect("the lesson must be indexed");
    let fresh_hit = results
        .iter()
        .find(|r| r.event_id == fresh.as_str())
        .expect("the fresh event must be indexed");
    let aged_hit = results
        .iter()
        .find(|r| r.event_id == aged_exemplar.as_str())
        .expect("the aged exemplar must be indexed");

    assert_eq!(
        lesson_hit.timestamp_sec, NOW_SEC as u64,
        "a lesson's recency is when consolidation last re-affirmed it, not when \
         its exemplar fired"
    );

    // Sanity leg: the decay factor really does bury a 90-day-old claim. This is
    // the number the lesson itself would have scored under exemplar sourcing.
    let aged_ratio = aged_hit.score / fresh_hit.score;
    assert!(
        aged_ratio < 0.01,
        "a 90-day-old event must decay hard for this test to mean anything, \
         got {aged_ratio}"
    );

    let lesson_ratio = lesson_hit.score / fresh_hit.score;
    assert!(
        lesson_ratio > 0.9,
        "a freshly consolidated lesson must rank comparably to a fresh event, \
         got {lesson_ratio} — stamping the exemplar's timestamp yields \
         exp(-90/14) ~= 0.0016 and buries the lesson"
    );

    handle.shutdown().await.expect("shutdown");
}

// Case 10 — an exemplar missing from the live episodic log is skipped, never
// indexed at score 0. Rev 3 cut the snapshot fallback, so this is the whole
// answer for the narrow paths where an exemplar can still vanish.

#[tokio::test]
async fn case_10_lesson_without_an_exemplar_is_skipped_not_indexed() {
    let (_dir, root) = scaffold();
    write_jsonl(
        &root,
        &[learning(
            event_id('0'),
            "rust::eh",
            "gasket torque drifted overnight",
            NOW_SEC - DAY_SECS,
        )],
    );
    // An exemplar id the live log does not hold — `archive --force-unpin`, a
    // hand-edited `pinned: false`, or a crash between `write_lessons_file` and
    // `apply_pin_unpin`.
    let orphan = event_id('9');
    write_lessons(
        &root,
        "rust::eh",
        vec![lesson(&orphan, "Re-torque the gasket after every cycle.")],
    );

    let handle = open(&root);

    assert!(
        query(&handle, "torque", Some(Layer::Semantic)).is_empty(),
        "a lesson with no exemplar has no pain/importance to inherit; indexing \
         it would add a document that scores 0.0 and can never rank"
    );
    assert_eq!(
        query(&handle, "gasket", Some(Layer::Episodic)).len(),
        1,
        "the skip must not disturb the episodic index"
    );

    handle.shutdown().await.expect("shutdown");
}
