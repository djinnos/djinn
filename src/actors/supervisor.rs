// AgentSupervisor — 1x global, manages in-process Goose session lifecycle.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use goose::agents::{
    Agent as GooseAgent, AgentConfig as GooseAgentConfig, GoosePlatform,
    SessionConfig as GooseSessionConfig,
};
use goose::config::{Config as GooseConfig, GooseMode, PermissionManager};
use goose::conversation::message::Message as GooseMessage;
use goose::model::ModelConfig;
use goose::providers;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::agent::extension;
use crate::agent::prompts::{TaskContext, render_prompt};
use crate::agent::{AgentType, GooseSessionHandle, SessionManager, SessionType};
use crate::db::repositories::credential::CredentialRepository;
use crate::db::repositories::git_settings::GitSettingsRepository;
use crate::db::repositories::task::TaskRepository;
use crate::models::task::{Task, TransitionAction};
use crate::server::AppState;

#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("actor channel closed")]
    ActorDead,
    #[error("no response from actor")]
    NoResponse,
    #[error("session already active for task {task_id}")]
    SessionAlreadyActive { task_id: String },
    #[error("task {task_id} not found")]
    TaskNotFound { task_id: String },
    #[error("invalid model id '{model_id}', expected provider/model")]
    InvalidModelId { model_id: String },
    #[error("no credential stored for provider {provider_id} (expected key {key_name})")]
    MissingCredential {
        provider_id: String,
        key_name: String,
    },
    #[error("task transition failed for {task_id}: {reason}")]
    TaskTransitionFailed { task_id: String, reason: String },
    #[error("goose session failed: {0}")]
    Goose(String),
    #[error("model {model_id} at capacity ({active}/{max})")]
    ModelAtCapacity {
        model_id: String,
        active: u32,
        max: u32,
    },
}

#[derive(Debug, Clone)]
pub struct SupervisorStatus {
    pub active_sessions: usize,
    pub capacity: HashMap<String, ModelCapacity>,
}

#[derive(Debug, Clone)]
pub struct ModelCapacity {
    pub active: u32,
    pub max: u32,
}

type Reply<T> = oneshot::Sender<Result<T, SupervisorError>>;

enum SupervisorMessage {
    Dispatch {
        task_id: String,
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
    InterruptAll {
        reason: String,
        respond_to: Reply<()>,
    },
    GetStatus {
        respond_to: Reply<SupervisorStatus>,
    },
    SessionCompleted {
        task_id: String,
        result: Result<(), String>,
    },
}

struct AgentSupervisor {
    receiver: mpsc::Receiver<SupervisorMessage>,
    sessions: HashMap<String, GooseSessionHandle>,
    capacity: HashMap<String, ModelCapacity>,
    session_models: HashMap<String, String>,
    session_agent_types: HashMap<String, AgentType>,
    interrupted_sessions: HashSet<String>,
    session_manager: Arc<SessionManager>,
    app_state: AppState,
    cancel: CancellationToken,
    sender: mpsc::Sender<SupervisorMessage>,
}

impl AgentSupervisor {
    fn new(
        receiver: mpsc::Receiver<SupervisorMessage>,
        sender: mpsc::Sender<SupervisorMessage>,
        app_state: AppState,
        session_manager: Arc<SessionManager>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            receiver,
            sessions: HashMap::new(),
            capacity: HashMap::new(),
            session_models: HashMap::new(),
            session_agent_types: HashMap::new(),
            interrupted_sessions: HashSet::new(),
            session_manager,
            app_state,
            cancel,
            sender,
        }
    }

