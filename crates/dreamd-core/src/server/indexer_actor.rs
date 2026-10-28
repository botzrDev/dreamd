//! The per-project indexer actor (WEG-42 / DR-202), split out of
//! `tantivy_handle` by BZR-170.
//!
//! One tokio task owns the Tantivy `IndexWriter`. The coordinator and the
//! dream cycle talk to it only through [`IndexerMsg`]. Everything that mutates
//! the writer lives here: episodic `add_document` (an upsert), the cadence and
//! [`IndexerMsg::Flush`] commits, the coverage pass on [`IndexerMsg::Resync`],
//! the decay prune, and the semantic (LESSONS.md) pass that
//! `TantivyIndexHandle::open` also runs. Every commit goes through
//! [`IndexerPersist::commit`] (BZR-1236). Recall's recurrence is not written
//! here: it is derived per searcher (`crate::recurrence`, BZR-1242).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dreamd_protocol::{AgentLearning, EventId};
use serde::{Deserialize, Serialize};
use tantivy::{doc, IndexReader, IndexWriter, TantivyDocument};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

#[cfg(unix)]
use crate::coordinator::LogIdentity;
use crate::index::{Layer, SchemaFields};
use crate::io::write_atomic;
use crate::layout::AgentRoot;
use crate::provenance::{Kind, Ledger};
use crate::server::index_coverage::{cover, CoverageReport};
use crate::server::index_freshness::{
    read_jsonl_events, write_progress, CommitMark, IndexProgress,
};
use crate::server::index_map::{tantivy_to_index, IndexError};

/// Default mpsc capacity for the coordinator → indexer hand-off. Sized so a
/// 5-second commit window plus replay headroom fits without blocking the
/// coordinator. The coordinator **awaits** `send` on this channel: once the
/// buffer fills, the append handler blocks and the actor stops reading its own
/// inbox, so in-flight learns queue in the 256-slot coordinator channel and
/// simply wait — nothing times out a queued request, so a brief park surfaces as
/// latency. Only once that inbox is also full does `Supervisor::try_send`'s
/// 100 ms `COORDINATOR_SEND_TIMEOUT` start returning HTTP 503, and that is the
/// HTTP ingress alone: the in-process `dreamd mcp` path sends on the coordinator
/// channel without that timeout, so it waits rather than 503ing.
///
/// Nothing is dropped. A shed `IndexerMsg::Append` would stay out of recall
/// until the next coverage pass (the next open, or a resync), so backpressure
/// is still what keeps live recall whole.
pub(crate) const DEFAULT_INDEXER_CHANNEL_CAPACITY: usize = 1024;

/// Relative filename for the semantic pass's report of lessons it could not
/// index, joined under the project's `.dreamd/` directory. Written by
/// [`index_semantic_lessons`], read by `dreamd doctor` (AILAB-700).
pub const SEMANTIC_PASS_FILENAME: &str = "semantic_pass.json";

/// What the last semantic (LESSONS.md) pass could not index (AILAB-700).
///
/// Lives at `<agent_root>/.agent/.dreamd/semantic_pass.json`. It exists so
/// `dreamd doctor` can report un-indexable lessons without opening the index
/// and without re-resolving LESSONS.md against the episodic log — a second
/// parser for the same rule is exactly the drift this avoids.
///
/// Deliberately carries **no timestamp**: `dream` reads wall-clock, which would
/// make the file non-deterministic for no operator benefit. Doctor reports the
/// current state of the store, not when that state was measured.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticPassRecord {
    /// `lesson.id`s dropped because the exemplar was not in the episodic log.
    pub skipped_lesson_ids: Vec<String>,
    /// The clustering key of the LESSONS.md those lessons came from.
    pub cluster_key: String,
    /// Lessons successfully added as `layer=semantic` documents.
    pub indexed: usize,
}

/// Read the semantic pass report for `agent_root` without opening Tantivy.
///
/// `Ok(None)` when the file is absent — a store that has never dreamed has no
/// report, which is a fact and not a fault. Same shape as
/// [`assess_index_freshness`](crate::server::index_freshness::assess_index_freshness): `dreamd doctor` is most valuable when the daemon
/// is down, so this path opens no index and takes no lock.
pub fn read_semantic_pass_record(
    agent_root: &AgentRoot,
) -> Result<Option<SemanticPassRecord>, IndexError> {
    let path = agent_root.dreamd_dir().join(SEMANTIC_PASS_FILENAME);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| IndexError::Other(format!("parse {SEMANTIC_PASS_FILENAME}: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(IndexError::Other(format!(
            "read {SEMANTIC_PASS_FILENAME}: {e}"
        ))),
    }
}

