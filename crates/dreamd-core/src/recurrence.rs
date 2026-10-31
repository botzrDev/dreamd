//! Recall recurrence as one derived table per searcher (BZR-1242).
//!
//! Recall's `recurrence` for a document with key `K` is the number of alive
//! `layer:episodic` documents in the searcher whose `skill_action` equals `K`
//! or extends it with `::`. Every episodic document counts itself, so its
//! value is at least 1. A lesson document gets the count of its
//! `cluster_key`. The index equals the log (BZR-1236), so this is also the
//! count of live log records under the same rule ([`ClusterCounts`]).
//!
//! [`RecurrenceWarmer`] computes the table once per searcher generation, before
//! the reader publishes the searcher, and caches each segment's scan by
//! `(SegmentId, delete_opstamp)`. An append-only commit therefore scans only
//! the new segment. [`RecallReader`] is an `IndexReader` with the warmer
//! attached; every recall, score and report entry point takes one.
//!
//! The `recurrence` fastfield is written as 0 and read by nothing. Promotion
//! keeps its own windowed count (`consolidation::compute_promoted_clusters`).

use std::collections::{BTreeMap, HashMap, HashSet};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use dreamd_protocol::AgentLearning;
use tantivy::index::SegmentId;
use tantivy::schema::{Field, IndexRecordOption, Schema, Term};
use tantivy::{
    DocId, DocSet, Index, IndexReader, Opstamp, ReloadPolicy, Searcher, SearcherGeneration,
    SegmentReader, Warmer, TERMINATED,
};

use crate::index::{Layer, LAYER_FIELD, SKILL_ACTION_FIELD};

/// Recall recurrence: records whose `skill_action` equals a key or extends it
/// with `::` (BZR-1242). Promotion uses its own windowed count.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClusterCounts {
    exact: BTreeMap<Vec<u8>, u32>,
}

impl ClusterCounts {
    /// Count `events` by `skill_action`. Each record given counts once; pass one
    /// record per id.
    pub fn from_events<'a>(events: impl IntoIterator<Item = &'a AgentLearning>) -> Self {
        let mut counts = Self::default();
        for ev in events {
            counts.add(ev.skill_action.as_bytes(), 1);
        }
        counts
    }

    pub(crate) fn add(&mut self, key: &[u8], n: u32) {
        *self.exact.entry(key.to_vec()).or_insert(0) += n;
    }

    /// `key`'s own count plus every key that extends it with `::`. Keys that
    /// start with `key::` are exactly the range [`key::`, `key:;`), because `;`
    /// is the byte after `:`.
    pub fn subtree(&self, key: &str) -> u32 {
        self.subtree_bytes(key.as_bytes())
    }

    pub(crate) fn subtree_bytes(&self, key: &[u8]) -> u32 {
        let lo = [key, b"::"].concat();
        let hi = [key, b":;"].concat();
        let own = self.exact.get(key).copied().unwrap_or(0);
        own + self.exact.range(lo..hi).map(|(_, n)| n).sum::<u32>()
    }
}

/// A doc with no `skill_action` term.
const NO_KEY: u32 = u32::MAX;

/// One segment's `skill_action` view, independent of every other segment.
struct SegmentScan {
    /// Term bytes, by term ordinal.
    keys: Vec<Vec<u8>>,
    /// Alive `layer:episodic` docs, by ordinal.
    episodic_by_ord: Vec<u32>,
    /// Ordinal per doc id; [`NO_KEY`] if none.
    ord_by_doc: Vec<u32>,
}

/// One segment's recurrence, read by doc id.
pub struct SegmentRecurrence {
    scan: Arc<SegmentScan>,
    /// [`ClusterCounts::subtree`] of each ordinal's key.
    rec_by_ord: Vec<u32>,
}

impl SegmentRecurrence {
    /// The doc's recurrence; 0 for a doc with no `skill_action`.
    pub fn get(&self, doc: DocId) -> u32 {
        match self.scan.ord_by_doc.get(doc as usize) {
            Some(&ord) if ord != NO_KEY => self.rec_by_ord[ord as usize],
            _ => 0,
        }
    }
}

/// The recurrence of every doc in one searcher, by segment.
pub struct RecurrenceTables {
    segments: HashMap<SegmentId, Arc<SegmentRecurrence>>,
}

