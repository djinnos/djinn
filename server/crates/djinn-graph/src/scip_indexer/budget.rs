//! Pure, unit-testable budget primitives for SCIP indexer timeout scaling.
//!
//! This module computes deterministic time budgets for SCIP indexer invocations
//! based on workspace size hints, optional prior timing data, and an optional
//! active deadline clamp. It has no dependency on real indexer binaries and is
//! fully unit-testable.
//!
//! Budget formula (from spike 1rzz / roadmap 5z90):
//! 1. Start from a per-indexer baseline (Rust 300s, TypeScript 120s, …).
//! 2. Add size scale: `ceil(files / 250) * 30s` + `ceil(bytes / 25 MiB) * 30s`,
//!    capped by a per-indexer maximum.
//! 3. If prior p95/last-success exists, raise to `max(formula, p95 * 2,
//!    last_success * 2)`, still capped by the indexer max.
//! 4. Clamp to active deadline: `usable = max_runtime - elapsed - reserve`.
//!    If `usable == 0`, return a zero budget so the caller can skip with a
//!    `deadline_exhausted` status detail. Otherwise `total = min(formula,
//!    usable)`.
//! 5. For partition-capable indexers (`partition_count > 1`), derive a bounded
//!    per-partition budget from the cumulative total.

use std::path::Path;
use std::time::{Duration, Instant};

use super::SupportedIndexer;

// ---------------------------------------------------------------------------
// Scaling constants
// ---------------------------------------------------------------------------

/// Source files per scaling step.
const SOURCE_FILES_PER_STEP: usize = 250;
/// Duration added per source-file scaling step.
const SOURCE_FILE_STEP_DURATION: Duration = Duration::from_secs(30);

/// Source bytes per scaling step (25 MiB).
const SOURCE_BYTES_PER_STEP: u64 = 25 * 1024 * 1024;
/// Duration added per source-byte scaling step.
const SOURCE_BYTES_STEP_DURATION: Duration = Duration::from_secs(30);

/// Minimum per-partition budget for partition-capable indexers.
const MIN_PER_PARTITION: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Active warm-job deadline with a reserve for post-indexing work.
///
/// `reserve` is time set aside for parse/build/cache writes after indexing
/// completes (e.g. 60s). The budget model never spends the reserve on indexer
/// invocations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ActiveDeadline {
    /// When the warm job (or indexer phase) started.
    pub started_at: Instant,
    /// Maximum total runtime from `started_at`.
    pub max_runtime: Duration,
    /// Time reserved for post-indexing work.
    pub reserve: Duration,
}

/// Cheaply-collectable hints about the size of a workspace for budget scaling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceSizeHint {
    /// Number of source files relevant to this indexer under the workspace.
    pub source_file_count: usize,
    /// Total byte size of those source files.
    pub source_bytes: u64,
    /// Number of below-workspace partitions (Go packages, Clang translation
    /// units). Defaults to 1 for workspace/project-only indexers.
    pub partition_count: usize,
}

impl Default for WorkspaceSizeHint {
    fn default() -> Self {
        Self {
            source_file_count: 0,
            source_bytes: 0,
            partition_count: 1,
        }
    }
}

/// Prior timing observations for an indexer, used to raise budgets when past
/// runs were slower than the formula-based estimate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PriorIndexerTiming {
    pub p50_ms: Option<u64>,
    pub p95_ms: Option<u64>,
    pub last_success_ms: Option<u64>,
}

/// A computed indexer budget with a human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexerBudget {
    /// Total cumulative budget for all invocations of this indexer/workspace.
    pub total: Duration,
    /// Per-invocation cap. Used as the process timeout for workspace-level
    /// runs. Equals `total` for workspace/project-only indexers.
    pub per_invocation: Duration,
    /// Per-partition cap for partition-capable indexers (Go packages, Clang
    /// translation units). `None` for workspace/project-only indexers.
    pub per_partition: Option<Duration>,
    /// Human-readable explanation of how the budget was derived. Useful for
    /// status-detail strings and operator logs.
    pub reason: String,
}

// ---------------------------------------------------------------------------
// Per-indexer baseline / max caps
// ---------------------------------------------------------------------------

