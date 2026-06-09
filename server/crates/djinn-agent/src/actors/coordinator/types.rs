use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant as StdInstant};

use super::consolidation::ConsolidationRunner;
use crate::actors::slot::SlotPoolHandle;
use crate::roles::RoleRegistry;
use djinn_core::events::DjinnEventEnvelope;
use djinn_db::Database;
use djinn_provider::catalog::CatalogService;
use djinn_provider::catalog::health::HealthTracker;
use djinn_runtime::GraphWarmerService;
use djinn_supervisor::ConnectionRegistry;
use djinn_workspace::MirrorManager;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Shared tracker for in-flight verification pipelines.  The verification
/// spawner registers task IDs here; the coordinator checks it during stuck
/// detection so it can distinguish live pipelines from orphans after restart.
pub type VerificationTracker = Arc<std::sync::Mutex<HashSet<String>>>;

pub struct CoordinatorDeps {
    pub events_tx: broadcast::Sender<DjinnEventEnvelope>,
    pub cancel: CancellationToken,
    pub db: Database,
    pub pool: SlotPoolHandle,
    pub catalog: CatalogService,
    pub health: HealthTracker,
    pub role_registry: Arc<RoleRegistry>,
    pub verification_tracker: VerificationTracker,
    pub lsp: crate::lsp::LspManager,
    /// Optional ADR-051 §3 canonical-graph warmer.  When `Some`, the
    /// coordinator tick loop calls `trigger` for every dispatch-enabled
    /// project on a 10-minute cadence (see `GRAPH_REFRESH_INTERVAL`).  Tests
    /// and off-server contexts leave this `None`, which makes the proactive
    /// refresh tick branch a no-op.
    pub graph_warmer: Option<Arc<dyn GraphWarmerService>>,
    pub(super) consolidation_runner: Option<Arc<dyn ConsolidationRunner>>,
    /// Shared bare-mirror manager. Threaded into the synthesized `AgentContext`
    /// built inside `process_approved_tasks` so the direct-push merge fallback
    /// can clone the ephemeral workspace from the mirror. `None` in test
    /// contexts — the direct-push path bails cleanly in that case.
    pub mirror: Option<Arc<MirrorManager>>,
    /// Host-side worker RPC connection registry. The zombie reaper consults it
    /// for ground-truth liveness so a long-running but actively-connected K8s
    /// worker is never false-reaped. `None` in off-server/test contexts, where
    /// the reaper falls back to its DB/activity heuristics.
    pub rpc_registry: Option<Arc<ConnectionRegistry>>,
}

impl CoordinatorDeps {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        events_tx: broadcast::Sender<DjinnEventEnvelope>,
        cancel: CancellationToken,
        db: Database,
        pool: SlotPoolHandle,
        catalog: CatalogService,
        health: HealthTracker,
        role_registry: Arc<RoleRegistry>,
        verification_tracker: VerificationTracker,
        lsp: crate::lsp::LspManager,
    ) -> Self {
        Self {
            events_tx,
            cancel,
            db,
            pool,
            catalog,
            health,
            role_registry,
            verification_tracker,
            lsp,
            graph_warmer: None,
            consolidation_runner: None,
            mirror: None,
            rpc_registry: None,
        }
    }

    /// Inject the production canonical-graph warmer, enabling the ADR-051 §3
    /// proactive staleness refresh tick in the coordinator loop.  Tests and
    /// off-server contexts that omit this leave the tick as a no-op.
    pub fn with_graph_warmer(mut self, warmer: Arc<dyn GraphWarmerService>) -> Self {
        self.graph_warmer = Some(warmer);
        self
    }

    /// Inject the production `MirrorManager`, enabling the mirror-native
    /// direct-push merge fallback. Off-server tests skip this and the fallback
    /// returns a descriptive error instead of crashing.
    pub fn with_mirror(mut self, mirror: Arc<MirrorManager>) -> Self {
        self.mirror = Some(mirror);
        self
    }

    /// Inject the host worker RPC connection registry so the zombie reaper can
    /// gate reaping on real connection liveness instead of drift-prone in-memory
    /// slot/activity bookkeeping. Off-server tests omit this.
    pub fn with_rpc_registry(mut self, registry: Arc<ConnectionRegistry>) -> Self {
        self.rpc_registry = Some(registry);
        self
    }
}

