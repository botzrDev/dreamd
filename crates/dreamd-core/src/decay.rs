//! Episodic decay pruner (DR-309 / WEG-62).
//!
//! Archives `AGENT_LEARNINGS.jsonl` records older than 90 days whose salience
//! has fallen below 2.0 into a date-stamped snapshot file under
//! `.agent/.dreamd/snapshots/`. The live JSONL is rewritten atomically via a
//! WAL-guarded rename so a mid-cycle crash leaves `.agent/` in a recoverable
//! state. Pinned records are never archived.

use std::fs::{self, File, OpenOptions};
use std::io::Write;

use dreamd_protocol::{AgentLearning, EventId};

use crate::layout::AgentRoot;
use crate::salience::salience;
use crate::wal::{self, WalIntent};

/// Age threshold: events older than this are candidates for decay.
pub const DECAY_AGE_THRESHOLD_SEC: i64 = 90 * 24 * 3600; // 90 days

/// Salience threshold: events below this are candidates for decay.
/// At age > 90d, salience is always < 2.0 for any realistic recurrence
/// (e.g., recurrence=999999 at 90d → salience ≈ 0.024). The threshold
/// check is kept for correctness; it is not the binding filter.
pub const DECAY_SALIENCE_THRESHOLD: f64 = 2.0;

/// Returns `true` if this record should be archived.
///
/// Preconditions caller must ensure: recurrence value doesn't need to be
/// exact — at age > 90d, salience is always below the threshold regardless
/// of recurrence. Pass 0 if the sidecar is unavailable.
pub fn should_decay(now_sec: i64, learning: &AgentLearning, recurrence: u64) -> bool {
    if learning.pinned {
        return false;
    }
    let age_sec = now_sec - learning.timestamp.timestamp();
    if age_sec <= DECAY_AGE_THRESHOLD_SEC {
        return false;
    }
    let sal = salience(
        now_sec,
        learning.timestamp.timestamp(),
        learning.pain as f64,
        learning.importance as f64,
        recurrence,
    );
    sal < DECAY_SALIENCE_THRESHOLD
}

// DECISION (WEG-25 T06): No function in this module takes >3 parameters.
// Both `should_decay(3)` and `run_decay_pruner(3)` have focused signatures;
// grouping into a struct would add ceremony without reducing cognitive load.
// Keeping flat parameters.

/// Returned by [`run_decay_pruner`] on success.
#[derive(Debug, Default)]
pub struct DecayResult {
    /// IDs of events moved to the snapshot file.
    pub decayed_ids: Vec<EventId>,
    /// Count of events that remain in the live JSONL.
    pub kept_count: usize,
}

impl DecayResult {
    /// Returns `true` when the pruner ran but made no changes: no events
    /// decayed and nothing was kept (i.e., the JSONL was absent or empty).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.decayed_ids.is_empty() && self.kept_count == 0
    }
}

