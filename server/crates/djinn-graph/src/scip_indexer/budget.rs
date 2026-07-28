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
//! 3. Where a *measured* superlinear cost model exists for the indexer, raise
//!    to `coefficient * MiB^1.5`. The linear model above underestimates a large
//!    Rust workspace by ~6.5x, and that estimate is what a project falls back
//!    to when it has no persisted timing row at all.
//! 4. If prior p95/last-success exists, raise to `max(formula, p95 * 2,
//!    last_success * 2)`, still capped by the indexer max. A success whose
//!    elapsed *exceeded* the static max_cap additionally raises the budget to
//!    ~1.25x that proven cost (bounded by the adaptive ceiling), so a cost we
//!    already paid for is never forgotten when the timed-out high-water resets.
//! 5. Clamp to active deadline: `usable = max_runtime - elapsed - reserve`.
//!    If `usable == 0`, return a zero budget so the caller can skip with a
//!    `deadline_exhausted` status detail. Otherwise `total = min(formula,
//!    usable)`.
//! 6. For partition-capable indexers (`partition_count > 1`), derive a bounded
//!    per-partition budget from the cumulative total.
//!
//! # The adaptive ceiling is evidence-relative, never absolute
//!
//! Steps 3 and 4 may climb above the static `max_cap`, bounded by
//! [`adaptive_ceiling`]. That ceiling used to be the flat constant
//! `max_cap * 3`, and on 2026-07-27 the djinn workspace's own rust-analyzer
//! pass measured 3522s against it — 2.2% of margin against a 3600s wall. An
//! absolute clamp is a one-way door: once real cost crosses it, the model can
//! never grant enough time again and the workspace is permanently unwarmable.
//! The ceiling therefore now rises with *proven* cost and is bounded only by
//! the enclosing warm Job deadline, which is a bound worth honouring (a budget
//! past it buys nothing — the Pod dies there) and one an operator can raise.

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

/// Multiplier applied to a timed-out run's observed elapsed to derive the next
/// invocation's headroom. A workspace killed at its cap gets ~1.5x the killed
/// elapsed next time, so it grows toward "enough" instead of retrying at the
/// identical too-small cap.
const TIMEOUT_HEADROOM_NUMERATOR: u32 = 3;
const TIMEOUT_HEADROOM_DENOMINATOR: u32 = 2;

/// Multiplier applied to a PROVEN success whose observed elapsed exceeded the
/// static `max_cap`, to derive the next invocation's budget. A success is hard
/// evidence of the *actual* cost (unlike a timeout, which only proves the cap
/// was too small), so we add a smaller variance cushion — ~1.25x the proven
/// elapsed — than the 1.5x timeout-growth factor. This keeps the budget above a
/// cost we have already paid for, so a heavy workspace that succeeded above
/// `max_cap` is never re-run at the too-small static cap. Like the timeout
/// path, this is allowed to climb above `max_cap`, bounded by the adaptive
/// ceiling. Successes *under* `max_cap` stay handled by [`prior_based_budget`]
/// (clamped to `max_cap`), so no-history / under-cap behaviour is unchanged.
const SUCCESS_HEADROOM_NUMERATOR: u32 = 5;
const SUCCESS_HEADROOM_DENOMINATOR: u32 = 4;

/// FLOOR of the adaptive ceiling, applied to the per-indexer static `max_cap`.
///
/// This used to be the *absolute* ceiling, and that was a production trap: for
/// rust-analyzer (`max_cap` 1200s) it pinned every adaptive path at a hard
/// 3600s. On 2026-07-27 the djinn workspace's own rust-analyzer pass measured
/// **3 522 197 ms against that 3600s clamp — 2.2% of margin.** Because BOTH the
/// timeout-headroom and the success-headroom paths ended in `.min(ceiling)`,
/// the moment a workspace's real cost crossed 3600s it could never be budgeted
/// enough time again: every warm would be killed at the ceiling, the timeout
/// high-water would grow, the headroom would be clamped straight back to 3600s,
/// and the workspace would be **permanently unwarmable with no adaptation path
/// out**. A legitimately growing workspace must never be able to reach that
/// state, so this constant is now only the floor — see [`adaptive_ceiling`].
const ADAPTIVE_CEILING_MULTIPLIER: u32 = 3;