// ─── Constants ───────────────────────────────────────────────────────────────

/// Interval between stuck-detection passes (AGENT-08).
pub(super) const STUCK_INTERVAL: Duration = Duration::from_secs(30);
pub(super) const STALE_SWEEP_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// ADR-051 §7 — stale auto-dispatch safety net.  Epics that fell through
/// all event-driven auto-dispatch paths are rechecked at this interval.
pub(super) const AUTO_DISPATCH_SWEEP_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// ADR-051 §3 proactive canonical-graph refresh cadence.
///
/// Every 10 minutes the coordinator asks the canonical-graph warmer to
/// refresh any project whose cache has fallen behind `origin/main`.  The
/// warmer is a no-op on cold caches (those are handled by the
/// first-consumer-demand path in `mcp_bridge::maybe_kick_background_warm`)
/// and on warm caches with `commits_since_pin == 0`, so this tick is cheap
/// for projects that are already current.
pub(super) const GRAPH_REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Minimum cooldown between idle-time memory consolidation sweeps (ADR-048 §3A).
pub(super) const IDLE_CONSOLIDATION_COOLDOWN: Duration = Duration::from_secs(300);

pub(super) const TASK_OUTCOME_CONFIDENCE_ACTIVITY: &str = "task_outcome_confidence";
pub(super) const TASK_OUTCOME_CONFIDENCE_SIGNAL: f64 = 0.1;
pub(super) const TASK_OUTCOME_REOPEN_COUNT: &str = "reopen_count";
pub(super) const TASK_OUTCOME_FAILED_CLOSE: &str = "failed_closed";

/// Activity-log event type recording that the coordinator routed a stuck task
/// to a Planner intervention pass (decompose / rescope / close / apply). One
/// marker is written per `reopen_count` value, which both documents the
/// intervention in the timeline AND serves as the idempotency guard so the
/// coordinator doesn't re-dispatch a Planner every tick while the task sits at
/// the same reopen count (or while the Planner intervention is in flight).
pub(super) const PLANNER_INTERVENTION_MARKER: &str = "planner_intervention";

/// Number of consecutive worker re-attempts (internal review rejections /
/// reopens) after which the coordinator stops re-dispatching the worker and
/// instead routes the task to a Planner intervention pass that DECIDES how to
/// unstick it (decompose into focused subtasks, rescope the acceptance
/// criteria, close as moot/duplicate, or re-spec and re-dispatch).
///
/// Rationale for `3`: a worker gets the original attempt plus two genuine
/// rework rounds against the same reviewer feedback. If the third re-dispatch
/// is about to happen the loop is demonstrably not converging on its own — the
/// same acceptance criterion keeps failing (the p4bb "Shadow gRPC registration"
/// case) — so a planner must reshape the work rather than burn another worker
/// session on it. This mirrors `PR_REVIEW_ROUND_THRESHOLD = 3` on the
/// PR/human-review side so internal and external review loops escalate at the
/// same depth.
pub(super) const REOPEN_INTERVENTION_THRESHOLD: i64 = 3;

/// Number of completed Planner interventions after which a task that has STILL
/// churned back up to `REOPEN_INTERVENTION_THRESHOLD` is parked terminally
/// instead of escalated to the Planner yet again.
///
/// Rationale for `1`: the first time a worker loop exceeds the reopen threshold
/// the Planner gets one pass to reshape it (rescope / decompose / re-spec /
/// close). `reset_intervention_counters` zeroes `reopen_count` and bumps
/// `intervention_count` so the sharpened task re-dispatches cleanly. But if the
/// SAME task climbs back to the threshold a second time, the Planner's reshape
/// demonstrably did not unstick it — re-escalating only resets the counter and
/// the worker loops anew, monopolizing the (often single) dispatch slot
/// indefinitely (the txr4 query_subgraph case: 37 sessions / ~11h / 10 total
/// reopens, where the worker kept re-doing already-accepted functional wiring
/// and never wrote the one required unit test even after the Planner narrowed
/// scope to exactly that). Past this ceiling the coordinator force-closes the
/// task with a recoverable reason so the queue drains; a human (or a freshly
/// scoped task against the existing branch) can finish it.
pub(super) const MAX_PLANNER_INTERVENTIONS: i64 = 1;

