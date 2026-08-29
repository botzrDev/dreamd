//! Cluster engine and pin/unpin lifecycle (DR-301 / WEG-58).
//!
//! `run_cluster_engine` reads the episodic JSONL, builds a prefix tree over
//! `skill_action` (split on `::`), applies deepest-wins disambiguation, and
//! writes `semantic/recurrence_counts.json`.
//!
//! `apply_pin_unpin` rewrites `AGENT_LEARNINGS.jsonl`, setting `pinned` on IDs
//! cited in the freshly-written `LESSONS.md` **unioned** with any `pinned` flag
//! an external writer already set — the cycle never unsets a pin it did not
//! itself set (SPEC §67 / WEG-426). Called by WEG-61 after the cycle's guarded
//! `LESSONS.md` write, before `commit_cycle`; not safe to call before
//! `LESSONS.md` exists (succeeds silently if absent).

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Utc};
use dreamd_protocol::AgentLearning;

use crate::episodic::{self, EpisodicError};
use crate::index::{ClusterCount, RecurrenceSidecar};
use crate::io::write_atomic;
use crate::layout::AgentRoot;
use crate::lessons::{self, Lesson, LessonsFile};
use crate::salience::{salience_with_context, RecurrenceContext};
use crate::wal::{self, GuardedReplaceKind, WalError};

#[derive(Debug, thiserror::Error)]
pub enum ConsolidationError {
    #[error("reading AGENT_LEARNINGS.jsonl: {0}")]
    Io(#[from] std::io::Error),
    /// Retained for the serialize side (WEG-378); the read side no longer
    /// produces a per-line parse error — `episodic::scan` tolerates a torn tail.
    #[error("parsing event at line {line}: {source}")]
    Json {
        line: usize,
        source: serde_json::Error,
    },
}

impl From<EpisodicError> for ConsolidationError {
    fn from(e: EpisodicError) -> Self {
        match e {
            EpisodicError::Io(io) => ConsolidationError::Io(io),
            EpisodicError::Serialize(je) => ConsolidationError::Json {
                line: 0,
                source: je,
            },
            // Never produced on the consolidation rewrite/read paths (no size
            // check), but the match must be exhaustive.
            EpisodicError::PayloadTooLarge { size, max } => ConsolidationError::Io(
                std::io::Error::other(format!("payload too large: {size} > {max}")),
            ),
        }
    }
}

/// Output of [`run_cluster_engine`]: which clusters were promoted and their
/// member events.
#[derive(Debug, Clone, Default)]
pub struct ClusterOutput {
    /// Promoted clusters in deepest-wins order. Each event belongs to exactly
    /// one cluster.
    pub promoted: Vec<PromotedCluster>,
}

/// One promoted cluster returned by [`run_cluster_engine`].
#[derive(Debug, Clone)]
pub struct PromotedCluster {
    /// The deepest prefix at which ≥ `PROMOTION_THRESHOLD` events landed.
    pub cluster_key: String,
    /// All member events (may have differing leaf `skill_action` keys).
    pub events: Vec<AgentLearning>,
    /// Sum of per-event salience scores at `now_sec`.
    pub salience_sum: f64,
}

/// Minimum event count within a recurrence window for cluster promotion.
pub const PROMOTION_THRESHOLD: usize = 3;

/// Trailing window for short-burst recurrence detection (seconds).
pub const WINDOW_7_DAYS_SEC: i64 = 7 * 24 * 3600;

/// Trailing window for slow-burn recurrence detection (seconds).
pub const WINDOW_30_DAYS_SEC: i64 = 30 * 24 * 3600;

/// Build the `skill_action` prefix tree over `events`: each event contributes
/// to every `::`-prefix of its key (segments are never re-split on `.` or `-`;
/// see ARCHITECTURE.md §9). Returns prefix → indices into `events`.
fn build_prefix_tree(events: &[AgentLearning]) -> BTreeMap<String, Vec<usize>> {
    let mut depth_map: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, event) in events.iter().enumerate() {
        let parts: Vec<&str> = event.skill_action.split("::").collect();
        for len in 1..=parts.len() {
            let prefix = parts[..len].join("::");
            depth_map.entry(prefix).or_default().push(i);
        }
    }
    depth_map
}

/// Run the cluster engine against `agent_root`'s episodic JSONL.
///
/// `now_sec` is caller-provided for determinism — do not call `Utc::now()`.
///
/// Steps:
/// 1. Read + parse `AGENT_LEARNINGS.jsonl`.
/// 2. Build the prefix tree: each event contributes to every prefix of its
///    `skill_action` (split on `::`).
/// 3. Find the deepest prefix where the 7-day **or** 30-day trailing window
///    holds ≥ `PROMOTION_THRESHOLD` events (deepest-wins — an event is
///    assigned to exactly one promoted cluster).
/// 4. Compute per-cluster salience sum.
/// 5. Write `recurrence_counts.json` sidecar.
/// 6. Return [`ClusterOutput`].
///
/// If the JSONL is absent or empty, returns `Ok(ClusterOutput { promoted: vec![] })`.
pub fn run_cluster_engine(
    agent_root: &AgentRoot,
    now_sec: i64,
) -> Result<ClusterOutput, ConsolidationError> {
    let events = episodic::read_all(&agent_root.episodic_jsonl())?;
    if events.is_empty() {
        return Ok(ClusterOutput { promoted: vec![] });
    }

    let promoted = compute_promoted_clusters(&events, now_sec);

    // Step 5: write recurrence sidecar.
    let sidecar_clusters: Vec<ClusterCount> = promoted
        .iter()
        .map(|c| ClusterCount {
            skill_action: c.cluster_key.clone(),
            count: c.events.len() as u32,
        })
        .collect();
    let sidecar = RecurrenceSidecar {
        schema_version: "1.0".to_string(),
        clusters: sidecar_clusters,
    };
    let sidecar_path = agent_root.semantic_dir().join("recurrence_counts.json");
    std::fs::create_dir_all(agent_root.semantic_dir())?;
    let sidecar_json = serde_json::to_string_pretty(&sidecar)
        .map_err(|e| ConsolidationError::Json { line: 0, source: e })?;
    write_atomic(&sidecar_path, sidecar_json.as_bytes())?;

    Ok(ClusterOutput { promoted })
}

