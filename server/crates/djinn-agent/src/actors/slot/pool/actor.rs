use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::actors::coordinator::CoordinatorHandle;
use crate::context::AgentContext;
use djinn_db::{SessionRepository, TaskRepository};
use djinn_orchestration_types::coordinator::DebugSlot;
use djinn_orchestration_types::trigger::CoordinatorTrigger;

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
    app_state: AgentContext,
    cancel: CancellationToken,
    slot_factory: SlotFactory,
    /// Test-only per-task `(token_count, turn_count)` overrides. In production
    /// live token spend is bridged from the worker's `touch_activity` RPC; in
    /// tests there is no worker, so the ceiling tests inject a high count here.
    /// Keyed by task_id. Empty in all non-test contexts.
    #[cfg(test)]
    test_token_overrides: HashMap<String, (u64, u64)>,
}

impl SlotPool {
    pub(super) fn new(
        receiver: mpsc::Receiver<PoolMessage>,
        app_state: AgentContext,
        cancel: CancellationToken,
        config: SlotPoolConfig,
    ) -> Self {
        let slot_factory: SlotFactory = Arc::new(|id, model_id, event_tx, app_state, cancel| {
            SlotHandle::spawn(id, model_id, event_tx, app_state, cancel)
        });
        Self::new_with_factory(receiver, app_state, cancel, config, slot_factory)
    }

