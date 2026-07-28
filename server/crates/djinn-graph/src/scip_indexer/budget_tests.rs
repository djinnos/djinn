//! Unit tests for [`super`], the SCIP indexer budget model.
//!
//! Split out of `budget.rs` so the production module stays inside the
//! `Server Guards` file-size budget (MAX_LINES=1500 / MAX_BYTES=51200).
//! Included with `#[path]` so it remains a child of the module under test and
//! keeps its `use super::*` access to the private budget internals.

use super::*;
use djinn_core::clock::{Clock, SystemClock};

// --- Helpers -----------------------------------------------------------

/// Budget for rust-analyzer with an EXPLICIT hard bound, so ceiling
/// behaviour is asserted without reading the process environment (tests in
/// this binary run concurrently; a shared env var would make them racy).
fn budget_with_bound(
    size: &WorkspaceSizeHint,
    prior: Option<&PriorIndexerTiming>,
    hard_bound: Duration,
) -> IndexerBudget {
    budget_for_indexer_at_with_bound(
        SupportedIndexer::RustAnalyzer,
        size,
        prior,
        None,
        SystemClock::new().now_instant(),
        hard_bound,
    )
}

/// The djinn workspace as measured on 2026-07-27: 1425 `.rs` files,
/// 31.6 MiB of source, whose rust-analyzer pass cost 3 522 197 ms.
fn measured_production_workspace() -> WorkspaceSizeHint {
    WorkspaceSizeHint {
        source_file_count: 1425,
        source_bytes: 33_135_002, // 31.6 MiB
        partition_count: 1,
    }
}

/// Build a deadline whose `started_at` is `now` so tests are deterministic.
fn fresh_deadline(max_secs: u64, reserve_secs: u64) -> (ActiveDeadline, Instant) {
    let now = SystemClock::new().now_instant();
    let deadline = ActiveDeadline {
        started_at: now,
        max_runtime: Duration::from_secs(max_secs),
        reserve: Duration::from_secs(reserve_secs),
    };
    (deadline, now)
}

// --- Baseline budgets --------------------------------------------------

#[test]
fn tiny_workspace_no_prior_no_deadline_receives_baseline() {
    let size = WorkspaceSizeHint::default();

    for indexer in SupportedIndexer::ALL {
        let budget = budget_for_indexer(indexer, &size, None, None);
        assert_eq!(
            budget.total,
            baseline(indexer),
            "{indexer:?}: tiny workspace should get baseline budget"
        );
        assert_eq!(budget.per_invocation, budget.total);
        assert!(
            budget.per_partition.is_none(),
            "{indexer:?}: no partitioning without partition_count > 1"
        );
        assert!(
            !budget.reason.is_empty(),
            "{indexer:?}: reason must be populated"
        );
    }
}

#[test]
fn baseline_values_match_spike_recommendation() {
    assert_eq!(
        baseline(SupportedIndexer::RustAnalyzer),
        Duration::from_secs(300)
    );
    assert_eq!(
        baseline(SupportedIndexer::TypeScript),
        Duration::from_secs(120)
    );
    assert_eq!(baseline(SupportedIndexer::Java), Duration::from_secs(120));
    assert_eq!(baseline(SupportedIndexer::Go), Duration::from_secs(60));
    assert_eq!(baseline(SupportedIndexer::Clang), Duration::from_secs(60));
    assert_eq!(baseline(SupportedIndexer::Python), Duration::from_secs(60));
    assert_eq!(baseline(SupportedIndexer::Ruby), Duration::from_secs(60));
    assert_eq!(baseline(SupportedIndexer::DotNet), Duration::from_secs(60));
}

#[test]
fn max_cap_values_match_spike_recommendation() {
    assert_eq!(
        max_cap(SupportedIndexer::RustAnalyzer),
        Duration::from_secs(1200)
    );
    assert_eq!(
        max_cap(SupportedIndexer::TypeScript),
        Duration::from_secs(900)
    );
    assert_eq!(max_cap(SupportedIndexer::Java), Duration::from_secs(600));
    assert_eq!(max_cap(SupportedIndexer::Go), Duration::from_secs(300));
    assert_eq!(max_cap(SupportedIndexer::Clang), Duration::from_secs(300));
    assert_eq!(max_cap(SupportedIndexer::Python), Duration::from_secs(300));
    assert_eq!(max_cap(SupportedIndexer::Ruby), Duration::from_secs(300));
    assert_eq!(max_cap(SupportedIndexer::DotNet), Duration::from_secs(300));
}

// --- Source-count / byte scaling --------------------------------------

#[test]
fn source_file_count_scales_budget() {
    // 500 files → ceil(500/250) = 2 steps → 300 + 2*30 = 360s for Rust.
    let size = WorkspaceSizeHint {
        source_file_count: 500,
        source_bytes: 0,
        partition_count: 1,
    };
    let budget = budget_for_indexer(SupportedIndexer::RustAnalyzer, &size, None, None);
    assert_eq!(budget.total, Duration::from_secs(360));
}