/// Multiplier applied to the largest PROVEN cost (a completed success, or the
/// high-water of a run we killed) to derive the evidence-driven ceiling.
///
/// It is deliberately strictly larger than both headroom factors
/// ([`TIMEOUT_HEADROOM_NUMERATOR`]/[`TIMEOUT_HEADROOM_DENOMINATOR`] = 1.5 and
/// [`SUCCESS_HEADROOM_NUMERATOR`]/[`SUCCESS_HEADROOM_DENOMINATOR`] = 1.25).
/// That inequality is the anti-stranding invariant: the ceiling can never clamp
/// the growth its own evidence implies, so each warm's budget is free to move
/// up with real measured cost instead of being pinned under it forever.
const EVIDENCE_CEILING_MULTIPLIER: u32 = 2;

/// Environment variable carrying the enclosing warm Job's
/// `activeDeadlineSeconds`, projected into the warm Pod by
/// `djinn_k8s::warm_job` (and mirrored by
/// `djinn_agent_worker::warm_step_budget::ENV_JOB_DEADLINE_SECONDS`).
///
/// This is the ONLY hard bound the budget honours, and it is a *real* one:
/// a budget larger than the deadline does not buy the indexer more time, it
/// just relocates the truncation from an observable `timed_out` status to an
/// unobservable kubelet kill. Raising it is the documented operator lever, so
/// unlike the old `max_cap * 3` clamp it can never strand a workspace with no
/// way out.
pub(crate) const ENV_WARM_JOB_DEADLINE_SECONDS: &str = "DJINN_WARM_JOB_DEADLINE_SECONDS";

/// Bound used when there is no enclosing Job deadline at all — the in-process
/// (dev / peer server) warm path, which no `activeDeadlineSeconds` will kill.
///
/// Deliberately far above any cost we have ever measured (the heaviest observed
/// real pass is ~3522s) so it never binds on legitimate work; it exists purely
/// so a genuinely hung indexer in an unenclosed run cannot ratchet its own
/// evidence upward without limit. Every warm Pod projects
/// [`ENV_WARM_JOB_DEADLINE_SECONDS`], so in production the Job deadline — not
/// this constant — is what bounds the budget.
const UNENCLOSED_HARD_CEILING: Duration = Duration::from_secs(4 * 60 * 60);

/// Superlinear size term for indexers whose cost is measurably superlinear in
/// workspace size, in **seconds per MiB^1.5 of source**.
///
/// MEASURED for rust-analyzer on the djinn workspace itself, at two points 45
/// days apart:
///
/// | date       | `.rs` files | `.rs` bytes | elapsed |
/// |------------|-------------|-------------|---------|
/// | 2026-06-12 | 642         | 9.7 MiB     | ~527 s  |
/// | 2026-07-27 | 1425        | 31.6 MiB    | 3522 s  |
///
/// 3.26x the source bytes cost 6.68x the time — an exponent of
/// `ln(6.68)/ln(3.26) ≈ 1.6`. The purely *linear* `baseline + file-steps +
/// byte-steps` model in [`scaled_budget`] predicts **540 s** for the second
/// row: a 6.5x underestimate. That gap is not cosmetic — it is what a project
/// falls back to whenever its `scip_indexer_timing` row is missing (project
/// recreated, workspace slug changed, FK cascade), and from 540s the
/// timeout-headroom path needs ~6 consecutive hour-long failed warms to climb
/// back to where it started.
///
/// `mib * sqrt(mib)` (exponent 1.5) is used rather than `powf(1.6)` because
/// `f64::sqrt` is exactly rounded by IEEE 754 on every platform, so the model
/// is bit-reproducible; 1.5 also errs slightly high on the small end, which is
/// the safe direction for a timeout. The coefficient is fitted to the measured
/// pair: `3522 / 31.6^1.5 ≈ 19.8`, rounded up to 20.
///
/// `None` = "not measured": those indexers keep exactly the linear model they
/// ship with today. Do not invent a coefficient without a measurement.
fn superlinear_secs_per_mib_pow_1_5(indexer: SupportedIndexer) -> Option<u32> {
    match indexer {
        SupportedIndexer::RustAnalyzer => Some(20),
        SupportedIndexer::TypeScript
        | SupportedIndexer::Java
        | SupportedIndexer::Go
        | SupportedIndexer::Clang
        | SupportedIndexer::Python
        | SupportedIndexer::Ruby
        | SupportedIndexer::DotNet => None,
    }
}

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
    /// High-water elapsed (ms) of the most recent TIMED-OUT run(s) since the
    /// last success. Distinct from the p95/last-success fields because a
    /// timeout is evidence the cap itself was too small: it feeds the
    /// [`ADAPTIVE_CEILING_MULTIPLIER`] headroom path, which may raise the
    /// budget *above* the static `max_cap` (unlike the success-derived hooks,
    /// which stay clamped by `max_cap`).
    pub last_timed_out_ms: Option<u64>,
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

