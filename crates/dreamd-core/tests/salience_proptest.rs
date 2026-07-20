//! Property tests for query-time salience scoring (WEG-47 / DR-207).
//!
//! Exercises [`dreamd_core::salience::salience`] over the valid input ranges
//! mandated by the formula spec. Hand-written monotonicity spot checks in
//! `salience.rs` were replaced by this generative suite so future formula
//! tweaks cannot silently break ordering invariants.
//!
//! Properties under test:
//! - Monotonic decreasing in `age_days` (holding pain, importance, recurrence).
//! - Monotonic non-decreasing in `pain`, `importance`, and `recurrence`.
//! - No `NaN` / `Infinity` for valid inputs.
//! - Edge cases: zero pain/importance → score 0; recurrence 0 → ln factor 1.

use dreamd_core::salience::salience;
use proptest::prelude::*;

const NOW: i64 = 1_700_000_000;
const SECS_PER_DAY: f64 = 86_400.0;
const MAX_RECURRENCE: u64 = u64::MAX / 2;

fn salience_at_age_days(age_days: f64, pain: f64, importance: f64, recurrence: u64) -> f64 {
    let ts = NOW - (age_days * SECS_PER_DAY) as i64;
    salience(NOW, ts, pain, importance, recurrence)
}

fn valid_inputs_strategy() -> impl Strategy<Value = (f64, f64, f64, u64)> {
    (
        0.0f64..=10_000.0,
        0.0f64..=10.0,
        0.0f64..=10.0,
        0u64..=MAX_RECURRENCE,
    )
}

// ── Edge cases (AC-mandated) ─────────────────────────────────────────────────

#[test]
fn edge_zero_pain_produces_zero_score() {
    let s = salience_at_age_days(1.0, 0.0, 5.0, 3);
    assert_eq!(s, 0.0, "pain=0 must zero the score");
}

#[test]
fn edge_zero_importance_produces_zero_score() {
    let s = salience_at_age_days(1.0, 5.0, 0.0, 3);
    assert_eq!(s, 0.0, "importance=0 must zero the score");
}

#[test]
fn edge_zero_recurrence_ln_factor_is_one() {
    let age_days = 7.0;
    let pain = 6.0;
    let importance = 8.0;
    let with_recurrence = salience_at_age_days(age_days, pain, importance, 0);
    let decay = (-age_days / 14.0).exp();
    let expected = decay * (pain / 10.0) * (importance / 10.0) * 1.0;
    assert!(
        (with_recurrence - expected).abs() < 1e-12,
        "recurrence=0 → factor (1 + ln(1)) = 1; got {with_recurrence}, expected {expected}"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Holding pain, importance, and recurrence fixed, younger events outscore
    /// older ones (strict decrease when age differs).
    #[test]
    fn monotonic_decreasing_in_age(
        (age_younger, pain, importance, recurrence) in valid_inputs_strategy(),
        age_delta in 0.001f64..10_000.0,
    ) {
        let age_older = (age_younger + age_delta).min(10_000.0);
        prop_assume!(age_younger < age_older);

        let younger = salience_at_age_days(age_younger, pain, importance, recurrence);
        let older = salience_at_age_days(age_older, pain, importance, recurrence);

        prop_assert!(
            younger > older,
            "younger ({age_younger}d → {younger}) must beat older ({age_older}d → {older})"
        );
    }

    /// Holding age, importance, and recurrence fixed, higher pain never lowers score.
    #[test]
    fn monotonic_non_decreasing_in_pain(
        (age_days, pain_lo, importance, recurrence) in valid_inputs_strategy(),
        pain_hi in 0.0f64..=10.0,
    ) {
        let (lo, hi) = if pain_lo <= pain_hi {
            (pain_lo, pain_hi)
        } else {
            (pain_hi, pain_lo)
        };

        let s_lo = salience_at_age_days(age_days, lo, importance, recurrence);
        let s_hi = salience_at_age_days(age_days, hi, importance, recurrence);

        prop_assert!(
            s_lo <= s_hi,
            "pain monotonicity violated: lo={lo} ({s_lo}) > hi={hi} ({s_hi})"
        );
    }

    /// Holding age, pain, and recurrence fixed, higher importance never lowers score.
    #[test]
    fn monotonic_non_decreasing_in_importance(
        (age_days, pain, importance_lo, recurrence) in valid_inputs_strategy(),
        importance_hi in 0.0f64..=10.0,
    ) {
        let (lo, hi) = if importance_lo <= importance_hi {
            (importance_lo, importance_hi)
        } else {
            (importance_hi, importance_lo)
        };

        let s_lo = salience_at_age_days(age_days, pain, lo, recurrence);
        let s_hi = salience_at_age_days(age_days, pain, hi, recurrence);

        prop_assert!(
            s_lo <= s_hi,
            "importance monotonicity violated: lo={lo} ({s_lo}) > hi={hi} ({s_hi})"
        );
    }

    /// Holding age, pain, and importance fixed, higher recurrence never lowers score.
    #[test]
    fn monotonic_non_decreasing_in_recurrence(
        (age_days, pain, importance, recurrence_lo) in valid_inputs_strategy(),
        recurrence_hi in 0u64..=MAX_RECURRENCE,
    ) {
        let (lo, hi) = if recurrence_lo <= recurrence_hi {
            (recurrence_lo, recurrence_hi)
        } else {
            (recurrence_hi, recurrence_lo)
        };

        let s_lo = salience_at_age_days(age_days, pain, importance, lo);
        let s_hi = salience_at_age_days(age_days, pain, importance, hi);

        prop_assert!(
            s_lo <= s_hi,
            "recurrence monotonicity violated: lo={lo} ({s_lo}) > hi={hi} ({s_hi})"
        );
    }

    /// Valid inputs never produce NaN or Infinity.
    #[test]
    fn outputs_are_finite(
        (age_days, pain, importance, recurrence) in valid_inputs_strategy(),
    ) {
        let s = salience_at_age_days(age_days, pain, importance, recurrence);
        prop_assert!(s.is_finite(), "expected finite score, got {s}");
    }
}