#[test]
fn source_bytes_scale_budget() {
    // 50 MiB → ceil(50MiB/25MiB) = 2 steps → 60 + 2*30 = 120s for Go.
    let size = WorkspaceSizeHint {
        source_file_count: 0,
        source_bytes: 50 * 1024 * 1024,
        partition_count: 1,
    };
    let budget = budget_for_indexer(SupportedIndexer::Go, &size, None, None);
    assert_eq!(budget.total, Duration::from_secs(120));
}

#[test]
fn combined_file_and_byte_scaling() {
    // 300 files (2 steps) + 30 MiB (2 steps) → 300 + 60 + 60 = 420s from the
    // LINEAR model. Asserted through `scaled_budget` because for
    // rust-analyzer the measured superlinear model now (correctly) dominates
    // at this size — 30 MiB of Rust really does cost ~3300s, and pretending
    // otherwise is the 540s collapse in miniature.
    let size = WorkspaceSizeHint {
        source_file_count: 300,
        source_bytes: 30 * 1024 * 1024,
        partition_count: 1,
    };
    let (linear, _, _) = scaled_budget(SupportedIndexer::RustAnalyzer, &size);
    assert_eq!(linear, Duration::from_secs(420));

    // The same size on an indexer with no measured superlinear model keeps
    // the linear budget end to end.
    let go = budget_for_indexer(SupportedIndexer::Go, &size, None, None);
    assert_eq!(go.total, Duration::from_secs(180)); // 60 + 60 + 60
}

#[test]
fn large_workspace_capped_by_indexer_max() {
    // 100k files → 400 steps → way over the Rust max of 1200s.
    let size = WorkspaceSizeHint {
        source_file_count: 100_000,
        source_bytes: 0,
        partition_count: 1,
    };
    let budget = budget_for_indexer(SupportedIndexer::RustAnalyzer, &size, None, None);
    assert_eq!(budget.total, Duration::from_secs(1200));
    assert_eq!(budget.per_invocation, Duration::from_secs(1200));
}

#[test]
fn large_typescript_workspace_scales_above_old_short_cap() {
    // 5000 files → 20 steps → 120 + 600 = 720s, above old 600s cap.
    let size = WorkspaceSizeHint {
        source_file_count: 5000,
        source_bytes: 0,
        partition_count: 1,
    };
    let budget = budget_for_indexer(SupportedIndexer::TypeScript, &size, None, None);
    assert_eq!(budget.total, Duration::from_secs(720));
    assert!(
        budget.total > Duration::from_secs(600),
        "should scale above old 600s cap"
    );
}

// --- Prior-duration scaling -------------------------------------------

#[test]
fn prior_p95_raises_budget_to_double() {
    // Formula for Rust with 0 files: 300s.  Prior p95 = 250s → p95*2 = 500s.
    let size = WorkspaceSizeHint::default();
    let prior = PriorIndexerTiming {
        p95_ms: Some(250_000),
        ..Default::default()
    };
    let budget = budget_for_indexer(SupportedIndexer::RustAnalyzer, &size, Some(&prior), None);
    assert_eq!(budget.total, Duration::from_secs(500));
    assert!(budget.reason.contains("prior timing raised"));
}

#[test]
fn prior_last_success_raises_budget_to_double() {
    // Formula for Go with 0 files: 60s.  last_success = 120s → 240s.
    let size = WorkspaceSizeHint::default();
    let prior = PriorIndexerTiming {
        last_success_ms: Some(120_000),
        ..Default::default()
    };
    let budget = budget_for_indexer(SupportedIndexer::Go, &size, Some(&prior), None);
    assert_eq!(budget.total, Duration::from_secs(240));
}

#[test]
fn prior_capped_by_indexer_max() {
    // Rust max is 1200s.  p95 = 700s → 1400s, capped to 1200s.
    let size = WorkspaceSizeHint::default();
    let prior = PriorIndexerTiming {
        p95_ms: Some(700_000),
        ..Default::default()
    };
    let budget = budget_for_indexer(SupportedIndexer::RustAnalyzer, &size, Some(&prior), None);
    assert_eq!(budget.total, Duration::from_secs(1200));
}

#[test]
fn prior_does_not_lower_formula_budget() {
    // Prior values lower than formula → formula wins.
    let size = WorkspaceSizeHint {
        source_file_count: 1000, // 4 steps → 300 + 120 = 420s for Rust
        source_bytes: 0,
        partition_count: 1,
    };
    let prior = PriorIndexerTiming {
        p95_ms: Some(50_000), // 100s → lower than 420s
        ..Default::default()
    };
    let budget = budget_for_indexer(SupportedIndexer::RustAnalyzer, &size, Some(&prior), None);
    assert_eq!(budget.total, Duration::from_secs(420));
    assert!(!budget.reason.contains("prior timing raised"));
}

// --- Timed-out headroom (adaptive ceiling) ----------------------------

