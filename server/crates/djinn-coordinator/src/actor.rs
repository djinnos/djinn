// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant as StdInstant};

use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::{self, Interval};
use tokio_util::sync::CancellationToken;

use super::consolidation::{ConsolidationRunner, DbConsolidationRunner};
use super::health;
use super::messages::CoordinatorMessage;
use super::types::*;
use crate::cargo_warm_base_gc::{WarmJobGuard, WarmJobListerGuard};
use crate::roles::RoleRegistry;
use djinn_control_plane::bridge::RuntimeOps;
use djinn_core::clock::{Clock, SystemClock};
use djinn_core::events::DjinnEventEnvelope;
use djinn_core::models::parse_json_array;
use djinn_db::Database;
use djinn_db::NoteRepository;
use djinn_db::ProjectRepository;
use djinn_db::{
    ActivityQuery, DispatchStateRecord, DispatchStateRepository, ReadyQuery, TaskRepository,
};
use djinn_provider::catalog::CatalogService;
use djinn_provider::catalog::health::HealthTracker;
use djinn_provider::rate_limit::suppression_remaining;
use djinn_slot::SlotPoolHandle;

/// Proof token produced only after every enumerated legacy-settings import
/// attempt has completed. The dispatch loop requires this token so startup
/// cannot enter normal dispatch while an import future is still pending.
struct StartupLegacySettingsImportsComplete;

/// Run the finite legacy-settings startup phase to completion before handing
/// its proof token to the normal-dispatch phase.
async fn complete_legacy_settings_import_phase<T, Import, ImportFuture>(
    projects: impl IntoIterator<Item = T>,
    mut import: Import,
) -> StartupLegacySettingsImportsComplete
where
    Import: FnMut(T) -> ImportFuture,
    ImportFuture: std::future::Future<Output = ()>,
{
    for project in projects {
        import(project).await;
    }
    StartupLegacySettingsImportsComplete
}

// ─── Actor (≤20 fields — AGENT-11) ───────────────────────────────────────────

/// Coordinator actor state.
///
/// Durability boundary: `last_dispatched`, `inflight_dispatches`,
/// `dispatch_cooldowns`, and `dispatch_failure_streak` are
/// persisted in the `dispatch_state` table via `DispatchStateRepository` (epic
/// n6xw, proposal 8ipw). The other caches below are deliberately
/// restart-safe-to-lose — they only feed poller/metrics decisions and are cheap
/// to rebuild on the next sweep.
pub(super) struct CoordinatorActor {
    // Ryhl core
    pub(super) receiver: mpsc::Receiver<CoordinatorMessage>,
    pub(super) events: broadcast::Receiver<DjinnEventEnvelope>,
    pub(super) cancel: CancellationToken,
    pub(super) tick: Interval,
    // Dependencies
    pub(super) db: Database,
    pub(super) coordinator_incarnation_id: String,
    pub(super) events_tx: broadcast::Sender<DjinnEventEnvelope>,
    pub(super) pool: SlotPoolHandle,
    /// Durable build admission shared by every task-run dispatch route.
    pub(super) build_admission: Option<Arc<crate::build_admission::BuildAdmissionController>>,
    #[cfg_attr(test, allow(dead_code))]
    pub(super) catalog: CatalogService,
    pub(super) health: HealthTracker,
    pub(super) role_registry: Arc<RoleRegistry>,
    pub(super) lsp: djinn_lsp::LspManager,
    /// Sender clone retained for background tasks that may post results back.
    #[allow(dead_code)]
    pub(super) self_sender: mpsc::Sender<CoordinatorMessage>,
    // Watch channel for lock-free status reads.
    pub(super) status_tx: watch::Sender<SharedCoordinatorState>,
    // State
    pub(super) dispatch_limit: usize,
    pub(super) model_priorities: HashMap<String, Vec<String>>,
    /// Narrow test seam for `resolve_dispatch_models_for_role`. When `false`
    /// (the default), the function returns the fixed `DEFAULT_MODEL_ID` without
    /// touching the credential catalog, preserving historical test behaviour so
    /// every existing dispatch test continues to work without seeding
    /// credentials. Only tests that need to prove owner-scoped credential
    /// filtering set this to `true`, forcing the production credential-lookup
    /// path.
    #[cfg(test)]
    pub(super) test_use_live_credential_resolution: bool,
    /// Per-project PR creation errors (project_id → error message).
    pub(super) pr_errors: HashMap<String, String>,
    /// Durable dispatch-state: per-task dispatch tracking (task UUID → last
    /// dispatch marker), rehydrated from `dispatch_state.last_dispatched_*`.
    /// When a task becomes ready again (no active session) within
    /// `FAILURE_DETECTION_WINDOW` for the same dispatch role, the prior run
    /// failed → it is placed in an escalating cooldown to prevent hot dispatch
    /// loops (missing credential, crash, or a provider returning empty/throttled
    /// turns). A role change is a successful stage transition, not a failure.
    // Persisted in dispatch_state — see epic n6xw and proposal 8ipw
    pub(super) last_dispatched: HashMap<String, DispatchMarker>,
    /// Durable dispatch-state when persisted: in-flight dispatch ledger
    /// (task UUID → (creator, model actually used)).
    /// Recorded the instant a dispatch succeeds and reconciled against the live
    /// slot pool each pass. The per-user concurrency cap is seeded from running
    /// session ROWS, but those don't exist until the worker pod boots and
    /// registers (20-60s after dispatch). Without this ledger, dispatch passes
    /// that re-fire during that window re-seed from a stale-low count and
    /// overshoot the cap (e.g. 8 workers for a cap of 4). This ledger makes a
    /// dispatched-but-not-yet-running task count against the cap immediately.
    /// Admission snapshots capture active task ids before reading aggregate
    /// counts, then add retained reservations; a session-row handoff can be
    /// conservatively double-counted for one pass but can never be missed.
    // Persisted in dispatch_state — see epic n6xw and proposal 8ipw
    pub(super) inflight_dispatches: HashMap<String, InflightDispatch>,
    /// Provisional admission reservations for refinement dispatch.
    ///
    /// Before a refinement task row exists, the dispatch path reserves an
    /// in-memory slot keyed by `proposal_id → (creator, model)` so that the
    /// per-user cap is enforced atomically (check + reserve) before any
    /// side-effecting work. Once the task row is created, the reservation is
    /// re-keyed to the real task id via `inflight_dispatches` and removed from
    /// here. This map is ephemeral (restart-safe-to-lose); reconciliation
    /// against the live pool handles orphaned entries.
    pub(super) provisional_admissions: HashMap<String, InflightDispatch>,
    /// Durable dispatch-state: task UUID → cooldown EXPIRY instant. Persisted as
    /// a wall-clock timestamp and converted to a process-local `StdInstant` on
    /// startup; expired persisted cooldowns are intentionally not reloaded.
    // Persisted in dispatch_state — see epic n6xw and proposal 8ipw
    pub(super) dispatch_cooldowns: HashMap<String, StdInstant>,
    /// Durable dispatch-state: task UUID → count of consecutive failed dispatches, driving the
    /// escalating `dispatch_cooldowns` backoff. Cleared once the task makes a
    /// successful stage transition to a different dispatch role.
    // Persisted in dispatch_state — see epic n6xw and proposal 8ipw
    pub(super) dispatch_failure_streak: HashMap<String, u32>,
    /// Shared tracker for in-flight background tasks.
    pub(super) background_work_tracker: BackgroundWorkTracker,
    /// Cached source for the stranded-ready doctor check. Refreshed each tick
    /// before the cheap doctor subset runs, so the check sees a bounded DB view
    /// without blocking the synchronous `DoctorCheck::run` seam. `None` in tests
    /// that construct the actor directly and do not need the stranded check.
    pub(super) stranded_ready_source:
        Option<Arc<crate::doctor::stranded_ready::TaskRepositoryStrandedReadySource>>,
    /// Cached board-health source for the closed-parent orphan doctor check.
    /// Refreshed immediately before cheap checks so newly terminal parents are
    /// visible without allowing the synchronous check to read the database.
    pub(super) closed_parent_open_children_source:
        Option<Arc<crate::doctor::TaskRepositoryClosedParentOpenChildrenSource>>,
    /// Per-task state of the PR poller's offloaded clean-merge fast path. The
    /// heavy mechanical merge (fetch + ephemeral clone + merge + push) runs in a
    /// spawned background task instead of inline on this tick; the poller reads
    /// this map to decide skip / reopen / resolved each tick. See
    /// [`AutoMergeFastPathState`].
    pub(super) auto_merge_tracker: AutoMergeTracker,
    pub(super) consolidation_runner: Arc<dyn ConsolidationRunner>,
    pub(super) mismatch_scan: crate::doctor::mismatch_scan::MismatchScanCoordinator,
    pub(super) last_stale_sweep: StdInstant,
    /// ADR-051 §7 — timestamp of the last auto-dispatch safety-net sweep.
    pub(super) last_auto_dispatch_sweep: StdInstant,
    /// Timestamp of the last proposal-review backfill sweep (dispatches a
    /// closeout review for drained `building` proposals lacking one).
    pub(super) last_proposal_review_sweep: StdInstant,
    /// ADR-051 §3 — timestamp of the last proactive canonical-graph
    /// staleness refresh sweep (see `GRAPH_REFRESH_INTERVAL`).
    pub(super) last_graph_refresh: StdInstant,
    /// ADR-051 §3 — production canonical-graph warmer.  When `Some`, the
    /// coordinator tick loop calls `trigger` for every dispatch-enabled
    /// project on a 10-minute cadence.  Tests leave this `None`, which makes
    /// the proactive refresh tick branch a no-op.
    pub(super) graph_warmer: Option<Arc<dyn djinn_runtime::GraphWarmerService>>,
    /// Shared bare-mirror manager used by `process_approved_tasks` to build
    /// an `AgentContext` whose direct-push fallback can clone ephemeral
    /// workspaces. `None` in tests.
    pub(super) mirror: Option<Arc<djinn_workspace::MirrorManager>>,
    /// Runtime bridge for coordinator-owned DB-truth finalization paths that
    /// must delete a task-run Job even when no slot-pool mapping remains.
    pub(super) runtime_ops: Option<Arc<dyn RuntimeOps>>,
    /// Host worker RPC connection registry — ground-truth liveness for the
    /// zombie reaper. `None` in tests (reaper falls back to DB/activity heuristics).
    pub(super) rpc_registry: Option<Arc<djinn_supervisor::ConnectionRegistry>>,
    /// Tick counter for association pruning (runs once per ~120 ticks ≈ 1 hour)
    pub(super) prune_tick_counter: u32,
    /// Rolling-window throughput tracking: epic_id → Vec of merge event instants.
    // Restart-safe-to-lose: sliding window for throughput metrics, rebuilt on the next metrics tick.
    pub(super) throughput_events: HashMap<String, Vec<StdInstant>>,
    /// Restart-safe-to-lose: PR status cache; losing it causes one redundant
    /// GitHub CI query, after which the cache is rebuilt.
    /// task_id → last known head SHA.
    ///
    /// Used by the PR poller to skip redundant CI check-run queries when the
    /// PR's head commit has not changed since the previous poll cycle.
    // Restart-safe-to-lose: caches PR open/merged status and is refetched on the next poll cycle.
    pub(super) pr_status_cache: HashMap<String, String>,
    /// Restart-safe-to-lose: tracks when each task was first seen in `pr_draft`
    /// status. Losing it makes the PR poller wait the minimum age again rather
    /// than falsely advancing a task.
    ///
    /// Used by the PR poller to enforce a minimum age before checking CI,
    /// preventing a race where GitHub hasn't registered workflow check-runs
    /// yet and the poller incorrectly concludes CI has passed.
    // Restart-safe-to-lose: draft-first-seen timestamps only throttle ready-to-merge notifier behavior and rebuild naturally on the next poll.
    pub(super) pr_draft_first_seen: HashMap<String, StdInstant>,
    /// Restart-safe-to-lose: task_id → (head SHA, first instant observed with
    /// terminal red blocking CI while parked in `needs_task_review`). Losing it
    /// delays, rather than accelerates, the review-stuck planner intervention.
    pub(super) review_stuck_sha_first_seen: HashMap<String, (String, StdInstant)>,
    /// Restart-safe-to-lose: consecutive merge failure count per task. A restart
    /// resets the recheck threshold, which is safe because the next poll observes
    /// GitHub's current PR/CI state.
    /// After
    /// `MERGE_RETRY_RECHECK_THRESHOLD` failures, the poller invalidates
    /// the CI SHA cache so it re-checks whether CI actually passed.
    // Restart-safe-to-lose: local merge retry backoff counter can reset to zero because the merge retrier reconciles.
    pub(super) merge_fail_count: HashMap<String, u32>,
    /// Restart-safe-to-lose: task_id → head SHA an auto-approve attempt was already made for
    /// (regardless of success). Suppresses retries on the same SHA — needed
    /// when GitHub returns 422 "Can not approve your own pull request" or
    /// when the approval already landed and the next tick hasn't observed
    /// it yet. Stale entries are harmless: a new push bumps the SHA and we
    /// retry once on the new commit.
    // Restart-safe-to-lose: suppresses duplicate auto-approve attempts, and the auto-approve path is idempotent if reset.
    pub(super) auto_approve_attempted: HashMap<String, String>,
    /// Restart-safe-to-lose: task_id → head SHA at the time we handed the PR off to GitHub
    /// (either via auto-merge enablement or direct merge-queue enqueue).
    /// While this entry is present the poller stays in observe-mode and
    /// does not re-attempt the REST merge call. Cleared on:
    ///   * PR merge / close (success)
    ///   * SHA change (a new push invalidated GitHub's queue entry)
    ///   * Merge-queue rejection (`PrCiFailed` reopens the task)
    pub(super) delegated_to_github: HashMap<String, String>,
    /// Restart-safe-to-lose: task_id → head SHA for which we already auto-resolved the PR's review
    /// conversations. Suppresses re-querying GitHub's review threads on every
    /// 30s observe tick when a DIFFERENT protection rule (e.g. an outstanding
    /// CODEOWNERS review) keeps `mergeStateStatus == BLOCKED` after the
    /// conversations are already resolved. Stale entries are harmless: a new
    /// push bumps the SHA and we re-resolve once on the new commit. Cleared
    /// alongside the other per-SHA caches on merge / close / conflict / SHA
    /// change.
    pub(super) conversations_resolved: HashMap<String, String>,
    /// Restart-safe-to-lose: task_id → `created_at` of the merge-queue failure
    /// dequeue event already consumed (task reopened with `PrCiFailed`).
    /// Prevents the sticky dequeue check in `observe_auto_merge_state` from
    /// re-firing on the same event while rework on the same head SHA is still
    /// in flight. A later dequeue carries a new timestamp and is processed
    /// fresh; losing this map on restart at worst re-reopens a task whose
    /// head SHA still matches the rejected one — the correct action anyway.
    pub(super) handled_dequeues: HashMap<String, String>,
    /// Restart-safe-to-lose: SESSION IDs for which a stall-kill has already been issued.  Prevents
    /// repeated kill + activity-log spam while the async lifecycle cleanup
    /// is still in progress (the DB session record stays `running` until
    /// the lifecycle finishes).  Entries are removed when the session
    /// disappears from `list_active()`.  Keyed by session id (not task id) so
    /// a redispatched successor session for the same task is never masked by a
    /// dead predecessor's entry — see `enforce_session_stall_timeout`.
    // Restart-safe-to-lose: records already-issued stall kills; repeated kill attempts on the next sweep are harmless terminal-state no-ops.
    pub(super) stall_killed: HashSet<String>,
    /// Restart-safe-to-lose: per-session watermark of DB-visible progress
    /// (`tokens_in + tokens_out + cache_read + cache_write` from the session
    /// row) observed on the previous stall sweep. The stall backstop compares
    /// the live row against this watermark: if the counters advanced, the
    /// session is demonstrably making progress even though the in-memory
    /// activity tracker is silent (a remote worker whose `touch_activity`
    /// bridge drifted), so it is spared the idle kill and the watermark is
    /// bumped. Keyed by session id; pruned alongside `stall_killed` when the
    /// session leaves `list_active()`.
    pub(super) stall_progress_watermark: HashMap<String, u64>,
    /// Restart-safe-to-lose: consecutive stall-cancelled sessions per TASK id
    /// with no durable task-status progress between them. A task stall-killed
    /// on two back-to-back sessions without advancing its status is caught in a
    /// redispatch loop the reopen-count escalation never observes (the loop
    /// never passes through `open`), so on the second strike we route it to
    /// Planner intervention instead of blindly redispatching a third time.
    /// Reset when the task's status advances or it leaves execution.
    pub(super) stall_cancel_streak: HashMap<String, StallCancelStreak>,
    /// Restart-safe-to-lose: per-session slow-extension count. When the
    /// liveness classifier produces a `Slow` verdict for a stalled session,
    /// the coordinator extends the claim instead of killing. This counter
    /// tracks how many times each session has been extended; after
    /// `SlowExtensionConfig::max_extensions` is reached, the session falls
    /// through to the kill path. Pruned alongside `stall_killed` when the
    /// session leaves `list_active()`.
    pub(super) stall_extension_count: HashMap<String, u32>,
    /// Restart-safe-to-lose: consecutive provider-error FAILED sessions per TASK
    /// id with no durable task-status progress between them. A task whose
    /// session dies on a terminal provider error (e.g. a poisoned transcript
    /// 400, an auth/server fault) is redispatched and fails the same way,
    /// burning attempts in the escalating backoff ladder with nobody deciding
    /// what to do — the cycling-intervention gate (trigger B) deliberately
    /// excludes provider faults, and the stall-cancel escalation only covers
    /// coordinator-initiated stall kills. On the Nth consecutive strike we route
    /// the task to a Planner intervention instead of another doomed redispatch.
    /// Reset when the task's status advances or it leaves execution. Sibling of
    /// `stall_cancel_streak` (see [`STALL_CANCEL_ESCALATION_THRESHOLD`] /
    /// [`FAILURE_ESCALATION_THRESHOLD`]).
    pub(super) provider_failure_streak: HashMap<String, StallCancelStreak>,
    /// Timestamp of the last completed idle-time consolidation sweep (ADR-048 §3A).
    pub(super) last_idle_consolidation: Option<StdInstant>,
    /// Cancellation token for an in-flight idle consolidation sweep.
    /// Cancelled when a new task becomes dispatch-ready.
    pub(super) idle_consolidation_cancel: Option<CancellationToken>,
    /// Join handle for the spawned idle consolidation task.
    pub(super) idle_consolidation_handle: Option<tokio::task::JoinHandle<()>>,
    /// Inline PR/branch cleanup configuration. Consumed by the inline cleanup
    /// hook in the terminal-close dispatch paths (sibling task hrv6).
    #[allow(dead_code)]
    pub(super) pr_cleanup_config: PrCleanupConfig,
    /// Durable-progress / preservation-aware controlled-exit lifecycle config.
    pub(super) worker_lifecycle_config: super::worker_lifecycle::WorkerLifecycleConfig,
    /// Active refinement loops by proposal_id.  The coordinator is the
    /// authoritative source for duplicate-start rejection — a proposal that
    /// already has an entry here cannot be started again until the loop
    /// completes or is terminated.
    pub(super) active_refinements: HashMap<String, super::refinement::RefinementLoopState>,
    /// In-flight refinement sessions by proposal_id.  Tracks which task is
    /// currently running for each active refinement loop so the coordinator
    /// can detect session completion and advance the phase.
    pub(super) refinement_sessions: HashMap<String, super::refinement_dispatch::RefinementSession>,
    // Metrics
    pub(super) dispatched: u64,
    pub(super) recovered: u64,
}

