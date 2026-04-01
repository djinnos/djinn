// CoordinatorActor — 1x global, orchestrates phase execution and task dispatch.
//
// Ryhl hand-rolled actor pattern (AGENT-01):
//   - `CoordinatorHandle` (mpsc sender) is the public API.
//   - `CoordinatorActor` (mpsc receiver) runs in a dedicated tokio task.
//
// Main loop (AGENT-07): tokio::select! over four arms:
//   1. CancellationToken — graceful shutdown.
//   2. mpsc message channel — API calls from MCP tools.
//   3. broadcast::Receiver<DjinnEventEnvelope> — react to open-task events.
//   4. 30-second Interval tick — stuck detection safety net (AGENT-08).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant as StdInstant};

use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::{self, Interval};
use tokio_util::sync::CancellationToken;

use crate::actors::coordinator::consolidation::{ConsolidationRunner, DbConsolidationRunner};
use crate::actors::slot::{PoolError, SlotPoolHandle};
use crate::roles::RoleRegistry;
use djinn_core::events::DjinnEventEnvelope;
use djinn_core::models::parse_json_array;
use djinn_db::Database;
use djinn_db::GitSettingsRepository;
use djinn_db::NoteRepository;
use djinn_db::ProjectRepository;
use djinn_db::SessionRepository;
use djinn_db::{ActivityQuery, ReadyQuery, TaskRepository};
use djinn_git::GitActorHandle;
use djinn_provider::catalog::CatalogService;
use djinn_provider::catalog::health::HealthTracker;

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
    consolidation_runner: Option<Arc<dyn ConsolidationRunner>>,
}

impl CoordinatorDeps {
    #[allow(clippy::too_many_arguments)]
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
            consolidation_runner: None,
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    fn with_consolidation_runner(mut self, runner: Arc<dyn ConsolidationRunner>) -> Self {
        self.consolidation_runner = Some(runner);
        self
    }
}

mod consolidation;
mod dispatch;
mod health;
pub(crate) mod pr_poller;
mod prompt_eval;
pub(crate) mod rules;
mod wave;

/// Interval between stuck-detection passes (AGENT-08).
const STUCK_INTERVAL: Duration = Duration::from_secs(30);
const STALE_SWEEP_INTERVAL: Duration = Duration::from_secs(15 * 60);
const TASK_OUTCOME_CONFIDENCE_ACTIVITY: &str = "task_outcome_confidence";
const TASK_OUTCOME_CONFIDENCE_SIGNAL: f64 = 0.1;
const TASK_OUTCOME_REOPEN_COUNT: &str = "reopen_count";
const TASK_OUTCOME_FAILED_CLOSE: &str = "failed_closed";

/// Cooldown before re-dispatching a task that failed lifecycle setup
/// (e.g. missing credential).  Prevents hot dispatch loops.
const DISPATCH_COOLDOWN: Duration = Duration::from_secs(60);

/// If a task becomes dispatch-ready again within this threshold of its last
/// dispatch, it is considered a rapid failure and placed in cooldown.
const RAPID_FAILURE_THRESHOLD: Duration = Duration::from_secs(10);
#[cfg(test)]
const DEFAULT_MODEL_ID: &str = "test/mock";

// ─── Error ────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error("actor channel closed")]
    ActorDead,
    #[error("no response from actor")]
    NoResponse,
}

// ─── Public types ─────────────────────────────────────────────────────────────

/// Snapshot of coordinator runtime state (returned by `CoordinatorHandle::get_status`).
#[derive(Debug, Clone)]
pub struct CoordinatorStatus {
    pub paused: bool,
    pub tasks_dispatched: u64,
    pub sessions_recovered: u64,
    /// Per-project health errors (project_id → error message).
    /// Only populated when queried for a specific project.
    pub unhealthy_projects: HashMap<String, String>,
    /// Tasks merged per hour per epic (rolling 1-hour window).
    pub epic_throughput: HashMap<String, usize>,
    /// Per-project PR creation errors (project_id → error message).
    /// Populated when GitHub PR creation fails (e.g. org OAuth restrictions).
    pub pr_errors: HashMap<String, String>,
}

/// Internal snapshot published via `watch` channel so `get_status()` reads
/// never queue behind long-running dispatch passes.
#[derive(Debug, Clone)]
struct SharedCoordinatorState {
    paused_projects: HashSet<String>,
    unhealthy_project_ids: HashSet<String>,
    unhealthy_project_errors: HashMap<String, String>,
    dispatched: u64,
    recovered: u64,
    /// Tasks merged per hour per epic (rolling window snapshot).
    epic_throughput: HashMap<String, usize>,
    /// Per-project PR creation errors (project_id → error message).
    pr_errors: HashMap<String, String>,
}

impl SharedCoordinatorState {
    fn to_status(&self, project_id: Option<&str>) -> CoordinatorStatus {
        let paused = match project_id {
            Some(id) => {
                self.unhealthy_project_ids.contains(id) || self.paused_projects.contains(id)
            }
            None => !self.paused_projects.is_empty(),
        };
        let unhealthy_projects = match project_id {
            Some(id) => self
                .unhealthy_project_errors
                .get(id)
                .map(|err| {
                    let mut m = HashMap::new();
                    m.insert(id.to_string(), err.clone());
                    m
                })
                .unwrap_or_default(),
            None => self.unhealthy_project_errors.clone(),
        };
        let pr_errors = match project_id {
            Some(id) => self
                .pr_errors
                .get(id)
                .map(|err| {
                    let mut m = HashMap::new();
                    m.insert(id.to_string(), err.clone());
                    m
                })
                .unwrap_or_default(),
            None => self.pr_errors.clone(),
        };
        CoordinatorStatus {
            paused,
            tasks_dispatched: self.dispatched,
            sessions_recovered: self.recovered,
            unhealthy_projects,
            epic_throughput: self.epic_throughput.clone(),
            pr_errors,
        }
    }
}

// ─── Messages (≤15 variants — AGENT-11) ──────────────────────────────────────

enum CoordinatorMessage {
    /// Run an immediate dispatch pass for all ready tasks.
    TriggerDispatch,
    /// Run an immediate dispatch pass for a specific project.
    TriggerProjectDispatch { project_id: String },
    /// Pause dispatch — no new sessions will start until `Resume`.
    Pause {
        /// If true, interrupt all active sessions immediately.
        interrupt_active: bool,
        /// Optional interruption reason passed to the slot pool.
        reason: String,
    },
    /// Resume dispatch and immediately run a dispatch pass.
    Resume,
    /// Resume dispatch for one project.
    ResumeProject { project_id: String },
    /// Pause dispatch for one project, optionally interrupting active sessions.
    PauseProject {
        project_id: String,
        interrupt_active: bool,
        reason: String,
    },
    /// Update runtime dispatch limit from settings reload.
    UpdateDispatchLimit { limit: usize },
    /// Update per-role model priority list from settings reload.
    UpdateModelPriorities {
        priorities: HashMap<String, Vec<String>>,
    },
    /// Run an immediate stuck-task detection pass.
    TriggerStuckScan,
    /// Trigger background health validation for all (or one) project on execution_start.
    ValidateProjectHealth { project_id_filter: Option<String> },
    /// Internal callback: result from a background project health-check task.
    SetProjectHealth {
        project_id: String,
        healthy: bool,
        error: Option<String>,
    },
    /// Trigger an immediate Architect patrol dispatch (for testing).
    #[cfg(test)]
    TriggerArchitectPatrol,
    /// Lead requests Architect escalation for a task.
    /// Creates a review task and dispatches Architect to it.
    DispatchArchitectEscalation {
        source_task_id: String,
        reason: String,
        project_id: String,
    },
    /// Increment the Lead escalation count for a task; reply with new count.
    IncrementEscalationCount {
        task_id: String,
        reply: tokio::sync::oneshot::Sender<u32>,
    },
}