#[derive(Debug, Clone, Copy)]
pub(super) struct DispatchMarker {
    pub(super) instant: StdInstant,
    pub(super) role: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReappearingDispatch {
    SameRoleFailure {
        next_streak: u32,
        cooldown: Duration,
    },
    RoleTransition,
}

/// Base cooldown before re-dispatching a task that failed (lifecycle setup, a
/// crashed run, or a provider that returned empty/throttled turns). Escalates
/// per consecutive failure via [`escalating_dispatch_cooldown`] to prevent hot
/// re-dispatch loops that hammer a degraded provider.
pub(super) const DISPATCH_COOLDOWN: Duration = Duration::from_secs(60);

/// Upper bound on the escalating per-task dispatch cooldown.
pub(super) const MAX_DISPATCH_COOLDOWN: Duration = Duration::from_secs(30 * 60);

/// Hard safety cap on a provider-stated retry-after used as a redispatch floor
/// (A6). A provider's reset window is deliberately allowed to EXCEED the
/// escalating ladder's [`MAX_DISPATCH_COOLDOWN`] ceiling — a genuine multi-hour
/// quota window should not be probed on the fixed ~30-min ladder — but is
/// clamped here so a malformed / absurd `Retry-After` can't wedge a task in
/// cooldown effectively forever. 6h comfortably covers real provider reset
/// windows (e.g. a 5-hour Codex quota cycle) while bounding the blast radius.
pub(super) const PROVIDER_RETRY_AFTER_MAX: Duration = Duration::from_secs(6 * 60 * 60);

/// Consecutive failed dispatch attempts (same role) — whether the run failed or
/// the task's owner has no model that resolves a credential — after which the
/// task is failed **terminally** instead of looping forever. Combined with the
/// escalating cooldown this spans hours, so transient provider/credential blips
/// never reach it; only structurally-undispatchable tasks do. Terminal close
/// also self-cleans the patrol guard (a non-closed patrol blocks all future
/// patrols).
pub(super) const MAX_DISPATCH_FAILURES: u32 = 10;

/// A task that becomes dispatch-ready again (with no active session) within
/// this window of its last dispatch is treated as a failed attempt and backed
/// off. Wide enough to catch SLOW failures — e.g. a worker that runs ~30s and
/// then fails on empty/throttled provider turns. The old 10s threshold missed
/// these entirely, so they re-dispatched every coordinator tick and stormed the
/// provider (the root of the 2026-05-27 Codex empty-turn outage amplification).
pub(super) const FAILURE_DETECTION_WINDOW: Duration = Duration::from_secs(35 * 60);

/// Escalating cooldown for a task on its `streak`-th consecutive failed
/// dispatch: `DISPATCH_COOLDOWN * 2^(streak-1)`, capped at [`MAX_DISPATCH_COOLDOWN`].
/// streak 1 → 60s, 2 → 120s, 3 → 240s, … 6+ → 1800s. A transient provider
/// throttle thus decays from "hammered every tick" to "probed every ~30 min".
pub(super) fn escalating_dispatch_cooldown(streak: u32) -> Duration {
    let shift = streak.saturating_sub(1).min(20);
    let secs = DISPATCH_COOLDOWN
        .as_secs()
        .saturating_mul(1u64 << shift)
        .min(MAX_DISPATCH_COOLDOWN.as_secs());
    Duration::from_secs(secs)
}

/// A3: the terminal `dispatch_failure_streak` value to STORE after a same-role
/// failed reappearance.
///
/// A structural failure advances the streak toward [`MAX_DISPATCH_FAILURES`]
/// (the task may be undispatchable). A `throttle` (rate-limit) reappearance is a
/// transient provider fault, not a fault of the task — so it must NOT advance
/// the terminal counter (else a healthy task whose only problem was a quota blip
/// gets terminally closed). The task still backs off via the escalating cooldown
/// and the breaker still fails over; only the terminal-close counter is spared.
pub(super) fn stored_streak_after_failure(
    current_streak: u32,
    next_streak: u32,
    throttle: bool,
) -> u32 {
    if throttle {
        current_streak
    } else {
        next_streak
    }
}

/// A6: floor an escalating-ladder cooldown by a provider-stated retry-after.
///
/// When the provider supplied a `Retry-After` / rate-limit-reset that exceeds
/// the ladder value, redispatch no earlier than that reset — a multi-hour quota
/// window must not be probed on the fixed ~30-min ladder. The provider reset is
/// deliberately allowed to EXCEED the ladder's [`MAX_DISPATCH_COOLDOWN`] ceiling
/// (that's the whole point of A6) but is clamped to [`PROVIDER_RETRY_AFTER_MAX`]
/// so a malformed value can't wedge a task in cooldown forever.
pub(super) fn apply_provider_retry_floor(
    ladder: Duration,
    retry_after_ms: Option<u64>,
) -> Duration {
    match retry_after_ms {
        Some(ms) => {
            let floor = Duration::from_millis(ms).min(PROVIDER_RETRY_AFTER_MAX);
            ladder.max(floor)
        }
        None => ladder,
    }
}

pub(super) fn classify_reappearing_dispatch(
    marker: DispatchMarker,
    current_role: &'static str,
    current_streak: u32,
) -> Option<ReappearingDispatch> {
    if marker.instant.elapsed() >= FAILURE_DETECTION_WINDOW {
        return None;
    }
    if marker.role != current_role {
        return Some(ReappearingDispatch::RoleTransition);
    }

    let next_streak = current_streak.saturating_add(1);
    Some(ReappearingDispatch::SameRoleFailure {
        next_streak,
        cooldown: escalating_dispatch_cooldown(next_streak),
    })
}

#[cfg(test)]
pub(super) const DEFAULT_MODEL_ID: &str = "test/mock";

#[cfg(test)]
mod cooldown_tests {
    use super::*;