impl RecurrenceTables {
    /// The table for one of the searcher's segments; `None` for a segment the
    /// searcher does not hold.
    pub fn segment(&self, id: SegmentId) -> Option<Arc<SegmentRecurrence>> {
        self.segments.get(&id).cloned()
    }
}

/// The Tantivy [`Warmer`] that builds a [`RecurrenceTables`] for every
/// searcher generation.
pub struct RecurrenceWarmer {
    skill_action: Field,
    layer: Field,
    /// Never held while a segment is scanned.
    state: Mutex<WarmState>,
    /// Segments scanned, for the cache tests.
    #[cfg(test)]
    scans: AtomicUsize,
    /// [`Self::tables`] calls that found no table and computed one.
    #[cfg(test)]
    misses: AtomicUsize,
}

#[derive(Default)]
struct WarmState {
    scans: HashMap<(SegmentId, Option<Opstamp>), Arc<SegmentScan>>,
    /// By generation id.
    tables: HashMap<u64, Arc<RecurrenceTables>>,
}

impl RecurrenceWarmer {
    /// Fields resolved by name from `schema`.
    pub fn new(schema: &Schema) -> tantivy::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            skill_action: schema.get_field(SKILL_ACTION_FIELD)?,
            layer: schema.get_field(LAYER_FIELD)?,
            state: Mutex::new(WarmState::default()),
            #[cfg(test)]
            scans: AtomicUsize::new(0),
            #[cfg(test)]
            misses: AtomicUsize::new(0),
        }))
    }

    /// The cached table for `searcher`'s generation, else compute it, cache it
    /// and (in tests) count a miss.
    pub fn tables(&self, searcher: &Searcher) -> tantivy::Result<Arc<RecurrenceTables>> {
        let generation = searcher.generation().generation_id();
        if let Some(tables) = self.lock().tables.get(&generation) {
            return Ok(Arc::clone(tables));
        }
        #[cfg(test)]
        self.misses.fetch_add(1, Ordering::Relaxed);
        self.compute(searcher)
    }

    /// The state only ever holds complete `Arc`s, so a poisoned lock is still
    /// consistent.
    fn lock(&self) -> MutexGuard<'_, WarmState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn compute(&self, searcher: &Searcher) -> tantivy::Result<Arc<RecurrenceTables>> {
        let generation = searcher.generation().generation_id();
        let readers = searcher.segment_readers();

        // 1. The cached scans, under the lock.
        let cached: Vec<Option<Arc<SegmentScan>>> = {
            let state = self.lock();
            readers
                .iter()
                .map(|r| {
                    state
                        .scans
                        .get(&(r.segment_id(), r.delete_opstamp()))
                        .cloned()
                })
                .collect()
        };

        // 2. Scan every segment that had none, without the lock.
        let mut fresh = Vec::new();
        let mut scans = Vec::with_capacity(readers.len());
        for (reader, cached) in readers.iter().zip(cached) {
            let scan = match cached {
                Some(scan) => scan,
                None => {
                    let scan = Arc::new(scan_segment(reader, self.skill_action, self.layer)?);
                    #[cfg(test)]
                    self.scans.fetch_add(1, Ordering::Relaxed);
                    fresh.push((
                        (reader.segment_id(), reader.delete_opstamp()),
                        Arc::clone(&scan),
                    ));
                    scan
                }
            };
            scans.push((reader.segment_id(), scan));
        }

        // 3. One count across every segment, keyed by term bytes.
        let mut counts = ClusterCounts::default();
        for (_, scan) in &scans {
            for (key, &n) in scan.keys.iter().zip(&scan.episodic_by_ord) {
                if n > 0 {
                    counts.add(key, n);
                }
            }
        }

        // 4. Each ordinal's subtree. A key in several segments is summed once.
        let mut subtree: HashMap<&[u8], u32> = HashMap::new();
        let mut segments = HashMap::with_capacity(scans.len());
        for (id, scan) in &scans {
            let rec_by_ord = scan
                .keys
                .iter()
                .map(|key| {
                    *subtree
                        .entry(key.as_slice())
                        .or_insert_with(|| counts.subtree_bytes(key))
                })
                .collect();
            segments.insert(
                *id,
                Arc::new(SegmentRecurrence {
                    scan: Arc::clone(scan),
                    rec_by_ord,
                }),
            );
        }
        let tables = Arc::new(RecurrenceTables { segments });

        // 5. Two callers computing one generation get equal tables; the last
        //    insert wins.
        let mut state = self.lock();
        state.scans.extend(fresh);
        state.tables.insert(generation, Arc::clone(&tables));
        Ok(tables)
    }
}

