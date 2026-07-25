//! Pre-pod-allocation warm build-cache freshness gate (proposal ri23 Part 2).
//!
//! Mirrors the bounded-wait / single-flight / timeout-best-effort shape of
//! `djinn_agent::warmer::InProcessGraphWarmer::await_fresh` (already used in
//! this dispatcher for the canonical graph cache), but for the per-project warm
//! Cargo build-cache. Before a task-run pod is allocated the dispatcher asks
//! this gate whether the warm build cache is fresh for the resolved environment
//! identity:
//!
//! - a compile-mode stack whose cache is fresh allocates immediately;
//! - a `CompileMode::None` (no-compile) stack bypasses the gate entirely;
//! - a missing/stale cache triggers a single-flight warm and waits, but only up
//!   to the configured bound;
//! - a bound timeout, an inventory-probe error, or a warmer-trigger error each
//!   cold-dispatches exactly once and records a bounded project+reason metric.
//!
//! Ownership boundary: the warm substrate owns inventory / warming / freshness /
//! eviction / seeding. This gate only READS freshness facts through the
//! [`WarmBuildCacheProbe`] trait, decides whether to wait, labels the decision,
//! and lets the caller allocate exactly once afterwards. It never mutates warm
//! state. This is a distinct pre-allocation seam from the worker-runtime
//! `prepare_build_cache` seeding path (sibling `1bnj`).

use std::time::Duration;

use async_trait::async_trait;
use djinn_core::clock::Clock;
use djinn_runtime::WarmerError;
use djinn_telemetry::warm_cache::{Outcome, Reason};

/// Poll interval while waiting for a background warm to make the cache fresh.
/// Matches `InProcessGraphWarmer::await_fresh`'s cadence: well below any
/// realistic warm completion time while keeping the probe cheap.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Default bounded wait for a warm build cache to go fresh before the gate
/// gives up and cold-dispatches. Same order as the architect graph gate (45s).
pub const DEFAULT_WAIT_BOUND: Duration = Duration::from_secs(45);

/// Whether a task's stack has a compile step the warm build cache can serve.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompileMode {
    /// A compile-mode stack (e.g. a Cargo workspace) — the gate applies.
    Compile,
    /// No compile step — the gate is bypassed and the run allocates directly.
    None,
}

/// A freshness fact READ from the warm substrate for a resolved identity.
///
/// Variants are constructed by [`WarmBuildCacheProbe`] implementations: the
/// contract-test fake today, and the warm substrate's coordinator-facing probe
/// once it is wired (see [`super::task_dispatch`]'s `warm_build_cache_probe`
/// seam). Until that production impl lands, the non-test build constructs none
/// of them, so dead-code analysis is suppressed for this not-yet-wired seam.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WarmFreshness {
    /// Present, within the age policy, and built for the resolved identity.
    Fresh,
    /// Present but outside the age policy (or otherwise not usable as-is).
    Stale,
    /// No warm base exists for this identity.
    Missing,
}

/// The warm inventory probe failed to determine freshness. Carries a bounded
/// human string for logs only — it never becomes a metric label.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WarmProbeError(pub String);

impl std::fmt::Display for WarmProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "warm inventory probe error: {}", self.0)
    }
}

/// The resolved environment identity a warm base must match to be a hit.
///
/// The freshness comparison against this identity happens INSIDE the probe (the
/// warm substrate owns the notion of identity); the gate merely threads it
/// through. A probe that ignores identity, or reports `Fresh` for a mismatched
/// identity, is a substrate bug the gate cannot paper over — but the gate never
/// upgrades a non-`Fresh` observation to fresh, so a mismatch reported as
/// `Stale`/`Missing` is never treated as fresh here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WarmCacheIdentity {
    pub project_id: String,
    /// Opaque, substrate-defined identity token (e.g. image + toolchain +
    /// mold-jobs variant digest). Compared for equality by the probe.
    pub environment_identity: String,
}

/// Reads warm build-cache freshness facts and single-flight-triggers a warm.
///
/// Implemented by the warm substrate (production) and by a scripted fake (the
/// contract test). Both methods are identity-scoped so freshness always answers
/// "fresh FOR THIS resolved environment identity".
#[async_trait]
pub trait WarmBuildCacheProbe: Send + Sync {
    /// Read the current freshness of the warm base for `identity`.
    async fn probe(&self, identity: &WarmCacheIdentity) -> Result<WarmFreshness, WarmProbeError>;

    /// Single-flight-trigger a warm for `identity`. Safe to call repeatedly;
    /// the substrate coalesces concurrent triggers for the same identity.
    async fn trigger(&self, identity: &WarmCacheIdentity) -> Result<(), WarmerError>;
}

/// Why the gate cold-dispatched instead of allocating against a warm cache.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColdReason {
    /// The bounded wait elapsed without the cache going fresh.
    Timeout,
    /// A freshness probe returned an inventory error.
    InventoryError,
    /// The single-flight warm trigger returned an error.
    WarmerError,
}

/// The terminal decision the gate reached for one dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WarmDispatchDecision {
    /// No-compile stack: the gate was bypassed.
    Bypass,
    /// The cache was fresh on the first probe.
    FreshImmediate,
    /// The cache was `initial` (stale/missing), then went fresh within bound.
    WaitedFresh { initial: WarmFreshness },
    /// The gate gave up and cold-dispatched.
    ColdDispatch { reason: ColdReason },
}