    #[test]
    fn escalating_cooldown_grows_then_caps() {
        assert_eq!(escalating_dispatch_cooldown(0), Duration::from_secs(60));
        assert_eq!(escalating_dispatch_cooldown(1), Duration::from_secs(60));
        assert_eq!(escalating_dispatch_cooldown(2), Duration::from_secs(120));
        assert_eq!(escalating_dispatch_cooldown(3), Duration::from_secs(240));
        assert_eq!(escalating_dispatch_cooldown(6), MAX_DISPATCH_COOLDOWN);
        assert_eq!(escalating_dispatch_cooldown(1_000), MAX_DISPATCH_COOLDOWN);
    }

    #[test]
    fn reappearing_same_role_is_failure_and_escalates_from_existing_streak() {
        let marker = DispatchMarker {
            instant: StdInstant::now(),
            role: "worker",
        };

        assert_eq!(
            classify_reappearing_dispatch(marker, "worker", 1),
            Some(ReappearingDispatch::SameRoleFailure {
                next_streak: 2,
                cooldown: Duration::from_secs(120),
            })
        );
    }

    #[test]
    fn reappearing_different_role_is_successful_stage_transition() {
        let marker = DispatchMarker {
            instant: StdInstant::now(),
            role: "worker",
        };

        assert_eq!(
            classify_reappearing_dispatch(marker, "reviewer", 4),
            Some(ReappearingDispatch::RoleTransition)
        );
    }

    #[test]
    fn a3_throttle_does_not_advance_terminal_streak_but_failure_does() {
        // A3: a structural Failure advances the terminal streak toward the cap;
        // a Throttle leaves it where it was (so a transient quota blip can never
        // march a healthy task to terminal close).
        let current = 4;
        let next = 5;
        assert_eq!(
            stored_streak_after_failure(current, next, false),
            next,
            "a non-throttle failure advances the terminal streak"
        );
        assert_eq!(
            stored_streak_after_failure(current, next, true),
            current,
            "a throttle must NOT advance the terminal streak"
        );

        // At the cap boundary: a throttle keeps the stored streak below MAX even
        // when `next_streak` would have hit it, so the terminal-close branch
        // (gated on `!throttle && next_streak >= MAX`) never fires.
        let just_below = MAX_DISPATCH_FAILURES - 1;
        assert_eq!(
            stored_streak_after_failure(just_below, MAX_DISPATCH_FAILURES, true),
            just_below
        );
    }

