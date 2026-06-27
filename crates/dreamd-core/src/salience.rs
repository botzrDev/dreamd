//! DR-204 / WEG-44 — query-time salience scoring.
//!
//! Pure function consumed by the DR-203 tantivy collector, the DR-703
//! `--explain` formatter, the DR-313 dream-cycle deterministic ranker, and
//! the DR-309 decay-threshold check. Storing the score is intentionally
//! avoided (ARCHITECTURE.md decision #2): `age_days` drifts every day, which would
//! force a nightly re-index. Recomputing on every hit keeps the index static.
//!
//! Formula is locked verbatim by ARCHITECTURE.md decision #2 and PRD FR-4.2 — do
//! not refactor the arithmetic shape (e.g., do not factor out `/ 10.0`) so
//! that `--explain` output reads the same as the spec.
//!
//! [`RecurrenceContext`] centralizes per-phase recurrence policy. Per SPEC.md,
//! `recurrence` is not an event field — each consumer derives the `u64` fed
//! into [`salience`] separately. Bugs in call-site policy (index-time
//! staleness vs live cluster size vs decay conservatism) are invisible to
//! [`salience`] unit tests; they surface here instead.

/// Phase-specific recurrence input for query-time salience.
///
/// Each variant documents what "recurrence" means for that consumer. Callers
/// MUST use the constructor matching their phase — do not pass raw `u64` values
/// into [`salience_with_context`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecurrenceContext {
    /// Recall / `--explain`: Tantivy `recurrence` fastfield at index time.
    ///
    /// Bounded staleness — older docs in a cluster may carry a lower count than
    /// the live cluster size until the dream-cycle sidecar re-indexes them.
    RecallIndexFastfield(u64),
    /// Dream-cycle cluster ranking: live member count of the promoted cluster.
    DreamCycleClusterSize(usize),
    /// Decay pruner: recurrence cannot rescue records older than 90 days, so
    /// pass 0 conservatively (see [`crate::decay::DECAY_SALIENCE_THRESHOLD`]).
    Decay,
}

impl RecurrenceContext {
    /// Recurrence from the Tantivy index fastfield (bounded staleness).
    #[must_use]
    pub const fn recall(index_fastfield: u64) -> Self {
        Self::RecallIndexFastfield(index_fastfield)
    }

    /// Recurrence from the live promoted-cluster member count.
    #[must_use]
    pub const fn dream_cycle(cluster_size: usize) -> Self {
        Self::DreamCycleClusterSize(cluster_size)
    }

    /// Conservative recurrence for the decay pruner (always 0).
    #[must_use]
    pub const fn decay() -> Self {
        Self::Decay
    }

    fn as_u64(self) -> u64 {
        match self {
            Self::RecallIndexFastfield(n) => n,
            Self::DreamCycleClusterSize(n) => n as u64,
            Self::Decay => 0,
        }
    }
}

/// Compute salience with an explicit per-phase recurrence policy.
#[must_use]
pub fn salience_with_context(
    now: i64,
    ts: i64,
    pain: f64,
    importance: f64,
    recurrence: RecurrenceContext,
) -> f64 {
    salience(now, ts, pain, importance, recurrence.as_u64())
}

/// Compute the salience score for an episodic record at query time.
///
/// `now` and `ts` are unix epoch seconds (`i64` to allow `now < ts` skew).
/// `recurrence` is `u64` per the protocol type; it is cast to `f64` before
/// `ln` so a recurrence of 0 evaluates to `ln(1.0) == 0.0` (no panic, no
/// `NaN`).
pub fn salience(now: i64, ts: i64, pain: f64, importance: f64, recurrence: u64) -> f64 {
    let age_days = (now - ts) as f64 / 86_400.0;
    (-age_days / 14.0).exp()
        * (pain / 10.0)
        * (importance / 10.0)
        * (1.0 + (1.0 + recurrence as f64).ln())
}

#[cfg(test)]
mod tests {
    use super::{salience, salience_with_context, RecurrenceContext};

    const NOW: i64 = 1_700_000_000;
    const DAY: i64 = 86_400;