/// Messages accepted by the indexer task.
///
/// `#[non_exhaustive]` keeps the enum forward-compatible — additional
/// variants (e.g., `Delete`, `Rewrite`) will land in later tickets without
/// breaking exhaustive matches in callers.
#[non_exhaustive]
pub enum IndexerMsg {
    /// Coordinator → indexer hand-off after a durable JSONL append.
    ///
    /// The doc's `recurrence` field is written as 0. Recall's recurrence is
    /// derived per searcher from the docs it holds (`crate::recurrence`), so
    /// an append raises the count of every doc in its key's subtree at the
    /// next commit and reload.
    Append {
        event_id: EventId,
        learning: AgentLearning,
    },
    /// Drives deterministic flush in tests; production commits run on the
    /// cadence ticker. The `ack` oneshot resolves with `Ok(())` after a
    /// successful Tantivy commit + progress-file update, or `Err(IndexError)`
    /// if either step failed.
    Flush {
        ack: oneshot::Sender<Result<(), IndexError>>,
    },
    /// Decay pruner hook (WEG-62 / DR-309): delete decayed event IDs from the index.
    /// Does not touch the JSONL — the JSONL rewrite is the cycle's plan
    /// (`plan_cycle`), committed with the cycle.
    PruneDecayedEvents {
        event_ids: Vec<EventId>,
        response: oneshot::Sender<Result<(), IndexError>>,
    },
    /// Dream-cycle hook (DR-211 / AILAB-205): re-read `semantic/LESSONS.md`,
    /// replace every `layer=semantic` document with the file's current lesson
    /// set, then commit. Lets a running daemon pick up the lessons consolidation
    /// just wrote without waiting for a restart. Never touches episodic
    /// documents or the episodic watermark. Resolves after the commit completes.
    IndexSemanticLessons {
        agent_root: AgentRoot,
        response: oneshot::Sender<Result<(), IndexError>>,
    },
    /// The log may hold ids the index has not seen, or lack ids it has: the
    /// coordinator found it replaced or edited outside itself (BZR-1236).
    /// Commits pending work, runs the coverage pass against the log, and
    /// commits again. The coordinator sends `ack: None`; tests pass `Some`.
    Resync {
        ack: Option<oneshot::Sender<Result<CoverageReport, IndexError>>>,
    },
}

/// Owning handle for the spawned indexer task. Constructed inside
/// [`TantivyIndexHandle::open`](crate::server::tantivy_handle::TantivyIndexHandle::open) and held privately. Dropped (and task
/// aborted or drained) when [`TantivyIndexHandle`](crate::server::tantivy_handle::TantivyIndexHandle) is closed or shut down.
pub(crate) struct IndexerHandle {
    pub(crate) tx: mpsc::Sender<IndexerMsg>,
    pub(crate) join: JoinHandle<()>,
}

impl IndexerHandle {
    pub(crate) fn sender(&self) -> mpsc::Sender<IndexerMsg> {
        self.tx.clone()
    }
}

/// The one persist step every Tantivy commit in production code goes through.
///
/// [`Self::commit`] commits, then appends an `index_doc` edge for every id in
/// `pending`, then writes the watermark. That order means a watermark never
/// covers an id with no edge, and a crash between the commit and the edges
/// costs nothing: the next coverage pass puts every log id back in `pending`.
///
/// A successful persist publishes the [`CommitMark`] that `/health` reads
/// (BZR-1243); a failed one publishes nothing.
pub(crate) struct IndexerPersist {
    progress_path: PathBuf,
    ledger: Ledger,
    /// Episodic ids upserted or covered since the last successful persist.
    pending: Vec<EventId>,
    /// The largest committed episodic id.
    watermark: Option<EventId>,
    /// What `index_progress.json` holds, so an unchanged watermark is not rewritten.
    written: Option<EventId>,
    /// Shared with the handle.
    mark: Arc<Mutex<CommitMark>>,
    /// Every `IndexerMsg::Append` taken off the channel, including one whose
    /// `add_document` failed.
    received: u64,
    /// `Append`s added to the writer since the last successful persist.
    pushed: u64,
    /// A coverage pass ran since the last successful persist.
    covered: bool,
}