impl WarmDispatchDecision {
    /// `true` when the run should allocate against a warm cache (a hit). Cold
    /// dispatch and bypass still allocate — this only distinguishes the
    /// warm-served path.
    pub fn is_warm_hit(self) -> bool {
        matches!(self, Self::FreshImmediate | Self::WaitedFresh { .. })
    }

    /// Map the decision to its closed `(outcome, reason)` telemetry labels.
    ///
    /// - fresh / waited-fresh → `hit` with the reason it started from;
    /// - cold dispatch → `cold` with the failure reason;
    /// - bypass → `fallback` with `no_compile` (never `cold`).
    pub fn telemetry_labels(self) -> (Outcome, Reason) {
        match self {
            Self::Bypass => (Outcome::Fallback, Reason::NoCompile),
            Self::FreshImmediate => (Outcome::Hit, Reason::Fresh),
            Self::WaitedFresh {
                initial: WarmFreshness::Stale,
            } => (Outcome::Hit, Reason::Stale),
            Self::WaitedFresh {
                initial: WarmFreshness::Missing,
            } => (Outcome::Hit, Reason::Missing),
            // A `WaitedFresh` can only start from stale/missing; treat the
            // impossible fresh-initial case as a plain fresh hit.
            Self::WaitedFresh {
                initial: WarmFreshness::Fresh,
            } => (Outcome::Hit, Reason::Fresh),
            Self::ColdDispatch {
                reason: ColdReason::Timeout,
            } => (Outcome::Cold, Reason::Timeout),
            Self::ColdDispatch {
                reason: ColdReason::InventoryError,
            } => (Outcome::Cold, Reason::InventoryError),
            Self::ColdDispatch {
                reason: ColdReason::WarmerError,
            } => (Outcome::Cold, Reason::WarmerError),
        }
    }
}

/// Bounded warm build-cache freshness gate.
#[derive(Clone, Copy, Debug)]
pub struct WarmDispatchGate {
    /// Maximum time to wait for a missing/stale cache to go fresh.
    pub wait_bound: Duration,
    /// Freshness re-probe cadence during the bounded wait.
    pub poll_interval: Duration,
}

impl Default for WarmDispatchGate {
    fn default() -> Self {
        Self::new(DEFAULT_WAIT_BOUND, DEFAULT_POLL_INTERVAL)
    }
}

impl WarmDispatchGate {
    pub fn new(wait_bound: Duration, poll_interval: Duration) -> Self {
        Self {
            wait_bound,
            poll_interval,
        }
    }

    /// Decide, without allocating, whether the warm cache is usable.
    ///
    /// Mirrors `await_fresh`: fresh returns immediately; a missing/stale cache
    /// fires a single-flight warm and polls until fresh or the bound elapses;
    /// any inventory/warmer error short-circuits to a single cold dispatch. The
    /// wait is always bounded by `self.wait_bound` measured on `clock`.
    pub async fn decide(
        &self,
        compile_mode: CompileMode,
        probe: &dyn WarmBuildCacheProbe,
        identity: &WarmCacheIdentity,
        clock: &dyn Clock,
    ) -> WarmDispatchDecision {
        if compile_mode == CompileMode::None {
            return WarmDispatchDecision::Bypass;
        }

        let initial = match probe.probe(identity).await {
            Ok(WarmFreshness::Fresh) => return WarmDispatchDecision::FreshImmediate,
            Ok(other) => other,
            Err(_) => {
                return WarmDispatchDecision::ColdDispatch {
                    reason: ColdReason::InventoryError,
                };
            }
        };

        // Fire a (single-flight-safe) warm, then poll until fresh or bound.
        if probe.trigger(identity).await.is_err() {
            return WarmDispatchDecision::ColdDispatch {
                reason: ColdReason::WarmerError,
            };
        }

        let deadline = clock.now_instant() + self.wait_bound;
        loop {
            match probe.probe(identity).await {
                Ok(WarmFreshness::Fresh) => {
                    return WarmDispatchDecision::WaitedFresh { initial };
                }
                Ok(_) => {}
                Err(_) => {
                    return WarmDispatchDecision::ColdDispatch {
                        reason: ColdReason::InventoryError,
                    };
                }
            }

            let remaining = deadline.saturating_duration_since(clock.now_instant());
            if remaining.is_zero() {
                return WarmDispatchDecision::ColdDispatch {
                    reason: ColdReason::Timeout,
                };
            }
            tokio::time::sleep(remaining.min(self.poll_interval)).await;
        }
    }

    /// Run the gate before pod allocation: decide, then emit the bounded
    /// per-project `(outcome, reason)` decision metric exactly once.
    ///
    /// The caller MUST invoke this strictly before allocating the task-run pod
    /// and then allocate exactly once regardless of the decision — cold dispatch
    /// is best-effort, mirroring `await_fresh`'s timeout behaviour. Returns the
    /// decision so the caller can log the warm-hit / cold path.
    pub async fn decide_and_record(
        &self,
        compile_mode: CompileMode,
        probe: &dyn WarmBuildCacheProbe,
        identity: &WarmCacheIdentity,
        clock: &dyn Clock,
    ) -> WarmDispatchDecision {
        let decision = self.decide(compile_mode, probe, identity, clock).await;
        let (outcome, reason) = decision.telemetry_labels();
        djinn_telemetry::warm_cache::record_decision(&identity.project_id, outcome, reason);
        decision
    }
}

#[cfg(test)]
#[path = "warm_dispatch_gate_tests.rs"]
mod warm_dispatch_gate_tests;
