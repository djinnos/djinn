//! Subscription concurrency learning over qualified Phase-C windows (task yh4d).
//!
//! Everything here is pure except [`ingest_qualified_window_v1`], which is the
//! only way a window reaches the learner at all: it goes through gscv's
//! exact-bound, active-catalog-qualified coordinator seam. There is no raw
//! controller-window query and no in-memory caller verdict — a caller cannot
//! hand the learner a window it says is trainable.
//!
//! The rate is an **aggregate**: output tokens are assigned by emission
//! timestamp and divided by the wall-clock length of the *union* of active
//! stream intervals, clipped to the aligned window. Summed stream-seconds are
//! never used; two fully overlapping streams occupy 60 seconds of wall clock,
//! not 120.

use std::collections::{BTreeMap, BTreeSet};

use djinn_db::{
    ModelTurnAdmissionRepository, ModelTurnControllerFence, ModelTurnLearnedConcurrencyInput,
    ModelTurnLeaseMutationOutcome,
};
use djinn_provider::{ProviderAttemptLossV1, ProviderAttemptTerminalV1, catalog::CatalogService};

use super::{
    AlignedPhaseCWindowV1, PhaseCLearnerWindowV1, learner_catalog_qualified_phase_c_window_v1,
};

// ── Contract constants ──────────────────────────────────────────────────────

/// Lowest and highest concurrency target the controller may ever hold.
pub const MIN_TARGET: i64 = 1;
pub const MAX_TARGET: i64 = 32;
/// A window teaches nothing below these thresholds.
pub const MIN_COMPLETED_TURNS: i64 = 8;
pub const MIN_ACTIVE_UNION_SECONDS: i64 = 30;
/// Baseline smoothing.
pub const BASELINE_EWMA_ALPHA: f64 = 0.2;
/// A probe must beat the baseline by this fraction to count as growth.
pub const GROWTH_THRESHOLD: f64 = 0.05;
/// Resamples in the deterministic bootstrap, and the confidence it produces.
pub const BOOTSTRAP_RESAMPLES: usize = 1_000;
pub const BOOTSTRAP_CONFIDENCE: f64 = 0.95;
/// Consecutive non-growing probes before the controller stops probing.
pub const NON_GROWING_PROBE_LIMIT: i64 = 3;
/// Windows held after the probe limit is reached.
pub const PLATEAU_HOLD_WINDOWS: i64 = 5;

// ── Aggregate throughput ────────────────────────────────────────────────────

/// Output tokens emitted at one wall-clock second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputTokenEmissionV1 {
    pub emitted_at_second: i64,
    pub output_tokens: i64,
}

/// One stream that was actively producing over a half-open second interval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveStreamV1 {
    pub started_at_second: i64,
    pub ended_at_second: i64,
    pub emissions: Vec<OutputTokenEmissionV1>,
}

impl ActiveStreamV1 {
    /// The part of this stream that lies inside the aligned half-open window,
    /// or `None` when the clipped interval is empty.
    fn clipped(&self, window: AlignedPhaseCWindowV1) -> Option<(i64, i64)> {
        let start = self.started_at_second.max(window.start_second());
        let end = self.ended_at_second.min(window.end_second());
        (start < end).then_some((start, end))
    }
}

/// Aggregate output rate over one aligned window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AggregateThroughputV1 {
    /// Output tokens whose emission timestamp fell inside the window and inside
    /// the emitting stream's own clipped active interval.
    pub output_tokens: i64,
    /// Wall-clock seconds covered by the union of clipped active intervals.
    pub active_union_seconds: i64,
    /// `output_tokens / active_union_seconds`, or `0.0` when nothing was active.
    pub tokens_per_second: f64,
}

/// Merge half-open intervals and return their total wall-clock length.
fn union_seconds(mut intervals: Vec<(i64, i64)>) -> i64 {
    intervals.sort_unstable();
    let mut total = 0;
    let mut current: Option<(i64, i64)> = None;
    for (start, end) in intervals {
        match current {
            Some((open, close)) if start <= close => current = Some((open, close.max(end))),
            Some((open, close)) => {
                total += close - open;
                current = Some((start, end));
            }
            None => current = Some((start, end)),
        }
    }
    if let Some((open, close)) = current {
        total += close - open;
    }
    total
}

