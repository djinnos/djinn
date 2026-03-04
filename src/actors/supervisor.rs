// AgentSupervisor — 1x global, manages in-process Goose session lifecycle.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Once;
use std::time::{Duration, Instant};

use goose::agents::{
    Agent as GooseAgent, AgentConfig as GooseAgentConfig, GoosePlatform,
    SessionConfig as GooseSessionConfig,
};
use goose::config::{Config as GooseConfig, GooseMode, PermissionManager};
use goose::conversation::message::Message as GooseMessage;
use goose::model::ModelConfig;
use goose::providers;
use goose::providers::base::ProviderMetadata;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::actors::git::GitError;
use crate::agent::extension;
use crate::agent::output_parser::{
    EpicReviewVerdict, ParsedAgentOutput, ReviewerVerdict, WorkerSignal,
};
use crate::agent::prompts::{TaskContext, render_prompt};
use crate::agent::{AgentType, GooseSessionHandle, SessionManager, SessionType};
use crate::db::repositories::credential::CredentialRepository;
use crate::db::repositories::epic::EpicRepository;
use crate::db::repositories::epic_review_batch::EpicReviewBatchRepository;
use crate::db::repositories::git_settings::GitSettingsRepository;
use crate::db::repositories::session::SessionRepository;
use crate::db::repositories::task::TaskRepository;
use crate::models::session::SessionStatus;
use crate::models::task::{Task, TransitionAction};
use crate::server::AppState;

const MERGE_CONFLICT_PREFIX: &str = "merge_conflict:";
static GOOSE_BUILTINS_REGISTERED: Once = Once::new();

fn register_goose_builtin_extensions() {
    GOOSE_BUILTINS_REGISTERED.call_once(|| {
        let builtins: HashMap<&'static str, goose::builtin_extension::SpawnServerFn> =
            goose_mcp::BUILTIN_EXTENSIONS
                .iter()
                .map(|(name, spawn)| (*name, *spawn))
                .collect();
        goose::builtin_extension::register_builtin_extensions(builtins);
    });
}

fn runtime_fs_diagnostics(project_path: &str, worktree_path: &Path) -> String {
    let project = Path::new(project_path);
    let worktree_git = worktree_path.join(".git");
    format!(
        "project_exists={} worktree_exists={} worktree_is_dir={} worktree_git_exists={} worktree_path={} project_path={}",
        project.exists(),
        worktree_path.exists(),
        worktree_path.is_dir(),
        worktree_git.exists(),
        worktree_path.display(),
        project.display(),
    )
}

fn runtime_env_diagnostics(session_id: &str, project_path: &str, worktree_path: &Path) -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unavailable>".to_string());
    let home = std::env::var("HOME").unwrap_or_else(|_| "<unset>".to_string());
    let xdg_config = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| "<unset>".to_string());
    let xdg_data = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| "<unset>".to_string());
    let path = std::env::var("PATH").unwrap_or_else(|_| "<unset>".to_string());

    let sessions_dir = PathBuf::from(&home).join(".djinn").join("sessions");
    let sessions_db = sessions_dir.join("sessions").join("sessions.db");
    format!(
        "session_id={} cwd={} home={} xdg_config_home={} xdg_data_home={} project_exists={} worktree_exists={} worktree_git_exists={} sessions_dir_exists={} sessions_db_exists={} worktree_path={} project_path={} path={}",
        session_id,
        cwd,
        home,
        xdg_config,
        xdg_data,
        Path::new(project_path).exists(),
        worktree_path.exists(),
        worktree_path.join(".git").exists(),
        sessions_dir.exists(),
        sessions_db.exists(),
        worktree_path.display(),
        project_path,
        path,
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MergeConflictMetadata {
    conflicting_files: Vec<String>,
    base_branch: String,
    merge_target: String,
}

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
    pub running_sessions: Vec<RunningSessionInfo>,
}

#[derive(Debug, Clone)]
pub struct ModelCapacity {
    pub active: u32,
    pub max: u32,
}

#[derive(Debug, Clone)]
pub struct RunningSessionInfo {
    pub task_id: String,
    pub model_id: String,
    pub session_id: String,
    pub duration_seconds: u64,
    pub worktree_path: Option<String>,
}

type Reply<T> = oneshot::Sender<Result<T, SupervisorError>>;

enum SupervisorMessage {
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
    InterruptAll {
        reason: String,
        respond_to: Reply<()>,
    },
    GetStatus {
        respond_to: Reply<SupervisorStatus>,
    },
    GetSessionForTask {
        task_id: String,
        respond_to: Reply<Option<RunningSessionInfo>>,
    },
    UpdateMaxSessions {
        max: u32,
        respond_to: Reply<()>,
    },
    UpdateSessionLimits {
        max_sessions: HashMap<String, u32>,
        default_max: u32,
        respond_to: Reply<()>,
    },
    SessionCompleted {
        task_id: String,
        result: Result<(), String>,
        output: ParsedAgentOutput,
    },
}

struct SessionClosure {
    model_id: Option<String>,
    agent_type: AgentType,
    goose_session_id: String,
    record_id: Option<String>,
    worktree_path: Option<PathBuf>,
}

