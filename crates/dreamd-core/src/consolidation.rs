//! Cluster engine and the pin rule (WEG-58).
//!
//! `run_cluster_engine` reads the episodic JSONL, builds a prefix tree over
//! `skill_action` (split on `::`), applies deepest-wins disambiguation, and
//! writes `semantic/recurrence_counts.json`. A dream cycle computes the same
//! counts inside [`crate::plan::plan_cycle`] ([`compute_promoted_clusters`],
//! `sidecar_bytes`) and stages them with the cycle's other outputs.
//!
//! [`cited_pins`] is the pin rule: every unpinned record a `LESSONS.md` cites
//! is pinned, **unioned** with any `pinned` flag already set. The cycle never
//! clears a pin, whoever set it (SPEC.md "Node schema (episodic)", the
//! `pinned` field; WEG-426). [`crate::plan::plan_cycle`] applies it to the lesson it plans,
//! and pins nothing when no lesson is written.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Utc};
use dreamd_protocol::{AgentLearning, EventId};

use crate::episodic::{self, EpisodicError, RawLine};
use crate::index::{ClusterCount, RecurrenceSidecar};
use crate::io::write_atomic;
use crate::layout::AgentRoot;
use crate::lessons::{Lesson, LessonsFile};
use crate::provenance::Ledger;
use crate::salience::{salience_with_context, RecurrenceContext};
use crate::wal::{self, CycleKind, WalError};

