//! WEG-43 / DR-203 — custom BM25 × salience collector.
//!
//! Tantivy's built-in `TopDocs` collector ranks by BM25 alone. dreamd needs
//! BM25 _multiplied by_ a query-time salience score (ARCHITECTURE.md decision #2,
//! PRD FR-4.2), so the score is never indexed. This module implements the
//! [`Collector`]/[`SegmentCollector`] pair that fetches the four salience
//! fastfields per matching doc, reweights the score with [`salience`], and
//! evicts via a min-heap of size `k`.
//!
//! Production path (WEG-69): HTTP/MCP recall → `TantivyIndexHandle::reader`
//! → [`recall`]. This module owns the collector engine; the write path is
//! untouched.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use ordered_float::OrderedFloat;
use tantivy::collector::{Collector, SegmentCollector};
use tantivy::columnar::Column;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::{IndexRecordOption, TantivyDocument, Term, Value};
use tantivy::{DocAddress, DocId, IndexReader, Score, SegmentOrdinal, SegmentReader};

use crate::index::{
    Layer, SchemaFields, IMPORTANCE_FIELD, PAIN_FIELD, RECURRENCE_FIELD, TIMESTAMP_SEC_FIELD,
};
use crate::salience::{salience_with_context, RecurrenceContext};

/// One hydrated result from a salience-scored recall query. Score is
/// `f64` to preserve precision for the upcoming `--explain` formatter
/// (DR-703); Tantivy's internal `Score` is `f32` and gets widened before
/// the salience multiply.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallResult {
    /// BM25 x salience product, widened to `f64` for the `--explain` formatter (DR-703).
    pub score: f64,
    /// Full text of the learning, hydrated from Tantivy's `STORED` content field (WEG-43).
    pub content: String,
    /// Unix seconds matching `AgentLearning::timestamp` at index time.
    pub timestamp_sec: u64,
    /// 0.0..=10.0 subjective friction score as stored at index time.
    pub pain: f64,
    /// 0.0..=10.0 long-term relevance score as stored at index time.
    pub importance: f64,
    /// Cluster occurrence count as stored at index time (bounded-staleness; see WEG-42).
    pub recurrence: u64,
    /// Memory layer the document was indexed under.
    pub layer: Layer,
    /// Hierarchical clustering key hydrated from the stored `skill_action` field.
    pub skill_action: String,
    /// Provenance harness identifier hydrated from the stored `source_harness` field.
    pub source_harness: String,
    /// Raw BM25 score before the salience multiply, for the `--explain` formatter (DR-703).
    pub bm25: f64,
    /// Salience multiplier at query time, for the `--explain` formatter (DR-703).
    pub salience: f64,
}

/// Per-doc bookkeeping kept on the heap. Fast-field values are cached
/// here at `collect()` time so `merge_fruits` and the hydration loop in
/// [`recall`] never need to re-open columns.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredDoc {
    /// BM25 x salience product, stored as `OrderedFloat` for heap comparison.
    score: OrderedFloat<f64>,
    /// Tantivy address used to retrieve the stored document in [`recall`].
    doc_address: DocAddress,
    timestamp_sec: u64,
    pain: f64,
    importance: f64,
    recurrence: u64,
    /// Raw BM25 score captured before the salience multiply (Tantivy's `f32 Score` widened to `f64`).
    pub bm25: f64,
    /// Salience multiplier computed at `collect()` time from the four `FastFields`.
    pub salience: f64,
}

impl Eq for ScoredDoc {}

impl PartialOrd for ScoredDoc {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoredDoc {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score.cmp(&other.score)
    }
}

/// Per-segment collector. Tantivy invokes `collect()` once per matching
/// doc; the min-heap (`BinaryHeap<Reverse<…>>`) caps at `k` entries.
pub struct SalienceSegmentCollector {
    heap: BinaryHeap<Reverse<ScoredDoc>>,
    k: usize,
    segment_ord: SegmentOrdinal,
    timestamp_col: Column<u64>,
    pain_col: Column<f64>,
    importance_col: Column<f64>,
    recurrence_col: Column<u64>,
    now_sec: i64,
}

impl SegmentCollector for SalienceSegmentCollector {
    type Fruit = Vec<ScoredDoc>;