    async fn run(mut self) {
        tracing::info!("AgentSupervisor started");
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    self.shutdown().await;
                    break;
                }
                msg = self.receiver.recv() => {
                    let Some(msg) = msg else { break; };
                    self.handle(msg).await;
                }
            }
        }
        tracing::info!("AgentSupervisor stopped");
    }

    async fn handle(&mut self, msg: SupervisorMessage) {
        match msg {
            SupervisorMessage::Dispatch {
                task_id,
                model_id,
                respond_to,
            } => {
                let _ = respond_to.send(self.dispatch(task_id, model_id).await);
            }
            SupervisorMessage::HasSession {
                task_id,
                respond_to,
            } => {
                let _ = respond_to.send(Ok(self.sessions.contains_key(&task_id)));
            }
            SupervisorMessage::KillSession {
                task_id,
                respond_to,
            } => {
                let _ = respond_to.send(self.kill_session(task_id).await);
            }
            SupervisorMessage::GetStatus { respond_to } => {
                let _ = respond_to.send(Ok(SupervisorStatus {
                    active_sessions: self.sessions.len(),
                    capacity: self.capacity.clone(),
                }));
            }
            SupervisorMessage::InterruptAll { reason, respond_to } => {
                self.interrupt_all_sessions(&reason).await;
                let _ = respond_to.send(Ok(()));
            }
            SupervisorMessage::SessionCompleted { task_id, result } => {
                if self.interrupted_sessions.remove(&task_id) {
                    return;
                }
                let model_id = self.session_models.get(&task_id).cloned();
                let agent_type = self.remove_session(&task_id);
                self.handle_session_result(&task_id, model_id.as_deref(), agent_type, result)
                    .await;
            }
        }
    }

    async fn dispatch(&mut self, task_id: String, model_id: String) -> Result<(), SupervisorError> {
        if self.sessions.contains_key(&task_id) {
            return Err(SupervisorError::SessionAlreadyActive { task_id });
        }

        let (active, max) = {
            let entry = self
                .capacity
                .entry(model_id.clone())
                .or_insert(ModelCapacity { active: 0, max: 1 });
            (entry.active, entry.max)
        };
        if active >= max {
            return Err(SupervisorError::ModelAtCapacity {
                model_id,
                active,
                max,
            });
        }

        if model_id == "test/mock" {
            if let Some(entry) = self.capacity.get_mut(&model_id) {
                entry.active += 1;
            }
            self.spawn_mock_session(task_id, model_id);
            return Ok(());
        }

        let task = self.load_task(&task_id).await?;
        let agent_type = self.agent_type_for_task(&task);
        self.transition_start(&task, agent_type).await?;

        let (provider_id, model_name) = Self::parse_model_id(&model_id)?;
        let (key_name, api_key) = self.load_provider_api_key(&provider_id).await?;
        GooseConfig::global()
            .set_secret(&key_name, &api_key)
            .map_err(|e| SupervisorError::Goose(e.to_string()))?;

        let session_name = format!("{} {}", task.short_id, task.title);
        let project_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let worktree_path = self.prepare_worktree(&project_dir, &task).await?;
        let session = self
            .session_manager
            .create_session(worktree_path.clone(), session_name, SessionType::SubAgent)
            .await
            .map_err(|e| SupervisorError::Goose(e.to_string()))?;

        let goose_model = ModelConfig::new(&model_name)
            .map_err(|e| SupervisorError::Goose(e.to_string()))?
            .with_canonical_limits(&provider_id);

        let extensions = self.extensions_for(agent_type);

        let provider = providers::create(&provider_id, goose_model, extensions.clone())
            .await
            .map_err(|e| {
                self.app_state.health_tracker().record_failure(&model_id);
                SupervisorError::Goose(e.to_string())
            })?;

        let agent = Arc::new(GooseAgent::with_config(GooseAgentConfig::new(
            self.session_manager.clone(),
            PermissionManager::instance(),
            None,
            GooseMode::Auto,
            true,
            GoosePlatform::GooseCli,
        )));

        agent
            .update_provider(provider, &session.id)
            .await
            .map_err(|e| {
                self.app_state.health_tracker().record_failure(&model_id);
                SupervisorError::Goose(e.to_string())
            })?;

        self.app_state.health_tracker().record_success(&model_id);

        for ext in extensions {
            agent
                .add_extension(ext, &session.id)
                .await
                .map_err(|e| SupervisorError::Goose(e.to_string()))?;
        }

        let prompt = render_prompt(
            agent_type,
            &task,
            &TaskContext {
                project_path: std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .display()
                    .to_string(),
                diff: None,
                commits: None,
                start_commit: None,
                end_commit: None,
                batch_num: None,
                task_count: None,
                tasks_summary: None,
                common_labels: None,
            },
        );
        agent.override_system_prompt(prompt).await;

        if let Some(entry) = self.capacity.get_mut(&model_id) {
            entry.active += 1;
        }
        let session_cancel = CancellationToken::new();
        let session_cancel_for_reply = session_cancel.clone();
        let global_cancel = self.cancel.clone();
        let task_id_for_join = task_id.clone();
        let session_id = session.id.clone();
        let sender = self.sender.clone();
        let app_state = self.app_state.clone();

        let join = tokio::spawn(async move {
            let kickoff = GooseMessage::user().with_text(
                "Start by understanding the task context and execute it fully before stopping.",
            );
            let session_cfg = GooseSessionConfig {
                id: session_id,
                schedule_id: None,
                max_turns: Some(300),
                retry_config: None,
            };

            let run_result: anyhow::Result<()> = async {
                let mut stream = agent
                    .reply(kickoff, session_cfg, Some(session_cancel_for_reply.clone()))
                    .await
                    .map_err(|e| anyhow::anyhow!(e.to_string()))?;

                let mut interrupted: Option<&'static str> = None;
                loop {
                    tokio::select! {
                        _ = session_cancel_for_reply.cancelled() => {
                            interrupted = Some("session cancelled");
                            break;
                        }
                        _ = global_cancel.cancelled() => {
                            interrupted = Some("supervisor shutting down");
                            break;
                        }
                        evt = stream.next() => {
                            let Some(evt) = evt else { break; };
                            let evt = evt.map_err(|e| anyhow::anyhow!(e.to_string()))?;
                            extension::handle_event(&app_state, &agent, &evt).await;
                        }
                    }
                }

                if let Some(reason) = interrupted {
                    return Err(anyhow::anyhow!(reason));
                }

                Ok(())
            }
            .await;

            let msg = match &run_result {
                Ok(()) => SupervisorMessage::SessionCompleted {
                    task_id: task_id_for_join,
                    result: Ok(()),
                },
                Err(e) => SupervisorMessage::SessionCompleted {
                    task_id: task_id_for_join,
                    result: Err(e.to_string()),
                },
            };
            let _ = sender.send(msg).await;

            run_result
        });

        self.sessions.insert(
            task_id.clone(),
            GooseSessionHandle {
                join,
                cancel: session_cancel,
                session_id: session.id,
                task_id: task_id.clone(),
                worktree_path: Some(worktree_path),
            },
        );
        self.session_models.insert(task_id.clone(), model_id);
        self.session_agent_types.insert(task_id, agent_type);
        Ok(())
    }

    fn spawn_mock_session(&mut self, task_id: String, model_id: String) {
        let session_cancel = CancellationToken::new();
        let session_cancel_for_join = session_cancel.clone();
        let global_cancel = self.cancel.clone();
        let task_id_for_join = task_id.clone();
        let sender = self.sender.clone();

        let join = tokio::spawn(async move {
            tokio::select! {
                _ = session_cancel_for_join.cancelled() => {}
                _ = global_cancel.cancelled() => {}
            }
            let _ = sender
                .send(SupervisorMessage::SessionCompleted {
                    task_id: task_id_for_join,
                    result: Ok(()),
                })
                .await;
            Ok(())
        });

        self.sessions.insert(
            task_id.clone(),
            GooseSessionHandle {
                join,
                cancel: session_cancel,
                session_id: format!("mock-session-{task_id}"),
                task_id: task_id.clone(),
                worktree_path: None,
            },
        );
        self.session_models.insert(task_id.clone(), model_id);
        self.session_agent_types.insert(task_id, AgentType::Worker);
    }

    async fn kill_session(&mut self, task_id: String) -> Result<(), SupervisorError> {
        let Some(mut handle) = self.sessions.remove(&task_id) else {
            return Ok(());
        };

        self.interrupted_sessions.insert(task_id.clone());

        let model_id = self.session_models.remove(&task_id);
        let agent_type = self
            .session_agent_types
            .remove(&task_id)
            .unwrap_or(AgentType::Worker);

        handle.cancel.cancel();

        match tokio::time::timeout(Duration::from_secs(30), &mut handle.join).await {
            Ok(_) => {}
            Err(_) => {
                tracing::warn!(task_id = %task_id, "session join timed out during kill; aborting");
                handle.join.abort();
                let _ = handle.join.await;
            }
        }

        self.decrement_capacity_for_model(model_id.as_deref());

        if let Some(worktree_path) = handle.worktree_path.as_ref() {
            self.commit_wip_if_needed(&task_id, worktree_path).await;
        }
        self.transition_interrupted(&task_id, agent_type, "session interrupted by supervisor kill")
            .await;

        Ok(())
    }

    fn remove_session(&mut self, task_id: &str) -> Option<AgentType> {
        let agent_type = self.session_agent_types.get(task_id).copied();
        if self.sessions.remove(task_id).is_some() {
            self.decrement_capacity(task_id);
            self.session_models.remove(task_id);
            self.session_agent_types.remove(task_id);
        }
        agent_type
    }

    fn decrement_capacity(&mut self, task_id: &str) {
        if let Some(model_id) = self.session_models.get(task_id)
            && let Some(model_capacity) = self.capacity.get_mut(model_id)
            && model_capacity.active > 0
        {
            model_capacity.active -= 1;
        }
    }

    fn decrement_capacity_for_model(&mut self, model_id: Option<&str>) {
        if let Some(model_id) = model_id
            && let Some(model_capacity) = self.capacity.get_mut(model_id)
            && model_capacity.active > 0
        {
            model_capacity.active -= 1;
        }
    }

    async fn commit_wip_if_needed(&self, task_id: &str, worktree_path: &PathBuf) {
        let git = match self.app_state.git_actor(worktree_path).await {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "failed to open git actor for worktree");
                return;
            }
        };

        let status = match git
            .run_command(vec!["status".into(), "--porcelain".into()])
            .await
        {
            Ok(out) => out,
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "failed to read worktree status");
                return;
            }
        };

        if status.stdout.trim().is_empty() {
            return;
        }

        if let Err(e) = git.run_command(vec!["add".into(), "-A".into()]).await {
            tracing::warn!(task_id = %task_id, error = %e, "failed to stage interrupted session changes");
            return;
        }

        let message = format!("WIP: interrupted session {task_id}");
        if let Err(e) = git
            .run_command(vec![
                "commit".into(),
                "--no-verify".into(),
                "-m".into(),
                message,
            ])
            .await
        {
            tracing::warn!(task_id = %task_id, error = %e, "failed to commit interrupted session changes");
        }
    }

    async fn transition_interrupted(&self, task_id: &str, agent_type: AgentType, reason: &str) {
        let action = match agent_type {
            AgentType::Worker => TransitionAction::Release,
            AgentType::TaskReviewer => TransitionAction::ReleaseTaskReview,
            AgentType::PhaseReviewer => TransitionAction::ReleasePhaseReview,
        };

        let repo = TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        if let Err(e) = repo
            .transition(
                task_id,
                action,
                "agent-supervisor",
                "system",
                Some(reason),
                None,
            )
            .await
        {
            tracing::warn!(task_id = %task_id, error = %e, "failed to transition interrupted task");
        }
    }

    async fn handle_session_result(
        &self,
        task_id: &str,
        model_id: Option<&str>,
        agent_type: Option<AgentType>,
        result: Result<(), String>,
    ) {
        let agent_type = agent_type.unwrap_or(AgentType::Worker);
        let repo =
            TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());

        if let Some(model_id) = model_id {
            match &result {
                Ok(()) => self.app_state.health_tracker().record_success(model_id),
                Err(_) => self.app_state.health_tracker().record_failure(model_id),
            }
        }

        let transition = match (agent_type, result) {
            (AgentType::Worker, Ok(())) => Some((TransitionAction::SubmitTaskReview, None)),
            (AgentType::TaskReviewer, Ok(())) => Some((TransitionAction::TaskReviewApprove, None)),
            (AgentType::PhaseReviewer, Ok(())) => {
                Some((TransitionAction::PhaseReviewApprove, None))
            }
            (AgentType::Worker, Err(reason)) => Some((TransitionAction::Release, Some(reason))),
            (AgentType::TaskReviewer, Err(reason)) => {
                Some((TransitionAction::ReleaseTaskReview, Some(reason)))
            }
            (AgentType::PhaseReviewer, Err(reason)) => {
                Some((TransitionAction::ReleasePhaseReview, Some(reason)))
            }
        };

        if let Some((action, reason)) = transition {
            let _ = repo
                .transition(
                    task_id,
                    action,
                    "agent-supervisor",
                    "system",
                    reason.as_deref(),
                    None,
                )
                .await;
        }
    }

    async fn transition_start(
        &self,
        task: &Task,
        agent_type: AgentType,
    ) -> Result<(), SupervisorError> {
        let action = match (agent_type, task.status.as_str()) {
            (AgentType::Worker, "open") => Some(TransitionAction::Start),
            (AgentType::TaskReviewer, "needs_task_review") => {
                Some(TransitionAction::TaskReviewStart)
            }
            (AgentType::PhaseReviewer, "needs_phase_review") => {
                Some(TransitionAction::PhaseReviewStart)
            }
            _ => None,
        };

        if let Some(action) = action {
            let repo =
                TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
            repo.transition(&task.id, action, "agent-supervisor", "system", None, None)
                .await
                .map_err(|e| SupervisorError::TaskTransitionFailed {
                    task_id: task.id.clone(),
                    reason: e.to_string(),
                })?;
        }
        Ok(())
    }

    async fn load_task(&self, task_id: &str) -> Result<Task, SupervisorError> {
        let repo =
            TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let task = repo
            .get(task_id)
            .await
            .map_err(|e| SupervisorError::Goose(e.to_string()))?;
        task.ok_or_else(|| SupervisorError::TaskNotFound {
            task_id: task_id.to_owned(),
        })
    }

    fn agent_type_for_task(&self, task: &Task) -> AgentType {
        match task.status.as_str() {
            "needs_task_review" | "in_task_review" => AgentType::TaskReviewer,
            "needs_phase_review" | "in_phase_review" => AgentType::PhaseReviewer,
            _ => AgentType::Worker,
        }
    }

    fn parse_model_id(model_id: &str) -> Result<(String, String), SupervisorError> {
        let Some((provider_id, model_name)) = model_id.split_once('/') else {
            return Err(SupervisorError::InvalidModelId {
                model_id: model_id.to_owned(),
            });
        };
        Ok((provider_id.to_owned(), model_name.to_owned()))
    }

    async fn load_provider_api_key(
        &self,
        provider_id: &str,
    ) -> Result<(String, String), SupervisorError> {
        let key_name = self
            .provider_secret_key(provider_id)
            .unwrap_or_else(|| format!("{}_API_KEY", provider_id.to_ascii_uppercase()));

        let repo =
            CredentialRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let key = repo
            .get_decrypted(&key_name)
            .await
            .map_err(|e| SupervisorError::Goose(e.to_string()))?;

        match key {
            Some(v) => Ok((key_name, v)),
            None => Err(SupervisorError::MissingCredential {
                provider_id: provider_id.to_owned(),
                key_name,
            }),
        }
    }

    fn provider_secret_key(&self, provider_id: &str) -> Option<String> {
        self.app_state
            .catalog()
            .list_providers()
            .into_iter()
            .find(|p| p.id == provider_id)
            .and_then(|p| p.env_vars.into_iter().next())
    }

    fn extensions_for(&self, agent_type: AgentType) -> Vec<goose::config::ExtensionConfig> {
        vec![extension::config(agent_type)]
    }

    async fn prepare_worktree(
        &self,
        project_dir: &PathBuf,
        task: &Task,
    ) -> Result<PathBuf, SupervisorError> {
        let branch = format!("task/{}", task.short_id);
        let git = self
            .app_state
            .git_actor(project_dir)
            .await
            .map_err(|e| SupervisorError::Goose(e.to_string()))?;

        match git.create_worktree(&task.short_id, &branch).await {
            Ok(path) => Ok(path),
            Err(_) => {
                let target_branch = self.default_target_branch().await;
                git.create_branch(&task.short_id, &target_branch)
                    .await
                    .map_err(|e| SupervisorError::Goose(e.to_string()))?;
                git.create_worktree(&task.short_id, &branch)
                    .await
                    .map_err(|e| SupervisorError::Goose(e.to_string()))
            }
        }
    }

    async fn default_target_branch(&self) -> String {
        let repo = GitSettingsRepository::new(
            self.app_state.db().clone(),
            self.app_state.events().clone(),
        );
        let project_path = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .display()
            .to_string();

        let project_id = sqlx::query_scalar::<_, String>("SELECT id FROM projects WHERE path = ?1")
            .bind(project_path)
            .fetch_optional(self.app_state.db().pool())
            .await
            .ok()
            .flatten();

        if let Some(project_id) = project_id
            && let Ok(settings) = repo.get(&project_id).await
        {
            return settings.target_branch;
        }

        "main".to_string()
    }

    async fn shutdown(&mut self) {
        self.interrupt_all_sessions("session interrupted by supervisor shutdown")
            .await;
    }

    async fn interrupt_all_sessions(&mut self, reason: &str) {
        struct PendingSession {
            task_id: String,
            join: tokio::task::JoinHandle<anyhow::Result<()>>,
            worktree_path: Option<PathBuf>,
            model_id: Option<String>,
            agent_type: AgentType,
        }

        let mut pending = Vec::new();
        for (task_id, mut handle) in std::mem::take(&mut self.sessions) {
            handle.cancel.cancel();
            self.interrupted_sessions.insert(task_id.clone());
            pending.push(PendingSession {
                model_id: self.session_models.remove(&task_id),
                agent_type: self
                    .session_agent_types
                    .remove(&task_id)
                    .unwrap_or(AgentType::Worker),
                task_id,
                join: handle.join,
                worktree_path: handle.worktree_path.take(),
            });
        }

        let deadline = Instant::now() + Duration::from_secs(30);
        for item in &mut pending {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                item.join.abort();
                continue;
            }

            if tokio::time::timeout(remaining, &mut item.join).await.is_err() {
                tracing::warn!(task_id = %item.task_id, "session join timed out during shutdown; aborting");
                item.join.abort();
            }
        }

        for item in pending {
            self.decrement_capacity_for_model(item.model_id.as_deref());
            if let Some(worktree_path) = item.worktree_path.as_ref() {
                self.commit_wip_if_needed(&item.task_id, worktree_path).await;
            }
            self.transition_interrupted(&item.task_id, item.agent_type, reason)
                .await;
        }
    }
}

