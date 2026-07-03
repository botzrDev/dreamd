//! Cluster engine and pin/unpin lifecycle (DR-301 / WEG-58).
//!
//! `run_cluster_engine` reads the episodic JSONL, builds a prefix tree over
//! `skill_action` (split on `::`), applies deepest-wins disambiguation, and
//! writes `semantic/recurrence_counts.json`.
//!
//! `apply_pin_unpin` rewrites `AGENT_LEARNINGS.jsonl` with all `pinned` flags
//! cleared then re-set for IDs cited in the freshly-written `LESSONS.md`.
//! Called by WEG-61 after `write_lessons_file`, before `commit_cycle`; not safe
//! to call before `LESSONS.md` exists (succeeds silently if absent).

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Utc};
use dreamd_protocol::AgentLearning;

use crate::episodic::{self, EpisodicError};
use crate::index::{ClusterCount, RecurrenceSidecar};
use crate::io::write_atomic;
use crate::layout::AgentRoot;
use crate::lessons::{self, Lesson, LessonsFile};
use crate::salience::{salience_with_context, RecurrenceContext};
use crate::wal::{self, WalError, WalIntent};

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

    // Step 2: build prefix tree. Each event contributes to ALL prefixes of
    // its skill_action. We store event indices to avoid cloning until the end.
    let mut depth_map: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, event) in events.iter().enumerate() {
        let parts: Vec<&str> = event.skill_action.split("::").collect();
        for len in 1..=parts.len() {
            let prefix = parts[..len].join("::");
            depth_map.entry(prefix).or_default().push(i);
        }
    }

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
        let in_7d = count_in_window(member_indices, &events, now_sec, WINDOW_7_DAYS_SEC);
        let in_30d = count_in_window(member_indices, &events, now_sec, WINDOW_30_DAYS_SEC);
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

