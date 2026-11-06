//! Custom BM25 × salience collector (WEG-43).
//!
//! Tantivy's built-in `TopDocs` collector ranks by BM25 alone. dreamd needs
//! BM25 _multiplied by_ a query-time salience score (ARCHITECTURE.md decision #2),
//! so the score is never indexed. This module implements the
//! [`Collector`]/[`SegmentCollector`] pair that fetches the three salience
//! fastfields and the searcher's recurrence table (`crate::recurrence`) per
//! matching doc, reweights the score with
//! [`salience_with_context`], and evicts via a min-heap of size `k`.
//!
//! Production path: HTTP/MCP recall → `TantivyIndexHandle::reader` → [`recall`].

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ops::Bound;
use std::sync::Arc;

use ordered_float::OrderedFloat;
use tantivy::collector::{Collector, SegmentCollector};
use tantivy::columnar::Column;
use tantivy::query::{
    AllQuery, BooleanQuery, Occur, Query, QueryParser, RangeQuery, TermQuery, TermSetQuery,
};
use tantivy::schema::{IndexRecordOption, TantivyDocument, Term, Value};
use tantivy::{DocAddress, DocId, Score, SegmentOrdinal, SegmentReader, TantivyError};

use crate::index::{Layer, SchemaFields, IMPORTANCE_FIELD, PAIN_FIELD, TIMESTAMP_SEC_FIELD};
use crate::recurrence::{RecallReader, RecurrenceTables, SegmentRecurrence};
use crate::salience::{salience_with_context, RecurrenceContext};

/// One hydrated result from a salience-scored recall query. Score is
/// `f64` to preserve precision for `dreamd recall --explain`;
/// Tantivy's internal `Score` is `f32` and gets widened before the
/// salience multiply.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallResult {
    /// BM25 x salience product, widened to `f64` for the `--explain` formatter.
    pub score: f64,
    /// Full text of the learning, hydrated from Tantivy's `STORED` content field (WEG-43).
    pub content: String,
    /// Unix seconds matching `AgentLearning::timestamp` at index time.
    pub timestamp_sec: u64,
    /// 0.0..=10.0 subjective friction score as stored at index time.
    pub pain: f64,
    /// 0.0..=10.0 long-term relevance score as stored at index time.
    pub importance: f64,
    /// Alive episodic docs in the searcher whose `skill_action` equals this
    /// doc's or extends it with `::` (`crate::recurrence`, BZR-1242).
    pub recurrence: u64,
    /// Memory layer the document was indexed under.
    pub layer: Layer,
    /// Hierarchical clustering key hydrated from the stored `skill_action` field.
    pub skill_action: String,
    /// Provenance harness identifier hydrated from the stored `source_harness` field.
    pub source_harness: String,
    /// Raw BM25 score before the salience multiply, for the `--explain` formatter.
    pub bm25: f64,
    /// Salience multiplier at query time, for the `--explain` formatter.
    pub salience: f64,
    /// Event id hydrated from the `STORED` `event_id` field (WEG-45).
    /// Empty string when the document predates the field or carries no id;
    /// surfaced as the `id` column by the `dreamd recall` formatter (WEG-51).
    pub event_id: String,
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
    /// Salience multiplier computed at `collect()` time from the three `FastFields`
    /// and the recurrence table.
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

/// Documents to drop before the top-k heap is filled (BZR-194).
///
/// Counterfactual recall: "what would recall return if these memories were
/// absent?" The index is never modified. [`recall_excluding`] turns the
/// exclusion into `MustNot` clauses of the query (BZR-1243), so matching
/// documents are dropped before scoring and the remaining hits backfill `k`.
/// Nothing reads the doc store for it.
///
/// * `ids` match the `event_id` term exactly. An `evt_…` id does not match
///   the `lsn_evt_…` lesson document derived from it.
/// * `cluster_prefixes` match `skill_action` per [`cluster_prefix_matches`],
///   on both layers: the exact key's term, and every term in the half-open
///   range `[prefix::, prefix:;)`.
#[derive(Debug, Clone, Default)]
pub struct RecallExclusion {
    pub ids: Vec<String>,
    pub cluster_prefixes: Vec<String>,
}

impl RecallExclusion {
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty() && self.cluster_prefixes.is_empty()
    }

    /// The reference rule the query clauses implement.
    #[cfg(test)]
    fn excludes(&self, event_id: &str, skill_action: &str) -> bool {
        self.ids.iter().any(|id| id == event_id)
            || self
                .cluster_prefixes
                .iter()
                .any(|prefix| cluster_prefix_matches(skill_action, prefix))
    }
}

/// `true` when `skill_action` is `prefix` or extends it with a `::` segment.
///
/// `rust::error_handling` matches itself and `rust::error_handling::axum`, not
/// `rust`, `rustacean`, or `rust::errors`. `skill_action` is one exact STRING
/// token in the index; [`recall_excluding`] matches the same keys with one
/// term and one term range.
pub fn cluster_prefix_matches(skill_action: &str, prefix: &str) -> bool {
    skill_action == prefix
        || skill_action
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with("::"))
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
    recurrence: Arc<SegmentRecurrence>,
    now_sec: i64,
}

impl SegmentCollector for SalienceSegmentCollector {
    type Fruit = Vec<ScoredDoc>;