    #[test]
    fn monotonic_decreasing_in_age() {
        let one_day = salience(NOW, NOW - DAY, 5.0, 5.0, 3);
        let one_week = salience(NOW, NOW - 7 * DAY, 5.0, 5.0, 3);
        let one_month = salience(NOW, NOW - 30 * DAY, 5.0, 5.0, 3);
        assert!(
            one_day > one_week,
            "1d ({one_day}) should beat 7d ({one_week})"
        );
        assert!(
            one_week > one_month,
            "7d ({one_week}) should beat 30d ({one_month})"
        );
    }

    #[test]
    fn monotonic_non_decreasing_in_pain() {
        let lo = salience(NOW, NOW - DAY, 1.0, 5.0, 3);
        let mid = salience(NOW, NOW - DAY, 5.0, 5.0, 3);
        let hi = salience(NOW, NOW - DAY, 10.0, 5.0, 3);
        assert!(
            lo <= mid && mid <= hi,
            "pain monotonicity: {lo} <= {mid} <= {hi}"
        );
        assert!(lo < hi, "pain should strictly increase score: {lo} < {hi}");
    }

    #[test]
    fn monotonic_non_decreasing_in_importance() {
        let lo = salience(NOW, NOW - DAY, 5.0, 1.0, 3);
        let mid = salience(NOW, NOW - DAY, 5.0, 5.0, 3);
        let hi = salience(NOW, NOW - DAY, 5.0, 10.0, 3);
        assert!(lo <= mid && mid <= hi);
        assert!(lo < hi);
    }

    #[test]
    fn monotonic_non_decreasing_in_recurrence() {
        let r0 = salience(NOW, NOW - DAY, 5.0, 5.0, 0);
        let r1 = salience(NOW, NOW - DAY, 5.0, 5.0, 1);
        let r10 = salience(NOW, NOW - DAY, 5.0, 5.0, 10);
        let r100 = salience(NOW, NOW - DAY, 5.0, 5.0, 100);
        assert!(r0 <= r1 && r1 <= r10 && r10 <= r100);
        assert!(r0 < r100);
    }

    #[test]
    fn zero_recurrence_is_finite_and_positive() {
        // `ln(1.0 + 0.0) == 0.0` so the recurrence factor collapses to 1.0,
        // not 0.0 — score must remain positive, not NaN, not panic.
        let s = salience(NOW, NOW - DAY, 5.0, 5.0, 0);
        assert!(s.is_finite(), "expected finite score, got {s}");
        assert!(s > 0.0, "expected positive score, got {s}");
    }

    #[test]
    fn same_day_event_uses_fractional_age() {
        // Regression guard: if anyone ever rewrites `age_days` as integer
        // division, an event 1 hour old would round to 0 days and tie a
        // simultaneous event. Fractional days must distinguish them.
        let now_score = salience(NOW, NOW, 5.0, 5.0, 3);
        let one_hour = salience(NOW, NOW - 3600, 5.0, 5.0, 3);
        assert!(
            now_score > one_hour,
            "now ({now_score}) should outscore 1h-old ({one_hour})"
        );
    }

    #[test]
    fn spot_check_known_input() {
        // Hand-computed:
        //   age_days = 14 / 14 = 1.0  → exp(-1.0)            = 0.36787944117144233
        //   pain     = 10 / 10                                = 1.0
        //   importance = 10 / 10                              = 1.0
        //   recurrence factor = 1.0 + ln(1.0 + 0.0) = 1.0 + 0 = 1.0
        //   product                                           = e^-1
        let s = salience(NOW, NOW - 14 * DAY, 10.0, 10.0, 0);
        let expected = (-1.0_f64).exp();
        assert!(
            (s - expected).abs() < 1e-12,
            "expected ~{expected}, got {s}"
        );
    }

    #[test]
    fn spot_check_with_recurrence() {
        // age_days = 0 → decay = 1.0
        // pain/10 = 0.5, importance/10 = 0.7
        // recurrence = 9 → 1 + ln(10)
        let s = salience(NOW, NOW, 5.0, 7.0, 9);
        let expected = 1.0 * 0.5 * 0.7 * (1.0 + 10.0_f64.ln());
        assert!((s - expected).abs() < 1e-12, "expected {expected}, got {s}");
    }