/// Aggregate output rate for one aligned window.
///
/// Tokens are attributed by emission timestamp, and the denominator is the union
/// of clipped active intervals — never the sum of per-stream durations.
#[must_use]
pub fn aggregate_output_throughput_v1(
    window: AlignedPhaseCWindowV1,
    streams: &[ActiveStreamV1],
) -> AggregateThroughputV1 {
    let mut intervals = Vec::with_capacity(streams.len());
    let mut output_tokens = 0i64;
    for stream in streams {
        let Some((start, end)) = stream.clipped(window) else {
            continue;
        };
        intervals.push((start, end));
        for emission in &stream.emissions {
            if start <= emission.emitted_at_second
                && emission.emitted_at_second < end
                && emission.output_tokens > 0
            {
                output_tokens = output_tokens.saturating_add(emission.output_tokens);
            }
        }
    }
    let active_union_seconds = union_seconds(intervals);
    let tokens_per_second = if active_union_seconds > 0 {
        output_tokens as f64 / active_union_seconds as f64
    } else {
        0.0
    };
    AggregateThroughputV1 {
        output_tokens,
        active_union_seconds,
        tokens_per_second,
    }
}

/// Per-stream rate samples for the bootstrap, one per stream that was active in
/// the window. Each sample is that stream's own tokens over its own clipped
/// wall-clock seconds, so the bootstrap measures spread across streams rather
/// than resampling a single aggregate.
#[must_use]
pub fn per_stream_rate_samples_v1(
    window: AlignedPhaseCWindowV1,
    streams: &[ActiveStreamV1],
) -> Vec<f64> {
    let mut samples = Vec::with_capacity(streams.len());
    for stream in streams {
        let Some((start, end)) = stream.clipped(window) else {
            continue;
        };
        let tokens: i64 = stream
            .emissions
            .iter()
            .filter(|emission| {
                start <= emission.emitted_at_second
                    && emission.emitted_at_second < end
                    && emission.output_tokens > 0
            })
            .map(|emission| emission.output_tokens)
            .sum();
        samples.push(tokens as f64 / (end - start) as f64);
    }
    samples
}

// ── Eligibility and baseline ────────────────────────────────────────────────

/// A window teaches only with enough completed turns *and* enough wall clock in
/// the union of active intervals.
#[must_use]
pub fn window_is_eligible_v1(completed_turns: i64, throughput: &AggregateThroughputV1) -> bool {
    completed_turns >= MIN_COMPLETED_TURNS
        && throughput.active_union_seconds >= MIN_ACTIVE_UNION_SECONDS
}

/// Exponentially weighted baseline, alpha 0.2.
#[must_use]
pub fn update_baseline_v1(baseline: Option<f64>, observed: f64) -> f64 {
    match baseline {
        None => observed,
        Some(previous) => BASELINE_EWMA_ALPHA * observed + (1.0 - BASELINE_EWMA_ALPHA) * previous,
    }
}

// ── Deterministic bootstrap ─────────────────────────────────────────────────

/// xorshift64*, so a trace replays bit-identically from its seed. No process
/// entropy and no wall clock enter the growth decision.
struct DeterministicRngV1(u64);

impl DeterministicRngV1 {
    fn new(seed: u64) -> Self {
        // A zero state is absorbing for xorshift; move it off zero.
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn index(&mut self, len: usize) -> usize {
        (self.next_u64() % len as u64) as usize
    }
}

/// Lower bound of the deterministic 95% bootstrap interval for the mean of
/// `samples`, or `None` when there is nothing to resample.
#[must_use]
pub fn bootstrap_lower_bound_v1(samples: &[f64], seed: u64) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut rng = DeterministicRngV1::new(seed);
    let mut means = Vec::with_capacity(BOOTSTRAP_RESAMPLES);
    for _ in 0..BOOTSTRAP_RESAMPLES {
        let mut total = 0.0;
        for _ in 0..samples.len() {
            total += samples[rng.index(samples.len())];
        }
        means.push(total / samples.len() as f64);
    }
    // `total_cmp` rather than `partial_cmp(..).expect(..)`: a NaN sample would
    // otherwise panic the controller instead of simply sorting last.
    means.sort_by(f64::total_cmp);
    let tail = (1.0 - BOOTSTRAP_CONFIDENCE) / 2.0;
    let index = ((BOOTSTRAP_RESAMPLES as f64) * tail).floor() as usize;
    Some(means[index.min(BOOTSTRAP_RESAMPLES - 1)])
}