/// Ceiling for the adaptive (evidence-driven) paths.
///
/// # Why this is not a constant
///
/// The previous shape — a flat `max_cap * 3` — was an *absolute* clamp, and an
/// absolute clamp on a growing workspace is a one-way door. Once real cost
/// crossed the clamp the workspace could never be given enough time again, so
/// it stopped producing a server index on every subsequent warm, forever, with
/// nothing in the model able to notice or recover. That is the defect this
/// function exists to remove.
///
/// The ceiling is now `max(static_floor, proven_cost * EVIDENCE_CEILING_MULTIPLIER)`
/// clamped by `hard_bound`, where:
///
/// * `static_floor = max_cap * ADAPTIVE_CEILING_MULTIPLIER` — unchanged
///   behaviour for every workspace whose cost fits inside it today;
/// * `proven_cost` = the largest cost we have *observed*: a completed success,
///   or the high-water elapsed of a run we killed. Both are measurements, not
///   requests — nothing a caller supplies can move this;
/// * `EVIDENCE_CEILING_MULTIPLIER` (2) is strictly above both headroom factors
///   (1.5 timeout / 1.25 success), so the ceiling can never clamp the growth
///   its own evidence implies. **This is the anti-stranding invariant**: for
///   any observed cost `c`, the next budget is at least `1.25c` (success) or
///   `1.5c` (timeout), never pinned below `c`;
/// * `hard_bound` is the enclosing Job deadline (see
///   [`ENV_WARM_JOB_DEADLINE_SECONDS`]). Being bounded by the deadline is not a
///   trap, because a budget above the deadline buys nothing — the Pod dies
///   there regardless — and because raising the deadline is an operator lever
///   that visibly moves the ceiling with it.
///
/// A genuinely hung indexer therefore still ratchets to, and stops at, the Job
/// deadline: bounded, and bounded by the number the operator already chose.
fn adaptive_ceiling(
    indexer: SupportedIndexer,
    prior: Option<&PriorIndexerTiming>,
    hard_bound: Duration,
) -> Duration {
    let static_floor = max_cap(indexer).saturating_mul(ADAPTIVE_CEILING_MULTIPLIER);
    let proven_cost = prior.map_or(Duration::ZERO, proven_cost);
    let evidence_ceiling = proven_cost.saturating_mul(EVIDENCE_CEILING_MULTIPLIER);
    static_floor.max(evidence_ceiling).min(hard_bound)
}

/// The largest cost this indexer/workspace pair has been *observed* to incur:
/// the last completed success, or the high-water elapsed of a run we killed.
///
/// A timeout is weaker evidence than a success (it proves only that the cap was
/// too small, not what the true cost is), but it is still a lower bound on real
/// cost, and excluding it is exactly how a workspace that grows faster than
/// `2x` between successes would get stranded under a success-only ceiling.
fn proven_cost(prior: &PriorIndexerTiming) -> Duration {
    let success = prior.last_success_ms.unwrap_or(0);
    let timed_out = prior.last_timed_out_ms.unwrap_or(0);
    Duration::from_millis(success.max(timed_out))
}

/// The hard bound the adaptive ceiling may never exceed, read from the
/// enclosing warm Job's `activeDeadlineSeconds`.
pub(crate) fn hard_budget_ceiling() -> Duration {
    hard_budget_ceiling_from(std::env::var(ENV_WARM_JOB_DEADLINE_SECONDS).ok().as_deref())
}