impl IndexerPersist {
    /// Seed from what `index_progress.json` holds now. `mark` is published to
    /// at every successful persist.
    pub(crate) fn new(
        progress_path: PathBuf,
        ledger: Ledger,
        progress: &IndexProgress,
        mark: Arc<Mutex<CommitMark>>,
    ) -> Self {
        let written = progress
            .last_indexed_id
            .as_deref()
            .and_then(|id| EventId::parse(id).ok());
        Self {
            progress_path,
            ledger,
            pending: Vec::new(),
            watermark: written.clone(),
            written,
            mark,
            received: 0,
            pushed: 0,
            covered: false,
        }
    }

    /// One `IndexerMsg::Append` taken off the channel, before its
    /// `add_document`.
    pub(crate) fn receive(&mut self) {
        self.received += 1;
    }

    /// One upserted id, recorded at the next commit.
    pub(crate) fn push(&mut self, id: EventId) {
        self.pending.push(id);
        self.pushed += 1;
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// A coverage pass covers the whole log: every log id goes into `pending`,
    /// and the watermark restarts from nothing, so the next commit backfills
    /// any missing edge and sets the watermark to the log's largest id, even
    /// when that is lower than before.
    pub(crate) fn cover_all(&mut self, events: &[AgentLearning]) {
        self.pending = events.iter().map(|ev| ev.id.clone()).collect();
        self.watermark = None;
        self.covered = true;
    }

    /// The one persist step: commit, then `index_doc` edges for `pending`, then
    /// the watermark, then the [`CommitMark`]. Every Tantivy commit in
    /// production code is this call.
    ///
    /// On a ledger failure `pending` stays intact, the watermark does not move
    /// and the mark is not published, so the next commit retries the edges. The
    /// ledger skips an edge it already holds.
    ///
    /// The mark's `committed_appends` grows by the `Append`s pushed since the
    /// last persist. After a coverage pass it becomes `received`: the pass
    /// indexed every log id, and the coordinator writes each line before it
    /// sends the `Append`, so every `Append` received so far is covered.
    pub(crate) fn commit(
        &mut self,
        writer: &mut IndexWriter<TantivyDocument>,
    ) -> Result<(), IndexError> {
        writer.commit().map_err(tantivy_to_index)?;
        #[cfg(test)]
        if self
            .progress_path
            .with_file_name("failpoint-after-commit")
            .exists()
        {
            return Err(IndexError::Other("failpoint-after-commit".to_string()));
        }
        let edges: Vec<(Kind, String, String)> = self
            .pending
            .iter()
            .map(|id| {
                (
                    Kind::IndexDoc,
                    id.as_str().to_owned(),
                    id.as_str().to_owned(),
                )
            })
            .collect();
        self.ledger
            .append(&edges)
            .map_err(|e| IndexError::Io(e.to_string()))?;
        let largest = self.pending.drain(..).max();
        self.watermark = self.watermark.take().max(largest);
        if self.watermark != self.written {
            write_progress(
                &self.progress_path,
                &IndexProgress {
                    last_indexed_id: self.watermark.as_ref().map(|id| id.as_str().to_owned()),
                },
            )?;
            self.written = self.watermark.clone();
        }
        {
            let mut mark = self.mark.lock().expect("commit mark poisoned");
            mark.last_indexed_id = self.watermark.clone();
            mark.committed_appends = if self.covered {
                self.received
            } else {
                mark.committed_appends + self.pushed
            };
        }
        self.pushed = 0;
        self.covered = false;
        Ok(())
    }
}

/// Everything the indexer task owns besides the writer, the schema fields, its
/// inbox and the cadence. One argument, so [`run_indexer`] stays within
/// clippy's argument limit.
pub(crate) struct IndexerState {
    pub(crate) persist: IndexerPersist,
    /// Manual-reload reader over the writer's index, for the coverage pass.
    pub(crate) coverage: IndexReader,
    pub(crate) agent_root: AgentRoot,
    /// The log's device, inode and length as of the last tick or coverage pass.
    #[cfg(unix)]
    pub(crate) log: Option<LogIdentity>,
}

/// The indexer task: owns the `IndexWriter`, upserts appends, commits on the
/// cadence tick or on [`IndexerMsg::Flush`], runs the coverage pass on
/// [`IndexerMsg::Resync`] or when the tick finds the log replaced or shrunk,
/// and exits after a final commit when every sender is dropped.
pub(crate) async fn run_indexer(
    mut writer: IndexWriter<TantivyDocument>,
    fields: SchemaFields,
    mut rx: mpsc::Receiver<IndexerMsg>,
    commit_cadence: Duration,
    mut state: IndexerState,
) {
    let mut interval = tokio::time::interval(commit_cadence);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Burn the first immediate tick so the cadence is measured from
    // construction time, not zero.
    interval.tick().await;

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Some(IndexerMsg::Append { event_id, learning }) => {
                        state.persist.receive();
                        if let Err(e) = add_document(&mut writer, &fields, &learning) {
                            tracing::warn!(error = ?e, "indexer add_document failed");
                            continue;
                        }
                        // Each upserted id becomes one `index_doc` edge at the
                        // next commit (BZR-155).
                        state.persist.push(event_id);
                    }
                    Some(IndexerMsg::Flush { ack }) => {
                        let result = if state.persist.has_pending() {
                            state.persist.commit(&mut writer)
                        } else {
                            Ok(())
                        };
                        let _ = ack.send(result);
                    }
                    Some(IndexerMsg::IndexSemanticLessons { agent_root, response }) => {
                        let result = (|| -> Result<(), IndexError> {
                            let outcome = index_semantic_lessons(&mut writer, &fields, &agent_root)?;
                            // No delete/add was issued (unreadable or malformed
                            // LESSONS.md) — nothing to commit. A *missing* file
                            // does issue a delete: it retires the layer.
                            if outcome.touched {
                                state.persist.commit(&mut writer)?;
                            }
                            Ok(())
                        })();
                        let _ = response.send(result);
                    }
                    Some(IndexerMsg::PruneDecayedEvents { event_ids, response }) => {
                        for id in &event_ids {
                            let term = tantivy::Term::from_field_text(fields.event_id, id.as_str());
                            writer.delete_term(term);
                        }
                        let _ = response.send(state.persist.commit(&mut writer));
                    }
                    Some(IndexerMsg::Resync { ack }) => {
                        let result = resync(&mut writer, &fields, &mut state);
                        match ack {
                            Some(ack) => {
                                let _ = ack.send(result);
                            }
                            None => {
                                if let Err(e) = result {
                                    tracing::warn!(error = ?e, "indexer resync failed");
                                }
                            }
                        }
                    }
                    None => {
                        // Channel closed: final commit, then exit.
                        if state.persist.has_pending() {
                            let _ = state.persist.commit(&mut writer);
                        }
                        break;
                    }
                }
            }
            _ = interval.tick() => {
                // A log replaced by rename, or one that shrank, can hold ids
                // the index has never seen and lack ids it has.
                #[cfg(unix)]
                {
                    let now = LogIdentity::at(&state.agent_root.episodic_jsonl()).ok().flatten();
                    let changed = match (state.log, now) {
                        (Some(was), Some(now)) => {
                            (was.dev, was.ino) != (now.dev, now.ino) || now.len < was.len
                        }
                        (None, None) => false,
                        _ => true,
                    };
                    if changed {
                        // On success the pass records the identity it read;
                        // on failure the old one stays, so the next tick retries.
                        if let Err(e) = resync(&mut writer, &fields, &mut state) {
                            tracing::warn!(error = ?e, "indexer resync after a log change failed");
                        }
                    } else {
                        state.log = now;
                    }
                }
                if state.persist.has_pending() {
                    if let Err(e) = state.persist.commit(&mut writer) {
                        tracing::warn!(error = ?e, "indexer cadence commit failed");
                    }
                }
            }
        }
    }
}