struct AgentSupervisor {
    receiver: mpsc::Receiver<SupervisorMessage>,
    sessions: HashMap<String, GooseSessionHandle>,
    capacity: HashMap<String, ModelCapacity>,
    session_models: HashMap<String, String>,
    session_agent_types: HashMap<String, AgentType>,
    task_session_records: HashMap<String, String>,
    interrupted_sessions: HashSet<String>,
    default_max_sessions: u32,
    configured_model_limits: HashMap<String, u32>,
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
        register_goose_builtin_extensions();
        Self {
            receiver,
            sessions: HashMap::new(),
            capacity: HashMap::new(),
            session_models: HashMap::new(),
            session_agent_types: HashMap::new(),
            task_session_records: HashMap::new(),
            interrupted_sessions: HashSet::new(),
            default_max_sessions: 1,
            configured_model_limits: HashMap::new(),
            session_manager,
            app_state,
            cancel,
            sender,
        }
    }

    fn max_for_model(&self, model_id: &str) -> u32 {
        self.configured_model_limits
            .get(model_id)
            .copied()
            .unwrap_or(self.default_max_sessions)
    }

    fn apply_session_limits(&mut self, max_sessions: HashMap<String, u32>, default_max: u32) {
        self.default_max_sessions = default_max.max(1);
        self.configured_model_limits = max_sessions
            .into_iter()
            .filter(|(_, max)| *max > 0)
            .collect();

        for (model_id, max) in &self.configured_model_limits {
            let entry = self
                .capacity
                .entry(model_id.clone())
                .or_insert(ModelCapacity {
                    active: 0,
                    max: *max,
                });
            entry.max = *max;
        }

        let configured = self.configured_model_limits.clone();
        let default_max = self.default_max_sessions;
        for (model_id, entry) in &mut self.capacity {
            entry.max = configured.get(model_id).copied().unwrap_or(default_max);
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
                project_path,
                model_id,
                respond_to,
            } => {
                let _ = respond_to.send(self.dispatch(task_id, project_path, model_id).await);
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
                    running_sessions: self.running_sessions_snapshot(),
                }));
            }
            SupervisorMessage::GetSessionForTask {
                task_id,
                respond_to,
            } => {
                let session = self
                    .sessions
                    .get(&task_id)
                    .map(|handle| self.session_snapshot(&task_id, handle));
                let _ = respond_to.send(Ok(session));
            }
            SupervisorMessage::InterruptAll { reason, respond_to } => {
                self.interrupt_all_sessions(&reason).await;
                let _ = respond_to.send(Ok(()));
            }
            SupervisorMessage::UpdateMaxSessions { max, respond_to } => {
                self.apply_session_limits(self.configured_model_limits.clone(), max);
                let _ = respond_to.send(Ok(()));
            }
            SupervisorMessage::UpdateSessionLimits {
                max_sessions,
                default_max,
                respond_to,
            } => {
                self.apply_session_limits(max_sessions, default_max);
                let _ = respond_to.send(Ok(()));
            }
            SupervisorMessage::SessionCompleted {
                task_id,
                result,
                output,
            } => {
                if self.interrupted_sessions.remove(&task_id) {
                    return;
                }
                let session = self.remove_session(&task_id);
                self.handle_session_result(&task_id, session, result, output)
                    .await;
            }
        }
    }

    async fn dispatch(
        &mut self,
        task_id: String,
        project_path: String,
        model_id: String,
    ) -> Result<(), SupervisorError> {
        if self.sessions.contains_key(&task_id) {
            return Err(SupervisorError::SessionAlreadyActive { task_id });
        }

        let max_for_model = self.max_for_model(&model_id);
        let (active, max) = {
            let entry = self
                .capacity
                .entry(model_id.clone())
                .or_insert(ModelCapacity {
                    active: 0,
                    max: max_for_model,
                });
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
        let active_batch = self.active_epic_batch_for_task(&task.id).await;
        let conflict_ctx = self.conflict_context_for_dispatch(&task.id).await;
        let agent_type = if active_batch.is_some() {
            AgentType::EpicReviewer
        } else {
            self.agent_type_for_task(&task, conflict_ctx.is_some())
        };
        self.transition_start(&task, agent_type).await?;

        let (catalog_provider_id, model_name) = Self::parse_model_id(&model_id)?;
        let goose_provider_id = self.resolve_goose_provider_id(&catalog_provider_id).await;

        if !self
            .provider_supports_oauth(&goose_provider_id)
            .await
            .unwrap_or(false)
        {
            let (key_name, api_key) = self.load_provider_api_key(&catalog_provider_id).await?;
            GooseConfig::global()
                .set_secret(&key_name, &api_key)
                .map_err(|e| SupervisorError::Goose(e.to_string()))?;
        }

        let session_name = format!("{} {}", task.short_id, task.title);
        let project_dir = PathBuf::from(&project_path);
        let worktree_path = self.prepare_worktree(&project_dir, &task).await?;
        let goose_logs_dir = goose::config::paths::Paths::in_state_dir("logs");
        if let Err(e) = std::fs::create_dir_all(&goose_logs_dir) {
            tracing::warn!(
                task_id = %task.short_id,
                path = %goose_logs_dir.display(),
                error = %e,
                "failed to ensure Goose state logs directory"
            );
        }
        if !worktree_path.exists() || !worktree_path.is_dir() {
            let diag = runtime_fs_diagnostics(&project_path, &worktree_path);
            return Err(SupervisorError::Goose(format!(
                "worktree preflight failed before session creation: {diag}"
            )));
        }
        let session = self
            .session_manager
            .create_session(worktree_path.clone(), session_name, SessionType::SubAgent)
            .await
            .map_err(|e| SupervisorError::Goose(e.to_string()))?;

        let session_repo =
            SessionRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let session_record = session_repo
            .create(
                &task.project_id,
                &task.id,
                &model_id,
                agent_type.as_str(),
                worktree_path.to_str(),
            )
            .await
            .map_err(|e| SupervisorError::Goose(e.to_string()))?;

        if agent_type == AgentType::EpicReviewer
            && let Some(batch_id) = active_batch
        {
            let batch_repo = EpicReviewBatchRepository::new(
                self.app_state.db().clone(),
                self.app_state.events().clone(),
            );
            if let Err(e) = batch_repo.mark_in_review(&batch_id, &session.id).await {
                tracing::warn!(
                    task_id = %task.short_id,
                    batch_id = %batch_id,
                    error = %e,
                    "failed to mark epic review batch in_review"
                );
            }
        }

        let goose_model = ModelConfig::new(&model_name)
            .map_err(|e| SupervisorError::Goose(e.to_string()))?
            .with_canonical_limits(&goose_provider_id);

        let extensions = self.extensions_for(agent_type);

        let provider = providers::create(&goose_provider_id, goose_model, extensions.clone())
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

        let conflict_files = conflict_ctx.as_ref().map(|m| {
            m.conflicting_files
                .iter()
                .map(|f| format!("- {f}"))
                .collect::<Vec<_>>()
                .join("\n")
        });
        let prompt = render_prompt(
            agent_type,
            &task,
            &TaskContext {
                project_path: project_path.clone(),
                workspace_path: worktree_path.display().to_string(),
                diff: None,
                commits: None,
                start_commit: None,
                end_commit: None,
                batch_num: None,
                task_count: None,
                tasks_summary: None,
                common_labels: None,
                conflict_files,
                merge_base_branch: conflict_ctx.as_ref().map(|m| m.base_branch.clone()),
                merge_target_branch: conflict_ctx.as_ref().map(|m| m.merge_target.clone()),
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
        let run_agent_type = agent_type;
        let worktree_path_for_join = worktree_path.clone();
        let project_path_for_join = project_path.clone();

        let join = tokio::spawn(async move {
            let kickoff = GooseMessage::user().with_text(
                "Start by understanding the task context and execute it fully before stopping.",
            );
            let mut output = ParsedAgentOutput::default();
            let run_result: anyhow::Result<()> = async {
                let mut prompts = vec![kickoff];
                let mut nudge_count: u8 = 0;
                let mut saw_any_event = false;

                while let Some(next_message) = prompts.pop() {
                    let env_diag = runtime_env_diagnostics(
                        &session_id,
                        &project_path_for_join,
                        &worktree_path_for_join,
                    );
                    tracing::info!(
                        task_id = %task_id_for_join,
                        session_id = %session_id,
                        worktree = %worktree_path_for_join.display(),
                        "Supervisor: starting Goose reply; {}",
                        env_diag
                    );

                    let mut stream = agent
                        .reply(
                            next_message,
                            GooseSessionConfig {
                                id: session_id.clone(),
                                schedule_id: None,
                                max_turns: Some(300),
                                retry_config: None,
                            },
                            Some(session_cancel_for_reply.clone()),
                        )
                        .await
                        .map_err(|e| {
                            let diag = runtime_fs_diagnostics(
                                &project_path_for_join,
                                &worktree_path_for_join,
                            );
                            let env_diag = runtime_env_diagnostics(
                                &session_id,
                                &project_path_for_join,
                                &worktree_path_for_join,
                            );
                            anyhow::anyhow!(
                                "agent reply init failed: display={} debug={:?}; {}; {}",
                                e,
                                e,
                                diag,
                                env_diag
                            )
                        })?;

                    let mut interrupted: Option<&'static str> = None;
                    let mut saw_round_event = false;
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
                                let evt = evt.map_err(|e| {
                                    let diag = runtime_fs_diagnostics(
                                        &project_path_for_join,
                                        &worktree_path_for_join,
                                    );
                                    let env_diag = runtime_env_diagnostics(
                                        &session_id,
                                        &project_path_for_join,
                                        &worktree_path_for_join,
                                    );
                                    anyhow::anyhow!(
                                        "agent stream event failed: display={} debug={:?}; {}; {}",
                                        e,
                                        e,
                                        diag,
                                        env_diag
                                    )
                                })?;
                                saw_any_event = true;
                                saw_round_event = true;
                                output.ingest_event(&evt);
                                extension::handle_event(&app_state, &agent, &evt).await;
                            }
                        }
                    }

                    if let Some(reason) = interrupted {
                        return Err(anyhow::anyhow!(reason));
                    }

                    if !saw_round_event {
                        let diag =
                            runtime_fs_diagnostics(&project_path_for_join, &worktree_path_for_join);
                        return Err(anyhow::anyhow!(
                            "agent stream ended without any events; {}",
                            diag
                        ));
                    }

                    if let Some(nudge) =
                        Self::missing_marker_nudge(run_agent_type, &output, nudge_count)
                    {
                        nudge_count = nudge_count.saturating_add(1);
                        prompts.push(GooseMessage::user().with_text(nudge));
                        continue;
                    }
                }

                if !saw_any_event {
                    let diag =
                        runtime_fs_diagnostics(&project_path_for_join, &worktree_path_for_join);
                    return Err(anyhow::anyhow!(
                        "agent session produced no events; {}",
                        diag
                    ));
                }

                if Self::missing_required_marker(run_agent_type, &output) {
                    let reason = match run_agent_type {
                        AgentType::Worker | AgentType::ConflictResolver => {
                            "worker ended without WORKER_RESULT marker"
                        }
                        AgentType::TaskReviewer => {
                            "task reviewer ended without REVIEW_RESULT marker"
                        }
                        AgentType::EpicReviewer => {
                            "epic reviewer ended without EPIC_REVIEW_RESULT marker"
                        }
                    };
                    return Err(anyhow::anyhow!(reason));
                }

                Ok(())
            }
            .await;

            let msg = match &run_result {
                Ok(()) => SupervisorMessage::SessionCompleted {
                    task_id: task_id_for_join,
                    result: Ok(()),
                    output,
                },
                Err(e) => SupervisorMessage::SessionCompleted {
                    task_id: task_id_for_join,
                    result: Err(e.to_string()),
                    output,
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
                started_at: Instant::now(),
            },
        );
        self.session_models.insert(task_id.clone(), model_id);
        self.task_session_records
            .insert(task_id.clone(), session_record.id);
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
                    output: ParsedAgentOutput::default(),
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
                started_at: Instant::now(),
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
        let session_record_id = self.task_session_records.remove(&task_id);
        let goose_session_id = handle.session_id.clone();

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
            self.cleanup_worktree(&task_id, worktree_path).await;
        }
        let (tokens_in, tokens_out) = self.tokens_for_session(&goose_session_id).await;
        self.update_session_record(
            session_record_id.as_deref(),
            SessionStatus::Interrupted,
            tokens_in,
            tokens_out,
        )
        .await;
        self.transition_interrupted(
            &task_id,
            agent_type,
            "session interrupted by supervisor kill",
        )
        .await;

        Ok(())
    }

    fn remove_session(&mut self, task_id: &str) -> SessionClosure {
        let removed = self.sessions.remove(task_id);
        let goose_session_id = removed
            .as_ref()
            .map(|h| h.session_id.clone())
            .unwrap_or_else(|| format!("unknown-session-{task_id}"));
        self.decrement_capacity(task_id);
        SessionClosure {
            model_id: self.session_models.remove(task_id),
            agent_type: self
                .session_agent_types
                .remove(task_id)
                .unwrap_or(AgentType::Worker),
            goose_session_id,
            record_id: self.task_session_records.remove(task_id),
            worktree_path: removed.and_then(|h| h.worktree_path),
        }
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

    fn running_sessions_snapshot(&self) -> Vec<RunningSessionInfo> {
        let mut sessions: Vec<RunningSessionInfo> = self
            .sessions
            .iter()
            .map(|(task_id, handle)| self.session_snapshot(task_id, handle))
            .collect();
        sessions.sort_by(|a, b| a.task_id.cmp(&b.task_id));
        sessions
    }

    fn session_snapshot(&self, task_id: &str, handle: &GooseSessionHandle) -> RunningSessionInfo {
        let model_id = self
            .session_models
            .get(task_id)
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());
        RunningSessionInfo {
            task_id: task_id.to_string(),
            model_id,
            session_id: handle.session_id.clone(),
            duration_seconds: handle.started_at.elapsed().as_secs(),
            worktree_path: handle
                .worktree_path
                .as_ref()
                .map(|path| path.display().to_string()),
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

    async fn commit_final_work_if_needed(
        &self,
        task_id: &str,
        worktree_path: &Path,
    ) -> Result<(), String> {
        let git = self
            .app_state
            .git_actor(worktree_path)
            .await
            .map_err(|e| format!("failed to open git actor for worktree: {e}"))?;

        let status = git
            .run_command(vec!["status".into(), "--porcelain".into()])
            .await
            .map_err(|e| format!("failed to read worktree status: {e}"))?;

        if status.stdout.trim().is_empty() {
            return Ok(());
        }

        git.run_command(vec!["add".into(), "-A".into()])
            .await
            .map_err(|e| format!("failed to stage completed session changes: {e}"))?;

        let message = format!("WIP: auto-save completed session {task_id}");
        git.run_command(vec![
            "commit".into(),
            "--no-verify".into(),
            "-m".into(),
            message,
        ])
        .await
        .map_err(|e| format!("failed to commit completed session changes: {e}"))?;

        Ok(())
    }

    async fn cleanup_worktree(&self, task_id: &str, worktree_path: &Path) {
        let task = match self.load_task(task_id).await {
            Ok(task) => task,
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "failed to load task for worktree cleanup");
                return;
            }
        };

        let Some(project_path) = self.project_path_for_id(&task.project_id).await else {
            tracing::warn!(task_id = %task_id, "project path not found for worktree cleanup");
            return;
        };

        let git = match self.app_state.git_actor(Path::new(&project_path)).await {
            Ok(git) => git,
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "failed to open git actor for worktree cleanup");
                return;
            }
        };

        if let Err(e) = git.remove_worktree(worktree_path).await {
            tracing::warn!(task_id = %task_id, error = %e, "failed to remove worktree; attempting filesystem cleanup");
            if worktree_path.exists()
                && let Err(remove_err) = std::fs::remove_dir_all(worktree_path)
            {
                tracing::warn!(task_id = %task_id, error = %remove_err, "failed to remove worktree directory");
            }
        }
    }

    async fn transition_interrupted(&self, task_id: &str, agent_type: AgentType, reason: &str) {
        let action = match agent_type {
            AgentType::Worker | AgentType::ConflictResolver => Some(TransitionAction::Release),
            AgentType::TaskReviewer => Some(TransitionAction::ReleaseTaskReview),
            AgentType::EpicReviewer => None,
        };

        let Some(action) = action else {
            return;
        };

        let repo =
            TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
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
        session: SessionClosure,
        result: Result<(), String>,
        output: ParsedAgentOutput,
    ) {
        let agent_type = session.agent_type;
        let repo =
            TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());

        if let Some(model_id) = session.model_id.as_deref() {
            match &result {
                Ok(()) => self.app_state.health_tracker().record_success(model_id),
                Err(_) => self.app_state.health_tracker().record_failure(model_id),
            }
            self.app_state.persist_model_health_state().await;
        }

        let (tokens_in, tokens_out) = self.tokens_for_session(&session.goose_session_id).await;
        let session_status = if result.is_ok() {
            SessionStatus::Completed
        } else {
            SessionStatus::Failed
        };
        self.update_session_record(
            session.record_id.as_deref(),
            session_status,
            tokens_in,
            tokens_out,
        )
        .await;

        if let Some(worktree_path) = session.worktree_path.as_ref() {
            let should_persist_final = result.is_ok()
                && matches!(agent_type, AgentType::Worker | AgentType::ConflictResolver)
                && matches!(output.worker_signal, Some(WorkerSignal::Done));

            let mut allow_cleanup = true;
            if should_persist_final
                && let Err(e) = self
                    .commit_final_work_if_needed(task_id, worktree_path)
                    .await
            {
                allow_cleanup = false;
                tracing::warn!(
                    task_id = %task_id,
                    worktree_path = %worktree_path.display(),
                    error = %e,
                    "failed to persist completed session changes; preserving worktree for recovery"
                );
            }

            if allow_cleanup {
                self.cleanup_worktree(task_id, worktree_path).await;
            }
        }

        if let Some(feedback) = output.reviewer_feedback.as_deref() {
            let payload = serde_json::json!({ "body": feedback }).to_string();
            if let Err(e) = repo
                .log_activity(
                    Some(task_id),
                    "agent-supervisor",
                    "task_reviewer",
                    "comment",
                    &payload,
                )
                .await
            {
                tracing::warn!(task_id = %task_id, error = %e, "failed to store reviewer feedback comment");
            }
        }

        if let Err(reason) = &result {
            let payload = serde_json::json!({
                "error": reason,
                "agent_type": agent_type.as_str(),
            })
            .to_string();
            if let Err(e) = repo
                .log_activity(
                    Some(task_id),
                    "agent-supervisor",
                    "system",
                    "session_error",
                    &payload,
                )
                .await
            {
                tracing::warn!(task_id = %task_id, error = %e, "failed to store session error activity");
            }
        }

        if result.is_ok()
            && let Some(reason) = output.runtime_error.as_deref()
        {
            let payload = serde_json::json!({
                "error": reason,
                "agent_type": agent_type.as_str(),
            })
            .to_string();
            if let Err(e) = repo
                .log_activity(
                    Some(task_id),
                    "agent-supervisor",
                    "system",
                    "session_error",
                    &payload,
                )
                .await
            {
                tracing::warn!(task_id = %task_id, error = %e, "failed to store session error activity");
            }
        }

        let epic_error = result.as_ref().err().cloned();
        let transition = match result {
            Ok(()) => self.success_transition(task_id, agent_type, &output).await,
            Err(reason) => match agent_type {
                AgentType::Worker | AgentType::ConflictResolver => {
                    Some((TransitionAction::Release, Some(reason)))
                }
                AgentType::TaskReviewer => {
                    Some((TransitionAction::ReleaseTaskReview, Some(reason)))
                }
                AgentType::EpicReviewer => None,
            },
        };

        if agent_type == AgentType::EpicReviewer {
            self.finalize_epic_batch(task_id, &output, epic_error.as_deref())
                .await;
        }

        if let Some((action, reason)) = transition {
            if let Err(e) = repo
                .transition(
                    task_id,
                    action,
                    "agent-supervisor",
                    "system",
                    reason.as_deref(),
                    None,
                )
                .await
            {
                tracing::warn!(task_id = %task_id, error = %e, "failed to transition task after session");
            }
        }

        // Capacity has just been released by this session completion. Trigger an
        // immediate dispatch pass for the same project so the next ready task
        // starts without waiting for the coordinator interval tick.
        if let Ok(task) = self.load_task(task_id).await
            && let Some(coordinator) = self.app_state.coordinator().await
        {
            let _ = coordinator
                .trigger_dispatch_for_project(&task.project_id)
                .await;
        }
    }

    async fn success_transition(
        &self,
        task_id: &str,
        agent_type: AgentType,
        output: &ParsedAgentOutput,
    ) -> Option<(TransitionAction, Option<String>)> {
        match agent_type {
            AgentType::Worker | AgentType::ConflictResolver => match output.worker_signal {
                Some(WorkerSignal::Done) => Some((TransitionAction::SubmitTaskReview, None)),
                Some(WorkerSignal::Blocked) => Some((
                    TransitionAction::Block,
                    Some(
                        output
                            .worker_reason
                            .clone()
                            .unwrap_or_else(|| "worker reported BLOCKED".to_string()),
                    ),
                )),
                Some(WorkerSignal::Progress) => Some((
                    TransitionAction::Release,
                    Some("worker session ended with PROGRESS signal".to_string()),
                )),
                None => {
                    let reason = output.runtime_error.clone().unwrap_or_else(|| {
                        "worker session completed without DONE/BLOCKED marker".to_string()
                    });
                    tracing::warn!(reason = %reason, "worker session completed without structured result marker");
                    Some((TransitionAction::Release, Some(reason)))
                }
            },
            AgentType::TaskReviewer => match output.reviewer_verdict {
                Some(ReviewerVerdict::Verified) => self.merge_after_task_review(task_id).await,
                Some(ReviewerVerdict::Reopen) => Some((
                    TransitionAction::TaskReviewReject,
                    Some(
                        output
                            .reviewer_feedback
                            .clone()
                            .unwrap_or_else(|| "reviewer requested REOPEN".to_string()),
                    ),
                )),
                Some(ReviewerVerdict::Cancel) => Some((
                    TransitionAction::ReleaseTaskReview,
                    Some(
                        output
                            .reviewer_feedback
                            .clone()
                            .unwrap_or_else(|| "reviewer returned CANCEL".to_string()),
                    ),
                )),
                None => {
                    tracing::warn!("task reviewer session completed without REVIEW_RESULT marker");
                    Some((
                        TransitionAction::ReleaseTaskReview,
                        Some("reviewer session completed without REVIEW_RESULT marker".to_string()),
                    ))
                }
            },
            AgentType::EpicReviewer => match output.epic_verdict {
                Some(EpicReviewVerdict::Clean) => None,
                Some(EpicReviewVerdict::IssuesFound) => None,
                None => {
                    tracing::warn!(
                        "epic reviewer session completed without EPIC_REVIEW_RESULT marker"
                    );
                    None
                }
            },
        }
    }

    async fn tokens_for_session(&self, goose_session_id: &str) -> (i64, i64) {
        let session = self
            .session_manager
            .get_session(goose_session_id, false)
            .await;
        let Ok(session) = session else {
            if let Some(tokens) = Self::tokens_from_goose_sqlite(goose_session_id).await {
                return tokens;
            }
            return (0, 0);
        };

        let tokens_in = session
            .accumulated_input_tokens
            .or(session.input_tokens)
            .unwrap_or(0) as i64;
        let tokens_out = session
            .accumulated_output_tokens
            .or(session.output_tokens)
            .unwrap_or(0) as i64;

        if tokens_in == 0
            && tokens_out == 0
            && let Some(tokens) = Self::tokens_from_goose_sqlite(goose_session_id).await
        {
            return tokens;
        }

        (tokens_in, tokens_out)
    }

    async fn tokens_from_goose_sqlite(goose_session_id: &str) -> Option<(i64, i64)> {
        for db_path in Self::goose_session_db_candidates() {
            let Some(tokens) = Self::tokens_from_goose_sqlite_at(&db_path, goose_session_id).await
            else {
                continue;
            };
            return Some(tokens);
        }

        None
    }

    fn goose_session_db_candidates() -> Vec<PathBuf> {
        let mut candidates = Vec::new();

        if let Ok(root) = std::env::var("GOOSE_PATH_ROOT") {
            let root = PathBuf::from(root);
            candidates.push(root.join("data").join("sessions").join("sessions.db"));
        }

        if let Some(home) = dirs::home_dir() {
            candidates.push(home.join(".djinn").join("sessions").join("sessions.db"));
            candidates.push(
                home.join(".djinn")
                    .join("sessions")
                    .join("sessions")
                    .join("sessions.db"),
            );
        }

        candidates
    }

    async fn tokens_from_goose_sqlite_at(
        db_path: &Path,
        goose_session_id: &str,
    ) -> Option<(i64, i64)> {
        if !db_path.exists() {
            return None;
        }

        let options = SqliteConnectOptions::new()
            .filename(db_path)
            .read_only(true)
            .create_if_missing(false)
            .busy_timeout(Duration::from_secs(1));

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .ok()?;

        let row = sqlx::query_as::<_, (i64, i64)>(
            "SELECT COALESCE(accumulated_input_tokens, input_tokens, 0), COALESCE(accumulated_output_tokens, output_tokens, 0) FROM sessions WHERE id = ?1",
        )
        .bind(goose_session_id)
        .fetch_optional(&pool)
        .await
        .ok()??;

        Some(row)
    }

    async fn update_session_record(
        &self,
        record_id: Option<&str>,
        status: SessionStatus,
        tokens_in: i64,
        tokens_out: i64,
    ) {
        let Some(record_id) = record_id else {
            return;
        };

        let repo =
            SessionRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        if let Err(e) = repo.update(record_id, status, tokens_in, tokens_out).await {
            tracing::warn!(record_id = %record_id, error = %e, "failed to update session record");
        }
    }

    async fn transition_start(
        &self,
        task: &Task,
        agent_type: AgentType,
    ) -> Result<(), SupervisorError> {
        let action = match (agent_type, task.status.as_str()) {
            (AgentType::Worker, "open") | (AgentType::ConflictResolver, "open") => {
                Some(TransitionAction::Start)
            }
            (AgentType::TaskReviewer, "needs_task_review") => {
                Some(TransitionAction::TaskReviewStart)
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

    async fn conflict_context_for_dispatch(&self, task_id: &str) -> Option<MergeConflictMetadata> {
        let repo =
            TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let activity = repo.list_activity(task_id).await.ok()?;
        let last_status = activity
            .iter()
            .rev()
            .find(|e| e.event_type == "status_changed")?;
        let payload: serde_json::Value = serde_json::from_str(&last_status.payload).ok()?;
        let from_status = payload
            .get("from_status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let to_status = payload
            .get("to_status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if from_status != "in_task_review" || to_status != "open" {
            return None;
        }
        let reason = payload
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        Self::parse_conflict_metadata(reason)
    }

    async fn active_epic_batch_for_task(&self, task_id: &str) -> Option<String> {
        let repo = EpicReviewBatchRepository::new(
            self.app_state.db().clone(),
            self.app_state.events().clone(),
        );
        repo.active_batch_for_task(task_id)
            .await
            .ok()
            .flatten()
            .map(|b| b.id)
    }

    fn parse_conflict_metadata(reason: &str) -> Option<MergeConflictMetadata> {
        let raw = reason.strip_prefix(MERGE_CONFLICT_PREFIX)?;
        serde_json::from_str(raw).ok()
    }

    async fn merge_after_task_review(
        &self,
        task_id: &str,
    ) -> Option<(TransitionAction, Option<String>)> {
        let repo =
            TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let task = match repo.get(task_id).await {
            Ok(Some(task)) => task,
            Ok(None) => {
                return Some((
                    TransitionAction::ReleaseTaskReview,
                    Some("task missing during post-review merge".to_string()),
                ));
            }
            Err(e) => {
                return Some((
                    TransitionAction::ReleaseTaskReview,
                    Some(format!("failed to load task for merge: {e}")),
                ));
            }
        };

        let project_dir = self
            .project_path_for_id(&task.project_id)
            .await
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let git = match self.app_state.git_actor(&project_dir).await {
            Ok(git) => git,
            Err(e) => {
                return Some((
                    TransitionAction::ReleaseTaskReview,
                    Some(format!("failed to open git actor for merge: {e}")),
                ));
            }
        };

        let base_branch = format!("task/{}", task.short_id);
        let merge_target = self.default_target_branch(&task.project_id).await;
        let commit_type = if task.issue_type == "task" {
            "chore"
        } else {
            "feat"
        };
        let message = format!("{}({}): {}", commit_type, task.short_id, task.title);

        match git
            .squash_merge(&base_branch, &merge_target, &message)
            .await
        {
            Ok(result) => {
                if let Err(e) = git.delete_branch(&base_branch).await {
                    tracing::warn!(
                        task_id = %task.short_id,
                        branch = %base_branch,
                        error = %e,
                        "failed to delete task branch after successful merge"
                    );
                }
                if let Err(e) = repo.set_merge_commit_sha(task_id, &result.commit_sha).await {
                    return Some((
                        TransitionAction::ReleaseTaskReview,
                        Some(format!("merged but failed to store merge SHA: {e}")),
                    ));
                }
                Some((TransitionAction::TaskReviewApprove, None))
            }
            Err(GitError::MergeConflict { files, .. }) => {
                let metadata = MergeConflictMetadata {
                    conflicting_files: files,
                    base_branch,
                    merge_target,
                };
                let reason = match serde_json::to_string(&metadata) {
                    Ok(v) => format!("{MERGE_CONFLICT_PREFIX}{v}"),
                    Err(_) => format!("{MERGE_CONFLICT_PREFIX}{{}}"),
                };
                let payload = serde_json::to_string(&metadata).unwrap_or_else(|_| "{}".to_string());
                let _ = repo
                    .log_activity(
                        Some(task_id),
                        "agent-supervisor",
                        "system",
                        "merge_conflict",
                        &payload,
                    )
                    .await;
                Some((TransitionAction::TaskReviewRejectConflict, Some(reason)))
            }
            Err(e) => Some((
                TransitionAction::ReleaseTaskReview,
                Some(format!("post-review squash merge failed: {e}")),
            )),
        }
    }

    fn missing_required_marker(agent_type: AgentType, output: &ParsedAgentOutput) -> bool {
        match agent_type {
            AgentType::Worker | AgentType::ConflictResolver => output.worker_signal.is_none(),
            AgentType::TaskReviewer => output.reviewer_verdict.is_none(),
            AgentType::EpicReviewer => output.epic_verdict.is_none(),
        }
    }

    fn missing_marker_nudge(
        agent_type: AgentType,
        output: &ParsedAgentOutput,
        nudge_count: u8,
    ) -> Option<&'static str> {
        const MAX_NUDGES: u8 = 2;
        if nudge_count >= MAX_NUDGES {
            return None;
        }
        if !Self::missing_required_marker(agent_type, output) {
            return None;
        }

        match agent_type {
            AgentType::Worker | AgentType::ConflictResolver => Some(
                "Emit exactly one final marker now: WORKER_RESULT: DONE | PROGRESS: <what remains> | BLOCKED: <specific blocker>. Do not continue analysis.",
            ),
            AgentType::TaskReviewer => Some(
                "Emit exactly one final marker now: REVIEW_RESULT: VERIFIED | REOPEN | CANCEL. If REOPEN or CANCEL, also emit FEEDBACK: <what is missing>. Do not continue analysis.",
            ),
            AgentType::EpicReviewer => Some(
                "Emit exactly one final marker now: EPIC_REVIEW_RESULT: CLEAN | ISSUES_FOUND. If ISSUES_FOUND, include concise actionable findings and create follow-up tasks in this epic before finishing.",
            ),
        }
    }

    async fn finalize_epic_batch(
        &self,
        task_id: &str,
        output: &ParsedAgentOutput,
        error_reason: Option<&str>,
    ) {
        let Some(batch_id) = self.active_epic_batch_for_task(task_id).await else {
            return;
        };
        let task_repo =
            TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let Some(task) = task_repo.get(task_id).await.ok().flatten() else {
            return;
        };
        let Some(epic_id) = task.epic_id.as_deref() else {
            return;
        };

        let batch_repo = EpicReviewBatchRepository::new(
            self.app_state.db().clone(),
            self.app_state.events().clone(),
        );
        let epic_repo =
            EpicRepository::new(self.app_state.db().clone(), self.app_state.events().clone());

        match output.epic_verdict {
            Some(EpicReviewVerdict::Clean) => {
                if let Err(e) = batch_repo.mark_clean(&batch_id).await {
                    tracing::warn!(batch_id = %batch_id, error = %e, "failed to mark epic review batch clean");
                    return;
                }

                let tasks = match task_repo.list_by_epic(epic_id).await {
                    Ok(tasks) => tasks,
                    Err(e) => {
                        tracing::warn!(epic_id = %epic_id, error = %e, "failed to list epic tasks after clean review");
                        return;
                    }
                };
                if tasks.iter().all(|t| t.status == "closed") {
                    let _ = epic_repo.close(epic_id).await;
                }
            }
            Some(EpicReviewVerdict::IssuesFound) => {
                let verdict = "epic reviewer reported EPIC_REVIEW_RESULT: ISSUES_FOUND";
                let _ = batch_repo.mark_issues_found(&batch_id, verdict).await;
                if let Ok(Some(epic)) = epic_repo.get(epic_id).await
                    && epic.status == "in_review"
                {
                    let _ = epic_repo.reopen(epic_id).await;
                }
            }
            None => {
                let verdict = error_reason
                    .unwrap_or("epic reviewer ended without required EPIC_REVIEW_RESULT marker");
                let _ = batch_repo.mark_issues_found(&batch_id, verdict).await;
                if let Ok(Some(epic)) = epic_repo.get(epic_id).await
                    && epic.status == "in_review"
                {
                    let _ = epic_repo.reopen(epic_id).await;
                }
            }
        }
    }

    fn agent_type_for_task(&self, task: &Task, has_conflict_context: bool) -> AgentType {
        match task.status.as_str() {
            "needs_task_review" | "in_task_review" => AgentType::TaskReviewer,
            "open" if has_conflict_context => AgentType::ConflictResolver,
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

    async fn goose_provider_entries(
        &self,
    ) -> Vec<(ProviderMetadata, goose::providers::base::ProviderType)> {
        providers::providers().await
    }

    fn canonical_provider_id(id: &str) -> String {
        id.chars()
            .filter(char::is_ascii_alphanumeric)
            .flat_map(char::to_lowercase)
            .collect()
    }

    fn resolve_provider_alias(
        provider_id: &str,
        entries: &[(ProviderMetadata, goose::providers::base::ProviderType)],
    ) -> Option<String> {
        if let Some((meta, _)) = entries.iter().find(|(meta, _)| meta.name == provider_id) {
            return Some(meta.name.clone());
        }

        let canonical = Self::canonical_provider_id(provider_id);
        entries
            .iter()
            .find(|(meta, _)| Self::canonical_provider_id(&meta.name) == canonical)
            .map(|(meta, _)| meta.name.clone())
    }

    async fn resolve_goose_provider_id(&self, provider_id: &str) -> String {
        let entries = self.goose_provider_entries().await;
        Self::resolve_provider_alias(provider_id, &entries)
            .unwrap_or_else(|| provider_id.to_string())
    }

    async fn provider_supports_oauth(&self, provider_id: &str) -> Option<bool> {
        let entries = self.goose_provider_entries().await;
        let resolved = Self::resolve_provider_alias(provider_id, &entries)?;
        entries
            .iter()
            .find(|(meta, _)| meta.name == resolved)
            .map(|(meta, _)| meta.config_keys.iter().any(|k| k.oauth_flow))
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
        let target_branch = self.default_target_branch(&task.project_id).await;
        let git = self
            .app_state
            .git_actor(project_dir)
            .await
            .map_err(|e| SupervisorError::Goose(e.to_string()))?;

        let stale_worktree_path = project_dir
            .join(".djinn")
            .join("worktrees")
            .join(&task.short_id);
        let _ = git.remove_worktree(&stale_worktree_path).await;
        if stale_worktree_path.exists() {
            let _ = std::fs::remove_dir_all(&stale_worktree_path);
        }

        let branch_exists = match git
            .run_command(vec![
                "show-ref".into(),
                "--verify".into(),
                "--quiet".into(),
                format!("refs/heads/{branch}"),
            ])
            .await
        {
            Ok(_) => true,
            Err(GitError::CommandFailed { code: 1, .. }) => false,
            Err(e) => return Err(SupervisorError::Goose(e.to_string())),
        };

        if !branch_exists {
            git.create_branch(&task.short_id, &target_branch)
                .await
                .map_err(|e| SupervisorError::Goose(e.to_string()))?;
        } else {
            self.try_rebase_existing_task_branch(project_dir, &branch, &target_branch)
                .await;
        }

        git.create_worktree(&task.short_id, &branch)
            .await
            .map_err(|e| SupervisorError::Goose(e.to_string()))
    }

    async fn try_rebase_existing_task_branch(
        &self,
        project_dir: &Path,
        branch: &str,
        target_branch: &str,
    ) {
        let git = match self.app_state.git_actor(project_dir).await {
            Ok(git) => git,
            Err(e) => {
                tracing::warn!(branch = %branch, error = %e, "failed to open git actor for branch sync");
                return;
            }
        };

        let _ = git
            .run_command(vec![
                "fetch".into(),
                "origin".into(),
                target_branch.to_string(),
            ])
            .await;

        let upstream = match git
            .run_command(vec![
                "rev-parse".into(),
                "--verify".into(),
                "--quiet".into(),
                format!("refs/remotes/origin/{target_branch}"),
            ])
            .await
        {
            Ok(_) => format!("origin/{target_branch}"),
            Err(GitError::CommandFailed { code: 1, .. }) => target_branch.to_string(),
            Err(e) => {
                tracing::warn!(
                    branch = %branch,
                    target_branch = %target_branch,
                    error = %e,
                    "failed to resolve upstream for branch sync"
                );
                return;
            }
        };

        let sync_name = format!(".sync-{}", branch.replace('/', "-"));
        let sync_worktree_path = project_dir.join(".djinn").join("worktrees").join(sync_name);
        let _ = git.remove_worktree(&sync_worktree_path).await;
        if sync_worktree_path.exists() {
            let _ = std::fs::remove_dir_all(&sync_worktree_path);
        }

        let sync_path = sync_worktree_path.to_str().unwrap_or_default().to_string();
        if let Err(e) = git
            .run_command(vec![
                "worktree".into(),
                "add".into(),
                "--detach".into(),
                sync_path.clone(),
                branch.to_string(),
            ])
            .await
        {
            tracing::warn!(branch = %branch, error = %e, "failed to create sync worktree for branch rebase");
            return;
        }

        let sync_git = match self.app_state.git_actor(&sync_worktree_path).await {
            Ok(git) => git,
            Err(e) => {
                tracing::warn!(branch = %branch, error = %e, "failed to open sync worktree git actor");
                let _ = git.remove_worktree(&sync_worktree_path).await;
                if sync_worktree_path.exists() {
                    let _ = std::fs::remove_dir_all(&sync_worktree_path);
                }
                return;
            }
        };

        match sync_git
            .run_command(vec!["rebase".into(), upstream.clone()])
            .await
        {
            Ok(_) => {
                tracing::info!(branch = %branch, upstream = %upstream, "rebased existing task branch before dispatch");
            }
            Err(GitError::CommandFailed { .. }) => {
                let _ = sync_git
                    .run_command(vec!["rebase".into(), "--abort".into()])
                    .await;
                tracing::warn!(
                    branch = %branch,
                    upstream = %upstream,
                    "existing task branch could not be rebased cleanly; continuing without rebase"
                );
            }
            Err(e) => {
                tracing::warn!(
                    branch = %branch,
                    upstream = %upstream,
                    error = %e,
                    "failed to rebase existing task branch"
                );
            }
        }

        let _ = git.remove_worktree(&sync_worktree_path).await;
        if sync_worktree_path.exists() {
            let _ = std::fs::remove_dir_all(&sync_worktree_path);
        }
    }

    async fn default_target_branch(&self, project_id: &str) -> String {
        let repo = GitSettingsRepository::new(
            self.app_state.db().clone(),
            self.app_state.events().clone(),
        );
        if let Ok(settings) = repo.get(project_id).await {
            return settings.target_branch;
        }

        "main".to_string()
    }

    async fn project_path_for_id(&self, project_id: &str) -> Option<String> {
        sqlx::query_scalar::<_, String>("SELECT path FROM projects WHERE id = ?1")
            .bind(project_id)
            .fetch_optional(self.app_state.db().pool())
            .await
            .ok()
            .flatten()
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
            goose_session_id: String,
            session_record_id: Option<String>,
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
                session_record_id: self.task_session_records.remove(&task_id),
                task_id,
                join: handle.join,
                worktree_path: handle.worktree_path.take(),
                goose_session_id: handle.session_id,
            });
        }

        let deadline = Instant::now() + Duration::from_secs(30);
        for item in &mut pending {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                item.join.abort();
                continue;
            }

            if tokio::time::timeout(remaining, &mut item.join)
                .await
                .is_err()
            {
                tracing::warn!(task_id = %item.task_id, "session join timed out during shutdown; aborting");
                item.join.abort();
            }
        }

        for item in pending {
            self.decrement_capacity_for_model(item.model_id.as_deref());
            if let Some(worktree_path) = item.worktree_path.as_ref() {
                self.commit_wip_if_needed(&item.task_id, worktree_path)
                    .await;
                self.cleanup_worktree(&item.task_id, worktree_path).await;
            }
            let (tokens_in, tokens_out) = self.tokens_for_session(&item.goose_session_id).await;
            self.update_session_record(
                item.session_record_id.as_deref(),
                SessionStatus::Interrupted,
                tokens_in,
                tokens_out,
            )
            .await;
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

    pub async fn dispatch(
        &self,
        task_id: &str,
        project_path: &str,
        model_id: &str,
    ) -> Result<(), SupervisorError> {
        self.request(|tx| SupervisorMessage::Dispatch {
            task_id: task_id.to_owned(),
            project_path: project_path.to_owned(),
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

    pub async fn session_for_task(
        &self,
        task_id: &str,
    ) -> Result<Option<RunningSessionInfo>, SupervisorError> {
        self.request(|tx| SupervisorMessage::GetSessionForTask {
            task_id: task_id.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn interrupt_all(&self, reason: &str) -> Result<(), SupervisorError> {
        self.request(|tx| SupervisorMessage::InterruptAll {
            reason: reason.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn update_max_sessions(&self, max: u32) -> Result<(), SupervisorError> {
        self.request(|tx| SupervisorMessage::UpdateMaxSessions {
            max: max.max(1),
            respond_to: tx,
        })
        .await
    }

    pub async fn update_session_limits(
        &self,
        max_sessions: HashMap<String, u32>,
        default_max: u32,
    ) -> Result<(), SupervisorError> {
        self.request(|tx| SupervisorMessage::UpdateSessionLimits {
            max_sessions,
            default_max: default_max.max(1),
            respond_to: tx,
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
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
        let project_path = temp.path().to_str().unwrap();

        assert!(!supervisor.has_session("task-1").await.unwrap());
        supervisor
            .dispatch("task-1", project_path, "test/mock")
            .await
            .unwrap();
        assert!(supervisor.has_session("task-1").await.unwrap());

        supervisor.kill_session("task-1").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert!(!supervisor.has_session("task-1").await.unwrap());
    }

    #[tokio::test]
    async fn enforces_per_model_capacity() {
        let temp = tempfile::tempdir().unwrap();
        let supervisor = spawn_supervisor(&temp);
        let project_path = temp.path().to_str().unwrap();

        supervisor
            .dispatch("task-1", project_path, "test/mock")
            .await
            .unwrap();
        let err = supervisor
            .dispatch("task-2", project_path, "test/mock")
            .await
            .unwrap_err();
        assert!(matches!(err, SupervisorError::ModelAtCapacity { .. }));

        supervisor.kill_session("task-1").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        supervisor
            .dispatch("task-2", project_path, "test/mock")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn status_reports_capacity_and_active_count() {
        let temp = tempfile::tempdir().unwrap();
        let supervisor = spawn_supervisor(&temp);
        let project_path = temp.path().to_str().unwrap();

        supervisor
            .dispatch("task-1", project_path, "test/mock")
            .await
            .unwrap();
        let status = supervisor.get_status().await.unwrap();
        assert_eq!(status.active_sessions, 1);
        let model = status.capacity.get("test/mock").unwrap();
        assert_eq!(model.active, 1);
        assert_eq!(model.max, 1);
    }

    #[tokio::test]
    async fn applies_per_model_session_limits_from_settings() {
        let temp = tempfile::tempdir().unwrap();
        let supervisor = spawn_supervisor(&temp);
        let project_path = temp.path().to_str().unwrap();

        let mut limits = HashMap::new();
        limits.insert("test/mock".to_string(), 4);
        limits.insert("synthetic/hf:nvidia/Kimi-K2.5-NVFP4".to_string(), 2);
        supervisor.update_session_limits(limits, 1).await.unwrap();

        for task_id in ["task-1", "task-2", "task-3", "task-4"] {
            supervisor
                .dispatch(task_id, project_path, "test/mock")
                .await
                .unwrap();
        }

        let err = supervisor
            .dispatch("task-5", project_path, "test/mock")
            .await
            .unwrap_err();
        assert!(matches!(err, SupervisorError::ModelAtCapacity { .. }));

        let status = supervisor.get_status().await.unwrap();
        let mock = status.capacity.get("test/mock").unwrap();
        assert_eq!(mock.max, 4);
        assert_eq!(mock.active, 4);

        let kimi = status
            .capacity
            .get("synthetic/hf:nvidia/Kimi-K2.5-NVFP4")
            .unwrap();
        assert_eq!(kimi.max, 2);
        assert_eq!(kimi.active, 0);
    }

    #[tokio::test]
    async fn interrupt_all_cancels_active_mock_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let supervisor = spawn_supervisor(&temp);
        let project_path = temp.path().to_str().unwrap();

        supervisor
            .dispatch("task-1", project_path, "test/mock")
            .await
            .unwrap();
        assert!(supervisor.has_session("task-1").await.unwrap());

        supervisor
            .interrupt_all("session interrupted by test")
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        assert!(!supervisor.has_session("task-1").await.unwrap());
    }

    #[tokio::test]
    async fn sqlite_fallback_reads_accumulated_token_counts() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("sessions.db");

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&db_path)
                    .create_if_missing(true),
            )
            .await
            .unwrap();

        sqlx::query(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, input_tokens INTEGER, output_tokens INTEGER, accumulated_input_tokens INTEGER, accumulated_output_tokens INTEGER)",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO sessions (id, input_tokens, output_tokens, accumulated_input_tokens, accumulated_output_tokens) VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind("session-123")
        .bind(3_i64)
        .bind(5_i64)
        .bind(13_i64)
        .bind(21_i64)
        .execute(&pool)
        .await
        .unwrap();

        let tokens = AgentSupervisor::tokens_from_goose_sqlite_at(
            PathBuf::from(&db_path).as_path(),
            "session-123",
        )
        .await;

        assert_eq!(tokens, Some((13, 21)));
    }
}
