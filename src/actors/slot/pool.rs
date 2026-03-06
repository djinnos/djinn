use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::actors::coordinator::CoordinatorHandle;
use crate::agent::SessionManager;
use crate::db::repositories::task::TaskRepository;
use crate::server::AppState;

use super::{ModelSlotConfig, SlotError, SlotEvent, SlotHandle, SlotPoolConfig, SlotState};

type SlotFactory = Arc<
    dyn Fn(
            usize,
            String,
            mpsc::Sender<SlotEvent>,
            AppState,
            Arc<SessionManager>,
            CancellationToken,
        ) -> SlotHandle
        + Send
        + Sync,
>;

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("actor channel closed")]
    ActorDead,
    #[error("no response from actor")]
    NoResponse,
    #[error("task {task_id} already has an active slot")]
    SessionAlreadyActive { task_id: String },
    #[error("task {task_id} has no active slot")]
    TaskNotFound { task_id: String },
    #[error("model {model_id} at capacity")]
    AtCapacity { model_id: String },
    #[error("slot {slot_id} not found")]
    SlotNotFound { slot_id: usize },
    #[error("slot error: {0}")]
    Slot(#[from] SlotError),
}

#[derive(Debug, Clone)]
pub struct ModelPoolStatus {
    pub active: u32,
    pub free: u32,
    pub total: u32,
}

#[derive(Debug, Clone)]
pub struct RunningTaskInfo {
    pub task_id: String,
    pub model_id: String,
    pub slot_id: usize,
    pub duration_seconds: u64,
}

#[derive(Debug, Clone)]
pub struct PoolStatus {
    pub active_slots: usize,
    pub total_slots: usize,
    pub per_model: HashMap<String, ModelPoolStatus>,
    pub running_tasks: Vec<RunningTaskInfo>,
}

type Reply<T> = oneshot::Sender<Result<T, PoolError>>;

pub enum PoolMessage {
    Dispatch {
        task_id: String,
        project_path: String,
        model_id: String,
        respond_to: Reply<()>,
    },
    HasSession {
        task_id: String,
        respond_to: Reply<bool>,
    },
    KillSession {
        task_id: String,
        respond_to: Reply<()>,
    },
    PauseSession {
        task_id: String,
        respond_to: Reply<()>,
    },
    GetStatus {
        respond_to: Reply<PoolStatus>,
    },
    GetSessionForTask {
        task_id: String,
        respond_to: Reply<Option<RunningTaskInfo>>,
    },
    Reconfigure {
        config: SlotPoolConfig,
        respond_to: Reply<()>,
    },
    InterruptAll {
        reason: String,
        respond_to: Reply<()>,
    },
    InterruptProject {
        project_id: String,
        reason: String,
        respond_to: Reply<()>,
    },
}

pub struct SlotPool {
    receiver: mpsc::Receiver<PoolMessage>,
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
    app_state: AppState,
    session_manager: Arc<SessionManager>,
    cancel: CancellationToken,
    slot_factory: SlotFactory,
}

impl SlotPool {
    fn new(
        receiver: mpsc::Receiver<PoolMessage>,
        app_state: AppState,
        session_manager: Arc<SessionManager>,
        cancel: CancellationToken,
        config: SlotPoolConfig,
    ) -> Self {
        let slot_factory: SlotFactory = Arc::new(
            |id, model_id, event_tx, app_state, session_manager, cancel| {
                SlotHandle::spawn(id, model_id, event_tx, app_state, session_manager, cancel)
            },
        );
        Self::new_with_factory(
            receiver,
            app_state,
            session_manager,
            cancel,
            config,
            slot_factory,
        )
    }