/// Growth requires the bootstrap lower bound to clear the baseline by the 5%
/// threshold. A point estimate that happens to be higher is not enough.
#[must_use]
pub fn growth_qualifies_v1(samples: &[f64], baseline: f64, seed: u64) -> bool {
    bootstrap_lower_bound_v1(samples, seed)
        .is_some_and(|lower| lower > baseline * (1.0 + GROWTH_THRESHOLD))
}

// ── Controller state ────────────────────────────────────────────────────────

/// One attempt's terminal outcome, keyed by an opaque attempt identity. The
/// identity is what deduplicates a loss; the typed B1 terminal is what makes it
/// a loss at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptTerminalObservationV1 {
    /// Opaque attempt identity. Never a raw request, lease, or credential id.
    pub attempt: String,
    pub terminal: ProviderAttemptTerminalV1,
}

impl AttemptTerminalObservationV1 {
    /// Only a typed B1 failure is a loss. Completed and aborted are not.
    #[must_use]
    pub fn loss(&self) -> Option<ProviderAttemptLossV1> {
        match self.terminal {
            ProviderAttemptTerminalV1::Failed(loss) => Some(loss),
            ProviderAttemptTerminalV1::Completed | ProviderAttemptTerminalV1::Aborted => None,
        }
    }
}

/// Everything one aligned window contributes to the controller.
#[derive(Clone, Debug, PartialEq)]
pub struct SubscriptionWindowObservationV1 {
    /// The qualified window, or `None` when the window was diagnostic, absent,
    /// malformed, catalog-invalid, or boundary-mismatched.
    pub qualified: Option<PhaseCLearnerWindowV1>,
    pub throughput: AggregateThroughputV1,
    pub rate_samples: Vec<f64>,
    pub terminals: Vec<AttemptTerminalObservationV1>,
    pub bootstrap_seed: u64,
}

/// Why the controller did what it did. Closed and non-identifying.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ControllerTransitionV1 {
    /// No qualified window: nothing, including the baseline, moved.
    HeldUnqualified,
    /// Qualified but under the turn/wall-clock thresholds.
    HeldIneligible,
    /// A loss already counted against this controller.
    HeldDuplicateLoss,
    /// Probing is suspended after the non-growing probe limit.
    HeldPlateau,
    /// Eligible, probed, did not clear the confidence-bounded threshold.
    ProbeDidNotGrow,
    /// The third consecutive non-growing probe; probing suspends.
    ProbeRejected,
    Grew,
    /// A new deduplicated loss reduced the target.
    BackedOff,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SubscriptionControllerStateV1 {
    target: Option<i64>,
    baseline: Option<f64>,
    non_growing_probes: i64,
    remaining_hold_windows: i64,
    counted_losses: BTreeSet<String>,
}

impl SubscriptionControllerStateV1 {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    /// Targets start at 1 and never leave `[1, 32]`.
    #[must_use]
    pub fn target(&self) -> i64 {
        self.target
            .unwrap_or(MIN_TARGET)
            .clamp(MIN_TARGET, MAX_TARGET)
    }
    #[must_use]
    pub fn baseline(&self) -> Option<f64> {
        self.baseline
    }
    #[must_use]
    pub fn non_growing_probes(&self) -> i64 {
        self.non_growing_probes
    }
    #[must_use]
    pub fn remaining_hold_windows(&self) -> i64 {
        self.remaining_hold_windows
    }
    fn set_target(&mut self, target: i64) {
        self.target = Some(target.clamp(MIN_TARGET, MAX_TARGET));
    }
}

