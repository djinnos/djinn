use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant as StdInstant};

use super::consolidation::ConsolidationRunner;
use crate::roles::RoleRegistry;
use djinn_control_plane::bridge::RuntimeOps;
use djinn_core::events::DjinnEventEnvelope;
use djinn_db::Database;
use djinn_provider::catalog::CatalogService;
use djinn_provider::catalog::health::HealthTracker;
use djinn_runtime::GraphWarmerService;
use djinn_slot::SlotPoolHandle;
use djinn_supervisor::ConnectionRegistry;
use djinn_workspace::MirrorManager;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

// ─── Re-exports from djinn-orchestration-types ─────────────────────────────
// Types that are pure DTOs and shared between slot and coordinator sides.

pub use djinn_orchestration_types::coordinator::{
    BackgroundWorkTracker, BreakerDebugEntry, CoordinatorDebugSnapshot, DebugCooldown,
    DebugDispatchState, DebugFailureStreak, DebugInflightEntry, DebugSlot, DebugTotals,
    DispatchPauseView,
};

/// State of the PR-poller's mechanical clean-merge fast path for one task.
///
/// The fast path (`try_auto_merge_target_into_task_branch`: fresh mirror fetch +
/// ephemeral clone + merge + push to mirror & GitHub) can take tens of seconds on
/// a large repo. Running it inline on the single-threaded coordinator tick wedged
/// the whole actor — the PR poller ticked once in 20 minutes and dispatch
/// "hung till it recovered". It is now offloaded to a spawned background task; the
/// poller consults this per-task state each tick instead of blocking on the merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutoMergeFastPathState {
    /// A background merge attempt is running. The poller skips the task this
    /// tick (no reopen, no second spawn) and re-evaluates next tick.
    InFlight,
    /// The background attempt completed by landing a clean merge and pushing it.
    /// GitHub will re-evaluate the PR as mergeable; the poller consumes this and
    /// skips the reopen (the old synchronous `true` path).
    Merged,
    /// The background attempt found a real conflict (or could not run: no App,
    /// missing coords, git error). The poller consumes this and falls through to
    /// its normal flag-and-reopen path (the old synchronous `false` path).
    Reopen,
}

/// Shared map of in-flight / just-completed clean-merge fast-path attempts, keyed
/// by task id. Written by the spawned background merge task, read+consumed by the
/// PR poller on the coordinator tick. `Arc<Mutex<..>>` so the detached task can
/// record its outcome without touching `&mut self`.
pub type AutoMergeTracker = Arc<std::sync::Mutex<HashMap<String, AutoMergeFastPathState>>>;

/// Runtime configuration for the inline PR/branch cleanup hook that fires
/// when a task transitions to a terminal CLOSED state.
///
/// This config is threaded into [`PrCleanupPolicy`] at coordinator startup.
/// When `dry_run` is true, every cleanup decision logs at `info` level
/// without making GitHub API calls.
#[derive(Debug, Clone)]
pub struct PrCleanupConfig {
    /// Master switch — when false, no cleanup actions are taken.
    pub enabled: bool,
    /// When true, log intended actions but do not execute GitHub API calls.
    pub dry_run: bool,
    /// Seconds to wait after task close before acting on the PR/branch.
    /// Default: 600 (10 minutes) to absorb merge→close reconciliation lag.
    pub grace_period_secs: u64,
    /// Branches that must never be deleted (e.g. `["main"]`).
    pub protected_branches: Vec<String>,
}

impl Default for PrCleanupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dry_run: false,
            grace_period_secs: 600,
            protected_branches: vec!["main".to_string()],
        }
    }
}

impl PrCleanupConfig {
    /// Convert to the policy-level config used by [`PrCleanupPolicy`].
    #[allow(dead_code)]
    pub(crate) fn to_policy_config(
        &self,
        owner: impl Into<String>,
        repo: impl Into<String>,
    ) -> super::pr_poller::pr_cleanup::PrCleanupPolicyConfig {
        super::pr_poller::pr_cleanup::PrCleanupPolicyConfig {
            enabled: self.enabled,
            dry_run: self.dry_run,
            grace_period: Duration::from_secs(self.grace_period_secs),
            owner: owner.into(),
            repo: repo.into(),
            protected_branches: self.protected_branches.iter().cloned().collect(),
            ..super::pr_poller::pr_cleanup::PrCleanupPolicyConfig::default()
        }
    }
}

