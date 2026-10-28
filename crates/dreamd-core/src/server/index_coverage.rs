//! Index coverage by set difference (BZR-1236).
//!
//! The episodic log is the source of truth, and the index is a cache of it.
//! This module compares the two as sets of event ids: the log's records on one
//! side, the alive `evt_` terms of the `event_id` field on the other, both
//! keyed by the 26-byte ULID body.
//!
//! [`cover`] closes the difference on a writer: it adds log ids the index does
//! not hold, re-adds ids with more than one alive document, and deletes indexed
//! ids the log does not hold. `TantivyIndexHandle::open` runs it at every open,
//! and the indexer task runs it on `IndexerMsg::Resync`. Neither a watermark nor
//! the order ids arrived in enters into it, so a gap below the watermark and a
//! lost watermark both heal.
//!
//! [`assess_index_coverage`] reports the same difference from the committed
//! index with no writer and no lock, for `dreamd doctor`.

use std::collections::{BTreeSet, HashMap};

use dreamd_protocol::AgentLearning;
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::{
    DocSet, Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term, TERMINATED,
};

use crate::index::{SchemaFields, EVENT_ID_FIELD};
use crate::layout::AgentRoot;
use crate::server::index_freshness::read_jsonl_events;
use crate::server::index_map::{tantivy_to_index, IndexError};
use crate::server::indexer_actor::add_document;
use crate::server::tantivy_handle::INDEX_DIR_NAME;

/// A pass skips its deletes when they would remove more than half of the
/// indexed ids and at least this many.
const MASS_DELETE_FLOOR: usize = 64;

/// The 26-byte ULID body of an `evt_` id. Both sides of the difference use it.
pub(crate) type IdKey = [u8; 26];

#[derive(Debug, Default)]
pub(crate) struct Indexed {
    /// Alive episodic docs per id.
    pub(crate) alive: HashMap<IdKey, u32>,
    /// `evt_` terms whose body is not 26 bytes, by their full bytes.
    pub(crate) malformed: BTreeSet<Vec<u8>>,
}

pub(crate) fn scan_indexed(reader: &IndexReader, event_id: Field) -> tantivy::Result<Indexed> {
    let searcher = reader.searcher();
    let mut out = Indexed::default();
    for segment in searcher.segment_readers() {
        let inverted = segment.inverted_index(event_id)?;
        let alive = segment.alive_bitset();
        let mut terms = inverted
            .terms()
            .range()
            .ge(b"evt_")
            .lt(b"evt`")
            .into_stream()?;
        while terms.advance() {
            let mut postings =
                inverted.read_postings_from_terminfo(terms.value(), IndexRecordOption::Basic)?;
            let mut n = 0u32;
            let mut doc = postings.doc();
            while doc != TERMINATED {
                if alive.is_none_or(|bits| bits.is_alive(doc)) {
                    n += 1;
                }
                doc = postings.advance();
            }
            if n == 0 {
                continue;
            }
            match <IdKey>::try_from(&terms.key()[4..]) {
                Ok(key) => *out.alive.entry(key).or_insert(0) += n,
                Err(_) => {
                    out.malformed.insert(terms.key().to_vec());
                }
            }
        }
    }
    Ok(out)
}

/// The set difference between the log and the index.
#[derive(Debug, Default)]
pub(crate) struct Diff<'a> {
    /// Log records with no alive document, in log order.
    pub(crate) missing: Vec<&'a AgentLearning>,
    /// Log records with more than one alive document, in log order.
    pub(crate) duplicated: Vec<&'a AgentLearning>,
    /// Indexed keys with no log record, sorted.
    pub(crate) not_in_log: Vec<IdKey>,
    /// `evt_` terms whose body is not 26 bytes, sorted.
    pub(crate) malformed: Vec<Vec<u8>>,
}

/// The key of a log record. `EventId` guarantees `evt_` plus 26 characters.
fn key_of(event: &AgentLearning) -> IdKey {
    <IdKey>::try_from(&event.id.as_str().as_bytes()[4..])
        .expect("EventId is evt_ followed by 26 characters")
}