impl Warmer for RecurrenceWarmer {
    fn warm(&self, searcher: &Searcher) -> tantivy::Result<()> {
        self.compute(searcher).map(drop)
    }

    fn garbage_collect(&self, live: &[&SearcherGeneration]) {
        let generations: HashSet<u64> = live.iter().map(|g| g.generation_id()).collect();
        let segments: HashSet<(SegmentId, Option<Opstamp>)> = live
            .iter()
            .flat_map(|g| g.segments().iter().map(|(id, opstamp)| (*id, *opstamp)))
            .collect();
        let mut state = self.lock();
        state
            .tables
            .retain(|generation, _| generations.contains(generation));
        state.scans.retain(|key, _| segments.contains(key));
    }
}

/// One segment's keys, its alive `layer:episodic` count per key, and each
/// doc's key. Postings include deleted docs; the alive bitset filters them.
fn scan_segment(
    segment: &SegmentReader,
    skill_action: Field,
    layer: Field,
) -> tantivy::Result<SegmentScan> {
    let max_doc = segment.max_doc() as usize;

    let mut episodic = vec![false; max_doc];
    let term = Term::from_field_text(layer, Layer::Episodic.as_str());
    if let Some(mut postings) = segment
        .inverted_index(layer)?
        .read_postings(&term, IndexRecordOption::Basic)?
    {
        let alive = segment.alive_bitset();
        let mut doc = postings.doc();
        while doc != TERMINATED {
            if alive.is_none_or(|bits| bits.is_alive(doc)) {
                episodic[doc as usize] = true;
            }
            doc = postings.advance();
        }
    }

    let inverted = segment.inverted_index(skill_action)?;
    let mut keys = Vec::new();
    let mut episodic_by_ord = Vec::new();
    let mut ord_by_doc = vec![NO_KEY; max_doc];
    let mut terms = inverted.terms().stream()?;
    while terms.advance() {
        let ord = keys.len() as u32;
        keys.push(terms.key().to_vec());
        let mut postings =
            inverted.read_postings_from_terminfo(terms.value(), IndexRecordOption::Basic)?;
        let mut count = 0;
        let mut doc = postings.doc();
        while doc != TERMINATED {
            ord_by_doc[doc as usize] = ord;
            if episodic[doc as usize] {
                count += 1;
            }
            doc = postings.advance();
        }
        episodic_by_ord.push(count);
    }

    Ok(SegmentScan {
        keys,
        episodic_by_ord,
        ord_by_doc,
    })
}

/// An `IndexReader` with a [`RecurrenceWarmer`] attached. The reader holds a
/// `Weak`; this struct keeps the warmer alive for as long as the reader lives.
#[derive(Clone)]
pub struct RecallReader {
    reader: IndexReader,
    recurrence: Arc<RecurrenceWarmer>,
}

impl RecallReader {
    pub fn open(index: &Index, policy: ReloadPolicy) -> tantivy::Result<Self> {
        let recurrence = RecurrenceWarmer::new(&index.schema())?;
        let warmer: Weak<dyn Warmer> = Arc::downgrade(&recurrence) as Weak<dyn Warmer>;
        let reader = index
            .reader_builder()
            .reload_policy(policy)
            .warmers(vec![warmer])
            .try_into()?;
        Ok(Self { reader, recurrence })
    }

    pub fn searcher(&self) -> Searcher {
        self.reader.searcher()
    }

    pub fn reload(&self) -> tantivy::Result<()> {
        self.reader.reload()
    }