/// Clear all `pinned` flags on episodic entries, then re-pin only those cited
/// in the freshly-written `LESSONS.md`.
///
/// Called by WEG-61 (DR-308) after `write_lessons_file`, while the
/// orchestrator-owned dream-cycle WAL is still open. Returns `Ok` without
/// mutations if `LESSONS.md` or the JSONL is absent.
pub fn apply_pin_unpin(agent_root: &AgentRoot) -> Result<(), ConsolidationError> {
    let lessons_path = agent_root.lessons_md();
    let cited_ids: HashSet<String> = if lessons_path.exists() {
        let lessons_file = lessons::read_lessons_file(&lessons_path)?;
        lessons_file.lessons.iter().map(|l| l.id.clone()).collect()
    } else {
        HashSet::new()
    };

    let jsonl_path = agent_root.episodic_jsonl();
    let mut events = episodic::read_all(&jsonl_path)?;
    if events.is_empty() {
        return Ok(());
    }

    for event in &mut events {
        event.pinned = cited_ids.contains(event.id.as_str());
    }

    // WAL: record the prune intent inside the atomic rewrite's hook (WEG-378) —
    // after the temp is fsynced, before the rename. This makes the named temp
    // provably exist when the intent references it; recovery cleans up that
    // temp on a mid-cycle crash. `append_intent` no-ops when no WAL is open.
    let tmp_path = jsonl_path.with_extension("tmp");
    episodic::rewrite_atomic(&jsonl_path, &events, || {
        wal::append_intent(
            agent_root,
            WalIntent::PruneEpisodicMemory {
                temp_file_path: tmp_path.to_string_lossy().into_owned(),
            },
        )
        .map_err(std::io::Error::other)
    })?;
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

/// Orchestrate the full dream cycle in `--no-llm` deterministic mode (DR-308).
///
/// Writes one `LESSONS.md` entry: the highest-salience exemplar from the
/// top promoted cluster (highest `salience_sum`). No network calls are made.
#[must_use = "dream cycle errors must be handled"]
pub fn run_deterministic_dream_cycle(
    agent_root: &AgentRoot,
    now_sec: i64,
) -> Result<(), DreamCycleError> {
    let _span = tracing::debug_span!("dream_cycle_deterministic", now_sec).entered();

    let cluster_output = run_cluster_engine(agent_root, now_sec)?;

    if cluster_output.promoted.is_empty() {
        return Ok(());
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
    let lessons = vec![Lesson {
        id: exemplar.id.as_str().to_string(),
        content: exemplar.content.clone(),
        pinned: false,
    }];

    let lessons_path = agent_root.lessons_md();
    let temp_path = lessons_path
        .with_extension("tmp")
        .to_string_lossy()
        .into_owned();
    wal::append_intent(
        agent_root,
        WalIntent::ReplaceSemanticMemory {
            temp_file_path: temp_path,
        },
    )?;

    let last_updated = DateTime::<Utc>::from_timestamp(now_sec, 0).unwrap_or_default();
    let lessons_file = LessonsFile {
        last_updated,
        prompt_version: "deterministic-only".to_string(),
        cluster_key: top_cluster.cluster_key.clone(),
        lessons,
    };
    std::fs::create_dir_all(agent_root.semantic_dir())?;
    lessons::write_lessons_file(&lessons_path, &lessons_file)?;

    // Pin/unpin rewrites JSONL — `append_intent(PruneEpisodicMemory)` lands in
    // the orchestrator-owned WAL envelope opened by `run_filesystem_phases`.
    apply_pin_unpin(agent_root)?;

    Ok(())
}

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
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use chrono::{DateTime, Utc};
    use dreamd_protocol::EventId;

    use crate::layout::AgentRoot;
    use crate::lessons::{read_lessons_file, write_lessons_file, Lesson, LessonsFile};

    fn unique_tmpdir(label: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "dreamd-consol-{}-{}-{}-{}",
            label,
            std::process::id(),
            nanos,
            n,
        ));
        fs::create_dir_all(&dir).expect("create unique tmpdir");
        dir
    }

    struct DirGuard(std::path::PathBuf);
    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

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
    fn apply_pin_unpin_clears_old_pins_and_sets_new() {
        let dir = unique_tmpdir("pinunpin");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);

        // Write 4 events: ids 0,1 are pinned=true; ids 2,3 are pinned=false.
        // LESSONS.md will cite ids 2,3 — so after apply, 0,1 → false; 2,3 → true.
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
        assert!(!find(0), "id 0: was pinned, not cited → must be false");
        assert!(!find(1), "id 1: was pinned, not cited → must be false");
        assert!(find(2), "id 2: cited in LESSONS.md → must be true");
        assert!(find(3), "id 3: cited in LESSONS.md → must be true");
    }

    #[test]
    fn apply_pin_unpin_no_lessons_md_clears_all_pins() {
        let dir = unique_tmpdir("nopins");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);

        let events = vec![
            make_event(0, "rust::types", true),
            make_event(1, "rust::types", true),
        ];
        write_jsonl(&root, &events);

        // LESSONS.md is absent — apply_pin_unpin should clear all pins.
        apply_pin_unpin(&root).unwrap();

        let updated = episodic::read_all(&root.episodic_jsonl()).unwrap();
        assert_eq!(updated.len(), 2);
        assert!(
            !updated[0].pinned,
            "id 0: no LESSONS.md → pinned must be false"
        );
        assert!(
            !updated[1].pinned,
            "id 1: no LESSONS.md → pinned must be false"
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
        let temp_path = lessons_path
            .with_extension("tmp")
            .to_string_lossy()
            .into_owned();
        wal::append_intent(
            &root,
            WalIntent::ReplaceSemanticMemory {
                temp_file_path: temp_path,
            },
        )
        .unwrap();

        let last_updated = DateTime::<Utc>::from_timestamp(NOW_SEC, 0).unwrap_or_default();
        let lessons_file = LessonsFile {
            last_updated,
            prompt_version: "deterministic-only".to_string(),
            cluster_key: top_cluster.cluster_key.clone(),
            lessons,
        };
        fs::create_dir_all(root.semantic_dir()).unwrap();
        lessons::write_lessons_file(&lessons_path, &lessons_file).unwrap();

        apply_pin_unpin(&root).unwrap();

        let wal: crate::wal::DreamWal =
            serde_json::from_str(&fs::read_to_string(root.wal_path()).unwrap()).unwrap();
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
        write_lessons_file(
            &lessons_path,
            &LessonsFile {
                last_updated: fixed_ts(),
                prompt_version: "deterministic-only".to_string(),
                cluster_key: "rust::types".to_string(),
                lessons: vec![Lesson {
                    id: test_id(2).as_str().to_string(),
                    content: "exemplar".to_string(),
                    pinned: false,
                }],
            },
        )
        .unwrap();

        let jsonl_tmp = root.episodic_jsonl().with_extension("tmp");
        fs::write(&jsonl_tmp, b"partial pin/unpin rewrite\n").unwrap();

        let lessons_tmp = lessons_path.with_extension("tmp");
        let wal = crate::wal::DreamWal {
            schema_version: "1.0".to_string(),
            intents: vec![
                WalIntent::ReplaceSemanticMemory {
                    temp_file_path: lessons_tmp.to_string_lossy().into_owned(),
                },
                WalIntent::PruneEpisodicMemory {
                    temp_file_path: jsonl_tmp.to_string_lossy().into_owned(),
                },
            ],
        };
        fs::write(
            root.wal_path(),
            serde_json::to_string_pretty(&wal).unwrap().as_bytes(),
        )
        .unwrap();
        fs::write(
            root.state_json(),
            br#"{"schema_version":"1.0","last_dream_cycle_status":"in_progress"}"#,
        )
        .unwrap();

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
