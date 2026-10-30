//! Episodic decay pruner (WEG-62).
//!
//! Archives `AGENT_LEARNINGS.jsonl` records older than 90 days whose salience
//! has fallen below 2.0 into a date-stamped snapshot file under
//! `.agent/.dreamd/snapshots/`. [`plan_decay`] decides what leaves the log, as
//! part of the cycle's plan ([`crate::plan::plan_cycle`]), and
//! [`merge_archive`] builds the day's archive. Both the archive and the pruned
//! log are staged with the cycle's other outputs, so they land together when
//! the cycle's redo WAL commits, or not at all. Pinned records, and records the
//! same plan pins, are never archived.

use std::collections::{HashMap, HashSet};

use dreamd_protocol::{AgentLearning, EventId};

use crate::episodic::{self, RawLine};
use crate::salience::{age_secs, salience_with_context, RecurrenceContext};

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

/// What a cycle's decay did, as `run_filesystem_phases` returns it
/// ([`crate::plan::CyclePlan::decay_result`]).
#[derive(Debug, Default)]
pub struct DecayResult {
    /// IDs of events moved to the snapshot file.
    pub decayed_ids: Vec<EventId>,
    /// Count of events that remain in the live JSONL.
    pub kept_count: usize,
}

impl DecayResult {
    /// Returns `true` when there was nothing to prune: no events decayed and
    /// none were kept (the JSONL was absent or empty). A run that kept every
    /// event is not empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.decayed_ids.is_empty() && self.kept_count == 0
    }
}

/// The decay decision for one log: [`plan_decay`]'s result.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DecayPlan {
    /// Ids whose lines leave the log. A drop matches every line with that id.
    pub drops: HashSet<EventId>,
    /// One id per dropped record line, in log order.
    pub decayed_ids: Vec<EventId>,
    /// Records the log keeps.
    pub kept_count: usize,
    /// The dropped record lines with their bytes, in log order.
    pub archived: Vec<RawLine>,
}

/// Decide which records of `raw` decay at `now_sec`.
///
/// `now_sec` is caller-provided — wall-clock APIs must not be called internally.
///
/// A record decays when [`should_decay`] picks it (recurrence = 0; see the
/// constant note above) and its id is not in `pins`. `should_decay` already
/// spares a record whose stored flag is set; `pins` holds the pins the same
/// plan sets, so a record this cycle cites is never archived. Lines with no
/// record — skipped lines and a torn final line — never decay. Pure: reads
/// nothing and writes nothing.
#[must_use]
pub fn plan_decay(raw: &[RawLine], pins: &HashMap<EventId, bool>, now_sec: i64) -> DecayPlan {
    let records = || raw.iter().filter_map(|line| line.parsed.as_ref());

    let drops: HashSet<EventId> = records()
        .filter(|event| !pins.contains_key(&event.id) && should_decay(now_sec, event))
        .map(|event| event.id.clone())
        .collect();
    let decayed_ids: Vec<EventId> = records()
        .filter(|event| drops.contains(&event.id))
        .map(|event| event.id.clone())
        .collect();
    let archived: Vec<RawLine> = raw
        .iter()
        .filter(|line| {
            line.parsed
                .as_ref()
                .is_some_and(|event| drops.contains(&event.id))
        })
        .cloned()
        .collect();
    let kept_count = records().count() - decayed_ids.len();

    DecayPlan {
        drops,
        decayed_ids,
        kept_count,
        archived,
    }
}

