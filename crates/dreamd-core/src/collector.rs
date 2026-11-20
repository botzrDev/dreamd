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

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::ops::Bound;
use std::sync::Arc;

use ordered_float::OrderedFloat;
use tantivy::collector::{Collector, SegmentCollector};
use tantivy::columnar::Column;
use tantivy::query::{
    AllQuery, BooleanQuery, EmptyQuery, Occur, PhraseQuery, Query, RangeQuery, TermQuery,
    TermSetQuery,
};
use tantivy::schema::{IndexRecordOption, TantivyDocument, Term, Value};
use tantivy::{DocAddress, DocId, Index, Score, SegmentOrdinal, SegmentReader, TantivyError};

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
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Score order only: `score_key` under `f64::total_cmp` (DRM-110). The heaps
/// cannot see `event_id`, so documents that tie here are kept at the top-`k`
/// boundary and ordered by `recall_order` after hydration.
impl Ord for ScoredDoc {
    fn cmp(&self, other: &Self) -> Ordering {
        score_key(self.score.0).total_cmp(&score_key(other.score.0))
    }
}

/// The score key every recall comparison uses (DRM-110): NaN is lowest, and
/// `-0.0` equals `0.0`.
fn score_key(score: f64) -> f64 {
    if score.is_nan() {
        f64::NEG_INFINITY
    } else if score == 0.0 {
        0.0
    } else {
        score
    }
}

/// Recall's one total order (DRM-110): score descending ([`score_key`],
/// `f64::total_cmp`), then `event_id` ascending. So an event comes before a
/// lesson that ties with it (`evt_` < `lsn_`), and the order depends on
/// neither the index's segments nor `k`.
fn recall_order(a: &RecallResult, b: &RecallResult) -> Ordering {
    score_key(b.score)
        .total_cmp(&score_key(a.score))
        .then_with(|| a.event_id.cmp(&b.event_id))
}

