//! The per-project indexer actor (WEG-42 / DR-202), split out of
//! `tantivy_handle` by BZR-170.
//!
//! One tokio task owns the Tantivy `IndexWriter`. The coordinator and the
//! dream cycle talk to it only through [`IndexerMsg`]. Everything that mutates
//! the writer lives here: episodic `add_document`, the cadence and
//! [`IndexerMsg::Flush`] commits, the recurrence sidecar, the decay prune, and
//! the semantic (LESSONS.md) pass that `TantivyIndexHandle::open` also runs
//! during replay.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dreamd_protocol::{AgentLearning, EventId};
use serde::{Deserialize, Serialize};
use tantivy::{doc, IndexWriter, TantivyDocument};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::index::{ClusterCount, Layer, RecurrenceSidecar, SchemaFields};
use crate::io::write_atomic;
use crate::layout::AgentRoot;
use crate::server::index_freshness::{read_jsonl_events, write_progress, IndexProgress};
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
/// Nothing is dropped — a shed `IndexerMsg::Append` would NOT be recovered by
/// startup replay, which filters `id > last_indexed_id` (a watermark, not a
/// contiguous prefix), so a gap followed by any indexed event is skipped
/// forever.
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
    /// Older docs in a cluster intentionally carry the recurrence value at
    /// their index time, not the live cluster count. This is bounded
    /// staleness — stale rows underweight their cluster, never overweight.
    /// Reconciliation is a dream-cycle concern, not a v0.1 indexer concern.
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
    /// Dream-cycle hook (WEG-45 / DR-205′): read `semantic/recurrence_counts.json`,
    /// walk the JSONL, delete-and-re-add each event with the authoritative
    /// cluster count, then commit. Resolves after the commit completes.
    ApplyRecurrenceSidecar {
        agent_root: AgentRoot,
        response: oneshot::Sender<Result<(), IndexError>>,
    },
    /// Decay pruner hook (WEG-62 / DR-309): delete decayed event IDs from the index.
    /// Does not touch the JSONL — JSONL rewrite is handled by `run_decay_pruner`.
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

/// The indexer task: owns the `IndexWriter`, batches appends, commits on the
/// cadence tick or on [`IndexerMsg::Flush`], and exits after a final commit
/// when every sender is dropped.
pub(crate) async fn run_indexer(
    mut writer: IndexWriter<TantivyDocument>,
    fields: SchemaFields,
    mut rx: mpsc::Receiver<IndexerMsg>,
    commit_cadence: Duration,
    mut clusters: HashMap<String, u32>,
    progress_path: PathBuf,
    mut last_committed_id: Option<String>,
) {
    let mut batch_last_id: Option<String> = None;
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
                        let counter = clusters
                            .entry(learning.skill_action.clone())
                            .or_insert(0);
                        *counter += 1;
                        let recurrence = *counter;
                        if let Err(e) = add_document(&mut writer, &fields, &learning, recurrence) {
                            tracing::warn!(error = ?e, "indexer add_document failed");
                            continue;
                        }
                        batch_last_id = Some(event_id.as_str().to_owned());
                    }
                    Some(IndexerMsg::Flush { ack }) => {
                        let result = commit_and_persist(
                            &mut writer,
                            &progress_path,
                            &mut batch_last_id,
                            &mut last_committed_id,
                        );
                        let _ = ack.send(result);
                    }
                    Some(IndexerMsg::ApplyRecurrenceSidecar { agent_root, response }) => {
                        let result = apply_recurrence_sidecar_inner(
                            &mut writer,
                            &fields,
                            &agent_root,
                        );
                        let _ = response.send(result);
                    }
                    Some(IndexerMsg::IndexSemanticLessons { agent_root, response }) => {
                        let result = (|| -> Result<(), IndexError> {
                            let outcome = index_semantic_lessons(&mut writer, &fields, &agent_root)?;
                            // No delete/add was issued (unreadable or malformed
                            // LESSONS.md) — nothing to commit. A *missing* file
                            // does issue a delete: it retires the layer.
                            if outcome.touched {
                                writer.commit().map_err(tantivy_to_index)?;
                            }
                            Ok(())
                        })();
                        let _ = response.send(result);
                    }
                    Some(IndexerMsg::PruneDecayedEvents { event_ids, response }) => {
                        let result = (|| -> Result<(), IndexError> {
                            for id in &event_ids {
                                let term = tantivy::Term::from_field_text(fields.event_id, id.as_str());
                                writer.delete_term(term);
                            }
                            writer.commit().map_err(tantivy_to_index)?;
                            Ok(())
                        })();
                        let _ = response.send(result);
                    }
                    None => {
                        // Channel closed: final flush, then exit.
                        let _ = commit_and_persist(
                            &mut writer,
                            &progress_path,
                            &mut batch_last_id,
                            &mut last_committed_id,
                        );
                        break;
                    }
                }
            }
            _ = interval.tick() => {
                if batch_last_id.is_some() {
                    if let Err(e) = commit_and_persist(
                        &mut writer,
                        &progress_path,
                        &mut batch_last_id,
                        &mut last_committed_id,
                    ) {
                        tracing::warn!(error = ?e, "indexer cadence commit failed");
                    }
                }
            }
        }
    }
}