#[derive(Clone)]
pub struct AgentSupervisorHandle {
    sender: mpsc::Sender<SupervisorMessage>,
}

impl AgentSupervisorHandle {
    pub fn spawn(
        app_state: AppState,
        session_manager: Arc<SessionManager>,
        cancel: CancellationToken,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(32);
        tokio::spawn(
            AgentSupervisor::new(receiver, sender.clone(), app_state, session_manager, cancel)
                .run(),
        );
        Self { sender }
    }

    async fn request<T>(
        &self,
        f: impl FnOnce(Reply<T>) -> SupervisorMessage,
    ) -> Result<T, SupervisorError> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(f(tx))
            .await
            .map_err(|_| SupervisorError::ActorDead)?;
        rx.await.map_err(|_| SupervisorError::NoResponse)?
    }

    pub async fn has_session(&self, task_id: &str) -> Result<bool, SupervisorError> {
        self.request(|tx| SupervisorMessage::HasSession {
            task_id: task_id.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn dispatch(&self, task_id: &str, model_id: &str) -> Result<(), SupervisorError> {
        self.request(|tx| SupervisorMessage::Dispatch {
            task_id: task_id.to_owned(),
            model_id: model_id.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn kill_session(&self, task_id: &str) -> Result<(), SupervisorError> {
        self.request(|tx| SupervisorMessage::KillSession {
            task_id: task_id.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn get_status(&self) -> Result<SupervisorStatus, SupervisorError> {
        self.request(|tx| SupervisorMessage::GetStatus { respond_to: tx })
            .await
    }

    pub async fn interrupt_all(&self, reason: &str) -> Result<(), SupervisorError> {
        self.request(|tx| SupervisorMessage::InterruptAll {
            reason: reason.to_owned(),
            respond_to: tx,
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::agent::init_session_manager;
    use crate::test_helpers;

    fn spawn_supervisor(temp: &TempDir) -> AgentSupervisorHandle {
        let db = test_helpers::create_test_db();
        let cancel = CancellationToken::new();
        let app_state = AppState::new(db, cancel.clone());
        let sessions = init_session_manager(temp.path().to_path_buf());
        AgentSupervisorHandle::spawn(app_state, sessions, cancel)
    }

    #[tokio::test]
    async fn tracks_session_lifecycle() {
        let temp = tempfile::tempdir().unwrap();
        let supervisor = spawn_supervisor(&temp);

        assert!(!supervisor.has_session("task-1").await.unwrap());
        supervisor.dispatch("task-1", "test/mock").await.unwrap();
        assert!(supervisor.has_session("task-1").await.unwrap());

        supervisor.kill_session("task-1").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert!(!supervisor.has_session("task-1").await.unwrap());
    }

    #[tokio::test]
    async fn enforces_per_model_capacity() {
        let temp = tempfile::tempdir().unwrap();
        let supervisor = spawn_supervisor(&temp);

        supervisor.dispatch("task-1", "test/mock").await.unwrap();
        let err = supervisor
            .dispatch("task-2", "test/mock")
            .await
            .unwrap_err();
        assert!(matches!(err, SupervisorError::ModelAtCapacity { .. }));

        supervisor.kill_session("task-1").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        supervisor.dispatch("task-2", "test/mock").await.unwrap();
    }

    #[tokio::test]
    async fn status_reports_capacity_and_active_count() {
        let temp = tempfile::tempdir().unwrap();
        let supervisor = spawn_supervisor(&temp);

        supervisor.dispatch("task-1", "test/mock").await.unwrap();
        let status = supervisor.get_status().await.unwrap();
        assert_eq!(status.active_sessions, 1);
        let model = status.capacity.get("test/mock").unwrap();
        assert_eq!(model.active, 1);
        assert_eq!(model.max, 1);
    }

    #[tokio::test]
    async fn interrupt_all_cancels_active_mock_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let supervisor = spawn_supervisor(&temp);

        supervisor.dispatch("task-1", "test/mock").await.unwrap();
        assert!(supervisor.has_session("task-1").await.unwrap());

        supervisor
            .interrupt_all("session interrupted by test")
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        assert!(!supervisor.has_session("task-1").await.unwrap());
    }
}