    /// The recurrence table for `searcher`, which must come from this reader.
    pub fn recurrence(&self, searcher: &Searcher) -> tantivy::Result<Arc<RecurrenceTables>> {
        self.recurrence.tables(searcher)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::recall;
    use crate::index::{build_schema, SchemaFields};
    use chrono::{DateTime, Utc};
    use dreamd_protocol::EventId;
    use proptest::prelude::*;
    use std::collections::BTreeSet;
    use tantivy::merge_policy::NoMergePolicy;
    use tantivy::schema::Value;
    use tantivy::{doc, DocAddress, IndexWriter, TantivyDocument};

    const NOW: i64 = 1_747_137_600;

    /// `ab` shares a byte prefix with `a` but not a segment, and `a::bc` shares
    /// one with `a::b`.
    const KEYS: [&str; 5] = ["a", "ab", "a::b", "a::bc", "a::b::c"];

    /// A valid id per `n`: 20 fixed Crockford characters and six digits.
    fn event_id(n: usize) -> String {
        format!("evt_01ARZ3NDEKTSV4RRFFQ6{n:06}")
    }

    fn learning(id: &str, skill_action: &str) -> AgentLearning {
        AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: EventId::parse(id).expect("valid id"),
            timestamp: DateTime::parse_from_rfc3339("2026-05-14T08:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            pain: 5.0,
            importance: 6.0,
            pinned: false,
            skill_action: skill_action.to_string(),
            source_harness: "test-harness".to_string(),
            content: "quokka".to_string(),
        }
    }

    /// An in-RAM index whose writer makes one segment per commit and never
    /// merges.
    fn index() -> (Index, SchemaFields, IndexWriter<TantivyDocument>) {
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema);
        let writer = index
            .writer_with_num_threads(1, 15_000_000)
            .expect("writer");
        writer.set_merge_policy(Box::new(NoMergePolicy));
        (index, fields, writer)
    }

    fn add(
        writer: &mut IndexWriter<TantivyDocument>,
        fields: &SchemaFields,
        id: &str,
        key: &str,
        layer: Layer,
    ) {
        writer
            .add_document(doc!(
                fields.content => "quokka",
                fields.timestamp_sec => NOW as u64,
                fields.pain => 5.0_f64,
                fields.importance => 5.0_f64,
                fields.layer => layer.as_str(),
                fields.event_id => id,
                fields.skill_action => key,
            ))
            .expect("add doc");
    }

    fn delete(writer: &mut IndexWriter<TantivyDocument>, fields: &SchemaFields, id: &str) {
        writer.delete_term(Term::from_field_text(fields.event_id, id));
    }

    /// Every alive doc's id and its table value, sorted by id.
    fn values(reader: &RecallReader) -> Vec<(String, u32)> {
        let (_, fields) = build_schema();
        let searcher = reader.searcher();
        let tables = reader.recurrence(&searcher).expect("tables");
        let mut out = Vec::new();
        for (ord, segment) in searcher.segment_readers().iter().enumerate() {
            let table = tables
                .segment(segment.segment_id())
                .expect("a table for every segment");
            for doc in segment.doc_ids_alive() {
                let stored: TantivyDocument = searcher
                    .doc(DocAddress::new(ord as u32, doc))
                    .expect("stored doc");
                let id = stored
                    .get_first(fields.event_id)
                    .and_then(|v| v.as_str())
                    .expect("stored id")
                    .to_owned();
                out.push((id, table.get(doc)));
            }
        }
        out.sort();
        out
    }

    fn scans(reader: &RecallReader) -> usize {
        reader.recurrence.scans.load(Ordering::Relaxed)
    }

    #[test]
    fn subtree_counts_the_key_and_its_colon_descendants() {
        // consolidation promotes at the deepest prefix that met the threshold,
        // so leaf events must count toward the promoted parent.
        let events = [
            learning(&event_id(0), "rust::eh"),
            learning(&event_id(1), "rust::eh::unwrap"),
            learning(&event_id(2), "rust::eh::expect"),
            // Sibling with a shared textual prefix but a different segment —
            // must not count.
            learning(&event_id(3), "rust::ehlers"),
            learning(&event_id(4), "python::pytest"),
        ];
        let counts = ClusterCounts::from_events(&events);
        assert_eq!(counts.subtree("rust::eh"), 3);
        assert_eq!(counts.subtree("rust::eh::unwrap"), 1);
        assert_eq!(counts.subtree("python::pytest"), 1);
        assert_eq!(counts.subtree("go::testing"), 0);
    }