/// The day's archive after `rows` are added to `existing` (its current bytes,
/// `None` when absent).
///
/// The result is a full copy: the existing bytes, a `\n` ending a torn tail,
/// then each row whose id the existing bytes do not already hold, so a row a
/// store already archived is not archived twice. The ids are collected once,
/// before any row is added.
#[must_use]
pub fn merge_archive(existing: Option<Vec<u8>>, rows: &[RawLine]) -> Vec<u8> {
    let mut archive = existing.unwrap_or_default();
    let held: HashSet<EventId> = episodic::raw_lines(&archive)
        .into_iter()
        .filter_map(|line| line.parsed.map(|event| event.id))
        .collect();
    // A crash mid-append by an older binary leaves a fragment with no `\n`.
    // End it first, or the next row is glued onto it and never parses.
    if !archive.is_empty() && !archive.ends_with(b"\n") {
        archive.push(b'\n');
    }
    for row in rows {
        let Some(event) = &row.parsed else { continue };
        if !held.contains(&event.id) {
            archive.extend_from_slice(&row.bytes);
        }
    }
    archive
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::{TimeZone, Utc};
    use dreamd_protocol::EventId;

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

    /// The log's lines for `events`, one serialized record per line.
    fn raw_of(events: &[AgentLearning]) -> Vec<RawLine> {
        let bytes: Vec<u8> = events.iter().flat_map(line_of).collect();
        episodic::raw_lines(&bytes)
    }

    /// Decay with no pins from the plan, as on a cycle that cites nothing.
    fn plan(raw: &[RawLine]) -> DecayPlan {
        plan_decay(raw, &HashMap::new(), NOW_SEC)
    }

    /// The log a cycle stages after `plan` drops its lines.
    fn pruned(raw: &[RawLine], plan: &DecayPlan) -> Vec<u8> {
        episodic::patch_lines(raw, &HashMap::new(), &plan.drops).unwrap()
    }

    fn non_empty_lines(bytes: &[u8]) -> Vec<&[u8]> {
        bytes
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .collect()
    }

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
    fn plan_decay_noop_no_candidates() {
        // All records are young — nothing should decay.
        let events: Vec<AgentLearning> = (0..5u8)
            .map(|i| make_learning(make_event_id(char::from(b'A' + i)), YOUNG_TS, false))
            .collect();
        let raw = raw_of(&events);

        let plan = plan(&raw);

        // Nothing decays, so the cycle stages no archive and no log.
        assert_eq!(
            plan,
            DecayPlan {
                drops: HashSet::new(),
                decayed_ids: Vec::new(),
                kept_count: 5,
                archived: Vec::new(),
            }
        );
    }

    #[test]
    fn plan_decay_moves_old_records() {
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
        let raw = raw_of(&events);

        let plan = plan(&raw);

        assert_eq!(plan.decayed_ids.len(), 30, "30 events decayed");
        assert_eq!(plan.kept_count, 70, "70 events kept");

        // The archive gets exactly 30 lines.
        let archive = merge_archive(None, &plan.archived);
        assert_eq!(non_empty_lines(&archive).len(), 30, "archive has 30 lines");

        // The pruned log has exactly 70 lines.
        let log = pruned(&raw, &plan);
        let log_lines = non_empty_lines(&log);
        assert_eq!(log_lines.len(), 70, "JSONL has 70 lines");

        // Decayed IDs must not appear in the pruned log.
        let kept_ids: Vec<String> = log_lines
            .iter()
            .map(|l| {
                let ev: serde_json::Value = serde_json::from_slice(l).unwrap();
                ev["id"].as_str().unwrap().to_string()
            })
            .collect();
        for id in &plan.decayed_ids {
            assert!(
                !kept_ids.contains(&id.as_str().to_string()),
                "decayed id {} must not appear in kept JSONL",
                id.as_str()
            );
        }
    }

    #[test]
    fn plan_decay_respects_pinned() {
        // Old + pinned: should not decay.
        let pinned_id = make_event_id('P');
        let pinned = make_learning(pinned_id.clone(), OLD_TS, true);

        // Old + unpinned: should decay.
        let decay_id = make_event_id('D');
        let decayable = make_learning(decay_id.clone(), OLD_TS, false);

        let raw = raw_of(&[pinned.clone(), decayable.clone()]);

        let plan = plan(&raw);

        assert_eq!(plan.decayed_ids, vec![decay_id]);
        assert_eq!(plan.kept_count, 1);
        // The archive holds only the unpinned event; the log keeps the pinned one.
        assert_eq!(merge_archive(None, &plan.archived), line_of(&decayable));
        assert_eq!(pruned(&raw, &plan), line_of(&pinned));
    }

    // ── BZR-1234: byte-preserving rewrite and an idempotent archive ─────────

    const SKIPPED: &[u8] = b"{\"not\":\"a learning\"}\n";

    /// `event`'s JSONL line with an unknown `x_note` member first.
    fn line_with_x_note(event: &AgentLearning) -> Vec<u8> {
        let line = serde_json::to_string(event).unwrap();
        format!("{{\"x_note\":\"keep\",{}\n", &line[1..]).into_bytes()
    }

    fn line_of(event: &AgentLearning) -> Vec<u8> {
        format!("{}\n", serde_json::to_string(event).unwrap()).into_bytes()
    }

    #[test]
    fn plan_decay_keeps_unknown_fields_and_skipped_lines() {
        let young = line_with_x_note(&make_learning(make_event_id('Y'), YOUNG_TS, false));
        let old = line_with_x_note(&make_learning(make_event_id('D'), OLD_TS, false));
        let log = [young.as_slice(), SKIPPED, old.as_slice()].concat();
        let raw = episodic::raw_lines(&log);

        let plan = plan(&raw);

        assert_eq!(plan.decayed_ids, vec![make_event_id('D')]);
        assert_eq!(plan.kept_count, 1);
        assert_eq!(
            pruned(&raw, &plan),
            [young.as_slice(), SKIPPED].concat(),
            "the young line and the skipped line keep their bytes"
        );
        assert_eq!(
            merge_archive(None, &plan.archived),
            old,
            "the archive holds the old line's bytes"
        );
    }

    /// A row already in the day's archive is not archived again, e.g. when a
    /// merge brings archived rows back into the log.
    #[test]
    fn merge_archive_skips_ids_already_archived() {
        let old_a = make_learning(make_event_id('A'), OLD_TS, false);
        let old_b = make_learning(make_event_id('B'), OLD_TS, false);
        let young = make_learning(make_event_id('Y'), YOUNG_TS, false);
        let raw = raw_of(&[old_a, young.clone(), old_b]);

        let first = plan(&raw);
        assert_eq!(first.decayed_ids.len(), 2);
        let archive = merge_archive(None, &first.archived);
        // The archived rows come back into the log, and decay again.
        let second = plan(&raw);
        assert_eq!(second.decayed_ids, first.decayed_ids);
        let archive = merge_archive(Some(archive), &second.archived);

        let archived = episodic::raw_lines(&archive);
        for id in &first.decayed_ids {
            let n = archived
                .iter()
                .filter(|line| line.parsed.as_ref().is_some_and(|ev| ev.id == *id))
                .count();
            assert_eq!(n, 1, "{id} must be archived exactly once");
        }
        assert_eq!(archived.len(), 2);
        assert_eq!(pruned(&raw, &second), line_of(&young));
    }

    #[test]
    fn merge_archive_ends_a_torn_tail_first() {
        let old = make_learning(make_event_id('D'), OLD_TS, false);
        let raw = raw_of(std::slice::from_ref(&old));
        let plan = plan(&raw);

        let archive = merge_archive(Some(b"{\"partial".to_vec()), &plan.archived);

        assert_eq!(
            archive,
            [b"{\"partial\n".as_slice(), &line_of(&old)].concat()
        );
        let records: Vec<AgentLearning> = episodic::raw_lines(&archive)
            .into_iter()
            .filter_map(|line| line.parsed)
            .collect();
        assert_eq!(records, vec![old]);
    }
}