/// Compute the clusters the dream cycle would promote from `events`, with no
/// disk access and no sidecar write. Steps 2–4 of [`run_cluster_engine`].
///
/// `now_sec` is caller-provided for determinism — do not call `Utc::now()`.
///
/// Shared by [`run_cluster_engine`] and `dreamd doctor --cluster-health` so the
/// promotion rules — threshold windows **and** deepest-wins claiming — cannot
/// drift between what the dream cycle writes and what doctor audits. Deriving
/// raw prefix-tree counts instead would report drift on every healthy store,
/// since the sidecar only ever holds promoted, deepest-wins-claimed counts.
pub fn compute_promoted_clusters(events: &[AgentLearning], now_sec: i64) -> Vec<PromotedCluster> {
    // Step 2: build prefix tree. Each event contributes to ALL prefixes of
    // its skill_action. We store event indices to avoid cloning until the end.
    let depth_map = build_prefix_tree(events);

    // Step 3: deepest-wins. Sort prefixes longest-first so each event is
    // claimed by the deepest qualifying cluster on first encounter.
    let mut prefixes: Vec<String> = depth_map.keys().cloned().collect();
    prefixes.sort_by(|a, b| {
        b.split("::")
            .count()
            .cmp(&a.split("::").count())
            .then(a.cmp(b))
    });

    let mut event_cluster: HashMap<usize, String> = HashMap::new();
    let mut cluster_indices: BTreeMap<String, Vec<usize>> = BTreeMap::new();

    for prefix in &prefixes {
        let member_indices = &depth_map[prefix];
        let in_7d = count_in_window(member_indices, events, now_sec, WINDOW_7_DAYS_SEC);
        let in_30d = count_in_window(member_indices, events, now_sec, WINDOW_30_DAYS_SEC);
        if in_7d < PROMOTION_THRESHOLD && in_30d < PROMOTION_THRESHOLD {
            continue;
        }
        for &idx in member_indices {
            if let std::collections::hash_map::Entry::Vacant(e) = event_cluster.entry(idx) {
                e.insert(prefix.clone());
                cluster_indices.entry(prefix.clone()).or_default().push(idx);
            }
        }
    }

    // Step 4: build PromotedCluster entries with salience sums.
    let promoted: Vec<PromotedCluster> = cluster_indices
        .into_iter()
        .map(|(cluster_key, indices)| {
            let cluster_events: Vec<AgentLearning> =
                indices.iter().map(|&i| events[i].clone()).collect();
            let recurrence = RecurrenceContext::dream_cycle(cluster_events.len());
            let salience_sum = cluster_events
                .iter()
                .map(|e| {
                    salience_with_context(
                        now_sec,
                        e.timestamp.timestamp(),
                        e.pain as f64,
                        e.importance as f64,
                        recurrence,
                    )
                })
                .sum();
            PromotedCluster {
                cluster_key,
                events: cluster_events,
                salience_sum,
            }
        })
        .collect();

    promoted
}

/// Set `pinned` on episodic entries cited in the freshly-written `LESSONS.md`,
/// **unioned** with any `pinned` flag an external writer already set — the cycle
/// never unsets a pin it did not itself set (SPEC §67 / WEG-426).
///
/// "Cited" means every [`Lesson::id`] **and** every id in the file-level
/// `citations` frontmatter (AILAB-200).
///
/// Called by WEG-61 (DR-308) after [`write_selected_lesson`] has replaced
/// `LESSONS.md`, while the orchestrator-owned dream-cycle WAL is still open. Returns `Ok` without
/// mutations if `LESSONS.md` or the JSONL is absent.
pub fn apply_pin_unpin(agent_root: &AgentRoot) -> Result<(), ConsolidationError> {
    let lessons_path = agent_root.lessons_md();
    let cited_ids: HashSet<String> = if lessons_path.exists() {
        let lessons_file = lessons::read_lessons_file(&lessons_path)?;
        // Union, not either-or (AILAB-200). `Lesson.id` is the exemplar the
        // lesson is filed under; `citations` is every cluster member the model
        // said it drew on. Both must outlive decay or the lesson outlives its
        // evidence. A pre-AILAB-200 file has no `citations` key, so the union
        // collapses to today's exemplar-only set.
        lessons_file
            .lessons
            .iter()
            .map(|l| l.id.clone())
            .chain(lessons_file.citations.iter().cloned())
            .collect()
    } else {
        HashSet::new()
    };

    let jsonl_path = agent_root.episodic_jsonl();
    let mut events = episodic::read_all(&jsonl_path)?;
    if events.is_empty() {
        return Ok(());
    }

    for event in &mut events {
        event.pinned = event.pinned || cited_ids.contains(event.id.as_str());
    }

    // One call: the prune intent is recorded inside the atomic rewrite, between
    // the temp's fsync and the rename, naming the temp the write itself created
    // (AILAB-164). Recovery cleans up that temp on a mid-cycle crash. No WAL
    // open (no cycle) still rewrites the log.
    episodic::rewrite_guarded(agent_root, &jsonl_path, &events)?;
    Ok(())
}

