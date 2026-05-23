//! WEG-48 / DR-208 — Criterion benchmarks: P50/P99 recall latency at n=1k/10k/100k.
//!
//! Corpus is built in RAM via `Index::create_in_ram()` so these benchmarks
//! run without disk I/O and are fully hermetic. The same `build_schema()` /
//! `recall()` path used by the production indexer ensures measurements reflect
//! the real collector, not a stub.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use dreamd_core::index::{build_schema, Layer, SchemaFields};
use dreamd_core::recall;
use std::hint::black_box;
use tantivy::{doc, IndexWriter, TantivyDocument};

/// Reference timestamp (seconds) used as "now" for salience decay.
/// Fixed so benchmark results are stable across re-runs.
const NOW_SEC: i64 = 1_748_000_000;

fn build_corpus(n: usize) -> (tantivy::Index, SchemaFields) {
    let (schema, fields) = build_schema();
    let index = tantivy::Index::create_in_ram(schema);
    // 50 MB heap — matches the WEG-42 production writer budget.
    let mut writer: IndexWriter<TantivyDocument> = index.writer(50_000_000).unwrap();

    for i in 0..n {
        let age_secs = (i as u64 % 90) * 86_400;
        let ts = (NOW_SEC as u64).saturating_sub(age_secs);
        writer
            .add_document(doc!(
                fields.content           => format!("rust error handling tokio async {i}"),
                fields.timestamp_sec     => ts,
                fields.pain              => ((i % 10) as f64 + 1.0),
                fields.importance        => ((i % 10) as f64 + 1.0),
                fields.recurrence        => ((i % 5 + 1) as u64),
                fields.layer             => Layer::Episodic.as_str().to_string(),
                fields.last_updated_sec  => ts,
                fields.cited_event_count => 0u64,
                fields.event_id          => format!("evt_BENCH{:026}", i),
            ))
            .unwrap();
    }
    writer.commit().unwrap();
    (index, fields)
}

fn bench_recall(c: &mut Criterion) {
    let mut group = c.benchmark_group("recall");

    for &n in &[1_000_usize, 10_000, 100_000] {
        let (index, fields) = build_corpus(n);
        let reader = index.reader().unwrap();

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

criterion_group!(benches, bench_recall);
criterion_main!(benches);
