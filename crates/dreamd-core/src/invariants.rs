//! A read-only statement of what a healthy `.agent/` store looks like.
//!
//! [`check_store`] is the oracle tests run after every step that touches a
//! store: an append, a dream cycle, a recovery, an index open. It reads the
//! episodic log, `LESSONS.md`, `state.json`, the WAL's presence, and (when
//! given) a committed index reader, and reports every way they disagree:
//! duplicate ids, the index's coverage of the log, the lesson documents, each
//! document's recall recurrence, a stuck cycle and unpinned citations.
//!
//! It opens no writer, writes nothing, and reads no clock, so calling it can
//! never change the answer to the next call.

use std::collections::{BTreeMap, BTreeSet};

use tantivy::collector::DocSetCollector;
use tantivy::query::AllQuery;
use tantivy::schema::{TantivyDocument, Value};
use tantivy::TantivyError;

use dreamd_protocol::AgentLearning;

use crate::daemon_state::read_daemon_state;
use crate::index::{build_schema, Layer};
use crate::layout::AgentRoot;
use crate::lessons::{read_lessons_file, LessonsFile};
use crate::recurrence::{ClusterCounts, RecallReader};
use crate::server::indexer_actor::semantic_event_id;

/// One way the store disagrees with itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// The episodic log holds `count` records with the same `id`.
    DuplicateEventId { id: String, count: usize },
    /// A log record has no `layer = "episodic"` document in the index.
    MissingFromIndex { id: String },
    /// An episodic document in the index has no record in the log.
    NotInLog { id: String },
    /// The index holds `count` episodic documents with the same `event_id`.
    DuplicateIndexDoc { id: String, count: usize },
    /// A lesson whose exemplar is in the log has no semantic document. `id`
    /// is the document id (`lsn_<lesson id>`).
    MissingLessonDoc { id: String },
    /// A semantic document matches no indexable lesson in `LESSONS.md`. `id`
    /// is the document id (`lsn_<lesson id>`).
    StaleLessonDoc { id: String },
    /// `state.json` says a dream cycle is in progress, but there is no WAL
    /// for recovery to act on.
    StuckInProgress,
    /// A lesson id or citation names a log record that is not pinned, so
    /// decay may remove the lesson's evidence.
    UnpinnedCitation { id: String },
    /// `LESSONS.md` exists but could not be read or parsed.
    LessonsUnreadable { reason: String },
    /// A document's recall recurrence differs from the log's subtree count for
    /// its `skill_action`. `id` is the document id (`evt_…` or `lsn_…`).
    RecurrenceMismatch {
        id: String,
        skill_action: String,
        index: u32,
        log: u32,
    },
}

impl Violation {
    /// Legitimate states that still deserve a look.
    pub fn is_warning(&self) -> bool {
        matches!(
            self,
            Self::UnpinnedCitation { .. } | Self::LessonsUnreadable { .. }
        )
    }
}