/// Offer `doc` to a top-`k` min-heap, keeping the ties at its boundary
/// (DRM-110). `ties` holds documents whose score equals the heap's minimum:
///
/// * heap not full: push;
/// * heap full, score above the minimum: pop the minimum into `ties`, push
///   `doc`, and drop every tie below the new minimum;
/// * heap full, score equal to the minimum: `doc` joins `ties`;
/// * heap full, score below the minimum (or `k == 0`): drop it.
///
/// The heap plus `ties` is then every document scoring at least the `k`-th
/// best score, so the caller can decide the boundary by `event_id`.
fn offer(
    heap: &mut BinaryHeap<Reverse<ScoredDoc>>,
    ties: &mut Vec<ScoredDoc>,
    k: usize,
    doc: ScoredDoc,
) {
    if heap.len() < k {
        heap.push(Reverse(doc));
        return;
    }
    let Some(Reverse(min)) = heap.peek() else {
        return;
    };
    match doc.cmp(min) {
        Ordering::Less => {}
        Ordering::Equal => ties.push(doc),
        Ordering::Greater => {
            let Some(Reverse(old)) = heap.pop() else {
                return;
            };
            heap.push(Reverse(doc));
            // Every tie equals `old`. They stay only while the new minimum
            // still equals it.
            match heap.peek() {
                Some(Reverse(new_min)) if new_min.cmp(&old) == Ordering::Equal => ties.push(old),
                _ => ties.clear(),
            }
        }
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
/// doc; the min-heap (`BinaryHeap<Reverse<…>>`) caps at `k` entries, and
/// `ties` keeps the documents that tie its minimum (`offer`, DRM-110).
pub struct SalienceSegmentCollector {
    heap: BinaryHeap<Reverse<ScoredDoc>>,
    ties: Vec<ScoredDoc>,
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

        let entry = ScoredDoc {
            score: OrderedFloat(final_score),
            doc_address: DocAddress::new(self.segment_ord, doc),
            timestamp_sec: ts as u64,
            pain: p,
            importance: imp,
            recurrence: rec,
            bm25,
            salience: sal,
        };
        offer(&mut self.heap, &mut self.ties, self.k, entry);
    }

    /// The segment's top `k`, plus the documents that tie the `k`-th.
    fn harvest(self) -> Vec<ScoredDoc> {
        self.heap
            .into_iter()
            .map(|Reverse(d)| d)
            .chain(self.ties)
            .collect()
    }
}

/// Top-level collector handed to `Searcher::search`. Stateless apart from
/// `k`, `now_sec` and the searcher's recurrence table; per-segment work
/// happens in [`SalienceSegmentCollector`].
///
/// Its fruit is the top `k` documents by score plus every document that ties
/// the `k`-th, in no particular order (DRM-110): the `event_id` that breaks
/// those ties is a stored field, which a collector cannot read cheaply.
/// [`recall`] and [`score_by_salience`] hydrate the fruit, order it by score
/// descending then `event_id` ascending, and keep the first `k`.
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
        // DRM-109: a segment never holds more than `max_doc` hits, so a huge
        // `k` sizes nothing beyond the segment.
        let capacity = self.k.min(segment.max_doc() as usize).saturating_add(1);
        Ok(SalienceSegmentCollector {
            heap: BinaryHeap::with_capacity(capacity),
            ties: Vec::new(),
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
        // Each segment already kept its own top-k and its ties; re-apply the
        // same rule globally ([`offer`]), so the fruit is the global top `k`
        // plus the documents that tie the `k`-th (DRM-110). Hydration orders
        // them by `event_id` and cuts to `k`. Sized by the hits there are,
        // not by `k` alone (DRM-109).
        let total: usize = segment_fruits.iter().map(Vec::len).sum();
        let capacity = self.k.min(total).saturating_add(1);
        let mut merged: BinaryHeap<Reverse<ScoredDoc>> = BinaryHeap::with_capacity(capacity);
        let mut ties = Vec::new();
        for doc in segment_fruits.into_iter().flatten() {
            offer(&mut merged, &mut ties, self.k, doc);
        }
        Ok(merged.into_iter().map(|Reverse(d)| d).chain(ties).collect())
    }
}

/// Hydrate [`ScoredDoc`] fruit into [`RecallResult`]s, sort them by
/// [`recall_order`] and keep the first `limit` (DRM-110).
///
/// Shared by [`recall`] and [`score_by_salience`]. `doc_address` is private to
/// this module on [`ScoredDoc`], so hydration must stay inside it.
fn hydrate_scored(
    searcher: &tantivy::Searcher,
    fields: &SchemaFields,
    top_docs: Vec<ScoredDoc>,
    limit: usize,
) -> tantivy::Result<Vec<RecallResult>> {
    Ok(hydrate_addressed(searcher, fields, top_docs, limit)?
        .into_iter()
        .map(|(_, result)| result)
        .collect())
}

/// [`hydrate_scored`], keeping each result's [`DocAddress`] so
/// [`rank_missing_tokens`] can tell documents apart across searches.
///
/// `top_docs` is a [`SalienceCollector`] fruit: the top `limit` plus the
/// documents that tie the `limit`-th. The `event_id` that decides those ties
/// is a stored field, so the boundary is cut here, after hydration.
fn hydrate_addressed(
    searcher: &tantivy::Searcher,
    fields: &SchemaFields,
    top_docs: Vec<ScoredDoc>,
    limit: usize,
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

    results.sort_by(|a, b| recall_order(&a.1, &b.1));
    results.truncate(limit);
    Ok(results)
}

/// Execute a salience-scored BM25 recall query.
///
/// `query_text` is any text. `words_query` turns it into words, and no
/// character in it is an operator, so no text is an error (DRM-108).
///
/// Results are ordered by score descending, then `event_id` ascending, on
/// every path (DRM-110), so the order depends on neither the index's
/// segments nor `k`.
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
/// `query_text` is any text: the query is its `words_query`, and there is
/// no grammar (DRM-108). The only errors are the index's own.
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
/// hit's content has is re-ranked by `rank_missing_tokens` (BZR-1522).
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

    // One word query, the exclusion's `MustNot` clauses, and the salience
    // collector. The bag and every token search go through it.
    let search = |text: &str, limit: usize| -> tantivy::Result<Vec<(DocAddress, RecallResult)>> {
        let final_query = exclude(
            words_query(searcher.index(), fields, text)?,
            fields,
            exclusion,
        );
        let collector = SalienceCollector::new(limit, now_sec, Arc::clone(&recurrence));
        let top_docs: Vec<ScoredDoc> = searcher.search(&*final_query, &collector)?;
        hydrate_addressed(&searcher, fields, top_docs, limit)
    };

    let bag = search(query_text, k)?;
    rank_missing_tokens(bag, query_text, k, search)
}

