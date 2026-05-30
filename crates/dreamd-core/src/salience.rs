//! DR-204 / WEG-44 — query-time salience scoring.
//!
//! Pure function consumed by the DR-203 tantivy collector, the DR-703
//! `--explain` formatter, the DR-313 dream-cycle deterministic ranker, and
//! the DR-309 decay-threshold check. Storing the score is intentionally
//! avoided (CLAUDE.md decision #2): `age_days` drifts every day, which would
//! force a nightly re-index. Recomputing on every hit keeps the index static.
//!
//! Formula is locked verbatim by CLAUDE.md decision #2 and PRD FR-4.2 — do
//! not refactor the arithmetic shape (e.g., do not factor out `/ 10.0`) so
//! that `--explain` output reads the same as the spec.

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
    use super::salience;

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
}