pub(crate) fn commit_and_persist(
    writer: &mut IndexWriter<TantivyDocument>,
    progress_path: &Path,
    batch_last_id: &mut Option<String>,
    last_committed_id: &mut Option<String>,
) -> Result<(), IndexError> {
    let Some(new_last) = batch_last_id.take() else {
        return Ok(());
    };
    // Write protocol (WEG-42): Tantivy commit first, then watermark on disk.
    // If we crash after commit but before write_progress, the next startup
    // replay re-indexes at most one 5-second window (idempotent). If we wrote
    // the watermark first and then crashed, those events would be silently
    // skipped on recovery -- silent data loss.
    writer.commit().map_err(tantivy_to_index)?;
    *last_committed_id = Some(new_last);
    write_progress(
        progress_path,
        &IndexProgress {
            last_indexed_id: last_committed_id.clone(),
        },
    )?;
    Ok(())
}

/// Implements the delete-and-re-add recurrence update triggered by
/// [`IndexerMsg::ApplyRecurrenceSidecar`] (WEG-45 / DR-205′).
///
/// Algorithm:
/// 1. Read `<agent_root>/.agent/semantic/recurrence_counts.json`.
/// 2. Parse every line of the JSONL into a per-`skill_action` bucket.
/// 3. For each cluster in the sidecar: delete every matching event by its
///    `event_id` term, then re-add the event document with the sidecar's
///    authoritative `count` as the `recurrence` FastField value.
/// 4. Commit once after all clusters are processed.
pub(crate) fn apply_recurrence_sidecar_inner(
    writer: &mut IndexWriter<TantivyDocument>,
    fields: &SchemaFields,
    agent_root: &AgentRoot,
) -> Result<(), IndexError> {
    // 1. Read and parse the sidecar.
    let sidecar_path = agent_root.semantic_dir().join("recurrence_counts.json");
    let sidecar_json = std::fs::read_to_string(&sidecar_path)
        .map_err(|e| IndexError::Other(format!("read recurrence_counts.json: {e}")))?;
    let sidecar: RecurrenceSidecar = serde_json::from_str(&sidecar_json)
        .map_err(|e| IndexError::Other(format!("parse recurrence_counts.json: {e}")))?;

    // 2. Walk the JSONL (shared episodic scan, WEG-378) and bucket by skill_action.
    let jsonl_path = agent_root.episodic_jsonl();
    let events = read_jsonl_events(&jsonl_path)?;
    if events.is_empty() {
        // Nothing indexed yet — sidecar application is a no-op.
        return Ok(());
    }
    let mut by_skill: HashMap<String, Vec<AgentLearning>> = HashMap::new();
    for learning in events {
        by_skill
            .entry(learning.skill_action.clone())
            .or_default()
            .push(learning);
    }

    // 3. Delete-and-re-add for each cluster listed in the sidecar.
    for ClusterCount {
        skill_action,
        count,
    } in &sidecar.clusters
    {
        let events = match by_skill.get(skill_action) {
            Some(v) => v,
            None => continue,
        };
        for event in events {
            // Delete the existing document by its exact event_id term.
            let id_str = event.id.as_str().to_string();
            let term = tantivy::Term::from_field_text(fields.event_id, &id_str);
            writer.delete_term(term);
            // Re-add with the sidecar-authoritative recurrence count.
            add_document(writer, fields, event, *count)?;
        }
    }

    // 4. Commit once after all clusters.
    writer.commit().map_err(tantivy_to_index)?;
    Ok(())
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
/// Load-bearing, not cosmetic. Both existing delete sites key on the raw
/// `event_id` term — the decay pruner and the recurrence sidecar — so a lesson
/// carrying its exemplar's id verbatim would be deleted alongside the event,
/// and the sidecar would re-add only the episodic document. A separate
/// namespace keeps both paths correct without touching either. It also keeps
/// lessons out of decay entirely: the pruner's candidate ids come from the
/// JSONL, which holds no `lsn_` records.
pub(crate) fn semantic_event_id(lesson_id: &str) -> String {
    format!("lsn_{lesson_id}")
}

/// Count the episodic events belonging to `cluster_key`, using the same prefix
/// semantics `consolidation` promotes with.
///
/// `compute_promoted_clusters` promotes at the *deepest* prefix that met the
/// threshold, so member events routinely carry longer leaf keys — three events
/// under `rust::eh::unwrap` and two under `rust::eh::expect` can promote as
/// `rust::eh`. An exact-match count would report 0 members for exactly those
/// clusters, and `recurrence` feeds the salience product.
pub(crate) fn cluster_member_count(events: &[AgentLearning], cluster_key: &str) -> u64 {
    let child_prefix = format!("{cluster_key}::");
    events
        .iter()
        .filter(|ev| ev.skill_action == cluster_key || ev.skill_action.starts_with(&child_prefix))
        .count() as u64
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
/// episodic replay's single commit inside [`TantivyIndexHandle::open`](crate::server::tantivy_handle::TantivyIndexHandle::open).
///
/// Exemplar lookup is the live episodic log only; a miss skips the lesson with
/// a `warn!` (AILAB-205 rev 3 cut the snapshot fallback — `apply_pin_unpin`
/// pins every cited exemplar and `should_decay` short-circuits on `pinned`, so
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
    let member_count = cluster_member_count(&events, &lessons_file.cluster_key);

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
        add_semantic_document(
            writer,
            fields,
            &lessons_file,
            lesson,
            exemplar,
            member_count,
        )?;
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
fn add_semantic_document(
    writer: &mut IndexWriter<TantivyDocument>,
    fields: &SchemaFields,
    lessons_file: &crate::lessons::LessonsFile,
    lesson: &crate::lessons::Lesson,
    exemplar: &AgentLearning,
    member_count: u64,
) -> Result<(), IndexError> {
    // Consolidation time, not the exemplar's timestamp — see the fn docs.
    let last_updated_sec = lessons_file.last_updated.timestamp() as u64;
    let doc = doc!(
        fields.content => lesson.content.clone(),
        fields.timestamp_sec => last_updated_sec,
        fields.pain => exemplar.pain as f64,
        fields.importance => exemplar.importance as f64,
        fields.recurrence => member_count,
        fields.layer => Layer::Semantic.as_str().to_string(),
        fields.last_updated_sec => last_updated_sec,
        fields.cited_event_count => member_count,
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

/// Map an [`AgentLearning`] onto a Tantivy document and add it to the writer.
/// `layer` is always [`Layer::Episodic`] in v0.1; semantic indexing is WEG-136.
/// `event_id` is stored as `STRING | STORED` for targeted delete-and-re-add
/// during recurrence sidecar application (WEG-45 / DR-205′).
pub(crate) fn add_document(
    writer: &mut IndexWriter<TantivyDocument>,
    fields: &SchemaFields,
    learning: &AgentLearning,
    recurrence: u32,
) -> Result<(), IndexError> {
    let ts = learning.timestamp.timestamp() as u64;
    let layer_str = Layer::Episodic.as_str().to_string();
    let id_str = learning.id.as_str().to_string();
    let doc = doc!(
        fields.content => learning.content.clone(),
        fields.timestamp_sec => ts,
        fields.pain => learning.pain as f64,
        fields.importance => learning.importance as f64,
        fields.recurrence => recurrence as u64,
        fields.layer => layer_str,
        fields.last_updated_sec => ts,
        fields.cited_event_count => 0u64,
        fields.event_id => id_str,
        fields.skill_action => learning.skill_action.clone(),
        fields.source_harness => learning.source_harness.clone(),
    );
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