    #[test]
    fn subtree_stops_at_the_segment_boundary() {
        let events: Vec<AgentLearning> = KEYS
            .iter()
            .enumerate()
            .map(|(n, key)| learning(&event_id(n), key))
            .collect();
        let counts = ClusterCounts::from_events(&events);
        assert_eq!(counts.subtree("a"), 4, "`ab` is not under `a`");
        assert_eq!(counts.subtree("a::b"), 2, "`a::bc` is not under `a::b`");
        assert_eq!(counts.subtree("ab"), 1);
    }

    /// One generated store.
    #[derive(Debug, Clone)]
    struct Case {
        commits: usize,
        /// Each episodic doc's key index and commit slot.
        docs: Vec<(usize, usize)>,
        /// The commit slots of `lsn_a` and `lsn_ab`.
        lessons: (usize, usize),
        /// Which episodic ids the last commit deletes, by doc index.
        deletes: Vec<bool>,
    }

    fn case() -> impl Strategy<Value = Case> {
        (
            1usize..=4,
            prop::collection::vec((0..KEYS.len(), 0usize..4), 0..=40),
            (0usize..4, 0usize..4),
            prop::collection::vec(any::<bool>(), 40),
        )
            .prop_map(|(commits, docs, lessons, deletes)| Case {
                commits,
                docs,
                lessons,
                deletes,
            })
    }

    /// The table equals the log rule over the alive episodic docs, for every
    /// alive doc, lessons included.
    fn check_against_counts(
        reader: &RecallReader,
        keys: &BTreeMap<String, &str>,
        episodic: &BTreeSet<String>,
    ) -> Result<(), TestCaseError> {
        let records: Vec<AgentLearning> =
            episodic.iter().map(|id| learning(id, keys[id])).collect();
        let counts = ClusterCounts::from_events(&records);
        let want: Vec<(String, u32)> = keys
            .iter()
            .map(|(id, key)| (id.clone(), counts.subtree(key)))
            .collect();
        prop_assert_eq!(values(reader), want);
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn tables_match_cluster_counts_across_segments_and_deletes(case in case()) {
            let (index, fields, mut writer) = index();
            let reader = RecallReader::open(&index, ReloadPolicy::Manual).expect("reader");
            // Every alive doc's key by id, and the alive episodic ids.
            let mut keys: BTreeMap<String, &str> = BTreeMap::new();
            let mut episodic: BTreeSet<String> = BTreeSet::new();
            let lessons = [
                ("lsn_a", "a", case.lessons.0),
                ("lsn_ab", "a::b", case.lessons.1),
            ];
            for commit in 0..case.commits {
                for (n, &(key, slot)) in case.docs.iter().enumerate() {
                    if slot % case.commits == commit {
                        let id = event_id(n);
                        add(&mut writer, &fields, &id, KEYS[key], Layer::Episodic);
                        keys.insert(id.clone(), KEYS[key]);
                        episodic.insert(id);
                    }
                }
                for &(id, key, slot) in &lessons {
                    if slot % case.commits == commit {
                        add(&mut writer, &fields, id, key, Layer::Semantic);
                        keys.insert(id.to_owned(), key);
                    }
                }
                writer.commit().expect("commit");
                reader.reload().expect("reload");
                check_against_counts(&reader, &keys, &episodic)?;
            }

            for (n, _) in case
                .deletes
                .iter()
                .enumerate()
                .take(case.docs.len())
                .filter(|(_, &deleted)| deleted)
            {
                let id = event_id(n);
                delete(&mut writer, &fields, &id);
                keys.remove(&id);
                episodic.remove(&id);
            }
            writer.commit().expect("commit");
            reader.reload().expect("reload");
            check_against_counts(&reader, &keys, &episodic)?;
        }
    }

