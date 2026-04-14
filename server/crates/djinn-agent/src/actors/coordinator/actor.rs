use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant as StdInstant};

use ::time::OffsetDateTime;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::{self, Interval};
use tokio_util::sync::CancellationToken;

use super::consolidation::{ConsolidationRunner, DbConsolidationRunner};
use super::health;
use super::messages::CoordinatorMessage;
use super::rules;
use super::types::*;
use crate::actors::slot::SlotPoolHandle;
use crate::roles::RoleRegistry;
use djinn_core::events::DjinnEventEnvelope;
use djinn_core::models::parse_json_array;
use djinn_db::Database;
use djinn_db::NoteRepository;
use djinn_db::ProjectRepository;
use djinn_db::{
    ActivityQuery, DoltHistoryMaintenanceAction, DoltHistoryMaintenanceExecution,
    DoltHistoryMaintenancePolicy, DoltHistoryMaintenanceService, ReadyQuery, TaskRepository,
};
use djinn_provider::catalog::CatalogService;
use djinn_provider::catalog::health::HealthTracker;
use djinn_provider::rate_limit::suppression_remaining;

// ─── Actor (≤20 fields — AGENT-11) ───────────────────────────────────────────

pub(super) struct CoordinatorActor {
    // Ryhl core
    pub(super) receiver: mpsc::Receiver<CoordinatorMessage>,
    pub(super) events: broadcast::Receiver<DjinnEventEnvelope>,
    pub(super) cancel: CancellationToken,
    pub(super) tick: Interval,
    // Dependencies
    pub(super) db: Database,
    pub(super) events_tx: broadcast::Sender<DjinnEventEnvelope>,
    pub(super) pool: SlotPoolHandle,
    #[cfg_attr(test, allow(dead_code))]
    pub(super) catalog: CatalogService,
    pub(super) health: HealthTracker,
    pub(super) role_registry: Arc<RoleRegistry>,
    pub(super) lsp: crate::lsp::LspManager,
    // Sender clone for background tasks to send results back.
    pub(super) self_sender: mpsc::Sender<CoordinatorMessage>,
    // Watch channel for lock-free status reads.
    pub(super) status_tx: watch::Sender<SharedCoordinatorState>,
    // State
    pub(super) paused_projects: HashSet<String>,
    pub(super) dispatch_limit: usize,
    pub(super) model_priorities: HashMap<String, Vec<String>>,
    // Per-project health: project_id → error message (only unhealthy projects appear here).
    pub(super) unhealthy_projects: HashMap<String, String>,
    /// Per-project PR creation errors (project_id → error message).
    pub(super) pr_errors: HashMap<String, String>,
    /// Per-task dispatch tracking: task UUID → last dispatch instant.
    /// When a task becomes ready again within `RAPID_FAILURE_THRESHOLD` of its
    /// last dispatch, it is placed in cooldown for `DISPATCH_COOLDOWN` to prevent
    /// hot dispatch loops (e.g. missing credential → release → re-dispatch).
    pub(super) last_dispatched: HashMap<String, StdInstant>,
    pub(super) dispatch_cooldowns: HashMap<String, StdInstant>,
    /// Shared tracker for in-flight verification background tasks.
    pub(super) verification_tracker: VerificationTracker,
    pub(super) consolidation_runner: Arc<dyn ConsolidationRunner>,
    pub(super) last_stale_sweep: StdInstant,
    /// ADR-051 §7 — timestamp of the last auto-dispatch safety-net sweep.
    pub(super) last_auto_dispatch_sweep: StdInstant,
    /// ADR-051 §3 — timestamp of the last proactive canonical-graph
    /// staleness refresh sweep (see `GRAPH_REFRESH_INTERVAL`).
    pub(super) last_graph_refresh: StdInstant,
    /// ADR-051 §3 — production canonical-graph warmer.  When `Some`, the
    /// coordinator tick loop calls `maybe_refresh_if_stale` for every
    /// dispatch-enabled project on a 10-minute cadence.  Tests leave this
    /// `None`, which makes the proactive refresh tick branch a no-op.
    pub(super) canonical_graph_warmer: Option<Arc<dyn crate::context::CanonicalGraphWarmer>>,
    /// Tick counter for association pruning (runs once per ~120 ticks ≈ 1 hour)
    pub(super) prune_tick_counter: u32,
    /// Timestamp of the last patrol completion (or actor start as initial baseline).
    /// The next patrol is eligible only after `next_patrol_interval` has elapsed
    /// since this instant.  Reset when a patrol task reaches a terminal state.
    pub(super) last_patrol_completed: StdInstant,
    /// Dynamic patrol interval, set by the planner's self-scheduling.
    /// Defaults to `rules::DEFAULT_PLANNER_PATROL_INTERVAL`.
    /// Updated when the coordinator reads a `patrol_schedule` activity entry.
    /// Per ADR-051 §1 the Planner owns the board patrol.
    pub(super) next_patrol_interval: Duration,
    /// Rolling-window throughput tracking: epic_id → Vec of merge event instants.
    pub(super) throughput_events: HashMap<String, Vec<StdInstant>>,
    /// Per-task Lead escalation count (request_lead call count per task UUID).
    /// When a task accumulates ≥ 2 escalations, the next request_lead routes to Architect.
    pub(super) escalation_counts: HashMap<String, u32>,
    /// PR status cache: task_id → last known head SHA.
    ///
    /// Used by the PR poller to skip redundant CI check-run queries when the
    /// PR's head commit has not changed since the previous poll cycle.
    pub(super) pr_status_cache: HashMap<String, String>,
    /// Tracks when each task was first seen in `pr_draft` status.
    ///
    /// Used by the PR poller to enforce a minimum age before checking CI,
    /// preventing a race where GitHub hasn't registered workflow check-runs
    /// yet and the poller incorrectly concludes CI has passed.
    pub(super) pr_draft_first_seen: HashMap<String, StdInstant>,
    /// Consecutive merge failure count per task.  After
    /// `MERGE_RETRY_RECHECK_THRESHOLD` failures, the poller invalidates
    /// the CI SHA cache so it re-checks whether CI actually passed.
    pub(super) merge_fail_count: HashMap<String, u32>,
    /// Task IDs for which a stall-kill has already been issued.  Prevents
    /// repeated kill + activity-log spam while the async lifecycle cleanup
    /// is still in progress (the DB session record stays `running` until
    /// the lifecycle finishes).  Entries are removed when the session
    /// disappears from `list_active()`.
    pub(super) stall_killed: HashSet<String>,
    /// Timestamp of the last completed idle-time consolidation sweep (ADR-048 §3A).
    pub(super) last_idle_consolidation: Option<StdInstant>,
    /// Cancellation token for an in-flight idle consolidation sweep.
    /// Cancelled when a new task becomes dispatch-ready.
    pub(super) idle_consolidation_cancel: Option<CancellationToken>,
    /// Join handle for the spawned idle consolidation task.
    pub(super) idle_consolidation_handle: Option<tokio::task::JoinHandle<()>>,
    // Metrics
    pub(super) dispatched: u64,
    pub(super) recovered: u64,
}