    fn collect(&mut self, doc: DocId, score: Score) {
        let ts = self.timestamp_col.first(doc).unwrap_or(0) as i64;
        let p = self.pain_col.first(doc).unwrap_or(0.0);
        let imp = self.importance_col.first(doc).unwrap_or(0.0);
        let rec = u64::from(self.recurrence.get(doc));

        // Capture bm25 and salience before multiplying — after the product they are
        // unrecoverable (0*anything=0; needed for the --explain formatter).
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
/// `k`, `now_sec` and the searcher's recurrence table; per-segment work
/// happens in [`SalienceSegmentCollector`].
pub struct SalienceCollector {
    k: usize,
    now_sec: i64,
    recurrence: Arc<RecurrenceTables>,
}

impl SalienceCollector {
    /// `recurrence` must be the table of the searcher this collector runs on
    /// ([`RecallReader::recurrence`]).
    pub fn new(k: usize, now_sec: i64, recurrence: Arc<RecurrenceTables>) -> Self {
        Self {
            k,
            now_sec,
            recurrence,
        }
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
        // The table is built from this searcher, so a segment it lacks is a
        // bug, never recurrence 0.
        let recurrence = self
            .recurrence
            .segment(segment.segment_id())
            .ok_or_else(|| {
                TantivyError::InternalError(format!(
                    "segment {} has no recurrence table",
                    segment.segment_id().uuid_string()
                ))
            })?;
        Ok(SalienceSegmentCollector {
            heap: BinaryHeap::with_capacity(self.k + 1),
            k: self.k,
            segment_ord: segment_local_id,
            // The pinned Tantivy's `FastFieldReaders::{u64,f64}` take `&str`, not Field IDs.
            timestamp_col: ff.u64(TIMESTAMP_SEC_FIELD)?,
            pain_col: ff.f64(PAIN_FIELD)?,
            importance_col: ff.f64(IMPORTANCE_FIELD)?,
            recurrence,
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

/// Hydrate [`ScoredDoc`] fruit into [`RecallResult`]s and sort descending by score.
///
/// Shared by [`recall`] and [`score_by_salience`]. `doc_address` is private to
/// this module on [`ScoredDoc`], so hydration must stay inside it.
fn hydrate_scored(
    searcher: &tantivy::Searcher,
    fields: &SchemaFields,
    top_docs: Vec<ScoredDoc>,
) -> tantivy::Result<Vec<RecallResult>> {
    Ok(hydrate_addressed(searcher, fields, top_docs)?
        .into_iter()
        .map(|(_, result)| result)
        .collect())
}

/// [`hydrate_scored`], keeping each result's [`DocAddress`] so
/// [`rank_missing_tokens`] can tell documents apart across searches.
fn hydrate_addressed(
    searcher: &tantivy::Searcher,
    fields: &SchemaFields,
    top_docs: Vec<ScoredDoc>,
) -> tantivy::Result<Vec<(DocAddress, RecallResult)>> {
    let mut results = Vec::with_capacity(top_docs.len());
    for scored in top_docs {
        // `searcher.doc` is generic over `D: DocumentDeserialize`;
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
        let event_id = doc
            .get_first(fields.event_id)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        results.push((
            scored.doc_address,
            RecallResult {
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
                event_id,
            },
        ));
    }

    results.sort_by(|a, b| {
        b.1.score
            .partial_cmp(&a.1.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(results)
}

/// Execute a salience-scored BM25 recall query.
///
/// `now_sec` is caller-provided so tests stay deterministic: this function
/// reads no clock.
pub fn recall(
    reader: &RecallReader,
    fields: &SchemaFields,
    query_text: &str,
    k: usize,
    now_sec: i64,
) -> tantivy::Result<Vec<RecallResult>> {
    recall_excluding(
        reader,
        fields,
        query_text,
        k,
        &RecallExclusion::default(),
        now_sec,
    )
}

/// [`recall`] with counterfactual exclusion (BZR-194).
///
/// A non-empty `exclusion` wraps the query in a [`BooleanQuery`]: the query as
/// one `Must` clause, then `MustNot` clauses for the excluded ids (one
/// [`TermSetQuery`] on `event_id`) and for each cluster prefix (its exact
/// `skill_action` term, and a [`RangeQuery`] over its `::` subtree). Matches
/// are dropped before scoring, so up to `k` of the remaining hits come back,
/// and `MustNot` clauses add no score, so every remaining hit scores as it
/// does in [`recall`] (BZR-1243). The index is not modified. An empty
/// exclusion searches the unchanged query, so it is exactly [`recall`].
///
/// A plain multi-word query whose top hit lacks a query token that another
/// hit's content has is re-ranked by [`rank_missing_tokens`] (BZR-1522).
/// Every other query returns the one search above, scores included.
pub fn recall_excluding(
    reader: &RecallReader,
    fields: &SchemaFields,
    query_text: &str,
    k: usize,
    exclusion: &RecallExclusion,
    now_sec: i64,
) -> tantivy::Result<Vec<RecallResult>> {
    let searcher = reader.searcher();
    let recurrence = reader.recurrence(&searcher)?;
    let query_parser = QueryParser::for_index(searcher.index(), vec![fields.content]);

    // One parsed query, the exclusion's `MustNot` clauses, and the salience
    // collector. The bag and every token search go through it.
    let search = |text: &str, limit: usize| -> tantivy::Result<Vec<(DocAddress, RecallResult)>> {
        let final_query = exclude(query_parser.parse_query(text)?, fields, exclusion);
        let collector = SalienceCollector::new(limit, now_sec, Arc::clone(&recurrence));
        let top_docs: Vec<ScoredDoc> = searcher.search(&*final_query, &collector)?;
        hydrate_addressed(&searcher, fields, top_docs)
    };

    let bag = search(query_text, k)?;
    rank_missing_tokens(bag, query_text, k, search)
}

/// The query's tokens when it is plain: two or more whitespace-separated
/// pieces, each `[A-Za-z0-9_]+`. Lowercased, first occurrence kept. `None`
/// for one piece or for any other character (BZR-1522).
fn plain_tokens(query_text: &str) -> Option<Vec<String>> {
    let pieces: Vec<&str> = query_text.split_whitespace().collect();
    let plain = pieces.len() >= 2
        && pieces.iter().all(|piece| {
            piece
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        });
    if !plain {
        return None;
    }
    let mut tokens: Vec<String> = Vec::with_capacity(pieces.len());
    for piece in pieces {
        let token = piece.to_ascii_lowercase();
        if !tokens.contains(&token) {
            tokens.push(token);
        }
    }
    Some(tokens)
}

/// `true` when `content`, split on every character that is not ASCII
/// alphanumeric or `_` and lowercased, yields `token`. `trust` does not have
/// `rust`.
fn content_has(content: &str, token: &str) -> bool {
    content
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|piece| piece.eq_ignore_ascii_case(token))
}

/// BZR-1522: rank the event that holds the query tokens the bag's top hit lacks.
///
/// `bag` is the one-query result, sorted as [`recall`] returns it. It comes
/// back untouched unless the query is plain ([`plain_tokens`]), the top hit's
/// content lacks at least one token, and a search for a missing token finds
/// a hit with another `event_id` whose content has every missing token
/// (Amendment 1). Then the bag and those hits are rescored:
///
/// * a hit with another `event_id` than the top hit, whose content has
///   every missing token, gets `bm25 = bag_bm25 + top.bm25 + 1.0`;
/// * every other hit keeps its bag BM25;
/// * `bag_bm25` is the hit's score for the whole query, from a bag re-run
///   with room for the token hits, and `0` for a hit outside it;
/// * `score = bm25 * salience`, with the salience the collector computed.
///
/// The result is sorted by score descending, then `event_id` ascending, and
/// cut to `k`. `salience()` and recurrence are not touched, so `--explain`'s
/// product still equals the score.
fn rank_missing_tokens(
    bag: Vec<(DocAddress, RecallResult)>,
    query_text: &str,
    k: usize,
    search: impl Fn(&str, usize) -> tantivy::Result<Vec<(DocAddress, RecallResult)>>,
) -> tantivy::Result<Vec<RecallResult>> {
    let untouched =
        |bag: Vec<(DocAddress, RecallResult)>| bag.into_iter().map(|(_, result)| result).collect();

    let Some(tokens) = plain_tokens(query_text) else {
        return Ok(untouched(bag));
    };
    let Some((_, top)) = bag.first() else {
        return Ok(untouched(bag));
    };
    let missing: Vec<String> = tokens
        .into_iter()
        .filter(|token| !content_has(&top.content, token))
        .collect();
    if missing.is_empty() {
        return Ok(untouched(bag));
    }

    let top_event_id = top.event_id.clone();
    let top_bm25 = top.bm25;
    let promoted = |hit: &RecallResult| {
        hit.event_id != top_event_id && missing.iter().all(|token| content_has(&hit.content, token))
    };

    let mut token_hits: Vec<(DocAddress, RecallResult)> = Vec::new();
    for token in &missing {
        for (address, hit) in search(token, k)? {
            if promoted(&hit) && !token_hits.iter().any(|(seen, _)| *seen == address) {
                token_hits.push((address, hit));
            }
        }
    }
    if token_hits.is_empty() {
        return Ok(untouched(bag));
    }

    let mut union = bag;
    let bag_len = union.len();
    for (address, hit) in token_hits.iter().cloned() {
        if !union.iter().any(|(seen, _)| *seen == address) {
            union.push((address, hit));
        }
    }

    // A hit found only by a token search may still sit inside a wider bag.
    let wide = if union.len() > bag_len {
        search(query_text, k + token_hits.len())?
    } else {
        union[..bag_len].to_vec()
    };
    let bag_bm25 = |address: DocAddress| {
        wide.iter()
            .find(|(seen, _)| *seen == address)
            .map_or(0.0, |(_, hit)| hit.bm25)
    };

    let mut ranked: Vec<RecallResult> = union
        .into_iter()
        .map(|(address, mut hit)| {
            let bm25 = bag_bm25(address);
            hit.bm25 = if promoted(&hit) {
                bm25 + top_bm25 + 1.0
            } else {
                bm25
            };
            hit.score = hit.bm25 * hit.salience;
            hit
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.event_id.cmp(&b.event_id))
    });
    ranked.truncate(k);
    Ok(ranked)
}

/// `base` with `exclusion`'s `MustNot` clauses; `base` itself when the
/// exclusion is empty.
fn exclude(
    base: Box<dyn Query>,
    fields: &SchemaFields,
    exclusion: &RecallExclusion,
) -> Box<dyn Query> {
    if exclusion.is_empty() {
        return base;
    }
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, base)];
    if !exclusion.ids.is_empty() {
        let ids = exclusion
            .ids
            .iter()
            .map(|id| Term::from_field_text(fields.event_id, id));
        clauses.push((Occur::MustNot, Box::new(TermSetQuery::new(ids))));
    }
    for prefix in &exclusion.cluster_prefixes {
        let key = |text: &str| Term::from_field_text(fields.skill_action, text);
        clauses.push((
            Occur::MustNot,
            Box::new(TermQuery::new(key(prefix), IndexRecordOption::Basic)),
        ));
        // `prefix::…`: `;` is the byte after `:`, the same half-open range as
        // `ClusterCounts::subtree_bytes`.
        clauses.push((
            Occur::MustNot,
            Box::new(RangeQuery::new(
                Bound::Included(key(&format!("{prefix}::"))),
                Bound::Excluded(key(&format!("{prefix}:;"))),
            )),
        ));
    }
    Box::new(BooleanQuery::new(clauses))
}

/// Top-`n` documents by query-time salience with no lexical filter (WEG-52).
///
/// Uses Tantivy [`AllQuery`] (every doc matches with score **1.0**) so
/// [`SalienceCollector`] heap order is salience order. Printed `bm25 ≈ 1` and
/// `score ≈ salience` under this path is expected — not a second collector.
///
/// `now_sec` is caller-provided (same invariant as [`recall`]).
pub fn score_by_salience(
    reader: &RecallReader,
    fields: &SchemaFields,
    n: usize,
    now_sec: i64,
) -> tantivy::Result<Vec<RecallResult>> {
    let searcher = reader.searcher();
    let recurrence = reader.recurrence(&searcher)?;
    let collector = SalienceCollector::new(n, now_sec, recurrence);
    let top_docs: Vec<ScoredDoc> = searcher.search(&AllQuery, &collector)?;
    hydrate_scored(&searcher, fields, top_docs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::build_schema;
    use tantivy::doc;
    use tantivy::{Index, ReloadPolicy};

    const DAY_SECS: i64 = 86_400;
    const NOW_SEC: i64 = 1_750_000_000;

    /// Build an in-RAM index + a populated reader for one test. Each entry
    /// is `(content, timestamp_sec, pain, importance, skill_action, layer)`.
    fn build_index_with(
        entries: &[(&str, i64, f64, f64, &str, Layer)],
    ) -> (Index, SchemaFields, RecallReader) {
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema);
        let mut writer = index.writer(15_000_000).expect("create writer");
        for (content, ts, pain, importance, skill_action, layer) in entries {
            writer
                .add_document(doc!(
                    fields.content => *content,
                    fields.timestamp_sec => *ts as u64,
                    fields.pain => *pain,
                    fields.importance => *importance,
                    fields.layer => layer.as_str(),
                    fields.last_updated_sec => *ts as u64,
                    fields.cited_event_count => 0u64,
                    fields.skill_action => *skill_action,
                    fields.source_harness => "claude-code",
                ))
                .expect("add doc");
        }
        writer.commit().expect("commit");
        let reader = RecallReader::open(&index, ReloadPolicy::OnCommitWithDelay).expect("reader");
        (index, fields, reader)
    }

    /// `scripts/benchmark/recall_gate.py` `single_anchor` (BZR-1522): the two
    /// events share a timestamp, pain 6 and importance 6. The queue event
    /// lacks `redis`; the redis event has it and must rank first.
    #[test]
    fn single_anchor_bag_query_ranks_the_event_with_the_missing_token() {
        const REDIS: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5F01";
        const QUEUE: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5F02";
        const QUERY: &str = "redis worker queue overflow";
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema);
        let mut writer = index.writer(15_000_000).expect("create writer");
        let ts = (NOW_SEC - DAY_SECS) as u64;
        for (id, skill_action, content) in [
            (REDIS, "rust::redis::worker", "redis worker rejects a job"),
            (
                QUEUE,
                "rust::worker::queue",
                "worker queue overflow worker queue overflow worker queue overflow",
            ),
        ] {
            writer
                .add_document(doc!(
                    fields.content => content,
                    fields.timestamp_sec => ts,
                    fields.pain => 6.0_f64,
                    fields.importance => 6.0_f64,
                    fields.layer => Layer::Episodic.as_str(),
                    fields.last_updated_sec => ts,
                    fields.cited_event_count => 0_u64,
                    fields.event_id => id,
                    fields.skill_action => skill_action,
                    fields.source_harness => "recall_gate",
                ))
                .expect("add doc");
        }
        writer.commit().expect("commit");
        let reader = RecallReader::open(&index, ReloadPolicy::OnCommitWithDelay).expect("reader");

        // Tantivy's BM25 for the whole query, which is the bag.
        let searcher = reader.searcher();
        let query = QueryParser::for_index(&index, vec![fields.content])
            .parse_query(QUERY)
            .expect("parse");
        let mut bag_bm25 = std::collections::HashMap::new();
        for (score, address) in searcher
            .search(
                &query,
                &tantivy::collector::TopDocs::with_limit(5).order_by_score(),
            )
            .expect("bag search")
        {
            let doc: TantivyDocument = searcher.doc(address).expect("doc");
            let id = doc
                .get_first(fields.event_id)
                .and_then(|v| v.as_str())
                .expect("id");
            bag_bm25.insert(id.to_string(), score as f64);
        }
        assert!(
            bag_bm25[QUEUE] > bag_bm25[REDIS],
            "the bag alone ranks the queue event first: {bag_bm25:?}"
        );

        let results = recall(&reader, &fields, QUERY, 5, NOW_SEC).unwrap();
        for r in &results {
            eprintln!(
                "{} bm25={} salience={} score={}",
                r.event_id, r.bm25, r.salience, r.score
            );
        }
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].event_id, REDIS, "redis event must be rank 1");
        let (redis, queue) = (&results[0], &results[1]);
        assert_eq!(queue.event_id, QUEUE);
        assert_eq!(redis.salience, queue.salience, "the gate ties on salience");
        assert_eq!(
            queue.bm25, bag_bm25[QUEUE],
            "the top hit keeps its bag BM25"
        );
        assert_eq!(redis.bm25, bag_bm25[REDIS] + bag_bm25[QUEUE] + 1.0);
        for r in &results {
            assert_eq!(r.score, r.bm25 * r.salience, "score stays bm25 * salience");
        }
    }

    #[test]
    fn test_event_id_hydrated() {
        // `event_id` is `STORED` in the schema (WEG-45); recall() must hydrate it
        // onto the result so the `dreamd recall` `id` column is real (WEG-51).
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema);
        let mut writer = index.writer(15_000_000).expect("create writer");
        writer
            .add_document(doc!(
                fields.content => "axum error handling pattern",
                fields.timestamp_sec => (NOW_SEC - DAY_SECS) as u64,
                fields.pain => 8.0_f64,
                fields.importance => 9.0_f64,
                fields.layer => Layer::Episodic.as_str(),
                fields.last_updated_sec => (NOW_SEC - DAY_SECS) as u64,
                fields.cited_event_count => 0_u64,
                fields.event_id => "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV",
                fields.skill_action => "rust::axum::error_handling",
                fields.source_harness => "claude-code",
            ))
            .expect("add doc");
        writer.commit().expect("commit");
        let reader = RecallReader::open(&index, ReloadPolicy::OnCommitWithDelay).expect("reader");
        let results = recall(&reader, &fields, "axum", 10, NOW_SEC).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].event_id, "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "recall must hydrate the stored event_id onto RecallResult"
        );
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
            "rust::axum::error_handling",
            Layer::Episodic,
        )]);
        let results = recall(&reader, &fields, "axum", 10, NOW_SEC).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].score, 0.0, "pain=0 should zero the final score");
    }

    #[test]
    fn test_top_k_eviction() {
        // Six matching docs at decreasing recency → with k=3, the three
        // most-recent (highest salience) survive.
        let entries: Vec<(&str, i64, f64, f64, &str, Layer)> = (0..6)
            .map(|i| {
                (
                    "axum handler error pattern",
                    NOW_SEC - (i as i64) * DAY_SECS,
                    5.0,
                    5.0,
                    "rust::axum::error_handling",
                    Layer::Episodic,
                )
            })
            .collect();
        let (_idx, fields, reader) = build_index_with(&entries);
        let results = recall(&reader, &fields, "axum", 3, NOW_SEC).unwrap();
        assert_eq!(results.len(), 3, "k=3 must evict to 3 results");
        // Returned in descending score order; with constant BM25 + pain + importance,
        // salience monotonically decays with age, so order matches age ascending.
        assert!(results[0].timestamp_sec >= results[1].timestamp_sec);
        assert!(results[1].timestamp_sec >= results[2].timestamp_sec);
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
                "rust::axum::error_handling",
                Layer::Episodic,
            ),
            (
                "tokio select cancel",
                NOW_SEC - DAY_SECS,
                9.0,
                5.0,
                "rust::axum::error_handling",
                Layer::Episodic,
            ),
        ]);
        let results = recall(&reader, &fields, "tokio", 10, NOW_SEC).unwrap();
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
        let entries: Vec<(&str, i64, f64, f64, &str, Layer)> = vec![
            (
                "axum requires IntoResponse on error types",
                NOW_SEC - DAY_SECS,
                8.0,
                9.0,
                "rust::axum::error_handling",
                Layer::Episodic,
            ),
            (
                "use ? not unwrap in axum handlers",
                NOW_SEC - 7 * DAY_SECS,
                6.0,
                7.0,
                "rust::axum::error_handling",
                Layer::Episodic,
            ),
            (
                "axum extractor rejection pattern",
                NOW_SEC - 30 * DAY_SECS,
                5.0,
                5.0,
                "rust::axum::error_handling",
                Layer::Episodic,
            ),
            (
                "tokio select cancels other futures",
                NOW_SEC - 2 * DAY_SECS,
                7.0,
                7.0,
                "rust::tokio::select",
                Layer::Episodic,
            ),
            (
                "use biased in select for shutdown",
                NOW_SEC - 14 * DAY_SECS,
                5.0,
                6.0,
                "rust::tokio::select",
                Layer::Episodic,
            ),
        ];
        let (_idx, fields, reader) = build_index_with(&entries);
        let results = recall(&reader, &fields, "axum error handling", 3, NOW_SEC).unwrap();
        insta::with_settings!({
            // Numeric fields are formula-derived f64s — filter all three so the
            // snapshot stays stable under irrelevant floating-point drift.
            filters => vec![
                (r"score: -?\d+\.\d+", "score: <f64>"),
                (r"bm25: -?\d+\.\d+", "bm25: <f64>"),
                (r"salience: -?\d+\.\d+", "salience: <f64>"),
            ],
        }, {
            insta::assert_debug_snapshot!(results);
        });
    }

    /// Top-N by salience with no lexical filter (WEG-52).
    /// Distinct pain/age so order is unambiguous; content strings do not share
    /// a BM25 term, proving ranking is not lexical match.
    #[test]
    fn test_score_by_salience_orders_and_caps_n() {
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema);
        let mut writer = index.writer(15_000_000).expect("create writer");
        // High salience: recent + high pain. Unique token "alpha".
        writer
            .add_document(doc!(
                fields.content => "alpha unique token one",
                fields.timestamp_sec => (NOW_SEC - DAY_SECS) as u64,
                fields.pain => 9.0_f64,
                fields.importance => 9.0_f64,
                fields.layer => Layer::Episodic.as_str(),
                fields.last_updated_sec => (NOW_SEC - DAY_SECS) as u64,
                fields.cited_event_count => 0_u64,
                fields.event_id => "evt_score_high",
                fields.skill_action => "rust::alpha",
                fields.source_harness => "claude-code",
            ))
            .expect("add high");
        // Mid salience: older + mid pain. Unique token "beta".
        writer
            .add_document(doc!(
                fields.content => "beta unique token two",
                fields.timestamp_sec => (NOW_SEC - 7 * DAY_SECS) as u64,
                fields.pain => 6.0_f64,
                fields.importance => 6.0_f64,
                fields.layer => Layer::Episodic.as_str(),
                fields.last_updated_sec => (NOW_SEC - 7 * DAY_SECS) as u64,
                fields.cited_event_count => 0_u64,
                fields.event_id => "evt_score_mid",
                fields.skill_action => "rust::beta",
                fields.source_harness => "claude-code",
            ))
            .expect("add mid");
        // Low salience: very old + low pain. Unique token "gamma".
        writer
            .add_document(doc!(
                fields.content => "gamma unique token three",
                fields.timestamp_sec => (NOW_SEC - 30 * DAY_SECS) as u64,
                fields.pain => 2.0_f64,
                fields.importance => 2.0_f64,
                fields.layer => Layer::Episodic.as_str(),
                fields.last_updated_sec => (NOW_SEC - 30 * DAY_SECS) as u64,
                fields.cited_event_count => 0_u64,
                fields.event_id => "evt_score_low",
                fields.skill_action => "rust::gamma",
                fields.source_harness => "claude-code",
            ))
            .expect("add low");
        writer.commit().expect("commit");
        let reader = RecallReader::open(&index, ReloadPolicy::OnCommitWithDelay).expect("reader");

        let results = score_by_salience(&reader, &fields, 2, NOW_SEC).expect("score");
        assert_eq!(results.len(), 2, "n=2 must cap to 2 hits across all docs");
        assert_eq!(results[0].event_id, "evt_score_high");
        assert_eq!(results[1].event_id, "evt_score_mid");
        assert!(
            results[0].salience > results[1].salience,
            "heap order must follow salience: {} > {}",
            results[0].salience,
            results[1].salience
        );
        // AllQuery scores 1.0 — bm25 flat; score ≈ salience.
        for r in &results {
            assert!(
                (r.bm25 - 1.0).abs() < 1e-5,
                "AllQuery bm25≈1, got {}",
                r.bm25
            );
            assert!(
                (r.score - r.salience).abs() < 1e-5,
                "score≈salience under AllQuery: {} vs {}",
                r.score,
                r.salience
            );
            assert!(!r.event_id.is_empty(), "event_id must be hydrated");
        }
    }

    fn scored(score: f64, doc_id: u32) -> ScoredDoc {
        ScoredDoc {
            score: OrderedFloat(score),
            doc_address: DocAddress::new(0, doc_id),
            timestamp_sec: 0,
            pain: 0.0,
            importance: 0.0,
            recurrence: 0,
            bm25: score,
            salience: 1.0,
        }
    }

    /// `merge_fruits` re-applies a global top-`k` across per-segment fruit.
    /// Asserts length and the kept score set only: the result is the min-heap's
    /// iteration order, which is not score order.
    #[test]
    fn merge_fruits_keeps_global_top_k_across_segments() {
        let empty = RecallReader::open(
            &Index::create_in_ram(build_schema().0),
            ReloadPolicy::Manual,
        )
        .expect("reader");
        let collector = SalienceCollector::new(
            3,
            NOW_SEC,
            empty.recurrence(&empty.searcher()).expect("tables"),
        );
        let seg_a = vec![scored(0.9, 0), scored(0.2, 1), scored(0.5, 2)];
        let seg_b = vec![scored(0.7, 3), scored(0.1, 4), scored(0.8, 5)];

        let merged = collector.merge_fruits(vec![seg_a, seg_b]).expect("merge");

        assert_eq!(merged.len(), 3, "merge must cap at k");
        let mut kept: Vec<f64> = merged.iter().map(|d| d.score.into_inner()).collect();
        kept.sort_by(|a, b| b.partial_cmp(a).unwrap());
        assert_eq!(
            kept,
            vec![0.9, 0.8, 0.7],
            "merge must keep the highest scores"
        );
    }

    /// BZR-194 fixtures: `(content, event_id, skill_action, timestamp_sec, pain, layer)`.
    /// `build_index_with` never sets `event_id`, so exclusion tests build their own.
    fn build_exclusion_index(
        entries: &[(&str, &str, &str, i64, f64, Layer)],
    ) -> (Index, SchemaFields, RecallReader) {
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema);
        let mut writer = index.writer(15_000_000).expect("create writer");
        for (content, event_id, skill_action, ts, pain, layer) in entries {
            writer
                .add_document(doc!(
                    fields.content => *content,
                    fields.timestamp_sec => *ts as u64,
                    fields.pain => *pain,
                    fields.importance => 9.0_f64,
                    fields.layer => layer.as_str(),
                    fields.last_updated_sec => *ts as u64,
                    fields.cited_event_count => 0_u64,
                    fields.event_id => *event_id,
                    fields.skill_action => *skill_action,
                    fields.source_harness => "claude-code",
                ))
                .expect("add doc");
        }
        writer.commit().expect("commit");
        let reader = RecallReader::open(&index, ReloadPolicy::OnCommitWithDelay).expect("reader");
        (index, fields, reader)
    }

    fn ids(results: &[RecallResult]) -> Vec<&str> {
        results.iter().map(|r| r.event_id.as_str()).collect()
    }

    #[test]
    fn exclusion_by_id_backfills_top_k() {
        // The excluded doc outranks the other (newer, higher pain). With k = 1
        // a post-filter would return nothing; dropping it before the heap
        // backfills.
        let (_idx, fields, reader) = build_exclusion_index(&[
            (
                "axum error handling",
                "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "rust::axum",
                NOW_SEC - DAY_SECS,
                9.0,
                Layer::Episodic,
            ),
            (
                "axum error handling",
                "evt_01BX5ZZKBKACTAV9WEVGEMMVRZ",
                "rust::axum",
                NOW_SEC - 20 * DAY_SECS,
                3.0,
                Layer::Episodic,
            ),
        ]);
        let baseline = recall(&reader, &fields, "axum", 1, NOW_SEC).unwrap();
        assert_eq!(ids(&baseline), vec!["evt_01ARZ3NDEKTSV4RRFFQ69G5FAV"]);

        let exclusion = RecallExclusion {
            ids: vec!["evt_01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string()],
            cluster_prefixes: Vec::new(),
        };
        let results = recall_excluding(&reader, &fields, "axum", 1, &exclusion, NOW_SEC).unwrap();
        assert_eq!(ids(&results), vec!["evt_01BX5ZZKBKACTAV9WEVGEMMVRZ"]);
    }

    #[test]
    fn exclusion_cluster_prefix_matches_token_or_colon_extension() {
        let (_idx, fields, reader) = build_exclusion_index(&[
            (
                "error handling",
                "evt_a",
                "rust::error_handling",
                NOW_SEC - DAY_SECS,
                8.0,
                Layer::Episodic,
            ),
            (
                "error handling",
                "evt_b",
                "rust::error_handling::axum",
                NOW_SEC - DAY_SECS,
                8.0,
                Layer::Episodic,
            ),
            (
                "error handling",
                "lsn_evt_b",
                "rust::error_handling::axum",
                NOW_SEC - DAY_SECS,
                8.0,
                Layer::Semantic,
            ),
            (
                "error handling",
                "evt_c",
                "rustacean",
                NOW_SEC - DAY_SECS,
                8.0,
                Layer::Episodic,
            ),
            (
                "error handling",
                "evt_d",
                "rust::errors",
                NOW_SEC - DAY_SECS,
                8.0,
                Layer::Episodic,
            ),
        ]);
        let exclusion = RecallExclusion {
            ids: Vec::new(),
            cluster_prefixes: vec!["rust::error_handling".to_string()],
        };
        let results = recall_excluding(&reader, &fields, "error", 10, &exclusion, NOW_SEC).unwrap();
        let mut got = ids(&results);
        got.sort_unstable();
        // Both layers drop; `rustacean` and `rust::errors` are not extensions.
        assert_eq!(got, vec!["evt_c", "evt_d"]);

        assert!(cluster_prefix_matches(
            "rust::error_handling",
            "rust::error_handling"
        ));
        assert!(cluster_prefix_matches(
            "rust::error_handling::axum",
            "rust::error_handling"
        ));
        assert!(!cluster_prefix_matches("rust", "rust::error_handling"));
        assert!(!cluster_prefix_matches("rustacean", "rust"));
        assert!(!cluster_prefix_matches("rust::errors", "rust::error"));
    }

    #[test]
    fn exclusion_by_evt_id_keeps_lsn_lesson_doc() {
        let (_idx, fields, reader) = build_exclusion_index(&[
            (
                "tokio runtime",
                "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "rust::tokio",
                NOW_SEC - DAY_SECS,
                8.0,
                Layer::Episodic,
            ),
            (
                "tokio runtime",
                "lsn_evt_01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "rust::tokio",
                NOW_SEC - DAY_SECS,
                8.0,
                Layer::Semantic,
            ),
        ]);
        let exclusion = RecallExclusion {
            ids: vec!["evt_01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string()],
            cluster_prefixes: Vec::new(),
        };
        let results = recall_excluding(&reader, &fields, "tokio", 10, &exclusion, NOW_SEC).unwrap();
        assert_eq!(ids(&results), vec!["lsn_evt_01ARZ3NDEKTSV4RRFFQ69G5FAV"]);
        assert_eq!(results[0].layer, Layer::Semantic);
    }

    #[test]
    fn recall_excluding_default_matches_recall() {
        let (_idx, fields, reader) = build_exclusion_index(&[
            (
                "serde derive",
                "evt_a",
                "rust::serde",
                NOW_SEC - DAY_SECS,
                8.0,
                Layer::Episodic,
            ),
            (
                "serde derive",
                "evt_b",
                "rust::serde",
                NOW_SEC - 5 * DAY_SECS,
                6.0,
                Layer::Episodic,
            ),
            (
                "serde derive",
                "lsn_evt_a",
                "rust::serde",
                NOW_SEC - 2 * DAY_SECS,
                7.0,
                Layer::Semantic,
            ),
        ]);
        let plain = recall(&reader, &fields, "serde", 2, NOW_SEC).unwrap();
        let excluding = recall_excluding(
            &reader,
            &fields,
            "serde",
            2,
            &RecallExclusion::default(),
            NOW_SEC,
        )
        .unwrap();
        assert!(RecallExclusion::default().is_empty());
        assert_eq!(ids(&plain), ids(&excluding));
        assert_eq!(plain.len(), 2);
    }

    /// BZR-1243: the `MustNot` clauses drop exactly what the reference rule
    /// drops, and every remaining hit keeps its score bit for bit.
    #[test]
    fn query_exclusion_matches_the_reference_filter() {
        // Same content, timestamp and importance: the order is pain times the
        // recurrence factor, and the pains make the docs each mutant would
        // keep or drop wrongly rank near the top.
        let docs: [(&str, &str, f64, Layer); 11] = [
            ("evt_adotx", "a.x", 9.45, Layer::Episodic),
            ("evt_acolonb", "a:b", 9.33, Layer::Episodic),
            ("evt_a", "a", 8.37, Layer::Episodic),
            ("evt_a_bc", "a::bc", 9.10, Layer::Episodic),
            ("evt_a_b", "a::b", 8.39, Layer::Episodic),
            ("lsn_evt_a_b", "a::b", 8.29, Layer::Semantic),
            ("evt_a_b_c", "a::b::c", 8.74, Layer::Episodic),
            ("evt_ab", "ab", 8.62, Layer::Episodic),
            ("evt_go", "go", 7.95, Layer::Episodic),
            ("evt_go_x", "go::x", 8.39, Layer::Episodic),
            ("lsn_evt_go", "go", 7.73, Layer::Semantic),
        ];
        let entries: Vec<(&str, &str, &str, i64, f64, Layer)> = docs
            .iter()
            .map(|&(id, key, pain, layer)| ("quokka", id, key, NOW_SEC - DAY_SECS, pain, layer))
            .collect();
        let (_idx, fields, reader) = build_exclusion_index(&entries);
        let all = recall(&reader, &fields, "quokka", docs.len(), NOW_SEC).unwrap();
        assert_eq!(
            ids(&all),
            docs.iter().map(|d| d.0).collect::<Vec<_>>(),
            "precondition: no two scores tie, in this order"
        );

        let exclusion = |prefixes: &[&str], excluded: &[&str]| RecallExclusion {
            ids: excluded.iter().map(|id| id.to_string()).collect(),
            cluster_prefixes: prefixes.iter().map(|p| p.to_string()).collect(),
        };
        let cases = [
            exclusion(&["a"], &[]),
            exclusion(&["a::b"], &[]),
            exclusion(&["go"], &["evt_a_bc"]),
            exclusion(&[], &["lsn_evt_a_b"]),
            exclusion(&["a:b"], &[]),
        ];
        let hits = |results: &[RecallResult]| -> Vec<(String, u64)> {
            results
                .iter()
                .map(|r| (r.event_id.clone(), r.score.to_bits()))
                .collect()
        };
        for exclusion in &cases {
            let reference: Vec<RecallResult> =
                recall(&reader, &fields, "quokka", docs.len(), NOW_SEC)
                    .unwrap()
                    .into_iter()
                    .filter(|r| !exclusion.excludes(&r.event_id, &r.skill_action))
                    .take(4)
                    .collect();
            let got = recall_excluding(&reader, &fields, "quokka", 4, exclusion, NOW_SEC).unwrap();
            assert_eq!(hits(&got), hits(&reference), "exclusion {exclusion:?}");
        }
    }

    /// BZR-1523 test 6: both contents have 7 tokens and one match per query
    /// term, so BM25 ties, and pain 7 beats pain 6 at any hub size.
    #[test]
    fn hub_parent_does_not_drown_a_more_painful_child() {
        for parents in [8_usize, 40, 500] {
            let parent_ids: Vec<String> = (0..parents).map(|i| format!("evt_hub_{i}")).collect();
            let mut entries: Vec<(&str, &str, &str, i64, f64, Layer)> = parent_ids
                .iter()
                .map(|id| {
                    (
                        "extractor order in the generic handler HUB",
                        id.as_str(),
                        "rust::http",
                        NOW_SEC - DAY_SECS,
                        6.0,
                        Layer::Episodic,
                    )
                })
                .collect();
            entries.push((
                "extractor order for the json body NEEDLE",
                "evt_needle",
                "rust::http::extractor",
                NOW_SEC - DAY_SECS,
                7.0,
                Layer::Episodic,
            ));
            let (_idx, fields, reader) = build_exclusion_index(&entries);
            let hits = recall(&reader, &fields, "extractor order", parents + 1, NOW_SEC).unwrap();
            assert_eq!(hits.len(), parents + 1, "{parents} parents");
            assert_eq!(hits[0].event_id, "evt_needle", "{parents} parents");
            assert_eq!(hits[0].recurrence, 1, "{parents} parents");
            assert_eq!(hits[1].recurrence, parents as u64 + 1, "{parents} parents");
        }
    }

    /// BZR-1523 test 7: a parent that matches no query term does not come back,
    /// whatever its recurrence.
    #[test]
    fn a_query_only_the_child_matches_returns_only_the_child() {
        let parent_ids: Vec<String> = (0..40).map(|i| format!("evt_hub_{i}")).collect();
        let mut entries: Vec<(&str, &str, &str, i64, f64, Layer)> = parent_ids
            .iter()
            .map(|id| {
                (
                    "generic handler HUB",
                    id.as_str(),
                    "rust::http",
                    NOW_SEC - DAY_SECS,
                    6.0,
                    Layer::Episodic,
                )
            })
            .collect();
        entries.push((
            "extractor order for the json body NEEDLE",
            "evt_needle",
            "rust::http::extractor",
            NOW_SEC - DAY_SECS,
            7.0,
            Layer::Episodic,
        ));
        let (_idx, fields, reader) = build_exclusion_index(&entries);
        let hits = recall(&reader, &fields, "extractor order", 10, NOW_SEC).unwrap();
        assert_eq!(ids(&hits), vec!["evt_needle"]);
    }

    /// BZR-1523 test 8: `d` keeps recurrence as a tie-breaker. Ten events beat a
    /// one-off at equal pain, and still beat one half a point more painful.
    #[test]
    fn recurrence_still_breaks_a_close_call() {
        for one_off_pain in [6.0, 6.5] {
            let recurring_ids: Vec<String> = (0..10).map(|i| format!("evt_recur_{i}")).collect();
            let mut entries: Vec<(&str, &str, &str, i64, f64, Layer)> = recurring_ids
                .iter()
                .map(|id| {
                    (
                        "retry the flaky socket RECUR",
                        id.as_str(),
                        "go::often",
                        NOW_SEC - DAY_SECS,
                        6.0,
                        Layer::Episodic,
                    )
                })
                .collect();
            entries.push((
                "retry the flaky socket ONEOFF",
                "evt_oneoff",
                "go::once",
                NOW_SEC - DAY_SECS,
                one_off_pain,
                Layer::Episodic,
            ));
            let (_idx, fields, reader) = build_exclusion_index(&entries);
            let hits = recall(&reader, &fields, "flaky socket", 11, NOW_SEC).unwrap();
            assert_eq!(hits.len(), 11, "one-off pain {one_off_pain}");
            assert_eq!(hits[0].recurrence, 10, "one-off pain {one_off_pain}");
            assert_eq!(
                hits[10].event_id, "evt_oneoff",
                "one-off pain {one_off_pain}"
            );
        }
    }
}
