//! BZR-1242 — Criterion benchmarks for recall with the recurrence table.
//!
//! The corpus has the same content shape and query as `benches/recall.rs`, plus
//! realistic keys: 120 three-segment `skill_action`s under 6 first segments,
//! assigned with a deterministic skew so the most common key holds about 20%
//! of the docs, and about 1% `layer:semantic` lesson docs keyed at two-segment
//! prefixes.
//!
//! - `recall_subtree`: `recall` over a `RecallReader` whose table is warmed
//!   before measurement. The gate is a 10k P50 under 1 ms.
//! - `recurrence_cold`: a fresh `RecurrenceWarmer` builds the table for the
//!   whole searcher. Informational: the cost of a first warm.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use dreamd_core::index::{build_schema, Layer, SchemaFields};
use dreamd_core::recurrence::RecurrenceWarmer;
use dreamd_core::{recall, RecallReader};
use std::hint::black_box;
use tantivy::{doc, IndexWriter, ReloadPolicy, TantivyDocument};

/// Reference timestamp (seconds) used as "now" for salience decay.
/// Fixed so benchmark results are stable across re-runs.
const NOW_SEC: i64 = 1_748_000_000;

const LANGUAGES: [&str; 6] = ["rust", "go", "python", "typescript", "java", "ruby"];
const AREAS: [&str; 5] = ["error_handling", "async", "testing", "build", "io"];
const ACTIONS: [&str; 4] = ["retry", "timeout", "rejection", "config"];
const KEYS: usize = LANGUAGES.len() * AREAS.len() * ACTIONS.len();

/// The `rank`-th key, `language::area::action`.
fn key(rank: usize) -> [&'static str; 3] {
    [
        LANGUAGES[rank % LANGUAGES.len()],
        AREAS[(rank / LANGUAGES.len()) % AREAS.len()],
        ACTIONS[rank / (LANGUAGES.len() * AREAS.len())],
    ]
}

/// A skewed rank for doc `i`: `u` steps through [0, 1) in a fixed order, and
/// `floor(KEYS · u³)` gives rank 0 about 20% of the docs and every rank at
/// least two in each block of 1,000.
fn rank(i: usize) -> usize {
    let u = ((i * 761) % 1000) as f64 / 1000.0;
    ((KEYS as f64) * u.powi(3)) as usize
}

fn build_corpus(n: usize) -> (tantivy::Index, SchemaFields) {
    let (schema, fields) = build_schema();
    let index = tantivy::Index::create_in_ram(schema);
    // 50 MB heap — matches the WEG-42 production writer budget.
    let mut writer: IndexWriter<TantivyDocument> = index.writer(50_000_000).unwrap();

    for i in 0..n {
        let age_secs = (i as u64 % 90) * 86_400;
        let ts = (NOW_SEC as u64).saturating_sub(age_secs);
        let [language, area, action] = key(rank(i));
        // One doc in 100 is a lesson, keyed at its two-segment prefix.
        let (layer, event_id, skill_action) = if i % 100 == 99 {
            (
                Layer::Semantic,
                format!("lsn_evt_BENCH{i:026}"),
                format!("{language}::{area}"),
            )
        } else {
            (
                Layer::Episodic,
                format!("evt_BENCH{i:026}"),
                format!("{language}::{area}::{action}"),
            )
        };
        writer
            .add_document(doc!(
                fields.content           => format!("rust error handling tokio async {i}"),
                fields.timestamp_sec     => ts,
                fields.pain              => ((i % 10) as f64 + 1.0),
                fields.importance        => ((i % 10) as f64 + 1.0),
                fields.recurrence        => 0u64,
                fields.layer             => layer.as_str().to_string(),
                fields.last_updated_sec  => ts,
                fields.cited_event_count => 0u64,
                fields.event_id          => event_id,
                fields.skill_action      => skill_action,
            ))
            .unwrap();
    }
    writer.commit().unwrap();
    (index, fields)
}

fn bench_recall_subtree(c: &mut Criterion) {
    let mut group = c.benchmark_group("recall_subtree");

    for &n in &[1_000_usize, 10_000, 100_000] {
        let (index, fields) = build_corpus(n);
        // Opening the reader warms the table for the committed corpus; the
        // explicit read below only confirms it is there.
        let reader = RecallReader::open(&index, ReloadPolicy::OnCommitWithDelay).unwrap();
        reader.recurrence(&reader.searcher()).unwrap();

        group.bench_with_input(BenchmarkId::new("n", n), &n, |b, _| {
            b.iter(|| {
                let results = recall(
                    black_box(&reader),
                    black_box(&fields),
                    black_box("rust error"),
                    black_box(10),
                    black_box(None),
                    black_box(NOW_SEC),
                )
                .unwrap();
                black_box(results);
            });
        });
    }

    group.finish();
}

fn bench_recurrence_cold(c: &mut Criterion) {
    let mut group = c.benchmark_group("recurrence_cold");

    for &n in &[10_000_usize, 100_000] {
        let (index, _) = build_corpus(n);
        let reader = RecallReader::open(&index, ReloadPolicy::OnCommitWithDelay).unwrap();
        let searcher = reader.searcher();

        group.bench_with_input(BenchmarkId::new("n", n), &n, |b, _| {
            b.iter(|| {
                let warmer = RecurrenceWarmer::new(&index.schema()).unwrap();
                black_box(warmer.tables(black_box(&searcher)).unwrap());
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_recall_subtree, bench_recurrence_cold);
criterion_main!(benches);