/// The one difference rule; [`cover`] and [`assess_index_coverage`] share it.
///
/// When the log repeats an id, the last record wins. `check_store` reports the
/// repeat itself as `DuplicateEventId`.
pub(crate) fn diff<'a>(log: &'a [AgentLearning], indexed: &Indexed) -> Diff<'a> {
    let mut last: HashMap<IdKey, usize> = HashMap::with_capacity(log.len());
    for (i, event) in log.iter().enumerate() {
        last.insert(key_of(event), i);
    }
    let mut out = Diff::default();
    for (i, event) in log.iter().enumerate() {
        let key = key_of(event);
        if last[&key] != i {
            continue;
        }
        match indexed.alive.get(&key).copied().unwrap_or(0) {
            0 => out.missing.push(event),
            1 => {}
            _ => out.duplicated.push(event),
        }
    }
    out.not_in_log = indexed
        .alive
        .keys()
        .filter(|key| !last.contains_key(*key))
        .copied()
        .collect();
    out.not_in_log.sort_unstable();
    out.malformed = indexed.malformed.iter().cloned().collect();
    out
}

/// What one coverage pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CoverageReport {
    /// Log ids that had no alive document and were added.
    pub added: usize,
    /// Log ids that had more than one alive document and were re-added once.
    pub healed: usize,
    /// Indexed terms with no log record that were deleted.
    pub deleted: usize,
    /// Indexed terms with no log record that the mass-delete guard kept.
    pub deletes_skipped: usize,
}

/// Close the difference between `events` and the index on `writer`.
///
/// `reader` must be a Manual-reload reader over the writer's index; it is
/// reloaded here, so it sees the last commit and nothing after it.
///
/// Deletes are skipped, with one `warn!`, when the log has no records, or when
/// they would remove more than half of the indexed ids and at least
/// [`MASS_DELETE_FLOOR`]. Adds and heals still run. Does not commit: the
/// caller owns the commit.
pub(crate) fn cover(
    writer: &mut IndexWriter<TantivyDocument>,
    fields: &SchemaFields,
    reader: &IndexReader,
    events: &[AgentLearning],
) -> Result<CoverageReport, IndexError> {
    reader.reload().map_err(tantivy_to_index)?;
    let indexed = scan_indexed(reader, fields.event_id).map_err(tantivy_to_index)?;
    let diff = diff(events, &indexed);

    let mut report = CoverageReport::default();
    for event in &diff.missing {
        add_document(writer, fields, event)?;
        report.added += 1;
    }
    for event in &diff.duplicated {
        add_document(writer, fields, event)?;
        report.healed += 1;
    }

    let deletes = diff.not_in_log.len() + diff.malformed.len();
    let indexed_ids = indexed.alive.len() + indexed.malformed.len();
    if deletes > 0 {
        let mass = deletes * 2 > indexed_ids && deletes >= MASS_DELETE_FLOOR;
        if events.is_empty() || mass {
            report.deletes_skipped = deletes;
            tracing::warn!(
                log_records = events.len(),
                indexed = indexed_ids,
                not_in_log = deletes,
                "the index holds ids the episodic log does not; kept them, because \
                 the log is empty or they are most of the index. Run \
                 `dreamd doctor --repair` to rebuild the index from the log"
            );
        } else {
            for key in &diff.not_in_log {
                let term = [b"evt_".as_slice(), key.as_slice()].concat();
                delete_event_id_term(writer, fields.event_id, &term);
            }
            for term in &diff.malformed {
                delete_event_id_term(writer, fields.event_id, term);
            }
            report.deleted = deletes;
        }
    }

    if report != CoverageReport::default() {
        tracing::info!(
            added = report.added,
            healed = report.healed,
            deleted = report.deleted,
            deletes_skipped = report.deletes_skipped,
            "index coverage pass"
        );
    }
    Ok(report)
}

/// Delete every document whose `event_id` term is `bytes`. A text field's
/// terms are UTF-8, so a term the scan read back always converts.
fn delete_event_id_term(writer: &mut IndexWriter<TantivyDocument>, field: Field, bytes: &[u8]) {
    match std::str::from_utf8(bytes) {
        Ok(text) => {
            writer.delete_term(Term::from_field_text(field, text));
        }
        Err(e) => tracing::warn!(error = %e, "event_id term is not UTF-8; not deleted"),
    }
}