#[cfg(test)]
mod rehydration_tests {
    use super::*;

    fn fixed_now() -> ::time::OffsetDateTime {
        ::time::OffsetDateTime::parse(
            "2026-06-12T00:00:00Z",
            &::time::format_description::well_known::Rfc3339,
        )
        .unwrap()
    }

    #[test]
    fn active_wall_clock_cooldown_converts_to_remaining_instant_deadline() {
        let wall_now = fixed_now();
        let deadline = wall_now + ::time::Duration::seconds(90);

        let remaining = positive_wall_clock_delta(deadline, wall_now).unwrap();

        assert_eq!(remaining, StdDuration::from_secs(90));
    }

    #[test]
    fn expired_wall_clock_cooldown_is_not_loaded() {
        let wall_now = fixed_now();
        let deadline = wall_now - ::time::Duration::seconds(1);

        assert_eq!(positive_wall_clock_delta(deadline, wall_now), None);
    }

    #[test]
    fn persisted_last_dispatch_wall_clock_rehydrates_elapsed_instant() {
        let wall_now = fixed_now();
        let instant_now = StdInstant::now();
        let persisted = wall_now - ::time::Duration::seconds(30);

        let marker_instant = instant_for_persisted_wall_clock(persisted, wall_now, instant_now);

        let elapsed = instant_now.duration_since(marker_instant);
        assert!(elapsed >= StdDuration::from_secs(29));
        assert!(elapsed <= StdDuration::from_secs(31));
    }
}

#[cfg(test)]
mod legacy_settings_startup_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn every_legacy_settings_import_attempt_finishes_before_dispatch_can_begin() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let import_events = Arc::clone(&events);
        let imports_complete = complete_legacy_settings_import_phase(
            ["project-a", "project-b", "project-c"],
            move |project| {
                let import_events = Arc::clone(&import_events);
                async move {
                    import_events
                        .lock()
                        .unwrap()
                        .push(format!("import:{project}:complete"));
                }
            },
        )
        .await;

        // Normal dispatch is type-gated on the proof returned only after every
        // enumerated import attempt has resolved.
        let _imports_complete = imports_complete;
        events.lock().unwrap().push("dispatch:entered".to_owned());
        assert_eq!(
            *events.lock().unwrap(),
            [
                "import:project-a:complete",
                "import:project-b:complete",
                "import:project-c:complete",
                "dispatch:entered",
            ]
        );
    }
}

// Field count: receiver, events, cancel, tick, db, events_tx, pool,
//              catalog, health, role registry, lsp, status, dispatch state,
//              verification, merge/proposal helpers, runtime hooks = ≤20

#[derive(Default)]
pub(super) struct RehydratedDispatchStateSummary {
    pub(super) records: usize,
    pub(super) failure_streaks: usize,
    pub(super) cooldowns: usize,
    pub(super) expired_cooldowns: usize,
    pub(super) last_dispatched: usize,
    pub(super) inflight: usize,
}

fn parse_dispatch_wall_clock_ts(raw: &str) -> Option<::time::OffsetDateTime> {
    use ::time::format_description::well_known::{Iso8601, Rfc3339};

    ::time::OffsetDateTime::parse(raw, &Iso8601::DEFAULT)
        .or_else(|_| ::time::OffsetDateTime::parse(raw, &Rfc3339))
        .ok()
}