// ─── Actor (≤20 fields — AGENT-11) ───────────────────────────────────────────

struct CoordinatorActor {
    // Ryhl core
    receiver: mpsc::Receiver<CoordinatorMessage>,
    events: broadcast::Receiver<DjinnEventEnvelope>,
    cancel: CancellationToken,
    tick: Interval,
    // Dependencies
    db: Database,
    events_tx: broadcast::Sender<DjinnEventEnvelope>,
    pool: SlotPoolHandle,
    #[cfg_attr(test, allow(dead_code))]
    catalog: CatalogService,
    health: HealthTracker,
    role_registry: Arc<RoleRegistry>,
    lsp: crate::lsp::LspManager,
    // Sender clone for background tasks to send results back.
    self_sender: mpsc::Sender<CoordinatorMessage>,
    // Watch channel for lock-free status reads.
    status_tx: watch::Sender<SharedCoordinatorState>,
    // State
    paused_projects: HashSet<String>,
    dispatch_limit: usize,
    model_priorities: HashMap<String, Vec<String>>,
    // Per-project health: project_id → error message (only unhealthy projects appear here).
    unhealthy_projects: HashMap<String, String>,
    /// Per-project PR creation errors (project_id → error message).
    pr_errors: HashMap<String, String>,
    /// Per-task dispatch tracking: task UUID → last dispatch instant.
    /// When a task becomes ready again within `RAPID_FAILURE_THRESHOLD` of its
    /// last dispatch, it is placed in cooldown for `DISPATCH_COOLDOWN` to prevent
    /// hot dispatch loops (e.g. missing credential → release → re-dispatch).
    last_dispatched: HashMap<String, StdInstant>,
    dispatch_cooldowns: HashMap<String, StdInstant>,
    /// Shared tracker for in-flight verification background tasks.
    verification_tracker: VerificationTracker,
    consolidation_runner: Arc<dyn ConsolidationRunner>,
    last_stale_sweep: StdInstant,
    /// Tick counter for association pruning (runs once per ~120 ticks ≈ 1 hour)
    prune_tick_counter: u32,
    /// Timestamp of the last patrol completion (or actor start as initial baseline).
    /// The next patrol is eligible only after `next_patrol_interval` has elapsed
    /// since this instant.  Reset when a patrol task reaches a terminal state.
    last_patrol_completed: StdInstant,
    /// Dynamic patrol interval, set by the architect's self-scheduling.
    /// Defaults to `rules::DEFAULT_ARCHITECT_PATROL_INTERVAL`.
    /// Updated when the coordinator reads a `patrol_schedule` activity entry.
    next_patrol_interval: Duration,
    /// Rolling-window throughput tracking: epic_id → Vec of merge event instants.
    throughput_events: HashMap<String, Vec<StdInstant>>,
    /// Per-task Lead escalation count (request_lead call count per task UUID).
    /// When a task accumulates ≥ 2 escalations, the next request_lead routes to Architect.
    escalation_counts: HashMap<String, u32>,
    /// PR status cache: task_id → last known head SHA.
    ///
    /// Used by the PR poller to skip redundant CI check-run queries when the
    /// PR's head commit has not changed since the previous poll cycle.
    pr_status_cache: HashMap<String, String>,
    /// Tracks when each task was first seen in `pr_draft` status.
    ///
    /// Used by the PR poller to enforce a minimum age before checking CI,
    /// preventing a race where GitHub hasn't registered workflow check-runs
    /// yet and the poller incorrectly concludes CI has passed.
    pr_draft_first_seen: HashMap<String, StdInstant>,
    /// Consecutive merge failure count per task.  After
    /// `MERGE_RETRY_RECHECK_THRESHOLD` failures, the poller invalidates
    /// the CI SHA cache so it re-checks whether CI actually passed.
    merge_fail_count: HashMap<String, u32>,
    /// Task IDs for which a stall-kill has already been issued.  Prevents
    /// repeated kill + activity-log spam while the async lifecycle cleanup
    /// is still in progress (the DB session record stays `running` until
    /// the lifecycle finishes).  Entries are removed when the session
    /// disappears from `list_active()`.
    stall_killed: HashSet<String>,
    // Metrics
    dispatched: u64,
    recovered: u64,
}

// Field count: receiver, events, cancel, tick, db, events_tx, pool,
//              catalog, health, paused, dispatched, recovered = 12 ✓ (≤20)