/// Error type for the deterministic dream cycle orchestrator.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum DreamCycleError {
    #[error("cluster engine: {0}")]
    Cluster(#[from] ConsolidationError),
    #[error("WAL: {0}")]
    Wal(#[from] WalError),
    #[error("lessons write: {0}")]
    Lessons(#[from] std::io::Error),
}

/// `prompt_version` written when the lesson body is the exemplar's own text.
pub const DETERMINISTIC_PROMPT_ID: &str = "deterministic-only";

/// Where a lesson body came from (AILAB-204).
///
/// Only the body text and the frontmatter `prompt_version` differ between the
/// two — the WAL intent, the `cluster_key`, the pin pass, and above all the
/// [`Lesson::id`] (always the exemplar's `evt_` id) are shared, so semantic
/// indexing inherits the same pain/importance whichever path composed the prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LessonBodySource {
    /// Copy the exemplar's own `content` verbatim. `prompt_version` is
    /// [`DETERMINISTIC_PROMPT_ID`].
    Deterministic,
    /// Use model-composed prose. Empty `content` degrades to
    /// [`LessonBodySource::Deterministic`] — a model that returned nothing must
    /// not blank out `LESSONS.md`.
    Llm {
        content: String,
        prompt_version: String,
        /// The model's citation trailer, already validated against the promoted
        /// cluster by [`crate::dream_cycle`] (AILAB-200). Unvalidated citations
        /// never reach this variant — they arrive as
        /// [`LessonBodySource::Deterministic`] instead.
        citations: Vec<String>,
    },
}

/// The cluster this cycle will write a lesson for, before a body is chosen.
///
/// Produced by [`select_lesson_or_retire`] and consumed by
/// [`write_selected_lesson`]. Splitting the cycle here is what lets an `await`
/// on a network call sit between cluster selection and the `LESSONS.md` write
/// while both paths still share one exemplar id and one WAL envelope.
#[derive(Debug, Clone)]
pub struct SelectedLesson {
    /// File-level cluster key of the winning (highest `salience_sum`) cluster.
    pub cluster_key: String,
    /// `EventId` of the exemplar. This becomes [`Lesson::id`] on **both** paths.
    pub exemplar_id: String,
    /// The exemplar's own body — the deterministic lesson text.
    pub exemplar_content: String,
    /// Every member event of the winning cluster, for prompt construction.
    pub events: Vec<AgentLearning>,
}

/// Cluster, then either select the winning cluster's exemplar or retire.
///
/// `Ok(None)` means **nothing promoted**: any existing `LESSONS.md` has already
/// been unlinked (AILAB-699) and the caller must not write one. Absence — not an
/// empty frontmatter file — is the canonical "nothing promoted" state, so a
/// store that stops recurring converges on the same shape as one that never
/// dreamed. Cited exemplars keep their pins.
pub fn select_lesson_or_retire(
    agent_root: &AgentRoot,
    now_sec: i64,
) -> Result<Option<SelectedLesson>, DreamCycleError> {
    let cluster_output = run_cluster_engine(agent_root, now_sec)?;

    if cluster_output.promoted.is_empty() {
        // AILAB-699: retirement is structural, not an expiry timer. When no
        // cluster clears `PROMOTION_THRESHOLD` in either window any more, the
        // `LESSONS.md` an earlier promoting cycle wrote is stale — leaving it
        // meant recall served that lesson forever.
        //
        // No `WalIntent`: recovery only cleans up intent temps (`wal.rs`), and
        // an unlink of the live file is itself the desired post-state — crash
        // after it and the lesson is retired, crash before and the file
        // remains for the next cycle to retire.
        //
        // Pins are untouched. `apply_pin_unpin` is union-only, so calling it
        // here would rewrite the JSONL for no effect; the exemplars this
        // lesson cited stay pinned (SPEC §67 / WEG-426).
        //
        // The index side is handled by the post-cycle semantic pass, which
        // runs unconditionally and turns the now-absent file into
        // `delete_term(layer="semantic")` (`server::tantivy_handle`).
        match std::fs::remove_file(agent_root.lessons_md()) {
            Ok(()) => {}
            // Already gone — the common never-promoted case, or a racing
            // remover. Either way the post-state is the one we wanted.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        return Ok(None);
    }

    let top_cluster = cluster_output
        .promoted
        .iter()
        .max_by(|a, b| {
            a.salience_sum
                .partial_cmp(&b.salience_sum)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap(); // safe: promoted.is_empty() guarded above

    let exemplar = pick_exemplar(&top_cluster.events, now_sec);

    Ok(Some(SelectedLesson {
        cluster_key: top_cluster.cluster_key.clone(),
        exemplar_id: exemplar.id.as_str().to_string(),
        exemplar_content: exemplar.content.clone(),
        events: top_cluster.events.clone(),
    }))
}

/// Cluster + pick exemplar with no sidecar write and no `LESSONS.md` unlink.
///
/// The read-only twin of [`select_lesson_or_retire`], for `dreamd dream --dry`
/// (AILAB-341). Same promotion rules — it calls the same
/// [`compute_promoted_clusters`] and the same [`pick_exemplar`] — but it reaches
/// disk exactly once, to read the episodic JSONL.
///
/// `Ok(None)` means **nothing would be promoted**. Unlike the write path this
/// leaves any existing `LESSONS.md` untouched: a preview that deleted the file
/// it is previewing the deletion of would be a write, and `--dry` writes
/// nothing. The caller reports "would retire" instead.
///
/// `now_sec` is caller-provided for determinism — do not call `Utc::now()`.
pub fn preview_select_lesson(
    agent_root: &AgentRoot,
    now_sec: i64,
) -> Result<Option<SelectedLesson>, DreamCycleError> {
    let events =
        episodic::read_all(&agent_root.episodic_jsonl()).map_err(ConsolidationError::from)?;
    let promoted = compute_promoted_clusters(&events, now_sec);
    if promoted.is_empty() {
        return Ok(None);
    }

    let top_cluster = promoted
        .iter()
        .max_by(|a, b| {
            a.salience_sum
                .partial_cmp(&b.salience_sum)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap(); // safe: promoted.is_empty() guarded above

    let exemplar = pick_exemplar(&top_cluster.events, now_sec);

    Ok(Some(SelectedLesson {
        cluster_key: top_cluster.cluster_key.clone(),
        exemplar_id: exemplar.id.as_str().to_string(),
        exemplar_content: exemplar.content.clone(),
        events: top_cluster.events.clone(),
    }))
}

/// Build the [`LessonsFile`] `selected` + `body` resolve to.
///
/// The single owner of the frontmatter shape and of the empty-LLM-body
/// degradation rule. [`write_selected_lesson`] serializes what this returns and
/// `dreamd dream --dry` renders it; sharing the builder is what makes the
/// preview byte-identical to the file a real cycle would persist (AILAB-341).
#[must_use]
pub fn lessons_file_from_selected(
    now_sec: i64,
    selected: &SelectedLesson,
    body: LessonBodySource,
) -> LessonsFile {
    // An LLM body that is empty (or whitespace) is not a body. Falling through
    // to the exemplar text keeps `LESSONS.md` non-empty and keeps the
    // frontmatter honest about which producer actually wrote the prose.
    let (content, prompt_version, citations) = match body {
        LessonBodySource::Llm {
            content,
            prompt_version,
            citations,
        } if !content.trim().is_empty() => (content, prompt_version, citations),
        // The deterministic body cites exactly the event it copied. Writing the
        // exemplar here rather than an empty list is what keeps `--no-llm` pin
        // behavior identical to the pre-AILAB-200 `Lesson.id`-only pass, and
        // what makes an LLM fallback byte-identical to a deterministic cycle.
        _ => (
            selected.exemplar_content.clone(),
            DETERMINISTIC_PROMPT_ID.to_string(),
            vec![selected.exemplar_id.clone()],
        ),
    };

    let lessons = vec![Lesson {
        // Always the exemplar id — never minted here. Semantic indexing
        // inherits pain/importance from this event (AILAB-204 §1.7).
        id: selected.exemplar_id.clone(),
        content,
        pinned: false,
    }];

    LessonsFile {
        last_updated: DateTime::<Utc>::from_timestamp(now_sec, 0).unwrap_or_default(),
        prompt_version,
        cluster_key: selected.cluster_key.clone(),
        citations,
        lessons,
    }
}

/// Write `LESSONS.md` for `selected` with the chosen body, then apply pins.
///
/// The `ReplaceSemanticMemory` intent lands in the orchestrator-owned WAL
/// envelope as part of the write itself, naming the temp that write created
/// (AILAB-164). It is deliberately **not** recorded ahead of the write any more:
/// that order could put an intent in the WAL pointing at a `.tmp` that did not
/// exist yet, so a crash between the two left recovery chasing a phantom.
///
/// The bytes are `lessons::render_lessons_file`'s — the same serializer
/// `lessons::write_lessons_file` and `dreamd dream --dry` use (AILAB-341), so
/// going around that writer to reach the WAL cannot change what lands on disk.
pub fn write_selected_lesson(
    agent_root: &AgentRoot,
    now_sec: i64,
    selected: &SelectedLesson,
    body: LessonBodySource,
) -> Result<(), DreamCycleError> {
    let lessons_file = lessons_file_from_selected(now_sec, selected, body);

    let lessons_path = agent_root.lessons_md();
    std::fs::create_dir_all(agent_root.semantic_dir())?;
    wal::guarded_replace(
        agent_root,
        &lessons_path,
        lessons::render_lessons_file(&lessons_file).as_bytes(),
        GuardedReplaceKind::ReplaceSemanticMemory,
    )?;

    // Pin/unpin rewrites JSONL under the same envelope, via
    // `episodic::rewrite_guarded` (opened by `run_filesystem_phases`).
    apply_pin_unpin(agent_root)?;

    Ok(())
}

/// Orchestrate the full dream cycle in `--no-llm` deterministic mode (DR-308).
///
/// Writes one `LESSONS.md` entry: the highest-salience exemplar from the
/// top promoted cluster (highest `salience_sum`). No network calls are made.
///
/// If **no** cluster is promoted, the cycle *retires* instead: any existing
/// `LESSONS.md` is unlinked (AILAB-699).
///
/// Byte-for-byte identical to the pre-AILAB-204 single-function version except
/// for the `citations` frontmatter key AILAB-200 added (always the exemplar id
/// on this path) — the dream-cycle snapshot fixtures pin that. The LLM path is
/// the same two steps with a different [`LessonBodySource`]; see
/// [`crate::dream_cycle::run_filesystem_phases`].
#[must_use = "dream cycle errors must be handled"]
pub fn run_deterministic_dream_cycle(
    agent_root: &AgentRoot,
    now_sec: i64,
) -> Result<(), DreamCycleError> {
    let _span = tracing::debug_span!("dream_cycle_deterministic", now_sec).entered();

    let Some(selected) = select_lesson_or_retire(agent_root, now_sec)? else {
        return Ok(());
    };
    write_selected_lesson(
        agent_root,
        now_sec,
        &selected,
        LessonBodySource::Deterministic,
    )
}

/// Pick the cluster exemplar: highest salience, then pain, then importance,
/// then reverse EventId string (deterministic total order when scores tie).
fn pick_exemplar(events: &[AgentLearning], now_sec: i64) -> &AgentLearning {
    let recurrence = RecurrenceContext::dream_cycle(events.len());
    events
        .iter()
        .max_by(|a, b| {
            let sa = salience_with_context(
                now_sec,
                a.timestamp.timestamp(),
                a.pain as f64,
                a.importance as f64,
                recurrence,
            );
            let sb = salience_with_context(
                now_sec,
                b.timestamp.timestamp(),
                b.pain as f64,
                b.importance as f64,
                recurrence,
            );
            sa.partial_cmp(&sb)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    a.pain
                        .partial_cmp(&b.pain)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| {
                    a.importance
                        .partial_cmp(&b.importance)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| b.id.as_str().cmp(a.id.as_str()))
        })
        .unwrap() // safe: events is non-empty (only promoted clusters reach here)
}

fn count_in_window(
    indices: &[usize],
    events: &[AgentLearning],
    now_sec: i64,
    window_sec: i64,
) -> usize {
    let cutoff = now_sec - window_sec;
    indices
        .iter()
        .filter(|&&i| events[i].timestamp.timestamp() >= cutoff)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use chrono::{DateTime, Utc};
    use dreamd_protocol::EventId;

    use crate::layout::AgentRoot;
    use crate::lessons::{read_lessons_file, write_lessons_file, Lesson, LessonsFile};
    use crate::test_support::{unique_tmpdir, DirGuard};
    // Production code here mints no intents any more (AILAB-164); the assertions
    // below still read them back off disk.
    use crate::wal::WalIntent;

    fn test_id(n: u32) -> EventId {
        EventId::parse(&format!("evt_{:0>26}", n)).unwrap()
    }

    fn fixed_ts() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-05-13T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn make_event(id: u32, skill_action: &str, pinned: bool) -> AgentLearning {
        AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: test_id(id),
            timestamp: fixed_ts(),
            pain: 5.0,
            importance: 5.0,
            pinned,
            skill_action: skill_action.to_string(),
            source_harness: "test".to_string(),
            content: format!("event {id}"),
        }
    }

    fn make_event_at(id: u32, skill_action: &str, ts: i64) -> AgentLearning {
        AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: test_id(id),
            timestamp: DateTime::from_timestamp(ts, 0).unwrap(),
            pain: 5.0,
            importance: 5.0,
            pinned: false,
            skill_action: skill_action.to_string(),
            source_harness: "test".to_string(),
            content: format!("event {id}"),
        }
    }

    fn write_jsonl(agent_root: &AgentRoot, events: &[AgentLearning]) {
        let jsonl_path = agent_root.episodic_jsonl();
        fs::create_dir_all(jsonl_path.parent().unwrap()).unwrap();
        let mut out = String::new();
        for e in events {
            out.push_str(&serde_json::to_string(e).unwrap());
            out.push('\n');
        }
        fs::write(&jsonl_path, out.as_bytes()).unwrap();
    }

    /// Fixed `now_sec` for deterministic salience: same as `fixed_ts` timestamp
    /// so age_days = 0.
    const NOW_SEC: i64 = 1747137600; // 2026-05-13T12:00:00Z

    // ── Legacy dotted-key corpora (ARCHITECTURE.md §9) ─────────────────────
    //
    // Pre-migration records may carry dotted keys (e.g. `rust.error_handling`).
    // The cluster engine splits on `::` only, so dotted keys are opaque
    // single-segment monoliths until `dreamd migrate` rewrites them.

    #[test]
    fn legacy_dotted_skill_action_clusters_as_opaque_monolith() {
        let dir = unique_tmpdir("legacy-dotted");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);
        let events: Vec<AgentLearning> = (0..3)
            .map(|i| make_event(i, "rust.error_handling", false))
            .collect();
        write_jsonl(&root, &events);

        let out = run_cluster_engine(&root, NOW_SEC).unwrap();
        assert_eq!(out.promoted.len(), 1);
        assert_eq!(out.promoted[0].cluster_key, "rust.error_handling");
        assert_eq!(out.promoted[0].events.len(), 3);
    }

    #[test]
    fn legacy_dotted_and_canonical_keys_do_not_merge() {
        let dir = unique_tmpdir("legacy-mixed");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);
        let mut events: Vec<AgentLearning> = (0..3)
            .map(|i| make_event(i, "rust::tokio", false))
            .collect();
        events.extend((3..6).map(|i| make_event(i, "rust.tokio", false)));
        write_jsonl(&root, &events);

        let out = run_cluster_engine(&root, NOW_SEC).unwrap();
        assert_eq!(out.promoted.len(), 2);
        let keys: Vec<&str> = out
            .promoted
            .iter()
            .map(|c| c.cluster_key.as_str())
            .collect();
        assert!(keys.contains(&"rust::tokio"));
        assert!(keys.contains(&"rust.tokio"));
    }

    #[test]
    fn legacy_dotted_key_does_not_participate_in_prefix_tree() {
        // A dotted key must not contribute prefixes to `::`-segmented siblings.
        let dir = unique_tmpdir("legacy-prefix");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);
        let mut events: Vec<AgentLearning> = (0..3)
            .map(|i| make_event(i, "rust::error_handling", false))
            .collect();
        events.push(make_event(3, "rust.error_handling", false));
        write_jsonl(&root, &events);

        let out = run_cluster_engine(&root, NOW_SEC).unwrap();
        let canonical = out
            .promoted
            .iter()
            .find(|c| c.cluster_key == "rust::error_handling")
            .expect("canonical cluster promoted");
        assert_eq!(canonical.events.len(), 3);
        assert!(
            !out.promoted
                .iter()
                .any(|c| c.cluster_key == "rust.error_handling"),
            "single dotted event must not reach promotion threshold"
        );
    }

    #[test]
    fn cluster_engine_empty_jsonl_returns_no_promoted() {
        let dir = unique_tmpdir("empty");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);
        let out = run_cluster_engine(&root, NOW_SEC).unwrap();
        assert!(out.promoted.is_empty());
    }

    #[test]
    fn prefix_tree_all_depths_populated() {
        // 3 events at a 3-segment skill_action. If the tree is only built to
        // depth 1, the engine would promote "a" instead of "a::b::c".
        let dir = unique_tmpdir("depths");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);
        let events: Vec<AgentLearning> = (0..3).map(|i| make_event(i, "a::b::c", false)).collect();
        write_jsonl(&root, &events);

        let out = run_cluster_engine(&root, NOW_SEC).unwrap();
        assert_eq!(out.promoted.len(), 1);
        assert_eq!(out.promoted[0].cluster_key, "a::b::c");
    }

    #[test]
    fn deepest_wins_assigns_event_to_leaf_not_root() {
        // 3 events with skill_action "rust::eh::unwrap" (leaf) AND 3 events
        // with skill_action "rust" (root). The leaf events must land in the
        // leaf cluster, not in "rust".
        let dir = unique_tmpdir("deepest");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);
        let mut events: Vec<AgentLearning> = (0..3)
            .map(|i| make_event(i, "rust::eh::unwrap", false))
            .collect();
        events.extend((3..6).map(|i| make_event(i, "rust", false)));
        write_jsonl(&root, &events);

        let out = run_cluster_engine(&root, NOW_SEC).unwrap();
        // Both clusters qualify: leaf gets 3 leaf-events; "rust" gets its
        // 3 direct-events (the 3 leaf-events were already claimed by the leaf).
        let leaf = out
            .promoted
            .iter()
            .find(|c| c.cluster_key == "rust::eh::unwrap")
            .expect("leaf cluster promoted");
        assert_eq!(leaf.events.len(), 3);
        for e in &leaf.events {
            assert_eq!(e.skill_action, "rust::eh::unwrap");
        }
        let root_cluster = out
            .promoted
            .iter()
            .find(|c| c.cluster_key == "rust")
            .expect("root cluster promoted");
        assert_eq!(root_cluster.events.len(), 3);
        for e in &root_cluster.events {
            assert_eq!(e.skill_action, "rust");
        }
    }

    #[test]
    fn cluster_below_threshold_not_promoted() {
        let dir = unique_tmpdir("below");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);
        let events: Vec<AgentLearning> = (0..2)
            .map(|i| make_event(i, "rust::types", false))
            .collect();
        write_jsonl(&root, &events);

        let out = run_cluster_engine(&root, NOW_SEC).unwrap();
        assert!(out.promoted.is_empty());
    }

    #[test]
    fn cluster_at_threshold_promoted() {
        let dir = unique_tmpdir("threshold");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);
        let events: Vec<AgentLearning> = (0..PROMOTION_THRESHOLD)
            .map(|i| make_event(i as u32, "rust::types", false))
            .collect();
        write_jsonl(&root, &events);

        let out = run_cluster_engine(&root, NOW_SEC).unwrap();
        assert_eq!(out.promoted.len(), 1);
        assert_eq!(out.promoted[0].cluster_key, "rust::types");
        assert_eq!(out.promoted[0].events.len(), PROMOTION_THRESHOLD);
    }

    #[test]
    fn sidecar_written_to_semantic_dir() {
        let dir = unique_tmpdir("sidecar");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);
        let events: Vec<AgentLearning> = (0..PROMOTION_THRESHOLD)
            .map(|i| make_event(i as u32, "go::testing", false))
            .collect();
        write_jsonl(&root, &events);

        run_cluster_engine(&root, NOW_SEC).unwrap();

        let sidecar_path = root.semantic_dir().join("recurrence_counts.json");
        assert!(sidecar_path.exists(), "sidecar file must exist");
        let sidecar: RecurrenceSidecar =
            serde_json::from_slice(&fs::read(&sidecar_path).unwrap()).unwrap();
        assert_eq!(sidecar.schema_version, "1.0");
        assert_eq!(sidecar.clusters.len(), 1);
        assert_eq!(sidecar.clusters[0].skill_action, "go::testing");
        assert_eq!(sidecar.clusters[0].count, PROMOTION_THRESHOLD as u32);
    }

    #[test]
    fn apply_pin_unpin_unions_external_pins_with_cited() {
        let dir = unique_tmpdir("pinunpin");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);

        // Write 4 events: ids 0,1 are pinned=true (external, uncited); ids 2,3
        // are pinned=false. LESSONS.md cites ids 2,3. Under union semantics the
        // external pins on 0,1 SURVIVE and cited 2,3 get pinned → all four true.
        let events = vec![
            make_event(0, "rust::types", true),
            make_event(1, "rust::types", true),
            make_event(2, "rust::types", false),
            make_event(3, "rust::types", false),
        ];
        write_jsonl(&root, &events);

        let lessons_path = root.lessons_md();
        fs::create_dir_all(lessons_path.parent().unwrap()).unwrap();
        write_lessons_file(
            &lessons_path,
            &LessonsFile {
                last_updated: fixed_ts(),
                prompt_version: "dream-cycle/v1.1@2026-05-13".to_string(),
                cluster_key: "rust::types".to_string(),
                citations: vec![],
                lessons: vec![
                    Lesson {
                        id: test_id(2).as_str().to_string(),
                        content: "lesson 2".to_string(),
                        pinned: false,
                    },
                    Lesson {
                        id: test_id(3).as_str().to_string(),
                        content: "lesson 3".to_string(),
                        pinned: false,
                    },
                ],
            },
        )
        .unwrap();

        apply_pin_unpin(&root).unwrap();

        let updated = episodic::read_all(&root.episodic_jsonl()).unwrap();
        assert_eq!(updated.len(), 4);
        let find = |n: u32| {
            updated
                .iter()
                .find(|e| e.id.as_str() == test_id(n).as_str())
                .unwrap()
                .pinned
        };
        assert!(find(0), "id 0: external pin, uncited → union preserves it");
        assert!(find(1), "id 1: external pin, uncited → union preserves it");
        assert!(find(2), "id 2: cited in LESSONS.md → must be true");
        assert!(find(3), "id 3: cited in LESSONS.md → must be true");
    }

    /// AILAB-200: the pin pass reads `Lesson.id` **and** the frontmatter
    /// `citations`. A cluster member the model cited but that is not the
    /// exemplar must survive decay, or the lesson outlives its evidence.
    #[test]
    fn apply_pin_unpin_unions_lesson_ids_with_frontmatter_citations() {
        let dir = unique_tmpdir("pincitations");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);

        // 0 is the exemplar (the `Lesson.id`), 1 is a second cluster member the
        // model cited, 2 is an uncited member. Only 0 and 1 may end up pinned.
        let events = vec![
            make_event(0, "rust::types", false),
            make_event(1, "rust::types", false),
            make_event(2, "rust::types", false),
        ];
        write_jsonl(&root, &events);

        let lessons_path = root.lessons_md();
        fs::create_dir_all(lessons_path.parent().unwrap()).unwrap();
        write_lessons_file(
            &lessons_path,
            &LessonsFile {
                last_updated: fixed_ts(),
                prompt_version: "dream-cycle/v1.2@2026-08-23".to_string(),
                cluster_key: "rust::types".to_string(),
                citations: vec![
                    test_id(0).as_str().to_string(),
                    test_id(1).as_str().to_string(),
                ],
                lessons: vec![Lesson {
                    id: test_id(0).as_str().to_string(),
                    content: "composed prose".to_string(),
                    pinned: false,
                }],
            },
        )
        .unwrap();

        apply_pin_unpin(&root).unwrap();

        let updated = episodic::read_all(&root.episodic_jsonl()).unwrap();
        let find = |n: u32| {
            updated
                .iter()
                .find(|e| e.id.as_str() == test_id(n).as_str())
                .unwrap()
                .pinned
        };
        assert!(find(0), "id 0: the exemplar `Lesson.id` → pinned");
        assert!(
            find(1),
            "id 1: cited in frontmatter but not the exemplar → the union must \
             still pin it; this is the whole point of AILAB-200"
        );
        assert!(
            !find(2),
            "id 2: an uncited cluster member must NOT be pinned — the union is \
             over what the lesson claims, not over the cluster"
        );
    }

    #[test]
    fn apply_pin_unpin_no_lessons_md_preserves_external_pins() {
        let dir = unique_tmpdir("nopins");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);

        let events = vec![
            make_event(0, "rust::types", true),
            make_event(1, "rust::types", true),
        ];
        write_jsonl(&root, &events);

        // LESSONS.md is absent — cited_ids is empty, so union preserves the
        // external pins rather than clearing them (SPEC §67 / WEG-426).
        apply_pin_unpin(&root).unwrap();

        let updated = episodic::read_all(&root.episodic_jsonl()).unwrap();
        assert_eq!(updated.len(), 2);
        assert!(
            updated[0].pinned,
            "id 0: no LESSONS.md, but external pin survives union"
        );
        assert!(
            updated[1].pinned,
            "id 1: no LESSONS.md, but external pin survives union"
        );
    }

    #[test]
    fn cluster_3_events_within_7_days_promotes() {
        let dir = unique_tmpdir("win7");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);
        // 3 events 1, 2, 5 days ago — all within the 7-day window.
        let events = vec![
            make_event_at(0, "rust::borrow", NOW_SEC - 86400),
            make_event_at(1, "rust::borrow", NOW_SEC - 2 * 86400),
            make_event_at(2, "rust::borrow", NOW_SEC - 5 * 86400),
        ];
        write_jsonl(&root, &events);

        let out = run_cluster_engine(&root, NOW_SEC).unwrap();
        assert_eq!(out.promoted.len(), 1);
        assert_eq!(out.promoted[0].cluster_key, "rust::borrow");
    }

    #[test]
    fn cluster_3_events_in_30_days_promotes() {
        let dir = unique_tmpdir("win30");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);
        // 3 events at 8, 15, 25 days ago — outside 7-day window but inside 30-day.
        let events = vec![
            make_event_at(0, "go::testing", NOW_SEC - 8 * 86400),
            make_event_at(1, "go::testing", NOW_SEC - 15 * 86400),
            make_event_at(2, "go::testing", NOW_SEC - 25 * 86400),
        ];
        write_jsonl(&root, &events);

        let out = run_cluster_engine(&root, NOW_SEC).unwrap();
        assert_eq!(out.promoted.len(), 1);
        assert_eq!(out.promoted[0].cluster_key, "go::testing");
    }

    #[test]
    fn cluster_2_events_in_7_days_does_not_promote() {
        let dir = unique_tmpdir("noprom");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);
        // 2 events within 7 days + 1 beyond 30 days = neither window hits threshold.
        let events = vec![
            make_event_at(0, "py::async", NOW_SEC - 86400),
            make_event_at(1, "py::async", NOW_SEC - 3 * 86400),
            make_event_at(2, "py::async", NOW_SEC - 31 * 86400),
        ];
        write_jsonl(&root, &events);

        let out = run_cluster_engine(&root, NOW_SEC).unwrap();
        assert!(out.promoted.is_empty());
    }

    // --- WEG-61: run_deterministic_dream_cycle tests ---

    #[test]
    fn deterministic_cycle_empty_jsonl_no_lessons_md() {
        let dir = unique_tmpdir("dc-empty");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);
        // Scaffold a real (empty) `.agent/` store. Previously this test relied
        // on begin_cycle's create_dir_all to conjure `.agent/`; WEG-281 makes
        // begin_cycle refuse a missing store, so the test sets one up — empty
        // JSONL still exercises the no-promotion path.
        write_jsonl(&root, &[]);
        run_deterministic_dream_cycle(&root, NOW_SEC).unwrap();
        assert!(!root.lessons_md().exists());
    }

    // --- AILAB-341: the read-only preview twin ---

    /// The write path unlinks a stale `LESSONS.md` when nothing promotes
    /// (AILAB-699). `preview_select_lesson` must reach the same *decision* —
    /// `None` — while leaving the file exactly where it was, because `--dry`
    /// writes nothing and an unlink is a write.
    #[test]
    fn preview_select_lesson_reports_none_without_unlinking_lessons_md() {
        let dir = unique_tmpdir("preview-no-unlink");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);

        // Events far enough in the past that neither window promotes them.
        let stale = NOW_SEC - WINDOW_30_DAYS_SEC - 1;
        let events: Vec<AgentLearning> = (0..PROMOTION_THRESHOLD)
            .map(|i| make_event_at(i as u32, "rust::types", stale))
            .collect();
        write_jsonl(&root, &events);

        // A LESSONS.md an earlier promoting cycle left behind.
        fs::create_dir_all(root.semantic_dir()).unwrap();
        let existing = LessonsFile {
            last_updated: fixed_ts(),
            prompt_version: "deterministic-only".to_string(),
            cluster_key: "rust::types".to_string(),
            citations: vec![],
            lessons: vec![Lesson {
                id: "evt_00000000000000000000000000".to_string(),
                content: "stale lesson".to_string(),
                pinned: false,
            }],
        };
        write_lessons_file(&root.lessons_md(), &existing).unwrap();
        let before = fs::read(root.lessons_md()).unwrap();

        assert!(
            preview_select_lesson(&root, NOW_SEC).unwrap().is_none(),
            "no cluster promotes, so the preview reports `would retire`"
        );
        assert_eq!(
            fs::read(root.lessons_md()).unwrap(),
            before,
            "the preview must leave the stale LESSONS.md byte-identical"
        );
        assert!(
            !root.semantic_dir().join("recurrence_counts.json").exists(),
            "the preview must not write the recurrence sidecar"
        );
        assert!(!root.wal_path().exists(), "the preview must not open a WAL");
    }

    /// A promoting store: the preview selects the same cluster and exemplar the
    /// write path would, and still touches nothing.
    #[test]
    fn preview_select_lesson_matches_the_write_path_selection() {
        let dir = unique_tmpdir("preview-selects");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);

        let events: Vec<AgentLearning> = (0..PROMOTION_THRESHOLD)
            .map(|i| make_event_at(i as u32, "rust::types", NOW_SEC))
            .collect();
        write_jsonl(&root, &events);
        let jsonl_before = fs::read(root.episodic_jsonl()).unwrap();

        let previewed = preview_select_lesson(&root, NOW_SEC)
            .unwrap()
            .expect("a cluster promotes");

        assert!(
            !root.semantic_dir().join("recurrence_counts.json").exists(),
            "the preview must not write the recurrence sidecar"
        );
        assert_eq!(
            fs::read(root.episodic_jsonl()).unwrap(),
            jsonl_before,
            "the preview must not rewrite the episodic log"
        );

        // Now let the write path decide, and compare.
        let written = select_lesson_or_retire(&root, NOW_SEC)
            .unwrap()
            .expect("a cluster promotes");
        assert_eq!(previewed.cluster_key, written.cluster_key);
        assert_eq!(previewed.exemplar_id, written.exemplar_id);
        assert_eq!(previewed.exemplar_content, written.exemplar_content);
    }

    // --- AILAB-699: a cycle that promotes nothing retires LESSONS.md ---

    #[test]
    fn deterministic_cycle_no_promotion_retires_lessons_md_and_keeps_pins() {
        let dir = unique_tmpdir("dc-retire");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);

        // Cycle 1: PROMOTION_THRESHOLD events at NOW_SEC — inside the 7-day
        // window, so the cluster promotes and a lesson is written.
        let events: Vec<AgentLearning> = (0..PROMOTION_THRESHOLD)
            .map(|i| make_event_at(i as u32, "rust::types", NOW_SEC))
            .collect();
        write_jsonl(&root, &events);

        run_deterministic_dream_cycle(&root, NOW_SEC).unwrap();

        assert!(
            root.lessons_md().exists(),
            "a promoting cycle must write LESSONS.md"
        );
        let exemplar_id = read_lessons_file(&root.lessons_md()).unwrap().lessons[0]
            .id
            .clone();
        let pinned_after_promote = |id: &str| {
            episodic::read_all(&root.episodic_jsonl())
                .unwrap()
                .into_iter()
                .find(|e| e.id.as_str() == id)
                .expect("exemplar is in the log")
                .pinned
        };
        assert!(
            pinned_after_promote(&exemplar_id),
            "apply_pin_unpin must pin the cited exemplar on the promoting cycle"
        );

        // Cycle 2: one later run whose `now_sec` is past the 30-day window. The
        // same events now fall outside BOTH windows (`count_in_window` keeps
        // `ts >= now_sec - window`), so nothing promotes. No hysteresis, no
        // second no-promotion cycle needed.
        let later = NOW_SEC + WINDOW_30_DAYS_SEC + 1;
        run_deterministic_dream_cycle(&root, later).unwrap();

        assert!(
            !root.lessons_md().exists(),
            "a no-promotion cycle must unlink the stale LESSONS.md, not leave \
             it (AILAB-699) and not replace it with an empty frontmatter file"
        );
        assert!(
            pinned_after_promote(&exemplar_id),
            "retirement removes the derived doc, never the pin on its source \
             event — the retire path must not call apply_pin_unpin or clear \
             `pinned` (option 3 is out of scope)"
        );

        // Idempotent: a second no-promotion cycle over the same input is a
        // no-op, not an error on the already-absent file.
        run_deterministic_dream_cycle(&root, later).unwrap();
        assert!(!root.lessons_md().exists());
    }

    #[test]
    fn deterministic_cycle_single_cluster_highest_salience_exemplar() {
        let dir = unique_tmpdir("dc-single");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);
        // Events with distinct pain values; all age_days=0 so salience ∝ pain.
        // Event 2 has the highest pain → highest salience → must be the exemplar.
        let events = vec![
            {
                let mut e = make_event(0, "rust::types", false);
                e.pain = 3.0;
                e
            },
            {
                let mut e = make_event(1, "rust::types", false);
                e.pain = 5.0;
                e
            },
            {
                let mut e = make_event(2, "rust::types", false);
                e.pain = 8.0;
                e
            },
        ];
        write_jsonl(&root, &events);

        run_deterministic_dream_cycle(&root, NOW_SEC).unwrap();

        assert!(root.lessons_md().exists());
        let lf = read_lessons_file(&root.lessons_md()).unwrap();
        assert_eq!(lf.lessons.len(), 1);
        assert_eq!(lf.prompt_version, "deterministic-only");
        assert_eq!(lf.lessons[0].id, test_id(2).as_str().to_string());
    }

    #[test]
    fn deterministic_cycle_tiebreak_lowest_id_wins() {
        let dir = unique_tmpdir("dc-tie");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);
        // Identical pain, importance, timestamp → salience tie on all three.
        // Tie-break 3: lowest id lexicographically. test_id(5) < test_id(10) < test_id(20).
        let events = vec![
            make_event(10, "rust::types", false),
            make_event(5, "rust::types", false),
            make_event(20, "rust::types", false),
        ];
        write_jsonl(&root, &events);

        run_deterministic_dream_cycle(&root, NOW_SEC).unwrap();

        let lf = read_lessons_file(&root.lessons_md()).unwrap();
        assert_eq!(lf.lessons[0].id, test_id(5).as_str().to_string());
    }

    #[test]
    fn deterministic_cycle_pin_unpin_recorded_in_wal_before_commit() {
        let dir = unique_tmpdir("dc-wal-pin");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);
        let events: Vec<AgentLearning> = (0..PROMOTION_THRESHOLD)
            .map(|i| make_event(i as u32, "rust::types", false))
            .collect();
        write_jsonl(&root, &events);

        wal::begin_cycle(&root, NOW_SEC).unwrap();
        let cluster_output = run_cluster_engine(&root, NOW_SEC).unwrap();
        assert!(!cluster_output.promoted.is_empty());

        let top_cluster = cluster_output
            .promoted
            .iter()
            .max_by(|a, b| {
                a.salience_sum
                    .partial_cmp(&b.salience_sum)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap();
        let exemplar = pick_exemplar(&top_cluster.events, NOW_SEC);
        let lessons = vec![Lesson {
            id: exemplar.id.as_str().to_string(),
            content: exemplar.content.clone(),
            pinned: false,
        }];

        let lessons_path = root.lessons_md();
        let last_updated = DateTime::<Utc>::from_timestamp(NOW_SEC, 0).unwrap_or_default();
        let lessons_file = LessonsFile {
            last_updated,
            prompt_version: "deterministic-only".to_string(),
            cluster_key: top_cluster.cluster_key.clone(),
            citations: vec![],
            lessons,
        };
        fs::create_dir_all(root.semantic_dir()).unwrap();
        // The lessons write records its own intent now — the test no longer
        // stands in for the production path by minting one by hand.
        wal::guarded_replace(
            &root,
            &lessons_path,
            lessons::render_lessons_file(&lessons_file).as_bytes(),
            GuardedReplaceKind::ReplaceSemanticMemory,
        )
        .unwrap();

        apply_pin_unpin(&root).unwrap();

        let wal: crate::wal::DreamWal =
            serde_json::from_str(&fs::read_to_string(root.wal_path()).unwrap()).unwrap();
        assert!(
            wal.intents
                .iter()
                .any(|i| matches!(i, WalIntent::ReplaceSemanticMemory { .. })),
            "the lessons write must record ReplaceSemanticMemory while WAL is active"
        );
        assert!(
            wal.intents
                .iter()
                .any(|i| matches!(i, WalIntent::PruneEpisodicMemory { .. })),
            "pin/unpin must append PruneEpisodicMemory while WAL is active"
        );
        assert!(
            !wal.intents.contains(&WalIntent::Commit),
            "commit must not precede pin/unpin"
        );

        wal::commit_cycle(&root, NOW_SEC).unwrap();
        assert!(!root.wal_path().exists());
    }

    #[test]
    fn recover_mid_pin_unpin_preserves_jsonl_when_tmp_survives() {
        let dir = unique_tmpdir("recover-pin");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);
        fs::create_dir_all(root.dreamd_dir()).unwrap();

        let events = vec![
            make_event(0, "rust::types", true),
            make_event(1, "rust::types", false),
            make_event(2, "rust::types", false),
        ];
        write_jsonl(&root, &events);
        let original_bytes = fs::read(root.episodic_jsonl()).unwrap();

        let lessons_path = root.lessons_md();
        fs::create_dir_all(lessons_path.parent().unwrap()).unwrap();

        wal::begin_cycle(&root, NOW_SEC).unwrap();

        // Step one of the cycle, run for real: this lands `ReplaceSemanticMemory`
        // in the WAL and renames its temp away. Recovery must therefore walk TWO
        // intents, one of whose temps is already gone — the multi-intent shape
        // the hand-written WAL fixture used to stand in for.
        let lessons_file = LessonsFile {
            last_updated: fixed_ts(),
            prompt_version: "deterministic-only".to_string(),
            cluster_key: "rust::types".to_string(),
            citations: vec![],
            lessons: vec![Lesson {
                id: test_id(2).as_str().to_string(),
                content: "exemplar".to_string(),
                pinned: false,
            }],
        };
        wal::guarded_replace(
            &root,
            &lessons_path,
            lessons::render_lessons_file(&lessons_file).as_bytes(),
            GuardedReplaceKind::ReplaceSemanticMemory,
        )
        .unwrap();
        let lessons_bytes = fs::read(&lessons_path).unwrap();

        // Crash the real pin rewrite in the window recovery exists for: the
        // temp is fsynced and the intent is durable, the rename never happened.
        // Driving the seam is the point — a hand-written WAL fixture would only
        // exercise the recovery reader against bytes production never emits.
        let mut pinned = events.clone();
        pinned[2].pinned = true;
        let pinned_bytes: String = pinned
            .iter()
            .map(|e| format!("{}\n", serde_json::to_string(e).unwrap()))
            .collect();
        crate::wal::guarded_replace_crash_after_intent(
            &root,
            &root.episodic_jsonl(),
            pinned_bytes.as_bytes(),
            GuardedReplaceKind::PruneEpisodicMemory,
        )
        .expect_err("injected crash must surface");

        let jsonl_tmp = root.episodic_jsonl().with_extension("tmp");
        assert!(
            jsonl_tmp.exists(),
            "the pin rewrite's .tmp must survive the crash as the recovery signal"
        );
        let wal_json = fs::read_to_string(root.wal_path()).unwrap();
        assert!(
            wal_json.contains("PruneEpisodicMemory"),
            "the crashed rewrite must have left its intent in the WAL"
        );
        assert!(
            wal_json.contains("ReplaceSemanticMemory"),
            "the completed lessons write must still be in the same WAL"
        );

        let outcome = wal::recover_if_needed(&root, NOW_SEC).unwrap();
        assert!(
            matches!(outcome, crate::wal::RecoveryOutcome::Recovered { .. }),
            "incomplete pin/unpin cycle must be recoverable"
        );
        assert!(
            !jsonl_tmp.exists(),
            ".tmp from pin/unpin must be cleaned up"
        );
        assert_eq!(
            fs::read(root.episodic_jsonl()).unwrap(),
            original_bytes,
            "JSONL must be unchanged when pin/unpin rename never completed"
        );

        let updated = episodic::read_all(&root.episodic_jsonl()).unwrap();
        assert!(
            updated[0].pinned,
            "pre-cycle pin state must survive recovery"
        );
        assert!(!updated[2].pinned, "partial rewrite must not have landed");

        // The committed half's temp was renamed away before the crash, so
        // recovery walks an intent whose temp is already gone and leaves the
        // promoted file alone rather than erroring on the missing path.
        assert_eq!(
            fs::read(&lessons_path).unwrap(),
            lessons_bytes,
            "an intent whose temp already renamed must not disturb the live file"
        );
    }

    #[test]
    fn deterministic_cycle_exemplar_pinned_non_exemplars_unpinned() {
        let dir = unique_tmpdir("dc-pin");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);
        // Event 2 has highest pain → exemplar → pinned; events 0 and 1 → not pinned.
        let events = vec![
            {
                let mut e = make_event(0, "rust::types", false);
                e.pain = 3.0;
                e
            },
            {
                let mut e = make_event(1, "rust::types", false);
                e.pain = 5.0;
                e
            },
            {
                let mut e = make_event(2, "rust::types", false);
                e.pain = 8.0;
                e
            },
        ];
        write_jsonl(&root, &events);

        run_deterministic_dream_cycle(&root, NOW_SEC).unwrap();

        let updated = episodic::read_all(&root.episodic_jsonl()).unwrap();
        assert_eq!(updated.len(), 3);
        let pinned = |n: u32| {
            updated
                .iter()
                .find(|e| e.id.as_str() == test_id(n).as_str())
                .unwrap()
                .pinned
        };
        assert!(pinned(2), "exemplar (id 2) must be pinned");
        assert!(!pinned(0), "non-exemplar (id 0) must not be pinned");
        assert!(!pinned(1), "non-exemplar (id 1) must not be pinned");
    }

    #[test]
    fn cluster_output_default_is_empty() {
        let output = ClusterOutput::default();
        assert!(output.promoted.is_empty());
    }
}