#[test]
fn timed_out_headroom_raises_above_static_max_cap() {
    // The whole point: rust-analyzer keeps timing out at its 1200s cap.
    // Next warm should get ~1.5x the killed elapsed (1800s), ABOVE the
    // static 1200s max_cap, so it stops retrying at a too-small cap.
    let size = WorkspaceSizeHint::default();
    let prior = PriorIndexerTiming {
        last_timed_out_ms: Some(1_200_000),
        ..Default::default()
    };
    let budget = budget_for_indexer(SupportedIndexer::RustAnalyzer, &size, Some(&prior), None);
    assert_eq!(budget.total, Duration::from_secs(1800));
    assert_eq!(budget.per_invocation, Duration::from_secs(1800));
    assert!(budget.total > max_cap(SupportedIndexer::RustAnalyzer));
    assert!(budget.reason.contains("timed-out headroom raised"));
}

#[test]
fn over_cap_success_keeps_budget_above_proven_cost() {
    // The exact production oscillation (PR #1891 follow-up): the Rust
    // `server` workspace succeeds at 2390s, ABOVE the static 1200s cap.
    // The DB clears the timed-out high-water on success, so only
    // `last_success_ms` carries the cost. The next budget must stay above
    // 2390s (≈1.25x = 2987s), NOT snap back to the 1200s cap and get killed.
    let size = WorkspaceSizeHint::default();
    let prior = PriorIndexerTiming {
        // High-water reset on the success, exactly as the repository writes.
        last_success_ms: Some(2_390_000),
        last_timed_out_ms: None,
        ..Default::default()
    };
    let budget = budget_for_indexer(SupportedIndexer::RustAnalyzer, &size, Some(&prior), None);
    // 2390s * 5 / 4 = 2987.5s (Duration keeps sub-second precision).
    assert_eq!(budget.total, Duration::from_millis(2_987_500));
    assert_eq!(budget.per_invocation, Duration::from_millis(2_987_500));
    assert!(
        budget.total >= Duration::from_secs(2390),
        "budget must never forget a proven 2390s success cost"
    );
    assert!(budget.total > max_cap(SupportedIndexer::RustAnalyzer));
    assert!(
        budget.total
            <= adaptive_ceiling(
                SupportedIndexer::RustAnalyzer,
                Some(&prior),
                UNENCLOSED_HARD_CEILING
            )
    );
    assert!(budget.reason.contains("over-cap success headroom raised"));
}

#[test]
fn under_cap_success_stays_at_static_cap() {
    // A success comfortably under the static cap must NOT trigger the
    // over-cap headroom path — it stays clamped at the static max_cap
    // (via prior_based_budget's ×2 → clamp), never growing above it and
    // never shrinking below the default.
    let size = WorkspaceSizeHint::default();
    let prior = PriorIndexerTiming {
        last_success_ms: Some(800_000), // under the 1200s cap
        ..Default::default()
    };
    let budget = budget_for_indexer(SupportedIndexer::RustAnalyzer, &size, Some(&prior), None);
    assert_eq!(budget.total, max_cap(SupportedIndexer::RustAnalyzer));
    assert_eq!(budget.total, Duration::from_secs(1200));
    assert!(!budget.reason.contains("over-cap success headroom raised"));
}

#[test]
fn over_cap_success_headroom_bounded_by_the_hard_deadline_not_a_constant() {
    // A proven success cost whose 1.25x cushion exceeds every static bound
    // is clamped by the enclosing Job deadline — the only bound that
    // actually means anything — and NOT by `max_cap * 3`.
    let size = WorkspaceSizeHint::default();
    let prior = PriorIndexerTiming {
        last_success_ms: Some(10_000_000), // 1.25x = 12500s
        ..Default::default()
    };
    let budget = budget_with_bound(&size, Some(&prior), Duration::from_secs(7200));
    assert_eq!(budget.total, Duration::from_secs(7200));
    assert!(budget.reason.contains("CEILING BOUND"));
    // …and with a deadline that DOES accommodate the evidence, the model
    // grants what the evidence implies rather than a fixed 3600s.
    let roomy = budget_with_bound(&size, Some(&prior), Duration::from_secs(20_000));
    assert_eq!(roomy.total, Duration::from_secs(12_500));
    assert!(!roomy.reason.contains("CEILING BOUND"));
}

#[test]
fn timed_out_headroom_bounded_by_the_hard_deadline_not_a_constant() {
    let size = WorkspaceSizeHint::default();
    let prior = PriorIndexerTiming {
        last_timed_out_ms: Some(10_000_000), // 1.5x = 15000s
        ..Default::default()
    };
    let budget = budget_with_bound(&size, Some(&prior), Duration::from_secs(7200));
    assert_eq!(budget.total, Duration::from_secs(7200));
    assert!(budget.reason.contains("CEILING BOUND"));
}

#[test]
fn adaptive_ceiling_floor_is_three_times_max_cap_without_evidence() {
    // With no prior evidence at all the ceiling is exactly what it always
    // was — the redesign only ever RAISES it, never lowers it.
    for indexer in SupportedIndexer::ALL {
        assert_eq!(
            adaptive_ceiling(indexer, None, UNENCLOSED_HARD_CEILING),
            max_cap(indexer) * 3,
            "{indexer:?}: ceiling floor must remain 3x static max_cap"
        );
    }
}