/// The [`IndexerMsg::Resync`] handler: commit pending work (a reader cannot see
/// uncommitted docs), read the log, run the coverage pass, and commit with
/// every log id pending.
fn resync(
    writer: &mut IndexWriter<TantivyDocument>,
    fields: &SchemaFields,
    state: &mut IndexerState,
) -> Result<CoverageReport, IndexError> {
    if state.persist.has_pending() {
        state.persist.commit(writer)?;
    }
    let jsonl = state.agent_root.episodic_jsonl();
    // Stat before the read: a change after this point is either in the read or
    // seen by the next tick.
    #[cfg(unix)]
    let seen = LogIdentity::at(&jsonl).map_err(|e| IndexError::Io(e.to_string()))?;
    let events = read_jsonl_events(&jsonl)?;
    let report = cover(writer, fields, &state.coverage, &events)?;
    state.persist.cover_all(&events);
    state.persist.commit(writer)?;
    #[cfg(unix)]
    {
        state.log = seen;
    }
    Ok(report)
}

/// What one semantic (LESSONS.md) indexing pass did.
///
/// `touched` records whether the pass issued any delete/add operation, so the
/// caller knows a commit is required. It is `true` from the wholesale delete
/// onward — even when every lesson was skipped, because the delete alone
/// changes the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct SemanticPassOutcome {
    /// Lessons added as `layer=semantic` documents.
    pub(crate) indexed: usize,
    /// Lessons dropped because their exemplar event was not in the episodic log.
    pub(crate) skipped: usize,
    /// `true` once the pass has mutated the writer (delete and/or add).
    pub(crate) touched: bool,
}