fn positive_wall_clock_delta(
    deadline: ::time::OffsetDateTime,
    wall_now: ::time::OffsetDateTime,
) -> Option<StdDuration> {
    if deadline <= wall_now {
        return None;
    }
    (deadline - wall_now).try_into().ok()
}

fn instant_for_persisted_wall_clock(
    persisted_at: ::time::OffsetDateTime,
    wall_now: ::time::OffsetDateTime,
    instant_now: StdInstant,
) -> StdInstant {
    if persisted_at >= wall_now {
        return instant_now;
    }
    let Ok(elapsed): Result<StdDuration, _> = (wall_now - persisted_at).try_into() else {
        return instant_now;
    };
    instant_now.checked_sub(elapsed).unwrap_or(instant_now)
}

fn format_rfc3339(ts: ::time::OffsetDateTime) -> String {
    ts.format(&::time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| ts.to_string())
}

fn format_instant_relative(
    instant: StdInstant,
    instant_now: StdInstant,
    wall_now: ::time::OffsetDateTime,
) -> String {
    let wall = if instant >= instant_now {
        wall_now + (instant - instant_now)
    } else {
        wall_now - instant_now.duration_since(instant)
    };
    format_rfc3339(wall)
}

fn debug_short_id(task_id: &str) -> String {
    task_id.chars().take(8).collect()
}

impl CoordinatorActor {
    pub(super) fn new(
        deps: CoordinatorDeps,
        receiver: mpsc::Receiver<CoordinatorMessage>,
        self_sender: mpsc::Sender<CoordinatorMessage>,
        status_tx: watch::Sender<SharedCoordinatorState>,
    ) -> Self {
        let CoordinatorDeps {
            events_tx,
            cancel,
            db,
            pool,
            build_admission,
            catalog,
            health,
            role_registry,
            background_work_tracker,
            lsp,
            graph_warmer,
            consolidation_runner,
            mirror,
            runtime_ops,
            rpc_registry,
            pr_cleanup_config,
            worker_lifecycle_config,
        } = deps;
        let events = events_tx.subscribe();
        let mut tick = time::interval(STUCK_INTERVAL);
        tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        // Wire the coordinator-side doctor checks into the global registry. The
        // stranded-ready source is cached here and refreshed each tick before the
        // cheap subset runs; the live-mover source is a no-op until the production
        // evidence-collector adapter (T5) is wired.
        let stranded_ready_source = Arc::new(
            crate::doctor::stranded_ready::TaskRepositoryStrandedReadySource::new(
                db.clone(),
                events_tx.clone(),
            ),
        );
        crate::doctor::register_doctor_checks(
            djinn_core::doctor::registry(),
            Arc::new(crate::doctor::live_mover::NoOpLiveMoverSource),
            Arc::clone(&stranded_ready_source) as Arc<dyn crate::doctor::StrandedReadySource>,
        );
        let closed_parent_open_children_source = Arc::new(
            crate::doctor::TaskRepositoryClosedParentOpenChildrenSource::new(
                db.clone(),
                events_tx.clone(),
            ),
        );
        crate::doctor::register_closed_parent_open_children_check_with_repair(
            djinn_core::doctor::registry(),
            Arc::clone(&closed_parent_open_children_source)
                as Arc<dyn crate::doctor::ClosedParentOpenChildrenSource>,
            Arc::clone(&closed_parent_open_children_source)
                as Arc<dyn crate::doctor::ClosedParentOpenChildrenRepairSource>,
        );

        Self {
            receiver,
            events,
            cancel,
            tick,
            db: db.clone(),
            coordinator_incarnation_id: uuid::Uuid::now_v7().to_string(),
            events_tx: events_tx.clone(),
            pool,
            build_admission,
            catalog,
            health,
            role_registry,
            lsp,
            self_sender,
            status_tx,
            dispatch_limit: 50,
            model_priorities: HashMap::new(),
            #[cfg(test)]
            test_use_live_credential_resolution: false,
            pr_errors: HashMap::new(),
            last_dispatched: HashMap::new(),
            inflight_dispatches: HashMap::new(),
            provisional_admissions: HashMap::new(),
            dispatch_cooldowns: HashMap::new(),
            dispatch_failure_streak: HashMap::new(),
            background_work_tracker,
            stranded_ready_source: Some(Arc::clone(&stranded_ready_source)),
            closed_parent_open_children_source: Some(Arc::clone(
                &closed_parent_open_children_source,
            )),
            auto_merge_tracker: Arc::new(std::sync::Mutex::new(HashMap::new())),
            consolidation_runner: consolidation_runner
                .unwrap_or_else(|| Arc::new(DbConsolidationRunner::new(db.clone()))),
            mismatch_scan: crate::doctor::mismatch_scan::MismatchScanCoordinator::new(
                db.clone(),
                crate::events::event_bus_for(&events_tx),
            ),
            last_stale_sweep: SystemClock::new().now_instant(),
            last_auto_dispatch_sweep: SystemClock::new().now_instant(),
            last_proposal_review_sweep: SystemClock::new().now_instant(),
            last_graph_refresh: SystemClock::new().now_instant(),
            graph_warmer,
            mirror,
            runtime_ops,
            rpc_registry,
            prune_tick_counter: 0,
            throughput_events: HashMap::new(),
            pr_status_cache: HashMap::new(),
            pr_draft_first_seen: HashMap::new(),
            review_stuck_sha_first_seen: HashMap::new(),
            merge_fail_count: HashMap::new(),
            auto_approve_attempted: HashMap::new(),
            delegated_to_github: HashMap::new(),
            conversations_resolved: HashMap::new(),
            handled_dequeues: HashMap::new(),
            stall_killed: HashSet::new(),
            stall_progress_watermark: HashMap::new(),
            stall_cancel_streak: HashMap::new(),
            stall_extension_count: HashMap::new(),
            provider_failure_streak: HashMap::new(),
            last_idle_consolidation: None,
            idle_consolidation_cancel: None,
            idle_consolidation_handle: None,
            pr_cleanup_config,
            worker_lifecycle_config,
            active_refinements: HashMap::new(),
            refinement_sessions: HashMap::new(),
            dispatched: 0,
            recovered: 0,
        }
    }

    pub(super) async fn rehydrate_durable_dispatch_state(&mut self) {
        let repo = DispatchStateRepository::new(self.db.clone());
        match repo.cleanup_terminal(&["closed", "done"]).await {
            Ok(pruned) if !pruned.is_empty() => {
                tracing::info!(
                    pruned = pruned.len(),
                    "CoordinatorActor: pruned terminal durable dispatch-state rows"
                )
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(
                error = %e,
                "CoordinatorActor: failed to prune terminal durable dispatch-state rows; continuing startup"
            ),
        }

        let records = match repo.list_all().await {
            Ok(records) => records,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "CoordinatorActor: failed to load durable dispatch state; continuing with empty runtime maps"
                );
                return;
            }
        };