    pub(super) fn new_with_factory(
        receiver: mpsc::Receiver<PoolMessage>,
        app_state: AgentContext,
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
            app_state,
            cancel,
            slot_factory,
            #[cfg(test)]
            test_token_overrides: HashMap::new(),
        };
        pool.spawn_slots_for_config(&config);
        pool
    }

    fn roles_by_model(models: &[ModelSlotConfig]) -> HashMap<String, HashSet<String>> {
        models
            .iter()
            .map(|m| (m.model_id.clone(), m.roles.clone()))
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
        let slot = (self.slot_factory)(
            id,
            model_id.clone(),
            self.event_tx.clone(),
            self.app_state.clone(),
            self.cancel.clone(),
        );
        self.slots.push(slot);
        self.slot_models.insert(id, model_id.clone());
        self.mark_slot_free(id, model_id);
    }

    /// Return a slot to the free pool — the single authoritative way to append
    /// to `free_slots`. Enforces two invariants the dispatch path depends on: a
    /// slot id appears on the free list at most once (no duplicates), and a
    /// retired slot is never resurrected into rotation. A duplicate or stale
    /// entry is what hands a still-busy slot to the next task, which answers
    /// `SlotBusy` and (pre-fix) wedged the whole model in a hot retry loop. Only
    /// `spawn_slot` (fresh slot) and `handle_slot_event` (a real `Free`/`Killed`
    /// event, i.e. the actor has actually stopped) call this — speculative
    /// callers like `evict_session` must not.
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

    /// Publish scrape-visible slot-pool gauges aggregated only by `(state, model)`.
    ///
    /// This is deliberately synchronous and walks the actor-owned live maps at
    /// snapshot time: `free_slots` is the source of truth for free capacity and
    /// `task_to_slot` is the source of truth for busy assignments. No per-slot or
    /// per-task labels are emitted, keeping cardinality bounded by the configured
    /// model set.
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

        let mut model_ids: HashSet<&str> = HashSet::new();
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

    fn slot(&self, slot_id: usize) -> Result<&SlotHandle, PoolError> {
        self.slots
            .get(slot_id)
            .ok_or(PoolError::SlotNotFound { slot_id })
    }

    pub(super) async fn run(mut self) {
        tracing::info!("SlotPool started");
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    self.shutdown().await;
                    break;
                }
                msg = self.receiver.recv() => {
                    let Some(msg) = msg else { break; };
                    self.handle_message(msg).await;
                }
                evt = self.event_rx.recv() => {
                    let Some(evt) = evt else { break; };
                    self.handle_slot_event(evt).await;
                }
            }
        }
        tracing::info!("SlotPool stopped");
    }

    async fn handle_message(&mut self, msg: PoolMessage) {
        match msg {
            PoolMessage::Dispatch {
                task_id,
                project_path,
                model_id,
                respond_to,
            } => {
                let _ = respond_to.send(self.dispatch(task_id, project_path, model_id).await);
            }
            PoolMessage::HasSession {
                task_id,
                respond_to,
            } => {
                let _ = respond_to.send(Ok(self.has_session(&task_id)));
            }
            PoolMessage::KillSession {
                task_id,
                respond_to,
            } => {
                let _ = respond_to.send(self.kill_session(&task_id).await);
            }
            PoolMessage::TerminateSession {
                task_id,
                respond_to,
            } => {
                let _ = respond_to.send(self.terminate_session(&task_id).await);
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
                let _ = respond_to.send(self.pause_session(&task_id).await);
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
                let _ = respond_to.send(Ok(self.session_for_task(&task_id)));
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
            #[cfg(test)]
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

    #[tracing::instrument(
        name = "djinn.slot.run_task",
        skip(self, project_path),
        fields(slot_id = tracing::field::Empty, model_id = %model_id, task_id = %task_id)
    )]
    #[allow(clippy::disallowed_methods)] // scoped: direct wall-clock read; migration tracked by lint-ratchet task 70y0 (Clock abstraction already lands in 8bcj/m5g4)
    async fn dispatch(
        &mut self,
        task_id: String,
        project_path: String,
        model_id: String,
    ) -> Result<(), PoolError> {
        if self.task_to_slot.contains_key(&task_id) {
            return Err(PoolError::SessionAlreadyActive { task_id });
        }

        // Elastic: there is no fixed per-model ceiling. Admission is gated
        // per-user by the coordinator (each user's per-model cap); if that gate
        // let this dispatch through, place the task on a slot — reuse a free
        // one, or spawn a fresh slot on demand (Karpenter backs the worker pod).
        //
        // A slot can linger on `free_slots` while its actor is still mid-
        // lifecycle. `mark_slot_free` makes that rare, but a residual stale
        // entry must never wedge dispatch: such a slot answers `run_task` with
        // `SlotBusy`. We must NOT return it to the free list — re-queuing a busy
        // slot re-poisons it and wedges every later dispatch for this model in a
        // hot `SlotBusy` retry loop (the `None => spawn_slot` arm never runs
        // because the poisoned entry keeps the list non-empty). Instead drop it
        // from rotation (it rejoins only when its real `Free`/`Killed` event
        // lands) and try the next free slot, finally spawning a fresh one.
        // Termination is guaranteed: a brand-new slot starts idle and always
        // accepts the task.
        loop {
            let slot_id = match self.free_slots.entry(model_id.clone()).or_default().pop() {
                Some(id) => id,
                None => {
                    self.spawn_slot(model_id.clone());
                    self.free_slots
                        .entry(model_id.clone())
                        .or_default()
                        .pop()
                        .ok_or(PoolError::AtCapacity {
                            model_id: model_id.clone(),
                        })?
                }
            };
            tracing::Span::current().record("slot_id", slot_id);

            let slot = self.slot(slot_id)?;
            match slot.run_task(task_id.clone(), project_path.clone()).await {
                Ok(()) => {}
                Err(SlotError::SlotBusy) => {
                    // Stale free-list entry: the slot's actor never truly freed.
                    // Drop it from rotation (do NOT re-queue) and mark it Busy so
                    // `get_status` stops counting phantom free capacity. It
                    // returns to the free list only when its real `Free`/`Killed`
                    // event reaches `handle_slot_event`.
                    self.slot_states.insert(
                        slot_id,
                        SlotState::Busy {
                            task_id: String::new(),
                            started_at: now_unix_string(),
                            agent_type: "worker".to_string(),
                        },
                    );
                    tracing::warn!(
                        slot_id,
                        model_id = %model_id,
                        "SlotPool: dropped stale free slot (actor still busy); retrying with another slot"
                    );
                    continue;
                }
                Err(err) => {
                    // Other errors leave the slot genuinely free — return it to
                    // the pool and surface the failure.
                    self.mark_slot_free(slot_id, model_id);
                    return Err(PoolError::Slot(err));
                }
            }

            self.task_to_slot.insert(task_id.clone(), slot_id);
            self.task_started.insert(task_id.clone(), Instant::now());
            if let Some(project_id) = self.project_id_for_task(&task_id).await {
                self.task_projects.insert(task_id.clone(), project_id);
            }
            self.slot_states.insert(
                slot_id,
                SlotState::Busy {
                    task_id,
                    started_at: now_unix_string(),
                    agent_type: "worker".to_string(),
                },
            );
            return Ok(());
        }
    }

    async fn project_id_for_task(&self, task_id: &str) -> Option<String> {
        let task_repo =
            TaskRepository::new(self.app_state.db.clone(), self.app_state.event_bus.clone());
        task_repo
            .get(task_id)
            .await
            .ok()
            .flatten()
            .map(|task| task.project_id)
    }

    async fn handle_slot_event(&mut self, event: SlotEvent) {
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
                tracing::info!(
                    event = if killed { "slot.event.killed" } else { "slot.event.free" },
                    slot_id,
                    model_id = %model_id,
                    task_id = %task_id,
                );
                let owns_task_mapping = self.task_to_slot.get(&task_id).copied() == Some(slot_id);

                // On a killed lifecycle (stall-kill, interrupt_all/project,
                // explicit Kill command) settle the session DB row to a terminal
                // state *now*, at the moment the kill lands — not only later via
                // the periodic zombie backstop. A worker that is killed mid-flow
                // never reaches its own end-of-session flush, so its row would sit
                // `running` and keep over-counting the per-user concurrency cap
                // (fatal at max_sessions=1: the user can't redispatch because a
                // dead session still "counts"). Idempotent: a no-op if no running
                // row exists. `Free` lifecycles settle their own row through the
                // normal terminal path, so we only settle here on `Killed`.
                //
                // A later lifecycle event can be stale for this task id after
                // operator `terminate_session` has synchronously reclaimed the
                // mapping and the task has already been re-dispatched on another
                // slot. In that case this event still frees *its own slot*, but it
                // must not tear down/interrupt the new session or remove the new
                // task→slot mapping/activity entry.
                if killed && owns_task_mapping {
                    self.teardown_taskrun_jobs_for_task(&task_id, "slot_event_killed")
                        .await;
                    self.settle_session_row(&task_id).await;
                }

                if owns_task_mapping {
                    self.task_to_slot.remove(&task_id);
                    self.task_started.remove(&task_id);
                    self.task_projects.remove(&task_id);
                    // Drop the host activity entry. `touch_activity` upserts one for
                    // remote workers (the host never `register_activity`s them), and
                    // nothing else removes it — so without this the map leaks an
                    // entry per completed task and, worse, a redispatched session
                    // reusing this task_id would inherit the stale "has shown
                    // activity" state and skip the first-call stall guard.
                    self.app_state.deregister_activity(&task_id);
                }

                if self.draining_slots.remove(&slot_id) {
                    self.retired_slots.insert(slot_id);
                    self.slot_states.insert(slot_id, SlotState::Draining);
                } else {
                    self.mark_slot_free(slot_id, model_id);
                }

                self.trigger_redispatch().await;
            }
        }
    }

    fn has_session(&self, task_id: &str) -> bool {
        self.task_to_slot.contains_key(task_id)
    }

    pub fn snapshot(&self) -> Vec<DebugSlot> {
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

    #[tracing::instrument(
        name = "djinn.slot.kill",
        skip(self),
        fields(slot_id = tracing::field::Empty, model_id = tracing::field::Empty, task_id = %task_id)
    )]
    async fn kill_session(&self, task_id: &str) -> Result<(), PoolError> {
        let slot_id =
            self.task_to_slot
                .get(task_id)
                .copied()
                .ok_or_else(|| PoolError::TaskNotFound {
                    task_id: task_id.to_string(),
                })?;
        tracing::Span::current().record("slot_id", slot_id);
        if let Some(model_id) = self.slot_models.get(&slot_id) {
            tracing::Span::current().record("model_id", tracing::field::display(model_id));
        }
        self.teardown_taskrun_jobs_for_task(task_id, "kill_session")
            .await;
        // `SlotEvent::Killed` also performs best-effort cleanup for kill paths
        // that bypass this request. Settle the row now so that event-side
        // cleanup sees no running session and does not tear down the same
        // task-run Job twice.
        self.settle_session_row(task_id).await;
        self.slot(slot_id)?.kill().await?;
        Ok(())
    }

    /// Authoritatively terminate an operator/user-requested running session.
    /// This uses the same synchronous reclaim path as leaked-session eviction,
    /// but is truthful: if no active task→slot mapping exists, the requested
    /// task is not currently running and the caller gets `TaskNotFound`.
    async fn terminate_session(&mut self, task_id: &str) -> Result<(), PoolError> {
        self.reclaim_session(task_id, "terminate_session", true)
            .await
    }

    /// Forcibly reclaim a leaked task→slot mapping. The normal lifecycle frees a
    /// slot only when the worker emits `SlotEvent::Free`/`Killed`; a pod that
    /// dies without that event (eviction, OOM, stuck RPC stream) leaves
    /// `task_to_slot` populated forever, so `has_session` lies `true` and the
    /// task can never redispatch (`dispatch` rejects with `SessionAlreadyActive`).
    /// This synthesizes the `Killed` cleanup that never arrived: it nudges the
    /// slot to die (best-effort — the pod is presumed unresponsive) and then
    /// removes the mapping/activity synchronously. Idempotent: a task with no
    /// mapping is a no-op.
    async fn evict_session(&mut self, task_id: &str) -> Result<(), PoolError> {
        self.reclaim_session(task_id, "evict_session", false).await
    }

    #[tracing::instrument(
        name = "djinn.slot.kill",
        skip(self),
        fields(slot_id = tracing::field::Empty, model_id = tracing::field::Empty, task_id = %task_id, reason = %reason)
    )]
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
        tracing::Span::current().record("slot_id", slot_id);
        if let Some(model_id) = self.slot_models.get(&slot_id) {
            tracing::Span::current().record("model_id", tracing::field::display(model_id));
        }
        self.teardown_taskrun_jobs_for_task(task_id, reason).await;
        // Best-effort terminate; ignore errors — the point of eviction is that
        // this reclaim path must not be blocked by an unresponsive slot. For
        // operator termination the later `SlotEvent::Killed` remains the only
        // authority that can return a non-draining slot to the free list.
        if let Ok(slot) = self.slot(slot_id) {
            let _ = slot.kill().await;
        }
        // A leaked/evicted pod may never emit `SlotEvent::Killed`, and operator
        // termination must be truthful before returning, so settle here
        // (idempotent) rather than depending on `handle_slot_event`. Without
        // this, the orphaned `running` row keeps over-counting the per-user
        // concurrency cap even after the task mapping is reclaimed.
        self.settle_session_row(task_id).await;
        self.task_to_slot.remove(task_id);
        self.task_started.remove(task_id);
        self.task_projects.remove(task_id);
        self.app_state.deregister_activity(task_id);
        if self.draining_slots.remove(&slot_id) {
            self.retired_slots.insert(slot_id);
            self.slot_states.insert(slot_id, SlotState::Draining);
        }
        // A non-draining slot is intentionally NOT returned to the free pool
        // here. The `kill` above drives the slot's actor to wind down its
        // lifecycle and emit `SlotEvent::Killed`; `handle_slot_event` then
        // returns the slot via `mark_slot_free` — exactly once, and only after
        // the actor has actually stopped running the task. Pushing it here
        // (before that confirmation) created a duplicate/stale free-list entry:
        // the slot would be handed to the next task, answer `SlotBusy`, and
        // wedge the whole model in a hot retry loop. If the worker is so wedged
        // that `Killed` never arrives, the slot leaks out of rotation and the
        // elastic pool spawns a fresh one on demand — strictly safer than
        // poisoning the free list.
        self.trigger_redispatch().await;
        Ok(())
    }

    /// Transition any `running` session row for `task_id` to a terminal
    /// (`interrupted`) state so it stops counting against the per-user
    /// concurrency cap the moment the slot is killed/evicted. Idempotent:
    /// `interrupt_running_for_task` updates only `running` rows, so re-settling
    /// an already-terminal row affects zero rows and is a no-op. Best-effort:
    /// the slot teardown proceeds regardless of a transient DB error, and the
    /// periodic `reap_zombie_sessions` backstop still covers any miss.
    async fn settle_session_row(&self, task_id: &str) {
        let session_repo =
            SessionRepository::new(self.app_state.db.clone(), self.app_state.event_bus.clone());
        if let Err(e) = session_repo.interrupt_running_for_task(task_id).await {
            tracing::warn!(
                task_id = %task_id,
                error = %e,
                "SlotPool: failed to settle session row on kill/evict (zombie backstop will retry)"
            );
        }
    }

    /// Best-effort task-run Job teardown for interrupting slot-pool paths. The
    /// slot pool stays behind the RuntimeOps bridge (no direct K8s dependency),
    /// and teardown failures are deliberately non-fatal so DB settlement and
    /// redispatch are never wedged by a Kubernetes/API hiccup.
    async fn teardown_taskrun_jobs_for_task(&self, task_id: &str, reason: &str) {
        let session_repo =
            SessionRepository::new(self.app_state.db.clone(), self.app_state.event_bus.clone());
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

        let mcp_state = self.app_state.to_mcp_state();
        for task_run_id in task_run_ids {
            if let Err(e) = mcp_state.teardown_taskrun_job(&task_run_id).await {
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

    #[tracing::instrument(
        name = "djinn.slot.cancel",
        skip(self),
        fields(slot_id = tracing::field::Empty, model_id = tracing::field::Empty, task_id = %task_id)
    )]
    async fn pause_session(&self, task_id: &str) -> Result<(), PoolError> {
        let slot_id =
            self.task_to_slot
                .get(task_id)
                .copied()
                .ok_or_else(|| PoolError::TaskNotFound {
                    task_id: task_id.to_string(),
                })?;
        tracing::Span::current().record("slot_id", slot_id);
        if let Some(model_id) = self.slot_models.get(&slot_id) {
            tracing::Span::current().record("model_id", tracing::field::display(model_id));
        }
        self.slot(slot_id)?.pause().await?;
        Ok(())
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
                let duration_seconds = started.elapsed().as_secs();
                // If activity tracker has no entry (reply loop not started yet),
                // the session has been idle since slot assignment.
                let tracked_idle = self.app_state.idle_seconds(task_id);
                let idle_seconds = tracked_idle.unwrap_or(duration_seconds);
                let project_id = self.task_projects.get(task_id).cloned();
                #[cfg(test)]
                let (token_count, turn_count) = self
                    .test_token_overrides
                    .get(task_id)
                    .copied()
                    .unwrap_or((0, 0));
                #[cfg(not(test))]
                let (token_count, turn_count) = (0, 0);
                Some(super::types::RunningTaskInfo {
                    task_id: task_id.clone(),
                    model_id,
                    slot_id: *slot_id,
                    duration_seconds,
                    idle_seconds,
                    activity_tracked: tracked_idle.is_some(),
                    project_id,
                    token_count,
                    turn_count,
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

    fn session_for_task(&self, task_id: &str) -> Option<super::types::RunningTaskInfo> {
        let slot_id = self.task_to_slot.get(task_id)?;
        let model_id = self.slot_models.get(slot_id)?.clone();
        let duration_seconds = self
            .task_started
            .get(task_id)
            .map(|ts| ts.elapsed().as_secs())
            .unwrap_or(0);
        // If activity tracker has no entry (reply loop not started yet),
        // the session has been idle since slot assignment.
        let tracked_idle = self.app_state.idle_seconds(task_id);
        let idle_seconds = tracked_idle.unwrap_or(duration_seconds);
        let project_id = self.task_projects.get(task_id).cloned();
        // Test-only token/turn overrides: in production the live counts are
        // bridged from the worker's `touch_activity` RPC, but in tests there
        // is no worker so the ceiling tests inject a count here.
        #[cfg(test)]
        let (token_count, turn_count) = self
            .test_token_overrides
            .get(task_id)
            .copied()
            .unwrap_or((0, 0));
        #[cfg(not(test))]
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
        })
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

        for (model_id, wanted) in &desired {
            let existing = current.get(model_id).map(|v| v.len()).unwrap_or(0);
            if *wanted > existing {
                for _ in 0..(*wanted - existing) {
                    self.spawn_slot(model_id.clone());
                }
            }
        }

        for (model_id, slots) in current {
            let wanted = desired.get(&model_id).copied().unwrap_or(0);
            if slots.len() <= wanted {
                continue;
            }

            let mut to_drain = slots.len() - wanted;

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

    async fn interrupt_project(&mut self, project_id: &str, _reason: &str) {
        let affected: Vec<String> = if self.task_projects.is_empty() {
            Vec::new()
        } else {
            self.task_projects
                .iter()
                .filter_map(|(task_id, task_project)| {
                    if task_project == project_id {
                        Some(task_id.clone())
                    } else {
                        None
                    }
                })
                .collect()
        };

        for task_id in affected {
            let _ = self.kill_session(&task_id).await;
        }
    }

    async fn trigger_redispatch(&self) {
        // Use CoordinatorTrigger::try_trigger_dispatch (non-blocking) to avoid
        // deadlock: the pool actor must not block on the coordinator channel
        // because the coordinator may be waiting on a pool response (e.g.
        // has_session).  The trait import breaks the direct slot → coordinator
        // internal dependency for this dispatch trigger path.
        let coordinator: Option<CoordinatorHandle> = self.app_state.coordinator().await;
        if let Some(coord) = coordinator {
            CoordinatorTrigger::try_trigger_dispatch(&coord);
        }
    }

    fn remove_from_free_list(&mut self, model_id: &str, slot_id: usize) {
        if let Some(free) = self.free_slots.get_mut(model_id)
            && let Some(pos) = free.iter().position(|id| *id == slot_id)
        {
            free.swap_remove(pos);
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

    #[allow(clippy::disallowed_methods)] // scoped: direct wall-clock read; migration tracked by lint-ratchet task 70y0 (Clock abstraction already lands in 8bcj/m5g4)
    async fn shutdown(&mut self) {
        let active_ids: Vec<usize> = self
            .slot_models
            .keys()
            .copied()
            .filter(|slot_id| !self.retired_slots.contains(slot_id))
            .collect();

        for slot_id in active_ids {
            let was_busy = matches!(self.slot_states.get(&slot_id), Some(SlotState::Busy { .. }));
            if let Ok(slot) = self.slot(slot_id) {
                let _ = slot.drain().await;
            }
            if !was_busy {
                self.retired_slots.insert(slot_id);
                self.draining_slots.remove(&slot_id);
                self.slot_states.insert(slot_id, SlotState::Draining);
            } else {
                self.draining_slots.insert(slot_id);
                self.slot_states.insert(slot_id, SlotState::Draining);
            }
        }

        let deadline = Instant::now() + Duration::from_secs(30);
        while !self.task_to_slot.is_empty() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let wait = deadline.saturating_duration_since(now);
            match tokio::time::timeout(wait, self.event_rx.recv()).await {
                Ok(Some(evt)) => self.handle_slot_event(evt).await,
                _ => break,
            }
        }
    }
}

#[cfg(test)]
impl SlotPool {
    /// White-box accessors for `pool::tests` (a sibling module that cannot
    /// otherwise reach `SlotPool`'s private state). They let a test drive
    /// `dispatch` directly and inspect/poison the free list to reproduce the
    /// stale-busy-slot wedge deterministically, without standing up the actor
    /// loop.
    pub(super) async fn test_dispatch(
        &mut self,
        task_id: &str,
        project_path: &str,
        model_id: &str,
    ) -> Result<(), PoolError> {
        self.dispatch(
            task_id.to_string(),
            project_path.to_string(),
            model_id.to_string(),
        )
        .await
    }

    pub(super) async fn test_terminate_session(&mut self, task_id: &str) -> Result<(), PoolError> {
        self.terminate_session(task_id).await
    }

    pub(super) fn test_slot_of(&self, task_id: &str) -> Option<usize> {
        self.task_to_slot.get(task_id).copied()
    }

    pub(super) fn test_free_slots(&self, model_id: &str) -> Vec<usize> {
        self.free_slots.get(model_id).cloned().unwrap_or_default()
    }

    pub(super) fn test_free_slots_by_model(&self) -> HashMap<String, Vec<usize>> {
        self.free_slots.clone()
    }

    pub(super) fn test_retired_slots(&self) -> HashSet<usize> {
        self.retired_slots.clone()
    }

    pub(super) fn test_slot_states(&self) -> HashMap<usize, SlotState> {
        self.slot_states.clone()
    }

    pub(super) fn test_task_slots(&self) -> HashMap<String, usize> {
        self.task_to_slot.clone()
    }

    pub(super) fn test_set_slot_model(&mut self, slot_id: usize, model_id: &str) {
        self.slot_models.insert(slot_id, model_id.to_owned());
    }

    pub(super) fn test_set_slot_state(&mut self, slot_id: usize, state: SlotState) {
        self.slot_states.insert(slot_id, state);
    }

    pub(super) fn test_set_task_slot(&mut self, task_id: &str, slot_id: usize) {
        self.task_to_slot.insert(task_id.to_owned(), slot_id);
    }

    /// Raw push onto the free list, bypassing `mark_slot_free` — used to inject
    /// the exact desync (`evict_session`'s old "push regardless" + the Killed
    /// event pushing again) that produced a duplicate/stale entry.
    pub(super) fn test_inject_free(&mut self, slot_id: usize, model_id: &str) {
        self.free_slots
            .entry(model_id.to_string())
            .or_default()
            .push(slot_id);
    }

    pub(super) fn test_mark_slot_free(&mut self, slot_id: usize, model_id: &str) {
        self.mark_slot_free(slot_id, model_id.to_string());
    }

    pub(super) fn test_assign_busy(&mut self, task_id: &str, slot_id: usize) {
        self.task_to_slot.insert(task_id.to_owned(), slot_id);
        if let Some(model_id) = self.slot_models.get(&slot_id).cloned() {
            self.remove_from_free_list(&model_id, slot_id);
        }
        self.slot_states.insert(
            slot_id,
            SlotState::Busy {
                task_id: task_id.to_owned(),
                started_at: now_unix_string(),
                agent_type: "worker".to_owned(),
            },
        );
    }

    pub(super) fn test_record_slot_pool_metrics(&self) {
        self.record_slot_pool_metrics();
    }

    pub(super) fn test_retire(&mut self, slot_id: usize) {
        self.retired_slots.insert(slot_id);
    }

    pub(super) async fn test_handle_slot_event(&mut self, event: SlotEvent) {
        self.handle_slot_event(event).await;
    }
}