    fn collect(&mut self, doc: DocId, score: Score) {
        let ts = self.timestamp_col.first(doc).unwrap_or(0) as i64;
        let p = self.pain_col.first(doc).unwrap_or(0.0);
        let imp = self.importance_col.first(doc).unwrap_or(0.0);
        let rec = self.recurrence_col.first(doc).unwrap_or(0);

        // Capture bm25 and salience before multiplying — after the product they are
        // unrecoverable (0*anything=0; needed for the --explain formatter, DR-703).
        let bm25 = score as f64;
        let sal = salience_with_context(self.now_sec, ts, p, imp, RecurrenceContext::recall(rec));
        let final_score = bm25 * sal;

        let entry = Reverse(ScoredDoc {
            score: OrderedFloat(final_score),
            doc_address: DocAddress::new(self.segment_ord, doc),
            timestamp_sec: ts as u64,
            pain: p,
            importance: imp,
            recurrence: rec,
            bm25,
            salience: sal,
        });

        if self.heap.len() < self.k {
            self.heap.push(entry);
        } else if let Some(Reverse(min)) = self.heap.peek() {
            if entry.0.score > min.score {
                self.heap.pop();
                self.heap.push(entry);
            }
        }
    }

    fn harvest(self) -> Vec<ScoredDoc> {
        self.heap.into_iter().map(|Reverse(d)| d).collect()
    }
}

/// Top-level collector handed to `Searcher::search`. Stateless apart from
/// `k` and `now_sec`; per-segment work happens in [`SalienceSegmentCollector`].
pub struct SalienceCollector {
    k: usize,
    now_sec: i64,
}

impl SalienceCollector {
    pub fn new(k: usize, now_sec: i64) -> Self {
        Self { k, now_sec }
    }
}

impl Collector for SalienceCollector {
    type Fruit = Vec<ScoredDoc>;
    type Child = SalienceSegmentCollector;

    fn for_segment(
        &self,
        segment_local_id: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<SalienceSegmentCollector> {
        let ff = segment.fast_fields();
        Ok(SalienceSegmentCollector {
            heap: BinaryHeap::with_capacity(self.k + 1),
            k: self.k,
            segment_ord: segment_local_id,
            // Tantivy 0.26 `FastFieldReaders::{u64,f64}` take `&str`, not Field IDs.
            timestamp_col: ff.u64(TIMESTAMP_SEC_FIELD)?,
            pain_col: ff.f64(PAIN_FIELD)?,
            importance_col: ff.f64(IMPORTANCE_FIELD)?,
            recurrence_col: ff.u64(RECURRENCE_FIELD)?,
            now_sec: self.now_sec,
        })
    }

    fn requires_scoring(&self) -> bool {
        true
    }

    fn merge_fruits(&self, segment_fruits: Vec<Vec<ScoredDoc>>) -> tantivy::Result<Vec<ScoredDoc>> {
        // Each segment already kept its own top-k; re-apply a global min-heap of
        // size `k` so cross-segment recall cannot return more than `k` hits.
        let mut merged: BinaryHeap<Reverse<ScoredDoc>> = BinaryHeap::with_capacity(self.k + 1);
        for doc in segment_fruits.into_iter().flatten() {
            if merged.len() < self.k {
                merged.push(Reverse(doc));
            } else if let Some(Reverse(min)) = merged.peek() {
                if doc.score > min.score {
                    merged.pop();
                    merged.push(Reverse(doc));
                }
            }
        }
        Ok(merged.into_iter().map(|Reverse(d)| d).collect())
    }
}

/// Execute a salience-scored BM25 recall query.
///
/// `now_sec` is caller-provided so tests stay deterministic — same
/// invariant as `LessonsFile` (drift entry `timestamps-caller-provided-no-utc-now`):
/// no wall-clock call inside this function.
///
/// Layer filtering is wired as a `BooleanQuery` AND with a `TermQuery` on
/// the `layer` field rather than inside the collector; the collector
/// stays layer-agnostic.
pub fn recall(
    reader: &IndexReader,
    fields: &SchemaFields,
    query_text: &str,
    k: usize,
    layer: Option<Layer>,
    now_sec: i64,
) -> tantivy::Result<Vec<RecallResult>> {
    let searcher = reader.searcher();

    let query_parser = QueryParser::for_index(searcher.index(), vec![fields.content]);
    let bm25_query = query_parser.parse_query(query_text)?;

    let final_query: Box<dyn Query> = if let Some(l) = layer {
        let term = Term::from_field_text(fields.layer, l.as_str());
        let layer_query = TermQuery::new(term, IndexRecordOption::Basic);
        Box::new(BooleanQuery::new(vec![
            (Occur::Must, bm25_query),
            (Occur::Must, Box::new(layer_query)),
        ]))
    } else {
        bm25_query
    };

    let collector = SalienceCollector::new(k, now_sec);
    let top_docs: Vec<ScoredDoc> = searcher.search(&*final_query, &collector)?;

    let mut results = Vec::with_capacity(top_docs.len());
    for scored in top_docs {
        // `searcher.doc` is generic over `D: DocumentDeserialize` in 0.26;
        // annotate as `TantivyDocument` so the value-trait methods resolve.
        let doc: TantivyDocument = searcher.doc(scored.doc_address)?;
        let content = doc
            .get_first(fields.content)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let layer_val = doc
            .get_first(fields.layer)
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<Layer>().ok())
            .unwrap_or(Layer::Episodic);
        let skill_action = doc
            .get_first(fields.skill_action)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let source_harness = doc
            .get_first(fields.source_harness)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        results.push(RecallResult {
            score: scored.score.0,
            bm25: scored.bm25,
            salience: scored.salience,
            content,
            timestamp_sec: scored.timestamp_sec,
            pain: scored.pain,
            importance: scored.importance,
            recurrence: scored.recurrence,
            layer: layer_val,
            skill_action,
            source_harness,
        });
    }

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::build_schema;
    use tantivy::doc;
    use tantivy::Index;