/// Identity token for a lesson document: the exemplar's id in a distinct
/// namespace (DR-211 / AILAB-205).
///
/// Load-bearing, not cosmetic. Both delete sites key on the raw `event_id`
/// term — the decay pruner and the upsert in [`add_document`] — so a lesson
/// carrying its exemplar's id verbatim would be deleted alongside the event,
/// and the upsert would put back only the episodic document. A separate
/// namespace keeps both paths correct without touching either. It also keeps
/// lessons out of decay entirely: the pruner's candidate ids come from the
/// JSONL, which holds no `lsn_` records.
pub(crate) fn semantic_event_id(lesson_id: &str) -> String {
    format!("lsn_{lesson_id}")
}

/// Index `<agent_root>/.agent/semantic/LESSONS.md` as `layer=semantic`
/// documents (DR-211 / AILAB-205).
///
/// Wholesale replace: delete every semantic document, then add the file's
/// current lesson set. LESSONS.md is rewritten in full each dream cycle, so the
/// index mirrors that — a lesson dropped between cycles disappears, and a
/// cluster that stops recurring retires structurally with no expiry logic.
///
/// Does **not** commit. The caller owns the commit so this pass can share the
/// coverage pass's single commit inside [`TantivyIndexHandle::open`](crate::server::tantivy_handle::TantivyIndexHandle::open).
///
/// Exemplar lookup is the live episodic log only; a miss skips the lesson with
/// a `warn!` (AILAB-205 rev 3 cut the snapshot fallback — `plan_cycle` pins
/// every cited exemplar (`cited_pins`) and its decay skips pinned records, so
/// a cited exemplar does not age out). A lesson is never indexed with defaulted
/// pain/importance: that document would score exactly 0.0 and could never rank.
///
/// Tolerate-and-report, in the posture `episodic::read_all` uses — but the two
/// failure modes are deliberately **not** symmetric (AILAB-699):
///
/// * **Missing** LESSONS.md is a *fact*, not a failure: either the store has
///   never dreamed, or a no-promotion cycle retired the file. Both mean zero
///   lessons, so the pass deletes the semantic layer and reports `touched`,
///   silently and with no log line.
/// * **Unreadable or malformed** LESSONS.md is a failure of unknown extent: the
///   lessons may still be there behind a torn write. It logs at `warn!` and
///   returns untouched, so a corrupt file can never wipe the layer. The parse
///   deliberately happens **before** the delete for the same reason.
///
/// Every path that resolves lessons also writes a [`SemanticPassRecord`] to
/// `.dreamd/semantic_pass.json` so `dreamd doctor` can name the skipped lessons
/// (AILAB-700). The write lives here, not at the call sites, because both
/// consumers — the [`TantivyIndexHandle::open`](crate::server::tantivy_handle::TantivyIndexHandle::open) rebuild and the indexer task's
/// `IndexSemanticLessons` handler — go through this one function. The malformed
/// path deliberately writes nothing: it did not touch the index, so the prior
/// record still describes it.
pub(crate) fn index_semantic_lessons(
    writer: &mut IndexWriter<TantivyDocument>,
    fields: &SchemaFields,
    agent_root: &AgentRoot,
) -> Result<SemanticPassOutcome, IndexError> {
    let lessons_path = agent_root.lessons_md();
    let lessons_file = match crate::lessons::read_lessons_file(&lessons_path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // An absent LESSONS.md means "zero lessons", not "skip the pass"
            // (AILAB-699). A no-promotion dream cycle *unlinks* the file — that
            // unlink is the whole retirement mechanism, and without this delete
            // the retired lesson keeps answering recall out of the live index
            // until the daemon restarts. Same STRING exact-match term as the
            // wholesale replace below, so it hits every semantic document and
            // zero episodic ones.
            //
            // `touched: true` is what makes the caller commit. On a store that
            // has never dreamed this costs one delete of an empty layer plus a
            // commit; that is the accepted price of not needing a "did a cycle
            // just remove this file?" signal threaded down here.
            let term = tantivy::Term::from_field_text(fields.layer, Layer::Semantic.as_str());
            writer.delete_term(term);
            // A retired file has no un-indexable lessons, so clear any prior
            // report (AILAB-700) — otherwise doctor keeps naming lessons that
            // no longer exist.
            write_semantic_pass_record(agent_root, &SemanticPassRecord::default());
            return Ok(SemanticPassOutcome {
                indexed: 0,
                skipped: 0,
                touched: true,
            });
        }
        Err(e) => {
            tracing::warn!(
                path = %lessons_path.display(),
                error = %e,
                "LESSONS.md unreadable; leaving the index untouched"
            );
            // Deliberately no report write: the index was not touched, so the
            // prior record is still the truth about what is in it.
            return Ok(SemanticPassOutcome::default());
        }
    };

    // Only read the episodic log once we know there is a LESSONS.md to index,
    // so a project that has never dreamed pays no extra I/O here.
    let events = read_jsonl_events(&agent_root.episodic_jsonl())?;
    let exemplars: HashMap<&str, &AgentLearning> =
        events.iter().map(|ev| (ev.id.as_str(), ev)).collect();

    // `layer` is STRING (raw-tokenized), so this exact-match term hits every
    // semantic document and zero episodic ones. Deleting by the file's
    // clustering key instead would take every episodic event in it as well.
    let term = tantivy::Term::from_field_text(fields.layer, Layer::Semantic.as_str());
    writer.delete_term(term);

    let mut outcome = SemanticPassOutcome {
        indexed: 0,
        skipped: 0,
        touched: true,
    };
    // Kept local rather than on `SemanticPassOutcome`: that type is `Copy` and
    // built with struct-literal syntax at every return, and a `Vec` field would
    // break both (AILAB-700).
    let mut skipped_lesson_ids: Vec<String> = Vec::new();
    for lesson in &lessons_file.lessons {
        let Some(exemplar) = exemplars.get(lesson.id.as_str()) else {
            tracing::warn!(
                lesson_id = %lesson.id,
                cluster_key = %lessons_file.cluster_key,
                path = %lessons_path.display(),
                "lesson exemplar is not in the episodic log; skipping the lesson \
                 (indexing it without the exemplar's pain/importance would produce \
                 a document that scores 0.0 and can never rank)"
            );
            outcome.skipped += 1;
            skipped_lesson_ids.push(lesson.id.clone());
            continue;
        };
        add_semantic_document(writer, fields, &lessons_file, lesson, exemplar)?;
        outcome.indexed += 1;
    }

    // Written on every full pass, including the zero-skip one: without that,
    // a store whose exemplars were restored would keep reporting yesterday's
    // skips forever (AILAB-700).
    write_semantic_pass_record(
        agent_root,
        &SemanticPassRecord {
            skipped_lesson_ids,
            cluster_key: lessons_file.cluster_key.clone(),
            indexed: outcome.indexed,
        },
    );

    Ok(outcome)
}