#[test]
fn adaptive_ceiling_rises_with_proven_cost() {
    let prior = PriorIndexerTiming {
        last_success_ms: Some(3_522_197), // the measured production cost
        ..Default::default()
    };
    let ceiling = adaptive_ceiling(
        SupportedIndexer::RustAnalyzer,
        Some(&prior),
        UNENCLOSED_HARD_CEILING,
    );
    // 2x the proven cost, comfortably above the old flat 3600s clamp.
    assert_eq!(ceiling, Duration::from_millis(7_044_394));
    assert!(ceiling > max_cap(SupportedIndexer::RustAnalyzer) * 3);
}

#[test]
fn timed_out_headroom_grows_across_successive_timeouts() {
    // Model the intended escalation. Under the OLD flat `max_cap * 3`
    // ceiling this sequence dead-ended at 3600s and stayed there forever,
    // which is exactly how a workspace became permanently unwarmable. It
    // now keeps climbing with its own evidence, bounded by the Job
    // deadline.
    let size = WorkspaceSizeHint::default();
    let step = |timed_out_ms: u64| {
        let prior = PriorIndexerTiming {
            last_timed_out_ms: Some(timed_out_ms),
            ..Default::default()
        };
        budget_with_bound(&size, Some(&prior), Duration::from_secs(20_000)).total
    };
    assert_eq!(step(1_200_000), Duration::from_secs(1800));
    assert_eq!(step(1_800_000), Duration::from_secs(2700));
    // The old ceiling: the sequence used to flat-line from here.
    assert_eq!(step(2_700_000), Duration::from_secs(4050));
    assert_eq!(step(3_600_000), Duration::from_secs(5400));
    assert_eq!(step(5_400_000), Duration::from_secs(8100));
}

#[test]
fn timed_out_headroom_never_lowers_formula_budget() {
    // A tiny timed-out elapsed must not shrink a larger size-scaled budget.
    let size = WorkspaceSizeHint {
        source_file_count: 1000, // 4 steps → 300 + 120 = 420s for Rust
        source_bytes: 0,
        partition_count: 1,
    };
    let prior = PriorIndexerTiming {
        last_timed_out_ms: Some(60_000), // 1.5x = 90s < 420s
        ..Default::default()
    };
    let budget = budget_for_indexer(SupportedIndexer::RustAnalyzer, &size, Some(&prior), None);
    assert_eq!(budget.total, Duration::from_secs(420));
    assert!(!budget.reason.contains("timed-out headroom raised"));
}

#[test]
fn timed_out_headroom_wins_over_success_prior_when_larger() {
    // Both signals present: success p95 caps at max_cap (1200s) while the
    // timeout headroom climbs above it. The larger (headroom) wins.
    let size = WorkspaceSizeHint::default();
    let prior = PriorIndexerTiming {
        p95_ms: Some(700_000),              // 2x = 1400 → capped to 1200s
        last_success_ms: Some(500_000),     // 2x = 1000s
        last_timed_out_ms: Some(1_400_000), // 1.5x = 2100s
        ..Default::default()
    };
    let budget = budget_for_indexer(SupportedIndexer::RustAnalyzer, &size, Some(&prior), None);
    assert_eq!(budget.total, Duration::from_secs(2100));
    assert!(budget.reason.contains("prior timing raised"));
    assert!(budget.reason.contains("timed-out headroom raised"));
}

#[test]
fn timed_out_headroom_still_clamped_by_active_deadline() {
    // The adaptive headroom raises the budget, but an active deadline still
    // clamps it — the pod's activeDeadline is the hard ceiling.
    let (deadline, now) = fresh_deadline(1060, 60); // remaining = 1000s
    let size = WorkspaceSizeHint::default();
    let prior = PriorIndexerTiming {
        last_timed_out_ms: Some(1_200_000), // 1.5x = 1800s
        ..Default::default()
    };
    let budget = budget_for_indexer_at(
        SupportedIndexer::RustAnalyzer,
        &size,
        Some(&prior),
        Some(&deadline),
        now,
    );
    assert_eq!(budget.total, Duration::from_secs(1000));
    assert!(budget.reason.contains("timed-out headroom raised"));
    assert!(budget.reason.contains("clamped"));
}

#[test]
fn no_timeout_evidence_is_byte_identical_to_no_prior() {
    // A prior with only the (default) None timeout field must produce the
    // same budget as passing no prior at all — no-history behaviour is
    // preserved exactly.
    let size = WorkspaceSizeHint {
        source_file_count: 500,
        source_bytes: 10 * 1024 * 1024,
        partition_count: 1,
    };
    let empty_prior = PriorIndexerTiming::default();
    for indexer in SupportedIndexer::ALL {
        let with_none = budget_for_indexer(indexer, &size, None, None);
        let with_empty = budget_for_indexer(indexer, &size, Some(&empty_prior), None);
        assert_eq!(
            with_none, with_empty,
            "{indexer:?}: empty prior must equal no prior"
        );
    }
}

