//! WEG-46 / DR-206 — BM25 + FastField integration test.
//!
//! Backs the wedge claim "recent painful important learnings outrank stale
//! benign ones on identical lexical-match queries" with a green CI check.
//! Uses the DR-922 demo-corpus fixture; canonical expectations live in
//! `tests/fixtures/demo-corpus/EXPECTED.md`. First consumer of that fixture.
//!
//! Per-query assertion layers:
//!   1. ordering (strict for Query A; set membership for Query B — see
//!      EXPECTED.md §Query B on the BM25 reorder window)
//!   2. salience matches EXPECTED.md within ±1e-3
//!   3. decomposition identity: `recall.score == bm25 * salience` ±1e-9,
//!      where `bm25` comes from a parallel `TopDocs` search against the
//!      same parsed content query on the same searcher.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use tantivy::collector::TopDocs;
use tantivy::doc;
use tantivy::query::QueryParser;
use tantivy::schema::{TantivyDocument, Value};
use tantivy::{Index, IndexReader};

use dreamd_core::index::{build_schema, Layer, SchemaFields};
use dreamd_core::salience::{salience_with_context, RecurrenceContext};
use dreamd_core::{recall, RecallResult};

/// Reference clock from EXPECTED.md §Reference clock: 2026-06-02T12:00:00Z.
/// All age computations resolve against this; no `Utc::now()` in this test.
const NOW_SEC: i64 = 1_780_401_600;

// Top-3 result IDs from EXPECTED.md (2026-05-16 amendment), keyed by
// timestamp_sec for unique matching against RecallResult (the schema
// doesn't STORE the ULID).
const E1_TS: u64 = 1_780_228_800; // 2026-05-31T12:00:00Z, evt_01JR05311200... (result_vs_panic)
const E2_TS: u64 = 1_780_052_400; // 2026-05-29T11:00:00Z, evt_01JR05291100... (tokio_select_branches)
const E3_TS: u64 = 1_779_699_600; // 2026-05-25T09:00:00Z, evt_01JR05250900... (thiserror_derive)

const E1_SALIENCE: f64 = 1.8323;
const E2_SALIENCE: f64 = 0.8716;
const E3_SALIENCE: f64 = 0.3876;

/// EXPECTED.md §Note on rounding: 4-dp values vs faithful f64 diverge
/// by up to ±5e-4 for E1–E3; tolerance band is 1e-3.
const SALIENCE_TOL: f64 = 1e-3;

/// `RecallResult.score` is `(bm25 as f64) * salience`. Both factors are
/// computed in the same precision, so the identity should hold to within
/// a handful of ULPs; 1e-9 is comfortably above that on these magnitudes.
const DECOMPOSITION_TOL: f64 = 1e-9;

#[derive(Deserialize)]
struct FixtureRecord {
    #[allow(dead_code)] // present in the JSONL but not asserted on here
    id: String,
    timestamp: DateTime<Utc>,
    pain: f64,
    importance: f64,
    skill_action: String,
    content: String,
}

fn load_fixture() -> Vec<FixtureRecord> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/demo-corpus/.agent/episodic/AGENT_LEARNINGS.jsonl");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read demo corpus at {}: {e}", path.display()));
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("parse demo-corpus record"))
        .collect()
}

/// Build an in-RAM Tantivy index from the fixture and derive `recurrence`
/// per `skill_action` cluster from the count of records sharing it. Mirrors
/// WEG-42's planned index-time recurrence semantics (demo-corpus/README.md
/// "Recurrence note"); the production indexer isn't shipped yet, so the test
/// performs the derivation locally.
fn build_index_from_fixture(records: &[FixtureRecord]) -> (Index, SchemaFields, IndexReader) {
    let (schema, fields) = build_schema();
    let index = Index::create_in_ram(schema);

    let mut cluster_counts: HashMap<&str, u64> = HashMap::new();
    for r in records {
        *cluster_counts.entry(r.skill_action.as_str()).or_default() += 1;
    }

    let mut writer = index.writer(15_000_000).expect("index writer");
    for r in records {
        let ts = r.timestamp.timestamp() as u64;
        let recurrence = cluster_counts[r.skill_action.as_str()];
        writer
            .add_document(doc!(
                fields.content => r.content.clone(),
                fields.timestamp_sec => ts,
                fields.pain => r.pain,
                fields.importance => r.importance,
                fields.recurrence => recurrence,
                fields.layer => Layer::Episodic.as_str(),
                fields.last_updated_sec => ts,
                fields.cited_event_count => 0u64,
            ))
            .expect("add doc");
    }
    writer.commit().expect("commit");
    let reader = index.reader().expect("reader");
    (index, fields, reader)
}

/// Parallel BM25 lookup: run the same parsed content query against the same
/// searcher with `TopDocs::with_limit`, then key results by content (each
/// fixture record's content is unique). Limit is `records.len()` so no
/// salience-favored doc can be evicted before the join.
fn bm25_by_content(
    reader: &IndexReader,
    fields: &SchemaFields,
    query_text: &str,
    limit: usize,
) -> HashMap<String, f64> {
    let searcher = reader.searcher();
    let parser = QueryParser::for_index(searcher.index(), vec![fields.content]);
    let query = parser.parse_query(query_text).expect("parse query");
    // Tantivy 0.26: `TopDocs::with_limit` is a builder; `.order_by_score()`
    // produces the `Collector<Fruit = Vec<(Score, DocAddress)>>` impl.
    let hits = searcher
        .search(&query, &TopDocs::with_limit(limit).order_by_score())
        .expect("topdocs search");
    let mut out = HashMap::with_capacity(hits.len());
    for (score, addr) in hits {
        let doc: TantivyDocument = searcher.doc(addr).expect("hydrate doc");
        let content = doc
            .get_first(fields.content)
            .and_then(|v| v.as_str())
            .expect("content stored")
            .to_string();
        out.insert(content, score as f64);
    }
    out
}