/// Fold one window into the controller.
///
/// Order is the contract: an unqualified window changes nothing at all, a new
/// loss takes precedence over any growth the same window could have shown, a
/// repeated loss holds, and only then can an eligible window probe.
pub fn observe_window_v1(
    state: &mut SubscriptionControllerStateV1,
    observation: &SubscriptionWindowObservationV1,
) -> ControllerTransitionV1 {
    let Some(qualified) = observation.qualified.as_ref() else {
        return ControllerTransitionV1::HeldUnqualified;
    };

    // Loss precedence, applied before anything can read as growth.
    let mut fresh = BTreeMap::new();
    let mut repeated = false;
    for terminal in &observation.terminals {
        if terminal.loss().is_none() {
            continue;
        }
        if state.counted_losses.contains(&terminal.attempt) {
            repeated = true;
        } else {
            fresh.insert(terminal.attempt.clone(), ());
        }
    }
    if !fresh.is_empty() {
        for attempt in fresh.into_keys() {
            state.counted_losses.insert(attempt);
        }
        // A single window's losses are one back-off, not one per attempt.
        let reduced = ((state.target() as f64) * 0.9).floor() as i64;
        state.set_target(reduced.max(MIN_TARGET));
        state.non_growing_probes = 0;
        state.remaining_hold_windows = 0;
        return ControllerTransitionV1::BackedOff;
    }
    if repeated {
        return ControllerTransitionV1::HeldDuplicateLoss;
    }

    if !window_is_eligible_v1(qualified.completed_turns, &observation.throughput) {
        return ControllerTransitionV1::HeldIneligible;
    }
    if state.remaining_hold_windows > 0 {
        state.remaining_hold_windows -= 1;
        return ControllerTransitionV1::HeldPlateau;
    }

    // The decision reads the baseline as it stood *before* this window, so a
    // window can never clear a threshold it just moved.
    let baseline_before = state.baseline;
    state.baseline = Some(update_baseline_v1(
        baseline_before,
        observation.throughput.tokens_per_second,
    ));
    let Some(baseline_before) = baseline_before else {
        // The first eligible window establishes the baseline; there is nothing
        // to have grown against yet.
        return ControllerTransitionV1::ProbeDidNotGrow;
    };
    if growth_qualifies_v1(
        &observation.rate_samples,
        baseline_before,
        observation.bootstrap_seed,
    ) {
        state.set_target(state.target() + 1);
        state.non_growing_probes = 0;
        return ControllerTransitionV1::Grew;
    }
    state.non_growing_probes += 1;
    if state.non_growing_probes >= NON_GROWING_PROBE_LIMIT {
        state.non_growing_probes = 0;
        state.remaining_hold_windows = PLATEAU_HOLD_WINDOWS;
        return ControllerTransitionV1::ProbeRejected;
    }
    ControllerTransitionV1::ProbeDidNotGrow
}

// ── Production ingestion ────────────────────────────────────────────────────

/// Streams and terminals a completed window observed. Neither says whether the
/// window is trainable — that verdict comes only from the durable ledger, read
/// back through gscv's catalog-qualified seam below.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowActivityV1 {
    pub streams: Vec<ActiveStreamV1>,
    pub terminals: Vec<AttemptTerminalObservationV1>,
}

/// The exact durable window a learner is asking about. The bounds are carried
/// verbatim because the storage read is exact-bound: an approximation of them is
/// simply a window that does not exist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QualifiedWindowRequestV1 {
    pub pool_id: i64,
    pub window: AlignedPhaseCWindowV1,
    pub started_at: String,
    pub ended_at: String,
}

