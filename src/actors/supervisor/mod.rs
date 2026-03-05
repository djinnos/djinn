// AgentSupervisor — 1x global, manages in-process Goose session lifecycle.

mod compaction;
mod dispatch;
mod epic_review;
mod git_worktree;
mod helpers;
mod provider;
mod session_ops;
mod tokens;

pub(super) use compaction::perform_compaction;


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
use crate::commands::{CommandSpec, run_commands};
use crate::db::repositories::credential::CredentialRepository;
use crate::db::repositories::epic::EpicRepository;
use crate::db::repositories::epic_review_batch::EpicReviewBatchRepository;
use crate::db::repositories::project::ProjectRepository;
use crate::db::repositories::session::SessionRepository;
use crate::db::repositories::task::TaskRepository;
use crate::events::DjinnEvent;
use crate::models::session::{SessionRecord, SessionStatus};
use crate::models::task::{Task, TransitionAction};
use crate::server::AppState;

pub(super) const MERGE_CONFLICT_PREFIX: &str = "merge_conflict:";
pub(super) const MERGE_VALIDATION_PREFIX: &str = "merge_validation_failed:";
static GOOSE_BUILTINS_REGISTERED: Once = Once::new();

pub(super) fn register_goose_builtin_extensions() {
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
pub(super) fn format_command_names(json: &str) -> Option<String> {
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

pub(super) fn runtime_fs_diagnostics(project_path: &str, worktree_path: &Path) -> String {
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

pub(super) fn runtime_env_diagnostics(session_id: &str, project_path: &str, worktree_path: &Path) -> String {
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

pub(super) fn log_snippet(text: &str, max_chars: usize) -> String {
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
pub(super) struct MergeConflictMetadata {
    pub(super) conflicting_files: Vec<String>,
    pub(super) base_branch: String,
    pub(super) merge_target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct MergeValidationFailureMetadata {
    pub(super) base_branch: String,
    pub(super) merge_target: String,
    pub(super) command: String,
    pub(super) cwd: String,
    pub(super) exit_code: i32,
    pub(super) stdout: String,
    pub(super) stderr: String,
}

impl MergeValidationFailureMetadata {
    pub(super) fn as_prompt_context(&self) -> String {
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
    #[error("paused session stale for task {task_id} (worktree missing)")]
    PausedSessionStale { task_id: String },
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

pub(super) type Reply<T> = oneshot::Sender<Result<T, SupervisorError>>;

pub(super) enum SupervisorMessage {
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
        tokens_in: i64,
        old_record_id: Option<String>,
    },
    CompactionNeeded {
        task_id: String,
        old_goose_session_id: String,
        tokens_in: i64,
        context_window: i64,
    },
    /// Compaction succeeded: new Goose session and agent are ready; supervisor registers them.
    CompactionComplete {
        task_id: String,
        model_id: String,
        agent_type: AgentType,
        project_id: String,
        new_goose_session_id: String,
        new_record_id: String,
        agent: Arc<GooseAgent>,
        worktree_path: PathBuf,
        summary: String,
        context_window: i64,
    },
    /// Compaction failed after the old session was cancelled; supervisor must release the task.
    CompactionAborted {
        task_id: String,
        model_id: String,
        agent_type: AgentType,
        worktree_path: Option<PathBuf>,
    },
}

pub(super) struct SessionClosure {
    pub(super) model_id: Option<String>,
    pub(super) agent_type: AgentType,
    pub(super) goose_session_id: String,
    pub(super) record_id: Option<String>,
    pub(super) worktree_path: Option<PathBuf>,
}

/// Spawns the agent reply loop task. Used by both fresh dispatch and session resume.
/// `reply_cancel` is a *clone* of the session's cancellation token (caller retains the original
/// for the GooseSessionHandle). `kickoff` is the first message sent to the agent.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_reply_task(
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
    context_window: i64,
    session_manager: Arc<SessionManager>,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    tokio::spawn(async move {
        let mut output = ParsedAgentOutput::new(agent_type);
        let run_result: anyhow::Result<()> = async {
            let mut pending_message = Some(kickoff);
            let mut saw_any_event = false;
            let mut saw_any_tool_use = false;
            let assistant_role = GooseMessage::assistant().role;
            let mut assistant_message_count: usize = 0;
            let mut assistant_fragments: Vec<String> = Vec::new();
            let mut compaction_signaled = false;

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

                                // After each agent turn, read token usage and emit a telemetry
                                // event. Also check against the 80% compaction threshold.
                                {
                                    let goose_session = session_manager.get_session(&session_id, false).await;
                                    let (tokens_in, tokens_out) = if let Ok(s) = goose_session {
                                        let ti = s.accumulated_input_tokens
                                            .or(s.input_tokens)
                                            .unwrap_or(0) as i64;
                                        let to = s.accumulated_output_tokens
                                            .or(s.output_tokens)
                                            .unwrap_or(0) as i64;
                                        (ti, to)
                                    } else {
                                        AgentSupervisor::tokens_from_goose_sqlite(&session_id)
                                            .await
                                            .unwrap_or((0, 0))
                                    };
                                    let usage_pct = if context_window > 0 {
                                        tokens_in as f64 / context_window as f64
                                    } else {
                                        0.0
                                    };
                                    let _ = app_state.events().send(DjinnEvent::SessionTokenUpdate {
                                        session_id: session_id.clone(),
                                        task_id: task_id.clone(),
                                        tokens_in,
                                        tokens_out,
                                        context_window,
                                        usage_pct,
                                    });
                                    if !compaction_signaled && context_window > 0 && usage_pct >= 0.8 {
                                        compaction_signaled = true;
                                        tracing::info!(
                                            task_id = %task_id,
                                            session_id = %session_id,
                                            tokens_in,
                                            context_window,
                                            threshold_pct = 80,
                                            "Supervisor: compaction threshold reached; signalling supervisor"
                                        );
                                        let _ = sender
                                            .send(SupervisorMessage::CompactionNeeded {
                                                task_id: task_id.clone(),
                                                old_goose_session_id: session_id.clone(),
                                                tokens_in,
                                                context_window,
                                            })
                                            .await;
                                    }
                                }
                            }
                            extension::handle_event(&app_state, &agent, &evt, &worktree_path).await;
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
                        extension::handle_event(&app_state, &agent, &evt, &worktree_path).await;
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

pub(super) struct AgentSupervisor {
    pub(super) receiver: mpsc::Receiver<SupervisorMessage>,
    pub(super) sessions: HashMap<String, GooseSessionHandle>,
    pub(super) capacity: HashMap<String, ModelCapacity>,
    pub(super) session_models: HashMap<String, String>,
    pub(super) session_agent_types: HashMap<String, AgentType>,
    pub(super) session_projects: HashMap<String, String>,
    pub(super) task_session_records: HashMap<String, String>,
    pub(super) interrupted_sessions: HashSet<String>,
    /// Tasks currently undergoing compaction (no active session, but not stuck).
    pub(super) compacting_tasks: HashSet<String>,
    /// Tasks fully owned by the supervisor: from dispatch start through post-session
    /// completion (verification, commit, transition, cleanup). Prevents stuck detection
    /// from false-positiving during the sessionless post-session window.
    pub(super) in_flight: HashSet<String>,
    pub(super) default_max_sessions: u32,
    pub(super) configured_model_limits: HashMap<String, u32>,
    pub(super) session_manager: Arc<SessionManager>,
    pub(super) app_state: AppState,
    pub(super) cancel: CancellationToken,
    pub(super) sender: mpsc::Sender<SupervisorMessage>,
}

impl AgentSupervisor {
    pub(super) fn new(
        receiver: mpsc::Receiver<SupervisorMessage>,
        sender: mpsc::Sender<SupervisorMessage>,
        app_state: AppState,
        session_manager: Arc<SessionManager>,
        cancel: CancellationToken,
    ) -> Self {
        register_goose_builtin_extensions();
        // Disable Goose's built-in auto-compaction so Djinn owns the compaction lifecycle entirely.
        // check_if_compaction_needed() returns false when threshold <= 0.0 || threshold >= 1.0.
        if let Err(e) = GooseConfig::global().set_param("GOOSE_AUTO_COMPACT_THRESHOLD", 0.0f64) {
            tracing::warn!(error = %e, "Failed to disable Goose auto-compaction threshold");
        }
        Self {
            receiver,
            sessions: HashMap::new(),
            capacity: HashMap::new(),
            session_models: HashMap::new(),
            session_agent_types: HashMap::new(),
            session_projects: HashMap::new(),
            task_session_records: HashMap::new(),
            interrupted_sessions: HashSet::new(),
            compacting_tasks: HashSet::new(),
            in_flight: HashSet::new(),
            default_max_sessions: 1,
            configured_model_limits: HashMap::new(),
            session_manager,
            app_state,
            cancel,
            sender,
        }
    }

    pub(super) fn max_for_model(&self, model_id: &str) -> u32 {
        self.configured_model_limits
            .get(model_id)
            .copied()
            .unwrap_or(self.default_max_sessions)
    }

    pub(super) fn apply_session_limits(&mut self, max_sessions: HashMap<String, u32>, default_max: u32) {
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

    pub(super) async fn run(mut self) {
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

    pub(super) async fn handle(&mut self, msg: SupervisorMessage) {
        match msg {
            SupervisorMessage::Dispatch {
                task_id,
                project_path,
                model_id,
                respond_to,
            } => {
                let result = self.dispatch(task_id.clone(), project_path, model_id).await;
                if result.is_err() {
                    self.in_flight.remove(&task_id);
                }
                let _ = respond_to.send(result);
            }
            SupervisorMessage::HasSession {
                task_id,
                respond_to,
            } => {
                let active = self.sessions.contains_key(&task_id)
                    || self.compacting_tasks.contains(&task_id)
                    || self.in_flight.contains(&task_id);
                let _ = respond_to.send(Ok(active));
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
                    self.in_flight.remove(&task_id);
                    return;
                }
                tracing::info!(
                    task_id = %task_id,
                    result = if result.is_ok() { "ok" } else { "error" },
                    "Supervisor: session completion received"
                );

                // Detect context exhaustion and trigger a fresh continuation
                // instead of failing/releasing the task.
                if self.maybe_compact_on_context_exhaustion(&task_id, &result, &output).await {
                    return;
                }

                let session = self.remove_session(&task_id);
                self.handle_session_result(&task_id, session, result, output)
                    .await;
                // Remove in_flight AFTER all post-session work (verification, commit,
                // transition, cleanup) is done. If handle_session_result queued a
                // ResumeSession (verification failure path), this removal is safe:
                // no HasSession query can be processed between this remove and the
                // re-insert in dispatch_resume since the actor is still in its loop.
                self.in_flight.remove(&task_id);
            }
            SupervisorMessage::ResumeSession {
                task_id,
                model_id,
                goose_session_id,
                worktree_path,
                resume_prompt,
                tokens_in,
                old_record_id,
            } => {
                if let Err(e) = self
                    .dispatch_resume(
                        task_id.clone(),
                        model_id,
                        goose_session_id,
                        worktree_path,
                        resume_prompt,
                        tokens_in,
                        old_record_id,
                    )
                    .await
                {
                    tracing::warn!(error = %e, "Supervisor: failed to dispatch resume session after verification failure");
                    self.in_flight.remove(&task_id);
                }
            }
            SupervisorMessage::CompactionNeeded {
                task_id,
                old_goose_session_id,
                tokens_in,
                context_window,
            } => {
                self.handle_compaction_needed(
                    task_id,
                    old_goose_session_id,
                    tokens_in,
                    context_window,
                )
                .await;
            }
            SupervisorMessage::CompactionComplete {
                task_id,
                model_id,
                agent_type,
                project_id,
                new_goose_session_id,
                new_record_id,
                agent,
                worktree_path,
                summary,
                context_window,
            } => {
                self.handle_compaction_complete(
                    task_id,
                    model_id,
                    agent_type,
                    project_id,
                    new_goose_session_id,
                    new_record_id,
                    agent,
                    worktree_path,
                    summary,
                    context_window,
                )
                .await;
            }
            SupervisorMessage::CompactionAborted {
                task_id,
                model_id,
                agent_type,
                worktree_path,
            } => {
                self.handle_compaction_aborted(task_id, model_id, agent_type, worktree_path)
                    .await;
            }
        }
    }

    pub(super) async fn shutdown(&mut self) {
        self.interrupt_all_sessions("session interrupted by supervisor shutdown")
            .await;
    }

    pub(super) fn remove_session(&mut self, task_id: &str) -> SessionClosure {
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

    pub(super) fn decrement_capacity(&mut self, task_id: &str) {
        if let Some(model_id) = self.session_models.get(task_id)
            && let Some(model_capacity) = self.capacity.get_mut(model_id)
            && model_capacity.active > 0
        {
            model_capacity.active -= 1;
        }
    }

    pub(super) fn decrement_capacity_for_model(&mut self, model_id: Option<&str>) {
        if let Some(model_id) = model_id
            && let Some(model_capacity) = self.capacity.get_mut(model_id)
            && model_capacity.active > 0
        {
            model_capacity.active -= 1;
        }
    }

    pub(super) fn running_sessions_snapshot(&self) -> Vec<RunningSessionInfo> {
        let mut sessions: Vec<RunningSessionInfo> = self
            .sessions
            .iter()
            .map(|(task_id, handle)| self.session_snapshot(task_id, handle))
            .collect();
        sessions.sort_by(|a, b| a.task_id.cmp(&b.task_id));
        sessions
    }

    pub(super) fn session_snapshot(&self, task_id: &str, handle: &GooseSessionHandle) -> RunningSessionInfo {
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

    pub(super) fn collect_pending_session(
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

    pub(super) async fn drain_pending_sessions(&mut self, pending: &mut Vec<PendingInterrupt>, reason: &str) {
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

    pub(super) async fn interrupt_all_sessions(&mut self, reason: &str) {
        let mut pending: Vec<PendingInterrupt> = Vec::new();
        for (task_id, handle) in std::mem::take(&mut self.sessions) {
            self.session_projects.remove(&task_id);
            pending.push(self.collect_pending_session(task_id, handle));
        }
        self.drain_pending_sessions(&mut pending, reason).await;
    }

    pub(super) async fn interrupt_project_sessions(&mut self, project_id: &str, reason: &str) {
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

    pub(super) fn missing_required_marker(agent_type: AgentType, output: &ParsedAgentOutput) -> bool {
        match agent_type {
            AgentType::Worker | AgentType::ConflictResolver => output.worker_signal.is_none(),
            AgentType::TaskReviewer => output.reviewer_verdict.is_none(),
            AgentType::EpicReviewer => output.epic_verdict.is_none(),
        }
    }

    pub(super) fn missing_marker_nudge(
        agent_type: AgentType,
        output: &ParsedAgentOutput,
    ) -> Option<&'static str> {
        if !Self::missing_required_marker(agent_type, output) {
            return None;
        }

        match agent_type {
            AgentType::Worker | AgentType::ConflictResolver => Some(
                "Emit exactly one final marker now: WORKER_RESULT: DONE.",
            ),
            AgentType::TaskReviewer => Some(
                "Emit exactly one final marker now: REVIEW_RESULT: VERIFIED | REOPEN | CANCEL. If REOPEN or CANCEL, also emit FEEDBACK: <what is missing>.",
            ),
            AgentType::EpicReviewer => Some(
                "Emit exactly one final marker now: EPIC_REVIEW_RESULT: CLEAN | ISSUES_FOUND. If ISSUES_FOUND, include concise actionable findings and create follow-up tasks in this epic before finishing.",
            ),
        }
    }

    pub(super) fn agent_type_for_task(&self, task: &Task, has_conflict_context: bool) -> AgentType {
        match task.status.as_str() {
            "needs_task_review" | "in_task_review" => AgentType::TaskReviewer,
            "open" if has_conflict_context => AgentType::ConflictResolver,
            _ => AgentType::Worker,
        }
    }

    pub(super) fn parse_model_id(model_id: &str) -> Result<(String, String), SupervisorError> {
        let Some((provider_id, model_name)) = model_id.split_once('/') else {
            return Err(SupervisorError::InvalidModelId {
                model_id: model_id.to_owned(),
            });
        };
        Ok((provider_id.to_owned(), model_name.to_owned()))
    }

    pub(super) fn extensions_for(&self, agent_type: AgentType) -> Vec<goose::config::ExtensionConfig> {
        vec![extension::config(agent_type)]
    }

    pub(super) fn spawn_mock_session(&mut self, task_id: String, model_id: String) {
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
}

pub(super) struct PendingInterrupt {
    pub(super) task_id: String,
    pub(super) join: tokio::task::JoinHandle<anyhow::Result<()>>,
    pub(super) worktree_path: Option<PathBuf>,
    pub(super) model_id: Option<String>,
    pub(super) agent_type: AgentType,
    pub(super) goose_session_id: String,
    pub(super) session_record_id: Option<String>,
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