// --- Active-deadline clamp --------------------------------------------

#[test]
fn remaining_deadline_subtracts_elapsed_and_reserve() {
    let (deadline, now) = fresh_deadline(600, 60);
    let remaining = remaining_deadline(&deadline, now).expect("usable time remains");
    // 600 - 0 elapsed - 60 reserve = 540s.
    assert_eq!(remaining, Duration::from_secs(540));
}

#[test]
fn deadline_clamps_total_and_per_invocation() {
    // Rust formula: 300s.  Deadline remaining: 100s.  → total = 100s.
    let (deadline, now) = fresh_deadline(160, 60);
    let size = WorkspaceSizeHint::default();
    let budget = budget_for_indexer_at(
        SupportedIndexer::RustAnalyzer,
        &size,
        None,
        Some(&deadline),
        now,
    );
    assert_eq!(budget.total, Duration::from_secs(100));
    assert_eq!(budget.per_invocation, Duration::from_secs(100));
    assert!(budget.reason.contains("clamped"));
    assert!(budget.reason.contains("remaining deadline"));
}

#[test]
fn deadline_clamp_preserves_reserve() {
    // Max runtime 200s, reserve 150s → remaining 50s (with 0 elapsed).
    // Rust formula 300s → clamped to 50s.
    let (deadline, now) = fresh_deadline(200, 150);
    let size = WorkspaceSizeHint::default();
    let budget = budget_for_indexer_at(
        SupportedIndexer::RustAnalyzer,
        &size,
        None,
        Some(&deadline),
        now,
    );
    assert_eq!(budget.total, Duration::from_secs(50));
}

// --- Exhausted-deadline zero/skip reason ------------------------------

#[test]
fn exhausted_deadline_returns_zero_budget() {
    // max_runtime = 0 → remaining is always 0.
    let (deadline, now) = fresh_deadline(0, 0);
    let size = WorkspaceSizeHint::default();
    let budget = budget_for_indexer_at(
        SupportedIndexer::RustAnalyzer,
        &size,
        None,
        Some(&deadline),
        now,
    );
    assert_eq!(budget.total, Duration::ZERO);
    assert_eq!(budget.per_invocation, Duration::ZERO);
    assert_eq!(budget.per_partition, Some(Duration::ZERO));
    assert!(budget.reason.contains("deadline_exhausted"));
}

#[test]
fn exhausted_deadline_when_reserve_exceeds_runtime() {
    // max_runtime = 30s, reserve = 60s → 30 - 0 - 60 → 0 (saturating).
    let (deadline, now) = fresh_deadline(30, 60);
    let size = WorkspaceSizeHint::default();
    let budget = budget_for_indexer_at(
        SupportedIndexer::RustAnalyzer,
        &size,
        None,
        Some(&deadline),
        now,
    );
    assert_eq!(budget.total, Duration::ZERO);
    assert!(budget.reason.contains("deadline_exhausted"));
}

#[test]
fn exhausted_deadline_after_elapsed_time() {
    // Simulate a deadline where the elapsed time has already exceeded
    // max_runtime. We construct the deadline with `started_at` in the past
    // so `remaining_deadline` saturates to zero.
    let clock = SystemClock::new();
    let now = clock.now_instant();
    let started_at = now - Duration::from_secs(10);
    let deadline = ActiveDeadline {
        started_at,
        max_runtime: Duration::from_secs(5),
        reserve: Duration::ZERO,
    };
    let size = WorkspaceSizeHint::default();
    let budget = budget_for_indexer_at(
        SupportedIndexer::RustAnalyzer,
        &size,
        None,
        Some(&deadline),
        now,
    );
    assert_eq!(budget.total, Duration::ZERO);
    assert!(budget.reason.contains("deadline_exhausted"));
}

// --- Bounded per-partition budgets ------------------------------------

#[test]
fn partition_count_produces_bounded_per_partition_budget() {
    // Rust formula 300s, 4 partitions.
    // share = 300/4 = 75s, doubled = 150s, clamp [10, 300] → 150s.
    let size = WorkspaceSizeHint {
        source_file_count: 0,
        source_bytes: 0,
        partition_count: 4,
    };
    let budget = budget_for_indexer(SupportedIndexer::Go, &size, None, None);
    // Go baseline is 60s.
    let total = Duration::from_secs(60);
    assert_eq!(budget.total, total);
    let per_partition = budget.per_partition.expect("partitioning budget");
    // share = 60/4 = 15s, doubled = 30s, clamp [10, 60] → 30s.
    assert_eq!(per_partition, Duration::from_secs(30));
}

#[test]
fn per_partition_never_exceeds_total() {
    // 2 partitions with a tiny total budget.
    let size = WorkspaceSizeHint {
        source_file_count: 0,
        source_bytes: 0,
        partition_count: 2,
    };
    let budget = budget_for_indexer(SupportedIndexer::Go, &size, None, None);
    // Go baseline: 60s. share = 30, doubled = 60. clamp [10, 60] → 60.
    let per_partition = budget.per_partition.expect("partitioning budget");
    assert!(per_partition <= budget.total);
    assert_eq!(per_partition, Duration::from_secs(60));
}