/// Map one [`crate::lessons::Lesson`] onto a `layer=semantic` Tantivy document.
///
/// `pain` and `importance` are inherited from the exemplar event because a
/// lesson has none of its own: `collector::recall` reads both fast fields with
/// `unwrap_or(0.0)` and the salience product multiplies by each, so a lesson
/// indexed without them would score exactly 0.0 and never surface.
///
/// `timestamp_sec` is the file's `last_updated` — the consolidation time — and
/// deliberately **not** the exemplar's timestamp. Salience decays as
/// `exp(-age_days/14)`, which asks "how stale is this claim?"; for an event that
/// is when it fired, but for a lesson it is when consolidation last re-affirmed
/// it. A lesson distilled today from a 90-day-old exemplar would otherwise score
/// `exp(-6.43) ~= 0.0016` and rank two orders of magnitude below any fresh
/// event — indexed, matching, and permanently buried.
///
/// `source_harness` is the literal `"dreamd"` rather than the exemplar's
/// harness — a promoted cluster spans harnesses, so attributing the synthesized
/// lesson to one contributor would misreport provenance. The exemplar (and its
/// harness) stay one hop away through `event_id`.
///
/// `recurrence` and `cited_event_count` are written as 0 and read by nothing.
/// A lesson's recall recurrence is the subtree count of its `cluster_key`
/// (`crate::recurrence`).
fn add_semantic_document(
    writer: &mut IndexWriter<TantivyDocument>,
    fields: &SchemaFields,
    lessons_file: &crate::lessons::LessonsFile,
    lesson: &crate::lessons::Lesson,
    exemplar: &AgentLearning,
) -> Result<(), IndexError> {
    // Consolidation time, not the exemplar's timestamp — see the fn docs.
    let last_updated_sec = lessons_file.last_updated.timestamp() as u64;
    let doc = doc!(
        fields.content => lesson.content.clone(),
        fields.timestamp_sec => last_updated_sec,
        fields.pain => exemplar.pain as f64,
        fields.importance => exemplar.importance as f64,
        fields.recurrence => 0u64,
        fields.layer => Layer::Semantic.as_str().to_string(),
        fields.last_updated_sec => last_updated_sec,
        fields.cited_event_count => 0u64,
        fields.event_id => semantic_event_id(&lesson.id),
        fields.skill_action => lessons_file.cluster_key.clone(),
        fields.source_harness => SEMANTIC_SOURCE_HARNESS.to_string(),
    );
    writer.add_document(doc).map_err(tantivy_to_index)?;
    Ok(())
}