// Field count: receiver, events, cancel, tick, db, events_tx, pool,
//              catalog, health, paused, dispatched, recovered = 12 ✓ (≤20)

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
            catalog,
            health,
            role_registry,
            verification_tracker,
            lsp,
            canonical_graph_warmer,
            consolidation_runner,
        } = deps;
        let events = events_tx.subscribe();
        let mut tick = time::interval(STUCK_INTERVAL);
        tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        Self {
            receiver,
            events,
            cancel,
            tick,
            db: db.clone(),
            events_tx,
            pool,
            catalog,
            health,
            role_registry,
            lsp,
            self_sender,
            status_tx,
            paused_projects: HashSet::new(),
            dispatch_limit: 50,
            model_priorities: HashMap::new(),
            unhealthy_projects: HashMap::new(),
            pr_errors: HashMap::new(),
            last_dispatched: HashMap::new(),
            dispatch_cooldowns: HashMap::new(),
            verification_tracker,
            consolidation_runner: consolidation_runner
                .unwrap_or_else(|| Arc::new(DbConsolidationRunner::new(db.clone()))),
            last_stale_sweep: StdInstant::now(),
            last_auto_dispatch_sweep: StdInstant::now(),
            last_graph_refresh: StdInstant::now(),
            canonical_graph_warmer,
            prune_tick_counter: 0,
            last_patrol_completed: StdInstant::now(),
            next_patrol_interval: rules::DEFAULT_PLANNER_PATROL_INTERVAL,
            throughput_events: HashMap::new(),
            escalation_counts: HashMap::new(),
            pr_status_cache: HashMap::new(),
            pr_draft_first_seen: HashMap::new(),
            merge_fail_count: HashMap::new(),
            stall_killed: HashSet::new(),
            last_idle_consolidation: None,
            idle_consolidation_cancel: None,
            idle_consolidation_handle: None,
            dispatched: 0,
            recovered: 0,
        }
    }

    pub(super) async fn run(mut self) {
        tracing::info!("CoordinatorActor started");

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

        // Always start with execution paused for all projects.
        #[cfg(not(test))]
        {
            let repo = djinn_db::ProjectRepository::new(
                self.db.clone(),
                crate::events::event_bus_for(&self.events_tx),
            );
            if let Ok(projects) = repo.list().await {
                for p in projects {
                    self.paused_projects.insert(p.id);
                }
            }
            tracing::info!(
                count = self.paused_projects.len(),
                "CoordinatorActor: all projects start paused"
            );
            self.publish_status();
        }

        loop {
            tokio::select! {
                biased;

                // 1. Graceful shutdown via cancellation token.
                _ = self.cancel.cancelled() => {
                    tracing::info!("CoordinatorActor: cancellation token fired, stopping");
                    break;
                }

                // 2. Incoming API messages.
                msg = self.receiver.recv() => {
                    let Some(msg) = msg else {
                        tracing::debug!("CoordinatorActor: message channel closed");
                        break;
                    };
                    self.handle_message(msg).await;
                }

                // 3. Domain events from repositories.
                event = self.events.recv() => {
                    self.handle_event_result(event).await;
                }

                // 4. 30s safety-net tick — stuck detection + dispatch pass for
                //    any tasks that missed an event (e.g. needs_lead_intervention
                //    tasks surviving a server restart).
                _ = self.tick.tick() => {
                    self.enforce_session_stall_timeout().await;
                    self.detect_and_recover_stuck_filtered(None).await;

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
                    }
                    self.process_approved_tasks().await;
                    self.poll_pr_statuses().await;
                    if self.last_stale_sweep.elapsed() >= STALE_SWEEP_INTERVAL {
                        let app_state = crate::context::AgentContext {
                            db: self.db.clone(),
                            event_bus: crate::events::event_bus_for(&self.events_tx),
                            git_actors: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
                            verifying_tasks: self.verification_tracker.clone(),
                            role_registry: self.role_registry.clone(),
                            health_tracker: self.health.clone(),
                            file_time: Arc::new(crate::file_time::FileTime::new()),
                            lsp: self.lsp.clone(),
                            catalog: self.catalog.clone(),
                            coordinator: Arc::new(tokio::sync::Mutex::new(None)),
                            active_tasks: crate::context::ActivityTracker::default(),
                            task_ops_project_path_override: None,
                            working_root: None,
                            canonical_graph_warmer: None,
                            repo_graph_ops: None,
                        };
                        health::sweep_stale_resources(&self.db, &app_state).await;
                        self.last_stale_sweep = StdInstant::now();
                    }
                    if self.last_auto_dispatch_sweep.elapsed() >= AUTO_DISPATCH_SWEEP_INTERVAL {
                        self.sweep_stale_auto_dispatches().await;
                        self.last_auto_dispatch_sweep = StdInstant::now();
                    }
                    if self.last_graph_refresh.elapsed() >= GRAPH_REFRESH_INTERVAL {
                        self.refresh_canonical_graphs_if_stale().await;
                        self.last_graph_refresh = StdInstant::now();
                    }
                    // Run association pruning once per ~hour (120 ticks at 30s intervals)
                    self.prune_tick_counter += 1;
                    if self.prune_tick_counter >= 120 {
                        self.prune_tick_counter = 0;
                        self.run_dolt_history_maintenance_tick().await;
                        self.prune_note_associations().await;
                        if !self.should_skip_background_llm_work("hourly_note_consolidation") {
                            super::consolidation::run_note_consolidation(&self.db, &self.consolidation_runner).await;
                        }
                        self.evict_throughput_events();
                        if !self.should_skip_background_llm_work("hourly_prompt_amendment_evaluation") {
                            self.evaluate_prompt_amendments().await;
                        }
                    }
                    // Planner patrol (per ADR-051 §1): completion-time-based scheduling.
                    // Only attempt dispatch once `next_patrol_interval` has
                    // elapsed since the last patrol completed (or actor start).
                    if self.last_patrol_completed.elapsed() >= self.next_patrol_interval {
                        self.maybe_dispatch_planner_patrol().await;
                    }

                    // ADR-048 §3A: idle-time memory consolidation.
                    // Check if a previously spawned sweep has completed.
                    if let Some(handle) = self.idle_consolidation_handle.as_ref()
                        && handle.is_finished()
                    {
                        self.idle_consolidation_handle = None;
                        self.idle_consolidation_cancel = None;
                        self.last_idle_consolidation = Some(StdInstant::now());
                        tracing::info!("CoordinatorActor: idle consolidation sweep completed");
                    }
                    // Only attempt a new sweep when no sweep is already running.
                    if self.idle_consolidation_handle.is_none() {
                        self.maybe_start_idle_consolidation().await;
                    }
                }
            }
        }
        tracing::info!("CoordinatorActor stopped");
    }

    /// Publish current state to the watch channel for lock-free status reads.
    pub(super) fn publish_status(&self) {
        let _ = self.status_tx.send(SharedCoordinatorState {
            paused_projects: self.paused_projects.clone(),
            unhealthy_project_ids: self.unhealthy_projects.keys().cloned().collect(),
            unhealthy_project_errors: self.unhealthy_projects.clone(),
            dispatched: self.dispatched,
            recovered: self.recovered,
            epic_throughput: self.throughput_snapshot(),
            pr_errors: self.pr_errors.clone(),
            rate_limited_until: self.current_rate_limited_until(),
        });
    }

    pub(super) fn current_rate_limited_until(&self) -> Option<StdInstant> {
        suppression_remaining(std::time::Instant::now())
            .map(|remaining| StdInstant::now() + remaining)
    }

    pub(super) fn should_skip_background_llm_work(&self, operation: &str) -> bool {
        if let Some(remaining) = suppression_remaining(std::time::Instant::now()) {
            tracing::info!(
                operation,
                remaining_ms = remaining.as_millis(),
                "CoordinatorActor: skipping non-critical background LLM work during provider suppression window"
            );
            return true;
        }
        false
    }

    async fn run_dolt_history_maintenance_tick(&self) {
        let service = DoltHistoryMaintenanceService::new(&self.db);
        if !service.is_dolt_backend() {
            tracing::debug!(
                backend = %self.db.backend_capabilities().backend_label,
                "CoordinatorActor: skipping ADR-055 Dolt history maintenance tick on non-Dolt backend"
            );
            return;
        }

        let current_hour_utc = OffsetDateTime::now_utc().hour();
        let policy = DoltHistoryMaintenancePolicy::default();
        match service.scheduled_report(&policy, current_hour_utc).await {
            Ok(report) => {
                let action = match report.plan.action {
                    DoltHistoryMaintenanceAction::None => "none",
                    DoltHistoryMaintenanceAction::Compact => "compact",
                    DoltHistoryMaintenanceAction::Flatten => "flatten",
                };
                let execution = match report.execution {
                    DoltHistoryMaintenanceExecution::UnsupportedBackend => "unsupported_backend",
                    DoltHistoryMaintenanceExecution::NoActionRequired => "no_action_required",
                    DoltHistoryMaintenanceExecution::BlockedBySafetyChecks => {
                        "blocked_by_safety_checks"
                    }
                    DoltHistoryMaintenanceExecution::PlannedOnly => "planned_only",
                };
                tracing::info!(
                    action,
                    execution,
                    commit_count = report.plan.commit_count,
                    current_hour_utc = report.plan.current_hour_utc,
                    non_main_branch_count = report.plan.non_main_branches.len(),
                    verification_required = report.plan.verification_required,
                    safety_warnings = ?report.plan.safety_warnings,
                    baseline_row_counts = ?report.plan.baseline_row_counts,
                    reason = %report.plan.reason,
                    "CoordinatorActor: ADR-055 Dolt history maintenance plan evaluated"
                );
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "CoordinatorActor: ADR-055 Dolt history maintenance planning failed"
                );
            }
        }
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
            CoordinatorMessage::Pause {
                interrupt_active,
                reason,
            } => {
                // Global pause = pause every known project individually.
                let repo = djinn_db::ProjectRepository::new(
                    self.db.clone(),
                    crate::events::event_bus_for(&self.events_tx),
                );
                if let Ok(projects) = repo.list().await {
                    for p in projects {
                        self.paused_projects.insert(p.id);
                    }
                }
                tracing::info!(
                    count = self.paused_projects.len(),
                    "CoordinatorActor: paused all projects"
                );
                self.publish_status();
                if interrupt_active && let Err(e) = self.pool.interrupt_all(&reason).await {
                    tracing::warn!(error = %e, "CoordinatorActor: failed to interrupt sessions on pause");
                }
            }
            CoordinatorMessage::PauseProject {
                project_id,
                interrupt_active,
                reason,
            } => {
                self.paused_projects.insert(project_id.clone());
                self.publish_status();
                if interrupt_active
                    && let Err(e) = self.pool.interrupt_project(&project_id, &reason).await
                {
                    tracing::warn!(
                        project_id = %project_id,
                        error = %e,
                        "CoordinatorActor: failed to interrupt project sessions on pause"
                    );
                }
            }
            CoordinatorMessage::Resume => {
                // Global resume = unpause every project and dispatch.
                self.paused_projects.clear();
                tracing::info!("CoordinatorActor: resumed all projects");
                self.detect_and_recover_stuck_filtered(None).await;
                self.dispatch_ready_tasks(None).await;
                self.publish_status();
            }
            CoordinatorMessage::ResumeProject { project_id } => {
                self.paused_projects.remove(&project_id);
                self.detect_and_recover_stuck_filtered(Some(&project_id))
                    .await;
                self.dispatch_ready_tasks(Some(&project_id)).await;
                self.publish_status();
            }
            CoordinatorMessage::TriggerStuckScan => {
                self.detect_and_recover_stuck_filtered(None).await;
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
            CoordinatorMessage::ValidateProjectHealth { project_id_filter } => {
                self.validate_all_project_health(project_id_filter).await;
            }
            CoordinatorMessage::SetProjectHealth {
                project_id,
                healthy,
                error,
            } => {
                let was_unhealthy = self.unhealthy_projects.contains_key(&project_id);
                if healthy {
                    self.unhealthy_projects.remove(&project_id);
                    tracing::info!(project_id = %project_id, "CoordinatorActor: project health check passed");
                } else {
                    let err = error.clone().unwrap_or_default();
                    tracing::warn!(
                        project_id = %project_id,
                        error = %err,
                        "CoordinatorActor: project health check failed — skipping dispatch for project"
                    );
                    self.unhealthy_projects.insert(project_id.clone(), err);
                }
                // Emit SSE event on health change.
                let changed = healthy && was_unhealthy || !healthy && !was_unhealthy;
                if changed {
                    let _ = self
                        .events_tx
                        .send(DjinnEventEnvelope::project_health_changed(
                            &project_id,
                            healthy,
                            error.as_deref(),
                        ));
                }
                self.publish_status();
                // If project just became healthy and dispatch is enabled, trigger a dispatch pass.
                if healthy && self.is_project_dispatch_enabled(&project_id) {
                    self.dispatch_ready_tasks(Some(&project_id)).await;
                }
            }
            #[cfg(test)]
            CoordinatorMessage::TriggerPlannerPatrol => {
                self.maybe_dispatch_planner_patrol().await;
            }
            CoordinatorMessage::DispatchPlannerEscalation {
                source_task_id,
                reason,
                project_id,
            } => {
                self.dispatch_planner_escalation(&source_task_id, &reason, &project_id)
                    .await;
            }
            CoordinatorMessage::IncrementEscalationCount { task_id, reply } => {
                let count = self.escalation_counts.entry(task_id).or_insert(0);
                *count += 1;
                let _ = reply.send(*count);
            }
        }
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
            // A task became dispatch-ready for any role → check dispatch.
            // is_project_dispatch_enabled() handles global-pause + per-project
            // resume, so we don't bail early here — a project-resumed project
            // must still react to events even when globally paused.
            // New projects start paused — must be explicitly resumed.
            ("project", "created") => {
                let Some(p) = envelope.parse_payload::<djinn_core::models::Project>() else {
                    return;
                };
                self.paused_projects.insert(p.id.clone());
                tracing::info!(
                    project_id = %p.id,
                    "CoordinatorActor: new project starts paused"
                );
                self.publish_status();
            }
            // Epic created → create a planning task for the Planner (wave 1),
            // but only if the epic is already open (not drafting).
            ("epic", "created") => {
                let Some(epic) = envelope.parse_payload::<djinn_core::models::Epic>() else {
                    return;
                };
                self.maybe_create_planning_task(&epic).await;
            }
            // Epic updated → if the epic is now open, create a planning task.
            // This handles the drafting→open promotion path.
            ("epic", "updated") => {
                let Some(epic) = envelope.parse_payload::<djinn_core::models::Epic>() else {
                    return;
                };
                self.maybe_create_planning_task(&epic).await;
            }
            // ADR-051 §7 — exit recheck.  When a planner session ends, look
            // up the epic its task was attached to and recheck whether an
            // auto-dispatch should fire (now that the guard no longer skips).
            ("session", "completed" | "interrupted" | "failed") => {
                let Some(session) = envelope.parse_payload::<djinn_core::models::SessionRecord>()
                else {
                    return;
                };
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
                    // Record throughput event when a task with a merge commit closes.
                    if task.merge_commit_sha.is_some()
                        && let Some(epic_id) = task.epic_id.as_deref()
                    {
                        self.record_merge_event(epic_id);
                    }
                    // When a patrol review task closes, reset the patrol timer
                    // so the next patrol is scheduled relative to completion.
                    if task.issue_type == "review" && task.title.contains("patrol") {
                        tracing::info!(
                            task_id = %task.short_id,
                            "CoordinatorActor: patrol task closed — resetting patrol timer"
                        );
                        self.last_patrol_completed = StdInstant::now();
                    }
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

    pub(super) fn is_project_dispatch_enabled(&self, project_id: &str) -> bool {
        !self.unhealthy_projects.contains_key(project_id)
            && !self.paused_projects.contains(project_id)
    }

    /// ADR-051 §3 proactive canonical-graph staleness refresh.
    ///
    /// Iterates every dispatch-enabled project and asks the injected
    /// `CanonicalGraphWarmer` to refresh whose cache has fallen behind
    /// `origin/main`.  The warmer is a no-op on cold caches and on warm
    /// caches that are already current, so this method is cheap on a board
    /// where nothing has changed.
    ///
    /// Run from the coordinator tick loop on a 10-minute cadence
    /// (`GRAPH_REFRESH_INTERVAL`).  Serial — the per-project warmer is
    /// already single-flight + detached at the server boundary.
    pub(super) async fn refresh_canonical_graphs_if_stale(&mut self) {
        let Some(warmer) = self.canonical_graph_warmer.clone() else {
            tracing::debug!("CoordinatorActor: graph refresh tick — no warmer injected, skipping");
            return;
        };

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
        for project in projects {
            if !self.is_project_dispatch_enabled(&project.id) {
                continue;
            }
            considered += 1;
            let project_root = std::path::PathBuf::from(&project.path);
            if let Err(e) = warmer
                .maybe_refresh_if_stale(&project.id, &project_root)
                .await
            {
                tracing::warn!(
                    project_id = %project.id,
                    error = %e,
                    "CoordinatorActor: graph refresh tick — maybe_refresh_if_stale reported error (swallowed)"
                );
            }
        }
        tracing::debug!(considered, "CoordinatorActor: graph refresh tick complete");
    }

    /// Resolve dispatch models for a given role from configured priorities,
    /// falling back to credential-backed tool-capable models.
    pub(super) async fn resolve_dispatch_models_for_role(&self, _role: &str) -> Vec<String> {
        #[cfg(test)]
        {
            vec![DEFAULT_MODEL_ID.to_owned()]
        }

        #[cfg(not(test))]
        {
            let cred_repo = djinn_provider::repos::CredentialRepository::new(
                self.db.clone(),
                crate::events::event_bus_for(&self.events_tx),
            );
            let credentials = match cred_repo.list().await {
                Ok(credentials) => credentials,
                Err(_) => return Vec::new(),
            };

            let credential_provider_ids = self.catalog.connected_provider_ids(&credentials);
            if credential_provider_ids.is_empty() {
                return Vec::new();
            }

            let mut selected = Vec::new();
            let mut seen = HashSet::new();

            // Fallback: if "architect" has no configured priorities, use
            // "worker" priorities so architect dispatch works out-of-the-box.
            let effective_priorities = self.model_priorities.get(_role).or_else(|| {
                if _role == "architect" {
                    self.model_priorities.get("worker")
                } else {
                    None
                }
            });

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

            // Return whatever resolved (may be empty if no priorities configured
            // or all configured providers are disconnected). Never fall back to
            // enumerating random credentials — only dispatch what the user configured.
            selected
        }
    }

    pub(super) fn task_repo(&self) -> TaskRepository {
        TaskRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        )
    }

    // ── ADR-051 §7 exit-recheck + stale sweep ────────────────────────────────

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
        repo.get_path(project_id).await.ok().flatten()
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
            Err(_) => return,
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
            Err(_) => return,
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