/// Pure seam for [`hard_budget_ceiling`].
///
/// Anything unset, empty, unparseable or zero falls back to
/// [`UNENCLOSED_HARD_CEILING`] — the same fail-open discipline
/// `warm_step_budget::duration_from_env` uses, because a typo in an env var
/// must never silently *shrink* an indexer's budget.
fn hard_budget_ceiling_from(raw: Option<&str>) -> Duration {
    raw.and_then(|text| text.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map_or(UNENCLOSED_HARD_CEILING, Duration::from_secs)
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
    use djinn_core::clock::{Clock, SystemClock};

    budget_for_indexer_at(
        indexer,
        size,
        prior,
        deadline,
        SystemClock::new().now_instant(),
    )
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

/// Headroom budget derived from a timed-out run's observed elapsed. Returns
/// `Duration::ZERO` when there is no timeout evidence. Unlike
/// [`prior_based_budget`], this may exceed the static `max_cap` — a timeout
/// means the cap itself was too small, so identical retries are pointless.
///
/// Deliberately returns the value the evidence implies, **unclamped**. The
/// caller applies [`adaptive_ceiling`] and can therefore tell the difference
/// between "the model wanted this much" and "the model got clipped", which is
/// exactly the signal that was missing when the flat `max_cap * 3` clamp was
/// silently stranding a workspace.
fn timeout_headroom_budget(prior: &PriorIndexerTiming) -> Duration {
    match prior.last_timed_out_ms {
        Some(ms) if ms > 0 => {
            Duration::from_millis(ms).saturating_mul(TIMEOUT_HEADROOM_NUMERATOR)
                / TIMEOUT_HEADROOM_DENOMINATOR
        }
        _ => Duration::ZERO,
    }
}

/// Headroom budget derived from a PROVEN success whose observed elapsed exceeded
/// the static `max_cap`. A success above the cap is hard evidence of the real
/// cost, so the next budget must stay above it (with a ~1.25x variance cushion,
/// see [`SUCCESS_HEADROOM_NUMERATOR`]) — otherwise clearing the timed-out
/// high-water on success would let the budget snap back to the too-small
/// `max_cap` and the workspace would oscillate kill→grow→success→kill.
///
/// Returns `Duration::ZERO` when there is no success evidence or the success
/// elapsed is within `max_cap` (those stay handled by [`prior_based_budget`],
/// preserving byte-identical under-cap behaviour). Like
/// [`timeout_headroom_budget`], the result may exceed `max_cap` and is returned
/// **unclamped** so the caller can see when the ceiling bound it.
fn success_headroom_budget(prior: &PriorIndexerTiming, max_cap: Duration) -> Duration {
    match prior.last_success_ms {
        Some(ms) if Duration::from_millis(ms) > max_cap => {
            Duration::from_millis(ms).saturating_mul(SUCCESS_HEADROOM_NUMERATOR)
                / SUCCESS_HEADROOM_DENOMINATOR
        }
        _ => Duration::ZERO,
    }
}

/// Superlinear size estimate for an indexer with a MEASURED cost model.
///
/// `coefficient * mib^1.5`, or [`Duration::ZERO`] for indexers whose model has
/// never been measured (see [`superlinear_secs_per_mib_pow_1_5`]). Unlike the
/// linear step model this is NOT clamped by `max_cap` at its call site: it is a
/// size-derived *estimate of real cost*, and clamping an estimate of real cost
/// to a static cap is precisely how the 540s no-history collapse happened.
fn superlinear_estimate(indexer: SupportedIndexer, size: &WorkspaceSizeHint) -> Duration {
    let Some(coefficient) = superlinear_secs_per_mib_pow_1_5(indexer) else {
        return Duration::ZERO;
    };
    if size.source_bytes == 0 {
        return Duration::ZERO;
    }
    let mib = size.source_bytes as f64 / (1024.0 * 1024.0);
    // `sqrt` is exactly rounded by IEEE 754, so `mib * mib.sqrt()` is
    // bit-reproducible on every platform this ships to.
    let seconds = f64::from(coefficient) * mib * mib.sqrt();
    if !seconds.is_finite() || seconds <= 0.0 {
        return Duration::ZERO;
    }
    Duration::from_secs(seconds.ceil().min(u64::MAX as f64) as u64)
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
    budget_for_indexer_at_with_bound(indexer, size, prior, deadline, now, hard_budget_ceiling())
}

/// Testable variant of [`budget_for_indexer_at`] that also accepts the hard
/// ceiling explicitly rather than reading the process environment.
fn budget_for_indexer_at_with_bound(
    indexer: SupportedIndexer,
    size: &WorkspaceSizeHint,
    prior: Option<&PriorIndexerTiming>,
    deadline: Option<&ActiveDeadline>,
    now: Instant,
    hard_bound: Duration,
) -> IndexerBudget {
    let cap = max_cap(indexer);
    let ceiling = adaptive_ceiling(indexer, prior, hard_bound);

    // Step 1–2: baseline + size scaling, capped by indexer max.
    let (mut total, file_steps, byte_steps) = scaled_budget(indexer, size);
    let mut reason_parts: Vec<String> = vec![format!(
        "baseline+scale: {} file-steps, {} byte-steps, capped at {:?}",
        file_steps, byte_steps, cap
    )];

    // `wanted` tracks what the model would grant with NO ceiling at all. Only
    // by keeping it can we tell a budget that landed where the evidence pointed
    // apart from one the ceiling clipped — the distinction whose absence let a
    // workspace sit permanently under-budgeted in silence.
    let mut wanted = total;

    // Step 2b: superlinear size estimate. The linear step model above is
    // calibrated for small workspaces and badly underestimates a large one
    // (measured 6.5x low on a 31.6 MiB Rust workspace), which is what a project
    // with no persisted timing row falls back to. Where a measured superlinear
    // model exists, let it raise the no-history budget above `max_cap`, bounded
    // by the same adaptive ceiling as the evidence paths.
    let superlinear = superlinear_estimate(indexer, size);
    wanted = wanted.max(superlinear);
    if superlinear.min(ceiling) > total {
        total = superlinear.min(ceiling);
        reason_parts.push(format!(
            "superlinear size model raised to {:?} (ceiling {:?})",
            total, ceiling
        ));
    }

    // Step 3: prior-timing scaling.
    if let Some(prior) = prior {
        // 3a: success/p95 prior scaling — raises within the static `max_cap`.
        let prior_budget = prior_based_budget(prior, cap);
        if prior_budget > total {
            total = prior_budget;
            reason_parts.push(format!("prior timing raised to {:?}", prior_budget));
        }

        // 3b: timed-out headroom — a run killed at its cap is evidence the cap
        // was too small, so grow toward "enough" (≈1.5x the killed elapsed)
        // rather than retrying identically. Allowed to climb above `max_cap`,
        // bounded by the adaptive ceiling.
        let headroom = timeout_headroom_budget(prior);
        wanted = wanted.max(headroom);
        if headroom.min(ceiling) > total {
            total = headroom.min(ceiling);
            reason_parts.push(format!(
                "timed-out headroom raised to {:?} (ceiling {:?})",
                total, ceiling
            ));
        }

        // 3c: proven-success headroom — a success whose elapsed exceeded the
        // static `max_cap` is hard evidence of the true cost. Keep the budget
        // above it (≈1.25x) so clearing the timed-out high-water on success
        // can't snap the budget back to the too-small `max_cap` and restart the
        // kill→grow→success→kill oscillation. Also allowed above `max_cap`,
        // bounded by the same adaptive ceiling.
        let success_headroom = success_headroom_budget(prior, cap);
        wanted = wanted.max(success_headroom);
        if success_headroom.min(ceiling) > total {
            total = success_headroom.min(ceiling);
            reason_parts.push(format!(
                "over-cap success headroom raised to {:?} (ceiling {:?})",
                total, ceiling
            ));
        }
    }

    // Step 3d: the loud diagnostic. Reaching the ceiling now means the enclosing
    // Job deadline is the binding constraint, and the operator lever is named
    // explicitly. This is the line whose absence made the old clamp a silent
    // one-way door.
    if wanted > ceiling {
        reason_parts.push(format!(
            "CEILING BOUND: model wanted {:?} but the ceiling is {:?} \
             (hard bound {:?} from {}); raise DJINN_K8S_WARM_JOB_TIMEOUT_SECONDS",
            wanted, ceiling, hard_bound, ENV_WARM_JOB_DEADLINE_SECONDS
        ));
        tracing::warn!(
            indexer = ?indexer,
            wanted_secs = wanted.as_secs(),
            ceiling_secs = ceiling.as_secs(),
            hard_bound_secs = hard_bound.as_secs(),
            source_file_count = size.source_file_count,
            source_bytes = size.source_bytes,
            "SCIP indexer budget clipped by the adaptive ceiling — this indexer \
             will very likely be killed before it finishes. The ceiling is the \
             enclosing warm Job deadline; raise DJINN_K8S_WARM_JOB_TIMEOUT_SECONDS \
             (helm: graphWarm.jobTimeoutSeconds) above the wanted budget."
        );
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
#[path = "budget_tests.rs"]
mod tests;