/// Baseline budget for an indexer before size scaling.
fn baseline(indexer: SupportedIndexer) -> Duration {
    match indexer {
        SupportedIndexer::RustAnalyzer => Duration::from_secs(300),
        SupportedIndexer::TypeScript | SupportedIndexer::Java => Duration::from_secs(120),
        SupportedIndexer::Go
        | SupportedIndexer::Clang
        | SupportedIndexer::Python
        | SupportedIndexer::Ruby
        | SupportedIndexer::DotNet => Duration::from_secs(60),
    }
}

/// Maximum budget cap for an indexer (applied before deadline clamping).
fn max_cap(indexer: SupportedIndexer) -> Duration {
    match indexer {
        SupportedIndexer::RustAnalyzer => Duration::from_secs(1200),
        SupportedIndexer::TypeScript => Duration::from_secs(900),
        SupportedIndexer::Java => Duration::from_secs(600),
        SupportedIndexer::Go
        | SupportedIndexer::Clang
        | SupportedIndexer::Python
        | SupportedIndexer::Ruby
        | SupportedIndexer::DotNet => Duration::from_secs(300),
    }
}

/// File extensions that count as "source" for each indexer's size estimation
/// and SCIP cache key computation.
pub(crate) fn source_extensions(indexer: SupportedIndexer) -> &'static [&'static str] {
    match indexer {
        SupportedIndexer::RustAnalyzer => &["rs"],
        SupportedIndexer::TypeScript => &["ts", "tsx", "js", "jsx", "mjs", "cjs"],
        SupportedIndexer::Python => &["py"],
        SupportedIndexer::Go => &["go"],
        SupportedIndexer::Java => &["java"],
        SupportedIndexer::Clang => &["c", "cc", "cpp", "cxx", "h", "hh", "hpp", "hxx"],
        SupportedIndexer::Ruby => &["rb"],
        SupportedIndexer::DotNet => &["cs"],
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Walk a workspace root and collect source-file count / byte-size hints.
///
/// Uses the same ignored-directory pruning as workspace discovery so vendored
/// / generated / build-output trees are excluded. `partition_count` is always
/// `1` here — partitioning is a later task that will populate it.
pub(crate) fn estimate_workspace_size(root: &Path, indexer: SupportedIndexer) -> WorkspaceSizeHint {
    let extensions = source_extensions(indexer);
    let mut source_file_count = 0usize;
    let mut source_bytes = 0u64;

    // Errors from the directory walk are non-fatal — we simply use whatever
    // counts were collected so far.
    let _ = super::workspaces::visit_dirs(root, &mut |path| {
        if let Some(ext) = path.extension().and_then(|e| e.to_str())
            && extensions.contains(&ext)
        {
            source_file_count += 1;
            if let Ok(metadata) = std::fs::metadata(path) {
                source_bytes += metadata.len();
            }
        }
        Ok(())
    });

    WorkspaceSizeHint {
        source_file_count,
        source_bytes,
        partition_count: 1,
    }
}

/// Remaining usable time under an active deadline, or `None` if exhausted.
///
/// `usable = max_runtime - elapsed(now) - reserve`. Returns `None` when the
/// result is zero or would underflow, signalling the caller to skip new
/// invocations.
pub(crate) fn remaining_deadline(deadline: &ActiveDeadline, now: Instant) -> Option<Duration> {
    let elapsed = now.saturating_duration_since(deadline.started_at);
    let usable = deadline
        .max_runtime
        .saturating_sub(elapsed)
        .saturating_sub(deadline.reserve);
    if usable == Duration::ZERO {
        None
    } else {
        Some(usable)
    }
}

/// Compute a budget for an indexer invocation.
///
/// Combines the deterministic size-scaling formula with optional prior-timing
/// scaling and an optional active-deadline clamp. See the module docs for the
/// full formula.
pub(crate) fn budget_for_indexer(
    indexer: SupportedIndexer,
    size: &WorkspaceSizeHint,
    prior: Option<&PriorIndexerTiming>,
    deadline: Option<&ActiveDeadline>,
) -> IndexerBudget {
    budget_for_indexer_at(indexer, size, prior, deadline, Instant::now())
}

// ---------------------------------------------------------------------------
// Internal implementation
// ---------------------------------------------------------------------------

/// Budget derived purely from prior timing observations, capped by `cap`.
/// Returns `Duration::ZERO` when no timing data is available.
fn prior_based_budget(prior: &PriorIndexerTiming, cap: Duration) -> Duration {
    let candidates = [prior.p95_ms, prior.last_success_ms];
    let prior_max = candidates
        .into_iter()
        .flatten()
        .map(|ms| Duration::from_millis(ms).saturating_mul(2))
        .max()
        .unwrap_or(Duration::ZERO);
    prior_max.min(cap)
}

/// Formula-based budget (baseline + size scaling), capped by the indexer max.
fn scaled_budget(indexer: SupportedIndexer, size: &WorkspaceSizeHint) -> (Duration, usize, u64) {
    let base = baseline(indexer);
    let cap = max_cap(indexer);

    let file_steps = size.source_file_count.div_ceil(SOURCE_FILES_PER_STEP);
    let byte_steps = size.source_bytes.div_ceil(SOURCE_BYTES_PER_STEP);

    let file_scale =
        SOURCE_FILE_STEP_DURATION.saturating_mul(u32::try_from(file_steps).unwrap_or(u32::MAX));
    let byte_scale =
        SOURCE_BYTES_STEP_DURATION.saturating_mul(u32::try_from(byte_steps).unwrap_or(u32::MAX));

    let total = base
        .saturating_add(file_scale)
        .saturating_add(byte_scale)
        .min(cap);

    (total, file_steps, byte_steps)
}

/// Testable variant of [`budget_for_indexer`] that accepts an explicit `now`
/// for deterministic deadline calculations.
fn budget_for_indexer_at(
    indexer: SupportedIndexer,
    size: &WorkspaceSizeHint,
    prior: Option<&PriorIndexerTiming>,
    deadline: Option<&ActiveDeadline>,
    now: Instant,
) -> IndexerBudget {
    let cap = max_cap(indexer);

    // Step 1–2: baseline + size scaling, capped by indexer max.
    let (mut total, file_steps, byte_steps) = scaled_budget(indexer, size);
    let mut reason_parts: Vec<String> = vec![format!(
        "baseline+scale: {} file-steps, {} byte-steps, capped at {:?}",
        file_steps, byte_steps, cap
    )];

    // Step 3: prior-timing scaling.
    if let Some(prior) = prior {
        let prior_budget = prior_based_budget(prior, cap);
        if prior_budget > total {
            total = prior_budget;
            reason_parts.push(format!("prior timing raised to {:?}", prior_budget));
        }
    }

    // Step 4: active-deadline clamp.
    if let Some(deadline) = deadline {
        match remaining_deadline(deadline, now) {
            None => {
                return IndexerBudget {
                    total: Duration::ZERO,
                    per_invocation: Duration::ZERO,
                    per_partition: Some(Duration::ZERO),
                    reason: "deadline_exhausted: no usable time remaining after reserve"
                        .to_string(),
                };
            }
            Some(remaining) => {
                if remaining < total {
                    total = remaining;
                    reason_parts.push(format!("clamped to {:?} remaining deadline", remaining));
                }
            }
        }
    }

    // Step 5: per-partition budget for partition-capable indexers.
    let per_partition = if size.partition_count > 1 {
        let share = total
            .checked_div(u32::try_from(size.partition_count).unwrap_or(u32::MAX))
            .unwrap_or(Duration::ZERO);
        let doubled = share.saturating_mul(2);
        // Clamp to [MIN_PER_PARTITION, total] so each partition gets at least
        // a useful floor but never more than the cumulative total.
        let bounded = doubled.max(MIN_PER_PARTITION).min(total);
        Some(bounded)
    } else {
        None
    };

    let reason = reason_parts.join("; ");

    IndexerBudget {
        total,
        per_invocation: total,
        per_partition,
        reason,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- Helpers -----------------------------------------------------------

    /// Build a deadline whose `started_at` is `now` so tests are deterministic.
    fn fresh_deadline(max_secs: u64, reserve_secs: u64) -> (ActiveDeadline, Instant) {
        let now = Instant::now();
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
        // 300 files (2 steps) + 30 MiB (2 steps) → 300 + 60 + 60 = 420s for Rust.
        let size = WorkspaceSizeHint {
            source_file_count: 300,
            source_bytes: 30 * 1024 * 1024,
            partition_count: 1,
        };
        let budget = budget_for_indexer(SupportedIndexer::RustAnalyzer, &size, None, None);
        assert_eq!(budget.total, Duration::from_secs(420));
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
        let started_at = Instant::now() - Duration::from_secs(10);
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
            Instant::now(),
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
}
