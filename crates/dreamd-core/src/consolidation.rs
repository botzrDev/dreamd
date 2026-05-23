//! Cluster engine and pin/unpin lifecycle (DR-301 / WEG-58).
//!
//! `run_cluster_engine` reads the episodic JSONL, builds a prefix tree over
//! `skill_action` (split on `::`), applies deepest-wins disambiguation, and
//! writes `semantic/recurrence_counts.json`.
//!
//! `apply_pin_unpin` rewrites `AGENT_LEARNINGS.jsonl` with all `pinned` flags
//! cleared then re-set for IDs cited in the freshly-written `LESSONS.md`.
//! Called by WEG-61 after `write_lessons_file`; not safe to call before
//! `LESSONS.md` exists (succeeds silently if absent).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use dreamd_protocol::AgentLearning;

use crate::index::{ClusterCount, RecurrenceSidecar};
use crate::io::write_atomic;
use crate::layout::AgentRoot;
use crate::lessons;
use crate::salience::salience;

#[derive(Debug, thiserror::Error)]
pub enum ConsolidationError {
    #[error("reading AGENT_LEARNINGS.jsonl: {0}")]
    Io(#[from] std::io::Error),
    #[error("parsing event at line {line}: {source}")]
    Json { line: usize, source: serde_json::Error },
}

/// Output of [`run_cluster_engine`]: which clusters were promoted and their
/// member events.
#[derive(Debug, Clone)]
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

/// Minimum event count at a single depth for cluster promotion.
///
/// v0.1 uses raw event count; WEG-59 replaces this with windowed counts.
pub const PROMOTION_THRESHOLD: usize = 3;

/// Run the cluster engine against `agent_root`'s episodic JSONL.
///
/// `now_sec` is caller-provided for determinism — do not call `Utc::now()`.
///
/// Steps:
/// 1. Read + parse `AGENT_LEARNINGS.jsonl`.
/// 2. Build the prefix tree: each event contributes to every prefix of its
///    `skill_action` (split on `::`).
/// 3. Find the deepest prefix where event count ≥ `PROMOTION_THRESHOLD`
///    (deepest-wins — an event is assigned to exactly one promoted cluster).
/// 4. Compute per-cluster salience sum.
/// 5. Write `recurrence_counts.json` sidecar.
/// 6. Return [`ClusterOutput`].
///
/// If the JSONL is absent or empty, returns `Ok(ClusterOutput { promoted: vec![] })`.
pub fn run_cluster_engine(
    agent_root: &AgentRoot,
    now_sec: i64,
) -> Result<ClusterOutput, ConsolidationError> {
    let events = read_jsonl(agent_root.episodic_jsonl())?;
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
        b.split("::").count()
            .cmp(&a.split("::").count())
            .then(a.cmp(b))
    });

    let mut event_cluster: HashMap<usize, String> = HashMap::new();
    let mut cluster_indices: BTreeMap<String, Vec<usize>> = BTreeMap::new();

    for prefix in &prefixes {
        let member_indices = &depth_map[prefix];
        if member_indices.len() < PROMOTION_THRESHOLD {
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
            let recurrence = cluster_events.len() as u64;
            let salience_sum = cluster_events
                .iter()
                .map(|e| {
                    salience(
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
/// Called by WEG-61 (DR-308) after `write_lessons_file`. Returns `Ok` without
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
    let mut events = read_jsonl(&jsonl_path)?;
    if events.is_empty() {
        return Ok(());
    }

    for event in &mut events {
        event.pinned = cited_ids.contains(event.id.as_str());
    }

    let mut out = String::with_capacity(events.len() * 256);
    for event in &events {
        out.push_str(
            &serde_json::to_string(event)
                .map_err(|e| ConsolidationError::Json { line: 0, source: e })?,
        );
        out.push('\n');
    }
    // TODO(WEG-60): wrap in WAL before the rewrite so a crash between
    // "clear all pins" and "write modified JSONL" can be recovered.
    write_atomic(&jsonl_path, out.as_bytes())?;
    Ok(())
}

fn read_jsonl(path: impl AsRef<Path>) -> Result<Vec<AgentLearning>, ConsolidationError> {
    let bytes = match std::fs::read(path.as_ref()) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(ConsolidationError::Io(e)),
    };
    let mut events = Vec::new();
    for (i, line) in bytes.split(|&b| b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let event = serde_json::from_slice::<AgentLearning>(line)
            .map_err(|e| ConsolidationError::Json { line: i + 1, source: e })?;
        events.push(event);
    }
    Ok(events)
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
    use crate::lessons::{write_lessons_file, Lesson, LessonsFile};

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
            schema_version: "1.0".to_string(),
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
        let events: Vec<AgentLearning> = (0..3)
            .map(|i| make_event(i, "a::b::c", false))
            .collect();
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

        let updated = read_jsonl(root.episodic_jsonl()).unwrap();
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

        let updated = read_jsonl(root.episodic_jsonl()).unwrap();
        assert_eq!(updated.len(), 2);
        assert!(!updated[0].pinned, "id 0: no LESSONS.md → pinned must be false");
        assert!(!updated[1].pinned, "id 1: no LESSONS.md → pinned must be false");
    }
}
