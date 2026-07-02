//! Slot pool actor: manages a pool of slot actors.
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::host::SlotContext;
use djinn_db::SessionRepository;
use djinn_orchestration_types::coordinator::DebugSlot;

use super::super::{ModelSlotConfig, SlotError, SlotEvent, SlotHandle, SlotPoolConfig, SlotState};
use super::types::{PoolError, PoolMessage, SlotFactory, now_unix_string};

pub(super) struct SlotPool {
    pub(super) receiver: mpsc::Receiver<PoolMessage>,
    event_rx: mpsc::Receiver<SlotEvent>,
    event_tx: mpsc::Sender<SlotEvent>,
    slots: Vec<SlotHandle>,
    free_slots: HashMap<String, Vec<usize>>,
    task_to_slot: HashMap<String, usize>,
    role_priorities: HashMap<String, Vec<String>>,
    model_roles: HashMap<String, HashSet<String>>,
    slot_states: HashMap<usize, SlotState>,
    slot_models: HashMap<usize, String>,
    task_projects: HashMap<String, String>,
    task_started: HashMap<String, Instant>,
    draining_slots: HashSet<usize>,
    retired_slots: HashSet<usize>,
    ctx: SlotContext,
    cancel: CancellationToken,
    slot_factory: SlotFactory,
    #[cfg(any(test, feature = "test-support"))]
    test_token_overrides: HashMap<String, (u64, u64)>,
}

impl SlotPool {
    pub(super) fn new(
        receiver: mpsc::Receiver<PoolMessage>,
        ctx: SlotContext,
        cancel: CancellationToken,
        config: SlotPoolConfig,
    ) -> Self {
        let slot_factory: SlotFactory = std::sync::Arc::new(SlotHandle::spawn);
        Self::new_with_factory(receiver, ctx, cancel, config, slot_factory)
    }

    pub(super) fn new_with_factory(
        receiver: mpsc::Receiver<PoolMessage>,
        ctx: SlotContext,
        cancel: CancellationToken,
        config: SlotPoolConfig,
        slot_factory: SlotFactory,
    ) -> Self {
        let (event_tx, event_rx) = mpsc::channel(128);
        let mut pool = Self {
            receiver,
            event_rx,
            event_tx,
            slots: Vec::new(),
            free_slots: HashMap::new(),
            task_to_slot: HashMap::new(),
            role_priorities: config.role_priorities.clone(),
            model_roles: Self::roles_by_model(&config.models),
            slot_states: HashMap::new(),
            slot_models: HashMap::new(),
            task_projects: HashMap::new(),
            task_started: HashMap::new(),
            draining_slots: HashSet::new(),
            retired_slots: HashSet::new(),
            ctx,
            cancel,
            slot_factory,
            #[cfg(any(test, feature = "test-support"))]
            test_token_overrides: HashMap::new(),
        };
        pool.spawn_slots_for_config(&config);
        pool
    }

    fn roles_by_model(models: &[ModelSlotConfig]) -> HashMap<String, HashSet<String>> {
        models
            .iter()
            .map(|m| (m.model_id.clone(), m.roles.iter().cloned().collect()))
            .collect()
    }

    fn spawn_slots_for_config(&mut self, config: &SlotPoolConfig) {
        for model in &config.models {
            for _ in 0..model.max_slots {
                self.spawn_slot(model.model_id.clone());
            }
        }
    }

    fn spawn_slot(&mut self, model_id: String) {
        let id = self.slots.len();
        let handle = (self.slot_factory)(
            id,
            model_id.clone(),
            self.event_tx.clone(),
            self.ctx.clone(),
            self.cancel.clone(),
        );
        self.slots.push(handle);
        self.slot_models.insert(id, model_id.clone());
        self.mark_slot_free(id, model_id);
    }

