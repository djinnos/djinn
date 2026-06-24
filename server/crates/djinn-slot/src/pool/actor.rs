//! Slot pool actor: manages a pool of slot actors.
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::host::SlotContext;
use djinn_orchestration_types::coordinator::DebugSlot;
// use djinn_orchestration_types::trigger::CoordinatorTrigger; // unused — re-enable when trigger dispatch is wired

use super::super::{ModelSlotConfig, SlotEvent, SlotHandle, SlotPoolConfig, SlotState};
use super::types::{PoolError, PoolMessage, SlotFactory};

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
        };
        pool.initialize_slots(&config.models);
        pool
    }

    fn roles_by_model(models: &[ModelSlotConfig]) -> HashMap<String, HashSet<String>> {
        let mut map = HashMap::new();
        for m in models {
            map.insert(m.model_id.clone(), m.roles.iter().cloned().collect());
        }
        map
    }

    fn initialize_slots(&mut self, models: &[ModelSlotConfig]) {
        for model_config in models {
            for _ in 0..model_config.max_slots {
                let id = self.slots.len();
                let handle = (self.slot_factory)(
                    id,
                    model_config.model_id.clone(),
                    self.event_tx.clone(),
                    self.ctx.clone(),
                    self.cancel.clone(),
                );
                self.slots.push(handle);
                self.slot_states.insert(id, SlotState::Free);
                self.slot_models.insert(id, model_config.model_id.clone());
                self.free_slots
                    .entry(model_config.model_id.clone())
                    .or_default()
                    .push(id);
            }
        }
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
                let result = if let Some(&slot_id) = self.task_to_slot.get(&task_id) {
                    self.slots[slot_id].kill().await.map_err(PoolError::from)
                } else {
                    Err(PoolError::TaskNotFound { task_id })
                };
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
                let info = self.task_to_slot.get(&task_id).and_then(|&sid| {
                    self.task_started
                        .get(&task_id)
                        .map(|started| super::types::RunningTaskInfo {
                            task_id: task_id.clone(),
                            model_id: self.slot_models.get(&sid).cloned().unwrap_or_default(),
                            slot_id: sid,
                            duration_seconds: started.elapsed().as_secs(),
                            idle_seconds: self.ctx.idle_seconds(&task_id).unwrap_or(0),
                            activity_tracked: true,
                            project_id: self.task_projects.get(&task_id).cloned(),
                            token_count: 0,
                            turn_count: 0,
                        })
                });
                let _ = respond_to.send(Ok(info));
            }
            PoolMessage::Reconfigure { config, respond_to } => {
                self.role_priorities = config.role_priorities;
                let _ = respond_to.send(Ok(()));
            }
            PoolMessage::InterruptAll { reason, respond_to } => {
                tracing::warn!(reason = %reason, "pool: interrupting all sessions");
                for slot in &self.slots {
                    let _ = slot.kill().await;
                }
                let _ = respond_to.send(Ok(()));
            }
            PoolMessage::InterruptProject {
                project_id,
                reason,
                respond_to,
            } => {
                tracing::warn!(project_id = %project_id, reason = %reason, "pool: interrupting project sessions");
                let task_ids: Vec<String> = self
                    .task_projects
                    .iter()
                    .filter(|(_, pid)| **pid == project_id)
                    .map(|(tid, _)| tid.clone())
                    .collect();
                for task_id in &task_ids {
                    if let Some(&slot_id) = self.task_to_slot.get(task_id) {
                        let _ = self.slots[slot_id].kill().await;
                    }
                }
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
                self.evict_session(&task_id);
                let _ = respond_to.send(Ok(()));
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
            #[cfg(test)]
            PoolMessage::TestSetTokenOverride { .. } => {}
        }
    }

    async fn dispatch(
        &mut self,
        task_id: &str,
        project_path: &str,
        model_id: &str,
    ) -> Result<(), PoolError> {
        if self.task_to_slot.contains_key(task_id) {
            return Err(PoolError::SessionAlreadyActive {
                task_id: task_id.to_string(),
            });
        }
        let free = self.free_slots.get_mut(model_id).and_then(|v| {
            if v.is_empty() {
                None
            } else {
                Some(v.remove(0))
            }
        });
        let slot_id = match free {
            Some(id) => id,
            None => {
                return Err(PoolError::AtCapacity {
                    model_id: model_id.to_string(),
                });
            }
        };
        self.slots[slot_id]
            .run_task(task_id.to_string(), project_path.to_string())
            .await?;
        self.task_to_slot.insert(task_id.to_string(), slot_id);
        self.task_projects
            .insert(task_id.to_string(), String::new());
        self.task_started
            .insert(task_id.to_string(), Instant::now());
        self.slot_states.insert(
            slot_id,
            SlotState::Busy {
                task_id: task_id.to_string(),
                started_at: super::types::now_unix_string(),
                agent_type: String::new(),
            },
        );
        self.ctx.register_activity(task_id);
        Ok(())
    }

    async fn handle_event(&mut self, event: SlotEvent) {
        match event {
            SlotEvent::Free {
                slot_id,
                model_id,
                task_id,
            } => {
                self.task_to_slot.remove(&task_id);
                self.task_projects.remove(&task_id);
                self.task_started.remove(&task_id);
                self.ctx.deregister_activity(&task_id);
                self.slot_states.insert(slot_id, SlotState::Free);
                self.free_slots.entry(model_id).or_default().push(slot_id);
                if self.draining_slots.contains(&slot_id) {
                    self.retired_slots.insert(slot_id);
                }
                // Trigger coordinator dispatch
                self.ctx.try_trigger_dispatch();
            }
            SlotEvent::Killed {
                slot_id,
                model_id,
                task_id,
            } => {
                self.task_to_slot.remove(&task_id);
                self.task_projects.remove(&task_id);
                self.task_started.remove(&task_id);
                self.ctx.deregister_activity(&task_id);
                self.slot_states.insert(slot_id, SlotState::Free);
                self.free_slots.entry(model_id).or_default().push(slot_id);
                if self.draining_slots.contains(&slot_id) {
                    self.retired_slots.insert(slot_id);
                }
                self.ctx.try_trigger_dispatch();
            }
        }
    }

    fn get_status(&self) -> super::types::PoolStatus {
        let mut per_model: HashMap<String, super::types::ModelPoolStatus> = HashMap::new();
        for (model_id, roles) in &self.model_roles {
            let free = self.free_slots.get(model_id).map(|v| v.len()).unwrap_or(0) as u32;
            let total = roles.len() as u32;
            per_model.insert(
                model_id.clone(),
                super::types::ModelPoolStatus {
                    active: total - free,
                    free,
                    total,
                },
            );
        }
        super::types::PoolStatus {
            active_slots: self.task_to_slot.len(),
            total_slots: self.slots.len(),
            per_model,
            running_tasks: Vec::new(),
        }
    }

    pub(super) fn snapshot(&self) -> Vec<DebugSlot> {
        self.slot_states
            .iter()
            .map(|(&id, state)| DebugSlot {
                slot_id: id as u32,
                model: self.slot_models.get(&id).cloned().unwrap_or_default(),
                state: format!("{:?}", state),
                task_id: match state {
                    SlotState::Busy { task_id, .. } => Some(task_id.clone()),
                    _ => None,
                },
                started_at: None,
            })
            .collect()
    }

    async fn terminate_session(&mut self, task_id: &str) -> Result<(), PoolError> {
        if let Some(&slot_id) = self.task_to_slot.get(task_id) {
            self.slots[slot_id].kill().await?;
            self.task_to_slot.remove(task_id);
            self.task_projects.remove(task_id);
            self.task_started.remove(task_id);
            self.ctx.deregister_activity(task_id);
            self.slot_states.insert(slot_id, SlotState::Free);
            if let Some(model_id) = self.slot_models.get(&slot_id).cloned() {
                self.free_slots.entry(model_id).or_default().push(slot_id);
            }
            Ok(())
        } else {
            Err(PoolError::TaskNotFound {
                task_id: task_id.to_string(),
            })
        }
    }

    fn evict_session(&mut self, task_id: &str) {
        if let Some(slot_id) = self.task_to_slot.remove(task_id) {
            self.task_projects.remove(task_id);
            self.task_started.remove(task_id);
            self.ctx.deregister_activity(task_id);
            self.slot_states.insert(slot_id, SlotState::Free);
            if let Some(model_id) = self.slot_models.get(&slot_id).cloned() {
                self.free_slots.entry(model_id).or_default().push(slot_id);
            }
        }
    }

    // ── Production helpers (extraction gap from djinn-agent) ────────────────

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

    fn slot(&self, slot_id: usize) -> Result<&SlotHandle, PoolError> {
        self.slots
            .get(slot_id)
            .ok_or(PoolError::SlotNotFound { slot_id })
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
                started_at: super::types::now_unix_string(),
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
