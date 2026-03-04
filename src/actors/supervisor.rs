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
use goose::conversation::message::{Message as GooseMessage, MessageContent};
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
use crate::commands::{CommandSpec, run_commands};
use crate::db::repositories::epic::EpicRepository;
use crate::db::repositories::epic_review_batch::EpicReviewBatchRepository;
use crate::db::repositories::git_settings::GitSettingsRepository;
use crate::db::repositories::project::ProjectRepository;
use crate::db::repositories::session::SessionRepository;
use crate::db::repositories::task::TaskRepository;
use crate::models::session::{SessionRecord, SessionStatus};
use crate::models::task::{Task, TransitionAction};
use crate::server::AppState;

const MERGE_CONFLICT_PREFIX: &str = "merge_conflict:";
const MERGE_VALIDATION_PREFIX: &str = "merge_validation_failed:";
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

/// Format a JSON array of `CommandSpec` objects into a markdown bullet list of names.
/// Returns `None` if the array is empty or cannot be parsed.
fn format_command_names(json: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct NameOnly {
        name: String,
    }
    let specs: Vec<NameOnly> = serde_json::from_str(json).unwrap_or_default();
    if specs.is_empty() {
        return None;
    }
    Some(
        specs
            .into_iter()
            .map(|s| format!("- `{}`", s.name))
            .collect::<Vec<_>>()
            .join("\n"),
    )
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

fn log_snippet(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    let mut out = String::new();
    for ch in trimmed.chars().take(max_chars) {
        out.push(ch);
    }
    if trimmed.chars().count() > max_chars {
        out.push_str("…");
    }
    if out.is_empty() {
        "<empty>".to_string()
    } else {
        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MergeConflictMetadata {
    conflicting_files: Vec<String>,
    base_branch: String,
    merge_target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MergeValidationFailureMetadata {
    base_branch: String,
    merge_target: String,
    command: String,
    cwd: String,
    exit_code: i32,
    stdout: String,
    stderr: String,
}

impl MergeValidationFailureMetadata {
    fn as_prompt_context(&self) -> String {
        format!(
            "Post-review merge validation failed. Fix the underlying issue, rerun verification, and commit the fix.\n\nmerge_base_branch: {}\nmerge_target_branch: {}\ncommand: git {}\nexit_code: {}\ncwd: {}\nstdout:\n{}\nstderr:\n{}",
            self.base_branch,
            self.merge_target,
            self.command,
            self.exit_code,
            self.cwd,
            self.stdout,
            self.stderr,
        )
    }
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
    PauseSession {
        task_id: String,
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
    GetStatus {
        respond_to: Reply<SupervisorStatus>,
    },
    GetSessionForTask {
        task_id: String,
        respond_to: Reply<Option<RunningSessionInfo>>,
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
    ResumeSession {
        task_id: String,
        model_id: String,
        goose_session_id: String,
        worktree_path: PathBuf,
        resume_prompt: String,
    },
}

struct SessionClosure {
    model_id: Option<String>,
    agent_type: AgentType,
    goose_session_id: String,
    record_id: Option<String>,
    worktree_path: Option<PathBuf>,
}

/// Spawns the agent reply loop task. Used by both fresh dispatch and session resume.
/// `reply_cancel` is a *clone* of the session's cancellation token (caller retains the original
/// for the GooseSessionHandle). `kickoff` is the first message sent to the agent.
#[allow(clippy::too_many_arguments)]
fn spawn_reply_task(
    agent: Arc<GooseAgent>,
    session_id: String,
    task_id: String,
    project_path: String,
    worktree_path: PathBuf,
    agent_type: AgentType,
    kickoff: GooseMessage,
    reply_cancel: CancellationToken,
    global_cancel: CancellationToken,
    sender: mpsc::Sender<SupervisorMessage>,
    app_state: AppState,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    tokio::spawn(async move {
        let mut output = ParsedAgentOutput::default();
        let run_result: anyhow::Result<()> = async {
            let mut pending_message = Some(kickoff);
            let mut saw_any_event = false;
            let mut saw_any_tool_use = false;
            let assistant_role = GooseMessage::assistant().role;
            let mut assistant_message_count: usize = 0;
            let mut assistant_fragments: Vec<String> = Vec::new();

            let push_fragment = |fragments: &mut Vec<String>, value: String| {
                const MAX_FRAGMENTS: usize = 12;
                let normalized = value.replace('\n', "\\n").trim().to_string();
                if normalized.is_empty() {
                    return;
                }
                let snippet: String = normalized.chars().take(160).collect();
                if fragments.len() >= MAX_FRAGMENTS {
                    fragments.remove(0);
                }
                fragments.push(snippet);
            };

            while let Some(next_message) = pending_message.take() {
                let env_diag = runtime_env_diagnostics(
                    &session_id,
                    &project_path,
                    &worktree_path,
                );
                tracing::info!(
                    task_id = %task_id,
                    session_id = %session_id,
                    worktree = %worktree_path.display(),
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
                        Some(reply_cancel.clone()),
                    )
                    .await
                    .map_err(|e| {
                        let diag = runtime_fs_diagnostics(&project_path, &worktree_path);
                        let env_diag = runtime_env_diagnostics(&session_id, &project_path, &worktree_path);
                        anyhow::anyhow!(
                            "agent reply init failed: display={} debug={:?}; {}; {}",
                            e, e, diag, env_diag
                        )
                    })?;

                let mut interrupted: Option<&'static str> = None;
                let mut saw_round_event = false;
                loop {
                    tokio::select! {
                        _ = reply_cancel.cancelled() => {
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
                                let diag = runtime_fs_diagnostics(&project_path, &worktree_path);
                                let env_diag = runtime_env_diagnostics(&session_id, &project_path, &worktree_path);
                                anyhow::anyhow!(
                                    "agent stream event failed: display={} debug={:?}; {}; {}",
                                    e, e, diag, env_diag
                                )
                            })?;
                            saw_any_event = true;
                            saw_round_event = true;
                            if let goose::agents::AgentEvent::Message(msg) = &evt
                                && msg.role == assistant_role
                            {
                                assistant_message_count += 1;
                                for content in &msg.content {
                                    match content {
                                        MessageContent::Text(text) => {
                                            output.ingest_text(&text.text);
                                            push_fragment(&mut assistant_fragments, format!("text:{}", text.text));
                                        }
                                        MessageContent::ToolRequest(req) => {
                                            push_fragment(&mut assistant_fragments, format!("tool_request:{}", req.id));
                                            saw_any_tool_use = true;
                                            output.ingest_text(&content.to_string());
                                        }
                                        MessageContent::FrontendToolRequest(req) => {
                                            push_fragment(&mut assistant_fragments, format!("frontend_tool_request:{}", req.id));
                                            saw_any_tool_use = true;
                                            output.ingest_text(&content.to_string());
                                        }
                                        _ => {
                                            push_fragment(&mut assistant_fragments, format!("{}", content));
                                            output.ingest_text(&content.to_string());
                                        }
                                    }
                                }
                            }
                            extension::handle_event(&app_state, &agent, &evt).await;
                        }
                    }
                }

                if let Some(reason) = interrupted {
                    return Err(anyhow::anyhow!(reason));
                }

                if !saw_round_event {
                    let diag = runtime_fs_diagnostics(&project_path, &worktree_path);
                    return Err(anyhow::anyhow!(
                        "agent stream ended without any events; {}",
                        diag
                    ));
                }
            }

            if !saw_any_event {
                let diag = runtime_fs_diagnostics(&project_path, &worktree_path);
                return Err(anyhow::anyhow!("agent session produced no events; {}", diag));
            }

            // Session complete — send a single nudge if the marker is missing
            if saw_any_tool_use && AgentSupervisor::missing_required_marker(agent_type, &output) {
                if let Some(nudge) = AgentSupervisor::missing_marker_nudge(agent_type, &output) {
                    tracing::info!(
                        task_id = %task_id,
                        agent_type = %agent_type.as_str(),
                        "Supervisor: session ended without required marker; sending post-session nudge"
                    );
                    let nudge_msg = GooseMessage::user().with_text(nudge);
                    let mut stream = agent
                        .reply(
                            nudge_msg,
                            GooseSessionConfig {
                                id: session_id.clone(),
                                schedule_id: None,
                                max_turns: Some(3),
                                retry_config: None,
                            },
                            Some(reply_cancel.clone()),
                        )
                        .await
                        .map_err(|e| anyhow::anyhow!("nudge reply init failed: {e}"))?;

                    while let Some(evt) = stream.next().await {
                        let evt = evt.map_err(|e| anyhow::anyhow!("nudge stream error: {e}"))?;
                        if let goose::agents::AgentEvent::Message(msg) = &evt
                            && msg.role == assistant_role
                        {
                            for content in &msg.content {
                                match content {
                                    MessageContent::Text(text) => {
                                        output.ingest_text(&text.text);
                                    }
                                    _ => {
                                        output.ingest_text(&content.to_string());
                                    }
                                }
                            }
                        }
                        extension::handle_event(&app_state, &agent, &evt).await;
                    }
                }
            }

            if let Some(last_assistant_text) =
                AgentSupervisor::last_assistant_text_from_goose_sqlite(&session_id).await
            {
                output.ingest_text(&last_assistant_text);
                tracing::info!(
                    task_id = %task_id,
                    agent_type = %agent_type.as_str(),
                    marker_present_after_persisted_check = !AgentSupervisor::missing_required_marker(agent_type, &output),
                    "Supervisor: parsed persisted last assistant message before marker decision"
                );
            }

            if AgentSupervisor::missing_required_marker(agent_type, &output) {
                tracing::warn!(
                    task_id = %task_id,
                    agent_type = %agent_type.as_str(),
                    saw_any_event,
                    saw_any_tool_use,
                    assistant_message_count,
                    worker_signal = ?output.worker_signal,
                    reviewer_verdict = ?output.reviewer_verdict,
                    epic_verdict = ?output.epic_verdict,
                    runtime_error = ?output.runtime_error,
                    reviewer_feedback = ?output.reviewer_feedback,
                    assistant_fragments = ?assistant_fragments,
                    "Supervisor: required marker missing at session end"
                );
                let reason = if !saw_any_tool_use {
                    match agent_type {
                        AgentType::Worker | AgentType::ConflictResolver => "worker ended without any tool use (provider error?)",
                        AgentType::TaskReviewer => "task reviewer ended without any tool use (provider error?)",
                        AgentType::EpicReviewer => "epic reviewer ended without any tool use (provider error?)",
                    }
                } else {
                    match agent_type {
                        AgentType::Worker | AgentType::ConflictResolver => "worker ended without WORKER_RESULT marker",
                        AgentType::TaskReviewer => "task reviewer ended without REVIEW_RESULT marker",
                        AgentType::EpicReviewer => "epic reviewer ended without EPIC_REVIEW_RESULT marker",
                    }
                };
                return Err(anyhow::anyhow!(reason));
            }

            Ok(())
        }
        .await;

        let msg = match &run_result {
            Ok(()) => SupervisorMessage::SessionCompleted {
                task_id: task_id.clone(),
                result: Ok(()),
                output,
            },
            Err(e) => SupervisorMessage::SessionCompleted {
                task_id: task_id.clone(),
                result: Err(e.to_string()),
                output,
            },
        };
        let _ = sender.send(msg).await;

        run_result
    })
}

struct AgentSupervisor {
    receiver: mpsc::Receiver<SupervisorMessage>,
    sessions: HashMap<String, GooseSessionHandle>,
    capacity: HashMap<String, ModelCapacity>,
    session_models: HashMap<String, String>,
    session_agent_types: HashMap<String, AgentType>,
    session_projects: HashMap<String, String>,
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
            session_projects: HashMap::new(),
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
            SupervisorMessage::PauseSession {
                task_id,
                respond_to,
            } => {
                let _ = respond_to.send(self.pause_session(task_id).await);
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
            SupervisorMessage::InterruptProject {
                project_id,
                reason,
                respond_to,
            } => {
                self.interrupt_project_sessions(&project_id, &reason).await;
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
                    tracing::info!(task_id = %task_id, "Supervisor: ignoring completion for interrupted session");
                    return;
                }
                tracing::info!(
                    task_id = %task_id,
                    result = if result.is_ok() { "ok" } else { "error" },
                    "Supervisor: session completion received"
                );
                let session = self.remove_session(&task_id);
                self.handle_session_result(&task_id, session, result, output)
                    .await;
            }
            SupervisorMessage::ResumeSession {
                task_id,
                model_id,
                goose_session_id,
                worktree_path,
                resume_prompt,
            } => {
                if let Err(e) = self
                    .dispatch_resume(task_id, model_id, goose_session_id, worktree_path, resume_prompt)
                    .await
                {
                    tracing::warn!(error = %e, "Supervisor: failed to dispatch resume session after verification failure");
                }
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

        // Check for a paused session — resume it instead of starting fresh.
        if let Some(paused) = self.find_paused_session_record(&task_id).await {
            let context = self.resume_context_for_task(&task_id).await;
            return self.resume_paused_session(task_id, project_path, model_id, paused, context).await;
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
        let merge_validation_ctx = self.merge_validation_context_for_dispatch(&task.id).await;
        let agent_type = if active_batch.is_some() {
            AgentType::EpicReviewer
        } else {
            self.agent_type_for_task(&task, conflict_ctx.is_some())
        };

        tracing::info!(
            task_id = %task.short_id,
            task_uuid = %task.id,
            project_id = %task.project_id,
            model_id = %model_id,
            agent_type = %agent_type.as_str(),
            task_status = %task.status,
            has_conflict_context = conflict_ctx.is_some(),
            has_merge_validation_context = merge_validation_ctx.is_some(),
            "Supervisor: dispatch accepted; preparing session"
        );

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

        // Load project commands once — used for both setup execution and prompt injection.
        let project_repo = ProjectRepository::new(
            self.app_state.db().clone(),
            self.app_state.events().clone(),
        );
        let (prompt_setup_commands, prompt_verification_commands) = {
            if let Ok(Some(ref p)) = project_repo.get(&task.project_id).await {
                let setup_names = format_command_names(&p.setup_commands);
                let verify_names = format_command_names(&p.verification_commands);
                (setup_names, verify_names)
            } else {
                (None, None)
            }
        };

        // Run setup commands in the worktree before starting the agent session.
        {
            if let Ok(Some(project)) = project_repo.get(&task.project_id).await {
                let setup_specs: Vec<CommandSpec> =
                    serde_json::from_str(&project.setup_commands).unwrap_or_default();
                if !setup_specs.is_empty() {
                    let setup_start = std::time::Instant::now();
                    tracing::info!(
                        task_id = %task.short_id,
                        command_count = setup_specs.len(),
                        "Supervisor: running setup commands"
                    );
                    let setup_result = run_commands(&setup_specs, &worktree_path).await;
                    match setup_result {
                        Ok(results) => {
                            let failed = results.last().filter(|r| r.exit_code != 0);
                            if let Some(failure) = failed {
                                let reason = format!(
                                    "Setup command '{}' failed (exit {})\nstdout: {}\nstderr: {}",
                                    failure.name,
                                    failure.exit_code,
                                    failure.stdout.trim(),
                                    failure.stderr.trim(),
                                );
                                tracing::warn!(
                                    task_id = %task.short_id,
                                    command = %failure.name,
                                    exit_code = failure.exit_code,
                                    "Supervisor: setup command failed; blocking task"
                                );
                                let task_repo = TaskRepository::new(
                                    self.app_state.db().clone(),
                                    self.app_state.events().clone(),
                                );
                                if let Err(e) = task_repo
                                    .transition(
                                        &task.id,
                                        TransitionAction::Block,
                                        "agent-supervisor",
                                        "system",
                                        Some(&reason),
                                        None,
                                    )
                                    .await
                                {
                                    tracing::warn!(
                                        task_id = %task.short_id,
                                        error = %e,
                                        "failed to block task after setup failure"
                                    );
                                }
                                self.cleanup_worktree(&task.id, &worktree_path).await;
                                return Err(SupervisorError::Goose(format!(
                                    "setup commands failed for task {}: {}",
                                    task.short_id, reason
                                )));
                            }
                            tracing::info!(
                                task_id = %task.short_id,
                                duration_ms = setup_start.elapsed().as_millis(),
                                "Supervisor: setup commands completed"
                            );
                        }
                        Err(e) => {
                            let reason = format!("Setup commands error: {e}");
                            tracing::warn!(
                                task_id = %task.short_id,
                                error = %e,
                                "Supervisor: setup command error; blocking task"
                            );
                            let task_repo = TaskRepository::new(
                                self.app_state.db().clone(),
                                self.app_state.events().clone(),
                            );
                            if let Err(e2) = task_repo
                                .transition(
                                    &task.id,
                                    TransitionAction::Block,
                                    "agent-supervisor",
                                    "system",
                                    Some(&reason),
                                    None,
                                )
                                .await
                            {
                                tracing::warn!(
                                    task_id = %task.short_id,
                                    error = %e2,
                                    "failed to block task after setup error"
                                );
                            }
                            self.cleanup_worktree(&task.id, &worktree_path).await;
                            return Err(SupervisorError::Goose(reason));
                        }
                    }
                }
            }
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
                Some(session.id.as_str()),
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

        // NOTE: do NOT record_success here — provider creation is just configuration,
        // not an actual API call. Success is recorded in handle_session_result when
        // the session completes without error.

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
                merge_failure_context: merge_validation_ctx,
                setup_commands: prompt_setup_commands,
                verification_commands: prompt_verification_commands,
            },
        );
        agent.override_system_prompt(prompt).await;

        if let Some(entry) = self.capacity.get_mut(&model_id) {
            entry.active += 1;
        }
        let session_cancel = CancellationToken::new();
        let kickoff = GooseMessage::user().with_text(
            "Start by understanding the task context and execute it fully before stopping.",
        );
        let join = spawn_reply_task(
            agent,
            session.id.clone(),
            task_id.clone(),
            project_path.clone(),
            worktree_path.clone(),
            agent_type,
            kickoff,
            session_cancel.clone(),
            self.cancel.clone(),
            self.sender.clone(),
            self.app_state.clone(),
        );

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
        self.session_projects
            .insert(task_id.clone(), task.project_id.clone());
        self.task_session_records
            .insert(task_id.clone(), session_record.id);
        self.session_agent_types.insert(task_id, agent_type);

        if let Some(handle) = self.sessions.get(&task.id) {
            tracing::info!(
                task_id = %task.short_id,
                task_uuid = %task.id,
                project_id = %task.project_id,
                session_id = %handle.session_id,
                agent_type = %agent_type.as_str(),
                worktree = ?handle.worktree_path.as_ref().map(|p| p.display().to_string()),
                "Supervisor: session registered"
            );
        }

        Ok(())
    }

    async fn dispatch_resume(
        &mut self,
        task_id: String,
        model_id: String,
        goose_session_id: String,
        worktree_path: PathBuf,
        resume_prompt: String,
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

        let task = self.load_task(&task_id).await?;
        let project_path = self
            .project_path_for_id(&task.project_id)
            .await
            .unwrap_or_else(|| task.project_id.clone());

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

        let session_repo =
            SessionRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let session_record = session_repo
            .create(
                &task.project_id,
                &task.id,
                &model_id,
                AgentType::Worker.as_str(),
                worktree_path.to_str(),
                Some(&goose_session_id),
            )
            .await
            .map_err(|e| SupervisorError::Goose(e.to_string()))?;

        let goose_model = ModelConfig::new(&model_name)
            .map_err(|e| SupervisorError::Goose(e.to_string()))?
            .with_canonical_limits(&goose_provider_id);

        let extensions = self.extensions_for(AgentType::Worker);

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
            .update_provider(provider, &goose_session_id)
            .await
            .map_err(|e| {
                self.app_state.health_tracker().record_failure(&model_id);
                SupervisorError::Goose(e.to_string())
            })?;

        for ext in extensions {
            agent
                .add_extension(ext, &goose_session_id)
                .await
                .map_err(|e| SupervisorError::Goose(e.to_string()))?;
        }

        if let Some(entry) = self.capacity.get_mut(&model_id) {
            entry.active += 1;
        }

        let session_cancel = CancellationToken::new();
        let kickoff = GooseMessage::user().with_text(&resume_prompt);

        let join = spawn_reply_task(
            agent,
            goose_session_id.clone(),
            task_id.clone(),
            project_path.clone(),
            worktree_path.clone(),
            AgentType::Worker,
            kickoff,
            session_cancel.clone(),
            self.cancel.clone(),
            self.sender.clone(),
            self.app_state.clone(),
        );

        self.sessions.insert(
            task_id.clone(),
            GooseSessionHandle {
                join,
                cancel: session_cancel,
                session_id: goose_session_id.clone(),
                task_id: task_id.clone(),
                worktree_path: Some(worktree_path),
                started_at: Instant::now(),
            },
        );
        self.session_models.insert(task_id.clone(), model_id);
        self.session_projects
            .insert(task_id.clone(), task.project_id.clone());
        self.task_session_records
            .insert(task_id.clone(), session_record.id);
        self.session_agent_types.insert(task_id.clone(), AgentType::Worker);

        tracing::info!(
            task_id = %task.short_id,
            task_uuid = %task.id,
            "Supervisor: resume session dispatched after verification failure"
        );

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
        self.session_projects.remove(&task_id);
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

    async fn pause_session(&mut self, task_id: String) -> Result<(), SupervisorError> {
        let Some(mut handle) = self.sessions.remove(&task_id) else {
            return Ok(());
        };

        self.interrupted_sessions.insert(task_id.clone());
        let model_id = self.session_models.remove(&task_id);
        let _agent_type = self
            .session_agent_types
            .remove(&task_id)
            .unwrap_or(AgentType::Worker);
        self.session_projects.remove(&task_id);
        let session_record_id = self.task_session_records.remove(&task_id);
        let goose_session_id = handle.session_id.clone();
        let worktree_path = handle.worktree_path.take();

        handle.cancel.cancel();

        match tokio::time::timeout(Duration::from_secs(30), &mut handle.join).await {
            Ok(_) => {}
            Err(_) => {
                tracing::warn!(task_id = %task_id, "session join timed out during pause; aborting");
                handle.join.abort();
                let _ = handle.join.await;
            }
        }

        self.decrement_capacity_for_model(model_id.as_deref());

        // Commit WIP but keep the worktree alive for resume.
        if let Some(worktree_path) = worktree_path.as_ref() {
            self.commit_wip_if_needed(&task_id, worktree_path).await;
        }

        let (tokens_in, tokens_out) = self.tokens_for_session(&goose_session_id).await;
        self.update_session_record_paused(session_record_id.as_deref(), tokens_in, tokens_out)
            .await;

        tracing::info!(
            task_id = %task_id,
            worktree = ?worktree_path.as_ref().map(|p: &PathBuf| p.display().to_string()),
            "Supervisor: session paused, worktree preserved"
        );

        Ok(())
    }

    async fn resume_paused_session(
        &mut self,
        task_id: String,
        project_path: String,
        _requested_model_id: String,
        paused: SessionRecord,
        context_message: String,
    ) -> Result<(), SupervisorError> {
        let goose_session_id = paused.goose_session_id.clone().ok_or_else(|| {
            SupervisorError::Goose(format!(
                "paused session {} has no goose_session_id",
                paused.id
            ))
        })?;
        let worktree_path = paused
            .worktree_path
            .as_deref()
            .map(PathBuf::from)
            .ok_or_else(|| {
                SupervisorError::Goose(format!(
                    "paused session {} has no worktree_path",
                    paused.id
                ))
            })?;

        // Use the model from the paused record (continuity — same model that wrote the code).
        let model_id = paused.model_id.clone();

        // Verify worktree still exists.
        if !worktree_path.exists() || !worktree_path.is_dir() {
            return Err(SupervisorError::Goose(format!(
                "paused session worktree no longer exists: {}",
                worktree_path.display()
            )));
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

        let task = self.load_task(&task_id).await?;
        let agent_type = self.agent_type_for_task(&task, false);

        tracing::info!(
            task_id = %task.short_id,
            task_uuid = %task.id,
            goose_session_id = %goose_session_id,
            model_id = %model_id,
            agent_type = %agent_type.as_str(),
            worktree = %worktree_path.display(),
            "Supervisor: resuming paused session"
        );

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
            .update_provider(provider, &goose_session_id)
            .await
            .map_err(|e| {
                self.app_state.health_tracker().record_failure(&model_id);
                SupervisorError::Goose(e.to_string())
            })?;

        for ext in extensions {
            agent
                .add_extension(ext, &goose_session_id)
                .await
                .map_err(|e| SupervisorError::Goose(e.to_string()))?;
        }

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
                conflict_files: None,
                merge_base_branch: None,
                merge_target_branch: None,
                merge_failure_context: None,
                setup_commands: None,
                verification_commands: None,
            },
        );
        agent.override_system_prompt(prompt).await;

        // Mark session record as running again.
        let session_repo =
            SessionRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        if let Err(e) = session_repo.set_running(&paused.id).await {
            tracing::warn!(
                record_id = %paused.id,
                error = %e,
                "failed to mark resumed session as running"
            );
        }

        if let Some(entry) = self.capacity.get_mut(&model_id) {
            entry.active += 1;
        }

        let session_cancel = CancellationToken::new();
        let kickoff = GooseMessage::user().with_text(&context_message);
        let join = spawn_reply_task(
            agent,
            goose_session_id.clone(),
            task_id.clone(),
            project_path.clone(),
            worktree_path.clone(),
            agent_type,
            kickoff,
            session_cancel.clone(),
            self.cancel.clone(),
            self.sender.clone(),
            self.app_state.clone(),
        );

        self.sessions.insert(
            task_id.clone(),
            GooseSessionHandle {
                join,
                cancel: session_cancel,
                session_id: goose_session_id,
                task_id: task_id.clone(),
                worktree_path: Some(worktree_path),
                started_at: Instant::now(),
            },
        );
        self.session_models.insert(task_id.clone(), model_id);
        self.session_projects
            .insert(task_id.clone(), task.project_id.clone());
        self.task_session_records
            .insert(task_id.clone(), paused.id);
        self.session_agent_types.insert(task_id, agent_type);

        tracing::info!(
            task_id = %task.short_id,
            agent_type = %agent_type.as_str(),
            "Supervisor: resumed session registered"
        );

        Ok(())
    }

    async fn find_paused_session_record(
        &self,
        task_id: &str,
    ) -> Option<SessionRecord> {
        let repo =
            SessionRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        repo.paused_for_task(task_id).await.ok().flatten()
    }

    async fn resume_context_for_task(&self, task_id: &str) -> String {
        let repo =
            TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let activity = repo.list_activity(task_id).await.ok().unwrap_or_default();

        // Check for most recent task reviewer comment (reviewer rejection feedback).
        for entry in activity.iter().rev() {
            if entry.event_type == "comment" && entry.actor_role == "task_reviewer" {
                if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&entry.payload)
                    && let Some(body) = payload.get("body").and_then(|v| v.as_str())
                {
                    return format!(
                        "Your previous work was reviewed and returned with this feedback:\n\n{body}\n\nAddress this feedback, make the necessary changes, then emit:\nWORKER_RESULT: DONE"
                    );
                }
            }
        }

        // Check for merge conflict context.
        if let Some(context) = self.merge_validation_context_for_dispatch(task_id).await {
            return context;
        }

        // Check for merge conflict info in activity.
        for entry in activity.iter().rev() {
            if entry.event_type == "merge_conflict" {
                if let Ok(meta) =
                    serde_json::from_str::<MergeConflictMetadata>(&entry.payload)
                {
                    let files = meta
                        .conflicting_files
                        .iter()
                        .map(|f| format!("- {f}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    return format!(
                        "A merge conflict was detected when merging your branch into `{}`. Resolve the conflicts in these files:\n\n{files}\n\nAfter resolving, commit and emit:\nWORKER_RESULT: DONE",
                        meta.merge_target
                    );
                }
            }
        }

        // Default fallback.
        "Your previous submission needs revision. Review your work, address any issues, then emit:\nWORKER_RESULT: DONE".to_string()
    }

    async fn cleanup_paused_worker_session(&self, task_id: &str) {
        let repo =
            SessionRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let Ok(Some(paused)) = repo.paused_for_task(task_id).await else {
            return;
        };

        let (tokens_in, tokens_out) = if let Some(ref gsid) = paused.goose_session_id {
            self.tokens_for_session(gsid).await
        } else {
            (paused.tokens_in, paused.tokens_out)
        };

        if let Err(e) = repo
            .update(&paused.id, SessionStatus::Completed, tokens_in, tokens_out)
            .await
        {
            tracing::warn!(
                record_id = %paused.id,
                error = %e,
                "failed to finalize paused session record on task approval"
            );
        }

        if let Some(worktree_path) = paused.worktree_path.as_deref().map(PathBuf::from) {
            self.cleanup_worktree(task_id, &worktree_path).await;
        }
    }

    async fn update_session_record_paused(
        &self,
        record_id: Option<&str>,
        tokens_in: i64,
        tokens_out: i64,
    ) {
        let Some(record_id) = record_id else {
            return;
        };

        let repo =
            SessionRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        if let Err(e) = repo.pause(record_id, tokens_in, tokens_out).await {
            tracing::warn!(
                record_id = %record_id,
                error = %e,
                "failed to pause session record"
            );
        }
    }

    fn remove_session(&mut self, task_id: &str) -> SessionClosure {
        let removed = self.sessions.remove(task_id);
        let goose_session_id = removed
            .as_ref()
            .map(|h| h.session_id.clone())
            .unwrap_or_else(|| format!("unknown-session-{task_id}"));
        self.decrement_capacity(task_id);
        self.session_projects.remove(task_id);
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

        // Worker Done: pause session record (keep worktree alive for resume after review).
        // All other cases: complete or fail the session record.
        let is_worker_done = result.is_ok()
            && matches!(agent_type, AgentType::Worker | AgentType::ConflictResolver)
            && matches!(output.worker_signal, Some(WorkerSignal::Done));

        if is_worker_done {
            self.update_session_record_paused(session.record_id.as_deref(), tokens_in, tokens_out)
                .await;
        } else {
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
        }

        if let Some(worktree_path) = session.worktree_path.as_ref() {
            // Run verification commands after DONE signal, before committing.
            if is_worker_done {
                if let Some(feedback) =
                    self.run_verification_commands(task_id, worktree_path).await
                {
                    // Verification failed — preserve worktree, resume the session.
                    self.queue_resume_after_verification_failure(
                        task_id,
                        &session,
                        worktree_path,
                        &feedback,
                    )
                    .await;
                    return;
                }
            }

            if is_worker_done {
                // Commit final work and keep worktree alive for the review→resume cycle.
                if let Err(e) = self
                    .commit_final_work_if_needed(task_id, worktree_path)
                    .await
                {
                    tracing::warn!(
                        task_id = %task_id,
                        worktree_path = %worktree_path.display(),
                        error = %e,
                        "failed to commit work before pausing for review; preserving worktree"
                    );
                }
                // Worktree intentionally kept — cleaned up in cleanup_paused_worker_session
                // when the task is finally approved.
            } else {
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
            tracing::info!(
                task_id = %task_id,
                agent_type = %agent_type.as_str(),
                transition_action = ?action,
                transition_reason = reason.as_deref().unwrap_or("<none>"),
                tokens_in,
                tokens_out,
                "Supervisor: applying session transition"
            );
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
        } else {
            tracing::info!(
                task_id = %task_id,
                agent_type = %agent_type.as_str(),
                tokens_in,
                tokens_out,
                "Supervisor: session completed with no task transition"
            );
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

    /// Runs the project's verification commands in the task worktree.
    /// Returns `None` if all commands pass or there are no verification commands.
    /// Returns `Some(feedback)` if any command fails, with the failure details.
    async fn run_verification_commands(
        &self,
        task_id: &str,
        worktree_path: &Path,
    ) -> Option<String> {
        let task = self.load_task(task_id).await.ok()?;
        let project_repo =
            ProjectRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let project = project_repo.get(&task.project_id).await.ok()??;
        let specs: Vec<CommandSpec> =
            serde_json::from_str(&project.verification_commands).unwrap_or_default();
        if specs.is_empty() {
            return None;
        }
        tracing::info!(
            task_id = %task_id,
            command_count = specs.len(),
            "Supervisor: running verification commands"
        );
        match run_commands(&specs, worktree_path).await {
            Ok(results) => {
                let failed = results.iter().find(|r| r.exit_code != 0)?;
                tracing::info!(
                    task_id = %task_id,
                    command = %failed.name,
                    exit_code = failed.exit_code,
                    "Supervisor: verification command failed"
                );
                Some(format!(
                    "Verification command '{}' failed with exit code {}.\n\nFix the issue and signal WORKER_RESULT: DONE when complete.\n\nstdout:\n{}\nstderr:\n{}",
                    failed.name,
                    failed.exit_code,
                    failed.stdout.trim(),
                    failed.stderr.trim(),
                ))
            }
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "Supervisor: verification command system error");
                Some(format!(
                    "Verification commands could not run: {e}\n\nFix the issue and signal WORKER_RESULT: DONE when complete."
                ))
            }
        }
    }

    /// Logs the verification failure as a task comment and queues a ResumeSession message.
    async fn queue_resume_after_verification_failure(
        &self,
        task_id: &str,
        session: &SessionClosure,
        worktree_path: &Path,
        feedback: &str,
    ) {
        let repo =
            TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let payload = serde_json::json!({ "body": feedback }).to_string();
        if let Err(e) = repo
            .log_activity(
                Some(task_id),
                "agent-supervisor",
                "verification",
                "comment",
                &payload,
            )
            .await
        {
            tracing::warn!(task_id = %task_id, error = %e, "failed to log verification failure comment");
        }

        let Some(model_id) = session.model_id.clone() else {
            tracing::warn!(
                task_id = %task_id,
                "no model_id in session closure; cannot resume after verification failure"
            );
            return;
        };

        let msg = SupervisorMessage::ResumeSession {
            task_id: task_id.to_owned(),
            model_id,
            goose_session_id: session.goose_session_id.clone(),
            worktree_path: worktree_path.to_owned(),
            resume_prompt: feedback.to_owned(),
        };
        if let Err(e) = self.sender.send(msg).await {
            tracing::warn!(task_id = %task_id, error = %e, "failed to queue resume session after verification failure");
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

    async fn last_assistant_text_from_goose_sqlite(goose_session_id: &str) -> Option<String> {
        for db_path in Self::goose_session_db_candidates() {
            let Some(text) =
                Self::last_assistant_text_from_goose_sqlite_at(&db_path, goose_session_id).await
            else {
                continue;
            };
            return Some(text);
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

    async fn last_assistant_text_from_goose_sqlite_at(
        db_path: &Path,
        goose_session_id: &str,
    ) -> Option<String> {
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

        let content_json = sqlx::query_scalar::<_, String>(
            "SELECT content_json FROM messages WHERE session_id = ?1 AND role = 'assistant' ORDER BY id DESC LIMIT 1",
        )
        .bind(goose_session_id)
        .fetch_optional(&pool)
        .await
        .ok()??;

        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content_json)
            && let Some(items) = value.as_array()
        {
            let mut text_parts = Vec::new();
            for item in items {
                let is_text = item
                    .get("type")
                    .and_then(|v| v.as_str())
                    .is_some_and(|t| t == "text");
                if !is_text {
                    continue;
                }
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    text_parts.push(text);
                }
            }
            if !text_parts.is_empty() {
                return Some(text_parts.join("\n"));
            }
        }

        Some(content_json)
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

    async fn merge_validation_context_for_dispatch(&self, task_id: &str) -> Option<String> {
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
        let metadata = Self::parse_merge_validation_metadata(reason)?;
        Some(metadata.as_prompt_context())
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

    fn parse_merge_validation_metadata(reason: &str) -> Option<MergeValidationFailureMetadata> {
        let raw = reason.strip_prefix(MERGE_VALIDATION_PREFIX)?;
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
                tracing::info!(
                    task_id = %task.short_id,
                    task_uuid = %task.id,
                    base_branch = %base_branch,
                    merge_target = %merge_target,
                    commit_sha = %result.commit_sha,
                    "Supervisor: post-review squash merge succeeded"
                );
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
                // Clean up the worker's paused session (worktree + record finalization).
                self.cleanup_paused_worker_session(task_id).await;
                Some((TransitionAction::TaskReviewApprove, None))
            }
            Err(GitError::MergeConflict { files, .. }) => {
                tracing::warn!(
                    task_id = %task.short_id,
                    task_uuid = %task.id,
                    base_branch = %base_branch,
                    merge_target = %merge_target,
                    conflict_count = files.len(),
                    conflicting_files = ?files,
                    "Supervisor: post-review merge conflict"
                );
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
            Err(GitError::CommitRejected {
                code,
                command,
                cwd,
                stdout,
                stderr,
            }) => {
                tracing::warn!(
                    task_id = %task.short_id,
                    task_uuid = %task.id,
                    base_branch = %base_branch,
                    merge_target = %merge_target,
                    exit_code = code,
                    command = %command,
                    cwd = %cwd,
                    stdout_snippet = %log_snippet(&stdout, 400),
                    stderr_snippet = %log_snippet(&stderr, 400),
                    "Supervisor: post-review merge commit rejected"
                );
                let metadata = MergeValidationFailureMetadata {
                    base_branch,
                    merge_target,
                    command,
                    cwd,
                    exit_code: code,
                    stdout,
                    stderr,
                };
                let reason_payload =
                    serde_json::to_string(&metadata).unwrap_or_else(|_| "{}".to_string());
                let reason = format!("{MERGE_VALIDATION_PREFIX}{reason_payload}");
                let _ = repo
                    .log_activity(
                        Some(task_id),
                        "agent-supervisor",
                        "system",
                        "merge_validation_failed",
                        &reason_payload,
                    )
                    .await;
                Some((TransitionAction::TaskReviewRejectConflict, Some(reason)))
            }
            Err(e) => {
                tracing::warn!(
                    task_id = %task.short_id,
                    task_uuid = %task.id,
                    base_branch = %base_branch,
                    merge_target = %merge_target,
                    error = %e,
                    error_debug = ?e,
                    "Supervisor: post-review squash merge failed"
                );
                Some((
                    TransitionAction::ReleaseTaskReview,
                    Some(format!("post-review squash merge failed: {e} ({e:?})")),
                ))
            }
        }
    }

    fn missing_required_marker(agent_type: AgentType, output: &ParsedAgentOutput) -> bool {
        match agent_type {
            AgentType::Worker | AgentType::ConflictResolver => output.worker_signal.is_none(),
            AgentType::TaskReviewer => output.reviewer_verdict.is_none(),
            AgentType::EpicReviewer => output.epic_verdict.is_none(),
        }
    }

    fn missing_marker_nudge(agent_type: AgentType, output: &ParsedAgentOutput) -> Option<&'static str> {
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

        match sync_git.rebase_with_retry(&upstream).await {
            Ok(_) => {
                tracing::info!(branch = %branch, upstream = %upstream, "rebased existing task branch before dispatch");
            }
            Err(GitError::CommandFailed { .. }) => {
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

    fn collect_pending_session(
        &mut self,
        task_id: String,
        mut handle: GooseSessionHandle,
    ) -> PendingInterrupt {
        handle.cancel.cancel();
        self.interrupted_sessions.insert(task_id.clone());
        PendingInterrupt {
            model_id: self.session_models.remove(&task_id),
            agent_type: self
                .session_agent_types
                .remove(&task_id)
                .unwrap_or(AgentType::Worker),
            session_record_id: self.task_session_records.remove(&task_id),
            goose_session_id: handle.session_id,
            join: handle.join,
            worktree_path: handle.worktree_path.take(),
            task_id,
        }
    }

    async fn drain_pending_sessions(&mut self, pending: &mut Vec<PendingInterrupt>, reason: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        for item in pending.iter_mut() {
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

        for item in pending.drain(..) {
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

    async fn interrupt_all_sessions(&mut self, reason: &str) {
        let mut pending: Vec<PendingInterrupt> = Vec::new();
        for (task_id, handle) in std::mem::take(&mut self.sessions) {
            self.session_projects.remove(&task_id);
            pending.push(self.collect_pending_session(task_id, handle));
        }
        self.drain_pending_sessions(&mut pending, reason).await;
    }

    async fn interrupt_project_sessions(&mut self, project_id: &str, reason: &str) {
        let matching_task_ids: Vec<String> = self
            .session_projects
            .iter()
            .filter(|(_, pid)| *pid == project_id)
            .map(|(tid, _)| tid.clone())
            .collect();

        let mut pending: Vec<PendingInterrupt> = Vec::new();
        for task_id in matching_task_ids {
            self.session_projects.remove(&task_id);
            if let Some(handle) = self.sessions.remove(&task_id) {
                pending.push(self.collect_pending_session(task_id, handle));
            }
        }
        self.drain_pending_sessions(&mut pending, reason).await;
    }
}

struct PendingInterrupt {
    task_id: String,
    join: tokio::task::JoinHandle<anyhow::Result<()>>,
    worktree_path: Option<PathBuf>,
    model_id: Option<String>,
    agent_type: AgentType,
    goose_session_id: String,
    session_record_id: Option<String>,
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

    pub async fn pause_session(&self, task_id: &str) -> Result<(), SupervisorError> {
        self.request(|tx| SupervisorMessage::PauseSession {
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

    pub async fn interrupt_project(
        &self,
        project_id: &str,
        reason: &str,
    ) -> Result<(), SupervisorError> {
        self.request(|tx| SupervisorMessage::InterruptProject {
            project_id: project_id.to_owned(),
            reason: reason.to_owned(),
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