    pub(super) async fn run(mut self) {
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    break;
                }
                Some(msg) = self.receiver.recv() => {
                    self.handle_message(msg).await;
                }
                Some(event) = self.event_rx.recv() => {
                    self.handle_event(event).await;
                }
            }
        }
    }

    async fn handle_message(&mut self, msg: PoolMessage) {
        match msg {
            PoolMessage::Dispatch {
                task_id,
                project_path,
                model_id,
                respond_to,
            } => {
                let result = self.dispatch(&task_id, &project_path, &model_id).await;
                let _ = respond_to.send(result);
            }
            PoolMessage::DispatchWithResume {
                task_id,
                project_path,
                model_id,
                resume_lifecycle_metadata,
                respond_to,
            } => {
                // Re-dispatch with optional resume-via-git lifecycle metadata.
                // The legacy `dispatch` path is reused: it spawns the slot, and
                // the slot actor hands the metadata to its lifecycle runner via
                // `SlotCommand::RunTaskWithResume`. Selection metadata is not
                // used to mutate the worktree here — the integration task
                // (`twsk`) reads it from the `TaskRunSpec`. Keeping this path
                // mechanically identical to the legacy `Dispatch` ensures the
                // existing capacity/slot-busy/recovery semantics apply
                // unchanged when resume selection is enabled.
                let result = self
                    .dispatch_with_resume(
                        &task_id,
                        &project_path,
                        &model_id,
                        resume_lifecycle_metadata,
                    )
                    .await;
                let _ = respond_to.send(result);
            }
            PoolMessage::HasSession {
                task_id,
                respond_to,
            } => {
                let _ = respond_to.send(Ok(self.task_to_slot.contains_key(&task_id)));
            }
            PoolMessage::KillSession {
                task_id,
                respond_to,
            } => {
                let result = self.kill_session(&task_id).await;
                let _ = respond_to.send(result);
            }
            PoolMessage::GetStatus { respond_to } => {
                let _ = respond_to.send(Ok(self.get_status()));
            }
            PoolMessage::Snapshot { respond_to } => {
                let _ = respond_to.send(Ok(self.snapshot()));
            }
            PoolMessage::GetSessionForTask {
                task_id,
                respond_to,
            } => {
                let info = self.session_for_task(&task_id);
                let _ = respond_to.send(Ok(info));
            }
            PoolMessage::Reconfigure { config, respond_to } => {
                let _ = respond_to.send(self.reconfigure(config).await);
            }
            PoolMessage::InterruptAll { reason, respond_to } => {
                self.interrupt_all(&reason).await;
                let _ = respond_to.send(Ok(()));
            }
            PoolMessage::InterruptProject {
                project_id,
                reason,
                respond_to,
            } => {
                self.interrupt_project(&project_id, &reason).await;
                let _ = respond_to.send(Ok(()));
            }
            PoolMessage::TerminateSession {
                task_id,
                respond_to,
            } => {
                let result = self.terminate_session(&task_id).await;
                let _ = respond_to.send(result);
            }
            PoolMessage::EvictSession {
                task_id,
                respond_to,
            } => {
                let _ = respond_to.send(self.evict_session(&task_id).await);
            }
            PoolMessage::PauseSession {
                task_id,
                respond_to,
            } => {
                let result = if let Some(&slot_id) = self.task_to_slot.get(&task_id) {
                    self.slots[slot_id].pause().await.map_err(PoolError::from)
                } else {
                    Err(PoolError::TaskNotFound { task_id })
                };
                let _ = respond_to.send(result);
            }
            #[cfg(any(test, feature = "test-support"))]
            PoolMessage::TestSetTokenOverride {
                task_id,
                token_count,
                turn_count,
            } => {
                self.test_token_overrides
                    .insert(task_id, (token_count, turn_count));
            }
        }
    }

    fn session_for_task(&self, task_id: &str) -> Option<super::types::RunningTaskInfo> {
        let slot_id = self.task_to_slot.get(task_id)?;
        let model_id = self.slot_models.get(slot_id)?.clone();
        let duration_seconds = self
            .task_started
            .get(task_id)
            .map(|ts| {
                self.ctx
                    .clock
                    .now_instant()
                    .saturating_duration_since(*ts)
                    .as_secs()
            })
            .unwrap_or(0);
        // If activity tracker has no entry (reply loop not started yet),
        // the session has been idle since slot assignment.
        let tracked_idle = self.ctx.idle_seconds(task_id);
        let idle_seconds = tracked_idle.unwrap_or(duration_seconds);
        let project_id = self.task_projects.get(task_id).cloned();
        // Test-only token/turn overrides: in production the live counts are
        // bridged from the worker's `touch_activity` RPC, but in tests there
        // is no worker so the ceiling tests inject a count here.
        #[cfg(any(test, feature = "test-support"))]
        let (token_count, turn_count) = self
            .test_token_overrides
            .get(task_id)
            .copied()
            .unwrap_or((0, 0));
        #[cfg(not(any(test, feature = "test-support")))]
        let (token_count, turn_count) = (0, 0);
        Some(super::types::RunningTaskInfo {
            task_id: task_id.to_string(),
            model_id,
            slot_id: *slot_id,
            duration_seconds,
            idle_seconds,
            activity_tracked: tracked_idle.is_some(),
            project_id,
            token_count,
            turn_count,
            no_progress_streak: 0,
        })
    }

    async fn dispatch(
        &mut self,
        task_id: &str,
        project_path: &str,
        model_id: &str,
    ) -> Result<(), PoolError> {
        self.dispatch_with_resume(task_id, project_path, model_id, None)
            .await
    }

    /// Re-dispatch with optional resume-via-git lifecycle metadata. This is
    /// the additive re-dispatch path used when the coordinator's
    /// `select_resume_lifecycle_metadata_for_dispatch` produced a selection
    /// (the enabled branch of the `worker_lifecycle_config.resume` gate).
    /// When `resume_lifecycle_metadata` is `None` the path is mechanically
    /// identical to [`Self::dispatch`]; when it is `Some` the metadata blob
    /// rides the slot pipeline through
    /// `SlotCommand::RunTaskWithResume` so the lifecycle runner can put it on
    /// the `TaskRunSpec`. No worktree/checkout mutation is performed in this
    /// task — that remains `twsk`.
    async fn dispatch_with_resume(
        &mut self,
        task_id: &str,
        project_path: &str,
        model_id: &str,
        resume_lifecycle_metadata: Option<serde_json::Value>,
    ) -> Result<(), PoolError> {
        if self.task_to_slot.contains_key(task_id) {
            return Err(PoolError::SessionAlreadyActive {
                task_id: task_id.to_string(),
            });
        }

        // Elastic: there is no fixed per-model ceiling. Admission is gated
        // per-user by the coordinator (each user's per-model cap); if that gate
        // let this dispatch through, place the task on a slot — reuse a free
        // one, or spawn a fresh slot on demand.
        //
        // A slot can linger on `free_slots` while its actor is still mid-
        // lifecycle. `mark_slot_free` makes that rare, but a residual stale
        // entry must never wedge dispatch: such a slot answers `run_task` with
        // `SlotBusy`. We must NOT return it to the free list — re-queuing a busy
        // slot re-poisons it and wedges every later dispatch for this model in a
        // hot `SlotBusy` retry loop. Instead drop it from rotation (it rejoins
        // only when its real `Free`/`Killed` event lands) and try the next free
        // slot, finally spawning a fresh one.
        let model_owned = model_id.to_string();
        let task_owned = task_id.to_string();
        let proj_owned = project_path.to_string();
        loop {
            let slot_id = match self
                .free_slots
                .entry(model_owned.clone())
                .or_default()
                .pop()
            {
                Some(id) => id,
                None => {
                    self.spawn_slot(model_owned.clone());
                    self.free_slots
                        .entry(model_owned.clone())
                        .or_default()
                        .pop()
                        .ok_or(PoolError::AtCapacity {
                            model_id: model_owned.clone(),
                        })?
                }
            };

            let slot = self.slot(slot_id)?;
            match slot
                .run_task_with_resume(
                    task_owned.clone(),
                    proj_owned.clone(),
                    resume_lifecycle_metadata.clone(),
                )
                .await
            {
                Ok(()) => {}
                Err(SlotError::SlotBusy) => {
                    // Stale free-list entry: the slot's actor never truly freed.
                    // Drop it from rotation (do NOT re-queue) and mark it Busy so
                    // `get_status` stops counting phantom free capacity. It
                    // returns to the free list only when its real `Free`/`Killed`
                    // event reaches `handle_event`.
                    self.slot_states.insert(
                        slot_id,
                        SlotState::Busy {
                            task_id: String::new(),
                            started_at: now_unix_string(self.ctx.clock.as_ref()),
                            agent_type: "worker".to_string(),
                        },
                    );
                    tracing::warn!(
                        slot_id,
                        model_id = %model_owned,
                        "SlotPool: dropped stale free slot (actor still busy); retrying with another slot"
                    );
                    continue;
                }
                Err(err) => {
                    // Other errors leave the slot genuinely free — return it to
                    // the pool and surface the failure.
                    self.mark_slot_free(slot_id, model_owned.clone());
                    return Err(PoolError::Slot(err));
                }
            }

            self.task_to_slot.insert(task_owned.clone(), slot_id);
            self.task_started
                .insert(task_owned.clone(), self.ctx.clock.now_instant());
            self.task_projects.insert(task_owned.clone(), String::new());
            // Look up the project ID for project-scoped queries (e.g. interrupt_project)
            if let Some(project_id) = self.project_id_for_task(&task_owned).await {
                self.task_projects.insert(task_owned.clone(), project_id);
            }
            self.slot_states.insert(
                slot_id,
                SlotState::Busy {
                    task_id: task_owned.clone(),
                    started_at: now_unix_string(self.ctx.clock.as_ref()),
                    agent_type: String::new(),
                },
            );
            self.ctx.register_activity(&task_owned);
            return Ok(());
        }
    }

    async fn project_id_for_task(&self, task_id: &str) -> Option<String> {
        let task_repo =
            djinn_db::TaskRepository::new(self.ctx.db.clone(), self.ctx.event_bus.clone());
        task_repo
            .get(task_id)
            .await
            .ok()
            .flatten()
            .map(|task| task.project_id)
    }

    async fn handle_event(&mut self, event: SlotEvent) {
        let killed = matches!(event, SlotEvent::Killed { .. });
        match event {
            SlotEvent::Free {
                slot_id,
                model_id,
                task_id,
            }
            | SlotEvent::Killed {
                slot_id,
                model_id,
                task_id,
            } => {
                let owns_task_mapping = self.task_to_slot.get(&task_id).copied() == Some(slot_id);

                // On a killed lifecycle (stall-kill, interrupt_all/project,
                // explicit Kill command) settle the session DB row to a terminal
                // state *now*, at the moment the kill lands. A worker that is
                // killed mid-flow never reaches its own end-of-session flush, so
                // its row would sit `running` and keep over-counting the per-user
                // concurrency cap.
                if killed && owns_task_mapping {
                    self.teardown_taskrun_jobs_for_task(&task_id, "slot_event_killed")
                        .await;
                    self.settle_session_row(&task_id).await;
                }

                if owns_task_mapping {
                    self.task_to_slot.remove(&task_id);
                    self.task_started.remove(&task_id);
                    self.task_projects.remove(&task_id);
                    self.ctx.deregister_activity(&task_id);
                }

                if self.draining_slots.remove(&slot_id) {
                    self.retired_slots.insert(slot_id);
                    self.slot_states.insert(slot_id, SlotState::Draining);
                } else {
                    self.mark_slot_free(slot_id, model_id);
                }

                // Trigger coordinator dispatch
                self.ctx.try_trigger_dispatch();
            }
        }
    }

    async fn kill_session(&self, task_id: &str) -> Result<(), PoolError> {
        let slot_id =
            self.task_to_slot
                .get(task_id)
                .copied()
                .ok_or_else(|| PoolError::TaskNotFound {
                    task_id: task_id.to_string(),
                })?;
        self.teardown_taskrun_jobs_for_task(task_id, "kill_session")
            .await;
        // Settle the row now so the Killed event handler sees no running session
        // and does not tear down the same task-run Job twice.
        self.settle_session_row(task_id).await;
        self.slot(slot_id)?.kill().await?;
        Ok(())
    }

    /// Authoritatively terminate an operator/user-requested running session.
    /// Unlike [`kill_session`], this synchronously reclaims the task mapping,
    /// activity tracker, and running session row before returning. Unlike
    /// [`evict_session`], an unmapped task is reported truthfully as
    /// [`PoolError::TaskNotFound`].
    async fn terminate_session(&mut self, task_id: &str) -> Result<(), PoolError> {
        self.reclaim_session(task_id, "terminate_session", true)
            .await
    }

    /// Forcibly reclaim a leaked task→slot mapping. The normal lifecycle frees a
    /// slot only when the worker emits `SlotEvent::Free`/`Killed`; a pod that
    /// dies without that event (eviction, OOM, stuck RPC stream) leaves
    /// `task_to_slot` populated forever. This synthesizes the cleanup that never
    /// arrived.
    async fn evict_session(&mut self, task_id: &str) -> Result<(), PoolError> {
        self.reclaim_session(task_id, "evict_session", false).await
    }

    /// Shared reclaim logic for `terminate_session` and `evict_session`.
    async fn reclaim_session(
        &mut self,
        task_id: &str,
        reason: &str,
        require_mapping: bool,
    ) -> Result<(), PoolError> {
        let Some(slot_id) = self.task_to_slot.get(task_id).copied() else {
            if require_mapping {
                return Err(PoolError::TaskNotFound {
                    task_id: task_id.to_string(),
                });
            }
            return Ok(());
        };
        self.teardown_taskrun_jobs_for_task(task_id, reason).await;
        // Best-effort terminate; ignore errors — the point of eviction is that
        // this reclaim path must not be blocked by an unresponsive slot.
        if let Ok(slot) = self.slot(slot_id) {
            let _ = slot.kill().await;
        }
        // Settle the session row so the concurrency cap is freed immediately.
        self.settle_session_row(task_id).await;
        self.task_to_slot.remove(task_id);
        self.task_started.remove(task_id);
        self.task_projects.remove(task_id);
        self.ctx.deregister_activity(task_id);
        if self.draining_slots.remove(&slot_id) {
            self.retired_slots.insert(slot_id);
            self.slot_states.insert(slot_id, SlotState::Draining);
        }
        // A non-draining slot is intentionally NOT returned to the free pool
        // here. The `kill` drives the slot to emit `SlotEvent::Killed`;
        // `handle_event` then returns the slot via `mark_slot_free`. If the
        // worker never emits Killed, the slot leaks out of rotation and the
        // elastic pool spawns a fresh one on demand.
        self.ctx.try_trigger_dispatch();
        Ok(())
    }

    /// Transition any `running` session row for `task_id` to a terminal
    /// (`interrupted`) state so it stops counting against the per-user
    /// concurrency cap. Idempotent: `interrupt_running_for_task` updates only
    /// `running` rows, so re-settling an already-terminal row is a no-op.
    /// Best-effort: the slot teardown proceeds regardless of a transient DB
    /// error.
    async fn settle_session_row(&self, task_id: &str) {
        let session_repo = SessionRepository::new(self.ctx.db.clone(), self.ctx.event_bus.clone());
        if let Err(e) = session_repo.interrupt_running_for_task(task_id).await {
            tracing::warn!(
                task_id = %task_id,
                error = %e,
                "SlotPool: failed to settle session row on kill/evict (zombie backstop will retry)"
            );
        }
    }

    /// Best-effort task-run Job teardown for interrupting slot-pool paths.
    /// The slot pool stays behind the RuntimeOps bridge (no direct K8s
    /// dependency), and teardown failures are deliberately non-fatal so DB
    /// settlement and redispatch are never wedged by a Kubernetes/API hiccup.
    async fn teardown_taskrun_jobs_for_task(&self, task_id: &str, reason: &str) {
        let Some(ref runtime_ops) = self.ctx.runtime_ops else {
            return;
        };
        let session_repo = SessionRepository::new(self.ctx.db.clone(), self.ctx.event_bus.clone());
        let sessions = match session_repo.list_for_task(task_id).await {
            Ok(sessions) => sessions,
            Err(e) => {
                tracing::warn!(
                    task_id = %task_id,
                    reason = %reason,
                    error = %e,
                    "SlotPool: failed to look up task-run ids for Job teardown"
                );
                return;
            }
        };

        let mut task_run_ids = HashSet::new();
        for session in sessions {
            if session.status != djinn_core::models::SessionStatus::Running.as_str() {
                continue;
            }
            if let Some(task_run_id) = session.task_run_id.as_deref().map(str::trim)
                && !task_run_id.is_empty()
            {
                task_run_ids.insert(task_run_id.to_string());
            }
        }

        for task_run_id in task_run_ids {
            if let Err(e) = runtime_ops.teardown_taskrun_job(&task_run_id).await {
                tracing::warn!(
                    task_id = %task_id,
                    task_run_id = %task_run_id,
                    reason = %reason,
                    error = %e,
                    "SlotPool: task-run Job teardown failed (continuing slot kill/evict)"
                );
            }
        }
    }

    async fn reconfigure(&mut self, config: SlotPoolConfig) -> Result<(), PoolError> {
        self.role_priorities = config.role_priorities.clone();
        self.model_roles = Self::roles_by_model(&config.models);

        let mut desired: HashMap<String, usize> = HashMap::new();
        for model in &config.models {
            desired.insert(model.model_id.clone(), model.max_slots as usize);
        }

        let mut current: HashMap<String, Vec<usize>> = HashMap::new();
        for (slot_id, model_id) in &self.slot_models {
            if self.retired_slots.contains(slot_id) {
                continue;
            }
            current.entry(model_id.clone()).or_default().push(*slot_id);
        }

        // Scale up: spawn additional slots
        for (model_id, wanted) in &desired {
            let existing = current.get(model_id).map(|v| v.len()).unwrap_or(0);
            if *wanted > existing {
                for _ in 0..(*wanted - existing) {
                    self.spawn_slot(model_id.clone());
                }
            }
        }

        // Scale down: drain excess slots
        for (model_id, slots) in current {
            let wanted = desired.get(&model_id).copied().unwrap_or(0);
            if slots.len() <= wanted {
                continue;
            }

            let mut to_drain = slots.len() - wanted;

            // Drain free slots immediately
            let mut free_candidates = self.free_slots.get(&model_id).cloned().unwrap_or_default();
            while to_drain > 0 {
                let Some(slot_id) = free_candidates.pop() else {
                    break;
                };
                self.remove_from_free_list(&model_id, slot_id);
                self.drain_slot_immediately(slot_id).await;
                to_drain -= 1;
            }

            if to_drain == 0 {
                continue;
            }

            // Mark busy slots for draining (they'll retire when their task finishes)
            for slot_id in slots {
                if to_drain == 0 {
                    break;
                }
                if matches!(self.slot_states.get(&slot_id), Some(SlotState::Busy { .. })) {
                    self.draining_slots.insert(slot_id);
                    self.slot_states.insert(slot_id, SlotState::Draining);
                    if let Ok(slot) = self.slot(slot_id) {
                        let _ = slot.drain().await;
                    }
                    to_drain -= 1;
                }
            }
        }

        Ok(())
    }

    async fn interrupt_all(&self, _reason: &str) {
        let task_ids: Vec<String> = self.task_to_slot.keys().cloned().collect();
        for task_id in task_ids {
            let _ = self.kill_session(&task_id).await;
        }
    }

    async fn interrupt_project(&self, project_id: &str, _reason: &str) {
        let affected: Vec<String> = self
            .task_projects
            .iter()
            .filter_map(|(task_id, task_project)| {
                if task_project == project_id {
                    Some(task_id.clone())
                } else {
                    None
                }
            })
            .collect();

        for task_id in affected {
            let _ = self.kill_session(&task_id).await;
        }
    }

    fn get_status(&self) -> super::types::PoolStatus {
        self.record_slot_pool_metrics();

        let mut per_model: HashMap<String, super::types::ModelPoolStatus> = HashMap::new();
        let mut active_slots = 0usize;

        for (slot_id, model_id) in &self.slot_models {
            if self.retired_slots.contains(slot_id) {
                continue;
            }

            let status =
                per_model
                    .entry(model_id.clone())
                    .or_insert(super::types::ModelPoolStatus {
                        active: 0,
                        free: 0,
                        total: 0,
                    });

            status.total += 1;
            match self.slot_states.get(slot_id) {
                Some(SlotState::Busy { .. }) => {
                    active_slots += 1;
                    status.active += 1;
                }
                Some(SlotState::Free) => {
                    status.free += 1;
                }
                _ => {}
            }
        }

        let running_tasks = self
            .task_to_slot
            .iter()
            .filter_map(|(task_id, slot_id)| {
                let model_id = self.slot_models.get(slot_id)?.clone();
                let started = self.task_started.get(task_id)?;
                let duration_seconds = self
                    .ctx
                    .clock
                    .now_instant()
                    .saturating_duration_since(*started)
                    .as_secs();
                let tracked_idle = self.ctx.idle_seconds(task_id);
                let idle_seconds = tracked_idle.unwrap_or(duration_seconds);
                let project_id = self.task_projects.get(task_id).cloned();
                Some(super::types::RunningTaskInfo {
                    task_id: task_id.clone(),
                    model_id,
                    slot_id: *slot_id,
                    duration_seconds,
                    idle_seconds,
                    activity_tracked: tracked_idle.is_some(),
                    project_id,
                    token_count: 0,
                    turn_count: 0,
                    no_progress_streak: 0,
                })
            })
            .collect();

        super::types::PoolStatus {
            active_slots,
            total_slots: self
                .slot_models
                .len()
                .saturating_sub(self.retired_slots.len()),
            per_model,
            running_tasks,
        }
    }

    pub(super) fn snapshot(&self) -> Vec<DebugSlot> {
        let mut slots: Vec<_> = self
            .slot_models
            .iter()
            .map(|(slot_id, model)| {
                let state = self.slot_states.get(slot_id).unwrap_or(&SlotState::Free);
                match state {
                    SlotState::Free => DebugSlot {
                        slot_id: *slot_id as u32,
                        model: model.clone(),
                        state: "free".to_owned(),
                        task_id: None,
                        started_at: None,
                    },
                    SlotState::Busy {
                        task_id,
                        started_at,
                        ..
                    } => DebugSlot {
                        slot_id: *slot_id as u32,
                        model: model.clone(),
                        state: "busy".to_owned(),
                        task_id: if task_id.is_empty() {
                            self.task_to_slot
                                .iter()
                                .find_map(|(mapped_task, mapped_slot)| {
                                    (*mapped_slot == *slot_id).then(|| mapped_task.clone())
                                })
                        } else {
                            Some(task_id.clone())
                        },
                        started_at: Some(started_at.clone()),
                    },
                    SlotState::Draining => DebugSlot {
                        slot_id: *slot_id as u32,
                        model: model.clone(),
                        state: "draining".to_owned(),
                        task_id: self.task_to_slot.iter().find_map(|(task_id, mapped_slot)| {
                            (*mapped_slot == *slot_id).then(|| task_id.clone())
                        }),
                        started_at: None,
                    },
                }
            })
            .collect();
        slots.sort_by_key(|slot| slot.slot_id);
        slots
    }

    // ── Internal helpers ─────────────────────────────────────────────────

    fn slot(&self, slot_id: usize) -> Result<&SlotHandle, PoolError> {
        self.slots
            .get(slot_id)
            .ok_or(PoolError::SlotNotFound { slot_id })
    }

    fn remove_from_free_list(&mut self, model_id: &str, slot_id: usize) {
        if let Some(free) = self.free_slots.get_mut(model_id)
            && let Some(pos) = free.iter().position(|id| *id == slot_id)
        {
            free.swap_remove(pos);
        }
    }

    /// Return a slot to the free list if it's not retired and not already present.
    fn mark_slot_free(&mut self, slot_id: usize, model_id: String) {
        if self.retired_slots.contains(&slot_id) {
            return;
        }
        self.slot_states.insert(slot_id, SlotState::Free);
        let free = self.free_slots.entry(model_id).or_default();
        if !free.contains(&slot_id) {
            free.push(slot_id);
        }
    }

    async fn drain_slot_immediately(&mut self, slot_id: usize) {
        if let Some(model_id) = self.slot_models.get(&slot_id).cloned() {
            self.remove_from_free_list(&model_id, slot_id);
        }
        self.draining_slots.insert(slot_id);
        self.slot_states.insert(slot_id, SlotState::Draining);
        if let Ok(slot) = self.slot(slot_id) {
            let _ = slot.drain().await;
        }
        self.draining_slots.remove(&slot_id);
        self.retired_slots.insert(slot_id);
    }

    /// Publish scrape-visible slot-pool gauges aggregated only by `(state, model)`.
    fn record_slot_pool_metrics(&self) {
        let mut free_by_model: HashMap<String, usize> = HashMap::new();
        let mut busy_by_model: HashMap<String, usize> = HashMap::new();

        for (model_id, slots) in &self.free_slots {
            let count = slots
                .iter()
                .filter(|slot_id| !self.retired_slots.contains(slot_id))
                .count();
            free_by_model.insert(model_id.clone(), count);
        }

        for slot_id in self.task_to_slot.values() {
            if self.retired_slots.contains(slot_id) {
                continue;
            }
            if let Some(model_id) = self.slot_models.get(slot_id) {
                *busy_by_model.entry(model_id.clone()).or_insert(0) += 1;
            }
        }

        let mut model_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
        model_ids.extend(free_by_model.keys().map(String::as_str));
        model_ids.extend(busy_by_model.keys().map(String::as_str));

        for model_id in model_ids {
            djinn_telemetry::slot_pool::set_slots(
                djinn_telemetry::slot_pool::STATE_FREE,
                model_id,
                free_by_model.get(model_id).copied().unwrap_or(0),
            );
            djinn_telemetry::slot_pool::set_slots(
                djinn_telemetry::slot_pool::STATE_BUSY,
                model_id,
                busy_by_model.get(model_id).copied().unwrap_or(0),
            );
        }
    }

    // ── Test helpers (behind #[cfg(test)]) ─────────────────────────────────

    #[cfg(test)]
    pub(super) async fn test_dispatch(
        &mut self,
        task_id: &str,
        project_path: &str,
        model_id: &str,
    ) -> Result<(), PoolError> {
        self.dispatch(task_id, project_path, model_id).await
    }

    #[cfg(test)]
    pub(super) async fn test_terminate_session(&mut self, task_id: &str) -> Result<(), PoolError> {
        self.terminate_session(task_id).await
    }

    #[cfg(test)]
    pub(super) fn test_slot_of(&self, task_id: &str) -> Option<usize> {
        self.task_to_slot.get(task_id).copied()
    }

    #[cfg(test)]
    pub(super) fn test_free_slots(&self, model_id: &str) -> Vec<usize> {
        self.free_slots.get(model_id).cloned().unwrap_or_default()
    }

    #[cfg(test)]
    pub(super) fn test_free_slots_by_model(&self) -> HashMap<String, Vec<usize>> {
        self.free_slots.clone()
    }

    #[cfg(test)]
    pub(super) fn test_retired_slots(&self) -> HashSet<usize> {
        self.retired_slots.clone()
    }

    #[cfg(test)]
    pub(super) fn test_slot_states(&self) -> HashMap<usize, SlotState> {
        self.slot_states.clone()
    }

    #[cfg(test)]
    pub(super) fn test_task_slots(&self) -> HashMap<String, usize> {
        self.task_to_slot.clone()
    }

    #[cfg(test)]
    pub(super) fn test_set_slot_model(&mut self, slot_id: usize, model_id: &str) {
        self.slot_models.insert(slot_id, model_id.to_owned());
    }

    #[cfg(test)]
    pub(super) fn test_set_slot_state(&mut self, slot_id: usize, state: SlotState) {
        self.slot_states.insert(slot_id, state);
    }

    #[cfg(test)]
    pub(super) fn test_set_task_slot(&mut self, task_id: &str, slot_id: usize) {
        self.task_to_slot.insert(task_id.to_owned(), slot_id);
    }

    /// Raw push onto the free list, bypassing `mark_slot_free`.
    #[cfg(test)]
    pub(super) fn test_inject_free(&mut self, slot_id: usize, model_id: &str) {
        self.free_slots
            .entry(model_id.to_string())
            .or_default()
            .push(slot_id);
    }

    #[cfg(test)]
    pub(super) fn test_mark_slot_free(&mut self, slot_id: usize, model_id: &str) {
        self.mark_slot_free(slot_id, model_id.to_string());
    }

    #[cfg(test)]
    pub(super) fn test_assign_busy(&mut self, task_id: &str, slot_id: usize) {
        self.task_to_slot.insert(task_id.to_owned(), slot_id);
        if let Some(model_id) = self.slot_models.get(&slot_id).cloned() {
            self.remove_from_free_list(&model_id, slot_id);
        }
        self.slot_states.insert(
            slot_id,
            SlotState::Busy {
                task_id: task_id.to_owned(),
                started_at: now_unix_string(self.ctx.clock.as_ref()),
                agent_type: "worker".to_owned(),
            },
        );
    }

    #[cfg(test)]
    pub(super) fn test_record_slot_pool_metrics(&self) {
        self.record_slot_pool_metrics();
    }

    #[cfg(test)]
    pub(super) fn test_retire(&mut self, slot_id: usize) {
        self.retired_slots.insert(slot_id);
    }

    #[cfg(test)]
    pub(super) async fn test_handle_slot_event(&mut self, event: SlotEvent) {
        self.handle_event(event).await;
    }
}