/// `source_harness` stamped on every lesson document. The dream cycle authored
/// it, not any one harness.
const SEMANTIC_SOURCE_HARNESS: &str = "dreamd";

/// Upsert an [`AgentLearning`] as an episodic Tantivy document: delete every
/// document with its `event_id` term, then add one. Every episodic add goes
/// through here, so an id that is added twice before or after a commit still
/// has one alive document (BZR-1236).
///
/// `layer` is always [`Layer::Episodic`]. Lesson documents are added by the
/// semantic pass, not by this function. `event_id` is `STRING | STORED`, so
/// the delete matches exactly one id. `recurrence` is written as 0; recall
/// derives it per searcher (`crate::recurrence`).
pub(crate) fn add_document(
    writer: &mut IndexWriter<TantivyDocument>,
    fields: &SchemaFields,
    learning: &AgentLearning,
) -> Result<(), IndexError> {
    let ts = learning.timestamp.timestamp() as u64;
    let layer_str = Layer::Episodic.as_str().to_string();
    let id_str = learning.id.as_str().to_string();
    let doc = doc!(
        fields.content => learning.content.clone(),
        fields.timestamp_sec => ts,
        fields.pain => learning.pain as f64,
        fields.importance => learning.importance as f64,
        fields.recurrence => 0u64,
        fields.layer => layer_str,
        fields.last_updated_sec => ts,
        fields.cited_event_count => 0u64,
        fields.event_id => id_str,
        fields.skill_action => learning.skill_action.clone(),
        fields.source_harness => learning.source_harness.clone(),
    );
    writer.delete_term(tantivy::Term::from_field_text(
        fields.event_id,
        learning.id.as_str(),
    ));
    writer.add_document(doc).map_err(tantivy_to_index)?;
    Ok(())
}

