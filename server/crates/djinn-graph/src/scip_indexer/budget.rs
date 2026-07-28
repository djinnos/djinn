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
        Some(ms) if Duration::from_millis(ms) > max_cap => Duration::from_millis(ms)
            .saturating_mul(SUCCESS_HEADROOM_NUMERATOR)
            / SUCCESS_HEADROOM_DENOMINATOR,
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
mod tests {
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
}