impl CoordinatorActor {
    fn new(
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
            prune_tick_counter: 0,
            last_patrol_completed: StdInstant::now(),
            next_patrol_interval: rules::DEFAULT_ARCHITECT_PATROL_INTERVAL,
            throughput_events: HashMap::new(),
            escalation_counts: HashMap::new(),
            pr_status_cache: HashMap::new(),
            pr_draft_first_seen: HashMap::new(),
            merge_fail_count: HashMap::new(),
            stall_killed: HashSet::new(),
            dispatched: 0,
            recovered: 0,
        }
    }

    async fn run(mut self) {
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
                        };
                        health::sweep_stale_resources(&self.db, &app_state).await;
                        self.last_stale_sweep = StdInstant::now();
                    }
                    // Run association pruning once per ~hour (120 ticks at 30s intervals)
                    self.prune_tick_counter += 1;
                    if self.prune_tick_counter >= 120 {
                        self.prune_tick_counter = 0;
                        self.prune_note_associations().await;
                        consolidation::run_note_consolidation(&self.db, &self.consolidation_runner).await;
                        self.evict_throughput_events();
                        self.evaluate_prompt_amendments().await;
                    }
                    // Architect patrol: completion-time-based scheduling.
                    // Only attempt dispatch once `next_patrol_interval` has
                    // elapsed since the last patrol completed (or actor start).
                    if self.last_patrol_completed.elapsed() >= self.next_patrol_interval {
                        self.maybe_dispatch_architect_patrol().await;
                    }
                }
            }
        }
        tracing::info!("CoordinatorActor stopped");
    }

    /// Publish current state to the watch channel for lock-free status reads.
    fn publish_status(&self) {
        let _ = self.status_tx.send(SharedCoordinatorState {
            paused_projects: self.paused_projects.clone(),
            unhealthy_project_ids: self.unhealthy_projects.keys().cloned().collect(),
            unhealthy_project_errors: self.unhealthy_projects.clone(),
            dispatched: self.dispatched,
            recovered: self.recovered,
            epic_throughput: self.throughput_snapshot(),
            pr_errors: self.pr_errors.clone(),
        });
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
            CoordinatorMessage::TriggerArchitectPatrol => {
                self.maybe_dispatch_architect_patrol().await;
            }
            CoordinatorMessage::DispatchArchitectEscalation {
                source_task_id,
                reason,
                project_id,
            } => {
                self.dispatch_architect_escalation(&source_task_id, &reason, &project_id)
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

    async fn handle_event(&mut self, envelope: DjinnEventEnvelope) {
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
                // Batch-completion check: when a *worker* task closes, check
                // if all non-planning worker tasks under the epic are closed
                // (AC5/AC6 — wave-based planning).
                // Planning/decomposition/review/spike task closures must NOT
                // trigger this — otherwise a planner task closing re-triggers
                // wave creation even when the epic should be done.
                if task.status == "closed"
                    && wave::is_worker_issue_type(&task.issue_type)
                    && let Some(ref epic_id) = task.epic_id
                {
                    self.maybe_create_next_wave_planning(epic_id, &task.project_id)
                        .await;
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

    fn is_project_dispatch_enabled(&self, project_id: &str) -> bool {
        !self.unhealthy_projects.contains_key(project_id)
            && !self.paused_projects.contains(project_id)
    }

    /// Resolve dispatch models for a given role from configured priorities,
    /// falling back to credential-backed tool-capable models.
    async fn resolve_dispatch_models_for_role(&self, _role: &str) -> Vec<String> {
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

    fn task_repo(&self) -> TaskRepository {
        TaskRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        )
    }

    async fn project_path_for_id(&self, project_id: &str) -> Option<String> {
        let repo = ProjectRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        repo.get_path(project_id).await.ok().flatten()
    }
}

// ─── Handle ───────────────────────────────────────────────────────────────────

/// Cheap-to-clone handle to the global `CoordinatorActor`.
#[derive(Clone)]
pub struct CoordinatorHandle {
    sender: mpsc::Sender<CoordinatorMessage>,
    status_rx: watch::Receiver<SharedCoordinatorState>,
}

impl CoordinatorHandle {
    /// Spawn the `CoordinatorActor` and return a handle to it.
    pub fn spawn(deps: CoordinatorDeps) -> Self {
        let (sender, receiver) = mpsc::channel(32);
        let initial_state = SharedCoordinatorState {
            paused_projects: HashSet::new(),
            unhealthy_project_ids: HashSet::new(),
            unhealthy_project_errors: HashMap::new(),
            dispatched: 0,
            recovered: 0,
            epic_throughput: HashMap::new(),
            pr_errors: HashMap::new(),
        };
        let (status_tx, status_rx) = watch::channel(initial_state);
        let deps = CoordinatorDeps {
            consolidation_runner: Some(
                deps.consolidation_runner
                    .unwrap_or_else(|| Arc::new(DbConsolidationRunner::new(deps.db.clone()))),
            ),
            ..deps
        };
        let actor = CoordinatorActor::new(deps, receiver, sender.clone(), status_tx);
        tokio::spawn(actor.run());
        Self { sender, status_rx }
    }

    async fn send(&self, msg: CoordinatorMessage) -> Result<(), CoordinatorError> {
        self.sender
            .send(msg)
            .await
            .map_err(|_| CoordinatorError::ActorDead)
    }

    /// Trigger an immediate dispatch pass for all ready tasks.
    pub async fn trigger_dispatch(&self) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::TriggerDispatch).await
    }

    /// Best-effort dispatch trigger that never blocks.
    ///
    /// Used by the pool actor's slot-completion handler to avoid a deadlock:
    /// the pool must not `.await` on the coordinator channel while the
    /// coordinator may be `.await`-ing on the pool (e.g. `has_session`).
    pub fn try_trigger_dispatch(&self) {
        let _ = self.sender.try_send(CoordinatorMessage::TriggerDispatch);
    }

    pub async fn trigger_dispatch_for_project(
        &self,
        project_id: &str,
    ) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::TriggerProjectDispatch {
            project_id: project_id.to_owned(),
        })
        .await
    }

    /// Pause dispatch (no new sessions will start).
    pub async fn pause(&self) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::Pause {
            interrupt_active: false,
            reason: "session interrupted by coordinator pause".to_string(),
        })
        .await
    }

    pub async fn pause_project(&self, project_id: &str) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::PauseProject {
            project_id: project_id.to_owned(),
            interrupt_active: false,
            reason: String::new(),
        })
        .await
    }

    pub async fn pause_project_immediate(
        &self,
        project_id: &str,
        reason: &str,
    ) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::PauseProject {
            project_id: project_id.to_owned(),
            interrupt_active: true,
            reason: reason.to_owned(),
        })
        .await
    }

    /// Pause dispatch and interrupt active sessions immediately.
    pub async fn pause_immediate(&self, reason: &str) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::Pause {
            interrupt_active: true,
            reason: reason.to_owned(),
        })
        .await
    }

    /// Resume dispatch and immediately run a dispatch pass.
    pub async fn resume(&self) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::Resume).await
    }

    pub async fn resume_project(&self, project_id: &str) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::ResumeProject {
            project_id: project_id.to_owned(),
        })
        .await
    }

    /// Return the current coordinator status snapshot (lock-free read via watch channel).
    pub fn get_status(&self) -> Result<CoordinatorStatus, CoordinatorError> {
        Ok(self.status_rx.borrow().to_status(None))
    }

    pub fn get_project_status(
        &self,
        project_id: &str,
    ) -> Result<CoordinatorStatus, CoordinatorError> {
        Ok(self.status_rx.borrow().to_status(Some(project_id)))
    }

    /// Trigger an immediate stuck-task detection pass.
    pub async fn trigger_stuck_scan(&self) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::TriggerStuckScan).await
    }

    /// Update ready-task dispatch limit.
    pub async fn update_dispatch_limit(&self, limit: usize) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::UpdateDispatchLimit {
            limit: limit.max(1),
        })
        .await
    }

    /// Update per-role model priority lists.
    pub async fn update_model_priorities(
        &self,
        priorities: HashMap<String, Vec<String>>,
    ) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::UpdateModelPriorities { priorities })
            .await
    }

    /// Trigger background project health validation on execution_start (ADR-014).
    /// Scoped to `project_id_filter` if provided, otherwise validates all projects.
    pub async fn validate_project_health(
        &self,
        project_id_filter: Option<String>,
    ) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::ValidateProjectHealth { project_id_filter })
            .await
    }
    /// Wait until the coordinator status satisfies the given predicate.
    /// For use in tests where we need to observe the effect of a sent message.
    #[cfg(test)]
    pub async fn wait_for_status<F>(&self, predicate: F)
    where
        F: Fn(&CoordinatorStatus) -> bool,
    {
        let mut rx = self.status_rx.clone();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if predicate(&rx.borrow().to_status(None)) {
                return;
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => continue,
                Ok(Err(_)) => panic!("watch channel closed"),
                Err(_) => panic!("timed out waiting for coordinator status condition"),
            }
        }
    }

    /// Like `wait_for_status` but evaluates the predicate against project-scoped status.
    #[cfg(test)]
    pub async fn wait_for_project_status<F>(&self, project_id: &str, predicate: F)
    where
        F: Fn(&CoordinatorStatus) -> bool,
    {
        let project_id = project_id.to_owned();
        let mut rx = self.status_rx.clone();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if predicate(&rx.borrow().to_status(Some(&project_id))) {
                return;
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => continue,
                Ok(Err(_)) => panic!("watch channel closed"),
                Err(_) => panic!("timed out waiting for coordinator project status condition"),
            }
        }
    }

    /// Trigger an immediate Architect patrol dispatch (for testing).
    #[cfg(test)]
    pub async fn trigger_architect_patrol(&self) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::TriggerArchitectPatrol).await
    }

    /// Dispatch an Architect escalation for a task.
    ///
    /// Creates a review task and dispatches the Architect to it.
    /// Called when Lead uses `request_architect` or auto-escalation fires on 2nd `request_lead`.
    pub async fn dispatch_architect_escalation(
        &self,
        source_task_id: &str,
        reason: &str,
        project_id: &str,
    ) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::DispatchArchitectEscalation {
            source_task_id: source_task_id.to_owned(),
            reason: reason.to_owned(),
            project_id: project_id.to_owned(),
        })
        .await
    }

    /// Increment the Lead escalation count for a task and return the new count.
    ///
    /// When the count reaches ≥ 2, the caller should route to Architect instead of Lead.
    pub async fn increment_escalation_count(&self, task_id: &str) -> Result<u32, CoordinatorError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(CoordinatorMessage::IncrementEscalationCount {
                task_id: task_id.to_owned(),
                reply: tx,
            })
            .await
            .map_err(|_| CoordinatorError::ActorDead)?;
        rx.await.map_err(|_| CoordinatorError::NoResponse)
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::Mutex;

    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::actors::slot::{ModelSlotConfig, SlotPoolConfig, SlotPoolHandle};
    use crate::roles::RoleRegistry;
    use crate::test_helpers;
    use djinn_core::models::TransitionAction;
    use djinn_db::EpicRepository;
    use djinn_db::NoteConsolidationRepository;
    use djinn_db::NoteRepository;
    use djinn_db::TaskRepository;
    use djinn_db::{CreateSessionParams, SessionRepository};
    use djinn_provider::catalog::health::HealthTracker;

    use super::consolidation::{self, ConsolidationRunner, DbConsolidationRunner};

    struct RecordingConsolidationRunner {
        calls: Arc<Mutex<Vec<djinn_db::DbNoteGroup>>>,
        session_calls: Arc<Mutex<Vec<(djinn_db::DbNoteGroup, String)>>>,
    }

    impl RecordingConsolidationRunner {
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                session_calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        #[allow(dead_code)]
        fn groups(&self) -> Vec<djinn_db::DbNoteGroup> {
            self.calls.lock().unwrap().clone()
        }

        fn session_groups(&self) -> Vec<(djinn_db::DbNoteGroup, String)> {
            self.session_calls.lock().unwrap().clone()
        }
    }

    impl ConsolidationRunner for RecordingConsolidationRunner {
        fn run_for_group<'a>(
            &'a self,
            group: djinn_db::DbNoteGroup,
        ) -> Pin<Box<dyn Future<Output = djinn_db::Result<()>> + Send + 'a>> {
            Box::pin(async move {
                self.calls.lock().unwrap().push(group);
                Ok(())
            })
        }

        fn run_for_group_in_session<'a>(
            &'a self,
            group: djinn_db::DbNoteGroup,
            session_id: String,
        ) -> Pin<Box<dyn Future<Output = djinn_db::Result<()>> + Send + 'a>> {
            Box::pin(async move {
                self.session_calls.lock().unwrap().push((group, session_id));
                Ok(())
            })
        }
    }

    fn spawn_coordinator(
        db: &Database,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
    ) -> CoordinatorHandle {
        let cancel = CancellationToken::new();
        let ctx = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
        let sessions_dir = std::env::temp_dir().join(format!(
            "djinn-test-sessions-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = sessions_dir;
        let pool = SlotPoolHandle::spawn(
            ctx,
            cancel.clone(),
            SlotPoolConfig {
                models: vec![ModelSlotConfig {
                    model_id: DEFAULT_MODEL_ID.to_owned(),
                    max_slots: 2,
                    roles: ["worker", "reviewer"]
                        .into_iter()
                        .map(ToOwned::to_owned)
                        .collect(),
                }],
                role_priorities: HashMap::new(),
            },
        );
        let catalog = CatalogService::new();
        let health = HealthTracker::new();
        let verification_tracker = VerificationTracker::default();
        let role_registry = Arc::new(RoleRegistry::new());
        CoordinatorHandle::spawn(CoordinatorDeps::new(
            tx.clone(),
            cancel,
            db.clone(),
            pool,
            catalog,
            health,
            role_registry,
            verification_tracker,
            crate::lsp::LspManager::new(),
        ))
    }

    async fn make_epic(
        db: &Database,
        tx: broadcast::Sender<DjinnEventEnvelope>,
    ) -> djinn_core::models::Epic {
        EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .create("Epic", "", "", "", "", None)
            .await
            .unwrap()
    }

    async fn create_task_with_note(
        db: &Database,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
        title: &str,
    ) -> (djinn_core::models::Task, djinn_core::models::Note) {
        let project = test_helpers::create_test_project(db).await;
        std::fs::create_dir_all(Path::new(&project.path)).unwrap();
        let epic = EpicRepository::new(db.clone(), crate::events::event_bus_for(tx))
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: None,
                },
            )
            .await
            .unwrap();
        let note_repo = NoteRepository::new(db.clone(), crate::events::event_bus_for(tx));
        let note = note_repo
            .create(
                &project.id,
                Path::new(&project.path),
                title,
                "body",
                "research",
                "[]",
            )
            .await
            .unwrap();
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(tx));
        let task = task_repo
            .create(&epic.id, title, "", "", "task", 0, "", Some("open"))
            .await
            .unwrap();
        let memory_refs = serde_json::to_string(&vec![note.permalink.clone()]).unwrap();
        let task = task_repo
            .update_memory_refs(&task.id, &memory_refs)
            .await
            .unwrap();
        sqlx::query("UPDATE notes SET confidence = 0.5 WHERE id = ?1")
            .bind(&note.id)
            .execute(db.pool())
            .await
            .unwrap();
        (task, note)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hourly_background_tick_invokes_consolidation_runner_for_db_note_group() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let consolidation_repo = NoteConsolidationRepository::new(db.clone());
        let note_a = note_repo
            .create_db_note(
                &project.id,
                "Retry Storm A",
                "Retry storm causes duplicate work during incident recovery.",
                "case",
                "[]",
            )
            .await
            .unwrap();
        let note_b = note_repo
            .create_db_note(
                &project.id,
                "Retry Storm B",
                "Retry storm causes duplicate work during incident recovery.",
                "case",
                "[]",
            )
            .await
            .unwrap();

        // Link both notes to the same session so session-scoped consolidation
        // discovers them.
        let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let session = session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: None,
                model: "test-model",
                agent_type: "worker",
                worktree_path: None,
                metadata_json: None,
            })
            .await
            .unwrap();
        consolidation_repo
            .add_provenance(&note_a.id, &session.id)
            .await
            .unwrap();
        consolidation_repo
            .add_provenance(&note_b.id, &session.id)
            .await
            .unwrap();

        let runner = Arc::new(RecordingConsolidationRunner::new());
        let actor = CoordinatorActor {
            receiver: tokio::sync::mpsc::channel(1).1,
            events: tx.subscribe(),
            cancel: CancellationToken::new(),
            tick: tokio::time::interval(STUCK_INTERVAL),
            db: db.clone(),
            events_tx: tx.clone(),
            pool: SlotPoolHandle::spawn(
                test_helpers::agent_context_from_db(db.clone(), CancellationToken::new()),
                CancellationToken::new(),
                SlotPoolConfig {
                    models: vec![ModelSlotConfig {
                        model_id: DEFAULT_MODEL_ID.to_owned(),
                        max_slots: 1,
                        roles: ["worker"].into_iter().map(ToOwned::to_owned).collect(),
                    }],
                    role_priorities: HashMap::new(),
                },
            ),
            catalog: CatalogService::new(),
            health: HealthTracker::new(),
            role_registry: Arc::new(RoleRegistry::new()),
            self_sender: tokio::sync::mpsc::channel(1).0,
            status_tx: tokio::sync::watch::channel(SharedCoordinatorState {
                paused_projects: HashSet::new(),
                unhealthy_project_ids: HashSet::new(),
                unhealthy_project_errors: HashMap::new(),
                dispatched: 0,
                recovered: 0,
                epic_throughput: HashMap::new(),
                pr_errors: HashMap::new(),
            })
            .0,
            paused_projects: HashSet::new(),
            dispatch_limit: 50,
            model_priorities: HashMap::new(),
            unhealthy_projects: HashMap::new(),
            pr_errors: HashMap::new(),
            last_dispatched: HashMap::new(),
            dispatch_cooldowns: HashMap::new(),
            verification_tracker: VerificationTracker::default(),
            consolidation_runner: runner.clone(),
            last_stale_sweep: StdInstant::now(),
            prune_tick_counter: 0,
            last_patrol_completed: StdInstant::now(),
            next_patrol_interval: rules::DEFAULT_ARCHITECT_PATROL_INTERVAL,
            throughput_events: HashMap::new(),
            escalation_counts: HashMap::new(),
            pr_status_cache: HashMap::new(),
            pr_draft_first_seen: HashMap::new(),
            merge_fail_count: HashMap::new(),
            stall_killed: HashSet::new(),
            dispatched: 0,
            recovered: 0,
        };
        consolidation::run_note_consolidation(&actor.db, &actor.consolidation_runner).await;

        let session_groups = runner.session_groups();
        assert_eq!(session_groups.len(), 1);
        assert_eq!(session_groups[0].0.project_id, project.id);
        assert_eq!(session_groups[0].0.note_type, "case");
        assert_eq!(session_groups[0].1, session.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn below_threshold_clusters_are_noop_for_consolidation_runner() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let consolidation_repo = NoteConsolidationRepository::new(db.clone());
        note_repo
            .create_db_note(
                &project.id,
                "Incident Pattern A",
                "Repeated timeout while syncing cache data.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        note_repo
            .create_db_note(
                &project.id,
                "Incident Pattern B",
                "Repeated timeout while syncing cache data.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        let metrics_before = consolidation_repo
            .list_run_metrics(&project.id, Some("pattern"), 20)
            .await
            .unwrap();
        assert!(metrics_before.is_empty());

        let runner = Arc::new(DbConsolidationRunner::new(db.clone()));
        runner
            .run_for_group(djinn_db::DbNoteGroup {
                project_id: project.id.clone(),
                note_type: "pattern".to_string(),
                note_count: 2,
            })
            .await
            .unwrap();

        let metrics_after = consolidation_repo
            .list_run_metrics(&project.id, Some("pattern"), 20)
            .await
            .unwrap();
        assert!(
            metrics_after.is_empty(),
            "below-threshold groups should remain a no-op with no run bookkeeping"
        );

        let notes = consolidation_repo
            .list_db_notes_in_group(&project.id, "pattern")
            .await
            .unwrap();
        assert_eq!(notes.len(), 2, "runner should not synthesize new notes");

        for note in &notes {
            let provenance = consolidation_repo.list_provenance(&note.id).await.unwrap();
            assert!(
                provenance.is_empty(),
                "below-threshold groups should not persist provenance for note {}",
                note.id
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn qualifying_clusters_create_canonical_note_provenance_and_completed_metric() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let consolidation_repo = NoteConsolidationRepository::new(db.clone());

        let note_a = note_repo
            .create_db_note(
                &project.id,
                "Retry Storm A",
                "Repeated retry storm during incident recovery.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        let note_b = note_repo
            .create_db_note(
                &project.id,
                "Retry Storm B",
                "Repeated retry storm during incident recovery.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        let note_c = note_repo
            .create_db_note(
                &project.id,
                "Retry Storm C",
                "Repeated retry storm during incident recovery.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        sqlx::query("UPDATE notes SET abstract = ?1, overview = ?2 WHERE id = ?3")
            .bind("Retry storms amplify duplicate work during recovery.")
            .bind("Prefer backoff and idempotent recovery steps.")
            .bind(&note_a.id)
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE notes SET abstract = ?1, overview = ?2 WHERE id = ?3")
            .bind("Retry storms amplify duplicate work during recovery.")
            .bind("Throttle retries before cache warmup completes.")
            .bind(&note_b.id)
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE notes SET abstract = ?1, overview = ?2 WHERE id = ?3")
            .bind("Retry storms amplify duplicate work during recovery.")
            .bind("Use idempotent jobs plus exponential backoff.")
            .bind(&note_c.id)
            .execute(db.pool())
            .await
            .unwrap();

        let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let session_a = session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: None,
                model: "test-model",
                agent_type: "worker",
                worktree_path: None,
                metadata_json: None,
            })
            .await
            .unwrap();
        let session_b = session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: None,
                model: "test-model",
                agent_type: "worker",
                worktree_path: None,
                metadata_json: None,
            })
            .await
            .unwrap();
        let session_c = session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: None,
                model: "test-model",
                agent_type: "worker",
                worktree_path: None,
                metadata_json: None,
            })
            .await
            .unwrap();
        consolidation_repo
            .add_provenance(&note_a.id, &session_a.id)
            .await
            .unwrap();
        consolidation_repo
            .add_provenance(&note_b.id, &session_b.id)
            .await
            .unwrap();
        consolidation_repo
            .add_provenance(&note_c.id, &session_c.id)
            .await
            .unwrap();

        let runner = Arc::new(DbConsolidationRunner::new(db.clone()));
        runner
            .run_for_group(djinn_db::DbNoteGroup {
                project_id: project.id.clone(),
                note_type: "pattern".to_string(),
                note_count: 3,
            })
            .await
            .unwrap();

        let notes = consolidation_repo
            .list_db_notes_in_group(&project.id, "pattern")
            .await
            .unwrap();
        assert_eq!(
            notes.len(),
            4,
            "runner should synthesize exactly one canonical note"
        );
        let canonical = notes
            .iter()
            .find(|note| note.id != note_a.id && note.id != note_b.id && note.id != note_c.id)
            .unwrap();
        assert!(
            canonical
                .title
                .starts_with("Canonical pattern: Retry Storm")
        );
        assert!(canonical.content.contains("## Source notes"));
        assert!(canonical.content.contains(&note_a.permalink));
        assert_eq!(
            canonical.abstract_.as_deref(),
            Some("Retry storms amplify duplicate work during recovery.")
        );
        assert!(canonical.confidence >= 0.65 && canonical.confidence <= 0.8);

        let provenance = consolidation_repo
            .list_provenance(&canonical.id)
            .await
            .unwrap();
        assert_eq!(
            provenance
                .iter()
                .map(|entry| entry.session_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                session_a.id.as_str(),
                session_b.id.as_str(),
                session_c.id.as_str()
            ]
        );

        let metrics = consolidation_repo
            .list_run_metrics(&project.id, Some("pattern"), 20)
            .await
            .unwrap();
        assert_eq!(metrics.len(), 1);
        let metric = &metrics[0];
        assert_eq!(metric.status, "completed");
        assert_eq!(metric.scanned_note_count, 3);
        assert_eq!(metric.candidate_cluster_count, 1);
        assert_eq!(metric.consolidated_cluster_count, 1);
        assert_eq!(metric.consolidated_note_count, 1);
        assert_eq!(metric.source_note_count, 3);
        assert!(metric.completed_at.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_scoped_consolidation_excludes_cross_session_notes_and_preserves_metrics() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let consolidation_repo = NoteConsolidationRepository::new(db.clone());

        let session_note_a = note_repo
            .create_db_note(
                &project.id,
                "Retry Cluster A",
                "Repeated retry storm during incident recovery.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        let session_note_b = note_repo
            .create_db_note(
                &project.id,
                "Retry Cluster B",
                "Repeated retry storm during incident recovery.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        let session_note_c = note_repo
            .create_db_note(
                &project.id,
                "Retry Cluster C",
                "Repeated retry storm during incident recovery.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        let cross_session_note = note_repo
            .create_db_note(
                &project.id,
                "Retry Cluster D",
                "Repeated retry storm during incident recovery.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        for (note_id, overview) in [
            (
                &session_note_a.id,
                "Prefer backoff and idempotent recovery steps.",
            ),
            (
                &session_note_b.id,
                "Throttle retries before cache warmup completes.",
            ),
            (
                &session_note_c.id,
                "Use idempotent jobs plus exponential backoff.",
            ),
            (
                &cross_session_note.id,
                "A later session found the same retry pattern independently.",
            ),
        ] {
            sqlx::query("UPDATE notes SET abstract = ?1, overview = ?2 WHERE id = ?3")
                .bind("Retry storms amplify duplicate work during recovery.")
                .bind(overview)
                .bind(note_id)
                .execute(db.pool())
                .await
                .unwrap();
        }

        let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let source_session = session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: None,
                model: "test-model",
                agent_type: "worker",
                worktree_path: None,
                metadata_json: None,
            })
            .await
            .unwrap();
        let later_session = session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: None,
                model: "test-model",
                agent_type: "worker",
                worktree_path: None,
                metadata_json: None,
            })
            .await
            .unwrap();

        for note_id in [&session_note_a.id, &session_note_b.id, &session_note_c.id] {
            consolidation_repo
                .add_provenance(note_id, &source_session.id)
                .await
                .unwrap();
        }
        consolidation_repo
            .add_provenance(&cross_session_note.id, &later_session.id)
            .await
            .unwrap();

        let runner = Arc::new(DbConsolidationRunner::new(db.clone()));
        runner
            .run_for_group_in_session(
                djinn_db::DbNoteGroup {
                    project_id: project.id.clone(),
                    note_type: "pattern".to_string(),
                    note_count: 3,
                },
                source_session.id.clone(),
            )
            .await
            .unwrap();

        let notes = consolidation_repo
            .list_db_notes_in_group(&project.id, "pattern")
            .await
            .unwrap();
        assert_eq!(
            notes.len(),
            5,
            "session-scoped run should create one canonical note"
        );
        let canonical = notes
            .iter()
            .find(|note| {
                ![
                    &session_note_a.id,
                    &session_note_b.id,
                    &session_note_c.id,
                    &cross_session_note.id,
                ]
                .contains(&&note.id)
            })
            .unwrap();
        assert!(canonical.content.contains(&session_note_a.permalink));
        assert!(canonical.content.contains(&session_note_b.permalink));
        assert!(canonical.content.contains(&session_note_c.permalink));
        assert!(
            !canonical.content.contains(&cross_session_note.permalink),
            "canonical content must exclude unrelated cross-session note"
        );

        let provenance = consolidation_repo
            .list_provenance(&canonical.id)
            .await
            .unwrap();
        assert_eq!(
            provenance
                .iter()
                .map(|entry| entry.session_id.as_str())
                .collect::<Vec<_>>(),
            vec![source_session.id.as_str()],
            "session-scoped canonical note should inherit only same-session provenance"
        );

        let metrics = consolidation_repo
            .list_run_metrics(&project.id, Some("pattern"), 20)
            .await
            .unwrap();
        assert_eq!(metrics.len(), 1);
        let metric = &metrics[0];
        assert_eq!(metric.status, "completed");
        assert_eq!(metric.scanned_note_count, 3);
        assert_eq!(metric.candidate_cluster_count, 1);
        assert_eq!(metric.consolidated_cluster_count, 1);
        assert_eq!(metric.consolidated_note_count, 1);
        assert_eq!(metric.source_note_count, 3);
        assert!(metric.completed_at.is_some());
    }

    // ── Status ───────────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn initial_status_is_active() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let handle = spawn_coordinator(&db, &tx);

        let status = handle.get_status().unwrap();
        assert!(
            !status.paused,
            "coordinator should start active (no global pause state)"
        );
        assert_eq!(status.tasks_dispatched, 0);
        assert_eq!(status.sessions_recovered, 0);
    }

    // ── Pause / Resume ───────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pause_project_and_resume_toggle_state() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let handle = spawn_coordinator(&db, &tx);

        let project_id = "test-project-id";

        // Pausing a project marks it paused in project-scoped status.
        handle.pause_project(project_id).await.unwrap();
        handle
            .wait_for_project_status(project_id, |s| s.paused)
            .await;
        assert!(handle.get_project_status(project_id).unwrap().paused);

        // Resuming removes it from the paused set.
        handle.resume_project(project_id).await.unwrap();
        handle
            .wait_for_project_status(project_id, |s| !s.paused)
            .await;
        assert!(!handle.get_project_status(project_id).unwrap().paused);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trigger_dispatch_while_project_paused_is_a_noop() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        repo.create(&epic.id, "T1", "", "", "task", 0, "", Some("open"))
            .await
            .unwrap();

        let handle = spawn_coordinator(&db, &tx);
        handle.pause_project(&epic.project_id).await.unwrap();
        handle
            .trigger_dispatch_for_project(&epic.project_id)
            .await
            .unwrap();
        // Give the actor a moment to process; dispatched count stays 0.
        tokio::task::yield_now().await;
        assert_eq!(handle.get_status().unwrap().tasks_dispatched, 0);
    }

    // ── Dispatch on open-task event ──────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trigger_dispatch_increments_counter_for_ready_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        // Create a ready task (open, no blockers).
        repo.create(&epic.id, "T1", "", "", "task", 0, "", Some("open"))
            .await
            .unwrap();

        let handle = spawn_coordinator(&db, &tx);
        handle.trigger_dispatch().await.unwrap();
        handle.wait_for_status(|s| s.tasks_dispatched >= 1).await;

        let status = handle.get_status().unwrap();
        assert!(
            status.tasks_dispatched >= 1,
            "should have dispatched the ready task"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trigger_dispatch_increments_counter_for_review_tasks() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        let task = repo
            .create(&epic.id, "Review me", "", "", "task", 0, "", Some("open"))
            .await
            .unwrap();
        repo.update(
            &task.id,
            "Review me",
            "",
            "",
            0,
            "",
            "",
            r#"[{"description":"default","met":false}]"#,
        )
        .await
        .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::Start,
            "test",
            "system",
            None,
            None,
        )
        .await
        .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::SubmitTaskReview,
            "test",
            "system",
            None,
            None,
        )
        .await
        .unwrap();

        let handle = spawn_coordinator(&db, &tx);
        handle.trigger_dispatch().await.unwrap();
        // Dispatch; wait for it to complete.
        handle.wait_for_status(|s| s.tasks_dispatched >= 1).await;

        let status = handle.get_status().unwrap();
        assert!(
            status.tasks_dispatched >= 1,
            "should dispatch task waiting for review"
        );
    }

    // ── Stuck detection ───────────────────────────────────────────────────────

    /// Variant of `spawn_coordinator` that returns the verification tracker
    /// so tests can register/deregister tasks to simulate background work.
    fn spawn_coordinator_with_tracker(
        db: &Database,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
    ) -> (CoordinatorHandle, VerificationTracker) {
        let cancel = CancellationToken::new();
        let ctx = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
        let pool = SlotPoolHandle::spawn(
            ctx,
            cancel.clone(),
            SlotPoolConfig {
                models: vec![ModelSlotConfig {
                    model_id: DEFAULT_MODEL_ID.to_owned(),
                    max_slots: 2,
                    roles: ["worker", "reviewer"]
                        .into_iter()
                        .map(ToOwned::to_owned)
                        .collect(),
                }],
                role_priorities: HashMap::new(),
            },
        );
        let catalog = CatalogService::new();
        let health = HealthTracker::new();
        let verification_tracker = VerificationTracker::default();
        let tracker_clone = verification_tracker.clone();
        let handle = CoordinatorHandle::spawn(CoordinatorDeps::new(
            tx.clone(),
            cancel,
            db.clone(),
            pool,
            catalog,
            health,
            Arc::new(RoleRegistry::new()),
            verification_tracker,
            crate::lsp::LspManager::new(),
        ));
        (handle, tracker_clone)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stuck_detection_skips_task_with_background_post_session_work() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        // Create a task and manually put it in in_task_review (simulating a
        // reviewer session that just ended — slot freed, but background merge
        // is still running).
        let task = repo
            .create(&epic.id, "Reviewing", "", "", "task", 0, "", Some("open"))
            .await
            .unwrap();
        repo.set_status(&task.id, "in_task_review").await.unwrap();

        let (handle, tracker) = spawn_coordinator_with_tracker(&db, &tx);

        // Register the task in the verification tracker (same as
        // spawn_post_session_work does for real sessions).
        tracker.lock().unwrap().insert(task.id.clone());

        // Trigger stuck scan — task should NOT be recovered because it has
        // registered background work.
        handle.trigger_stuck_scan().await.unwrap();
        // Give the actor time to process.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let updated = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(
            updated.status, "in_task_review",
            "task with background work should NOT be recovered"
        );

        // Now deregister — simulating background work completing.
        tracker.lock().unwrap().remove(&task.id);

        // Trigger stuck scan again — this time the task should be recovered.
        handle.trigger_stuck_scan().await.unwrap();
        handle.wait_for_status(|s| s.sessions_recovered >= 1).await;

        let final_task = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(
            final_task.status, "needs_task_review",
            "task without background work should be recovered to needs_task_review"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stuck_detection_releases_orphaned_in_progress_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        // Manually put a task in_progress (simulating an orphaned session).
        let task = repo
            .create(&epic.id, "Stuck", "", "", "task", 0, "", Some("open"))
            .await
            .unwrap();
        repo.set_status(&task.id, "in_progress").await.unwrap();

        let handle = spawn_coordinator(&db, &tx);
        handle.trigger_dispatch().await.unwrap();
        // Trigger dispatch to also run stuck detection; wait for recovery.
        handle.wait_for_status(|s| s.sessions_recovered >= 1).await;

        let status = handle.get_status().unwrap();
        assert!(
            status.sessions_recovered >= 1,
            "stuck task should have been recovered"
        );

        // The released task should now be back to open.
        let updated = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(
            updated.status, "open",
            "released task should be back to open"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_closed_task_applies_failure_confidence_once() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let _handle = spawn_coordinator(&db, &tx);
        let (task, note) = create_task_with_note(&db, &tx, "failed-close").await;
        let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        repo.set_status_with_reason(&task.id, "closed", Some("failed"))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let note_repo = NoteRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let note_after = note_repo.get(&note.id).await.unwrap().unwrap();
        assert!(note_after.confidence < 0.5);

        let markers = repo
            .query_activity(ActivityQuery {
                task_id: Some(task.id.clone()),
                event_type: Some(TASK_OUTCOME_CONFIDENCE_ACTIVITY.to_string()),
                actor_role: Some("system".to_string()),
                project_id: None,
                from_time: None,
                to_time: None,
                limit: 20,
                offset: 0,
            })
            .await
            .unwrap();
        assert_eq!(markers.len(), 1);
        let payload: serde_json::Value = serde_json::from_str(&markers[0].payload).unwrap();
        assert_eq!(payload["kind"], TASK_OUTCOME_FAILED_CLOSE);
        assert_eq!(payload["reopen_count"], 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reopened_twice_applies_failure_once_per_reopen_count() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let _handle = spawn_coordinator(&db, &tx);
        let (task, note) = create_task_with_note(&db, &tx, "reopen-twice").await;
        let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let note_repo = NoteRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        repo.set_status_with_reason(&task.id, "closed", Some("failed"))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        repo.set_status(&task.id, "open").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let reopened_once = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(reopened_once.reopen_count, 1);
        let after_first = note_repo.get(&note.id).await.unwrap().unwrap().confidence;
        assert!(after_first < 0.5, "first reopen should reduce confidence");

        repo.set_status(&task.id, "open").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let after_duplicate = note_repo.get(&note.id).await.unwrap().unwrap().confidence;
        assert!((after_duplicate - after_first).abs() < 1e-9);

        repo.set_status_with_reason(&task.id, "closed", Some("failed"))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        repo.set_status(&task.id, "open").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let reopened_twice = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(reopened_twice.reopen_count, 2);
        let after_second = note_repo.get(&note.id).await.unwrap().unwrap().confidence;
        assert!(
            after_second <= after_first,
            "second reopen should not increase confidence, got after_second={after_second}, after_first={after_first}"
        );

        let markers = repo
            .query_activity(ActivityQuery {
                task_id: Some(task.id.clone()),
                event_type: Some(TASK_OUTCOME_CONFIDENCE_ACTIVITY.to_string()),
                actor_role: Some("system".to_string()),
                project_id: None,
                from_time: None,
                to_time: None,
                limit: 20,
                offset: 0,
            })
            .await
            .unwrap();
        let reopen_markers: Vec<serde_json::Value> = markers
            .into_iter()
            .map(|entry| serde_json::from_str::<serde_json::Value>(&entry.payload).unwrap())
            .filter(|payload: &serde_json::Value| payload["kind"] == TASK_OUTCOME_REOPEN_COUNT)
            .collect();
        assert_eq!(reopen_markers.len(), 2);
        assert!(
            reopen_markers
                .iter()
                .any(|payload| payload["reopen_count"] == 1)
        );
        assert!(
            reopen_markers
                .iter()
                .any(|payload| payload["reopen_count"] == 2)
        );
    }

    // ── Architect patrol dispatch ─────────────────────────────────────────────

    // ── Wave-based Planner decomposition (task watx) ──────────────────────────

    /// Spawn a coordinator that includes "architect" and "planner" model slots,
    /// used by both patrol tests and wave decomposition tests.
    fn spawn_coordinator_with_planner(
        db: &Database,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
    ) -> CoordinatorHandle {
        let cancel = CancellationToken::new();
        let ctx = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
        let pool = SlotPoolHandle::spawn(
            ctx,
            cancel.clone(),
            SlotPoolConfig {
                models: vec![ModelSlotConfig {
                    model_id: DEFAULT_MODEL_ID.to_owned(),
                    max_slots: 4,
                    roles: ["worker", "reviewer", "planner", "architect"]
                        .into_iter()
                        .map(ToOwned::to_owned)
                        .collect(),
                }],
                role_priorities: HashMap::new(),
            },
        );
        let catalog = CatalogService::new();
        let health = HealthTracker::new();
        let verification_tracker = VerificationTracker::default();
        let role_registry = Arc::new(RoleRegistry::new());
        CoordinatorHandle::spawn(CoordinatorDeps::new(
            tx.clone(),
            cancel,
            db.clone(),
            pool,
            catalog,
            health,
            role_registry,
            verification_tracker,
            crate::lsp::LspManager::new(),
        ))
    }

    fn spawn_coordinator_with_architect(
        db: &Database,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
    ) -> CoordinatorHandle {
        spawn_coordinator_with_planner(db, tx)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn patrol_skips_when_no_open_epics() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        // No epics at all — patrol should skip without creating any tasks.
        let handle = spawn_coordinator_with_architect(&db, &tx);
        handle.trigger_architect_patrol().await.unwrap();
        // Give actor time to process.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let review_tasks = task_repo
            .list_ready(djinn_db::ReadyQuery {
                issue_type: Some("review".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();

        assert!(
            review_tasks.is_empty(),
            "patrol should not create review task when there are no open epics"
        );
        // Dispatch counter should remain 0.
        assert_eq!(
            handle.get_status().unwrap().tasks_dispatched,
            0,
            "patrol should not dispatch when no open epics"
        );
    }

    /// Helper: poll until there is at least `min_count` decomposition tasks for
    /// the given epic (open or in-progress), or timeout after 2 seconds.
    async fn wait_for_decomp_tasks(
        db: &Database,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
        epic_id: &str,
        min_count: usize,
    ) -> Vec<djinn_core::models::Task> {
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(tx));
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let tasks = task_repo.list_by_epic(epic_id).await.unwrap_or_default();
            let open_decomp: Vec<_> = tasks
                .into_iter()
                .filter(|t| {
                    matches!(t.issue_type.as_str(), "planning" | "decomposition")
                        && matches!(t.status.as_str(), "open" | "in_progress")
                })
                .collect();
            if open_decomp.len() >= min_count {
                return open_decomp;
            }
            if tokio::time::Instant::now() >= deadline {
                return open_decomp;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn epic_creation_triggers_decomposition_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        // Create project and epic BEFORE spawning coordinator so the project
        // is not auto-paused (coordinator pauses projects only when it receives
        // a project_created event — but create_test_project uses noop bus).
        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        // Spawn coordinator BEFORE creating epic so it receives the event.
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        // Yield to give the coordinator task a chance to start.
        tokio::task::yield_now().await;

        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Wave Test Epic",
                    description: "test",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                },
            )
            .await
            .unwrap();

        // Wait for the coordinator to process the epic_created event and create
        // the decomposition task (polling with 2s timeout).
        let decomp_tasks = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;

        assert_eq!(
            decomp_tasks.len(),
            1,
            "expected 1 decomposition task after epic creation"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn patrol_skips_when_architect_already_running() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        // Create an open epic so the patrol would normally run.
        let project = test_helpers::create_test_project(&db).await;
        EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Test Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                },
            )
            .await
            .unwrap();

        // Insert a fake running Architect session into the DB to simulate one already running.
        let session_id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO sessions (id, project_id, task_id, model_id, agent_type, status, started_at)
             VALUES (?1, ?2, NULL, 'test/mock', 'architect', 'running', strftime('%s','now'))",
        )
        .bind(&session_id)
        .bind(&project.id)
        .execute(db.pool())
        .await
        .unwrap();

        let handle = spawn_coordinator_with_architect(&db, &tx);
        handle.trigger_architect_patrol().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Dispatch counter should remain 0 — patrol was skipped.
        assert_eq!(
            handle.get_status().unwrap().tasks_dispatched,
            0,
            "patrol should skip when an Architect session is already running"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn epic_creation_does_not_create_duplicate_decomposition_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        tokio::task::yield_now().await;

        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Dedup Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                },
            )
            .await
            .unwrap();

        // Wait for the first decomposition task to be created.
        let decomp_tasks = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;
        assert_eq!(decomp_tasks.len(), 1, "expected 1 decomposition task");

        // Send a duplicate epic_created event (e.g. from a sync artifact).
        let _ = tx.send(DjinnEventEnvelope::epic_created(&epic));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        let open_planning_count = tasks
            .iter()
            .filter(|t| {
                matches!(t.issue_type.as_str(), "planning" | "decomposition")
                    && matches!(t.status.as_str(), "open" | "in_progress")
            })
            .count();
        assert_eq!(
            open_planning_count, 1,
            "duplicate epic_created events should not create duplicate planning tasks"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drafting_epic_creation_does_not_trigger_planning_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        tokio::task::yield_now().await;

        // Create a drafting epic (the new default).
        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Drafting Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("drafting"),
                },
            )
            .await
            .unwrap();

        // Give coordinator time to process.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        let planning_count = tasks
            .iter()
            .filter(|t| matches!(t.issue_type.as_str(), "planning" | "decomposition"))
            .count();
        assert_eq!(
            planning_count, 0,
            "drafting epic should not trigger planning task creation"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drafting_to_open_promotion_triggers_planning_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        tokio::task::yield_now().await;

        // Create a drafting epic — should NOT trigger planning.
        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Promote Me Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("drafting"),
                },
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Promote: update status to open directly and fire epic_updated event.
        sqlx::query("UPDATE epics SET status = 'open' WHERE id = ?1")
            .bind(&epic.id)
            .execute(db.pool())
            .await
            .unwrap();
        let promoted: djinn_core::models::Epic =
            sqlx::query_as("SELECT id, project_id, short_id, title, description, emoji, color, status, owner, memory_refs, closed_at, created_at, updated_at FROM epics WHERE id = ?1")
                .bind(&epic.id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        let _ = tx.send(DjinnEventEnvelope::epic_updated(&promoted));

        // Wait for planning task creation.
        let decomp_tasks = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;
        assert_eq!(
            decomp_tasks.len(),
            1,
            "drafting→open promotion should create exactly one planning task"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drafting_to_open_promotion_does_not_duplicate_planning_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        tokio::task::yield_now().await;

        // Create a drafting epic and promote it.
        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "No Dup Promote Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("drafting"),
                },
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Promote to open.
        sqlx::query("UPDATE epics SET status = 'open' WHERE id = ?1")
            .bind(&epic.id)
            .execute(db.pool())
            .await
            .unwrap();
        let promoted: djinn_core::models::Epic =
            sqlx::query_as("SELECT id, project_id, short_id, title, description, emoji, color, status, owner, memory_refs, closed_at, created_at, updated_at FROM epics WHERE id = ?1")
                .bind(&epic.id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        let _ = tx.send(DjinnEventEnvelope::epic_updated(&promoted));

        // Wait for first planning task.
        let decomp_tasks = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;
        assert_eq!(decomp_tasks.len(), 1);

        // Send another epic_updated event (e.g. title change while still open).
        let _ = tx.send(DjinnEventEnvelope::epic_updated(&promoted));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        let open_planning_count = tasks
            .iter()
            .filter(|t| {
                matches!(t.issue_type.as_str(), "planning" | "decomposition")
                    && matches!(t.status.as_str(), "open" | "in_progress")
            })
            .count();
        assert_eq!(
            open_planning_count, 1,
            "repeated epic_updated events should not duplicate planning tasks"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn patrol_creates_review_task_when_open_epic_exists() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        // Create project with an open epic and an open task.
        let project = test_helpers::create_test_project(&db).await;
        let epic = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Active Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                },
            )
            .await
            .unwrap();

        // Add an open task so the empty-board precondition passes.
        TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .create_in_project(
                &project.id,
                Some(&epic.id),
                "Test task",
                "",
                "",
                "task",
                1,
                "",
                None,
                None,
            )
            .await
            .unwrap();

        let handle = spawn_coordinator_with_architect(&db, &tx);
        handle.trigger_architect_patrol().await.unwrap();
        // Give the actor time to process.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Verify a review task was created (the patrol creates one for visibility).
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let tasks_by_project = task_repo.list_by_project(&project.id).await.unwrap();
        assert!(
            tasks_by_project
                .iter()
                .any(|t| t.issue_type == "review" && t.title.contains("patrol")),
            "patrol should create a review task; found tasks: {:?}",
            tasks_by_project
                .iter()
                .map(|t| (&t.title, &t.issue_type))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batch_completion_triggers_next_wave_decomposition() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        tokio::task::yield_now().await;

        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Batch Completion Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                },
            )
            .await
            .unwrap();

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        // Wait for the first decomposition task.
        let initial_decomp = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;
        assert_eq!(
            initial_decomp.len(),
            1,
            "should have initial decomposition task"
        );
        let decomp_task = &initial_decomp[0];

        // Manually close the decomposition task (simulating Planner completed wave 1).
        task_repo
            .set_status_with_reason(&decomp_task.id, "closed", Some("completed"))
            .await
            .unwrap();

        // Create 2 worker tasks under the epic.
        let w1 = task_repo
            .create(
                &epic.id,
                "Worker Task 1",
                "",
                "",
                "task",
                0,
                "",
                Some("open"),
            )
            .await
            .unwrap();
        let w2 = task_repo
            .create(
                &epic.id,
                "Worker Task 2",
                "",
                "",
                "task",
                0,
                "",
                Some("open"),
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Close both worker tasks — this should trigger batch-completion detection.
        task_repo
            .set_status_with_reason(&w1.id, "closed", Some("completed"))
            .await
            .unwrap();
        task_repo
            .set_status_with_reason(&w2.id, "closed", Some("completed"))
            .await
            .unwrap();

        // Wait for the coordinator to create the next-wave decomposition task.
        let next_wave = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;
        assert_eq!(
            next_wave.len(),
            1,
            "batch completion should create exactly one new decomposition task"
        );
    }
}