#[test]
fn per_partition_clamped_to_minimum_when_share_is_tiny() {
    // Deadline-clamped total of 5s with 4 partitions.
    // share = 5/4 = 1s (rounded), doubled = 2s, clamp [10, 5] → 5s (min wins
    // vs total since total < MIN).
    let (deadline, now) = fresh_deadline(65, 60); // remaining = 5s
    let size = WorkspaceSizeHint {
        source_file_count: 0,
        source_bytes: 0,
        partition_count: 4,
    };
    let budget = budget_for_indexer_at(
        SupportedIndexer::RustAnalyzer,
        &size,
        None,
        Some(&deadline),
        now,
    );
    assert_eq!(budget.total, Duration::from_secs(5));
    let per_partition = budget.per_partition.expect("partitioning budget");
    // With total < MIN_PER_PARTITION, per_partition = total = 5s.
    assert_eq!(per_partition, Duration::from_secs(5));
}

#[test]
fn no_partition_when_partition_count_is_one() {
    let size = WorkspaceSizeHint {
        source_file_count: 0,
        source_bytes: 0,
        partition_count: 1,
    };
    let budget = budget_for_indexer(SupportedIndexer::RustAnalyzer, &size, None, None);
    assert!(budget.per_partition.is_none());
}

// --- estimate_workspace_size filesystem walk --------------------------

#[test]
fn estimate_workspace_size_counts_source_files() {
    let tmp = tempfile::Builder::new()
        .prefix("djinn-budget-size-")
        .tempdir_in(".")
        .expect("create tempdir");

    // Three Rust files + one non-Rust file.
    std::fs::write(tmp.path().join("a.rs"), "fn main() {}\n").unwrap();
    std::fs::write(tmp.path().join("b.rs"), "mod b;\n").unwrap();
    std::fs::write(tmp.path().join("c.rs"), "mod c;\n").unwrap();
    std::fs::write(tmp.path().join("d.txt"), "not source\n").unwrap();

    let hint = estimate_workspace_size(tmp.path(), SupportedIndexer::RustAnalyzer);
    assert_eq!(hint.source_file_count, 3);
    assert!(hint.source_bytes > 0);
    assert_eq!(hint.partition_count, 1);
}

#[test]
fn estimate_workspace_size_ignores_vendor_dirs() {
    let tmp = tempfile::Builder::new()
        .prefix("djinn-budget-ignored-")
        .tempdir_in(".")
        .expect("create tempdir");

    std::fs::create_dir_all(tmp.path().join("vendor")).unwrap();
    std::fs::write(tmp.path().join("main.go"), "package main\n").unwrap();
    std::fs::write(tmp.path().join("vendor/vendored.go"), "package vendored\n").unwrap();

    let hint = estimate_workspace_size(tmp.path(), SupportedIndexer::Go);
    assert_eq!(hint.source_file_count, 1, "vendor dir must be pruned");
}

#[test]
fn estimate_workspace_size_nonexistent_root_is_zero() {
    let hint = estimate_workspace_size(
        Path::new("/nonexistent-djinn-budget-test"),
        SupportedIndexer::RustAnalyzer,
    );
    assert_eq!(hint.source_file_count, 0);
    assert_eq!(hint.source_bytes, 0);
    assert_eq!(hint.partition_count, 1);
}

// --- Integration: prior + deadline ------------------------------------

#[test]
fn prior_and_deadline_combine_correctly() {
    // Rust baseline 300s.  Prior p95 = 250s → 500s.
    // Deadline remaining = 400s.  Formula takes prior (500s), clamps to 400s.
    let (deadline, now) = fresh_deadline(460, 60); // remaining = 400s
    let size = WorkspaceSizeHint::default();
    let prior = PriorIndexerTiming {
        p95_ms: Some(250_000),
        ..Default::default()
    };
    let budget = budget_for_indexer_at(
        SupportedIndexer::RustAnalyzer,
        &size,
        Some(&prior),
        Some(&deadline),
        now,
    );
    assert_eq!(budget.total, Duration::from_secs(400));
    assert!(budget.reason.contains("prior timing raised"));
    assert!(budget.reason.contains("clamped"));
}

// --- The 2026-07-27 adaptive-budget cliff ------------------------------