    #[test]
    fn an_append_only_commit_rescans_only_the_new_segment() {
        let (index, fields, mut writer) = index();
        let reader = RecallReader::open(&index, ReloadPolicy::Manual).expect("reader");
        for n in 0..3 {
            add(&mut writer, &fields, &event_id(n), "k", Layer::Episodic);
        }
        writer.commit().expect("commit");
        reader.reload().expect("reload");
        let before = scans(&reader);

        add(&mut writer, &fields, &event_id(3), "k", Layer::Episodic);
        writer.commit().expect("commit");
        reader.reload().expect("reload");

        assert_eq!(reader.searcher().segment_readers().len(), 2);
        assert_eq!(
            scans(&reader),
            before + 1,
            "only the new segment is scanned"
        );
        let want: Vec<(String, u32)> = (0..4).map(|n| (event_id(n), 4)).collect();
        assert_eq!(values(&reader), want);
    }

    #[test]
    fn a_delete_in_an_old_segment_rescans_that_segment() {
        let (index, fields, mut writer) = index();
        let reader = RecallReader::open(&index, ReloadPolicy::Manual).expect("reader");
        // Segment A: three docs under `k`. Segment B: two under `j`.
        for n in 0..3 {
            add(&mut writer, &fields, &event_id(n), "k", Layer::Episodic);
        }
        writer.commit().expect("commit");
        for n in 3..5 {
            add(&mut writer, &fields, &event_id(n), "j", Layer::Episodic);
        }
        writer.commit().expect("commit");
        reader.reload().expect("reload");
        let before = scans(&reader);

        delete(&mut writer, &fields, &event_id(0));
        writer.commit().expect("commit");
        reader.reload().expect("reload");

        assert_eq!(
            scans(&reader),
            before + 1,
            "only segment A is scanned again"
        );
        let want: Vec<(String, u32)> = (1..5).map(|n| (event_id(n), 2)).collect();
        assert_eq!(values(&reader), want);
    }

    #[test]
    fn garbage_collect_keeps_only_live_generations() {
        let (index, fields, mut writer) = index();
        add(&mut writer, &fields, &event_id(0), "k", Layer::Episodic);
        add(&mut writer, &fields, &event_id(1), "k", Layer::Episodic);
        writer.commit().expect("commit");
        // A plain reader and a warmer it does not hold, so no GC thread calls
        // this warmer.
        let reader: IndexReader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .expect("reader");
        let warmer = RecurrenceWarmer::new(&index.schema()).expect("warmer");
        let g1 = reader.searcher();
        warmer.tables(&g1).expect("g1 tables");

        // g2 adds a segment, and its delete leaves g1's scan of the first
        // segment stale.
        add(&mut writer, &fields, &event_id(2), "k", Layer::Episodic);
        delete(&mut writer, &fields, &event_id(0));
        writer.commit().expect("commit");
        reader.reload().expect("reload");
        let g2 = reader.searcher();
        warmer.tables(&g2).expect("g2 tables");
        assert_eq!(warmer.lock().tables.len(), 2);
        assert_eq!(warmer.lock().scans.len(), 3);

        warmer.garbage_collect(&[g2.generation()]);

        let state = warmer.lock();
        let tables: Vec<u64> = state.tables.keys().copied().collect();
        assert_eq!(tables, vec![g2.generation().generation_id()]);
        let scans: HashSet<(SegmentId, Option<Opstamp>)> = state.scans.keys().copied().collect();
        let live: HashSet<(SegmentId, Option<Opstamp>)> = g2
            .segment_readers()
            .iter()
            .map(|r| (r.segment_id(), r.delete_opstamp()))
            .collect();
        assert_eq!(scans, live);
    }

    #[test]
    fn queries_after_a_reload_hit_the_warmed_table() {
        let (index, fields, mut writer) = index();
        let reader = RecallReader::open(&index, ReloadPolicy::Manual).expect("reader");
        add(&mut writer, &fields, &event_id(0), "k", Layer::Episodic);
        add(&mut writer, &fields, &event_id(1), "k::x", Layer::Episodic);
        writer.commit().expect("commit");
        reader.reload().expect("reload");

        let results = recall(&reader, &fields, "quokka", 10, NOW).expect("recall");
        let mut got: Vec<(String, u64)> = results
            .iter()
            .map(|r| (r.event_id.clone(), r.recurrence))
            .collect();
        got.sort();
        assert_eq!(got, vec![(event_id(0), 2), (event_id(1), 1)]);
        assert_eq!(
            reader.recurrence.misses.load(Ordering::Relaxed),
            0,
            "the reload warmed the table"
        );
    }
}