/// The query for any recall text (DRM-108), built as Tantivy's parser builds
/// text that has no grammar, so a query that parsed before scores bit for bit
/// as it did.
///
/// Split on whitespace. A piece that repeats an earlier piece byte for byte
/// is dropped, as the parser did. Run each other piece through the content
/// field's tokenizer: a piece with one token is a [`TermQuery`], a piece with
/// several is a [`PhraseQuery`] with each term at its token's position (a
/// token `RemoveLongFilter` drops keeps its gap), and a piece with none adds
/// nothing. Every clause is `Should`, in input order; one clause is returned
/// bare, and no clause at all is an [`EmptyQuery`].
///
/// No text is an error. The only error is the content field's tokenizer
/// missing from the index, which no input causes.
fn words_query(
    index: &Index,
    fields: &SchemaFields,
    text: &str,
) -> tantivy::Result<Box<dyn Query>> {
    let mut analyzer = index.tokenizer_for_field(fields.content)?;
    let mut pieces: Vec<&str> = Vec::new();
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    for piece in text.split_whitespace() {
        if pieces.contains(&piece) {
            continue;
        }
        pieces.push(piece);
        let mut terms: Vec<(usize, Term)> = Vec::new();
        analyzer.token_stream(piece).process(&mut |token| {
            terms.push((
                token.position,
                Term::from_field_text(fields.content, &token.text),
            ));
        });
        let query: Box<dyn Query> = match terms.len() {
            0 => continue,
            1 => {
                let (_, term) = terms.remove(0);
                Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs))
            }
            _ => Box::new(PhraseQuery::new_with_offset(terms)),
        };
        clauses.push((Occur::Should, query));
    }
    Ok(match clauses.len() {
        0 => Box::new(EmptyQuery),
        1 => clauses.remove(0).1,
        _ => Box::new(BooleanQuery::new(clauses)),
    })
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
        search(query_text, k.saturating_add(token_hits.len()))?
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
    ranked.sort_by(recall_order);
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
    hydrate_scored(&searcher, fields, top_docs, n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::build_schema;
    use tantivy::doc;
    use tantivy::query::QueryParser;
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
        kept.sort_by(|a, b| b.total_cmp(a));
        assert_eq!(
            kept,
            vec![0.9, 0.8, 0.7],
            "merge must keep the highest scores"
        );
    }

    /// DRM-109 test 1: a `k` far past the index sizes no heap by `k`. Over
    /// three documents, `k = usize::MAX` returns the three; on the base the
    /// segment heap's `k + 1` overflows or the allocation aborts.
    #[test]
    fn a_huge_k_allocates_for_the_documents_only() {
        let docs: Vec<_> = ["evt_a", "evt_b", "evt_c"]
            .into_iter()
            .map(|id| ("tokio", id, "rust::a", NOW_SEC, 5.0, Layer::Episodic))
            .collect();
        let (_idx, fields, reader) = build_exclusion_index(&docs);
        let results = recall(&reader, &fields, "tokio", usize::MAX, NOW_SEC).expect("recall");
        assert_eq!(results.len(), 3, "every match comes back");
    }

    /// DRM-110 fixtures: `(content, event_id, skill_action, timestamp_sec,
    /// pain, importance, layer)`.
    type TieDoc = (&'static str, String, &'static str, i64, f64, f64, Layer);

    /// An in-RAM index with one commit per group of `commits`, in order, and
    /// no merges, so each group stays its own segment.
    fn build_committed_index(commits: &[Vec<TieDoc>]) -> (Index, SchemaFields, RecallReader) {
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema);
        let mut writer = index.writer(15_000_000).expect("create writer");
        writer.set_merge_policy(Box::new(tantivy::indexer::NoMergePolicy));
        for group in commits {
            for (content, event_id, skill_action, ts, pain, importance, layer) in group {
                writer
                    .add_document(doc!(
                        fields.content => *content,
                        fields.timestamp_sec => *ts as u64,
                        fields.pain => *pain,
                        fields.importance => *importance,
                        fields.layer => layer.as_str(),
                        fields.last_updated_sec => *ts as u64,
                        fields.cited_event_count => 0_u64,
                        fields.event_id => event_id.as_str(),
                        fields.skill_action => *skill_action,
                        fields.source_harness => "claude-code",
                    ))
                    .expect("add doc");
            }
            writer.commit().expect("commit");
        }
        let reader = RecallReader::open(&index, ReloadPolicy::Manual).expect("reader");
        (index, fields, reader)
    }

    fn owned_ids(results: &[RecallResult]) -> Vec<String> {
        results.iter().map(|r| r.event_id.clone()).collect()
    }

    /// DRM-110 test 1 (LP-READ-10): an event and the lesson built from it
    /// score exactly the same. In every segment layout, `k = 1` returns the
    /// event, `k = 4` lists the event then the lesson, and the three layouts
    /// give the same lists.
    #[test]
    fn ties_follow_event_id_across_segment_layouts() {
        const TEXT: &str = "tokio runtime panics under load";
        let event: TieDoc = (
            TEXT,
            "evt_01TIE".into(),
            "rust::tokio",
            NOW_SEC,
            9.0,
            9.0,
            Layer::Episodic,
        );
        let lesson: TieDoc = (
            TEXT,
            "lsn_evt_01TIE".into(),
            "rust::tokio",
            NOW_SEC,
            9.0,
            9.0,
            Layer::Semantic,
        );
        let others: Vec<TieDoc> = (1..=6)
            .map(|n| {
                (
                    "tokio runtime notes",
                    format!("evt_01OTHER{n}"),
                    "rust::other",
                    NOW_SEC - n * DAY_SECS,
                    f64::from(n as u8),
                    5.0,
                    Layer::Episodic,
                )
            })
            .collect();

        // One commit with the lesson first; one commit per document; the
        // lesson's commit before the event's.
        let mut one: Vec<TieDoc> = vec![lesson.clone()];
        one.extend(others.iter().cloned());
        one.push(event.clone());
        let mut each: Vec<Vec<TieDoc>> = vec![vec![event.clone()]];
        each.extend(others.iter().cloned().map(|d| vec![d]));
        each.push(vec![lesson.clone()]);
        let mut split: Vec<Vec<TieDoc>> = vec![vec![lesson.clone()], others[..3].to_vec()];
        let mut last = vec![event.clone()];
        last.extend(others[3..].iter().cloned());
        split.push(last);

        let mut lists: Vec<Vec<String>> = Vec::new();
        for (name, layout) in [
            ("one commit", vec![one]),
            ("per doc", each),
            ("lesson first", split),
        ] {
            let (_idx, fields, reader) = build_committed_index(&layout);
            let top = owned_ids(&recall(&reader, &fields, "tokio", 1, NOW_SEC).expect("k=1"));
            let four = owned_ids(&recall(&reader, &fields, "tokio", 4, NOW_SEC).expect("k=4"));
            assert_eq!(top, ["evt_01TIE"], "{name}: k=1");
            assert_eq!(
                &four[..2],
                ["evt_01TIE", "lsn_evt_01TIE"],
                "{name}: k=4 {four:?}"
            );
            assert!(
                four.starts_with(&top),
                "{name}: {top:?} is not a prefix of {four:?}"
            );
            lists.push(four);
        }
        assert!(
            lists.windows(2).all(|w| w[0] == w[1]),
            "layouts disagree: {lists:?}"
        );
    }

    /// A hand-made hit for the re-rank tests.
    fn hit(
        doc: u32,
        id: &str,
        content: &str,
        bm25: f64,
        salience: f64,
    ) -> (DocAddress, RecallResult) {
        (
            DocAddress::new(0, doc),
            RecallResult {
                score: bm25 * salience,
                bm25,
                salience,
                content: content.to_owned(),
                timestamp_sec: 0,
                pain: 5.0,
                importance: 5.0,
                recurrence: 0,
                layer: Layer::Episodic,
                skill_action: "rust::x".to_owned(),
                source_harness: "claude-code".to_owned(),
                event_id: id.to_owned(),
            },
        )
    }

    /// DRM-110 test 2: the BZR-1522 re-rank orders two hits that tie after
    /// it by `event_id`, though its token search found the higher id first,
    /// and its whole output is in score-then-id order.
    #[test]
    fn rank_missing_tokens_orders_ties_like_the_plain_path() {
        let top = hit(0, "evt_t", "alpha alpha", 5.0, 1.0);
        let other = hit(1, "evt_o", "alpha", 2.0, 1.0);
        // Both hold the missing token and score the same.
        let high = hit(2, "evt_q", "alpha beta", 1.0, 0.5);
        let low = hit(3, "evt_p", "alpha beta", 1.0, 0.5);
        let search =
            |text: &str, _limit: usize| -> tantivy::Result<Vec<(DocAddress, RecallResult)>> {
                Ok(match text {
                    "beta" => vec![high.clone(), low.clone()],
                    _ => vec![top.clone(), other.clone(), high.clone(), low.clone()],
                })
            };
        let ranked = rank_missing_tokens(vec![top.clone(), other.clone()], "alpha beta", 4, search)
            .expect("re-rank");
        assert_eq!(ids(&ranked), ["evt_t", "evt_p", "evt_q", "evt_o"]);
        assert_eq!(
            ranked[1].score, ranked[2].score,
            "the two promoted hits tie"
        );
        assert!(
            ranked.windows(2).all(|w| {
                w[0].score > w[1].score
                    || (w[0].score == w[1].score && w[0].event_id < w[1].event_id)
            }),
            "score descending, then event_id ascending: {:?}",
            ids(&ranked)
        );
    }

    /// DRM-110 test 3: NaN orders last, and `-0.0` ties `0.0`, so the id
    /// decides between them.
    #[test]
    fn score_key_orders_nan_last_and_zeroes_equal() {
        let results = |scores: &[(f64, &str)]| -> Vec<RecallResult> {
            scores
                .iter()
                .map(|&(score, id)| hit(0, id, "", score, 1.0).1)
                .collect()
        };
        let mut mixed = results(&[
            (f64::NAN, "e1"),
            (1.0, "e2"),
            (0.0, "e3"),
            (-0.0, "e4"),
            (f64::INFINITY, "e5"),
        ]);
        mixed.sort_by(recall_order);
        assert_eq!(ids(&mixed), ["e5", "e2", "e3", "e4", "e1"]);

        let mut zeros = results(&[(0.0, "b"), (-0.0, "a")]);
        zeros.sort_by(recall_order);
        assert_eq!(
            ids(&zeros),
            ["a", "b"],
            "-0.0 equals 0.0, so the id decides"
        );
    }

    mod prefix {
        use super::*;
        use proptest::prelude::*;

        const CONTENTS: [&str; 4] = [
            "tokio runtime",
            "tokio axum handler",
            "axum runtime error",
            "tokio",
        ];
        const WORDS: [&str; 5] = ["tokio", "runtime", "axum", "handler", "error"];
        const KEYS: [&str; 2] = ["rust::tokio", "rust::axum"];

        /// One document: content, pain, importance, age, key and commit slot,
        /// each from a small set so that whole documents repeat and tie.
        fn doc() -> impl Strategy<Value = (usize, usize, usize, usize, usize, usize)> {
            (
                0..CONTENTS.len(),
                0..3usize,
                0..3usize,
                0..2usize,
                0..KEYS.len(),
                0..3usize,
            )
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(64))]

            /// DRM-110 test 4: on a store full of ties, `recall(k)` is a
            /// prefix of `recall(k + 1)`, for any segment layout. The event
            /// ids are a random permutation of the documents' order, so a
            /// segment does not meet its ties in id order.
            #[test]
            fn prefix_property(
                (docs, ids) in prop::collection::vec(doc(), 1..=40).prop_flat_map(|docs| {
                    let order: Vec<usize> = (0..docs.len()).collect();
                    (Just(docs), Just(order).prop_shuffle())
                }),
                commits in 1..=3usize,
                word in 0..WORDS.len(),
                k in 0..=20usize,
            ) {
                let mut groups: Vec<Vec<TieDoc>> = vec![Vec::new(); commits];
                for (n, &(content, pain, importance, age, key, slot)) in docs.iter().enumerate() {
                    groups[slot % commits].push((
                        CONTENTS[content],
                        format!("evt_{:04}", ids[n]),
                        KEYS[key],
                        NOW_SEC - age as i64 * DAY_SECS,
                        [3.0, 6.0, 9.0][pain],
                        [3.0, 6.0, 9.0][importance],
                        Layer::Episodic,
                    ));
                }
                let (_idx, fields, reader) = build_committed_index(&groups);
                let at = |k: usize| owned_ids(&recall(&reader, &fields, WORDS[word], k, NOW_SEC).expect("recall"));
                let (shorter, longer) = (at(k), at(k + 1));
                prop_assert!(
                    longer.starts_with(&shorter),
                    "k={}: {:?} is not a prefix of k+1: {:?}", k, shorter, longer
                );
            }
        }
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

    // ── DRM-108: recall takes plain words, not Tantivy's grammar ─────────────

    /// Event A of [`WordsStore`]: `tokio runtime panics`.
    const WORDS_A: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FB0";
    /// Event B of [`WordsStore`]: `axum handler`.
    const WORDS_B: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FB1";

    /// One fixed store for the DRM-108 tests: 20 events written to the
    /// episodic log first, then indexed by the first open's coverage pass, as
    /// every store is (AGENTS.md: the index is a function of the log). Fields
    /// drop in order: the handle goes before its lock and the folder.
    struct WordsStore {
        handle: crate::server::tantivy_handle::TantivyIndexHandle,
        _lock: crate::writer_lock::WriterLock,
        fields: SchemaFields,
        _dir: tempfile::TempDir,
    }

    impl WordsStore {
        /// The 41-letter run between `foo_` and `_bar` that `RemoveLongFilter`
        /// drops, leaving a position gap (Amendment 2).
        fn long_word() -> String {
            "a".repeat(41)
        }

        /// Write the log, then open the store. Call inside a tokio runtime:
        /// the open starts the indexer task.
        fn open() -> Self {
            use crate::layout::AgentRoot;
            use crate::server::tantivy_handle::{TantivyIndexHandle, DEFAULT_COMMIT_CADENCE};
            use crate::writer_lock::{WriterKind, WriterLock};
            use dreamd_protocol::{AgentLearning, EventId};

            let gap = format!("foo_{}_bar appears in a generated name", Self::long_word());
            // (id suffix, content, skill_action, days old, pain, importance)
            let events: Vec<(&str, &str, &str, i64, f32, f32)> = vec![
                ("B0", "tokio runtime panics", "rust::tokio", 1, 8.0, 8.0),
                ("B1", "axum handler", "rust::axum", 1, 6.0, 6.0),
                (
                    "B2",
                    "tokio scheduler starves the runtime under load",
                    "rust::tokio",
                    3,
                    5.0,
                    6.0,
                ),
                (
                    "B3",
                    "axum handler error returns a rejection",
                    "rust::axum::error",
                    2,
                    7.0,
                    7.0,
                ),
                (
                    "B4",
                    "error handling in rust with thiserror",
                    "rust::error_handling",
                    5,
                    6.0,
                    5.0,
                ),
                (
                    "B5",
                    "an axum extractor error",
                    "rust::axum::extract",
                    4,
                    5.0,
                    8.0,
                ),
                (
                    "B6",
                    "rust error handling: rust error handling notes",
                    "rust::error_handling",
                    1,
                    6.0,
                    6.0,
                ),
                ("B7", "foo_bar baz config", "rust::config", 2, 6.0, 6.0),
                ("B8", "bar then foo and baz", "rust::config", 2, 6.0, 6.0),
                (
                    "B9",
                    "dead-lock in a tokio mutex",
                    "rust::tokio::sync",
                    3,
                    7.0,
                    6.0,
                ),
                (
                    "BA",
                    "the lock is dead after a panic",
                    "rust::sync",
                    2,
                    6.0,
                    6.0,
                ),
                ("BB", gap.as_str(), "rust::codegen", 2, 6.0, 6.0),
                (
                    "BC",
                    "We don't deploy on Fridays.",
                    "ops::deploy",
                    6,
                    6.0,
                    9.0,
                ),
                (
                    "BD",
                    "parse_config() panics on an empty file",
                    "rust::config",
                    2,
                    7.0,
                    6.0,
                ),
                (
                    "BE",
                    "update README.md with an install section",
                    "docs::readme",
                    8,
                    4.0,
                    5.0,
                ),
                (
                    "BF",
                    "std::sync::Mutex poisoning after a panic",
                    "rust::sync",
                    3,
                    8.0,
                    7.0,
                ),
                (
                    "BG",
                    "what's the runtime cost of tokio::spawn",
                    "rust::tokio",
                    2,
                    5.0,
                    5.0,
                ),
                (
                    "BH",
                    "retry on 4xx responses is a bug",
                    "http::retry",
                    4,
                    6.0,
                    7.0,
                ),
                (
                    "BJ",
                    "run the tests with --verbose",
                    "rust::testing",
                    7,
                    3.0,
                    4.0,
                ),
                (
                    "BK",
                    "cargo test fails to pass on CI",
                    "rust::testing",
                    2,
                    6.0,
                    6.0,
                ),
            ];

            let dir = tempfile::tempdir().expect("tempdir");
            let root = AgentRoot::new(dir.path());
            std::fs::create_dir_all(root.episodic_dir()).expect("episodic dir");
            let mut log = String::new();
            for (suffix, content, skill_action, days, pain, importance) in &events {
                let learning = AgentLearning {
                    schema_version: "1.0.0".to_string(),
                    id: EventId::parse(&format!("evt_01ARZ3NDEKTSV4RRFFQ69G5F{suffix}"))
                        .expect("event id"),
                    timestamp: chrono::DateTime::from_timestamp(NOW_SEC - days * DAY_SECS, 0)
                        .expect("timestamp"),
                    pain: *pain,
                    importance: *importance,
                    pinned: false,
                    skill_action: skill_action.to_string(),
                    source_harness: "drm-108".to_string(),
                    content: content.to_string(),
                };
                log.push_str(&serde_json::to_string(&learning).expect("serialize"));
                log.push('\n');
            }
            std::fs::write(root.episodic_jsonl(), log).expect("write the log");

            let lock = WriterLock::acquire(&root, WriterKind::Watch).expect("writer lock");
            let handle =
                TantivyIndexHandle::open(&root, &lock, DEFAULT_COMMIT_CADENCE).expect("open");
            let (_, fields) = build_schema();
            let store = Self {
                handle,
                _lock: lock,
                fields,
                _dir: dir,
            };
            assert_eq!(
                store.recall("", 0).len(),
                0,
                "sanity: an empty query matches nothing"
            );
            store
        }

        fn reader(&self) -> &RecallReader {
            self.handle.reader()
        }

        fn try_recall(&self, query: &str, k: usize) -> tantivy::Result<Vec<RecallResult>> {
            recall(self.reader(), &self.fields, query, k, NOW_SEC)
        }

        fn recall(&self, query: &str, k: usize) -> Vec<RecallResult> {
            self.try_recall(query, k)
                .unwrap_or_else(|e| panic!("{query:?}: {e}"))
        }

        /// What recall returned before DRM-108: the same search, with the
        /// query built by Tantivy's `QueryParser`, through the same collector
        /// and the same BZR-1522 re-rank. With `rerank` false, the bag alone.
        fn parser_recall(&self, query: &str, k: usize) -> Vec<RecallResult> {
            self.parser_search(query, k, true)
        }

        fn parser_search(&self, query: &str, k: usize, rerank: bool) -> Vec<RecallResult> {
            let searcher = self.reader().searcher();
            let recurrence = self.reader().recurrence(&searcher).expect("recurrence");
            let parser = QueryParser::for_index(searcher.index(), vec![self.fields.content]);
            let search =
                |text: &str, limit: usize| -> tantivy::Result<Vec<(DocAddress, RecallResult)>> {
                    let parsed = parser.parse_query(text)?;
                    let collector = SalienceCollector::new(limit, NOW_SEC, Arc::clone(&recurrence));
                    let top: Vec<ScoredDoc> = searcher.search(&*parsed, &collector)?;
                    hydrate_addressed(&searcher, &self.fields, top, limit)
                };
            let bag = search(query, k).unwrap_or_else(|e| panic!("parser {query:?}: {e}"));
            if !rerank {
                return bag.into_iter().map(|(_, result)| result).collect();
            }
            rank_missing_tokens(bag, query, k, search).expect("parser re-rank")
        }
    }

    /// Each hit's id, BM25 bits, salience bits and score bits, in order.
    fn bits(results: &[RecallResult]) -> Vec<(String, u64, u64, u64)> {
        results
            .iter()
            .map(|r| {
                (
                    r.event_id.clone(),
                    r.bm25.to_bits(),
                    r.salience.to_bits(),
                    r.score.to_bits(),
                )
            })
            .collect()
    }

    /// DRM-108 test 1: no text is an error, whatever its punctuation.
    #[tokio::test]
    async fn any_text_is_a_query() {
        let store = WordsStore::open();
        for query in [
            "What's the best way to handle errors here?",
            "Don't retry on 4xx",
            "I can't get `cargo test` to pass",
            "run with --verbose",
            "why does parse_config() panic",
            "Update README.md: add an install section",
            "see https://docs.rs/tokio/latest",
            "add [dev-dependencies]",
            "Handle the case where x > 10 && y < 5",
            "We don't deploy on Fridays.",
            "std::sync::Mutex",
            "fn main() {",
            "axum AND",
            "*",
            "timestamp_sec:[0 TO 9999999999]",
            "+axum +tokio",
            "NOT tokio",
            "\"unbalanced",
            "(open",
            "rust::error",
        ] {
            let result = store.try_recall(query, 5);
            assert!(result.is_ok(), "{query:?}: {result:?}");
        }
    }

    /// DRM-108 test 2 (D1, Amendment 2): every query the parser took scores
    /// bit for bit as it did: the same ids in the same order, with the same
    /// BM25, salience and score. `rust error handling axum` takes the
    /// BZR-1522 re-rank, so the reference runs it too.
    #[tokio::test]
    async fn plain_queries_score_as_the_parser_did() {
        let store = WordsStore::open();
        let gap = format!("foo_{}_bar", WordsStore::long_word());
        for query in [
            "tokio",
            "tokio runtime",
            "tokio tokio runtime",
            "Tokio tokio runtime",
            "foo_bar baz",
            "dead-lock tokio",
            "axum handler error",
            "zzznotindexed",
            "rust error handling axum",
            // DRM-109 (from DRM-108's verify): a tab and a newline split words
            // as a space does.
            "axum\thandler\nerror tokio",
            gap.as_str(),
        ] {
            let want = store.parser_recall(query, 20);
            assert_eq!(
                want.is_empty(),
                query == "zzznotindexed",
                "{query:?}: the fixture matches every query but zzznotindexed"
            );
            assert_eq!(bits(&store.recall(query, 20)), bits(&want), "{query:?}");
        }
        // The BZR-1522 path changed the top hit: the bag's lacks `axum`.
        const RERANKED: &str = "rust error handling axum";
        let bag_top = store.parser_search(RERANKED, 20, false)[0].content.clone();
        let top = store.recall(RERANKED, 20)[0].content.clone();
        assert!(
            !bag_top.contains("axum") && top.contains("axum"),
            "the re-rank ran: bag top {bag_top:?}, top {top:?}"
        );
    }

    /// DRM-108 test 3: text with no words matches nothing.
    #[tokio::test]
    async fn text_with_no_words_matches_nothing() {
        let store = WordsStore::open();
        let long = WordsStore::long_word();
        for query in ["", "   ", "*", "!!! ???", long.as_str()] {
            assert_eq!(bits(&store.recall(query, 20)), vec![], "{query:?}");
        }
    }

    /// DRM-108 test 4 (D3): operators and quote marks are words.
    #[tokio::test]
    async fn operators_are_words() {
        let store = WordsStore::open();
        let ids_of = |query: &str| -> Vec<String> {
            store
                .recall(query, 20)
                .into_iter()
                .map(|r| r.event_id)
                .collect()
        };
        let both = ids_of("+axum +tokio");
        assert!(
            both.iter().any(|id| id == WORDS_A) && both.iter().any(|id| id == WORDS_B),
            "+axum +tokio returns A and B: {both:?}"
        );
        assert_eq!(
            ids_of("\"tokio runtime\"").first().map(String::as_str),
            Some(WORDS_A),
            "\"tokio runtime\" is two words, and A ranks first"
        );
        assert!(
            ids_of("NOT tokio").iter().any(|id| id == WORDS_A),
            "NOT tokio returns A"
        );
    }

    /// DRM-108 test 5: the exclusion wraps the word query. A is gone, and
    /// every other hit scores as in [`recall`].
    #[tokio::test]
    async fn exclusion_wraps_the_word_query() {
        let store = WordsStore::open();
        const QUERY: &str = "What's tokio";
        let all = store.recall(QUERY, 20);
        assert!(
            all.iter().any(|r| r.event_id == WORDS_A),
            "A matches {QUERY:?} without the exclusion"
        );
        let exclusion = RecallExclusion {
            ids: vec![WORDS_A.to_string()],
            cluster_prefixes: Vec::new(),
        };
        let kept = recall_excluding(
            store.reader(),
            &store.fields,
            QUERY,
            20,
            &exclusion,
            NOW_SEC,
        )
        .expect("recall_excluding");
        let without_a: Vec<RecallResult> =
            all.into_iter().filter(|r| r.event_id != WORDS_A).collect();
        assert_eq!(bits(&kept), bits(&without_a));
    }
}