/// The headline defect. rust-analyzer measured 3 522 197 ms against a hard
/// 3600s ceiling — 2.2% of margin — and BOTH adaptive paths ended in
/// `.min(ceiling)`, so crossing 3600s made the workspace permanently
/// unwarmable. A cost we have already PAID must still be budgetable.
#[test]
fn measured_production_cost_above_the_old_ceiling_still_gets_a_workable_budget() {
    const MEASURED_MS: u64 = 3_522_197;
    const OLD_ABSOLUTE_CEILING: Duration = Duration::from_secs(3600);

    let size = measured_production_workspace();
    let prior = PriorIndexerTiming {
        last_success_ms: Some(MEASURED_MS),
        ..Default::default()
    };
    let budget = budget_with_bound(&size, Some(&prior), Duration::from_secs(7200));

    assert!(
        budget.total > OLD_ABSOLUTE_CEILING,
        "a proven {MEASURED_MS}ms cost must be budgetable above the old \
         {OLD_ABSOLUTE_CEILING:?} clamp, got {:?}",
        budget.total
    );
    // …and with real headroom over the proven cost, not merely equal to it.
    assert_eq!(budget.total, Duration::from_millis(MEASURED_MS) * 5 / 4);
    assert!(budget.total > Duration::from_millis(MEASURED_MS));
}

/// The property that actually matters: a workspace whose cost grows
/// steadily must never reach a state it cannot recover from. Under the old
/// flat ceiling this loop pins at 3600s from warm 3 onward and every
/// subsequent warm fails forever.
#[test]
fn a_steadily_growing_workspace_is_never_permanently_stranded() {
    let size = measured_production_workspace();
    let hard_bound = Duration::from_secs(20_000);

    // Start just under the old ceiling and grow 5% per warm — slower than
    // the measured 3.26x-in-45-days, so this is a conservative model.
    let mut true_cost = Duration::from_secs(3400);
    let mut prior = PriorIndexerTiming::default();
    let mut consecutive_failures = 0usize;
    let mut worst_streak = 0usize;
    let mut successes = 0usize;

    for warm in 0..40 {
        let budget = budget_with_bound(&size, Some(&prior), hard_bound).total;
        if budget >= true_cost {
            successes += 1;
            consecutive_failures = 0;
            // The repository clears the timed-out high-water on success.
            prior.last_success_ms = Some(true_cost.as_millis() as u64);
            prior.last_timed_out_ms = None;
        } else {
            consecutive_failures += 1;
            worst_streak = worst_streak.max(consecutive_failures);
            let killed_at = budget.as_millis() as u64;
            prior.last_timed_out_ms = Some(prior.last_timed_out_ms.unwrap_or(0).max(killed_at));
        }
        assert!(
            consecutive_failures <= 3,
            "warm {warm}: {consecutive_failures} consecutive failures — the \
             budget is no longer able to catch up with real cost (budget \
             {budget:?}, true cost {true_cost:?})"
        );
        true_cost = true_cost.mul_f64(1.05);
    }

    assert!(
        successes >= 35,
        "expected the workspace to keep warming; only {successes}/40 warms \
         were budgeted enough time (worst failure streak {worst_streak})"
    );
}

/// Even a workspace that grows FASTER than the success cushion (1.25x)
/// between warms must recover: the timeout path keeps ratcheting.
#[test]
fn a_workspace_growing_faster_than_its_cushion_still_recovers() {
    let size = WorkspaceSizeHint::default();
    let hard_bound = Duration::from_secs(400_000);
    let mut true_cost = Duration::from_secs(3400);
    // Start from a workspace that HAS warmed successfully at 3400s, then
    // let its cost explode. Recovery is the timeout ratchet catching up.
    let mut prior = PriorIndexerTiming {
        last_success_ms: Some(3_400_000),
        ..Default::default()
    };
    let mut consecutive_failures = 0usize;

    for warm in 0..25 {
        // Beyond this the workspace genuinely does not fit the Job deadline
        // and the operator lever — not the model — is the answer.
        if true_cost.saturating_mul(2) > hard_bound {
            break;
        }
        let budget = budget_with_bound(&size, Some(&prior), hard_bound).total;
        if budget >= true_cost {
            consecutive_failures = 0;
            prior.last_success_ms = Some(true_cost.as_millis() as u64);
            prior.last_timed_out_ms = None;
        } else {
            consecutive_failures += 1;
            prior.last_timed_out_ms = Some(
                prior
                    .last_timed_out_ms
                    .unwrap_or(0)
                    .max(budget.as_millis() as u64),
            );
        }
        assert!(
            consecutive_failures <= 4,
            "warm {warm}: stranded after {consecutive_failures} failures \
             (budget {budget:?}, true cost {true_cost:?})"
        );
        // 40% growth per warm — far beyond anything measured.
        true_cost = true_cost.mul_f64(1.4);
    }
}

/// A workspace clipped by the Job deadline must SAY SO. The old clamp was
/// silent, which is why a permanently-unwarmable workspace looked healthy.
#[test]
fn a_clipped_budget_names_the_deadline_and_the_operator_lever() {
    let size = measured_production_workspace();
    let prior = PriorIndexerTiming {
        last_success_ms: Some(3_522_197),
        ..Default::default()
    };
    let budget = budget_with_bound(&size, Some(&prior), Duration::from_secs(1800));
    assert_eq!(budget.total, Duration::from_secs(1800));
    assert!(budget.reason.contains("CEILING BOUND"), "{}", budget.reason);
    assert!(
        budget.reason.contains("DJINN_K8S_WARM_JOB_TIMEOUT_SECONDS"),
        "the diagnostic must name the lever that fixes it: {}",
        budget.reason
    );
}