/// The set difference between the log and the committed index, as counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexCoverage {
    /// `false` when there is no committed index (`index/meta.json` absent).
    pub present: bool,
    /// Log records with no alive document.
    pub missing: usize,
    /// Indexed `evt_` terms with no log record, malformed terms included.
    pub not_in_log: usize,
    /// Log records with more than one alive document.
    pub duplicated: usize,
}

/// Compare the episodic log with the committed index for `agent_root`.
///
/// Opens the index read-only: no writer and no project lock, so it is safe
/// beside a live daemon. It sees the last commit, not a writer's uncommitted
/// batch.
pub fn assess_index_coverage(agent_root: &AgentRoot) -> Result<IndexCoverage, IndexError> {
    let index_dir = agent_root.dreamd_dir().join(INDEX_DIR_NAME);
    if !index_dir.join("meta.json").exists() {
        return Ok(IndexCoverage::default());
    }
    let index = Index::open_in_dir(&index_dir).map_err(tantivy_to_index)?;
    let event_id = index
        .schema()
        .get_field(EVENT_ID_FIELD)
        .map_err(tantivy_to_index)?;
    let reader: IndexReader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()
        .map_err(tantivy_to_index)?;
    reader.reload().map_err(tantivy_to_index)?;
    let indexed = scan_indexed(&reader, event_id).map_err(tantivy_to_index)?;
    let events = read_jsonl_events(&agent_root.episodic_jsonl())?;
    let diff = diff(&events, &indexed);
    Ok(IndexCoverage {
        present: true,
        missing: diff.missing.len(),
        not_in_log: diff.not_in_log.len() + diff.malformed.len(),
        duplicated: diff.duplicated.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{build_schema, Layer};
    use chrono::DateTime;
    use dreamd_protocol::EventId;
    use tantivy::collector::Count;
    use tantivy::query::{AllQuery, TermQuery};
    use tantivy::{doc, Index};

    struct Fixture {
        _dir: tempfile::TempDir,
        writer: IndexWriter<TantivyDocument>,
        reader: IndexReader,
        fields: SchemaFields,
    }

    /// An on-disk index with the production schema and a Manual reader.
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let (schema, fields) = build_schema();
        let index = Index::create_in_dir(dir.path(), schema).unwrap();
        let writer = index.writer(15_000_000).unwrap();
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .unwrap();
        Fixture {
            _dir: dir,
            writer,
            reader,
            fields,
        }
    }

    /// `evt_` plus a 26-character body that ends in `n` as three digits.
    fn id(n: usize) -> String {
        format!("evt_01ARZ3NDEKTSV4RRFFQ69G5{n:03}")
    }

    fn ev(n: usize) -> AgentLearning {
        AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: EventId::parse(&id(n)).unwrap(),
            timestamp: DateTime::from_timestamp(1_780_000_000, 0).unwrap(),
            pain: 6.0,
            importance: 6.0,
            pinned: false,
            skill_action: "rust::eh".to_string(),
            source_harness: "test-harness".to_string(),
            content: format!("event {n}"),
        }
    }

    fn evs(ns: impl IntoIterator<Item = usize>) -> Vec<AgentLearning> {
        ns.into_iter().map(ev).collect()
    }

    impl Fixture {
        fn index(&mut self, events: &[AgentLearning]) {
            for event in events {
                add_document(&mut self.writer, &self.fields, event).unwrap();
            }
            self.writer.commit().unwrap();
        }

        fn cover(&mut self, log: &[AgentLearning]) -> CoverageReport {
            let report = cover(&mut self.writer, &self.fields, &self.reader, log).unwrap();
            self.writer.commit().unwrap();
            self.reader.reload().unwrap();
            report
        }

        /// Alive documents whose `event_id` is `term`, by a term query.
        fn alive(&self, term: &str) -> usize {
            let query = TermQuery::new(
                Term::from_field_text(self.fields.event_id, term),
                IndexRecordOption::Basic,
            );
            self.reader.searcher().search(&query, &Count).unwrap()
        }

        fn total(&self) -> usize {
            self.reader.searcher().search(&AllQuery, &Count).unwrap()
        }
    }

    #[test]
    fn cover_adds_log_ids_missing_from_the_index() {
        let mut f = fixture();
        f.index(&evs([0]));

        let report = f.cover(&evs([0, 1, 2]));

        assert_eq!(
            report,
            CoverageReport {
                added: 2,
                ..Default::default()
            }
        );
        assert_eq!(
            (f.alive(&id(0)), f.alive(&id(1)), f.alive(&id(2))),
            (1, 1, 1)
        );
    }

    #[test]
    fn cover_deletes_indexed_ids_not_in_the_log() {
        let mut f = fixture();
        f.index(&evs([0, 1, 2]));

        let report = f.cover(&evs([0, 1]));

        assert_eq!(
            report,
            CoverageReport {
                deleted: 1,
                ..Default::default()
            }
        );
        assert_eq!(
            f.alive(&id(2)),
            0,
            "an id the log lacks must leave the index"
        );
        assert_eq!(f.total(), 2);
    }

    #[test]
    fn cover_heals_an_id_with_two_alive_docs() {
        let mut f = fixture();
        f.index(&evs([0]));
        // A second document for the same id, past the upsert.
        let fields = f.fields;
        f.writer
            .add_document(doc!(
                fields.content => "a duplicate the upsert never saw",
                fields.layer => Layer::Episodic.as_str(),
                fields.event_id => id(0),
            ))
            .unwrap();
        f.writer.commit().unwrap();
        f.reader.reload().unwrap();
        assert_eq!(f.alive(&id(0)), 2, "precondition: two alive docs");

        let report = f.cover(&evs([0]));

        assert_eq!(
            report,
            CoverageReport {
                healed: 1,
                ..Default::default()
            }
        );
        assert_eq!(f.alive(&id(0)), 1);
    }

    /// The upsert inside one commit leaves a deleted posting for the id in that
    /// segment. Only alive postings count.
    #[test]
    fn cover_adds_nothing_when_the_index_matches_the_log() {
        let mut f = fixture();
        let log = evs([0, 1]);
        add_document(&mut f.writer, &f.fields, &log[0]).unwrap();
        add_document(&mut f.writer, &f.fields, &log[0]).unwrap();
        add_document(&mut f.writer, &f.fields, &log[1]).unwrap();
        f.writer.commit().unwrap();

        let report = f.cover(&log);

        assert_eq!(report, CoverageReport::default());
        assert_eq!((f.alive(&id(0)), f.alive(&id(1))), (1, 1));
    }

    #[test]
    fn cover_leaves_lesson_docs_alone() {
        let mut f = fixture();
        let log = evs([0]);
        f.index(&log);
        let lesson = format!("lsn_{}", id(0));
        let fields = f.fields;
        f.writer
            .add_document(doc!(
                fields.content => "a lesson",
                fields.layer => Layer::Semantic.as_str(),
                fields.event_id => lesson.clone(),
            ))
            .unwrap();
        f.writer.commit().unwrap();

        let report = f.cover(&log);

        assert_eq!(report, CoverageReport::default());
        assert_eq!(f.alive(&lesson), 1, "a lesson doc is not an episodic id");
    }

    #[test]
    fn cover_keeps_docs_when_the_log_is_empty() {
        let mut f = fixture();
        f.index(&evs([0, 1, 2]));

        let report = f.cover(&[]);

        assert_eq!(
            report,
            CoverageReport {
                deletes_skipped: 3,
                ..Default::default()
            }
        );
        assert_eq!(f.total(), 3, "an empty log must not empty the index");
    }

    #[test]
    fn cover_keeps_docs_when_most_of_the_index_would_go() {
        let mut f = fixture();
        f.index(&evs(0..100));

        let report = f.cover(&evs(0..10));

        assert_eq!(
            report,
            CoverageReport {
                deletes_skipped: 90,
                ..Default::default()
            }
        );
        assert_eq!(f.total(), 100, "90 of 100 ids is a mass delete");
    }

    #[test]
    fn assess_index_coverage_is_absent_without_meta_json() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        let index_dir = root.dreamd_dir().join(INDEX_DIR_NAME);
        std::fs::create_dir_all(&index_dir).unwrap();
        std::fs::write(index_dir.join("junk.seg"), b"not an index").unwrap();

        let coverage = assess_index_coverage(&root).unwrap();

        assert_eq!(coverage, IndexCoverage::default());
        assert!(!coverage.present);
    }
}