pub struct CoordinatorDeps {
    pub events_tx: broadcast::Sender<DjinnEventEnvelope>,
    pub cancel: CancellationToken,
    pub db: Database,
    pub pool: SlotPoolHandle,
    pub catalog: CatalogService,
    pub health: HealthTracker,
    pub role_registry: Arc<RoleRegistry>,
    pub background_work_tracker: BackgroundWorkTracker,
    pub lsp: djinn_lsp::LspManager,
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
    /// Runtime bridge used for best-effort task-run Job teardown in coordinator
    /// recovery paths that finalize DB rows directly (for example zombie reaping
    /// when the slot-pool task→slot mapping has drifted away). `None` in tests
    /// unless a fake is injected; production wires `AppState` through this seam.
    pub runtime_ops: Option<Arc<dyn RuntimeOps>>,
    /// Host-side worker RPC connection registry. The zombie reaper consults it
    /// for ground-truth liveness so a long-running but actively-connected K8s
    /// worker is never false-reaped. `None` in off-server/test contexts, where
    /// the reaper falls back to its DB/activity heuristics.
    pub rpc_registry: Option<Arc<ConnectionRegistry>>,
    /// Runtime configuration for the inline PR/branch cleanup hook.
    pub pr_cleanup_config: PrCleanupConfig,
    /// Durable-progress / preservation-aware worker lifecycle rollout config.
    /// Defaults are safe: no no-progress enforcement, no auto-submit, and no
    /// checkpoint requirement unless explicitly enabled by production config.
    pub worker_lifecycle_config: super::worker_lifecycle::WorkerLifecycleConfig,
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
        background_work_tracker: BackgroundWorkTracker,
        lsp: djinn_lsp::LspManager,
    ) -> Self {
        Self {
            events_tx,
            cancel,
            db,
            pool,
            catalog,
            health,
            role_registry,
            background_work_tracker,
            lsp,
            graph_warmer: None,
            consolidation_runner: None,
            mirror: None,
            runtime_ops: None,
            rpc_registry: None,
            pr_cleanup_config: PrCleanupConfig::default(),
            worker_lifecycle_config: super::worker_lifecycle::WorkerLifecycleConfig::default(),
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

    /// Inject the production runtime bridge for coordinator-owned recovery
    /// teardown. Slot-mapped paths still delete via `SlotPoolHandle`, but direct
    /// DB-truth finalizers need this fallback because there may be no pool
    /// mapping left to consult.
    pub fn with_runtime_ops(mut self, runtime_ops: Arc<dyn RuntimeOps>) -> Self {
        self.runtime_ops = Some(runtime_ops);
        self
    }

    /// Inject the host worker RPC connection registry so the zombie reaper can
    /// gate reaping on real connection liveness instead of drift-prone in-memory
    /// slot/activity bookkeeping. Off-server tests omit this.
    pub fn with_rpc_registry(mut self, registry: Arc<ConnectionRegistry>) -> Self {
        self.rpc_registry = Some(registry);
        self
    }

    /// Override the default inline PR/branch cleanup configuration.
    pub fn with_pr_cleanup_config(mut self, config: PrCleanupConfig) -> Self {
        self.pr_cleanup_config = config;
        self
    }

    /// Override durable-progress / controlled-exit lifecycle rollout config.
    pub fn with_worker_lifecycle_config(
        mut self,
        config: super::worker_lifecycle::WorkerLifecycleConfig,
    ) -> Self {
        self.worker_lifecycle_config = config;
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
/// The marker payload also stores the `quality_strikes` count for audit.
pub(super) const PLANNER_INTERVENTION_MARKER: &str = "planner_intervention";

/// uv3p Part B: activity marker recording that the human-park rung declined to
/// park and redispatched instead (no post-intervention remediation was ever
/// attempted, or a brand-new CI fingerprint deserved one shot). Payload carries
/// the decision (`kind`), the fingerprint when relevant, and the non-attempt
/// model count — an audit trail proving the fleet actually tried before a human
/// is ever paged.
pub(super) const PARK_REDISPATCH_MARKER: &str = "park_attempted_remediation_redispatch";

/// uv3p Part B: number of CONSECUTIVE post-intervention sessions that terminated
/// pre-submission (across DIFFERENT models) after which the human-park rung
/// stops rotating and finally parks. Below this bound the coordinator redispatches
/// with forced model rotation instead of parking; at/above it the task parks
/// with a reason that names the terminated sessions/models rather than the
/// templated "same acceptance criteria kept failing" text.
///
/// Rationale for `2`: the first non-attempt termination (loop-guard trip, infra
/// death, handshake failure) is evidence about that one session/model, not about
/// the task — a single rotation to a different model is cheap and frequently
/// succeeds (kibj would have). Two distinct models both terminating pre-submission
/// is a genuine signal the remediation itself is stuck, so park for a human.
pub(super) const NON_ATTEMPT_PARK_THRESHOLD: usize = 2;

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

/// Autonomous-escalation ceiling: the maximum number of held-remediation
/// escalations (`planner-park-escalation` or legacy `human-review-hold`
/// blockers) the board will spend on a single source task before it gives up
/// LOUDLY instead of parking it for a person.
///
/// Every loop-breaker rung (second-strike, CI-loop, arbiter-deadline park) now
/// routes to an autonomous planner-park escalation rather than a human-review
/// hold. To keep that ladder FINITE — the operator directive is zero human
/// holds, but the board must not escalate forever — the coordinator counts the
/// held-remediation blockers already created for the source (see
/// `planner_escalation_count`). Once that count reaches this ceiling, the next
/// loop-breaker terminally fails (ForceClose) the source with a reason
/// documenting the exhausted ladder rather than creating another escalation. A
/// planner can always resurrect the work from the epic level.
pub(super) const MAX_AUTONOMOUS_ESCALATIONS: i64 = 3;

/// Number of CONSECUTIVE stall-cancelled sessions (with no durable task-status
/// progress between them) after which the coordinator routes the task to a
/// Planner intervention instead of blindly redispatching it again.
///
/// A worker task stall-killed twice in a row without its status advancing is
/// caught in a redispatch loop that the reopen-count escalation (trigger A/B)
/// never observes: a stall kill releases the task straight back into execution
/// without passing through `open`, so `reopen_count` stays flat and the loop is
/// invisible to the reopen threshold. Left alone it can burn a dispatch slot
/// for hours (the overnight incident: one task stall-killed 14 times). On the
/// SECOND strike we hand it to the Planner (decompose / rescope / close) rather
/// than dispatch a third doomed session.
pub(super) const STALL_CANCEL_ESCALATION_THRESHOLD: u32 = 2;

/// Number of CONSECUTIVE provider-error FAILED sessions (with no durable
/// task-status progress between them) after which the coordinator routes the
/// task to a Planner intervention instead of another backoff+redispatch cycle.
///
/// A session that dies on a terminal provider error — the poisoned-transcript
/// 400 (an assistant `tool_calls` message replayed without its tool results),
/// a dead credential, a persistent server fault — is redispatched and fails
/// identically, riding the escalating cooldown ladder toward the terminal close
/// at [`MAX_DISPATCH_FAILURES`] (10) with nobody deciding what to do. The
/// cycling-intervention gate (trigger B) deliberately excludes provider faults
/// (`!had_provider_failure`), and the stall-cancel escalation only covers
/// coordinator-initiated stall kills — so provider-error failures had no
/// planner path at all. Mirroring the stall second-strike, on the THIRD
/// consecutive failed session we hand the task to the Planner (decompose /
/// rescope / close) rather than dispatch a fourth doomed run. Set to `3` (one
/// higher than the stall threshold of 2): the escalating backoff already gives
/// breathing room at streaks 1-2, so a transient provider blip decays without
/// escalating, while a genuinely poisoned/undispatchable task is caught well
/// before the streak-10 terminal close.
pub(super) const FAILURE_ESCALATION_THRESHOLD: u32 = 3;

/// Per-task record of consecutive same-class session failures without durable
/// task-status progress, driving a second-strike Planner escalation. Shared by
/// the stall-cancel streak (see [`STALL_CANCEL_ESCALATION_THRESHOLD`]) and the
/// provider-error failure streak (see [`FAILURE_ESCALATION_THRESHOLD`]).
#[derive(Debug, Clone)]
pub(super) struct StallCancelStreak {
    /// Consecutive same-class failures with no status change between them.
    pub(super) count: u32,
    /// Task status observed at the most recent failure. A different status on
    /// the next failure means the task made durable progress in between, so the
    /// streak resets rather than escalating.
    pub(super) last_status: String,
}

/// Per-session total-token ceiling.  A session that has consumed more than
/// this many tokens across all turns has likely entered a runaway loop
/// (repeated identical tool calls, circular reasoning, or broken self-correction).
/// This is a **session-ownership / runaway guard**, not a provider-health
/// signal — the model may be perfectly healthy; the task itself is stuck.
/// When tripped, the coordinator kills the session and routes the task to a
/// Planner intervention (loop-guard path) rather than redispatching the same
/// worker.  The value is deliberately generous (2_000_000) so only genuine
/// runaways fire; normal multi-turn work stays well under it.
pub(super) const SESSION_TOKEN_CEILING: u64 = 2_000_000;

/// Per-session turn ceiling.  A session that has taken more than this many
/// turns without completing is structurally stuck — either the acceptance
/// criteria are underspecified, the worker is looping, or the task scope
/// is too large.  Like `SESSION_TOKEN_CEILING`, this is a **runaway guard**
/// (not provider-health evidence).  Tripped sessions are killed and routed
/// to Planner intervention.  The value (500) is generous enough that only
/// genuine loops trigger it.
pub(super) const SESSION_TURN_CEILING: u64 = 500;

#[derive(Debug, Clone)]
pub(super) struct DispatchMarker {
    pub(super) instant: StdInstant,
    pub(super) role: String,
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
/// also keeps review-type tasks from wedging open forever.
pub(super) const MAX_DISPATCH_FAILURES: u32 = 10;

/// Consecutive same-role failed dispatch attempts after which the task is
/// routed to a Planner intervention (trigger B) instead of only riding the
/// cooldown ladder toward [`MAX_DISPATCH_FAILURES`].
///
/// Trigger A (`reopen_count >= REOPEN_INTERVENTION_THRESHOLD`) only sees loops
/// that pass through `open`: a reviewer rejection reopens the task and bumps
/// the count. A review-cycle livelock never reopens — the task bounces
/// `needs_task_review → in_progress → needs_task_review`, each cycle a
/// same-role reappearance, `reopen_count` pinned at 0 — so the only
/// bound was the terminal close at streak 10, which force-closes a task whose
/// worker output may be perfectly fine (the t9wi/32bk wedge, 2026-06-11:
/// streaks of 7-8 with 30-min cooldowns while the reviewer stage never ran).
/// Trigger B hands that loop to the Planner for the same decompose / rescope /
/// close decision the reopen path gets, well before the terminal close.
///
/// Rationale for `4`: the original attempt plus three same-role cycles — with
/// the ladder that already spans well over an hour of wall clock, comparable
/// depth to the reopen threshold of 3 — without firing on a one-off pod kill
/// or a transient infra blip that the first cooldown rungs absorb.
pub(super) const STREAK_INTERVENTION_THRESHOLD: u32 = 4;

/// Trigger-B gate: should a same-role failure streak route this dispatch to a
/// Planner intervention?
///
/// Only "the run keeps completing but the task never moves" loops qualify:
///
/// - `had_provider_failure` excludes reappearances explained by a typed
///   provider fault (throttle, auth, server error) — those are provider
///   problems, already bounded by the breaker/failover and the cooldown
///   ladder, not task-shape problems a Planner can fix.
/// - worker/reviewer roles only: a planner-role task must never escalate to
///   another Planner (recursive intervention), and lead/architect loops have
///   their own routing.
pub(super) fn should_route_cycling_intervention(
    role: &str,
    next_streak: u32,
    had_provider_failure: bool,
) -> bool {
    !had_provider_failure
        && next_streak >= STREAK_INTERVENTION_THRESHOLD
        && matches!(role, "worker" | "reviewer")
}

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
            role: "worker".to_owned(),
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
            role: "worker".to_owned(),
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

    #[test]
    fn trigger_b_gate_arms_on_clean_same_role_streak_only() {
        // The review-cycle livelock shape: reviewer-role reappearances with no
        // provider failure, streak at the threshold → arm the Planner route.
        assert!(
            should_route_cycling_intervention("reviewer", STREAK_INTERVENTION_THRESHOLD, false),
            "a clean reviewer streak at the threshold must arm trigger B \
             (the t9wi/32bk review-cycle bounce)"
        );
        assert!(should_route_cycling_intervention(
            "worker",
            STREAK_INTERVENTION_THRESHOLD + 3,
            false
        ));

        // Below the threshold: the ordinary ladder rungs absorb one-off pod
        // kills / infra blips without burning a Planner session.
        assert!(!should_route_cycling_intervention(
            "reviewer",
            STREAK_INTERVENTION_THRESHOLD - 1,
            false
        ));

        // A typed provider failure explains the reappearance — that's a
        // provider problem (breaker/failover territory), not a task-shape
        // problem a Planner can fix.
        assert!(!should_route_cycling_intervention(
            "reviewer",
            STREAK_INTERVENTION_THRESHOLD,
            true
        ));

        // Planner-role tasks must never recursively escalate to a Planner.
        assert!(!should_route_cycling_intervention(
            "planner",
            STREAK_INTERVENTION_THRESHOLD,
            false
        ));

        // Trigger B must arm strictly before the terminal close so the
        // Planner gets its pass before a force-close can fire.
        const {
            assert!(
                STREAK_INTERVENTION_THRESHOLD < MAX_DISPATCH_FAILURES,
                "intervention must precede the terminal-close backstop"
            );
        }
    }
}

// ─── Error ───────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error("actor channel closed")]
    ActorDead,
    #[error("no response from actor")]
    NoResponse,
    #[error("task not found: {0}")]
    TaskNotFound(String),
    #[error("live-mover evidence query failed: {0}")]
    LiveMoverEvidence(String),
    #[error("{0}")]
    Other(String),
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