/// Why [`check_store`] could not finish. A store it can read but that breaks
/// an invariant is a [`Violation`], not an error.
#[derive(Debug, thiserror::Error)]
pub enum InvariantError {
    /// The episodic log could not be read.
    #[error(transparent)]
    Episodic(#[from] crate::episodic::EpisodicError),
    /// `state.json` could not be read or parsed.
    #[error(transparent)]
    State(#[from] crate::daemon_state::DaemonStateError),
    /// Searching the index failed.
    #[error(transparent)]
    Index(#[from] tantivy::TantivyError),
}

/// Check `agent_root` against the invariants of a healthy store.
///
/// Checks run in a fixed order — log, index, semantic layer, recurrence,
/// an unreadable `LESSONS.md`, state, citations — and each emits its ids
/// sorted, so equal stores give equal output.
///
/// The index, semantic and recurrence checks run only when `index` is `Some`.
/// The caller owns that reader's freshness: it must see a committed, reloaded
/// view. Appends still sitting in the indexer's uncommitted batch show up as
/// [`Violation::MissingFromIndex`].
///
/// The recurrence check runs only when the index covers the log exactly: no
/// duplicate log id, no missing, extra or duplicated index document. Each of
/// those already names the fault, and every count in its cluster would
/// disagree too.
///
/// An error means the log, `state.json` or the index could not be read. A
/// `LESSONS.md` that exists but cannot be read or parsed is not an error: it
/// is one [`Violation::LessonsUnreadable`], with or without an index, and the
/// semantic and citation checks skip it.
pub fn check_store(
    agent_root: &AgentRoot,
    index: Option<&RecallReader>,
) -> Result<Vec<Violation>, InvariantError> {
    let mut out = Vec::new();

    // Log.
    let events = crate::episodic::read_all(&agent_root.episodic_jsonl())?;
    let mut log_counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut unpinned: BTreeSet<&str> = BTreeSet::new();
    for ev in &events {
        *log_counts.entry(ev.id.as_str()).or_insert(0) += 1;
        if !ev.pinned {
            unpinned.insert(ev.id.as_str());
        }
    }
    out.extend(duplicate_event_ids(&events));

    // Read once; the semantic and citation checks both use it.
    let lessons = match read_lessons_file(&agent_root.lessons_md()) {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    };

    if let Some(reader) = index {
        let IndexDocs {
            episodic: episodic_docs,
            semantic: semantic_docs,
            mut recurrence,
        } = read_index_docs(reader)?;

        // Index.
        for (id, &count) in &episodic_docs {
            if count > 1 {
                out.push(Violation::DuplicateIndexDoc {
                    id: id.clone(),
                    count,
                });
            }
        }
        for id in log_counts.keys() {
            if !episodic_docs.contains_key(*id) {
                out.push(Violation::MissingFromIndex {
                    id: (*id).to_owned(),
                });
            }
        }
        for id in episodic_docs.keys() {
            if !log_counts.contains_key(id.as_str()) {
                out.push(Violation::NotInLog { id: id.clone() });
            }
        }

        // Semantic. The indexer skips a lesson whose exemplar is not in the
        // log, so only those lessons are expected to have a document.
        if let Ok(file) = &lessons {
            let expected: BTreeSet<String> = file
                .iter()
                .flat_map(|f| &f.lessons)
                .filter(|l| log_counts.contains_key(l.id.as_str()))
                .map(|l| semantic_event_id(&l.id))
                .collect();
            for id in expected.difference(&semantic_docs) {
                out.push(Violation::MissingLessonDoc { id: id.clone() });
            }
            for id in semantic_docs.difference(&expected) {
                out.push(Violation::StaleLessonDoc { id: id.clone() });
            }
        }

        // Recurrence, only over an index that covers the log exactly.
        let covered = !out.iter().any(|v| {
            matches!(
                v,
                Violation::DuplicateEventId { .. }
                    | Violation::MissingFromIndex { .. }
                    | Violation::NotInLog { .. }
                    | Violation::DuplicateIndexDoc { .. }
            )
        });
        if covered {
            let counts = ClusterCounts::from_events(&events);
            recurrence.sort_by(|a, b| a.id.cmp(&b.id));
            for doc in recurrence {
                let log = counts.subtree(&doc.skill_action);
                if doc.recurrence != log {
                    out.push(Violation::RecurrenceMismatch {
                        id: doc.id,
                        skill_action: doc.skill_action,
                        index: doc.recurrence,
                        log,
                    });
                }
            }
        }
    }

    // Reported with or without an index: it is also why the citation check
    // below has nothing to say.
    if let Err(reason) = &lessons {
        out.push(Violation::LessonsUnreadable {
            reason: reason.clone(),
        });
    }

    // State.
    let state = read_daemon_state(agent_root)?;
    if state.last_dream_cycle_status == "in_progress" && !agent_root.wal_path().exists() {
        out.push(Violation::StuckInProgress);
    }

    // Citations.
    if let Ok(Some(file)) = &lessons {
        for id in cited_ids(file) {
            if unpinned.contains(id) {
                out.push(Violation::UnpinnedCitation { id: id.to_owned() });
            }
        }
    }

    Ok(out)
}

/// One [`Violation::DuplicateEventId`] for each id that more than one record
/// in `events` carries, sorted by id. The one duplicate detector: [`check_store`]
/// and `dreamd doctor` both call it (BZR-1302).
pub fn duplicate_event_ids(events: &[AgentLearning]) -> Vec<Violation> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for ev in events {
        *counts.entry(ev.id.as_str()).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .filter(|&(_, count)| count > 1)
        .map(|(id, count)| Violation::DuplicateEventId {
            id: id.to_owned(),
            count,
        })
        .collect()
}

/// What [`read_index_docs`] reads from one searcher.
struct IndexDocs {
    /// Episodic ids, with a count so duplicates are visible.
    episodic: BTreeMap<String, usize>,
    /// Semantic ids.
    semantic: BTreeSet<String>,
    /// Every alive document's recall recurrence, from the same searcher.
    recurrence: Vec<DocRecurrence>,
}

struct DocRecurrence {
    id: String,
    skill_action: String,
    recurrence: u32,
}

/// Every alive document's stored `event_id`, split by `layer`, and its recall
/// recurrence from the table of the searcher the ids were read from.
fn read_index_docs(reader: &RecallReader) -> Result<IndexDocs, TantivyError> {
    let (_, fields) = build_schema();
    let searcher = reader.searcher();
    let tables = reader.recurrence(&searcher)?;
    let mut docs = IndexDocs {
        episodic: BTreeMap::new(),
        semantic: BTreeSet::new(),
        recurrence: Vec::new(),
    };
    for addr in searcher.search(&AllQuery, &DocSetCollector)? {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let text = |field| {
            doc.get_first(field)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned()
        };
        let layer = doc.get_first(fields.layer).and_then(|v| v.as_str());
        // A document with no stored id keeps the empty string, the same
        // fallback recall uses, so it still surfaces as `NotInLog`.
        let id = text(fields.event_id);
        let segment = searcher.segment_reader(addr.segment_ord).segment_id();
        let table = tables.segment(segment).ok_or_else(|| {
            TantivyError::InternalError(format!(
                "segment {} has no recurrence table",
                segment.uuid_string()
            ))
        })?;
        docs.recurrence.push(DocRecurrence {
            id: id.clone(),
            skill_action: text(fields.skill_action),
            recurrence: table.get(addr.doc_id),
        });
        if layer == Some(Layer::Episodic.as_str()) {
            *docs.episodic.entry(id).or_insert(0) += 1;
        } else if layer == Some(Layer::Semantic.as_str()) {
            docs.semantic.insert(id);
        }
    }
    Ok(docs)
}

/// `lessons[].id` ∪ `citations`, sorted. This is the set the dream cycle pins.
fn cited_ids(file: &LessonsFile) -> BTreeSet<&str> {
    file.lessons
        .iter()
        .map(|l| l.id.as_str())
        .chain(file.citations.iter().map(String::as_str))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lessons::{render_lessons_file, Lesson};
    use crate::test_support::{unique_tmpdir, DirGuard};
    use chrono::DateTime;
    use dreamd_protocol::{AgentLearning, EventId};
    use std::fs;
    use std::io::Write;
    use tantivy::doc;

    const ULID_BASE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA";
    const NOW_SEC: i64 = 1_780_000_000;

    fn id(suffix: char) -> String {
        format!("evt_{ULID_BASE}{suffix}")
    }

    fn learning(id: &str, content: &str, pinned: bool) -> AgentLearning {
        AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: EventId::parse(id).expect("synthesize EventId"),
            timestamp: DateTime::from_timestamp(NOW_SEC, 0).expect("valid timestamp"),
            pain: 6.0,
            importance: 6.0,
            pinned,
            skill_action: "rust::eh".to_string(),
            source_harness: "claude-code".to_string(),
            content: content.to_string(),
        }
    }

    fn line(ev: &AgentLearning) -> String {
        let mut s = serde_json::to_string(ev).expect("serialize learning");
        s.push('\n');
        s
    }

    fn scaffold(label: &str, events: &[AgentLearning]) -> (AgentRoot, DirGuard) {
        let dir = unique_tmpdir(label);
        let root = AgentRoot::new(&dir);
        fs::create_dir_all(root.episodic_dir()).unwrap();
        fs::create_dir_all(root.semantic_dir()).unwrap();
        fs::create_dir_all(root.dreamd_dir()).unwrap();
        let body: String = events.iter().map(line).collect();
        fs::write(root.episodic_jsonl(), body).unwrap();
        (root, DirGuard(dir))
    }

    fn write_lessons(root: &AgentRoot, lesson_id: &str, citations: Vec<String>) {
        let file = LessonsFile {
            last_updated: DateTime::from_timestamp(NOW_SEC, 0).expect("valid timestamp"),
            prompt_version: "dream-cycle/v1.1@2026-08-23".to_string(),
            cluster_key: "rust::eh".to_string(),
            citations,
            lessons: vec![Lesson {
                id: lesson_id.to_string(),
                content: "Propagate with ? outside tests.".to_string(),
                pinned: false,
            }],
        };
        fs::write(root.lessons_md(), render_lessons_file(&file)).unwrap();
    }

    /// The handle's indexer task keeps the token, so the project stays locked
    /// until the handle is shut down.
    #[cfg(unix)]
    fn open(root: &AgentRoot) -> crate::server::TantivyIndexHandle {
        use crate::writer_lock::{WriterKind, WriterLock};
        let lock = WriterLock::acquire(root, WriterKind::Watch).expect("project writer lock");
        crate::server::TantivyIndexHandle::open(root, &lock, crate::server::DEFAULT_COMMIT_CADENCE)
            .expect("open index handle")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn freshly_opened_store_is_clean() {
        let (root, _guard) = scaffold(
            "inv-clean",
            &[
                learning(&id('0'), "first", false),
                learning(&id('1'), "second", false),
            ],
        );
        let h = open(&root);

        let violations = check_store(&root, Some(h.reader())).unwrap();

        assert_eq!(violations, vec![]);
        h.shutdown().await.unwrap();
    }

    #[test]
    fn duplicate_log_id_is_reported_once_with_its_count() {
        let dup = id('0');
        let (root, _guard) = scaffold(
            "inv-dup",
            &[
                learning(&dup, "first", false),
                learning(&dup, "again", false),
            ],
        );

        let violations = check_store(&root, None).unwrap();

        assert_eq!(
            violations,
            vec![Violation::DuplicateEventId { id: dup, count: 2 }]
        );
    }

    /// BZR-1302 test 11: one violation per repeated id, sorted by id, with
    /// its count; an id seen once is not reported.
    #[test]
    fn duplicate_event_ids_reports_each_repeated_id_sorted() {
        let (a, b, c) = (id('A'), id('B'), id('C'));
        let events = [
            learning(&a, "a1", false),
            learning(&b, "b1", false),
            learning(&a, "a2", false),
            learning(&c, "c1", false),
            learning(&c, "c2", false),
            learning(&c, "c3", false),
        ];

        assert_eq!(
            duplicate_event_ids(&events),
            vec![
                Violation::DuplicateEventId { id: a, count: 2 },
                Violation::DuplicateEventId { id: c, count: 3 },
            ]
        );
        assert_eq!(duplicate_event_ids(&[]), vec![]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn line_appended_after_open_is_missing_from_index() {
        let (root, _guard) = scaffold(
            "inv-missing",
            &[
                learning(&id('0'), "first", false),
                learning(&id('1'), "second", false),
            ],
        );
        let h = open(&root);
        let late = id('2');
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(root.episodic_jsonl())
            .unwrap();
        file.write_all(line(&learning(&late, "third", false)).as_bytes())
            .unwrap();

        let violations = check_store(&root, Some(h.reader())).unwrap();

        assert_eq!(violations, vec![Violation::MissingFromIndex { id: late }]);
        h.shutdown().await.unwrap();
    }

    #[test]
    fn in_progress_without_a_wal_is_stuck() {
        let (root, _guard) = scaffold("inv-stuck", &[]);
        fs::write(
            root.state_json(),
            r#"{"schema_version":"1.0","last_dream_cycle_status":"in_progress"}"#,
        )
        .unwrap();

        assert_eq!(
            check_store(&root, None).unwrap(),
            vec![Violation::StuckInProgress]
        );

        fs::write(root.wal_path(), b"").unwrap();

        assert_eq!(check_store(&root, None).unwrap(), vec![]);
    }

    #[test]
    fn unpinned_cited_event_is_a_warning() {
        let exemplar = id('0');
        let pinned_citation = id('1');
        let (root, _guard) = scaffold(
            "inv-unpinned",
            &[
                learning(&exemplar, "first", false),
                learning(&pinned_citation, "second", true),
            ],
        );
        write_lessons(
            &root,
            &exemplar,
            vec![exemplar.clone(), pinned_citation.clone()],
        );

        let violations = check_store(&root, None).unwrap();

        assert_eq!(
            violations,
            vec![Violation::UnpinnedCitation { id: exemplar }]
        );
        assert!(violations[0].is_warning());
    }

    #[test]
    fn unreadable_lessons_is_reported_without_an_index() {
        let (root, _guard) = scaffold("inv-unreadable", &[learning(&id('0'), "first", false)]);
        fs::write(root.lessons_md(), "not frontmatter\n").unwrap();

        let violations = check_store(&root, None).unwrap();

        assert!(
            matches!(violations.as_slice(), [Violation::LessonsUnreadable { .. }]),
            "expected exactly one LessonsUnreadable; got {violations:?}"
        );
        assert!(violations[0].is_warning());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn no_reader_skips_index_and_semantic_checks() {
        let exemplar = id('0');
        let (root, _guard) = scaffold("inv-noreader", &[learning(&exemplar, "first", true)]);
        let h = open(&root);
        // Both land after the open: the reader has neither the event nor the
        // lesson document.
        let late = id('1');
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(root.episodic_jsonl())
            .unwrap();
        file.write_all(line(&learning(&late, "second", true)).as_bytes())
            .unwrap();
        write_lessons(&root, &exemplar, vec![exemplar.clone()]);

        assert_eq!(
            check_store(&root, Some(h.reader())).unwrap(),
            vec![
                Violation::MissingFromIndex { id: late },
                Violation::MissingLessonDoc {
                    id: semantic_event_id(&exemplar)
                },
            ],
            "precondition: the index is stale"
        );
        assert_eq!(check_store(&root, None).unwrap(), vec![]);
        h.shutdown().await.unwrap();
    }

    /// A raw in-RAM index of `(event_id, skill_action, layer)` docs, committed
    /// and read through a reloaded `RecallReader`.
    fn raw_reader(docs: &[(&str, &str, Layer)]) -> RecallReader {
        let (schema, fields) = build_schema();
        let index = tantivy::Index::create_in_ram(schema);
        let mut writer = index.writer(15_000_000).unwrap();
        for &(doc_id, skill_action, layer) in docs {
            writer
                .add_document(doc!(
                    fields.layer => layer.as_str(),
                    fields.event_id => doc_id,
                    fields.skill_action => skill_action,
                ))
                .unwrap();
        }
        writer.commit().unwrap();
        let reader = RecallReader::open(&index, tantivy::ReloadPolicy::Manual).unwrap();
        reader.reload().unwrap();
        reader
    }

    fn mismatch(doc_id: &str, skill_action: &str, index: u32, log: u32) -> Violation {
        Violation::RecurrenceMismatch {
            id: doc_id.to_owned(),
            skill_action: skill_action.to_owned(),
            index,
            log,
        }
    }

    #[test]
    fn recurrence_mismatch_names_each_doc_whose_count_differs() {
        let (a, b, c) = (id('0'), id('1'), id('2'));
        let (root, _guard) = scaffold(
            "inv-recurrence",
            &[
                learning(&a, "first", false),
                learning(&b, "second", false),
                learning(&c, "third", false),
            ],
        );
        // The ids match the log, so no coverage violation fires. C's key does
        // not.
        let reader = raw_reader(&[
            (&a, "rust::eh", Layer::Episodic),
            (&b, "rust::eh", Layer::Episodic),
            (&c, "go::x", Layer::Episodic),
        ]);

        assert_eq!(
            check_store(&root, Some(&reader)).unwrap(),
            vec![
                mismatch(&a, "rust::eh", 2, 3),
                mismatch(&b, "rust::eh", 2, 3),
                mismatch(&c, "go::x", 1, 0),
            ]
        );
    }

    #[test]
    fn a_lesson_doc_whose_count_differs_is_a_mismatch() {
        let (a, b, c) = (id('0'), id('1'), id('2'));
        let (root, _guard) = scaffold(
            "inv-recurrence-lesson",
            &[
                learning(&a, "first", true),
                learning(&b, "second", false),
                learning(&c, "third", false),
            ],
        );
        write_lessons(&root, &a, vec![]);
        let lesson = semantic_event_id(&a);
        let reader = raw_reader(&[
            (&a, "rust::eh", Layer::Episodic),
            (&b, "rust::eh", Layer::Episodic),
            (&c, "go::x", Layer::Episodic),
            (&lesson, "rust::eh", Layer::Semantic),
        ]);

        assert_eq!(
            check_store(&root, Some(&reader)).unwrap(),
            vec![
                mismatch(&a, "rust::eh", 2, 3),
                mismatch(&b, "rust::eh", 2, 3),
                mismatch(&c, "go::x", 1, 0),
                mismatch(&lesson, "rust::eh", 2, 3),
            ]
        );
    }

    #[test]
    fn a_duplicate_log_id_skips_the_recurrence_check() {
        let (a, b) = (id('0'), id('1'));
        let (root, _guard) = scaffold(
            "inv-recurrence-dup-log",
            &[
                learning(&a, "first", false),
                learning(&a, "again", false),
                learning(&b, "second", false),
            ],
        );
        let reader = raw_reader(&[
            (&a, "rust::eh", Layer::Episodic),
            (&b, "rust::eh", Layer::Episodic),
        ]);

        assert_eq!(
            check_store(&root, Some(&reader)).unwrap(),
            vec![Violation::DuplicateEventId { id: a, count: 2 }]
        );
    }

    #[test]
    fn a_doc_not_in_the_log_skips_the_recurrence_check() {
        let (a, ghost) = (id('0'), id('9'));
        let (root, _guard) = scaffold("inv-recurrence-ghost", &[learning(&a, "first", false)]);
        let reader = raw_reader(&[
            (&a, "rust::eh", Layer::Episodic),
            (&ghost, "rust::eh", Layer::Episodic),
        ]);

        assert_eq!(
            check_store(&root, Some(&reader)).unwrap(),
            vec![Violation::NotInLog { id: ghost }]
        );
    }

    #[test]
    fn a_duplicated_index_doc_skips_the_recurrence_check() {
        let (a, b) = (id('0'), id('1'));
        let (root, _guard) = scaffold(
            "inv-recurrence-dup-index",
            &[learning(&a, "first", false), learning(&b, "second", false)],
        );
        let reader = raw_reader(&[
            (&a, "rust::eh", Layer::Episodic),
            (&a, "rust::eh", Layer::Episodic),
            (&b, "rust::eh", Layer::Episodic),
        ]);

        assert_eq!(
            check_store(&root, Some(&reader)).unwrap(),
            vec![Violation::DuplicateIndexDoc { id: a, count: 2 }]
        );
    }
}