    #[test]
    fn recurrence_context_maps_each_phase() {
        let zero_rec = salience(NOW, NOW - DAY, 5.0, 5.0, 0);
        assert_eq!(
            salience_with_context(NOW, NOW - DAY, 5.0, 5.0, RecurrenceContext::recall(0)),
            zero_rec,
        );
        assert_eq!(
            salience_with_context(NOW, NOW - DAY, 5.0, 5.0, RecurrenceContext::dream_cycle(0)),
            zero_rec,
        );
        assert_eq!(
            salience_with_context(NOW, NOW - DAY, 5.0, 5.0, RecurrenceContext::decay()),
            zero_rec,
        );
        let seven_rec = salience(NOW, NOW - DAY, 5.0, 5.0, 7);
        assert_eq!(
            salience_with_context(NOW, NOW - DAY, 5.0, 5.0, RecurrenceContext::recall(7)),
            seven_rec,
        );
        assert_eq!(
            salience_with_context(NOW, NOW - DAY, 5.0, 5.0, RecurrenceContext::dream_cycle(7)),
            seven_rec,
        );
    }

    /// Same numeric recurrence must yield identical scores across recall and
    /// dream-cycle policies — ranking stays predictable when the index is fresh.
    #[test]
    fn recall_and_dream_cycle_agree_when_recurrence_matches() {
        let ts = NOW - DAY;
        let pain = 6.0;
        let importance = 8.0;
        let count = 5_u64;

        let recall_score =
            salience_with_context(NOW, ts, pain, importance, RecurrenceContext::recall(count));
        let dream_score = salience_with_context(
            NOW,
            ts,
            pain,
            importance,
            RecurrenceContext::dream_cycle(count as usize),
        );
        assert_eq!(recall_score, dream_score);
    }

    /// Decay policy is strictly conservative: recurrence=0 never inflates a
    /// score relative to recall/dream-cycle with recurrence > 0.
    #[test]
    fn decay_policy_never_inflates_relative_to_positive_recurrence() {
        let ts = NOW - 91 * DAY;
        let pain = 8.0;
        let importance = 9.0;

        let decay_score =
            salience_with_context(NOW, ts, pain, importance, RecurrenceContext::decay());
        let recall_score =
            salience_with_context(NOW, ts, pain, importance, RecurrenceContext::recall(100));
        assert!(
            decay_score <= recall_score,
            "decay ({decay_score}) must not exceed recall with recurrence=100 ({recall_score})"
        );
    }

    /// Cross-phase ranking on a fixed corpus: when index fastfields match live
    /// cluster sizes, recall and dream-cycle order events identically.
    #[test]
    fn cross_phase_ranking_consistent_when_staleness_absent() {
        // Three events, same cluster size (3), distinct pain for tie-breaking.
        let events = [(5.0, 5.0), (7.0, 5.0), (3.0, 5.0)];
        let cluster_size = 3_usize;
        let ts = NOW - DAY;

        let mut recall_scores: Vec<(f64, f64)> = events
            .iter()
            .map(|&(pain, importance)| {
                let s = salience_with_context(
                    NOW,
                    ts,
                    pain,
                    importance,
                    RecurrenceContext::recall(cluster_size as u64),
                );
                (pain, s)
            })
            .collect();
        recall_scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

        let mut dream_scores: Vec<(f64, f64)> = events
            .iter()
            .map(|&(pain, importance)| {
                let s = salience_with_context(
                    NOW,
                    ts,
                    pain,
                    importance,
                    RecurrenceContext::dream_cycle(cluster_size),
                );
                (pain, s)
            })
            .collect();
        dream_scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

        assert_eq!(
            recall_scores.iter().map(|(p, _)| p).collect::<Vec<_>>(),
            dream_scores.iter().map(|(p, _)| p).collect::<Vec<_>>(),
            "recall and dream-cycle must rank identically when recurrence matches"
        );
    }
}