/// Errors from [`run_decay_pruner`].
#[derive(Debug, thiserror::Error)]
pub enum DecayError {
    #[error("decay I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("decay parse at line {line}: {source}")]
    Json {
        line: usize,
        source: serde_json::Error,
    },
    #[error("decay WAL: {0}")]
    Wal(#[from] crate::wal::WalError),
}

/// Archive episodic events that have decayed below the salience threshold.
///
/// `now_sec` is caller-provided — wall-clock APIs must not be called internally.
/// `cycle_date` is `"YYYY-MM-DD"` string for the snapshot filename — caller
/// derives it from `now_sec` (avoids timezone ambiguity inside the fn).
///
/// Write sequence:
///   1. Read + parse AGENT_LEARNINGS.jsonl (empty/absent → early return)
///   2. Partition into decayed / kept (recurrence = 0; see constant note above)
///   3. If decayed is empty → return early (no files touched, no WAL opened)
///   4. fs::create_dir_all(snapshots_dir)
///   5. Append decayed records to snapshot_file(cycle_date) — one JSONL line each
///   6. File::open(snapshot_file)?.sync_data() — must complete before WAL opens
///   7. wal::begin_cycle(agent_root, now_sec)
///   8. Write kept records to episodic_jsonl().with_extension("tmp")
///   9. wal::append_intent(agent_root, WalIntent::PruneEpisodicMemory { temp_file_path })
///  10. fs::rename(tmp, episodic_jsonl())
///  11. File::open(episodic_dir)?.sync_all() — parent-dir fsync
///  12. wal::commit_cycle(agent_root, now_sec)
///  13. Return DecayResult { decayed_ids, kept_count }
pub fn run_decay_pruner(
    agent_root: &AgentRoot,
    now_sec: i64,
    cycle_date: &str,
) -> Result<DecayResult, DecayError> {
    // Step 1: read + parse AGENT_LEARNINGS.jsonl.
    let jsonl_path = agent_root.episodic_jsonl();
    let read_result = fs::read(&jsonl_path);
    if let Err(ref e) = read_result {
        if e.kind() == std::io::ErrorKind::NotFound {
            return Ok(DecayResult::default());
        }
    }
    let bytes = read_result?;

    let mut all_events: Vec<AgentLearning> = Vec::new();
    for (i, line) in bytes.split(|&b| b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let event =
            serde_json::from_slice::<AgentLearning>(line).map_err(|e| DecayError::Json {
                line: i + 1,
                source: e,
            })?;
        all_events.push(event);
    }

    if all_events.is_empty() {
        return Ok(DecayResult {
            decayed_ids: Vec::new(),
            kept_count: 0,
        });
    }

    // Step 2: partition into decayed / kept (recurrence = 0 per spec).
    let mut decayed: Vec<AgentLearning> = Vec::new();
    let mut kept: Vec<AgentLearning> = Vec::new();
    for event in all_events {
        if should_decay(now_sec, &event, 0) {
            decayed.push(event);
        } else {
            kept.push(event);
        }
    }

    // Step 3: if nothing decayed, return early — no files touched, no WAL.
    if decayed.is_empty() {
        return Ok(DecayResult {
            decayed_ids: Vec::new(),
            kept_count: kept.len(),
        });
    }

    // Step 4: ensure snapshot directory exists.
    let snapshots_dir = agent_root.snapshots_dir();
    fs::create_dir_all(&snapshots_dir)?;

    // Step 5: append decayed records to snapshot_file — one JSONL line each.
    let snapshot_path = agent_root.snapshot_file(cycle_date);
    {
        let mut snap_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&snapshot_path)?;
        for record in &decayed {
            let line = serde_json::to_string(record)
                .map_err(|e| DecayError::Json { line: 0, source: e })?;
            snap_file.write_all(line.as_bytes())?;
            snap_file.write_all(b"\n")?;
        }
        // Step 6: sync snapshot before opening the WAL.
        snap_file.sync_data()?;
    }

    // Step 7: open the WAL.
    wal::begin_cycle(agent_root, now_sec)?;

    // Step 8: write kept records to .tmp file.
    let tmp_path = jsonl_path.with_extension("tmp");
    {
        let mut tmp_file = File::create(&tmp_path)?;
        for record in &kept {
            let line = serde_json::to_string(record)
                .map_err(|e| DecayError::Json { line: 0, source: e })?;
            tmp_file.write_all(line.as_bytes())?;
            tmp_file.write_all(b"\n")?;
        }
        tmp_file.sync_data()?;
    }

    // Step 9: record WAL intent before the rename.
    wal::append_intent(
        agent_root,
        WalIntent::PruneEpisodicMemory {
            temp_file_path: tmp_path.to_string_lossy().into_owned(),
        },
    )?;

    // Step 10: atomic rename.
    fs::rename(&tmp_path, &jsonl_path)?;

    // Step 11: parent-dir fsync so the rename is durable.
    let episodic_dir = agent_root.episodic_dir();
    File::open(&episodic_dir)?.sync_all()?;

    // Step 12: commit WAL.
    wal::commit_cycle(agent_root, now_sec)?;