/// The sole production ingestion path.
///
/// It re-reads the window through
/// [`learner_catalog_qualified_phase_c_window_v1`], so a diagnostic, absent,
/// malformed-summary, catalog-invalid, or boundary-mismatched window simply
/// yields no qualified window and cannot reach any rate or state transition.
pub async fn ingest_qualified_window_v1(
    repository: &ModelTurnAdmissionRepository,
    catalog: &CatalogService,
    state: &mut SubscriptionControllerStateV1,
    request: &QualifiedWindowRequestV1,
    activity: &WindowActivityV1,
) -> djinn_db::Result<ControllerTransitionV1> {
    let window = request.window;
    let qualified = learner_catalog_qualified_phase_c_window_v1(
        repository,
        catalog,
        request.pool_id,
        window
            .start_second()
            .div_euclid(AlignedPhaseCWindowV1::SECONDS),
        &request.started_at,
        &request.ended_at,
    )
    .await?;
    let observation = SubscriptionWindowObservationV1 {
        qualified,
        throughput: aggregate_output_throughput_v1(window, &activity.streams),
        rate_samples: per_stream_rate_samples_v1(window, &activity.streams),
        terminals: activity.terminals.clone(),
        // Deterministic in the window itself: the same window always replays to
        // the same bootstrap.
        bootstrap_seed: (request.pool_id as u64) ^ (window.start_second() as u64),
    };
    Ok(observe_window_v1(state, &observation))
}

/// The transitions that actually moved the controller's target, and are
/// therefore the only ones with anything to commit.
///
/// Every other transition is a hold: the target the pool already carries is
/// still the controller's answer, so re-writing it would be a write that says
/// nothing. Keeping this predicate explicit is what stops the writer from
/// stamping `MIN_TARGET` onto pools the controller has never actually decided
/// anything about.
#[must_use]
pub fn transition_moved_target_v1(transition: ControllerTransitionV1) -> bool {
    matches!(
        transition,
        ControllerTransitionV1::Grew | ControllerTransitionV1::BackedOff
    )
}

/// What one production learner pass did for one pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubscriptionLearningOutcomeV1 {
    pub transition: ControllerTransitionV1,
    /// The target committed to `model_turn_pools.learned_concurrency`, or
    /// `None` when the window moved nothing and there was nothing to commit.
    pub committed_target: Option<i64>,
    /// True when the durable fence refused the commit. The controller's
    /// in-memory target still moved; the pool's persisted one did not.
    pub fenced: bool,
}

/// Ingest one completed window and commit the resulting target.
///
/// This is the production path from a durable Phase-C window to
/// `model_turn_pools.learned_concurrency`, and the two halves are deliberately
/// welded together here rather than left for a caller to pair up:
///
/// * the window is re-read through [`ingest_qualified_window_v1`], so only a
///   window the durable ledger itself calls trainable can reach the controller;
/// * the resulting target is committed through
///   [`ModelTurnAdmissionRepository::apply_learned_concurrency`], under the same
///   durable leadership fence the window row was written under, so a superseded
///   leader's late decision cannot land.
///
/// Only a transition that actually moved the target writes anything — see
/// [`transition_moved_target_v1`]. A fenced commit is reported, not swallowed:
/// the caller learns that the persisted target and the in-memory controller
/// have diverged.
pub async fn learn_and_persist_window_target_v1(
    repository: &ModelTurnAdmissionRepository,
    catalog: &CatalogService,
    fence: &ModelTurnControllerFence,
    controller_generation: i64,
    state: &mut SubscriptionControllerStateV1,
    request: &QualifiedWindowRequestV1,
    activity: &WindowActivityV1,
) -> djinn_db::Result<SubscriptionLearningOutcomeV1> {
    let transition =
        ingest_qualified_window_v1(repository, catalog, state, request, activity).await?;
    if !transition_moved_target_v1(transition) {
        return Ok(SubscriptionLearningOutcomeV1 {
            transition,
            committed_target: None,
            fenced: false,
        });
    }
    let target = state.target();
    let applied = repository
        .apply_learned_concurrency(ModelTurnLearnedConcurrencyInput {
            pool_id: request.pool_id,
            learned_concurrency: target,
            controller_generation,
            fence: fence.clone(),
        })
        .await?;
    Ok(match applied {
        ModelTurnLeaseMutationOutcome::Applied => SubscriptionLearningOutcomeV1 {
            transition,
            committed_target: Some(target),
            fenced: false,
        },
        _ => SubscriptionLearningOutcomeV1 {
            transition,
            committed_target: None,
            fenced: true,
        },
    })
}

#[cfg(test)]
#[path = "model_turn_admission_subscription_learner_tests.rs"]
mod tests;