/// Recompute salience from a `RecallResult`'s fastfield-cached values
/// (NOT from `result.score` — that would tautologically pass the
/// decomposition check). Inputs flow: index FastFields → SalienceCollector
/// → RecallResult → here → `salience()` → assert.
fn recompute_salience(r: &RecallResult) -> f64 {
    salience_with_context(
        NOW_SEC,
        r.timestamp_sec as i64,
        r.pain,
        r.importance,
        RecurrenceContext::recall(r.recurrence),
    )
}

fn assert_decomposition_identity(
    results: &[RecallResult],
    bm25_map: &HashMap<String, f64>,
    query_label: &str,
) {
    for r in results {
        let bm25 = bm25_map
            .get(&r.content)
            .copied()
            .unwrap_or_else(|| panic!("{query_label}: no BM25 hit for content {:?}", r.content));
        let s = recompute_salience(r);
        let expected = bm25 * s;
        let diff = (r.score - expected).abs();
        assert!(
            diff < DECOMPOSITION_TOL,
            "{query_label}: decomposition identity failed: \
             recall.score={} bm25*salience={} diff={} (tol={DECOMPOSITION_TOL:e})",
            r.score,
            expected,
            diff,
        );
    }
}

#[test]
fn query_a_rust_error_handling_top3_strict_ordering() {
    let records = load_fixture();
    let (_idx, fields, reader) = build_index_from_fixture(&records);

    let results =
        recall(&reader, &fields, "rust error handling", 3, None, NOW_SEC).expect("recall Query A");

    assert_eq!(results.len(), 3, "Query A: expected top-3");

    // Layer 1: strict ordering — EXPECTED.md §Query A pins E1 > E2 > E3.
    assert_eq!(results[0].timestamp_sec, E1_TS, "Query A rank 1 != E1");
    assert_eq!(results[1].timestamp_sec, E2_TS, "Query A rank 2 != E2");
    assert_eq!(results[2].timestamp_sec, E3_TS, "Query A rank 3 != E3");

    // Layer 2: salience values match EXPECTED.md within ±1e-3.
    let s1 = recompute_salience(&results[0]);
    let s2 = recompute_salience(&results[1]);
    let s3 = recompute_salience(&results[2]);
    assert!(
        (s1 - E1_SALIENCE).abs() < SALIENCE_TOL,
        "E1 salience {s1} vs expected {E1_SALIENCE}"
    );
    assert!(
        (s2 - E2_SALIENCE).abs() < SALIENCE_TOL,
        "E2 salience {s2} vs expected {E2_SALIENCE}"
    );
    assert!(
        (s3 - E3_SALIENCE).abs() < SALIENCE_TOL,
        "E3 salience {s3} vs expected {E3_SALIENCE}"
    );

    // Layer 3: decomposition identity recall.score ≈ bm25 × salience ±1e-9.
    let bm25_map = bm25_by_content(&reader, &fields, "rust error handling", records.len());
    assert_decomposition_identity(&results, &bm25_map, "Query A");
}

#[test]
fn query_b_error_handling_rust_top3_set_membership() {
    let records = load_fixture();
    let (_idx, fields, reader) = build_index_from_fixture(&records);

    let results =
        recall(&reader, &fields, "error handling rust", 3, None, NOW_SEC).expect("recall Query B");

    assert_eq!(results.len(), 3, "Query B: expected top-3");

    // Layer 1: set membership only — EXPECTED.md §Query B notes BM25 may
    // reorder rows 1–3 within {E1, E2, E3} depending on term-order weighting,
    // so positional assertions would be brittle. The wedge claim is set
    // dominance: the freshest/most-painful cluster wins the podium.
    let expected_set: HashSet<u64> = [E1_TS, E2_TS, E3_TS].into_iter().collect();
    let actual_set: HashSet<u64> = results.iter().map(|r| r.timestamp_sec).collect();
    assert_eq!(
        actual_set, expected_set,
        "Query B top-3 set must be {{E1, E2, E3}}, got {actual_set:?}"
    );

    // Layer 2: salience per known timestamp matches EXPECTED.md within ±1e-3,
    // independent of positional rank.
    let expected_sal: HashMap<u64, f64> = [
        (E1_TS, E1_SALIENCE),
        (E2_TS, E2_SALIENCE),
        (E3_TS, E3_SALIENCE),
    ]
    .into_iter()
    .collect();
    for r in &results {
        let s = recompute_salience(r);
        let want = expected_sal[&r.timestamp_sec];
        assert!(
            (s - want).abs() < SALIENCE_TOL,
            "Query B ts={} salience={} vs expected={}",
            r.timestamp_sec,
            s,
            want,
        );
    }

    // Layer 3: decomposition identity.
    let bm25_map = bm25_by_content(&reader, &fields, "error handling rust", records.len());
    assert_decomposition_identity(&results, &bm25_map, "Query B");
}

/// Sanity: results are sorted by `score` descending. Defends against a
/// regression that swaps the final `sort_by` in `recall()` to ascending,
/// which would still produce three "correct" timestamps for Query A on
/// this fixture (because the same three docs win either way) but flip
/// the rank order and silently break the wedge claim.
#[test]
fn recall_results_are_score_descending() {
    let records = load_fixture();
    let (_idx, fields, reader) = build_index_from_fixture(&records);
    let results = recall(&reader, &fields, "rust error handling", 5, None, NOW_SEC).unwrap();
    for pair in results.windows(2) {
        assert!(
            pair[0].score >= pair[1].score,
            "results not sorted descending: {} then {}",
            pair[0].score,
            pair[1].score,
        );
    }
}