    // Step 13: return result.
    let decayed_ids: Vec<EventId> = decayed.into_iter().map(|r| r.id).collect();
    Ok(DecayResult {
        decayed_ids,
        kept_count: kept.len(),
    })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU64, Ordering};

    use chrono::{TimeZone, Utc};
    use dreamd_protocol::EventId;

    use crate::layout::AgentRoot;

    const SAMPLE_ULID_BASE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA";

    fn unique_tmpdir(label: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "dreamd-decay-{}-{}-{}",
            label,
            std::process::id(),
            n,
        ));
        std::fs::create_dir_all(&dir).expect("create tmpdir");
        dir
    }

    struct DirGuard(std::path::PathBuf);
    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn make_event_id(suffix_char: char) -> EventId {
        let raw = format!("evt_{SAMPLE_ULID_BASE}{suffix_char}");
        EventId::parse(&raw).expect("synthesize EventId")
    }

    // fixed "now" for all decay tests: 2026-05-24T00:00:00Z
    const NOW_SEC: i64 = 1748044800;

    // 91 days before NOW_SEC — clearly over the 90-day threshold
    const OLD_TS: i64 = NOW_SEC - 91 * 24 * 3600;

    // 10 days before NOW_SEC — well under threshold
    const YOUNG_TS: i64 = NOW_SEC - 10 * 24 * 3600;

    fn make_learning(id: EventId, ts: i64, pinned: bool) -> AgentLearning {
        AgentLearning {
            schema_version: "1.0.0".to_string(),
            id,
            timestamp: Utc.timestamp_opt(ts, 0).single().expect("valid ts"),
            pain: 5.0,
            importance: 6.0,
            pinned,
            skill_action: "rust.test".to_string(),
            source_harness: "test-harness".to_string(),
            content: "test content".to_string(),
        }
    }

    fn write_jsonl(root: &AgentRoot, events: &[AgentLearning]) {
        let path = root.episodic_jsonl();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut bytes = Vec::new();
        for e in events {
            let mut line = serde_json::to_string(e).unwrap();
            line.push('\n');
            bytes.extend_from_slice(line.as_bytes());
        }
        std::fs::write(&path, &bytes).unwrap();
    }

    // -----------------------------------------------------------------------

    #[test]
    fn decay_result_default_is_empty() {
        let r = DecayResult::default();
        assert!(r.decayed_ids.is_empty());
        assert_eq!(r.kept_count, 0);
    }

    #[test]
    fn should_decay_old_low_salience() {
        let id = make_event_id('0');
        let learning = make_learning(id, OLD_TS, false);
        assert!(should_decay(NOW_SEC, &learning, 0));
    }

    #[test]
    fn should_not_decay_pinned() {
        let id = make_event_id('0');
        let learning = make_learning(id, OLD_TS, true);
        assert!(!should_decay(NOW_SEC, &learning, 0));
    }

    #[test]
    fn should_not_decay_young() {
        let id = make_event_id('0');
        let learning = make_learning(id, YOUNG_TS, false);
        assert!(!should_decay(NOW_SEC, &learning, 0));
    }

    #[test]
    fn run_decay_pruner_noop_no_candidates() {
        let dir = unique_tmpdir("noop");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);

        // All records are young — nothing should decay.
        let events: Vec<AgentLearning> = (0..5u8)
            .map(|i| make_learning(make_event_id(char::from(b'A' + i)), YOUNG_TS, false))
            .collect();
        write_jsonl(&root, &events);
        std::fs::create_dir_all(root.dreamd_dir()).unwrap();

        let result = run_decay_pruner(&root, NOW_SEC, "2026-05-24").unwrap();

        assert!(result.decayed_ids.is_empty());
        assert_eq!(result.kept_count, 5);
        // No snapshot file created.
        assert!(!root.snapshot_file("2026-05-24").exists());
        // JSONL is untouched (no WAL opened).
        assert!(!root.wal_path().exists());
    }

    #[test]
    fn run_decay_pruner_moves_old_records() {
        let dir = unique_tmpdir("moves");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);

        // Build IDs using only Crockford-valid chars.
        // Base (24 chars): "01ARZ3NDEKTSV4RRFFQ69G5F" + 2 varying chars = 26 total.
        let crockford: Vec<char> = "0123456789ABCDEFGHJKMNPQRSTVWXYZ".chars().collect();

        // 30 old + unpinned: last 2 chars = 'A' + crockford[i].
        // 'A' as the 25th char distinguishes old from young.
        let mut events: Vec<AgentLearning> = Vec::new();
        for i in 0..30usize {
            let c = crockford[i]; // 0..30 all fit within 32
            let raw = format!("evt_01ARZ3NDEKTSV4RRFFQ69G5FA{c}");
            let id = EventId::parse(&raw).expect("old event id");
            events.push(make_learning(id, OLD_TS, false));
        }
        // 70 young: last 2 chars = crockford[i/32] + crockford[i%32], prefix char '0'.
        // '0' as 25th char keeps young IDs distinct from old (which use 'A').
        for i in 0..70usize {
            let hi = crockford[i / crockford.len()]; // 0 for i<32, 1 for 32<=i<64, 2 for 64<=i<70
            let lo = crockford[i % crockford.len()];
            let raw = format!("evt_01ARZ3NDEKTSV4RRFFQ69G5F{hi}{lo}");
            let id = EventId::parse(&raw).expect("young event id");
            events.push(make_learning(id, YOUNG_TS, false));
        }

        write_jsonl(&root, &events);
        std::fs::create_dir_all(root.dreamd_dir()).unwrap();

        let result = run_decay_pruner(&root, NOW_SEC, "2026-05-24").unwrap();

        assert_eq!(result.decayed_ids.len(), 30, "30 events decayed");
        assert_eq!(result.kept_count, 70, "70 events kept");

        // Snapshot file has exactly 30 lines.
        let snap_path = root.snapshot_file("2026-05-24");
        assert!(snap_path.exists());
        let snap_bytes = std::fs::read(&snap_path).unwrap();
        let snap_lines: Vec<&[u8]> = snap_bytes
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(snap_lines.len(), 30, "snapshot has 30 lines");

        // JSONL has exactly 70 lines.
        let jsonl_bytes = std::fs::read(root.episodic_jsonl()).unwrap();
        let jsonl_lines: Vec<&[u8]> = jsonl_bytes
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(jsonl_lines.len(), 70, "JSONL has 70 lines");

        // Decayed IDs must NOT appear in the rewritten JSONL.
        let kept_ids: Vec<String> = jsonl_lines
            .iter()
            .map(|l| {
                let ev: serde_json::Value = serde_json::from_slice(l).unwrap();
                ev["id"].as_str().unwrap().to_string()
            })
            .collect();
        for id in &result.decayed_ids {
            assert!(
                !kept_ids.contains(&id.as_str().to_string()),
                "decayed id {} must not appear in kept JSONL",
                id.as_str()
            );
        }
    }

    #[test]
    fn run_decay_pruner_respects_pinned() {
        let dir = unique_tmpdir("pinned");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);

        // Old + pinned: should NOT decay.
        let pinned_id = make_event_id('P');
        let pinned = make_learning(pinned_id.clone(), OLD_TS, true);

        // Old + unpinned: should decay.
        let decay_id = make_event_id('D');
        let decayable = make_learning(decay_id.clone(), OLD_TS, false);

        write_jsonl(&root, &[pinned, decayable]);
        std::fs::create_dir_all(root.dreamd_dir()).unwrap();

        let result = run_decay_pruner(&root, NOW_SEC, "2026-05-24").unwrap();

        assert_eq!(result.decayed_ids.len(), 1);
        assert_eq!(result.decayed_ids[0], decay_id);
        assert_eq!(result.kept_count, 1);

        // Snapshot contains only the unpinned event.
        let snap_bytes = std::fs::read(root.snapshot_file("2026-05-24")).unwrap();
        let snap: Vec<AgentLearning> = snap_bytes
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_slice(l).unwrap())
            .collect();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, decay_id);

        // JSONL still contains the pinned event.
        let jsonl_bytes = std::fs::read(root.episodic_jsonl()).unwrap();
        let kept: Vec<AgentLearning> = jsonl_bytes
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_slice(l).unwrap())
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, pinned_id);
    }
}