/// Record what the semantic pass could not index, for `dreamd doctor`
/// (AILAB-700).
///
/// Best-effort by design, unlike [`write_progress`]: the watermark is
/// crash-recovery state, this is a diagnostic. A store whose `.dreamd/` is
/// read-only must still open its index and serve recall, so a failure here
/// logs and returns rather than failing the pass — doctor then reports the
/// previous pass, which is a stale hint, not a wrong index.
pub(crate) fn write_semantic_pass_record(agent_root: &AgentRoot, record: &SemanticPassRecord) {
    let dir = agent_root.dreamd_dir();
    let path = dir.join(SEMANTIC_PASS_FILENAME);
    let written = serde_json::to_vec(record)
        .map_err(|e| format!("serialize {SEMANTIC_PASS_FILENAME}: {e}"))
        .and_then(|bytes| {
            std::fs::create_dir_all(&dir)
                .and_then(|()| write_atomic(&path, &bytes))
                .map_err(|e| format!("write {SEMANTIC_PASS_FILENAME}: {e}"))
        });
    if let Err(e) = written {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "could not record the semantic pass report; `dreamd doctor` will \
             report the previous pass until the next one lands"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::build_schema;
    use chrono::DateTime;
    use tantivy::collector::Count;
    use tantivy::query::TermQuery;
    use tantivy::schema::IndexRecordOption;
    use tantivy::Index;

    #[test]
    fn add_document_replaces_an_existing_doc_for_the_id() {
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema);
        let mut writer = index.writer(15_000_000).unwrap();
        let id = "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let learning = AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: EventId::parse(id).unwrap(),
            timestamp: DateTime::from_timestamp(1_780_000_000, 0).unwrap(),
            pain: 6.0,
            importance: 6.0,
            pinned: false,
            skill_action: "rust::eh".to_string(),
            source_harness: "test-harness".to_string(),
            content: "indexed twice".to_string(),
        };

        add_document(&mut writer, &fields, &learning).unwrap();
        writer.commit().unwrap();
        add_document(&mut writer, &fields, &learning).unwrap();
        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        reader.reload().unwrap();
        let query = TermQuery::new(
            tantivy::Term::from_field_text(fields.event_id, id),
            IndexRecordOption::Basic,
        );
        let alive = reader.searcher().search(&query, &Count).unwrap();
        assert_eq!(
            alive, 1,
            "a second add of the same id must replace the first"
        );
    }

    /// BZR-1243: after a coverage pass, `committed_appends` is every `Append`
    /// received, including one whose `add_document` failed. The failure is
    /// simulated at the persist level: `receive()` without `push()`.
    #[test]
    fn a_coverage_pass_counts_every_received_append() {
        let dir = tempfile::tempdir().unwrap();
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema);
        let mut writer = index.writer(15_000_000).unwrap();
        let mark = Arc::new(Mutex::new(CommitMark::default()));
        let mut persist = IndexerPersist::new(
            dir.path().join("index_progress.json"),
            Ledger::open(dir.path().join("ledger.jsonl")),
            &IndexProgress::default(),
            Arc::clone(&mark),
        );
        let learning = |tail: char| AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: EventId::parse(&format!("evt_01ARZ3NDEKTSV4RRFFQ69G5FA{tail}")).unwrap(),
            timestamp: DateTime::from_timestamp(1_780_000_000, 0).unwrap(),
            pain: 6.0,
            importance: 6.0,
            pinned: false,
            skill_action: "rust::eh".to_string(),
            source_harness: "test-harness".to_string(),
            content: "probe".to_string(),
        };
        let (a, b, c) = (learning('A'), learning('B'), learning('C'));

        // Two `Append`s received; A's add "failed", so only B is pushed.
        persist.receive();
        persist.receive();
        add_document(&mut writer, &fields, &b).unwrap();
        persist.push(b.id.clone());
        persist.commit(&mut writer).unwrap();
        assert_eq!(mark.lock().unwrap().committed_appends, 1, "live persist");

        // The coverage pass indexes every log id, A included.
        add_document(&mut writer, &fields, &a).unwrap();
        persist.cover_all(&[a.clone(), b.clone()]);
        persist.commit(&mut writer).unwrap();
        assert_eq!(
            mark.lock().unwrap().committed_appends,
            2,
            "a coverage pass covers every received Append"
        );

        // A later live persist counts its own pushes again.
        persist.receive();
        add_document(&mut writer, &fields, &c).unwrap();
        persist.push(c.id.clone());
        persist.commit(&mut writer).unwrap();
        assert_eq!(
            *mark.lock().unwrap(),
            CommitMark {
                last_indexed_id: Some(c.id.clone()),
                committed_appends: 3,
            }
        );
    }

    /// BZR-1243: a persist whose ledger append fails publishes nothing, and
    /// the retry publishes the whole step.
    #[test]
    fn a_ledger_failure_in_the_persist_step_publishes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema);
        let mut writer = index.writer(15_000_000).unwrap();
        let mark = Arc::new(Mutex::new(CommitMark::default()));
        let mut persist = IndexerPersist::new(
            dir.path().join("index_progress.json"),
            Ledger::open(dir.path().join("ledger.jsonl")),
            &IndexProgress::default(),
            Arc::clone(&mark),
        );
        let a = AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: EventId::parse("evt_01ARZ3NDEKTSV4RRFFQ69G5FAA").unwrap(),
            timestamp: DateTime::from_timestamp(1_780_000_000, 0).unwrap(),
            pain: 6.0,
            importance: 6.0,
            pinned: false,
            skill_action: "rust::eh".to_string(),
            source_harness: "test-harness".to_string(),
            content: "ledger failure".to_string(),
        };
        let failpoint = dir.path().join("failpoint-ledger-write");

        persist.receive();
        add_document(&mut writer, &fields, &a).unwrap();
        persist.push(a.id.clone());
        std::fs::write(&failpoint, b"").unwrap();
        let before = mark.lock().unwrap().clone();
        assert!(
            persist.commit(&mut writer).is_err(),
            "the ledger append fails"
        );
        assert_eq!(
            *mark.lock().unwrap(),
            before,
            "a failed persist publishes nothing"
        );

        std::fs::remove_file(&failpoint).unwrap();
        persist.commit(&mut writer).unwrap();
        assert_eq!(
            *mark.lock().unwrap(),
            CommitMark {
                last_indexed_id: Some(a.id.clone()),
                committed_appends: 1,
            }
        );
    }
}