        let summary = self.apply_rehydrated_dispatch_state(
            records,
            ::time::OffsetDateTime::now_utc(),
            SystemClock::new().now_instant(),
        );
        tracing::info!(
            records = summary.records,
            failure_streaks = summary.failure_streaks,
            cooldowns = summary.cooldowns,
            expired_cooldowns = summary.expired_cooldowns,
            last_dispatched = summary.last_dispatched,
            inflight = summary.inflight,
            "CoordinatorActor: rehydrated durable dispatch state"
        );
    }

    pub(super) fn apply_rehydrated_dispatch_state(
        &mut self,
        records: Vec<DispatchStateRecord>,
        wall_now: ::time::OffsetDateTime,
        instant_now: StdInstant,
    ) -> RehydratedDispatchStateSummary {
        let mut summary = RehydratedDispatchStateSummary {
            records: records.len(),
            ..Default::default()
        };

        for record in records {
            let inflight_lane = record
                .last_dispatched_role
                .as_deref()
                .map(djinn_core::models::ModelLane::for_role)
                .unwrap_or(djinn_core::models::ModelLane::Plan);
            if record.failure_streak > 0 {
                self.dispatch_failure_streak.insert(
                    record.task_id.clone(),
                    record.failure_streak.min(u32::MAX as i64) as u32,
                );
                summary.failure_streaks += 1;
            }

            if let Some(deadline) = record
                .cooldown_until
                .as_deref()
                .and_then(parse_dispatch_wall_clock_ts)
            {
                if let Some(remaining) = positive_wall_clock_delta(deadline, wall_now) {
                    self.dispatch_cooldowns
                        .insert(record.task_id.clone(), instant_now + remaining);
                    summary.cooldowns += 1;
                } else {
                    summary.expired_cooldowns += 1;
                }
            }

            if let (Some(dispatched_at), Some(role)) = (
                record
                    .last_dispatched_at
                    .as_deref()
                    .and_then(parse_dispatch_wall_clock_ts),
                record.last_dispatched_role.as_deref(),
            ) {
                let instant =
                    instant_for_persisted_wall_clock(dispatched_at, wall_now, instant_now);
                self.last_dispatched.insert(
                    record.task_id.clone(),
                    DispatchMarker {
                        instant,
                        role: role.to_owned(),
                    },
                );
                summary.last_dispatched += 1;
            }

            if let Some(model_id) = record.inflight_model_id {
                self.inflight_dispatches.insert(
                    record.task_id,
                    InflightDispatch {
                        creator: record.inflight_creator_user_id,
                        model: model_id,
                        lane: inflight_lane,
                    },
                );
                summary.inflight += 1;
            }
        }

        summary
    }

    pub fn dispatch_state_snapshot(&self) -> CoordinatorDebugSnapshot {
        let instant_now = SystemClock::new().now_instant();
        let wall_now = ::time::OffsetDateTime::now_utc();

        let mut cooldowns: Vec<_> = self
            .dispatch_cooldowns
            .iter()
            .filter(|(_, expires_at)| **expires_at > instant_now)
            .map(|(task_id, expires_at)| DebugCooldown {
                task_id: task_id.clone(),
                short_id: debug_short_id(task_id),
                expires_at: format_instant_relative(*expires_at, instant_now, wall_now),
                // Dispatch cooldowns are currently keyed only by task_id; no
                // per-entry scope is tracked, so expose the conservative task scope.
                scope: "task".to_owned(),
            })
            .collect();
        cooldowns.sort_by(|a, b| a.task_id.cmp(&b.task_id));

        let mut failure_streaks: Vec<_> = self
            .dispatch_failure_streak
            .iter()
            .filter(|(_, streak)| **streak > 0)
            .map(|(task_id, streak)| DebugFailureStreak {
                task_id: task_id.clone(),
                short_id: debug_short_id(task_id),
                streak: *streak,
            })
            .collect();
        failure_streaks.sort_by(|a, b| a.task_id.cmp(&b.task_id));

        let mut inflight_ledger: Vec<_> = self
            .inflight_dispatches
            .iter()
            .map(|(task_id, dispatch)| DebugInflightEntry {
                task_id: task_id.clone(),
                short_id: debug_short_id(task_id),
                creator: dispatch.creator.clone(),
                model: dispatch.model.clone(),
                started_at: self
                    .last_dispatched
                    .get(task_id)
                    .map(|marker| format_instant_relative(marker.instant, instant_now, wall_now))
                    .unwrap_or_else(|| format_rfc3339(wall_now)),
            })
            .collect();
        inflight_ledger.sort_by(|a, b| a.task_id.cmp(&b.task_id));

        CoordinatorDebugSnapshot {
            cooldowns,
            failure_streaks,
            inflight_ledger,
        }
    }

    /// Watchdog deadline for a single coordinator pass (one API message, one
    /// domain event, or one safety-net tick).
    ///
    /// The coordinator is a single-mailbox actor: [`run`](Self::run) is one
    /// `select!` loop that services each pass serially. If any pass blocks
    /// forever (the 2026-07-09 incident: tick 72 blocked on an un-timed
    /// coordinator→pool ask while the pool was transiently stalled), the whole
    /// board freezes — dispatch, PR poller, reviewer dispatch, and refinement
    /// driving all starve at once until a manual restart.
    ///
    /// The bounded pool/slot asks (`POOL_ASK_TIMEOUT` / `SLOT_ACK_TIMEOUT`)
    /// address the known cause; this watchdog is the defense-in-depth backstop
    /// for any *other* unbounded await a pass might hit. It is generous
    /// relative to any healthy pass yet far below the multi-minute freeze it
    /// guards against.
    const PASS_DEADLINE: StdDuration = StdDuration::from_secs(120);

    /// Run one coordinator pass under the whole-board-freeze watchdog.
    ///
    /// On elapse the `pass` future is dropped (cancelled) and a loud ERROR is
    /// logged; the caller's `select!` loop then continues to the next
    /// iteration instead of freezing. This is the coordinator analogue of
    /// session stall-kill.
    ///
    /// Cancel-safety: dropping a pass future mid-await cancels whatever async
    /// operation it was suspended on. Every DB mutation the passes perform is a
    /// single transactional repository call (sqlx autocommit, or an explicit
    /// transaction that rolls back on drop), so dropping *between* operations
    /// leaves earlier committed operations applied and any in-flight one rolled
    /// back — never a torn write. The passes are sequences of independent,
    /// idempotent reconcile / dispatch steps that the 30s safety-net tick
    /// re-derives, so an abandoned partial pass is recovered next tick.
    /// `handle_message` may drop an API caller's reply oneshot (the caller sees
    /// an error rather than a hang) and `handle_event_result` may drop a
    /// mid-processed event (the tick re-drives any missed dispatch) — both
    /// strictly better than freezing the entire board.
    pub(super) async fn run_pass_with_watchdog(
        pass_kind: &'static str,
        pass: impl std::future::Future<Output = ()>,
    ) {
        if time::timeout(Self::PASS_DEADLINE, pass).await.is_err() {
            tracing::error!(
                pass_kind,
                deadline_secs = Self::PASS_DEADLINE.as_secs(),
                "CoordinatorActor: pass exceeded watchdog deadline; abandoning it and \
                 continuing the loop (whole-board-freeze backstop)"
            );
        }
    }

    pub(super) async fn run_build_admission_release_pass(&mut self) {
        // Keep the release arm's state small. `dispatch_ready_tasks` is also
        // reachable through event handling, and embedding another copy of its
        // large future directly in `run` made the coordinator future overflow
        // a Tokio worker stack while processing rapid status-change events.
        // Boxing bounds this arm without changing dispatch or watchdog
        // semantics.
        Self::run_pass_with_watchdog(
            "build-admission-release",
            Box::pin(self.dispatch_ready_tasks(None)),
        )
        .await;
    }

    pub(super) async fn run(mut self) {
        tracing::info!("CoordinatorActor started");
        if let Err(error) = djinn_db::CoordinatorIncarnationRepository::new(self.db.clone())
            .register(&self.coordinator_incarnation_id)
            .await
        {
            tracing::error!(incarnation_id = %self.coordinator_incarnation_id, %error, "CoordinatorActor: failed to register coordinator incarnation lease");
        }

        let _startup_imports_complete = match ProjectRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        )
        .list()
        .await
        {
            Ok(projects) => {
                let db = self.db.clone();
                complete_legacy_settings_import_phase(projects, move |project| {
                    let db = db.clone();
                    async move {
                        let checkout = djinn_core::paths::project_dir(
                            &project.github_owner,
                            &project.github_repo,
                        );
                        if let Err(error) = djinn_workspace::import_legacy_settings_file(
                            db,
                            &project.id,
                            &checkout,
                        )
                        .await
                        {
                            tracing::error!(project_id = %project.id, checkout = %checkout.display(), %error, "legacy settings import failed; retained source for this project");
                        }
                    }
                })
                .await
            }
            Err(error) => {
                tracing::error!(%error, "cannot enumerate projects for legacy settings import");
                StartupLegacySettingsImportsComplete
            }
        };

        // Log detected system memory at startup.
        if let Some(mem) = crate::resource_monitor::MemoryStatus::read() {
            tracing::info!(
                total_gb = mem.total_bytes / (1024 * 1024 * 1024),
                available_gb = mem.available_bytes / (1024 * 1024 * 1024),
                effective_limit_gb = mem.effective_limit_bytes / (1024 * 1024 * 1024),
                suggested_max_sessions = mem.suggested_max_sessions(),
                "CoordinatorActor: system memory detected"
            );
        }

        // Startup reap: any `task_runs` row still marked `running` from before
        // this process started is, by definition, orphaned — the worker Pod
        // that owned it can no longer flush a terminal RPC to us. Run the
        // same sweep the 15-min tick uses so the dev UI / queries don't show
        // weeks-old stale rows after every restart.
        health::reap_stale_task_runs_for_startup(&self.db).await;
        // Reap pending task_attempts orphaned while this coordinator was down
        // (or wedged from before the reaper existed) so the respawn guard
        // unblocks those (task, role) pairs immediately after a deploy.
        health::reap_orphaned_pending_attempts_for_startup(&self.db).await;
        let startup_context = self.maintenance_context();
        health::reap_orphaned_taskrun_jobs_for_startup(&self.db, &startup_context).await;
        self.rehydrate_durable_dispatch_state().await;

        // Reconcile refinements whose in-memory loop was lost across this
        // restart: their durable `refinement_start` rows still report `active`
        // but no loop drives them. Stop them cleanly so they don't linger as
        // zombies. Runs before the loop starts, so `active_refinements` is
        // empty and there is no race with a freshly-started refinement.
        self.recover_interrupted_refinements().await;

        // Recover any linked evidence spikes that reached a terminal task
        // state while the coordinator was down, so missed closed-task events are
        // persisted durably. Delegates classification/idempotency to the
        // repository primitive.  For successful findings the durable evidence
        // link/claim is cleared and the in-memory refinement loop is advanced;
        // for failed spikes the proposal remains blocked.
        self.recover_terminal_linked_spike_evidence().await;

        self.run_dispatch_loop(_startup_imports_complete).await;
        tracing::info!("CoordinatorActor stopped");
    }

    /// Enter the normal, potentially infinite dispatch loop only after the
    /// finite startup import phase has completed.
    async fn run_dispatch_loop(&mut self, _imports_complete: StartupLegacySettingsImportsComplete) {
        loop {
            tokio::select! {
                biased;

                // 1. Graceful shutdown via cancellation token.
                _ = self.cancel.cancelled() => {
                    tracing::info!("CoordinatorActor: cancellation token fired, stopping");
                    break;
                }

                // A terminal admission transition releases durable capacity.
                _ = async {
                    if let Some(controller) = self.build_admission.as_ref() {
                        controller.release_notifier().notified().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    self.run_build_admission_release_pass().await;
                }

                // 2. Incoming API messages.
                msg = self.receiver.recv() => {
                    let Some(msg) = msg else {
                        tracing::debug!("CoordinatorActor: message channel closed");
                        break;
                    };
                    Self::run_pass_with_watchdog("message", self.handle_message(msg)).await;
                }

                // 3. Domain events from repositories.
                event = self.events.recv() => {
                    Self::run_pass_with_watchdog("event", self.handle_event_result(event)).await;
                }

                // 4. 30s safety-net tick — stuck detection + dispatch pass for
                //    any tasks that missed an event (e.g. needs_lead_intervention
                //    tasks surviving a server restart).
                _ = self.tick.tick() => {
                    Self::run_pass_with_watchdog("tick", self.run_tick()).await;
                }
            }
        }
    }

    #[tracing::instrument(
        name = "djinn.coordinator.tick",
        skip(self),
        fields(cycle_id = self.prune_tick_counter + 1, pass_kind = "tick")
    )]
    async fn run_tick(&mut self) {
        self.enforce_session_stall_timeout().await;
        self.reap_zombie_sessions().await;
        self.reap_idle_chat_sessions().await;
        self.detect_and_recover_stuck_filtered(None).await;

        self.mismatch_scan
            .trigger(crate::doctor::mismatch_scan::Trigger::Timer)
            .await;

        // Doctor framework integration (epic 4q1t, task 1lx0). Run only the
        // cheap subset so cluster-facing on-demand checks (e.g. k8s.pod_leak)
        // do not inflate every 30s tick. Failures are isolated by the helper
        // and never panic or block the rest of the tick.
        //
        // The run_id is monotonic per-tick so a future `doctor_list_findings`
        // call can scope its query back to one leader-tick invocation.
        if let Some(source) = self.stranded_ready_source.as_ref() {
            source.refresh().await;
        }
        if let Some(source) = self.closed_parent_open_children_source.as_ref() {
            source.refresh().await;
        }
        let doctor_run_id = format!("leader-tick-{}", self.prune_tick_counter.wrapping_add(1));
        crate::doctor::leader_tick::run_cheap_doctor_checks(
            djinn_core::doctor::registry(),
            &self.db,
            &self.events_tx,
            Some(&doctor_run_id),
        )
        .await;

        // Audit sampler scheduler (epic ihf1, task 0utu). Materializes
        // selected audit records into ordinary review tasks at a configurable
        // rate, enforcing max-open and SLO backlog controls. Failures are
        // isolated and never panic or block the rest of the tick.
        let audit_config = crate::audit_sampler::scheduler::AuditSchedulerConfig::default();
        let audit_repo = self.audit_sampler_repo();
        let task_repo = self.task_repo();
        let epic_repo = djinn_db::EpicRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let audit_result = crate::audit_sampler::scheduler::run_audit_scheduler(
            &audit_config,
            &audit_repo,
            &task_repo,
            &epic_repo,
        )
        .await;
        if audit_result.ran && !audit_result.materialized_items.is_empty() {
            tracing::info!(
                materialized = audit_result.materialized_items.len(),
                total_unmaterialized = audit_result.total_unmaterialized,
                "audit scheduler: tick complete"
            );
        }

        // Publish Linux PSI (CPU/memory/IO pressure) through the bounded
        // telemetry helpers. Each resource is published independently, so a
        // partially supported kernel still reports the resources it exposes, and
        // read/parse failures never stop repeated monitor sampling.
        crate::resource_monitor::sample_and_publish_psi();

        // Check memory pressure before dispatching.
        let memory_throttled = if let Some(mem) = crate::resource_monitor::MemoryStatus::read() {
            if mem.is_critical() {
                tracing::error!(
                    psi_full_avg10 = mem.psi_full_avg10,
                    available_mb = mem.available_bytes / (1024 * 1024),
                    "memory pressure CRITICAL — all tasks stalled; skipping dispatch"
                );
                true
            } else if mem.should_throttle() {
                tracing::warn!(
                    psi_some_avg10 = mem.psi_some_avg10,
                    available_mb = mem.available_bytes / (1024 * 1024),
                    "memory pressure elevated — throttling dispatch"
                );
                true
            } else {
                false
            }
        } else {
            false
        };

        if !memory_throttled {
            self.dispatch_ready_tasks(None).await;
            self.drive_active_refinements().await;
        }
        self.process_approved_tasks().await;
        self.poll_pr_statuses().await;
        if self.last_stale_sweep.elapsed() >= STALE_SWEEP_INTERVAL {
            let app_state = self.maintenance_context();
            health::sweep_stale_resources(&self.db, &app_state).await;
            health::renew_coordinator_incarnation(&self.db, &self.coordinator_incarnation_id).await;
            self.last_stale_sweep = SystemClock::new().now_instant();
        }
        if self.last_auto_dispatch_sweep.elapsed() >= AUTO_DISPATCH_SWEEP_INTERVAL {
            self.sweep_stale_auto_dispatches().await;
            self.last_auto_dispatch_sweep = SystemClock::new().now_instant();
        }
        if self.last_proposal_review_sweep.elapsed() >= STALE_SWEEP_INTERVAL {
            self.sweep_proposals_needing_review().await;
            self.sweep_proposals_needing_reconcile().await;
            self.last_proposal_review_sweep = SystemClock::new().now_instant();
        }
        if self.last_graph_refresh.elapsed() >= GRAPH_REFRESH_INTERVAL {
            self.refresh_canonical_graphs_if_stale().await;
            self.last_graph_refresh = SystemClock::new().now_instant();
        }
        // Run association pruning once per ~hour (120 ticks at 30s intervals)
        self.prune_tick_counter += 1;
        if self.prune_tick_counter >= 120 {
            self.prune_tick_counter = 0;
            self.prune_note_associations().await;
            if !self.should_skip_background_llm_work("hourly_note_consolidation") {
                super::consolidation::run_note_consolidation(&self.db, &self.consolidation_runner)
                    .await;
            }
            self.evict_throughput_events();
        }
        // ADR-048 §3A: idle-time memory consolidation.
        // Check if a previously spawned sweep has completed.
        if let Some(handle) = self.idle_consolidation_handle.as_ref()
            && handle.is_finished()
        {
            self.idle_consolidation_handle = None;
            self.idle_consolidation_cancel = None;
            self.last_idle_consolidation = Some(SystemClock::new().now_instant());
            tracing::info!("CoordinatorActor: idle consolidation sweep completed");
        }
        // Only attempt a new sweep when no sweep is already running.
        if self.idle_consolidation_handle.is_none() {
            self.maybe_start_idle_consolidation().await;
        }
    }

    /// Publish current state to the watch channel for lock-free status reads.
    pub(super) fn publish_status(&self) {
        self.record_live_metrics();
        let _ = self.status_tx.send(SharedCoordinatorState {
            dispatched: self.dispatched,
            recovered: self.recovered,
            epic_throughput: self.throughput_snapshot(),
            pr_errors: self.pr_errors.clone(),
            rate_limited_until: self.current_rate_limited_until(),
        });
    }

    /// Publish aggregate coordinator live-state gauges from actor-owned maps.
    ///
    /// This helper is deliberately synchronous: `/metrics` can request a fresh
    /// actor snapshot before rendering without any lock guard crossing an await,
    /// and the storage remains O(1) with no per-task metric labels.
    pub(super) fn record_live_metrics(&self) {
        djinn_telemetry::dispatch::set_cooldowns_active(self.dispatch_cooldowns.len());
        djinn_telemetry::dispatch::set_inflight_ledger_size(self.inflight_dispatches.len());
        let pr_poller_tracked = match self.auto_merge_tracker.lock() {
            Ok(guard) => guard.len(),
            Err(poisoned) => {
                tracing::warn!("auto_merge_tracker mutex poisoned; recovering with data");
                poisoned.into_inner().len()
            }
        };
        djinn_telemetry::pr_poller::set_tracked(pr_poller_tracked);
    }

    pub(super) fn maintenance_context(&self) -> crate::context::CoordinatorContext {
        // If the coordinator is backed by the Kubernetes graph warmer, extract
        // its warm-job lister so the stale-resource sweep uses the same
        // non-terminal Job semantics as the warmer itself. Any other graph
        // warmer implementation leaves the guard empty, falling back to the
        // unavailable default.
        let warm_job_guard = self.graph_warmer.as_ref().and_then(|gw| {
            gw.as_any()
                .downcast_ref::<djinn_k8s::graph_warmer::K8sGraphWarmer>()
                .and_then(|k8s| {
                    k8s.warm_job_lister().map(|lister| {
                        Arc::new(WarmJobListerGuard::new(lister, k8s.namespace().to_string()))
                            as Arc<dyn WarmJobGuard>
                    })
                })
        });
        crate::context::CoordinatorContext {
            db: self.db.clone(),
            event_bus: crate::events::event_bus_for(&self.events_tx),
            git_actors: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            background_work_tasks: self.background_work_tracker.clone(),
            role_registry: self.role_registry.clone(),
            health_tracker: self.health.clone(),
            file_time: Arc::new(crate::file_time::FileTime::new()),
            lsp: self.lsp.clone(),
            catalog: self.catalog.clone(),
            active_tasks: crate::context::ActivityTracker::default(),
            task_ops_project_path_override: None,
            working_root: None,
            graph_warmer: None,
            warm_job_guard,
            repo_graph_ops: None,
            runtime_ops: self.runtime_ops.clone(),
            // Explicit host-side runs root so the periodic sweep targets the
            // directory actually mounted in the server pod
            // (`$DJINN_HOME/cache/cargo-target-runs`) rather than the Job-pod
            // `/cache` convention, which does not exist here. Relying on the
            // sweep's `unwrap_or_else` fallback meant the sweep silently no-op'd
            // on `ErrorKind::NotFound` and `cargo-target-runs` grew unbounded.
            cargo_target_runs_root: Some(djinn_core::paths::cargo_target_runs_root()),
            mirror: self.mirror.clone(),
            rpc_registry: self.rpc_registry.clone(),
            default_project_id: None,
            reconciliation_sweep: crate::context::ReconciliationSweepConfig::from_env(),
            cache_cleanup: crate::context::CacheCleanupConfig::from_env(),
        }
    }

    pub(super) fn current_rate_limited_until(&self) -> Option<StdInstant> {
        let now = SystemClock::new().now_instant();
        suppression_remaining(now).map(|remaining| now + remaining)
    }

    pub(super) fn should_skip_background_llm_work(&self, operation: &str) -> bool {
        let now = SystemClock::new().now_instant();
        if let Some(remaining) = suppression_remaining(now) {
            tracing::info!(
                operation,
                remaining_ms = remaining.as_millis(),
                "CoordinatorActor: skipping non-critical background LLM work during provider suppression window"
            );
            return true;
        }
        false
    }

    async fn handle_message(&mut self, msg: CoordinatorMessage) {
        match msg {
            CoordinatorMessage::TriggerDispatch => {
                // Do NOT run stuck detection here — TriggerDispatch fires on
                // every slot-free event (via trigger_redispatch).  Running the
                // stuck detector each time creates a tight loop when
                // prepare_worktree keeps failing: the detector immediately
                // releases the in_progress task back to open, which gets
                // re-dispatched, fails again, frees the slot, triggers
                // dispatch, etc.  The 30-second tick is sufficient for stuck
                // recovery.
                self.dispatch_ready_tasks(None).await;
            }
            CoordinatorMessage::TriggerProjectDispatch { project_id } => {
                self.dispatch_ready_tasks(Some(&project_id)).await;
            }
            CoordinatorMessage::TriggerStuckScan => {
                self.detect_and_recover_stuck_filtered(None).await;
            }
            CoordinatorMessage::TriggerBoardHealthMismatchScan => {
                self.mismatch_scan
                    .trigger(crate::doctor::mismatch_scan::Trigger::Api)
                    .await;
            }
            CoordinatorMessage::UpdateDispatchLimit { limit } => {
                let limit = limit.max(1);
                if self.dispatch_limit != limit {
                    tracing::info!(
                        old = self.dispatch_limit,
                        new = limit,
                        "CoordinatorActor: updated dispatch limit"
                    );
                    self.dispatch_limit = limit;
                }
            }
            CoordinatorMessage::UpdateModelPriorities { priorities } => {
                self.model_priorities = priorities;
                tracing::info!("CoordinatorActor: updated per-role model priorities");
            }
            CoordinatorMessage::DispatchPlannerEscalation {
                source_task_id,
                reason,
                project_id,
            } => {
                self.dispatch_planner_escalation(&source_task_id, &reason, &project_id)
                    .await;
            }
            CoordinatorMessage::RouteLoopGuardPlannerIntervention {
                source_task_id,
                role,
                reason,
            } => {
                self.route_loop_guard_planner_intervention(&source_task_id, role, &reason)
                    .await;
            }
            CoordinatorMessage::ClearPlannedDispatchCompletion { task_id, reason } => {
                self.clear_planned_dispatch_completion(&task_id, &reason)
                    .await;
                self.route_settled_noop_without_live_mover(&task_id).await;
            }
            CoordinatorMessage::RouteSettledNoopWithoutLiveMover { task_id } => {
                self.route_settled_noop_without_live_mover(&task_id).await;
            }
            CoordinatorMessage::RecordLiveMetrics { reply } => {
                self.record_live_metrics();
                let _ = reply.send(());
            }
            CoordinatorMessage::CheckLiveMover { task_id, reply } => {
                let result = match self.task_repo().get(&task_id).await {
                    Ok(Some(task)) => self
                        .collect_live_mover_evidence(&task)
                        .await
                        .map(|evidence| crate::supervisor_impl::summarize_live_mover(&evidence))
                        .map_err(|err| CoordinatorError::LiveMoverEvidence(err.to_string())),
                    Ok(None) => Err(CoordinatorError::TaskNotFound(task_id)),
                    Err(err) => Err(CoordinatorError::LiveMoverEvidence(err.to_string())),
                };
                let _ = reply.send(result);
            }
            CoordinatorMessage::DebugSnapshot { reply } => {
                let _ = reply.send(self.dispatch_state_snapshot());
            }
            CoordinatorMessage::StartProposalRefinement {
                proposal_id,
                current_revision_seq,
                owner_user_id,
                reply,
            } => {
                let result = self
                    .handle_start_proposal_refinement(
                        &proposal_id,
                        current_revision_seq,
                        owner_user_id,
                    )
                    .await;
                let _ = reply.send(result);
            }
            CoordinatorMessage::DemandProposalRefinementRound {
                proposal_id,
                current_revision_seq,
                reply,
            } => {
                let result = self
                    .handle_demand_proposal_refinement_round(&proposal_id, current_revision_seq)
                    .await;
                let _ = reply.send(result);
            }
            CoordinatorMessage::ResolveRefinementReview {
                proposal_id,
                accept,
                feedback,
                reply,
            } => {
                let result = self
                    .resolve_refinement_review(&proposal_id, accept, feedback)
                    .await;
                let _ = reply.send(result);
            }
        }
    }

    /// Handle a proposal-refinement start request.  Rejects duplicate starts
    /// (coordinator is authoritative) and initialises `RefinementLoopState`.
    async fn handle_start_proposal_refinement(
        &mut self,
        proposal_id: &str,
        current_revision_seq: i32,
        owner_user_id: Option<String>,
    ) -> Result<(), String> {
        use super::refinement::RefinementLoopState;

        // Coordinator-level duplicate rejection — this is authoritative over
        // the lifecycle-level check in the control-plane tool.
        if self.active_refinements.contains_key(proposal_id) {
            return Err(format!(
                "refinement is already active for proposal {proposal_id}"
            ));
        }

        let state = RefinementLoopState::new(proposal_id, current_revision_seq)
            .with_attributed_user(owner_user_id.clone());
        self.active_refinements
            .insert(proposal_id.to_string(), state);

        tracing::info!(
            proposal_id = %proposal_id,
            current_revision_seq,
            owner_user_id = ?owner_user_id,
            "CoordinatorActor: started proposal refinement"
        );

        Ok(())
    }

    /// Handle a demand-round request. Unlike start, this allows restarting
    /// a completed refinement loop. If the loop is still active (not complete),
    /// it returns an error. If the loop has completed or been cleaned up, it
    /// creates a fresh loop and inserts it.
    async fn handle_demand_proposal_refinement_round(
        &mut self,
        proposal_id: &str,
        current_revision_seq: i32,
    ) -> Result<(), String> {
        use super::refinement::RefinementLoopState;

        // If refinement is still actively running (not completed), reject.
        if let Some(state) = self.active_refinements.get(proposal_id)
            && !state.is_complete()
        {
            return Err(format!(
                "refinement is already active for proposal {proposal_id}"
            ));
        }

        let state = RefinementLoopState::new(proposal_id, current_revision_seq);
        self.active_refinements
            .insert(proposal_id.to_string(), state);

        tracing::info!(
            proposal_id = %proposal_id,
            current_revision_seq,
            "CoordinatorActor: demanded another refinement round"
        );

        Ok(())
    }

    async fn handle_event_result(
        &mut self,
        result: Result<DjinnEventEnvelope, broadcast::error::RecvError>,
    ) {
        match result {
            Ok(envelope) => self.handle_event(envelope).await,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(
                    missed = n,
                    "CoordinatorActor: lagged behind event stream, re-subscribing"
                );
                self.events = self.events_tx.subscribe();
                self.detect_and_recover_stuck_filtered(None).await;
                self.dispatch_ready_tasks(None).await;
            }
            Err(broadcast::error::RecvError::Closed) => {
                tracing::warn!("CoordinatorActor: event broadcast channel closed");
            }
        }
    }

    pub(super) async fn handle_event(&mut self, envelope: DjinnEventEnvelope) {
        match (envelope.entity_type, envelope.action) {
            ("activity", "logged") => {
                self.handle_task_outcome_activity(&envelope).await;
            }
            // Epic created → create a planning task for the Planner (wave 1),
            // gated to `open` epics with auto_breakdown enabled.
            ("epic", "created") => {
                let Some(epic) = envelope.parse_payload::<djinn_core::models::Epic>() else {
                    return;
                };
                self.maybe_create_planning_task(&epic).await;
            }
            // Epic updated → if the epic is now open, create a planning task
            // (e.g. a reopened epic, or a re-emitted epic.updated). If the epic
            // is now closed and belongs to a `building` proposal, dispatch a
            // Planner to reconcile that proposal's acceptance criteria.
            ("epic", "updated") => {
                let Some(epic) = envelope.parse_payload::<djinn_core::models::Epic>() else {
                    return;
                };
                self.maybe_create_planning_task(&epic).await;
                if epic.status == "closed" {
                    self.maybe_review_proposal_on_epic_close(&epic).await;
                }
            }
            // Proposal updated → if a material amend landed while the proposal
            // is already building, dispatch a single reconcile task. Status-only
            // updates are filtered by the proposal drift fields.
            ("proposal", "updated") => {
                let Some(proposal) = envelope.parse_payload::<djinn_core::models::Proposal>()
                else {
                    return;
                };
                self.maybe_reconcile_proposal_on_update(&proposal).await;
            }
            // ADR-051 §7 — exit recheck.  When a planner session ends, look
            // up the epic its task was attached to and recheck whether an
            // auto-dispatch should fire (now that the guard no longer skips).
            // Also: classify session exit for protocol-violation detection
            // on ALL session types (not just planner). A status-0 worker
            // exit while the task remains nonterminal is a protocol
            // violation and must count as a failed attempt for retry
            // accounting.
            // SessionRepository emits `started` both when a runtime session is
            // created and when it is subsequently observed running. Binding
            // here makes the UID available to the terminal callback below.
            ("session", "started") => {
                let Some(session) = envelope.parse_payload::<djinn_core::models::SessionRecord>()
                else {
                    return;
                };
                let (Some(task_id), Some(task_run_id)) =
                    (session.task_id.as_deref(), session.task_run_id.as_deref())
                else {
                    return;
                };
                let task_repo = TaskRepository::new(
                    self.db.clone(),
                    crate::events::event_bus_for(&self.events_tx),
                );
                if let Ok(Some(task)) = task_repo.get(task_id).await {
                    self.live_task_run_build_admission(
                        task_id,
                        task.reopen_count.max(0),
                        task_run_id,
                    )
                    .await;
                }
            }
            ("session", "completed" | "interrupted" | "failed") => {
                let Some(session) = envelope.parse_payload::<djinn_core::models::SessionRecord>()
                else {
                    return;
                };
                // ── Protocol-violation classification (all session types) ──
                // When a session ends and the task is still nonterminal,
                // classify the exit and persist structured evidence. This
                // ensures protocol violations are recorded and count as
                // failed attempts. Slow extensions never reach this path
                // (they extend the claim without ending the session).
                // For failed/interrupted exits on a nonterminal task this
                // also terminalizes the live task_attempts row (crashed) so
                // the respawn guard does not defer the (task, role) pair
                // forever on an orphaned pending attempt.
                if let Some(task_id) = session.task_id.as_deref() {
                    if let Some(task_run_id) = session.task_run_id.as_deref() {
                        self.terminal_task_run_build_admission(task_run_id).await;
                    }
                    let _ = self
                        .classify_session_exit_liveness(
                            &session.id,
                            task_id,
                            session.task_run_id.as_deref(),
                            &session.status,
                            &session.agent_type,
                        )
                        .await;
                }
                // Existing planner-specific epic recheck.
                self.handle_planner_session_ended(&session).await;
            }
            ("task", "created") | ("task", "updated") => {
                let Some(task_payload) = envelope
                    .payload
                    .as_object()
                    .and_then(|m| m.get("task"))
                    .cloned()
                else {
                    return;
                };
                let Some(task) =
                    serde_json::from_value::<djinn_core::models::Task>(task_payload).ok()
                else {
                    return;
                };
                if task.status == "closed" {
                    // A cap-denied task has no runtime task-run to emit a terminal callback.
                    if let Some(admission) = &self.build_admission {
                        admission.cancel_deferred_task(&task.id).await;
                    }
                    // Terminalize the live attempt when a task closes via a
                    // force-close path. Best-effort; does not block the event.
                    self.terminalize_force_close_attempt(&task).await;

                    // Record throughput event when a task with a merge commit closes.
                    if task.merge_commit_sha.is_some()
                        && let Some(epic_id) = task.epic_id.as_deref()
                    {
                        self.record_merge_event(epic_id);
                    }
                    self.persist_terminal_linked_spike_evidence_from_closed_task(&task)
                        .await;
                    // Tripwire: a closed `human-review-hold` task means a human
                    // resolved the hold — emit `tripwire.hold.released` on each
                    // held source so the merge-boundary gate can clear for the
                    // current head (no release producer existed before; hold
                    // task `yynd` never cleared its gate on close).
                    self.emit_tripwire_release_on_hold_close(&task).await;
                    // Fire epic completion rules (spike/batch).
                    self.on_task_closed(&task).await;
                }
                if matches!(
                    task.status.as_str(),
                    "open" | "needs_task_review" | "needs_lead_intervention" | "closed"
                ) {
                    tracing::debug!(
                        task_id = %task.short_id,
                        status = %task.status,
                        "CoordinatorActor: ready-task event → dispatch pass"
                    );
                    self.dispatch_ready_tasks(Some(&task.project_id)).await;
                }
            }
            // A credential was (re)connected or replaced — typically the owner
            // reconnecting a provider we marked revoked (set_with_owner clears the
            // revoked mark and emits this). Clear that owner's breaker buckets so
            // tasks that parked / failed over on a dead credential retry the
            // recovered model immediately instead of waiting out the stall
            // cooldown, then nudge a dispatch pass.
            ("credential", "created" | "updated") => {
                let owner = envelope
                    .payload
                    .as_object()
                    .and_then(|m| m.get("owner_user_id"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let cleared = self.health.reset_scope(owner.as_deref());
                if cleared > 0 {
                    tracing::info!(
                        owner = ?owner,
                        cleared,
                        "CoordinatorActor: credential (re)connected — cleared breaker buckets, re-dispatching"
                    );
                    self.dispatch_ready_tasks(None).await;
                }
            }
            _ => {}
        }
    }

    async fn handle_task_outcome_activity(&mut self, envelope: &DjinnEventEnvelope) {
        let Some(payload) = envelope.payload.as_object() else {
            return;
        };

        let Some(task_id) = payload
            .get("task_id")
            .and_then(serde_json::Value::as_str)
            .filter(|task_id| !task_id.is_empty())
        else {
            return;
        };

        let Some(action) = payload.get("action").and_then(serde_json::Value::as_str) else {
            return;
        };
        if action != "status_changed" {
            return;
        }

        let task_repo = self.task_repo();
        let Ok(Some(task)) = task_repo.get(task_id).await else {
            return;
        };

        if let Err(e) = self.maybe_apply_task_outcome_confidence(&task).await {
            tracing::warn!(
                task_id = %task_id,
                error = %e,
                "failed to apply task outcome confidence penalty"
            );
        }
    }

    async fn maybe_apply_task_outcome_confidence(
        &self,
        task: &djinn_core::models::Task,
    ) -> djinn_db::Result<()> {
        if task.status == "closed"
            && task.close_reason.as_deref() == Some("failed")
            && !self
                .task_outcome_marker_exists(task, TASK_OUTCOME_FAILED_CLOSE)
                .await?
        {
            self.apply_task_outcome_confidence_to_task_refs(task)
                .await?;
            self.record_task_outcome_marker(task, TASK_OUTCOME_FAILED_CLOSE)
                .await?;
        }

        if task.status == "open"
            && task.reopen_count > 0
            && !self
                .task_outcome_marker_exists(task, TASK_OUTCOME_REOPEN_COUNT)
                .await?
        {
            self.apply_task_outcome_confidence_to_task_refs(task)
                .await?;
            self.record_task_outcome_marker(task, TASK_OUTCOME_REOPEN_COUNT)
                .await?;
        }

        Ok(())
    }

    async fn task_outcome_marker_exists(
        &self,
        task: &djinn_core::models::Task,
        kind: &str,
    ) -> djinn_db::Result<bool> {
        let task_repo = self.task_repo();
        let entries = task_repo
            .query_activity(ActivityQuery {
                task_id: Some(task.id.clone()),
                event_type: Some(TASK_OUTCOME_CONFIDENCE_ACTIVITY.to_string()),
                actor_role: Some("system".to_string()),
                project_id: None,
                from_time: None,
                to_time: None,
                limit: 100,
                offset: 0,
            })
            .await?;

        let expected_reopen_count = task.reopen_count;
        Ok(entries.iter().any(|entry| {
            serde_json::from_str::<serde_json::Value>(&entry.payload)
                .ok()
                .and_then(|payload| {
                    let marker_kind = payload.get("kind").and_then(serde_json::Value::as_str)?;
                    if marker_kind != kind {
                        return None;
                    }
                    payload
                        .get("reopen_count")
                        .and_then(serde_json::Value::as_i64)
                        .filter(|value| *value == expected_reopen_count)
                        .map(|_| ())
                })
                .is_some()
        }))
    }

    async fn record_task_outcome_marker(
        &self,
        task: &djinn_core::models::Task,
        kind: &str,
    ) -> djinn_db::Result<()> {
        let task_repo = self.task_repo();
        let payload = serde_json::json!({
            "kind": kind,
            "reopen_count": task.reopen_count,
        })
        .to_string();

        task_repo
            .log_activity(
                Some(&task.id),
                "coordinator",
                "system",
                TASK_OUTCOME_CONFIDENCE_ACTIVITY,
                &payload,
            )
            .await?;

        Ok(())
    }

    async fn apply_task_outcome_confidence_to_task_refs(
        &self,
        task: &djinn_core::models::Task,
    ) -> djinn_db::Result<()> {
        let note_repo = NoteRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );

        for permalink in parse_json_array(&task.memory_refs) {
            let Some(note) = note_repo
                .get_by_permalink(&task.project_id, &permalink)
                .await?
            else {
                continue;
            };

            note_repo
                .update_confidence(&note.id, TASK_OUTCOME_CONFIDENCE_SIGNAL)
                .await?;
        }

        Ok(())
    }

    /// ADR-051 §3 proactive canonical-graph staleness refresh.
    ///
    /// Iterates every dispatch-enabled project and fires
    /// [`djinn_runtime::GraphWarmerService::trigger`] — fire-and-forget.  The
    /// warmer implementation owns the cache-freshness short-circuit and
    /// single-flight semantics, so this tick is cheap on a board where
    /// nothing has changed.
    ///
    /// Run from the coordinator tick loop on a 10-minute cadence
    /// (`GRAPH_REFRESH_INTERVAL`).
    pub(super) async fn refresh_canonical_graphs_if_stale(&mut self) {
        let Some(warmer) = self.graph_warmer.clone() else {
            tracing::debug!("CoordinatorActor: graph refresh tick — no warmer injected, skipping");
            return;
        };

        let pause_state = match crate::dispatch_pause::load_dispatch_pause_state(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        )
        .await
        {
            Ok(state) => state,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "CoordinatorActor: graph refresh tick — failed to load dispatch-pause state; deferring warm triggers"
                );
                return;
            }
        };
        if let Some(pause) = crate::dispatch_pause::active_global_dispatch_pause(&pause_state) {
            tracing::info!(
                paused_by = %pause.paused_by,
                paused_at = %pause.paused_at,
                reason = %pause.reason,
                "CoordinatorActor: graph refresh tick deferred by global administrative dispatch pause"
            );
            return;
        }

        let project_repo = ProjectRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let projects = match project_repo.list().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "CoordinatorActor: graph refresh tick — failed to list projects"
                );
                return;
            }
        };

        let mut considered = 0usize;
        let mut skipped_no_code = 0usize;
        for project in projects {
            considered += 1;
            // Skip projects with no indexable code stack. The canonical graph
            // is a CODE graph, so a docs / memory-only repo (no detected
            // language) has nothing to index — warming it just churns empty
            // Jobs. Dispatch no longer depends on the warm (see
            // `is_ready_for_dispatch`), so skipping is purely a saving.
            // `project_has_indexable_code` resolves through the assigned
            // catalog image — catalog projects keep an empty per-project
            // languages block by design.
            if !crate::environment::project_has_indexable_code(&self.db, &project.id).await {
                skipped_no_code += 1;
                tracing::debug!(
                    project_id = %project.id,
                    "CoordinatorActor: graph refresh tick — project has no indexable code stack; skipping warm"
                );
                // Stamp the project as warmed so the UI badge resolves to
                // "ready" instead of being stuck on "Warming" forever: a
                // code-less repo has nothing to index, so "nothing to warm =
                // considered warmed". Best-effort; a stamp failure just means
                // we re-attempt the stamp on the next tick.
                if let Err(e) = project_repo.mark_graph_warmed(&project.id).await {
                    tracing::warn!(
                        project_id = %project.id,
                        error = %e,
                        "CoordinatorActor: graph refresh tick — failed to record graph freshness for code-less project"
                    );
                }
                continue;
            }
            warmer.trigger(&project.id).await;
        }
        tracing::debug!(
            considered,
            skipped_no_code,
            "CoordinatorActor: graph refresh tick complete"
        );
    }

    /// Resolve dispatch models for a given role from configured priorities,
    /// falling back to credential-backed tool-capable models.
    pub(super) async fn resolve_dispatch_models_for_role(
        &self,
        role: &str,
        user_id: Option<&str>,
    ) -> Vec<String> {
        #[cfg(test)]
        if !self.test_use_live_credential_resolution {
            let _ = (role, user_id);
            return vec![DEFAULT_MODEL_ID.to_owned()];
        }

        let cred_repo = djinn_provider::repos::CredentialRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        // Scope eligibility to the SAME credentials the runtime will use for
        // this task's creator (own + org-shared) — never the global unscoped
        // set — so the coordinator can't deem a model dispatchable that the
        // worker then can't authenticate.
        let credentials = match cred_repo.list_for_user(user_id).await {
            Ok(credentials) => credentials,
            Err(_) => return Vec::new(),
        };

        let credential_provider_ids = self.catalog.connected_provider_ids(&credentials);
        if credential_provider_ids.is_empty() {
            return Vec::new();
        }

        let mut selected = Vec::new();
        let mut seen = HashSet::new();

        // Per-role priorities are an OVERRIDE. When a role has none
        // configured, fall back to the "worker" role's priorities as the
        // de-facto per-user default model, so EVERY role (planner, lead,
        // architect, reviewer) is dispatchable out of the box once the user
        // has connected a model — model preference is effectively global
        // per user, with per-role config layered on top.
        //
        // Previously only "architect" fell back here, so planner/lead
        // silently resolved to NO model. That made stuck-task Planner
        // intervention (reopen_count >= REOPEN_INTERVENTION_THRESHOLD) a
        // no-op ("no model configured for planner role") and let tasks loop
        // on the same rejected acceptance criterion forever instead of
        // escalating to a Planner that can decompose/rescope/close them.
        let effective_priorities = self
            .model_priorities
            .get(role)
            .or_else(|| self.model_priorities.get("worker"));

        if let Some(priority_models) = effective_priorities {
            for configured in priority_models {
                if let Some((provider_id, model_name)) = configured.split_once('/') {
                    if !credential_provider_ids.contains(provider_id) {
                        continue;
                    }
                    // Match by model ID, bare name (after last '/'), display
                    // name, or full configured ID.  Internal IDs may be in
                    // HuggingFace form (e.g. "hf:zai-org/GLM-4.7") while
                    // settings store the API form ("synthetic/GLM-4.7").
                    let exists = self.catalog.list_models(provider_id).iter().any(|m| {
                        let bare = m.id.rsplit('/').next().unwrap_or(&m.id);
                        bare == model_name
                            || m.id == model_name
                            || m.name == model_name
                            || m.id == *configured
                    });
                    if exists && seen.insert(configured.clone()) {
                        selected.push(configured.clone());
                    }
                    continue;
                }

                if credential_provider_ids.contains(configured) {
                    let models = self.catalog.list_models(configured);
                    if let Some(model) = models.iter().find(|m| m.tool_call) {
                        let model_id = format!("{configured}/{}", model.id);
                        if seen.insert(model_id.clone()) {
                            selected.push(model_id);
                        }
                    }
                    for model in models {
                        let model_id = format!("{configured}/{}", model.id);
                        if seen.insert(model_id.clone()) {
                            selected.push(model_id);
                        }
                    }
                }
            }
        }

        // When the role resolved no model (no per-role priorities — the
        // common case, since model_priorities is usually empty and workers
        // get their model from the per-USER selection below — or all
        // configured providers disconnected), fall back to the creator's
        // GLOBAL per-user model selection: the SAME `resolve_user_model_priority`
        // the worker dispatch path uses. This is still "only what the user
        // configured" (their global model choice), not random credentials.
        // Without it, escalation roles (planner, lead) silently get
        // NO model and the autonomous stuck-task Planner intervention no-ops
        // ("no model configured for planner role"), so stuck tasks loop on
        // the same rejected acceptance criterion forever instead of
        // escalating to a Planner that can decompose/rescope/close them.
        if selected.is_empty() {
            return self.resolve_user_model_priority(user_id, role).await;
        }
        selected
    }

    pub(super) fn task_repo(&self) -> TaskRepository {
        TaskRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        )
    }

    pub(super) fn audit_sampler_repo(&self) -> djinn_db::AuditSamplerRepository {
        djinn_db::AuditSamplerRepository::new(self.db.clone())
    }

    /// Handle the end of a planner session by re-evaluating the epic its
    /// task was attached to.  Non-planner sessions and task-less sessions
    /// are ignored.
    async fn handle_planner_session_ended(&mut self, session: &djinn_core::models::SessionRecord) {
        if session.agent_type != "planner" {
            return;
        }
        let Some(task_id) = session.task_id.as_deref() else {
            return;
        };
        let task_repo = self.task_repo();
        let task = match task_repo.get(task_id).await {
            Ok(Some(t)) => t,
            _ => return,
        };
        let Some(epic_id) = task.epic_id.as_deref() else {
            return;
        };
        self.recheck_epic_after_planner_end(epic_id).await;
    }

    /// Re-run the eligibility check for an epic that was just touched by
    /// a planner session.  If the epic is eligible and no other planner is
    /// still active on it, fire the auto-dispatch that may have been
    /// suppressed mid-intervention.
    pub(super) async fn recheck_epic_after_planner_end(&mut self, epic_id: &str) {
        let task_repo = self.task_repo();
        if !self
            .epic_is_eligible_for_next_wave(&task_repo, epic_id)
            .await
        {
            return;
        }
        // Still check the active-planner guard — another planner could be
        // running on the same epic.
        if !super::reentrance::should_auto_dispatch_planner(
            &self.db,
            super::reentrance::DispatchEvent::TaskClosed {
                epic_id,
                close_reason: None,
            },
        )
        .await
        {
            return;
        }

        // Derive the project_id from the epic.
        let epic_repo = djinn_db::EpicRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let Ok(Some(epic)) = epic_repo.get(epic_id).await else {
            return;
        };
        self.create_planning_task_by_ids(
            &task_repo,
            epic_id,
            &epic.project_id,
            "post_planner_recheck",
        )
        .await;
    }

    /// ADR-051 §7 — defensive safety-net sweep that rechecks every open
    /// epic for auto-dispatch eligibility.  Catches epics that fell
    /// through all event-driven paths.
    pub(super) async fn sweep_stale_auto_dispatches(&mut self) {
        let epic_repo = djinn_db::EpicRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let epics = match epic_repo.list().await {
            Ok(e) => e
                .into_iter()
                .filter(|e| e.status == "open")
                .collect::<Vec<_>>(),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "CoordinatorActor: ADR-051 stale-sweep failed to list epics",
                );
                return;
            }
        };
        let task_repo = self.task_repo();
        for epic in epics {
            // Wave-1 self-heal. A worker-pod agent (the proposal-decomposition
            // planner, Mode D) creates epics whose `epic.created` event may not
            // reach this host coordinator (RPC boundary); without a backstop
            // such an epic — zero tasks — would never break down, since
            // `epic_is_eligible_for_next_wave` only covers epics that already
            // had a wave. `maybe_create_planning_task` is the wave-1 entry and
            // is fully guarded (bails if any worker/planning task already
            // exists, if the epic has unresolved epic-blockers, or if a planner
            // is already active on it), so calling it here is an idempotent,
            // loop-safe backstop that self-heals within one sweep interval.
            self.maybe_create_planning_task(&epic).await;

            if !self
                .epic_is_eligible_for_next_wave(&task_repo, &epic.id)
                .await
            {
                continue;
            }
            if !super::reentrance::should_auto_dispatch_planner(
                &self.db,
                super::reentrance::DispatchEvent::TaskClosed {
                    epic_id: &epic.id,
                    close_reason: None,
                },
            )
            .await
            {
                continue;
            }
            self.create_planning_task_by_ids(
                &task_repo,
                &epic.id,
                &epic.project_id,
                "stale_auto_dispatch_sweep",
            )
            .await;
        }
    }

    pub(super) async fn project_path_for_id(&self, project_id: &str) -> Option<String> {
        let repo = ProjectRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        repo.get(project_id).await.ok().flatten().map(|p| {
            djinn_core::paths::project_dir(&p.github_owner, &p.github_repo)
                .to_string_lossy()
                .into_owned()
        })
    }

    // ─── Idle-time memory consolidation (ADR-048 §3A) ───────────────────────

    /// Check whether the system is idle (no active slots, no ready tasks) and
    /// enough time has passed since the last consolidation.  If so, spawn a
    /// cancellable background consolidation sweep.
    pub(crate) async fn maybe_start_idle_consolidation(&mut self) {
        // Respect cooldown.
        if let Some(last) = self.last_idle_consolidation
            && last.elapsed() < IDLE_CONSOLIDATION_COOLDOWN
        {
            return;
        }

        // Check pool: all slots must be idle (no active sessions).
        let pool_status = match self.pool.get_status().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "CoordinatorActor: skipping idle consolidation — pool.get_status failed"
                );
                return;
            }
        };
        if pool_status.active_slots > 0 {
            return;
        }

        // Check board: no tasks waiting for dispatch.
        let repo = self.task_repo();
        let has_ready = match repo
            .list_ready(ReadyQuery {
                issue_type: None,
                limit: 1,
                ..Default::default()
            })
            .await
        {
            Ok(tasks) => !tasks.is_empty(),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "CoordinatorActor: skipping idle consolidation — list_ready failed"
                );
                return;
            }
        };
        if has_ready {
            return;
        }

        if self.should_skip_background_llm_work("idle_note_consolidation") {
            self.publish_status();
            return;
        }

        // All idle — spawn the sweep.
        let token = CancellationToken::new();
        let db = self.db.clone();
        let runner = self.consolidation_runner.clone();
        let child_token = token.clone();

        let handle = tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = child_token.cancelled() => {
                    tracing::info!("CoordinatorActor: idle consolidation sweep cancelled");
                }
                _ = super::consolidation::run_note_consolidation(&db, &runner) => {}
            }
        });

        tracing::info!("CoordinatorActor: starting idle-time consolidation sweep");
        self.idle_consolidation_cancel = Some(token);
        self.idle_consolidation_handle = Some(handle);
    }

    /// Cancel any in-flight idle consolidation sweep (e.g. when new work arrives).
    pub(super) fn cancel_idle_consolidation(&mut self) {
        if let Some(token) = self.idle_consolidation_cancel.take() {
            token.cancel();
            tracing::debug!(
                "CoordinatorActor: cancelled idle consolidation sweep (new work arrived)"
            );
        }
        // Drop the handle — the spawned task will wind down on its own.
        self.idle_consolidation_handle = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_slot::{ModelSlotConfig, SlotPoolConfig};
    use std::collections::HashSet;

    fn rendered_metric_sample<'a>(rendered: &'a str, metric: &str) -> &'a str {
        rendered
            .lines()
            .find(|line| line.starts_with(metric))
            .unwrap_or_else(|| panic!("missing metric {metric} in:\n{rendered}"))
    }

    #[tokio::test]
    async fn record_live_metrics_publishes_synthetic_cooldown_and_inflight_state() {
        djinn_telemetry::init().unwrap();

        let db = crate::test_helpers::create_test_db();
        let (events_tx, _events_rx) = broadcast::channel(16);
        let cancel = CancellationToken::new();
        let pool = SlotPoolHandle::spawn(
            crate::test_helpers::agent_context_from_db(db.clone(), cancel.clone()),
            cancel.clone(),
            SlotPoolConfig {
                models: vec![ModelSlotConfig {
                    model_id: DEFAULT_MODEL_ID.to_owned(),
                    max_slots: 1,
                    roles: HashSet::from(["worker".to_owned()]),
                }],
                role_priorities: HashMap::new(),
            },
        );
        let (sender, receiver) = mpsc::channel(1);
        let (status_tx, _status_rx) = watch::channel(SharedCoordinatorState {
            dispatched: 0,
            recovered: 0,
            epic_throughput: HashMap::new(),
            pr_errors: HashMap::new(),
            rate_limited_until: None,
        });
        let mut actor = CoordinatorActor::new(
            CoordinatorDeps::new(
                events_tx,
                cancel,
                db,
                pool,
                CatalogService::new(),
                HealthTracker::new(),
                Arc::new(RoleRegistry::new()),
                BackgroundWorkTracker::default(),
                djinn_lsp::LspManager::new(),
            ),
            receiver,
            sender,
            status_tx,
        );
        actor.dispatch_cooldowns.insert(
            "cooldown-a".to_owned(),
            StdInstant::now() + StdDuration::from_secs(30),
        );
        actor.dispatch_cooldowns.insert(
            "cooldown-b".to_owned(),
            StdInstant::now() + StdDuration::from_secs(60),
        );
        actor.inflight_dispatches.insert(
            "inflight-a".to_owned(),
            InflightDispatch {
                creator: Some("user-a".to_owned()),
                model: DEFAULT_MODEL_ID.to_owned(),
                lane: djinn_core::models::ModelLane::Plan,
            },
        );
        actor.inflight_dispatches.insert(
            "inflight-b".to_owned(),
            InflightDispatch {
                creator: Some("user-b".to_owned()),
                model: DEFAULT_MODEL_ID.to_owned(),
                lane: djinn_core::models::ModelLane::Review,
            },
        );
        actor.inflight_dispatches.insert(
            "inflight-c".to_owned(),
            InflightDispatch {
                creator: None,
                model: DEFAULT_MODEL_ID.to_owned(),
                lane: djinn_core::models::ModelLane::Implement,
            },
        );
        {
            let mut tracker = actor.auto_merge_tracker.lock().unwrap();
            tracker.insert("pr-a".to_owned(), AutoMergeFastPathState::InFlight);
            tracker.insert("pr-b".to_owned(), AutoMergeFastPathState::Reopen);
        }

        actor.record_live_metrics();

        let rendered = djinn_telemetry::render().unwrap();
        assert_eq!(
            rendered_metric_sample(&rendered, "djinn_dispatch_cooldowns_active"),
            "djinn_dispatch_cooldowns_active 2"
        );
        assert_eq!(
            rendered_metric_sample(&rendered, "djinn_inflight_ledger_size"),
            "djinn_inflight_ledger_size 3"
        );
        assert_eq!(
            rendered_metric_sample(&rendered, "djinn_pr_poller_tracked"),
            "djinn_pr_poller_tracked 2"
        );
    }

    /// Create a minimal actor for message-handling tests without spawning the
    /// full run-loop.  Returns the actor and a oneshot-capable sender.
    fn minimal_test_actor() -> CoordinatorActor {
        use crate::test_helpers;
        let db = test_helpers::create_test_db();
        let (events_tx, _events_rx) = broadcast::channel(16);
        let cancel = CancellationToken::new();
        let pool = SlotPoolHandle::spawn(
            test_helpers::agent_context_from_db(db.clone(), cancel.clone()),
            cancel.clone(),
            SlotPoolConfig {
                models: vec![ModelSlotConfig {
                    model_id: DEFAULT_MODEL_ID.to_owned(),
                    max_slots: 1,
                    roles: HashSet::from(["worker".to_owned()]),
                }],
                role_priorities: HashMap::new(),
            },
        );
        let (sender, receiver) = mpsc::channel(64);
        let (status_tx, _status_rx) = watch::channel(SharedCoordinatorState {
            dispatched: 0,
            recovered: 0,
            epic_throughput: HashMap::new(),
            pr_errors: HashMap::new(),
            rate_limited_until: None,
        });
        CoordinatorActor::new(
            CoordinatorDeps::new(
                events_tx,
                cancel,
                db,
                pool,
                CatalogService::new(),
                HealthTracker::new(),
                Arc::new(RoleRegistry::new()),
                BackgroundWorkTracker::default(),
                djinn_lsp::LspManager::new(),
            ),
            receiver,
            sender,
            status_tx,
        )
    }

    #[tokio::test]
    async fn start_proposal_refinement_initializes_loop_state() {
        let mut actor = minimal_test_actor();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        actor
            .handle_message(CoordinatorMessage::StartProposalRefinement {
                proposal_id: "p-1".to_string(),
                current_revision_seq: 0,
                owner_user_id: None,
                reply: reply_tx,
            })
            .await;
        assert!(reply_rx.await.unwrap().is_ok());
        assert!(actor.active_refinements.contains_key("p-1"));
        let state = &actor.active_refinements["p-1"];
        assert_eq!(state.proposal_id, "p-1");
        assert_eq!(state.current_revision_seq, 0);
    }

    #[tokio::test]
    async fn duplicate_refinement_start_is_rejected() {
        let mut actor = minimal_test_actor();
        // First start succeeds.
        let (tx1, rx1) = tokio::sync::oneshot::channel();
        actor
            .handle_message(CoordinatorMessage::StartProposalRefinement {
                proposal_id: "p-dup".to_string(),
                current_revision_seq: 1,
                owner_user_id: None,
                reply: tx1,
            })
            .await;
        assert!(rx1.await.unwrap().is_ok());

        // Second start for the same proposal is rejected.
        let (tx2, rx2) = tokio::sync::oneshot::channel();
        actor
            .handle_message(CoordinatorMessage::StartProposalRefinement {
                proposal_id: "p-dup".to_string(),
                current_revision_seq: 1,
                owner_user_id: None,
                reply: tx2,
            })
            .await;
        let result = rx2.await.unwrap();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("already active"),
            "expected duplicate rejection, got: {err}"
        );
        // Original entry is still present.
        assert!(actor.active_refinements.contains_key("p-dup"));
    }

    #[tokio::test]
    async fn separate_proposals_can_refine_independently() {
        let mut actor = minimal_test_actor();
        let (tx1, rx1) = tokio::sync::oneshot::channel();
        actor
            .handle_message(CoordinatorMessage::StartProposalRefinement {
                proposal_id: "p-a".to_string(),
                current_revision_seq: 0,
                owner_user_id: None,
                reply: tx1,
            })
            .await;
        assert!(rx1.await.unwrap().is_ok());

        let (tx2, rx2) = tokio::sync::oneshot::channel();
        actor
            .handle_message(CoordinatorMessage::StartProposalRefinement {
                proposal_id: "p-b".to_string(),
                current_revision_seq: 2,
                owner_user_id: None,
                reply: tx2,
            })
            .await;
        assert!(rx2.await.unwrap().is_ok());
        assert_eq!(actor.active_refinements.len(), 2);
        assert_eq!(actor.active_refinements["p-b"].current_revision_seq, 2);
    }
}