    fn new_with_factory(
        receiver: mpsc::Receiver<PoolMessage>,
        app_state: AppState,
        session_manager: Arc<SessionManager>,
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
            session_manager,
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
            self.session_manager.clone(),
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

    pub async fn run(mut self) {
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

    async fn project_id_for_task(&self, task_id: &str) -> Option<String> {
        let task_repo =
            TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
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

    fn get_status(&self) -> PoolStatus {
        let mut per_model: HashMap<String, ModelPoolStatus> = HashMap::new();
        let mut active_slots = 0usize;

        for (slot_id, model_id) in &self.slot_models {
            if self.retired_slots.contains(slot_id) {
                continue;
            }

            let status = per_model
                .entry(model_id.clone())
                .or_insert(ModelPoolStatus {
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
                Some(RunningTaskInfo {
                    task_id: task_id.clone(),
                    model_id,
                    slot_id: *slot_id,
                    duration_seconds: started.elapsed().as_secs(),
                })
            })
            .collect();

        PoolStatus {
            active_slots,
            total_slots: self
                .slot_models
                .len()
                .saturating_sub(self.retired_slots.len()),
            per_model,
            running_tasks,
        }
    }

    fn session_for_task(&self, task_id: &str) -> Option<RunningTaskInfo> {
        let slot_id = self.task_to_slot.get(task_id)?;
        let model_id = self.slot_models.get(slot_id)?.clone();
        let duration_seconds = self
            .task_started
            .get(task_id)
            .map(|ts| ts.elapsed().as_secs())
            .unwrap_or(0);
        Some(RunningTaskInfo {
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

#[derive(Clone)]
pub struct SlotPoolHandle {
    sender: mpsc::Sender<PoolMessage>,
}

impl SlotPoolHandle {
    pub fn spawn(
        app_state: AppState,
        session_manager: Arc<SessionManager>,
        cancel: CancellationToken,
        config: SlotPoolConfig,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(64);
        tokio::spawn(SlotPool::new(receiver, app_state, session_manager, cancel, config).run());
        Self { sender }
    }

    #[cfg(test)]
    pub(crate) fn spawn_with_factory(
        app_state: AppState,
        session_manager: Arc<SessionManager>,
        cancel: CancellationToken,
        config: SlotPoolConfig,
        slot_factory: SlotFactory,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(64);
        tokio::spawn(
            SlotPool::new_with_factory(
                receiver,
                app_state,
                session_manager,
                cancel,
                config,
                slot_factory,
            )
            .run(),
        );
        Self { sender }
    }

    async fn request<T>(&self, f: impl FnOnce(Reply<T>) -> PoolMessage) -> Result<T, PoolError> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(f(tx))
            .await
            .map_err(|_| PoolError::ActorDead)?;
        rx.await.map_err(|_| PoolError::NoResponse)?
    }

    pub async fn dispatch(
        &self,
        task_id: &str,
        project_path: &str,
        model_id: &str,
    ) -> Result<(), PoolError> {
        self.request(|tx| PoolMessage::Dispatch {
            task_id: task_id.to_owned(),
            project_path: project_path.to_owned(),
            model_id: model_id.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn has_session(&self, task_id: &str) -> Result<bool, PoolError> {
        self.request(|tx| PoolMessage::HasSession {
            task_id: task_id.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn kill_session(&self, task_id: &str) -> Result<(), PoolError> {
        self.request(|tx| PoolMessage::KillSession {
            task_id: task_id.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn pause_session(&self, task_id: &str) -> Result<(), PoolError> {
        self.request(|tx| PoolMessage::PauseSession {
            task_id: task_id.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn get_status(&self) -> Result<PoolStatus, PoolError> {
        self.request(|tx| PoolMessage::GetStatus { respond_to: tx })
            .await
    }

    pub async fn session_for_task(
        &self,
        task_id: &str,
    ) -> Result<Option<RunningTaskInfo>, PoolError> {
        self.request(|tx| PoolMessage::GetSessionForTask {
            task_id: task_id.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn reconfigure(&self, config: SlotPoolConfig) -> Result<(), PoolError> {
        self.request(|tx| PoolMessage::Reconfigure {
            config,
            respond_to: tx,
        })
        .await
    }

    pub async fn interrupt_all(&self, reason: &str) -> Result<(), PoolError> {
        self.request(|tx| PoolMessage::InterruptAll {
            reason: reason.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn interrupt_project(&self, project_id: &str, reason: &str) -> Result<(), PoolError> {
        self.request(|tx| PoolMessage::InterruptProject {
            project_id: project_id.to_owned(),
            reason: reason.to_owned(),
            respond_to: tx,
        })
        .await
    }
}

fn now_unix_string() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    use tempfile::TempDir;
    use tokio::sync::mpsc;

    use super::*;
    use crate::agent::init_session_manager;
    use crate::test_helpers;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum RunnerSignal {
        Started(String),
        Completed(String),
        Killed(String),
        Paused(String),
    }

    fn test_app_state() -> (AppState, Arc<SessionManager>, CancellationToken, TempDir) {
        let db = test_helpers::create_test_db();
        let cancel = CancellationToken::new();
        let app_state = AppState::new(db, cancel.clone());
        let temp = tempfile::tempdir().expect("tempdir");
        let session_manager = init_session_manager(temp.path().to_path_buf());
        (app_state, session_manager, cancel, temp)
    }

    fn model(model_id: &str, max_slots: u32, roles: &[&str]) -> ModelSlotConfig {
        ModelSlotConfig {
            model_id: model_id.to_string(),
            max_slots,
            roles: roles.iter().map(|r| (*r).to_string()).collect(),
        }
    }

    fn role_set(roles: &[&str]) -> HashSet<String> {
        roles.iter().map(|r| (*r).to_string()).collect()
    }

    fn make_config(
        models: Vec<ModelSlotConfig>,
        role_priorities: &[(&str, Vec<&str>)],
    ) -> SlotPoolConfig {
        SlotPoolConfig {
            models,
            role_priorities: role_priorities
                .iter()
                .map(|(role, priorities)| {
                    (
                        (*role).to_string(),
                        priorities.iter().map(|m| (*m).to_string()).collect(),
                    )
                })
                .collect(),
        }
    }

    fn test_slot_factory(
        runtime: Duration,
        signal_tx: mpsc::UnboundedSender<RunnerSignal>,
    ) -> SlotFactory {
        Arc::new(
            move |slot_id, model_id, event_tx, app_state, session_manager, cancel| {
                let signal_tx = signal_tx.clone();
                let runner: super::super::actor::TestLifecycleRunner = Arc::new(
                    move |task_id,
                          _project_path,
                          _model_id,
                          _app_state,
                          _session_manager,
                          kill,
                          pause| {
                        let signal_tx = signal_tx.clone();
                        Box::pin(async move {
                            let _ = signal_tx.send(RunnerSignal::Started(task_id.clone()));
                            tokio::select! {
                                _ = tokio::time::sleep(runtime) => {
                                    let _ = signal_tx.send(RunnerSignal::Completed(task_id));
                                }
                                _ = kill.cancelled() => {
                                    let _ = signal_tx.send(RunnerSignal::Killed(task_id));
                                }
                                _ = pause.cancelled() => {
                                    let _ = signal_tx.send(RunnerSignal::Paused(task_id));
                                }
                            }
                            Ok(())
                        })
                    },
                );

                SlotHandle::spawn_with_test_runner(
                    slot_id,
                    model_id,
                    event_tx,
                    app_state,
                    session_manager,
                    cancel,
                    runner,
                )
            },
        )
    }

    async fn wait_until_no_sessions(pool: &SlotPoolHandle, task_ids: &[String]) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let mut any_running = false;
            for task_id in task_ids {
                if pool
                    .has_session(task_id)
                    .await
                    .expect("has_session should succeed")
                {
                    any_running = true;
                    break;
                }
            }
            if !any_running {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for sessions to clear"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn dispatch_for_role(
        pool: &SlotPoolHandle,
        task_id: &str,
        project_path: &str,
        role: &str,
        role_priorities: &HashMap<String, Vec<String>>,
        model_roles: &HashMap<String, HashSet<String>>,
    ) -> Result<String, PoolError> {
        let priorities = role_priorities.get(role).cloned().unwrap_or_default();

        let mut last_capacity: Option<PoolError> = None;
        for model_id in priorities {
            if !model_roles
                .get(&model_id)
                .is_some_and(|roles| roles.contains(role))
            {
                continue;
            }

            match pool.dispatch(task_id, project_path, &model_id).await {
                Ok(()) => return Ok(model_id),
                Err(PoolError::AtCapacity { .. }) => {
                    last_capacity = Some(PoolError::AtCapacity {
                        model_id: model_id.clone(),
                    });
                }
                Err(other) => return Err(other),
            }
        }

        Err(last_capacity.unwrap_or(PoolError::AtCapacity {
            model_id: role.to_string(),
        }))
    }

    #[tokio::test]
    async fn parallel_completions_finish_concurrently() {
        let (app_state, session_manager, cancel, _temp) = test_app_state();
        let (signal_tx, _signal_rx) = mpsc::unbounded_channel();
        let config = make_config(
            vec![model("model-a", 4, &["worker"])],
            &[("worker", vec!["model-a"])],
        );
        let pool = SlotPoolHandle::spawn_with_factory(
            app_state,
            session_manager,
            cancel,
            config,
            test_slot_factory(Duration::from_millis(120), signal_tx),
        );

        let task_ids: Vec<String> = (0..4).map(|i| format!("parallel-{i}")).collect();
        for task_id in &task_ids {
            pool.dispatch(task_id, "/tmp/project", "model-a")
                .await
                .expect("dispatch should succeed");
        }

        let started = Instant::now();
        wait_until_no_sessions(&pool, &task_ids).await;
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(380),
            "expected concurrent completion under 380ms, got {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn model_priority_fallback_uses_next_model_when_primary_full() {
        let (app_state, session_manager, cancel, _temp) = test_app_state();
        let (signal_tx, _signal_rx) = mpsc::unbounded_channel();
        let config = make_config(
            vec![
                model("model-a", 1, &["worker"]),
                model("model-b", 2, &["worker"]),
            ],
            &[("worker", vec!["model-a", "model-b"])],
        );
        let role_priorities = config.role_priorities.clone();
        let model_roles: HashMap<String, HashSet<String>> = HashMap::from([
            ("model-a".to_string(), role_set(&["worker"])),
            ("model-b".to_string(), role_set(&["worker"])),
        ]);

        let pool = SlotPoolHandle::spawn_with_factory(
            app_state,
            session_manager,
            cancel,
            config,
            test_slot_factory(Duration::from_secs(10), signal_tx),
        );

        let m1 = dispatch_for_role(
            &pool,
            "task-1",
            "/tmp/project",
            "worker",
            &role_priorities,
            &model_roles,
        )
        .await
        .expect("first dispatch should succeed");
        let m2 = dispatch_for_role(
            &pool,
            "task-2",
            "/tmp/project",
            "worker",
            &role_priorities,
            &model_roles,
        )
        .await
        .expect("second dispatch should succeed");
        let m3 = dispatch_for_role(
            &pool,
            "task-3",
            "/tmp/project",
            "worker",
            &role_priorities,
            &model_roles,
        )
        .await
        .expect("third dispatch should succeed");

        assert_eq!(m1, "model-a");
        assert_eq!(m2, "model-b");
        assert_eq!(m3, "model-b");

        let fourth = dispatch_for_role(
            &pool,
            "task-4",
            "/tmp/project",
            "worker",
            &role_priorities,
            &model_roles,
        )
        .await;
        assert!(matches!(fourth, Err(PoolError::AtCapacity { .. })));

        pool.interrupt_all("test cleanup")
            .await
            .expect("interrupt_all should succeed");
        wait_until_no_sessions(&pool, &["task-1".into(), "task-2".into(), "task-3".into()]).await;
    }

    #[tokio::test]
    async fn role_isolation_skips_models_that_do_not_serve_role() {
        let (app_state, session_manager, cancel, _temp) = test_app_state();
        let (signal_tx, _signal_rx) = mpsc::unbounded_channel();
        let config = make_config(
            vec![
                model("opus", 1, &["task_reviewer"]),
                model("sonnet", 1, &["worker"]),
            ],
            &[
                ("worker", vec!["opus", "sonnet"]),
                ("task_reviewer", vec!["opus"]),
            ],
        );
        let role_priorities = config.role_priorities.clone();
        let model_roles: HashMap<String, HashSet<String>> = HashMap::from([
            ("opus".to_string(), role_set(&["task_reviewer"])),
            ("sonnet".to_string(), role_set(&["worker"])),
        ]);

        let pool = SlotPoolHandle::spawn_with_factory(
            app_state,
            session_manager,
            cancel,
            config,
            test_slot_factory(Duration::from_secs(10), signal_tx),
        );

        let first = dispatch_for_role(
            &pool,
            "worker-1",
            "/tmp/project",
            "worker",
            &role_priorities,
            &model_roles,
        )
        .await
        .expect("worker dispatch should succeed");
        assert_eq!(first, "sonnet");

        let status = pool.get_status().await.expect("status should succeed");
        assert_eq!(status.per_model.get("opus").map(|s| s.free), Some(1));

        let second = dispatch_for_role(
            &pool,
            "worker-2",
            "/tmp/project",
            "worker",
            &role_priorities,
            &model_roles,
        )
        .await;
        assert!(matches!(second, Err(PoolError::AtCapacity { .. })));

        pool.interrupt_all("test cleanup")
            .await
            .expect("interrupt_all should succeed");
        wait_until_no_sessions(&pool, &["worker-1".into()]).await;
    }

    #[tokio::test]
    async fn reconfigure_scale_up_adds_free_slots_for_dispatch() {
        let (app_state, session_manager, cancel, _temp) = test_app_state();
        let (signal_tx, _signal_rx) = mpsc::unbounded_channel();
        let config = make_config(
            vec![model("model-a", 2, &["worker"])],
            &[("worker", vec!["model-a"])],
        );
        let pool = SlotPoolHandle::spawn_with_factory(
            app_state,
            session_manager,
            cancel,
            config,
            test_slot_factory(Duration::from_secs(10), signal_tx),
        );

        pool.dispatch("up-1", "/tmp/project", "model-a")
            .await
            .expect("dispatch 1 should succeed");
        pool.dispatch("up-2", "/tmp/project", "model-a")
            .await
            .expect("dispatch 2 should succeed");
        assert!(matches!(
            pool.dispatch("up-3", "/tmp/project", "model-a").await,
            Err(PoolError::AtCapacity { .. })
        ));

        pool.reconfigure(make_config(
            vec![model("model-a", 4, &["worker"])],
            &[("worker", vec!["model-a"])],
        ))
        .await
        .expect("reconfigure should succeed");

        let status = pool.get_status().await.expect("status should succeed");
        let per_model = status
            .per_model
            .get("model-a")
            .expect("model-a should exist in status");
        assert_eq!(status.total_slots, 4);
        assert_eq!(per_model.active, 2);
        assert_eq!(per_model.free, 2);

        pool.dispatch("up-3", "/tmp/project", "model-a")
            .await
            .expect("dispatch 3 should succeed after scale-up");
        pool.dispatch("up-4", "/tmp/project", "model-a")
            .await
            .expect("dispatch 4 should succeed after scale-up");

        pool.interrupt_all("test cleanup")
            .await
            .expect("interrupt_all should succeed");
        wait_until_no_sessions(
            &pool,
            &["up-1".into(), "up-2".into(), "up-3".into(), "up-4".into()],
        )
        .await;
    }

    #[tokio::test]
    async fn reconfigure_scale_down_drains_busy_slots_then_retires_them() {
        let (app_state, session_manager, cancel, _temp) = test_app_state();
        let (signal_tx, _signal_rx) = mpsc::unbounded_channel();
        let config = make_config(
            vec![model("model-a", 4, &["worker"])],
            &[("worker", vec!["model-a"])],
        );
        let pool = SlotPoolHandle::spawn_with_factory(
            app_state,
            session_manager,
            cancel,
            config,
            test_slot_factory(Duration::from_millis(500), signal_tx),
        );

        let task_ids: Vec<String> = (0..4).map(|i| format!("down-{i}")).collect();
        for task_id in &task_ids {
            pool.dispatch(task_id, "/tmp/project", "model-a")
                .await
                .expect("dispatch should succeed");
        }

        pool.reconfigure(make_config(
            vec![model("model-a", 2, &["worker"])],
            &[("worker", vec!["model-a"])],
        ))
        .await
        .expect("reconfigure should succeed");

        let status_during_drain = pool.get_status().await.expect("status should succeed");
        assert_eq!(status_during_drain.total_slots, 4);

        wait_until_no_sessions(&pool, &task_ids).await;

        let status_after = pool.get_status().await.expect("status should succeed");
        assert_eq!(status_after.total_slots, 2);

        pool.dispatch("down-next-1", "/tmp/project", "model-a")
            .await
            .expect("dispatch should succeed");
        pool.dispatch("down-next-2", "/tmp/project", "model-a")
            .await
            .expect("dispatch should succeed");
        assert!(matches!(
            pool.dispatch("down-next-3", "/tmp/project", "model-a")
                .await,
            Err(PoolError::AtCapacity { .. })
        ));
    }

    #[tokio::test]
    async fn kill_and_pause_are_routed_to_the_correct_task_slot() {
        let (app_state, session_manager, cancel, _temp) = test_app_state();
        let (signal_tx, mut signal_rx) = mpsc::unbounded_channel();
        let config = make_config(
            vec![model("model-a", 2, &["worker"])],
            &[("worker", vec!["model-a"])],
        );
        let pool = SlotPoolHandle::spawn_with_factory(
            app_state,
            session_manager,
            cancel,
            config,
            test_slot_factory(Duration::from_secs(10), signal_tx),
        );

        pool.dispatch("task-kill", "/tmp/project", "model-a")
            .await
            .expect("kill task dispatch should succeed");
        pool.dispatch("task-pause", "/tmp/project", "model-a")
            .await
            .expect("pause task dispatch should succeed");

        let kill_slot = pool
            .session_for_task("task-kill")
            .await
            .expect("session lookup should succeed")
            .expect("kill task should have active session")
            .slot_id;
        let pause_slot = pool
            .session_for_task("task-pause")
            .await
            .expect("session lookup should succeed")
            .expect("pause task should have active session")
            .slot_id;
        assert_ne!(
            kill_slot, pause_slot,
            "tasks should be running in different slots"
        );

        pool.kill_session("task-kill")
            .await
            .expect("kill should succeed");
        pool.pause_session("task-pause")
            .await
            .expect("pause should succeed");

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut saw_kill = false;
        let mut saw_pause = false;
        while !(saw_kill && saw_pause) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for kill/pause signals"
            );
            if let Some(signal) = tokio::time::timeout(Duration::from_millis(200), signal_rx.recv())
                .await
                .expect("signal read should not timeout")
            {
                match signal {
                    RunnerSignal::Killed(task_id) if task_id == "task-kill" => saw_kill = true,
                    RunnerSignal::Paused(task_id) if task_id == "task-pause" => saw_pause = true,
                    _ => {}
                }
            }
        }

        wait_until_no_sessions(&pool, &["task-kill".into(), "task-pause".into()]).await;
    }
}