    const DAY_SECS: i64 = 86_400;
    const NOW_SEC: i64 = 1_750_000_000;

    /// Build an in-RAM index + a populated reader for one test. Each entry
    /// is `(content, timestamp_sec, pain, importance, recurrence, layer)`.
    fn build_index_with(
        entries: &[(&str, i64, f64, f64, u64, Layer)],
    ) -> (Index, SchemaFields, IndexReader) {
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema);
        let mut writer = index.writer(15_000_000).expect("create writer");
        for (content, ts, pain, importance, recurrence, layer) in entries {
            writer
                .add_document(doc!(
                    fields.content => *content,
                    fields.timestamp_sec => *ts as u64,
                    fields.pain => *pain,
                    fields.importance => *importance,
                    fields.recurrence => *recurrence,
                    fields.layer => layer.as_str(),
                    fields.last_updated_sec => *ts as u64,
                    fields.cited_event_count => 0u64,
                    fields.skill_action => "rust::axum::error_handling",
                    fields.source_harness => "claude-code",
                ))
                .expect("add doc");
        }
        writer.commit().expect("commit");
        let reader = index.reader().expect("reader");
        (index, fields, reader)
    }

    #[test]
    fn test_zero_pain_produces_zero_score() {
        // pain=0 collapses the salience factor to 0, so BM25 × salience = 0
        // regardless of how strong the text match is.
        let (_idx, fields, reader) = build_index_with(&[(
            "axum error handling pattern",
            NOW_SEC - DAY_SECS,
            0.0,
            9.0,
            3,
            Layer::Episodic,
        )]);
        let results = recall(&reader, &fields, "axum", 10, Some(Layer::Episodic), NOW_SEC).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].score, 0.0, "pain=0 should zero the final score");
    }

    #[test]
    fn test_top_k_eviction() {
        // Six matching docs at decreasing recency → with k=3, the three
        // most-recent (highest salience) survive.
        let entries: Vec<(&str, i64, f64, f64, u64, Layer)> = (0..6)
            .map(|i| {
                (
                    "axum handler error pattern",
                    NOW_SEC - (i as i64) * DAY_SECS,
                    5.0,
                    5.0,
                    3,
                    Layer::Episodic,
                )
            })
            .collect();
        let (_idx, fields, reader) = build_index_with(&entries);
        let results = recall(&reader, &fields, "axum", 3, Some(Layer::Episodic), NOW_SEC).unwrap();
        assert_eq!(results.len(), 3, "k=3 must evict to 3 results");
        // Returned in descending score order; with constant BM25 + pain + importance,
        // salience monotonically decays with age, so order matches age ascending.
        assert!(results[0].timestamp_sec >= results[1].timestamp_sec);
        assert!(results[1].timestamp_sec >= results[2].timestamp_sec);
    }

    #[test]
    fn test_layer_filter_excludes_semantic() {
        let (_idx, fields, reader) = build_index_with(&[
            (
                "axum bug episodic",
                NOW_SEC - DAY_SECS,
                5.0,
                5.0,
                3,
                Layer::Episodic,
            ),
            (
                "axum lesson semantic",
                NOW_SEC - DAY_SECS,
                5.0,
                5.0,
                3,
                Layer::Semantic,
            ),
        ]);
        let results = recall(&reader, &fields, "axum", 10, Some(Layer::Episodic), NOW_SEC).unwrap();
        assert_eq!(results.len(), 1, "layer filter must drop semantic doc");
        assert_eq!(results[0].layer, Layer::Episodic);
        assert!(results[0].content.contains("bug"));
    }

    #[test]
    fn test_ordering_descending() {
        // Two matching docs with identical text but different pain — the
        // higher-pain doc has higher salience and must come first.
        let (_idx, fields, reader) = build_index_with(&[
            (
                "tokio select cancel",
                NOW_SEC - DAY_SECS,
                3.0,
                5.0,
                2,
                Layer::Episodic,
            ),
            (
                "tokio select cancel",
                NOW_SEC - DAY_SECS,
                9.0,
                5.0,
                2,
                Layer::Episodic,
            ),
        ]);
        let results = recall(
            &reader,
            &fields,
            "tokio",
            10,
            Some(Layer::Episodic),
            NOW_SEC,
        )
        .unwrap();
        assert_eq!(results.len(), 2);
        assert!(
            results[0].score > results[1].score,
            "results must be sorted descending: {} > {}",
            results[0].score,
            results[1].score
        );
        assert!(results[0].pain > results[1].pain);
    }

    /// Defends absolute score ordering on a deterministic 5-event corpus.
    /// The insta snapshot pins the formula output so any drift in `salience()`
    /// or the BM25 weighting is caught immediately rather than silently
    /// changing recall rankings.
    #[test]
    fn test_recall_snapshot() {
        // Hand-authored 5-event corpus, two skill_action clusters. NOW_SEC
        // is hardcoded so the salience decay is deterministic across runs.
        let entries: Vec<(&str, i64, f64, f64, u64, Layer)> = vec![
            (
                "axum requires IntoResponse on error types",
                NOW_SEC - DAY_SECS,
                8.0,
                9.0,
                3,
                Layer::Episodic,
            ),
            (
                "use ? not unwrap in axum handlers",
                NOW_SEC - 7 * DAY_SECS,
                6.0,
                7.0,
                3,
                Layer::Episodic,
            ),
            (
                "axum extractor rejection pattern",
                NOW_SEC - 30 * DAY_SECS,
                5.0,
                5.0,
                3,
                Layer::Episodic,
            ),
            (
                "tokio select cancels other futures",
                NOW_SEC - 2 * DAY_SECS,
                7.0,
                7.0,
                2,
                Layer::Episodic,
            ),
            (
                "use biased in select for shutdown",
                NOW_SEC - 14 * DAY_SECS,
                5.0,
                6.0,
                2,
                Layer::Episodic,
            ),
        ];
        let (_idx, fields, reader) = build_index_with(&entries);
        let results = recall(
            &reader,
            &fields,
            "axum error handling",
            3,
            Some(Layer::Episodic),
            NOW_SEC,
        )
        .unwrap();
        insta::with_settings!({
            // Numeric fields are formula-derived f64s — filter all three so the
            // snapshot stays stable under irrelevant floating-point drift (DR-703).
            filters => vec![
                (r"score: -?\d+\.\d+", "score: <f64>"),
                (r"bm25: -?\d+\.\d+", "bm25: <f64>"),
                (r"salience: -?\d+\.\d+", "salience: <f64>"),
            ],
        }, {
            insta::assert_debug_snapshot!(results);
        });
    }
}