    #[test]
    fn a6_cooldown_is_max_of_ladder_and_provider_retry_after() {
        // No provider reset → ladder unchanged.
        let ladder = escalating_dispatch_cooldown(2); // 120s
        assert_eq!(apply_provider_retry_floor(ladder, None), ladder);

        // Provider reset BELOW the ladder → ladder wins (max).
        assert_eq!(
            apply_provider_retry_floor(ladder, Some(30_000)),
            ladder,
            "a sub-ladder retry-after never shortens the cooldown"
        );

        // Provider reset ABOVE the ladder → reset floors it.
        let five_min_ms = 5 * 60 * 1000;
        assert_eq!(
            apply_provider_retry_floor(ladder, Some(five_min_ms)),
            Duration::from_millis(five_min_ms)
        );

        // Provider reset EXCEEDING the ladder's 30-min ceiling is honored (the
        // whole point of A6): a 5-hour quota window is not probed on the ladder.
        let five_hours_ms: u64 = 5 * 60 * 60 * 1000;
        let five_hours = Duration::from_millis(five_hours_ms);
        let capped_ladder = escalating_dispatch_cooldown(1_000); // MAX_DISPATCH_COOLDOWN (30m)
        assert_eq!(capped_ladder, MAX_DISPATCH_COOLDOWN);
        assert_eq!(
            apply_provider_retry_floor(capped_ladder, Some(five_hours_ms)),
            five_hours,
            "provider reset is allowed to exceed the 30-min ladder ceiling"
        );
        assert!(five_hours > MAX_DISPATCH_COOLDOWN);

        // A malformed / absurd retry-after is clamped to the hard safety max so
        // it can't wedge the task forever.
        let absurd_ms = 100 * 24 * 60 * 60 * 1000; // 100 days
        assert_eq!(
            apply_provider_retry_floor(ladder, Some(absurd_ms)),
            PROVIDER_RETRY_AFTER_MAX
        );
    }
}

// ─── Error ───────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error("actor channel closed")]
    ActorDead,
    #[error("no response from actor")]
    NoResponse,
}

// ─── Public types ────────────────────────────────────────────────────────────

/// Snapshot of coordinator runtime state (returned by `CoordinatorHandle::get_status`).
#[derive(Debug, Clone)]
pub struct CoordinatorStatus {
    pub tasks_dispatched: u64,
    pub sessions_recovered: u64,
    /// Tasks merged per hour per epic (rolling 1-hour window).
    pub epic_throughput: HashMap<String, usize>,
    /// Per-project PR creation errors (project_id → error message).
    /// Populated when GitHub PR creation fails (e.g. org OAuth restrictions).
    pub pr_errors: HashMap<String, String>,
    /// Shared suppression window propagated from provider rate-limit retries.
    pub rate_limited_until: Option<StdInstant>,
}

/// Internal snapshot published via `watch` channel so `get_status()` reads
/// never queue behind long-running dispatch passes.
#[derive(Debug, Clone)]
pub(super) struct SharedCoordinatorState {
    pub(super) dispatched: u64,
    pub(super) recovered: u64,
    /// Tasks merged per hour per epic (rolling window snapshot).
    pub(super) epic_throughput: HashMap<String, usize>,
    /// Per-project PR creation errors (project_id → error message).
    pub(super) pr_errors: HashMap<String, String>,
    pub(super) rate_limited_until: Option<StdInstant>,
}

impl SharedCoordinatorState {
    pub(super) fn to_status(&self) -> CoordinatorStatus {
        CoordinatorStatus {
            tasks_dispatched: self.dispatched,
            sessions_recovered: self.recovered,
            epic_throughput: self.epic_throughput.clone(),
            pr_errors: self.pr_errors.clone(),
            rate_limited_until: self.rate_limited_until,
        }
    }
}
