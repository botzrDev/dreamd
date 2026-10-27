//! Episodic decay pruner (DR-309 / WEG-62).
//!
//! Archives `AGENT_LEARNINGS.jsonl` records older than 90 days whose salience
//! has fallen below 2.0 into a date-stamped snapshot file under
//! `.agent/.dreamd/snapshots/`. Both the archive and the pruned log are staged
//! with the cycle's other outputs, so they land together when the cycle's redo
//! WAL commits, or not at all. Pinned records are never archived.

use std::collections::{HashMap, HashSet};

use dreamd_protocol::{AgentLearning, EventId};

use crate::episodic::{self, EpisodicError};
use crate::salience::{age_secs, salience_with_context, RecurrenceContext};
use crate::stage::Stage;

/// Age threshold: events older than this are candidates for decay.
pub const DECAY_AGE_THRESHOLD_SEC: i64 = 90 * 24 * 3600; // 90 days

/// Salience threshold: events below this are candidates for decay.
/// At age > 90d, salience is always < 2.0 for any realistic recurrence
/// (e.g., recurrence=999999 at 90d → salience ≈ 0.024). The threshold
/// check is kept for correctness; it is not the binding filter.
pub const DECAY_SALIENCE_THRESHOLD: f64 = 2.0;

/// Returns `true` if this record should be archived.
///
/// Uses [`RecurrenceContext::decay`] — at age > 90d, salience is always below
/// the threshold regardless of live cluster recurrence.
pub fn should_decay(now_sec: i64, learning: &AgentLearning) -> bool {
    if learning.pinned {
        return false;
    }
    let age_sec = age_secs(now_sec, learning.timestamp.timestamp());
    if age_sec <= DECAY_AGE_THRESHOLD_SEC {
        return false;
    }
    let sal = salience_with_context(
        now_sec,
        learning.timestamp.timestamp(),
        learning.pain as f64,
        learning.importance as f64,
        RecurrenceContext::decay(),
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
    /// Retained for the serialize side (WEG-378); the read side no longer
    /// produces a per-line parse error — `episodic::scan` tolerates a torn tail.
    #[error("decay parse at line {line}: {source}")]
    Json {
        line: usize,
        source: serde_json::Error,
    },
    #[error("decay WAL: {0}")]
    Wal(#[from] crate::wal::WalError),
}

impl From<EpisodicError> for DecayError {
    fn from(e: EpisodicError) -> Self {
        match e {
            EpisodicError::Io(io) => DecayError::Io(io),
            EpisodicError::Serialize(je) => DecayError::Json {
                line: 0,
                source: je,
            },
            // Never produced on the decay rewrite/read paths (no size check),
            // but the match must be exhaustive.
            EpisodicError::PayloadTooLarge { size, max } => DecayError::Io(std::io::Error::other(
                format!("payload too large: {size} > {max}"),
            )),
        }
    }
}

/// Archive episodic events that have decayed below the salience threshold.
///
/// `now_sec` is caller-provided — wall-clock APIs must not be called internally.
/// `cycle_date` is `"YYYY-MM-DD"` string for the snapshot filename — caller
/// derives it from `now_sec` (avoids timezone ambiguity inside the fn).
///
/// Sequence:
///   1. Read AGENT_LEARNINGS.jsonl through `stage` (absent → empty; every line
///      keeps its bytes), so the pin pass staged earlier in the cycle is seen
///   2. Select the parsed records `should_decay` picks (recurrence = 0; see
///      constant note above); their ids are the drops
///   3. If nothing is selected → return early (nothing staged)
///   4. Build the day's archive: its current bytes (staged or live), a `\n`
///      ending a torn tail, then each dropped line's bytes whose id the
///      archive does not already hold
///   5. Stage the archive, then the log with `episodic::patch_lines` output
///   6. Return DecayResult { decayed_ids, kept_count }
///
/// `stage` belongs to `dream_cycle::run_filesystem_phases`, which commits it.
pub fn run_decay_pruner(
    stage: &mut Stage,
    now_sec: i64,
    cycle_date: &str,
) -> Result<DecayResult, DecayError> {
    // Step 1: read AGENT_LEARNINGS.jsonl through the shared episodic scan
    // (WEG-378), keeping every line's bytes. A torn tail is never a record, so
    // the CLI `dreamd dream` path does not abort on a post-SIGKILL torn line.
    let jsonl_path = stage.agent_root().episodic_jsonl();
    let raw = episodic::raw_lines(&stage.read(&jsonl_path)?.unwrap_or_default());
    let records = || raw.iter().filter_map(|line| line.parsed.as_ref());

    // Step 2: select the decayed records ([`RecurrenceContext::decay`]). A drop
    // matches every line with that id.
    let drops: HashSet<EventId> = records()
        .filter(|event| should_decay(now_sec, event))
        .map(|event| event.id.clone())
        .collect();
    let decayed_ids: Vec<EventId> = records()
        .filter(|event| drops.contains(&event.id))
        .map(|event| event.id.clone())
        .collect();
    let kept_count = records().count() - decayed_ids.len();

    // Step 3: if nothing decayed, return early — nothing staged.
    if drops.is_empty() {
        return Ok(DecayResult {
            decayed_ids,
            kept_count,
        });
    }

    // Step 4: the archive is a full copy. Ids already in it are skipped, so a
    // row a store already archived is not archived twice.
    let snapshot_path = stage.agent_root().snapshot_file(cycle_date);
    let mut archive = stage.read(&snapshot_path)?.unwrap_or_default();
    let existing: HashSet<EventId> = episodic::raw_lines(&archive)
        .into_iter()
        .filter_map(|line| line.parsed.map(|event| event.id))
        .collect();
    // A crash mid-append by an older binary leaves a fragment with no `\n`.
    // End it first, or the next row is glued onto it and never parses.
    if !archive.is_empty() && !archive.ends_with(b"\n") {
        archive.push(b'\n');
    }
    for line in &raw {
        let Some(event) = &line.parsed else { continue };
        if drops.contains(&event.id) && !existing.contains(&event.id) {
            archive.extend_from_slice(&line.bytes);
        }
    }

    // Step 5: the archive, then the log without the dropped lines.
    stage.put(&snapshot_path, &archive)?;
    let contents = episodic::patch_lines(&raw, &HashMap::new(), &drops)?;
    stage.put(&jsonl_path, &contents)?;

    Ok(DecayResult {
        decayed_ids,
        kept_count,
    })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::{TimeZone, Utc};
    use dreamd_protocol::EventId;

    use crate::layout::AgentRoot;
    use crate::provenance::Ledger;
    use crate::test_support::{unique_tmpdir, DirGuard};
    use crate::wal::{self, CycleKind};

    const SAMPLE_ULID_BASE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA";

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

    /// The decay pass in its own stage, committed, so the asserts read the live
    /// files.
    fn prune(root: &AgentRoot, date: &str) -> DecayResult {
        let mut stage = wal::begin_cycle(root, CycleKind::Dream, NOW_SEC).unwrap();
        let result = run_decay_pruner(&mut stage, NOW_SEC, date).unwrap();
        wal::commit_cycle(stage, &Ledger::open(root.provenance_ledger()), NOW_SEC).unwrap();
        result
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
        assert!(should_decay(NOW_SEC, &learning));
    }

    #[test]
    fn should_not_decay_pinned() {
        let id = make_event_id('0');
        let learning = make_learning(id, OLD_TS, true);
        assert!(!should_decay(NOW_SEC, &learning));
    }

    #[test]
    fn should_not_decay_young() {
        let id = make_event_id('0');
        let learning = make_learning(id, YOUNG_TS, false);
        assert!(!should_decay(NOW_SEC, &learning));
    }

    #[test]
    fn should_not_decay_future_dated() {
        let id = make_event_id('0');
        let learning = make_learning(id, NOW_SEC + 200 * 24 * 3600, false);
        assert!(!should_decay(NOW_SEC, &learning));
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

        let mut stage = Stage::create(&root, "dream-1").unwrap();
        let result = run_decay_pruner(&mut stage, NOW_SEC, "2026-05-24").unwrap();

        assert!(result.decayed_ids.is_empty());
        assert_eq!(result.kept_count, 5);
        // Nothing staged: no archive, no log.
        assert!(stage.redo().renames.is_empty());
        // No snapshot file created.
        assert!(!root.snapshot_file("2026-05-24").exists());
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
        // 0..30 all fit within 32
        for &c in crockford.iter().take(30) {
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

        let result = prune(&root, "2026-05-24");

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

        let result = prune(&root, "2026-05-24");

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

    // ── BZR-1234: byte-preserving rewrite and an idempotent archive ─────────

    const SKIPPED: &[u8] = b"{\"not\":\"a learning\"}\n";
    const DATE: &str = "2026-05-24";

    /// `event`'s JSONL line with an unknown `x_note` member first.
    fn line_with_x_note(event: &AgentLearning) -> Vec<u8> {
        let line = serde_json::to_string(event).unwrap();
        format!("{{\"x_note\":\"keep\",{}\n", &line[1..]).into_bytes()
    }

    fn line_of(event: &AgentLearning) -> Vec<u8> {
        format!("{}\n", serde_json::to_string(event).unwrap()).into_bytes()
    }

    fn setup(label: &str, log: &[u8]) -> (AgentRoot, DirGuard) {
        let dir = unique_tmpdir(label);
        let root = AgentRoot::new(&dir);
        std::fs::create_dir_all(root.episodic_dir()).unwrap();
        std::fs::create_dir_all(root.dreamd_dir()).unwrap();
        std::fs::write(root.episodic_jsonl(), log).unwrap();
        (root, DirGuard(dir))
    }

    #[test]
    fn decay_keeps_unknown_fields_and_skipped_lines() {
        let young = line_with_x_note(&make_learning(make_event_id('Y'), YOUNG_TS, false));
        let old = line_with_x_note(&make_learning(make_event_id('D'), OLD_TS, false));
        let log = [young.as_slice(), SKIPPED, old.as_slice()].concat();
        let (root, _g) = setup("decay-bytes", &log);

        let result = prune(&root, DATE);

        assert_eq!(result.decayed_ids, vec![make_event_id('D')]);
        assert_eq!(result.kept_count, 1);
        assert_eq!(
            std::fs::read(root.episodic_jsonl()).unwrap(),
            [young.as_slice(), SKIPPED].concat(),
            "the young line and the skipped line keep their bytes"
        );
        assert_eq!(
            std::fs::read(root.snapshot_file(DATE)).unwrap(),
            old,
            "the archive holds the old line's bytes"
        );
    }

    /// A row already in the day's archive is not archived again, e.g. when a
    /// merge brings archived rows back into the log.
    #[test]
    fn decay_skips_ids_already_in_the_archive() {
        let old_a = make_learning(make_event_id('A'), OLD_TS, false);
        let old_b = make_learning(make_event_id('B'), OLD_TS, false);
        let young = make_learning(make_event_id('Y'), YOUNG_TS, false);
        let log = [line_of(&old_a), line_of(&young), line_of(&old_b)].concat();
        let (root, _g) = setup("decay-rerun", &log);

        let first = prune(&root, DATE);
        assert_eq!(first.decayed_ids.len(), 2);
        // The archived rows come back into the log.
        std::fs::write(root.episodic_jsonl(), &log).unwrap();
        let second = prune(&root, DATE);
        assert_eq!(second.decayed_ids, first.decayed_ids);

        let archived = episodic::read_all(&root.snapshot_file(DATE)).unwrap();
        for id in &first.decayed_ids {
            let n = archived.iter().filter(|ev| ev.id == *id).count();
            assert_eq!(n, 1, "{id} must be archived exactly once");
        }
        let live = episodic::read_all(&root.episodic_jsonl()).unwrap();
        assert!(
            live.iter().all(|ev| !first.decayed_ids.contains(&ev.id)),
            "no decayed id may stay in the log"
        );
        assert_eq!(live, vec![young]);
    }

    #[test]
    fn decay_archive_with_a_torn_tail_gets_a_newline_first() {
        let old = make_learning(make_event_id('D'), OLD_TS, false);
        let (root, _g) = setup("decay-torn-archive", &line_of(&old));
        std::fs::create_dir_all(root.snapshots_dir()).unwrap();
        std::fs::write(root.snapshot_file(DATE), b"{\"partial").unwrap();

        prune(&root, DATE);

        let archive = root.snapshot_file(DATE);
        assert_eq!(
            std::fs::read(&archive).unwrap(),
            [b"{\"partial\n".as_slice(), &line_of(&old)].concat()
        );
        assert_eq!(episodic::read_all(&archive).unwrap(), vec![old]);
    }

    /// One stage, pin pass then decay: decay sees the staged pin, so an old
    /// cited row stays and is pinned, and only the uncited old row goes.
    #[test]
    fn decay_reads_the_staged_log() {
        let cited = make_learning(make_event_id('P'), OLD_TS, false);
        let uncited = make_learning(make_event_id('D'), OLD_TS, false);
        let young = make_learning(make_event_id('Y'), YOUNG_TS, false);
        let log = [line_of(&cited), line_of(&uncited), line_of(&young)].concat();
        let (root, _g) = setup("decay-staged-log", &log);
        let mut stage = wal::begin_cycle(&root, CycleKind::Dream, NOW_SEC).unwrap();
        let lessons = crate::lessons::render_lessons_file(&crate::lessons::LessonsFile {
            last_updated: Utc.timestamp_opt(NOW_SEC, 0).single().unwrap(),
            prompt_version: "deterministic-only".to_string(),
            cluster_key: "rust.test".to_string(),
            citations: vec![cited.id.as_str().to_string()],
            lessons: vec![crate::lessons::Lesson {
                id: cited.id.as_str().to_string(),
                content: "test content".to_string(),
                pinned: false,
            }],
        });
        stage.put(&root.lessons_md(), lessons.as_bytes()).unwrap();

        crate::consolidation::apply_pin_unpin(&mut stage).unwrap();
        let result = run_decay_pruner(&mut stage, NOW_SEC, DATE).unwrap();

        assert_eq!(result.decayed_ids, vec![uncited.id.clone()]);
        let staged = stage.read(&root.episodic_jsonl()).unwrap().unwrap();
        let staged: Vec<AgentLearning> = episodic::raw_lines(&staged)
            .into_iter()
            .filter_map(|line| line.parsed)
            .collect();
        let mut pinned = cited.clone();
        pinned.pinned = true;
        assert_eq!(
            staged,
            vec![pinned, young],
            "the staged log keeps the pin and lacks the decayed row"
        );
        assert_eq!(
            std::fs::read(root.episodic_jsonl()).unwrap(),
            log,
            "nothing live changes before the commit"
        );
    }
}