// --- The 540s no-history collapse --------------------------------------

/// With NO persisted `scip_indexer_timing` row — project recreated, slug
/// changed, FK cascade — the budget used to collapse to the linear model's
/// 540s for a workspace that needs 3522s, then climb 1.5x per warm across
/// ~6 consecutive hour-long failures. The measured superlinear model closes
/// that hole on the FIRST warm.
#[test]
fn absent_timing_row_no_longer_collapses_the_budget() {
    const OLD_LINEAR_MODEL: Duration = Duration::from_secs(540);
    const MEASURED_COST: Duration = Duration::from_millis(3_522_197);

    let size = measured_production_workspace();
    let budget = budget_with_bound(&size, None, Duration::from_secs(7200));

    assert!(
        budget.total > OLD_LINEAR_MODEL * 5,
        "no-history budget {:?} is still near the old {OLD_LINEAR_MODEL:?} \
         collapse",
        budget.total
    );
    assert!(
        budget.total >= MEASURED_COST,
        "a workspace with no history must still be budgeted its MEASURED \
         cost {MEASURED_COST:?}, got {:?}",
        budget.total
    );
}

/// The superlinear coefficient is fitted, so pin it against BOTH measured
/// points. A silent drift here re-opens the collapse.
#[test]
fn superlinear_model_reproduces_both_measured_points() {
    // 2026-07-27: 31.6 MiB → 3522s measured.
    let now = superlinear_estimate(
        SupportedIndexer::RustAnalyzer,
        &measured_production_workspace(),
    );
    assert!(
        now >= Duration::from_secs(3400) && now <= Duration::from_secs(3800),
        "31.6 MiB should model to ~3522s, got {now:?}"
    );

    // 2026-06-12: 9.7 MiB → ~527s measured. The 1.5 exponent errs high on
    // the small end, which is the safe direction for a timeout.
    let then = superlinear_estimate(
        SupportedIndexer::RustAnalyzer,
        &WorkspaceSizeHint {
            source_file_count: 642,
            source_bytes: 10_171_187, // 9.7 MiB
            partition_count: 1,
        },
    );
    assert!(
        then >= Duration::from_secs(527) && then <= Duration::from_secs(800),
        "9.7 MiB should model to ~527-600s, got {then:?}"
    );
}

/// Indexers with no measured cost model keep exactly the linear behaviour
/// they ship with — an unmeasured coefficient is not invented.
#[test]
fn unmeasured_indexers_keep_the_linear_model() {
    for indexer in SupportedIndexer::ALL {
        if indexer == SupportedIndexer::RustAnalyzer {
            continue;
        }
        assert_eq!(
            superlinear_estimate(indexer, &measured_production_workspace()),
            Duration::ZERO,
            "{indexer:?} has no measured superlinear coefficient"
        );
    }
}

/// Small Rust workspaces must not be inflated by the superlinear term: the
/// baseline still wins well past any workspace we would call small.
#[test]
fn small_rust_workspaces_are_unaffected_by_the_superlinear_term() {
    let size = WorkspaceSizeHint {
        source_file_count: 200,
        source_bytes: 2 * 1024 * 1024, // 2 MiB → 20 * 2^1.5 ≈ 57s
        partition_count: 1,
    };
    let budget = budget_with_bound(&size, None, Duration::from_secs(7200));
    // baseline 300 + 1 file-step + 1 byte-step = 360s, unchanged.
    assert_eq!(budget.total, Duration::from_secs(360));
}

// --- Hard bound resolution ---------------------------------------------

#[test]
fn hard_bound_follows_the_projected_job_deadline() {
    assert_eq!(
        hard_budget_ceiling_from(Some("7200")),
        Duration::from_secs(7200)
    );
    assert_eq!(
        hard_budget_ceiling_from(Some(" 10800 ")),
        Duration::from_secs(10800)
    );
    // Unset / empty / garbage / zero must never SHRINK the bound.
    for raw in [None, Some(""), Some("nonsense"), Some("0"), Some("-5")] {
        assert_eq!(
            hard_budget_ceiling_from(raw),
            UNENCLOSED_HARD_CEILING,
            "{raw:?} must fall back to the unenclosed ceiling"
        );
    }
}

/// Raising the Job deadline must visibly raise what a workspace can be
/// granted — that is the property that makes the bound escapable.
#[test]
fn raising_the_job_deadline_raises_the_grantable_budget() {
    let size = WorkspaceSizeHint::default();
    let prior = PriorIndexerTiming {
        last_timed_out_ms: Some(6_000_000), // wants 9000s
        ..Default::default()
    };
    let tight = budget_with_bound(&size, Some(&prior), Duration::from_secs(7200));
    let roomy = budget_with_bound(&size, Some(&prior), Duration::from_secs(12_000));
    assert_eq!(tight.total, Duration::from_secs(7200));
    assert_eq!(roomy.total, Duration::from_secs(9000));
    assert!(roomy.total > tight.total);
}
