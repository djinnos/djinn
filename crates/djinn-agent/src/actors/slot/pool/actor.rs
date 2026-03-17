use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::actors::coordinator::CoordinatorHandle;
use djinn_db::TaskRepository;
use crate::context::AgentContext;

use super::super::{ModelSlotConfig, SlotEvent, SlotHandle, SlotPoolConfig, SlotState};
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
        self.slot_states.insert(id, SlotState::Free);
        self.slot_models.insert(id, model_id.clone());
        self.free_slots.entry(model_id).or_default().push(id);
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
            PoolMessage::DispatchProject {
                project_id,
                project_path,
                agent_type,
                model_id,
                respond_to,
            } => {
                let _ = respond_to.send(
                    self.dispatch_project(project_id, project_path, agent_type, model_id)
                        .await,
                );
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
            PoolMessage::PauseSession {
                task_id,
                respond_to,
            } => {
                let _ = respond_to.send(self.pause_session(&task_id).await);
            }
            PoolMessage::GetStatus { respond_to } => {
                let _ = respond_to.send(Ok(self.get_status()));
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
        }
    }

    async fn dispatch(
        &mut self,
        task_id: String,
        project_path: String,
        model_id: String,
    ) -> Result<(), PoolError> {
        if self.task_to_slot.contains_key(&task_id) {
            return Err(PoolError::SessionAlreadyActive { task_id });
        }

        let slot_id = self
            .free_slots
            .entry(model_id.clone())
            .or_default()
            .pop()
            .ok_or(PoolError::AtCapacity {
                model_id: model_id.clone(),
            })?;

        let slot = self.slot(slot_id)?;
        if let Err(err) = slot.run_task(task_id.clone(), project_path).await {
            self.free_slots.entry(model_id).or_default().push(slot_id);
            return Err(PoolError::Slot(err));
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
        Ok(())
    }

    async fn dispatch_project(
        &mut self,
        project_id: String,
        project_path: String,
        agent_type: String,
        model_id: String,
    ) -> Result<(), PoolError> {
        let task_id = format!("project:{project_id}:{agent_type}");
        if self.task_to_slot.contains_key(&task_id) {
            return Err(PoolError::SessionAlreadyActive { task_id });
        }

        let slot_id = self
            .free_slots
            .entry(model_id.clone())
            .or_default()
            .pop()
            .ok_or(PoolError::AtCapacity {
                model_id: model_id.clone(),
            })?;

        let slot = self.slot(slot_id)?;
        if let Err(err) = slot
            .run_project(
                project_id.clone(),
                project_path,
                agent_type.clone(),
                model_id.clone(),
            )
            .await
        {
            self.free_slots.entry(model_id).or_default().push(slot_id);
            return Err(PoolError::Slot(err));
        }

        self.task_to_slot.insert(task_id.clone(), slot_id);
        self.task_started.insert(task_id.clone(), Instant::now());
        self.task_projects
            .insert(task_id.clone(), project_id.clone());
        self.slot_states.insert(
            slot_id,
            SlotState::Busy {
                task_id,
                started_at: now_unix_string(),
                agent_type,
            },
        );
        Ok(())
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
                self.task_to_slot.remove(&task_id);
                self.task_started.remove(&task_id);
                self.task_projects.remove(&task_id);

                if self.draining_slots.remove(&slot_id) {
                    self.retired_slots.insert(slot_id);
                    self.slot_states.insert(slot_id, SlotState::Draining);
                } else {
                    self.slot_states.insert(slot_id, SlotState::Free);
                    self.free_slots.entry(model_id).or_default().push(slot_id);
                }

                self.trigger_redispatch().await;
            }
        }
    }

    fn has_session(&self, task_id: &str) -> bool {
        self.task_to_slot.contains_key(task_id)
    }

    async fn kill_session(&self, task_id: &str) -> Result<(), PoolError> {
        let slot_id =
            self.task_to_slot
                .get(task_id)
                .copied()
                .ok_or_else(|| PoolError::TaskNotFound {
                    task_id: task_id.to_string(),
                })?;
        self.slot(slot_id)?.kill().await?;
        Ok(())
    }

    async fn pause_session(&self, task_id: &str) -> Result<(), PoolError> {
        let slot_id =
            self.task_to_slot
                .get(task_id)
                .copied()
                .ok_or_else(|| PoolError::TaskNotFound {
                    task_id: task_id.to_string(),
                })?;
        self.slot(slot_id)?.pause().await?;
        Ok(())
    }

    fn get_status(&self) -> super::types::PoolStatus {
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
                Some(super::types::RunningTaskInfo {
                    task_id: task_id.clone(),
                    model_id,
                    slot_id: *slot_id,
                    duration_seconds: started.elapsed().as_secs(),
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
        Some(super::types::RunningTaskInfo {
            task_id: task_id.to_string(),
            model_id,
            slot_id: *slot_id,
            duration_seconds,
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
        let coordinator: Option<CoordinatorHandle> = self.app_state.coordinator().await;
        if let Some(coord) = coordinator {
            // Use try_trigger_dispatch (non-blocking) to avoid deadlock:
            // the pool actor must not block on the coordinator channel because
            // the coordinator may be waiting on a pool response (e.g. has_session).
            coord.try_trigger_dispatch();
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
