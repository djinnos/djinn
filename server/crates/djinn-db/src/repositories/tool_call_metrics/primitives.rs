//! Statistical primitives for the tool-surface metrics module.
//!
//! Provides a deterministic 95% Wilson score interval and the Newcombe hybrid
//! Wilson difference interval used by the decision gate.

use serde::{Deserialize, Serialize};

/// A 95% Wilson score interval for a single proportion.
///
/// Uses `z = 1.959964` (the two-sided 95% normal quantile).
pub const Z_95: f64 = 1.959963984540054;

/// A rate metric with auditable numerator/denominator and the resulting rate.
///
/// Rates are stored as `f64` in `[0, 1]`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RateMetric {
    pub numerator: usize,
    pub denominator: usize,
    pub rate: f64,
}

/// A confidence interval with auditable bounds.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ConfidenceInterval {
    pub lower: f64,
    pub upper: f64,
    /// True when the interval excludes zero (does not contain 0.0).
    pub excludes_zero: bool,
}

impl RateMetric {
    /// Compute a plain rate: `numerator / denominator` (0.0 when denominator is 0).
    pub fn new(numerator: usize, denominator: usize) -> Self {
        let rate = if denominator == 0 {
            0.0
        } else {
            numerator as f64 / denominator as f64
        };
        Self {
            numerator,
            denominator,
            rate,
        }
    }

    /// Compute a rate with one pseudo-count added to both numerator and
    /// denominator, per proposal a5ht's zero-rate handling.
    pub fn with_pseudo_count(numerator: usize, denominator: usize) -> Self {
        Self::new(numerator + 1, denominator + 1)
    }
}

/// Compute the Wilson score interval for a single proportion.
///
/// Returns `(lower, upper)` bounds. Uses the standard Wilson formula.
pub fn wilson_interval(successes: usize, total: usize) -> (f64, f64) {
    if total == 0 {
        return (0.0, 0.0);
    }
    let n = total as f64;
    let p = successes as f64 / n;
    let z2 = Z_95 * Z_95;
    let denom = 1.0 + z2 / n;
    let center = (p + z2 / (2.0 * n)) / denom;
    let margin = Z_95 * ((p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt()) / denom;
    (center - margin, center + margin)
}

/// (`p1 - p2`) using the Wilson score method per proposal a5ht.
///
/// The difference interval is computed as:
/// `diff ± z * sqrt(p1*(1-p1)/n1 + p2*(1-p2)/n2)`
///
/// This is the Newcombe hybrid Wilson interval for the difference of two
/// proportions, which is the standard method recommended for the Wilson-based
/// difference interval.
pub fn wilson_difference_interval(
    successes1: usize,
    total1: usize,
    successes2: usize,
    total2: usize,
) -> ConfidenceInterval {
    if total1 == 0 || total2 == 0 {
        return ConfidenceInterval {
            lower: 0.0,
            upper: 0.0,
            excludes_zero: false,
        };
    }

    let p1 = successes1 as f64 / total1 as f64;
    let p2 = successes2 as f64 / total2 as f64;

    let (l1, u1) = wilson_interval(successes1, total1);
    let (l2, u2) = wilson_interval(successes2, total2);

    // Newcombe hybrid method: the difference interval bounds are computed from
    // the individual Wilson bounds.
    let diff = p1 - p2;
    let lower = diff - ((p1 - l1).powi(2) + (u2 - p2).powi(2)).sqrt();
    let upper = diff + ((u1 - p1).powi(2) + (p2 - l2).powi(2)).sqrt();

    let excludes_zero = lower > 0.0 || upper < 0.0;

    ConfidenceInterval {
        lower,
        upper,
        excludes_zero,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_metric_plain() {
        let m = RateMetric::new(3, 10);
        assert_eq!(m.numerator, 3);
        assert_eq!(m.denominator, 10);
        assert!((m.rate - 0.3).abs() < 1e-9);
    }

    #[test]
    fn rate_metric_zero_denominator() {
        let m = RateMetric::new(0, 0);
        assert_eq!(m.rate, 0.0);
    }

    #[test]
    fn pseudo_count_adds_one_to_both() {
        let m = RateMetric::with_pseudo_count(0, 0);
        assert_eq!(m.numerator, 1);
        assert_eq!(m.denominator, 1);
        assert!((m.rate - 1.0).abs() < 1e-9);

        let m2 = RateMetric::with_pseudo_count(5, 10);
        assert_eq!(m2.numerator, 6);
        assert_eq!(m2.denominator, 11);
    }

    #[test]
    fn wilson_interval_zero_successes() {
        let (lower, upper) = wilson_interval(0, 100);
        assert!(lower >= 0.0);
        assert!(upper > 0.0);
        assert!(upper < 0.1);
    }

    #[test]
    fn wilson_interval_all_successes() {
        let (lower, upper) = wilson_interval(100, 100);
        assert!(lower > 0.9);
        assert!(upper <= 1.0);
    }

    #[test]
    fn wilson_interval_zero_total() {
        let (lower, upper) = wilson_interval(0, 0);
        assert_eq!(lower, 0.0);
        assert_eq!(upper, 0.0);
    }

    #[test]
    fn wilson_difference_interval_nonzero_case_excludes_zero() {
        // p1 = 0.5, p2 = 0.1 — large difference.
        let ci = wilson_difference_interval(50, 100, 10, 100);
        assert!(ci.lower > 0.0, "lower bound {} should be > 0", ci.lower);
        assert!(ci.excludes_zero);
    }

    #[test]
    fn wilson_difference_interval_equal_rates_does_not_exclude_zero() {
        let ci = wilson_difference_interval(50, 100, 50, 100);
        assert!(!ci.excludes_zero);
        assert!(ci.lower <= 0.0);
        assert!(ci.upper >= 0.0);
    }

    #[test]
    fn wilson_difference_interval_zero_rates_does_not_exclude_zero() {
        let ci = wilson_difference_interval(0, 100, 0, 100);
        assert!(!ci.excludes_zero);
    }

    #[test]
    fn wilson_difference_interval_boundary_case() {
        // Very small sample with pseudo-count.
        let ci = wilson_difference_interval(1, 2, 0, 2);
        // With such small samples, the interval should be wide.
        assert!(ci.upper - ci.lower > 0.5);
    }
}