/// Errors from the cluster engine and from planning a cycle's log, sidecar
/// and lessons.
#[derive(Debug, thiserror::Error)]
pub enum ConsolidationError {
    /// Reading the log or writing the sidecar failed, or a planned
    /// `LESSONS.md` did not parse back after rendering (`InvalidData`).
    #[error("reading AGENT_LEARNINGS.jsonl: {0}")]
    Io(#[from] std::io::Error),
    /// Serializing the recurrence sidecar failed, or `episodic::patch_lines`
    /// could not serialize a line it flips. `line` is always 0: neither path
    /// knows a line number, so the message reads "line 0". Reading the log
    /// never produces this; readers skip bad lines (WEG-378).
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
    /// Promoted clusters, sorted by `cluster_key`. Deepest-wins claiming puts
    /// each event in exactly one cluster.
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
///
/// This writes the live sidecar directly, for tests and doctor's fixtures. A
/// dream cycle and `forget::cascade` plan the sidecar instead
/// ([`crate::plan`]) and stage it.
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
    let sidecar_path = agent_root.semantic_dir().join("recurrence_counts.json");
    std::fs::create_dir_all(agent_root.semantic_dir())?;
    write_atomic(&sidecar_path, &sidecar_bytes(&promoted)?)?;

    Ok(ClusterOutput { promoted })
}

/// The `recurrence_counts.json` bytes for `promoted`: one count per promoted
/// cluster. Shared by [`run_cluster_engine`] and [`crate::plan`].
pub(crate) fn sidecar_bytes(promoted: &[PromotedCluster]) -> Result<Vec<u8>, ConsolidationError> {
    let sidecar = RecurrenceSidecar {
        schema_version: "1.0".to_string(),
        clusters: promoted
            .iter()
            .map(|c| ClusterCount {
                skill_action: c.cluster_key.clone(),
                count: c.events.len() as u32,
            })
            .collect(),
    };
    serde_json::to_string_pretty(&sidecar)
        .map(String::into_bytes)
        .map_err(|e| ConsolidationError::Json { line: 0, source: e })
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
    // Step 2: build prefix tree. Each event contributes to every prefix of
    // its skill_action. The tree holds event indices, so nothing is cloned
    // until the end.
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

/// The pins `file` asks for: every record in `raw` that is not pinned and
/// whose id is a [`Lesson::id`] or in the file-level `citations` (AILAB-200)
/// maps to `true`.
///
/// Union only: the result never holds `false`, so a cycle can pin an event it
/// cites but never clears a pin (SPEC.md `pinned` field; WEG-426). A record
/// that is already pinned is left out, so a file that flips nothing gives an
/// empty map and the cycle leaves the log alone.
#[must_use]
pub fn cited_pins(raw: &[RawLine], file: &LessonsFile) -> HashMap<EventId, bool> {
    // Union, not either-or (AILAB-200). `Lesson.id` is the exemplar the lesson
    // is filed under; `citations` is every cluster member the model said it
    // drew on. Both must outlive decay or the lesson outlives its evidence. A
    // file with no `citations` key (written before AILAB-200) gives the
    // exemplar-only set.
    let cited_ids: HashSet<&str> = file
        .lessons
        .iter()
        .map(|l| l.id.as_str())
        .chain(file.citations.iter().map(String::as_str))
        .collect();
    raw.iter()
        .filter_map(|line| line.parsed.as_ref())
        .filter(|event| !event.pinned && cited_ids.contains(event.id.as_str()))
        .map(|event| (event.id.clone(), true))
        .collect()
}

/// Error type for the consolidation (lesson) phase of a dream cycle.
///
/// Named apart from [`crate::dream_cycle::DreamCycleError`], which wraps it as
/// `Consolidation(..)` (BZR-147 / BZR-172).
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum LessonPhaseError {
    /// Clustering or planning the cycle failed.
    #[error("cluster engine: {0}")]
    Cluster(#[from] ConsolidationError),
    /// Beginning, staging or committing the cycle failed. Reads through the
    /// stage, `LESSONS.md` included, report their I/O errors here.
    #[error("WAL: {0}")]
    Wal(#[from] WalError),
    /// An I/O error on `LESSONS.md` outside the stage. No path in this crate
    /// returns it.
    #[error("lessons write: {0}")]
    Lessons(#[from] std::io::Error),
}

/// `prompt_version` written when the lesson body is the exemplar's own text.
pub const DETERMINISTIC_PROMPT_ID: &str = "deterministic-only";

/// Where a lesson body came from (AILAB-204).
///
/// Only the body text and the frontmatter `prompt_version` differ between the
/// two — the planned write, the `cluster_key`, the pins, and above all the
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
/// Produced by [`select_lesson`]. The dream cycle selects once for the compose
/// (the model prompt), then [`crate::plan::plan_cycle_with`] selects again over
/// the log as of the commit, and `dream_cycle::gate_at_commit` checks the
/// composed body against that selection (BZR-1241). Records that landed during
/// the model call can change the selection; the gate then writes the
/// deterministic body for the commit's own selection.
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

/// Select the lesson for `promoted`: the top cluster by `salience_sum`, then
/// its exemplar (`pick_exemplar`).
///
/// `None` means **nothing promoted**. Pure: it reads nothing and stages
/// nothing. On a real cycle [`crate::plan::plan_cycle`] turns `None` into the
/// retirement of any existing `LESSONS.md` (AILAB-699).
#[must_use]
pub fn select_lesson(promoted: &[PromotedCluster], now_sec: i64) -> Option<SelectedLesson> {
    let top_cluster = promoted.iter().max_by(|a, b| {
        a.salience_sum
            .partial_cmp(&b.salience_sum)
            .unwrap_or(std::cmp::Ordering::Equal)
    })?;

    let exemplar = pick_exemplar(&top_cluster.events, now_sec);

    Some(SelectedLesson {
        cluster_key: top_cluster.cluster_key.clone(),
        exemplar_id: exemplar.id.as_str().to_string(),
        exemplar_content: exemplar.content.clone(),
        events: top_cluster.events.clone(),
    })
}

/// Cluster + pick exemplar with no sidecar write and no `LESSONS.md` unlink.
///
/// The read-only selection for `dreamd dream --dry` (AILAB-341). Same
/// promotion rules — it calls the same [`compute_promoted_clusters`] and the
/// same [`select_lesson`] the cycle's plan does — and it reaches disk exactly
/// once, to read the episodic JSONL.
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
) -> Result<Option<SelectedLesson>, LessonPhaseError> {
    let events =
        episodic::read_all(&agent_root.episodic_jsonl()).map_err(ConsolidationError::from)?;
    let promoted = compute_promoted_clusters(&events, now_sec);
    Ok(select_lesson(&promoted, now_sec))
}

/// Build the [`LessonsFile`] `selected` + `body` resolve to.
///
/// The single owner of the frontmatter shape and of the empty-LLM-body
/// degradation rule. [`crate::plan::plan_cycle`] plans what this returns and
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
        // exemplar here rather than an empty list means a `--no-llm` cycle pins
        // exactly its exemplar, and an LLM fallback is byte-identical to a
        // deterministic cycle.
        _ => (
            selected.exemplar_content.clone(),
            DETERMINISTIC_PROMPT_ID.to_string(),
            vec![selected.exemplar_id.clone()],
        ),
    };

    let lessons = vec![Lesson {
        // Always the exemplar id — never minted here. Semantic indexing
        // inherits pain/importance from this event (AILAB-204).
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

/// Orchestrate the full dream cycle in `--no-llm` deterministic mode.
///
/// Writes one `LESSONS.md` entry: the highest-salience exemplar from the
/// top promoted cluster (highest `salience_sum`). No network calls are made.
///
/// If **no** cluster is promoted, the cycle *retires* instead: any existing
/// `LESSONS.md` is unlinked (AILAB-699).
///
/// It reads the log and `LESSONS.md` once, plans every output with
/// [`crate::plan::plan_cycle`], stages the plan and commits it: the same plan
/// as [`crate::dream_cycle::run_filesystem_phases`] with the deterministic
/// body. So it decays too (BZR-1240), but it does not autosnap. It has no
/// production caller.
///
/// One stage, one commit: `ledger` appends the plan's provenance edges.
#[must_use = "dream cycle errors must be handled"]
pub fn run_deterministic_dream_cycle(
    agent_root: &AgentRoot,
    ledger: &Ledger,
    now_sec: i64,
) -> Result<(), LessonPhaseError> {
    let _span = tracing::debug_span!("dream_cycle_deterministic", now_sec).entered();

    let mut stage = wal::begin_cycle(agent_root, CycleKind::Dream, now_sec)?;
    let log = stage
        .read(&agent_root.episodic_jsonl())
        .map_err(WalError::Io)?;
    let raw = episodic::raw_lines(&log.unwrap_or_default());
    let prior = stage.read(&agent_root.lessons_md()).map_err(WalError::Io)?;
    let plan = crate::plan::plan_cycle(
        &raw,
        prior.as_deref(),
        now_sec,
        LessonBodySource::Deterministic,
    )?;
    stage
        .apply_plan(&plan, &crate::dream_cycle::cycle_date_from_now_sec(now_sec))
        .map_err(WalError::Io)?;
    wal::commit_cycle(stage, ledger, now_sec)?;
    Ok(())
}

/// Pick the cluster exemplar: highest salience, then pain, then importance,
/// then the smallest `EventId` (the oldest event), so a tie has one answer.
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

    fn ledger(root: &AgentRoot) -> Ledger {
        Ledger::open(root.provenance_ledger())
    }

    /// The log's lines for `events`, one serialized record per line.
    fn raw_of(events: &[AgentLearning]) -> Vec<RawLine> {
        let bytes: Vec<u8> = events
            .iter()
            .flat_map(|e| format!("{}\n", serde_json::to_string(e).unwrap()).into_bytes())
            .collect();
        episodic::raw_lines(&bytes)
    }

    /// A `rust::types` file with one lesson per id in `lessons`.
    fn file_citing(lessons: &[u32], citations: &[u32]) -> LessonsFile {
        LessonsFile {
            last_updated: fixed_ts(),
            prompt_version: "dream-cycle/v1.1@2026-05-13".to_string(),
            cluster_key: "rust::types".to_string(),
            citations: citations
                .iter()
                .map(|n| test_id(*n).as_str().to_string())
                .collect(),
            lessons: lessons
                .iter()
                .map(|n| Lesson {
                    id: test_id(*n).as_str().to_string(),
                    content: format!("lesson {n}"),
                    pinned: false,
                })
                .collect(),
        }
    }

    fn pins_of(ids: &[u32]) -> HashMap<EventId, bool> {
        ids.iter().map(|n| (test_id(*n), true)).collect()
    }

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
    fn cited_pins_never_unpins() {
        // Ids 0,1 are pinned=true (external, uncited); ids 2,3 are pinned=false.
        // The file cites 2,3. Under union semantics the external pins on 0,1
        // survive and cited 2,3 get pinned → all four true.
        let events = vec![
            make_event(0, "rust::types", true),
            make_event(1, "rust::types", true),
            make_event(2, "rust::types", false),
            make_event(3, "rust::types", false),
        ];
        let raw = raw_of(&events);

        let pins = cited_pins(&raw, &file_citing(&[2, 3], &[]));

        assert_eq!(
            pins,
            pins_of(&[2, 3]),
            "only the unpinned cited ids flip, and only to true"
        );
        let patched = episodic::patch_lines(&raw, &pins, &HashSet::new()).unwrap();
        let updated: Vec<AgentLearning> = episodic::raw_lines(&patched)
            .into_iter()
            .filter_map(|line| line.parsed)
            .collect();
        assert_eq!(updated.len(), 4);
        assert!(
            updated.iter().all(|e| e.pinned),
            "ids 0,1 keep their external pins; ids 2,3 are cited → all true"
        );
    }

    /// AILAB-200: the pins come from `Lesson.id` **and** the frontmatter
    /// `citations`. A cluster member the model cited but that is not the
    /// exemplar must survive decay, or the lesson outlives its evidence.
    #[test]
    fn cited_pins_unions_lesson_ids_with_citations() {
        // 0 is the exemplar (the `Lesson.id`), 1 is a second cluster member the
        // model cited, 2 is an uncited member. Only 0 and 1 may be pinned.
        let events = vec![
            make_event(0, "rust::types", false),
            make_event(1, "rust::types", false),
            make_event(2, "rust::types", false),
        ];

        let pins = cited_pins(&raw_of(&events), &file_citing(&[0], &[0, 1]));

        assert_eq!(
            pins,
            pins_of(&[0, 1]),
            "id 1 is cited in frontmatter but not the exemplar, so the union must \
             still pin it; id 2 is uncited and must not be pinned"
        );
    }

    #[test]
    fn cited_pins_of_a_file_citing_nothing_is_empty() {
        let events = vec![
            make_event(0, "rust::types", true),
            make_event(1, "rust::types", false),
        ];

        // Nothing cited: no pin is set, and the external pin on 0 is not
        // cleared (SPEC.md `pinned` field; WEG-426).
        let pins = cited_pins(&raw_of(&events), &file_citing(&[], &[]));

        assert_eq!(pins, HashMap::new());
    }

    /// BZR-1234: a second cycle with the same exemplar flips nothing, so the
    /// cycle must stage nothing for the log, and the commit must not rename
    /// it.
    #[cfg(unix)]
    #[test]
    fn a_cycle_that_flips_no_pin_leaves_the_log_inode() {
        use std::os::unix::fs::MetadataExt;

        let dir = unique_tmpdir("pin-noflip");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);

        // Event 0 has the highest pain, so it is the exemplar, and it is
        // already pinned. Nothing is old, so nothing decays.
        let events = vec![
            {
                let mut e = make_event(0, "rust::types", true);
                e.pain = 8.0;
                e
            },
            make_event(1, "rust::types", false),
            make_event(2, "rust::types", false),
        ];
        write_jsonl(&root, &events);
        let jsonl = root.episodic_jsonl();
        let ino_before = fs::metadata(&jsonl).unwrap().ino();
        let bytes_before = fs::read(&jsonl).unwrap();

        run_deterministic_dream_cycle(&root, &ledger(&root), NOW_SEC).unwrap();

        assert_eq!(
            read_lessons_file(&root.lessons_md()).unwrap().lessons[0].id,
            test_id(0).as_str(),
            "precondition: the cycle wrote a lesson for the pinned exemplar"
        );
        assert_eq!(
            fs::metadata(&jsonl).unwrap().ino(),
            ino_before,
            "no flip must not rename the log"
        );
        assert_eq!(fs::read(&jsonl).unwrap(), bytes_before);
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

    // ── run_deterministic_dream_cycle (WEG-61) ─────────────────────────────

    #[test]
    fn deterministic_cycle_empty_jsonl_no_lessons_md() {
        let dir = unique_tmpdir("dc-empty");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);
        // Scaffold a real (empty) `.agent/` store: begin_cycle refuses a
        // missing store (WEG-281). The empty JSONL exercises the
        // no-promotion path.
        write_jsonl(&root, &[]);
        run_deterministic_dream_cycle(&root, &ledger(&root), NOW_SEC).unwrap();
        assert!(!root.lessons_md().exists());
    }

    // ── The read-only preview twin (AILAB-341) ─────────────────────────────

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

        // Now let the write path's plan decide, and compare.
        let raw = episodic::raw_lines(&jsonl_before);
        let plan =
            crate::plan::plan_cycle(&raw, None, NOW_SEC, LessonBodySource::Deterministic).unwrap();
        let crate::plan::LessonsChange::Write(written) = plan.lessons else {
            panic!("a cluster promotes, so the plan writes LESSONS.md");
        };
        assert_eq!(previewed.cluster_key, written.cluster_key);
        assert_eq!(previewed.exemplar_id, written.lessons[0].id);
        assert_eq!(previewed.exemplar_content, written.lessons[0].content);
    }

    // ── A cycle that promotes nothing retires LESSONS.md (AILAB-699) ───────

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

        run_deterministic_dream_cycle(&root, &ledger(&root), NOW_SEC).unwrap();

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
            "the plan must pin the cited exemplar on the promoting cycle"
        );

        // Cycle 2: one later run whose `now_sec` is past the 30-day window. The
        // same events now fall outside both windows (`count_in_window` keeps
        // `ts >= now_sec - window`), so nothing promotes. No hysteresis, no
        // second no-promotion cycle needed.
        let later = NOW_SEC + WINDOW_30_DAYS_SEC + 1;
        run_deterministic_dream_cycle(&root, &ledger(&root), later).unwrap();

        assert!(
            !root.lessons_md().exists(),
            "a no-promotion cycle must unlink the stale LESSONS.md, not leave \
             it (AILAB-699) and not replace it with an empty frontmatter file"
        );
        assert!(
            pinned_after_promote(&exemplar_id),
            "retirement removes the derived doc, never the pin on its source \
             event — the retire path must not plan pins or clear `pinned` \
             (option 3 is out of scope)"
        );

        // Idempotent: a second no-promotion cycle over the same input is a
        // no-op, not an error on the already-absent file.
        run_deterministic_dream_cycle(&root, &ledger(&root), later).unwrap();
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

        run_deterministic_dream_cycle(&root, &ledger(&root), NOW_SEC).unwrap();

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

        run_deterministic_dream_cycle(&root, &ledger(&root), NOW_SEC).unwrap();

        let lf = read_lessons_file(&root.lessons_md()).unwrap();
        assert_eq!(lf.lessons[0].id, test_id(5).as_str().to_string());
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

        run_deterministic_dream_cycle(&root, &ledger(&root), NOW_SEC).unwrap();

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
