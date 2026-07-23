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

/// Which input axis the monotonicity harness varies.
#[derive(Debug, Clone, Copy)]
enum MonotonicAxis {
    Age,
    Pain,
    Importance,
    Recurrence,
}

fn axis_strategy() -> impl Strategy<Value = MonotonicAxis> {
    prop_oneof![
        Just(MonotonicAxis::Age),
        Just(MonotonicAxis::Pain),
        Just(MonotonicAxis::Importance),
        Just(MonotonicAxis::Recurrence),
    ]
}

/// Bundled proptest inputs for the monotonicity harness.
struct MonotonicCase {
    axis: MonotonicAxis,
    age_days: f64,
    pain: f64,
    importance: f64,
    recurrence: u64,
    age_delta: f64,
    hi_pain: f64,
    hi_importance: f64,
    hi_recurrence: u64,
}

fn check_monotonicity(case: MonotonicCase) -> Result<(), TestCaseError> {
    let MonotonicCase {
        axis,
        age_days,
        pain,
        importance,
        recurrence,
        age_delta,
        hi_pain,
        hi_importance,
        hi_recurrence,
    } = case;
    match axis {
        MonotonicAxis::Age => {
            let age_older = (age_days + age_delta).min(10_000.0);
            prop_assume!(age_days < age_older);
            let younger = salience_at_age_days(age_days, pain, importance, recurrence);
            let older = salience_at_age_days(age_older, pain, importance, recurrence);
            prop_assert!(
                younger > older,
                "younger ({age_days}d → {younger}) must beat older ({age_older}d → {older})"
            );
        }
        MonotonicAxis::Pain => {
            let (lo, hi) = if pain <= hi_pain {
                (pain, hi_pain)
            } else {
                (hi_pain, pain)
            };
            let s_lo = salience_at_age_days(age_days, lo, importance, recurrence);
            let s_hi = salience_at_age_days(age_days, hi, importance, recurrence);
            prop_assert!(
                s_lo <= s_hi,
                "pain monotonicity violated: lo={lo} ({s_lo}) > hi={hi} ({s_hi})"
            );
        }
        MonotonicAxis::Importance => {
            let (lo, hi) = if importance <= hi_importance {
                (importance, hi_importance)
            } else {
                (hi_importance, importance)
            };
            let s_lo = salience_at_age_days(age_days, pain, lo, recurrence);
            let s_hi = salience_at_age_days(age_days, pain, hi, recurrence);
            prop_assert!(
                s_lo <= s_hi,
                "importance monotonicity violated: lo={lo} ({s_lo}) > hi={hi} ({s_hi})"
            );
        }
        MonotonicAxis::Recurrence => {
            let (lo, hi) = if recurrence <= hi_recurrence {
                (recurrence, hi_recurrence)
            } else {
                (hi_recurrence, recurrence)
            };
            let s_lo = salience_at_age_days(age_days, pain, importance, lo);
            let s_hi = salience_at_age_days(age_days, pain, importance, hi);
            prop_assert!(
                s_lo <= s_hi,
                "recurrence monotonicity violated: lo={lo} ({s_lo}) > hi={hi} ({s_hi})"
            );
        }
    }
    Ok(())
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

    /// Table-driven monotonicity over age (decreasing) and pain, importance,
    /// recurrence (non-decreasing).
    #[test]
    fn monotonicity_holds_per_axis(
        axis in axis_strategy(),
        (age_days, pain, importance, recurrence) in valid_inputs_strategy(),
        age_delta in 0.001f64..10_000.0,
        hi_pain in 0.0f64..=10.0,
        hi_importance in 0.0f64..=10.0,
        hi_recurrence in 0u64..=MAX_RECURRENCE,
    ) {
        check_monotonicity(MonotonicCase {
            axis,
            age_days,
            pain,
            importance,
            recurrence,
            age_delta,
            hi_pain,
            hi_importance,
            hi_recurrence,
        })?;
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
