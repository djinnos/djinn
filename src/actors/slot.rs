use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use goose::agents::{
    Agent as GooseAgent, AgentConfig as GooseAgentConfig, GoosePlatform,
    SessionConfig as GooseSessionConfig,
};
use goose::config::{Config as GooseConfig, GooseMode, PermissionManager};
use goose::conversation::message::{Message as GooseMessage, MessageContent};
use goose::model::ModelConfig;
use goose::providers;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::actors::git::GitError;
use crate::agent::extension;
use crate::agent::output_parser::{
    EpicReviewVerdict, ParsedAgentOutput, ReviewerVerdict, WorkerSignal,
};
use crate::agent::prompts::{TaskContext, render_prompt};
use crate::agent::{AgentType, SessionManager, SessionType};
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

// ─── Slot types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum SlotEvent {
    /// Slot finished its task (success or failure) and is free for reassignment.
    Free {
        slot_id: usize,
        model_id: String,
        task_id: String,
    },
    /// Slot's task was killed by external request.
    Killed {
        slot_id: usize,
        model_id: String,
        task_id: String,
    },
}

#[derive(Debug)]
pub enum SlotCommand {
    /// Run a task lifecycle in this slot.
    RunTask {
        task_id: String,
        project_path: String,
        respond_to: oneshot::Sender<Result<(), SlotError>>,
    },
    /// Kill the currently running task.
    Kill,
    /// Pause the currently running task (commit WIP, preserve worktree).
    Pause,
    /// Finish current task then shut down (for capacity reduction).
    Drain,
}

#[derive(Debug, Error, Clone)]
pub enum SlotError {
    #[error("slot is busy")]
    SlotBusy,
    #[error("session failed: {0}")]
    SessionFailed(String),
    #[error("setup failed: {0}")]
    SetupFailed(String),
    #[error("worktree failed: {0}")]
    WorktreeFailed(String),
    #[error("goose error: {0}")]
    GooseError(String),
    #[error("task not found: {0}")]
    TaskNotFound(String),
    #[error("cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct SlotInfo {
    pub slot_id: usize,
    pub model_id: String,
    pub state: SlotState,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SlotState {
    Free,
    Busy {
        task_id: String,
        started_at: String,
        agent_type: String,
    },
    Draining,
}

#[derive(Debug, Clone)]
pub struct ModelSlotConfig {
    pub model_id: String,
    pub max_slots: u32,
    pub roles: HashSet<String>,
}

#[derive(Debug, Clone)]
pub struct SlotPoolConfig {
    pub models: Vec<ModelSlotConfig>,
    pub role_priorities: HashMap<String, Vec<String>>,
}

// ─── Constants ───────────────────────────────────────────────────────────────

pub(crate) const MERGE_CONFLICT_PREFIX: &str = "merge_conflict:";
pub(crate) const MERGE_VALIDATION_PREFIX: &str = "merge_validation_failed:";

// ─── Shared metadata structs ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MergeConflictMetadata {
    pub(crate) conflicting_files: Vec<String>,
    pub(crate) base_branch: String,
    pub(crate) merge_target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MergeValidationFailureMetadata {
    pub(crate) base_branch: String,
    pub(crate) merge_target: String,
    pub(crate) command: String,
    pub(crate) cwd: String,
    pub(crate) exit_code: i32,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl MergeValidationFailureMetadata {
    pub(crate) fn as_prompt_context(&self) -> String {
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

// ─── Utility functions ────────────────────────────────────────────────────────

pub(crate) fn log_snippet(text: &str, max_chars: usize) -> String {
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

pub(crate) fn format_command_names(json: &str) -> Option<String> {
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

pub(crate) fn runtime_fs_diagnostics(project_path: &str, worktree_path: &Path) -> String {
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

pub(crate) fn runtime_env_diagnostics(
    session_id: &str,
    project_path: &str,
    worktree_path: &Path,
) -> String {
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

// ─── Token helpers ────────────────────────────────────────────────────────────

pub(crate) async fn tokens_for_session(
    goose_session_id: &str,
    session_manager: &Arc<SessionManager>,
) -> (i64, i64) {
    let session = session_manager.get_session(goose_session_id, false).await;
    let Ok(session) = session else {
        if let Some(tokens) = tokens_from_goose_sqlite(goose_session_id).await {
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
        && let Some(tokens) = tokens_from_goose_sqlite(goose_session_id).await
    {
        return tokens;
    }

    (tokens_in, tokens_out)
}

pub(crate) async fn tokens_from_goose_sqlite(goose_session_id: &str) -> Option<(i64, i64)> {
    for db_path in goose_session_db_candidates() {
        let Some(tokens) = tokens_from_goose_sqlite_at(&db_path, goose_session_id).await else {
            continue;
        };
        return Some(tokens);
    }
    None
}

pub(crate) async fn last_assistant_text_from_goose_sqlite(
    goose_session_id: &str,
) -> Option<String> {
    for db_path in goose_session_db_candidates() {
        let Some(text) =
            last_assistant_text_from_goose_sqlite_at(&db_path, goose_session_id).await
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

pub(crate) async fn tokens_from_goose_sqlite_at(
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

// ─── Session record helpers ───────────────────────────────────────────────────

pub(crate) async fn update_session_record(
    record_id: Option<&str>,
    status: SessionStatus,
    tokens_in: i64,
    tokens_out: i64,
    app_state: &AppState,
) {
    let Some(record_id) = record_id else {
        return;
    };
    let repo = SessionRepository::new(app_state.db().clone(), app_state.events().clone());
    if let Err(e) = repo.update(record_id, status, tokens_in, tokens_out).await {
        tracing::warn!(record_id = %record_id, error = %e, "failed to update session record");
    }
}

pub(crate) async fn update_session_record_paused(
    record_id: Option<&str>,
    tokens_in: i64,
    tokens_out: i64,
    app_state: &AppState,
) {
    let Some(record_id) = record_id else {
        return;
    };
    let repo = SessionRepository::new(app_state.db().clone(), app_state.events().clone());
    if let Err(e) = repo.pause(record_id, tokens_in, tokens_out).await {
        tracing::warn!(record_id = %record_id, error = %e, "failed to pause session record");
    }
}

// ─── Task / project helpers ───────────────────────────────────────────────────

pub(crate) async fn load_task(
    task_id: &str,
    app_state: &AppState,
) -> anyhow::Result<Task> {
    let repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
    let task = repo
        .get(task_id)
        .await
        .map_err(|e| anyhow::anyhow!("db error loading task: {e}"))?;
    task.ok_or_else(|| anyhow::anyhow!("task not found: {task_id}"))
}

pub(crate) async fn default_target_branch(project_id: &str, app_state: &AppState) -> String {
    let repo = ProjectRepository::new(app_state.db().clone(), app_state.events().clone());
    if let Ok(Some(config)) = repo.get_config(project_id).await {
        return config.target_branch;
    }
    "main".to_string()
}

pub(crate) async fn project_path_for_id(project_id: &str, app_state: &AppState) -> Option<String> {
    sqlx::query_scalar::<_, String>("SELECT path FROM projects WHERE id = ?1")
        .bind(project_id)
        .fetch_optional(app_state.db().pool())
        .await
        .ok()
        .flatten()
}

pub(crate) async fn find_paused_session_record(
    task_id: &str,
    app_state: &AppState,
) -> Option<SessionRecord> {
    let repo = SessionRepository::new(app_state.db().clone(), app_state.events().clone());
    repo.paused_for_task(task_id).await.ok().flatten()
}

pub(crate) async fn active_epic_batch_for_task(
    task_id: &str,
    app_state: &AppState,
) -> Option<String> {
    let repo =
        EpicReviewBatchRepository::new(app_state.db().clone(), app_state.events().clone());
    repo.active_batch_for_task(task_id)
        .await
        .ok()
        .flatten()
        .map(|b| b.id)
}

pub(crate) async fn conflict_context_for_dispatch(
    task_id: &str,
    app_state: &AppState,
) -> Option<MergeConflictMetadata> {
    let repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
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
    parse_conflict_metadata(reason)
}

pub(crate) async fn merge_validation_context_for_dispatch(
    task_id: &str,
    app_state: &AppState,
) -> Option<String> {
    let repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
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
    let metadata = parse_merge_validation_metadata(reason)?;
    Some(metadata.as_prompt_context())
}

pub(crate) fn parse_conflict_metadata(reason: &str) -> Option<MergeConflictMetadata> {
    let raw = reason.strip_prefix(MERGE_CONFLICT_PREFIX)?;
    serde_json::from_str(raw).ok()
}

pub(crate) fn parse_merge_validation_metadata(
    reason: &str,
) -> Option<MergeValidationFailureMetadata> {
    let raw = reason.strip_prefix(MERGE_VALIDATION_PREFIX)?;
    serde_json::from_str(raw).ok()
}

pub(crate) async fn resume_context_for_task(task_id: &str, app_state: &AppState) -> String {
    let repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
    let activity = repo.list_activity(task_id).await.ok().unwrap_or_default();

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

    if let Some(context) = merge_validation_context_for_dispatch(task_id, app_state).await {
        return context;
    }

    for entry in activity.iter().rev() {
        if entry.event_type == "merge_conflict" {
            if let Ok(meta) = serde_json::from_str::<MergeConflictMetadata>(&entry.payload) {
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

    "Your previous submission needs revision. Review your work, address any issues, then emit:\nWORKER_RESULT: DONE".to_string()
}

// ─── Transition helpers ───────────────────────────────────────────────────────

pub(crate) async fn transition_start(
    task: &Task,
    agent_type: AgentType,
    app_state: &AppState,
) -> anyhow::Result<()> {
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
        let repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
        repo.transition(&task.id, action, "agent-supervisor", "system", None, None)
            .await
            .map_err(|e| anyhow::anyhow!("task transition failed for {}: {e}", task.id))?;
    }
    Ok(())
}

pub(crate) async fn transition_interrupted(
    task_id: &str,
    agent_type: AgentType,
    reason: &str,
    app_state: &AppState,
) {
    let action = match agent_type {
        AgentType::Worker | AgentType::ConflictResolver => Some(TransitionAction::Release),
        AgentType::TaskReviewer => Some(TransitionAction::ReleaseTaskReview),
        AgentType::EpicReviewer => None,
    };

    let Some(action) = action else {
        return;
    };

    let repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
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

// ─── Provider helpers ─────────────────────────────────────────────────────────

pub(crate) fn canonical_provider_id(id: &str) -> String {
    id.chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

pub(crate) async fn resolve_goose_provider_id(provider_id: &str) -> String {
    let entries = providers::providers().await;
    if let Some((meta, _)) = entries.iter().find(|(meta, _)| meta.name == provider_id) {
        return meta.name.clone();
    }
    let canonical = canonical_provider_id(provider_id);
    entries
        .iter()
        .find(|(meta, _)| canonical_provider_id(&meta.name) == canonical)
        .map(|(meta, _)| meta.name.clone())
        .unwrap_or_else(|| provider_id.to_string())
}

pub(crate) async fn provider_supports_oauth(
    provider_id: &str,
    goose_provider_id: &str,
) -> bool {
    let entries = providers::providers().await;
    entries
        .iter()
        .find(|(meta, _)| meta.name == goose_provider_id)
        .map(|(meta, _)| meta.config_keys.iter().any(|k| k.oauth_flow))
        .unwrap_or(false)
}

pub(crate) async fn load_provider_api_key(
    provider_id: &str,
    app_state: &AppState,
) -> anyhow::Result<(String, String)> {
    let key_name = app_state
        .catalog()
        .list_providers()
        .into_iter()
        .find(|p| p.id == provider_id)
        .and_then(|p| p.env_vars.into_iter().next())
        .unwrap_or_else(|| format!("{}_API_KEY", provider_id.to_ascii_uppercase()));

    let repo = CredentialRepository::new(app_state.db().clone(), app_state.events().clone());
    let key = repo
        .get_decrypted(&key_name)
        .await
        .map_err(|e| anyhow::anyhow!("credential lookup failed: {e}"))?;

    match key {
        Some(v) => Ok((key_name, v)),
        None => Err(anyhow::anyhow!(
            "no credential stored for provider {provider_id} (expected key {key_name})"
        )),
    }
}

pub(crate) fn parse_model_id(model_id: &str) -> anyhow::Result<(String, String)> {
    let Some((provider_id, model_name)) = model_id.split_once('/') else {
        return Err(anyhow::anyhow!("invalid model id '{model_id}', expected provider/model"));
    };
    Ok((provider_id.to_owned(), model_name.to_owned()))
}

pub(crate) fn extensions_for(agent_type: AgentType) -> Vec<goose::config::ExtensionConfig> {
    vec![extension::config(agent_type)]
}

pub(crate) fn agent_type_for_task(task: &Task, has_conflict_context: bool) -> AgentType {
    match task.status.as_str() {
        "needs_task_review" | "in_task_review" => AgentType::TaskReviewer,
        "open" if has_conflict_context => AgentType::ConflictResolver,
        _ => AgentType::Worker,
    }
}

// ─── Git / worktree helpers ───────────────────────────────────────────────────

pub(crate) async fn prepare_worktree(
    project_dir: &PathBuf,
    task: &Task,
    app_state: &AppState,
) -> anyhow::Result<PathBuf> {
    let branch = format!("task/{}", task.short_id);
    let target_branch = default_target_branch(&task.project_id, app_state).await;
    let git = app_state
        .git_actor(project_dir)
        .await
        .map_err(|e| anyhow::anyhow!("git actor: {e}"))?;

    let stale_worktree_path = project_dir
        .join(".djinn")
        .join("worktrees")
        .join(&task.short_id);

    let session_repo =
        SessionRepository::new(app_state.db().clone(), app_state.events().clone());
    let has_paused_session = session_repo
        .paused_for_task(&task.id)
        .await
        .ok()
        .flatten()
        .is_some();
    if has_paused_session
        && stale_worktree_path.exists()
        && stale_worktree_path.join(".git").exists()
    {
        tracing::info!(
            task_id = %task.short_id,
            worktree = %stale_worktree_path.display(),
            "Lifecycle: reusing existing worktree from paused session"
        );
        return Ok(stale_worktree_path);
    }

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
        Err(e) => return Err(anyhow::anyhow!("show-ref failed: {e}")),
    };

    if !branch_exists {
        git.create_branch(&task.short_id, &target_branch)
            .await
            .map_err(|e| anyhow::anyhow!("create branch: {e}"))?;
    } else {
        try_rebase_existing_task_branch(project_dir, &branch, &target_branch, app_state).await;
    }

    git.create_worktree(&task.short_id, &branch, false)
        .await
        .map_err(|e| anyhow::anyhow!("create worktree: {e}"))
}

pub(crate) async fn prepare_epic_reviewer_worktree(
    project_dir: &PathBuf,
    batch_id: &str,
    app_state: &AppState,
) -> anyhow::Result<PathBuf> {
    let git = app_state
        .git_actor(project_dir)
        .await
        .map_err(|e| anyhow::anyhow!("git actor: {e}"))?;

    let folder_name = format!("batch-{batch_id}");
    let stale_path = project_dir
        .join(".djinn")
        .join("worktrees")
        .join(&folder_name);
    let _ = git.remove_worktree(&stale_path).await;
    if stale_path.exists() {
        let _ = std::fs::remove_dir_all(&stale_path);
    }

    git.create_worktree(&folder_name, "HEAD", true)
        .await
        .map_err(|e| anyhow::anyhow!("create epic reviewer worktree: {e}"))
}

pub(crate) async fn try_rebase_existing_task_branch(
    project_dir: &Path,
    branch: &str,
    target_branch: &str,
    app_state: &AppState,
) {
    let git = match app_state.git_actor(project_dir).await {
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

    let sync_git = match app_state.git_actor(&sync_worktree_path).await {
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

pub(crate) async fn commit_wip_if_needed(
    task_id: &str,
    worktree_path: &PathBuf,
    app_state: &AppState,
) {
    let git = match app_state.git_actor(worktree_path).await {
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

pub(crate) async fn commit_final_work_if_needed(
    task_id: &str,
    worktree_path: &Path,
    app_state: &AppState,
) -> Result<(), String> {
    let git = app_state
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

pub(crate) async fn cleanup_worktree(
    task_id: &str,
    worktree_path: &Path,
    app_state: &AppState,
) {
    let session_repo =
        SessionRepository::new(app_state.db().clone(), app_state.events().clone());
    if let Ok(Some(paused)) = session_repo.paused_for_task(task_id).await {
        if paused.worktree_path.as_deref() == Some(worktree_path.to_str().unwrap_or("")) {
            tracing::info!(
                task_id = %task_id,
                worktree = %worktree_path.display(),
                "Lifecycle: skipping worktree cleanup — paused session still references it"
            );
            return;
        }
    }

    let task = match load_task(task_id, app_state).await {
        Ok(task) => task,
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "failed to load task for worktree cleanup");
            return;
        }
    };

    let Some(project_path) = project_path_for_id(&task.project_id, app_state).await else {
        tracing::warn!(task_id = %task_id, "project path not found for worktree cleanup");
        return;
    };

    let git = match app_state.git_actor(Path::new(&project_path)).await {
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

// ─── Command helpers ──────────────────────────────────────────────────────────

pub(crate) async fn run_setup_commands_checked(
    task_id: &str,
    worktree_path: &Path,
    app_state: &AppState,
) -> Option<String> {
    let task = load_task(task_id, app_state).await.ok()?;
    let project_repo = ProjectRepository::new(app_state.db().clone(), app_state.events().clone());
    let project = project_repo.get(&task.project_id).await.ok()??;
    let specs: Vec<CommandSpec> =
        serde_json::from_str(&project.setup_commands).unwrap_or_default();
    if specs.is_empty() {
        return None;
    }
    tracing::info!(
        task_id = %task_id,
        command_count = specs.len(),
        "Lifecycle: running setup commands"
    );
    match run_commands(&specs, worktree_path).await {
        Ok(results) => {
            let failed = results.iter().find(|r| r.exit_code != 0)?;
            tracing::info!(
                task_id = %task_id,
                command = %failed.name,
                exit_code = failed.exit_code,
                "Lifecycle: setup command failed"
            );
            let trim_output = |s: &str| -> String {
                let lines: Vec<&str> = s.trim().lines().collect();
                if lines.len() > 50 {
                    format!(
                        "... ({} lines truncated) ...\n{}",
                        lines.len() - 50,
                        lines[lines.len() - 50..].join("\n")
                    )
                } else {
                    lines.join("\n")
                }
            };
            Some(format!(
                "Setup command '{}' failed with exit code {}.\n\nYour changes likely broke a setup step (e.g. lockfile out of sync with package.json). Use your shell tools to fix the issue, then signal WORKER_RESULT: DONE.\n\nstdout:\n{}\nstderr:\n{}",
                failed.name,
                failed.exit_code,
                trim_output(&failed.stdout),
                trim_output(&failed.stderr),
            ))
        }
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: setup command system error");
            Some(format!(
                "Setup commands could not run: {e}\n\nFix the issue and signal WORKER_RESULT: DONE when complete."
            ))
        }
    }
}

pub(crate) async fn run_verification_commands(
    task_id: &str,
    worktree_path: &Path,
    app_state: &AppState,
) -> Option<String> {
    let task = load_task(task_id, app_state).await.ok()?;
    let project_repo = ProjectRepository::new(app_state.db().clone(), app_state.events().clone());
    let project = project_repo.get(&task.project_id).await.ok()??;
    let specs: Vec<CommandSpec> =
        serde_json::from_str(&project.verification_commands).unwrap_or_default();
    if specs.is_empty() {
        return None;
    }
    tracing::info!(
        task_id = %task_id,
        command_count = specs.len(),
        "Lifecycle: running verification commands"
    );
    match run_commands(&specs, worktree_path).await {
        Ok(results) => {
            let failed = results.iter().find(|r| r.exit_code != 0)?;
            tracing::info!(
                task_id = %task_id,
                command = %failed.name,
                exit_code = failed.exit_code,
                "Lifecycle: verification command failed"
            );
            let trim_output = |s: &str| -> String {
                let lines: Vec<&str> = s.trim().lines().collect();
                if lines.len() > 50 {
                    format!(
                        "... ({} lines truncated) ...\n{}",
                        lines.len() - 50,
                        lines[lines.len() - 50..].join("\n")
                    )
                } else {
                    lines.join("\n")
                }
            };
            Some(format!(
                "Verification command '{}' failed with exit code {}.\n\nUse your shell and editor tools to inspect and fix the issue, then signal WORKER_RESULT: DONE.\n\nstdout:\n{}\nstderr:\n{}",
                failed.name,
                failed.exit_code,
                trim_output(&failed.stdout),
                trim_output(&failed.stderr),
            ))
        }
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: verification command system error");
            Some(format!(
                "Verification commands could not run: {e}\n\nFix the issue and signal WORKER_RESULT: DONE when complete."
            ))
        }
    }
}

// ─── Output parser helpers ────────────────────────────────────────────────────

pub(crate) fn missing_required_marker(agent_type: AgentType, output: &ParsedAgentOutput) -> bool {
    match agent_type {
        AgentType::Worker | AgentType::ConflictResolver => output.worker_signal.is_none(),
        AgentType::TaskReviewer => output.reviewer_verdict.is_none(),
        AgentType::EpicReviewer => output.epic_verdict.is_none(),
    }
}

pub(crate) fn missing_marker_nudge(
    agent_type: AgentType,
    output: &ParsedAgentOutput,
) -> Option<&'static str> {
    if !missing_required_marker(agent_type, output) {
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

// ─── Epic review helpers ──────────────────────────────────────────────────────

pub(crate) async fn merge_after_task_review(
    task_id: &str,
    app_state: &AppState,
) -> Option<(TransitionAction, Option<String>)> {
    let repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
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

    let project_dir = project_path_for_id(&task.project_id, app_state)
        .await
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let git = match app_state.git_actor(&project_dir).await {
        Ok(git) => git,
        Err(e) => {
            return Some((
                TransitionAction::ReleaseTaskReview,
                Some(format!("failed to open git actor for merge: {e}")),
            ));
        }
    };

    let base_branch = format!("task/{}", task.short_id);
    let merge_target = default_target_branch(&task.project_id, app_state).await;
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
                "Lifecycle: post-review squash merge succeeded"
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
            cleanup_paused_worker_session(task_id, app_state).await;
            Some((TransitionAction::TaskReviewApprove, None))
        }
        Err(GitError::MergeConflict { files, .. }) => {
            tracing::warn!(
                task_id = %task.short_id,
                task_uuid = %task.id,
                conflict_count = files.len(),
                conflicting_files = ?files,
                "Lifecycle: post-review merge conflict"
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
                exit_code = code,
                command = %command,
                "Lifecycle: post-review merge commit rejected"
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
                error = %e,
                "Lifecycle: post-review squash merge failed"
            );
            Some((
                TransitionAction::ReleaseTaskReview,
                Some(format!("post-review squash merge failed: {e} ({e:?})")),
            ))
        }
    }
}

pub(crate) async fn finalize_epic_batch(
    task_id: &str,
    output: &ParsedAgentOutput,
    error_reason: Option<&str>,
    app_state: &AppState,
) {
    let Some(batch_id) = active_epic_batch_for_task(task_id, app_state).await else {
        return;
    };
    let task_repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
    let Some(task) = task_repo.get(task_id).await.ok().flatten() else {
        return;
    };
    let Some(epic_id) = task.epic_id.as_deref() else {
        return;
    };

    let batch_repo =
        EpicReviewBatchRepository::new(app_state.db().clone(), app_state.events().clone());
    let epic_repo = EpicRepository::new(app_state.db().clone(), app_state.events().clone());

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

pub(crate) async fn cleanup_paused_worker_session(task_id: &str, app_state: &AppState) {
    let repo = SessionRepository::new(app_state.db().clone(), app_state.events().clone());
    let Ok(Some(paused)) = repo.paused_for_task(task_id).await else {
        return;
    };

    let (tokens_in, tokens_out) = if let Some(ref gsid) = paused.goose_session_id {
        // Best effort — use stored tokens if sqlite unavailable
        let from_sqlite = tokens_from_goose_sqlite(gsid).await;
        from_sqlite.unwrap_or((paused.tokens_in, paused.tokens_out))
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
        cleanup_worktree(task_id, &worktree_path, app_state).await;
    }
}

pub(crate) async fn interrupt_paused_worker_session(task_id: &str, app_state: &AppState) {
    let repo = SessionRepository::new(app_state.db().clone(), app_state.events().clone());
    let Ok(Some(paused)) = repo.paused_for_task(task_id).await else {
        return;
    };
    if let Err(e) = repo
        .update(
            &paused.id,
            SessionStatus::Interrupted,
            paused.tokens_in,
            paused.tokens_out,
        )
        .await
    {
        tracing::warn!(
            task_id = %task_id,
            record_id = %paused.id,
            error = %e,
            "failed to interrupt paused worker session after reviewer rejection"
        );
    } else {
        tracing::info!(
            task_id = %task_id,
            record_id = %paused.id,
            goose_session_id = paused.goose_session_id.as_deref().unwrap_or("<none>"),
            "Lifecycle: interrupted paused worker session after reviewer rejection"
        );
    }
}

// ─── Success transition ───────────────────────────────────────────────────────

pub(crate) async fn success_transition(
    task_id: &str,
    agent_type: AgentType,
    output: &ParsedAgentOutput,
    app_state: &AppState,
) -> Option<(TransitionAction, Option<String>)> {
    match agent_type {
        AgentType::Worker | AgentType::ConflictResolver => match output.worker_signal {
            Some(WorkerSignal::Done) => Some((TransitionAction::SubmitTaskReview, None)),
            None => {
                let reason = output.runtime_error.clone().unwrap_or_else(|| {
                    "worker session completed without DONE marker".to_string()
                });
                tracing::warn!(reason = %reason, "worker session completed without structured result marker");
                Some((TransitionAction::Release, Some(reason)))
            }
        },
        AgentType::TaskReviewer => match output.reviewer_verdict {
            Some(ReviewerVerdict::Verified) => {
                merge_after_task_review(task_id, app_state).await
            }
            Some(ReviewerVerdict::Reopen) => Some((
                TransitionAction::TaskReviewReject,
                Some(
                    output
                        .reviewer_feedback
                        .clone()
                        .unwrap_or_else(|| "reviewer requested REOPEN".to_string()),
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

// ─── Reply loop sub-function ──────────────────────────────────────────────────

/// Compaction signal returned by the reply loop when the 80% threshold is hit.
struct CompactionSignal {
    session_id: String,
    tokens_in: i64,
    context_window: i64,
}

/// Runs the Goose reply loop for one session turn. Returns the result, the
/// accumulated output, and an optional compaction signal (if the 80% context
/// window threshold was reached mid-stream). The caller should compact and
/// restart the loop if a compaction signal is returned.
///
/// When `cancel` is triggered, the loop exits and returns `Err("cancelled")`.
#[allow(clippy::too_many_arguments)]
async fn run_reply_loop(
    agent: &GooseAgent,
    session_id: &str,
    task_id: &str,
    project_path: &str,
    worktree_path: &Path,
    agent_type: AgentType,
    kickoff: GooseMessage,
    cancel: &CancellationToken,
    global_cancel: &CancellationToken,
    app_state: &AppState,
    context_window: i64,
    session_manager: &Arc<SessionManager>,
) -> (anyhow::Result<()>, ParsedAgentOutput, Option<CompactionSignal>) {
    let mut output = ParsedAgentOutput::new(agent_type);
    let mut compaction_signal: Option<CompactionSignal> = None;

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

        'outer: while let Some(next_message) = pending_message.take() {
            let env_diag = runtime_env_diagnostics(session_id, project_path, worktree_path);
            tracing::info!(
                task_id = %task_id,
                session_id = %session_id,
                worktree = %worktree_path.display(),
                "Lifecycle: starting Goose reply; {}",
                env_diag
            );

            let mut stream = agent
                .reply(
                    next_message,
                    GooseSessionConfig {
                        id: session_id.to_owned(),
                        schedule_id: None,
                        max_turns: Some(300),
                        retry_config: None,
                    },
                    Some(cancel.clone()),
                )
                .await
                .map_err(|e| {
                    let diag = runtime_fs_diagnostics(project_path, worktree_path);
                    let env_diag = runtime_env_diagnostics(session_id, project_path, worktree_path);
                    anyhow::anyhow!(
                        "agent reply init failed: display={} debug={:?}; {}; {}",
                        e, e, diag, env_diag
                    )
                })?;

            let mut interrupted: Option<&'static str> = None;
            let mut saw_round_event = false;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
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
                            let diag = runtime_fs_diagnostics(project_path, worktree_path);
                            let env_diag = runtime_env_diagnostics(session_id, project_path, worktree_path);
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

                            // Token tracking + compaction threshold check.
                            {
                                let goose_session = session_manager.get_session(session_id, false).await;
                                let (tokens_in, tokens_out) = if let Ok(s) = goose_session {
                                    let ti = s.accumulated_input_tokens
                                        .or(s.input_tokens)
                                        .unwrap_or(0) as i64;
                                    let to = s.accumulated_output_tokens
                                        .or(s.output_tokens)
                                        .unwrap_or(0) as i64;
                                    (ti, to)
                                } else {
                                    tokens_from_goose_sqlite(session_id).await.unwrap_or((0, 0))
                                };
                                let usage_pct = if context_window > 0 {
                                    tokens_in as f64 / context_window as f64
                                } else {
                                    0.0
                                };
                                let _ = app_state.events().send(DjinnEvent::SessionTokenUpdate {
                                    session_id: session_id.to_owned(),
                                    task_id: task_id.to_owned(),
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
                                        "Lifecycle: compaction threshold reached; breaking reply loop"
                                    );
                                    compaction_signal = Some(CompactionSignal {
                                        session_id: session_id.to_owned(),
                                        tokens_in,
                                        context_window,
                                    });
                                    // Break out of both loops — compaction will restart with a fresh session.
                                    break 'outer;
                                }
                            }
                        }
                        extension::handle_event(app_state, agent, &evt, worktree_path).await;
                    }
                }
            }

            if let Some(reason) = interrupted {
                return Err(anyhow::anyhow!(reason));
            }

            if !saw_round_event {
                let diag = runtime_fs_diagnostics(project_path, worktree_path);
                return Err(anyhow::anyhow!(
                    "agent stream ended without any events; {}",
                    diag
                ));
            }
        }

        // If we broke out for compaction, skip the nudge / marker checks.
        if compaction_signal.is_some() {
            return Ok(());
        }

        if !saw_any_event {
            let diag = runtime_fs_diagnostics(project_path, worktree_path);
            return Err(anyhow::anyhow!("agent session produced no events; {}", diag));
        }

        // Send a nudge if the required marker is missing.
        if saw_any_tool_use && missing_required_marker(agent_type, &output) {
            if let Some(nudge) = missing_marker_nudge(agent_type, &output) {
                tracing::info!(
                    task_id = %task_id,
                    agent_type = %agent_type.as_str(),
                    "Lifecycle: session ended without required marker; sending post-session nudge"
                );
                let nudge_msg = GooseMessage::user().with_text(nudge);
                let mut stream = agent
                    .reply(
                        nudge_msg,
                        GooseSessionConfig {
                            id: session_id.to_owned(),
                            schedule_id: None,
                            max_turns: Some(3),
                            retry_config: None,
                        },
                        Some(cancel.clone()),
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("nudge reply init failed: {e}"))?;

                let assistant_role = GooseMessage::assistant().role;
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
                    extension::handle_event(app_state, agent, &evt, worktree_path).await;
                }
            }
        }

        if let Some(last_assistant_text) =
            last_assistant_text_from_goose_sqlite(session_id).await
        {
            output.ingest_text(&last_assistant_text);
            tracing::info!(
                task_id = %task_id,
                agent_type = %agent_type.as_str(),
                marker_present_after_persisted_check = !missing_required_marker(agent_type, &output),
                "Lifecycle: parsed persisted last assistant message before marker decision"
            );
        }

        if missing_required_marker(agent_type, &output) {
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
                "Lifecycle: required marker missing at session end"
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

    (run_result, output, compaction_signal)
}

// ─── Inline compaction ────────────────────────────────────────────────────────

struct CompactResult {
    new_session_id: String,
    new_record_id: String,
    agent: Arc<GooseAgent>,
    kickoff_summary: String,
}

/// Performs context compaction inline (without actor messaging). Creates a new
/// Goose session with a summary of the old one, and returns the new session info.
#[allow(clippy::too_many_arguments)]
async fn compact_inline(
    task_id: &str,
    agent_type: AgentType,
    project_id: &str,
    old_session_id: &str,
    old_record_id: Option<&str>,
    model_id: &str,
    goose_provider_id: &str,
    model_name: &str,
    worktree_path: &Path,
    context_window: i64,
    tokens_in: i64,
    session_manager: &Arc<SessionManager>,
    app_state: &AppState,
    resume_context: Option<&str>,
) -> Result<CompactResult, String> {
    // 1. Read conversation history + final token counts.
    let (final_tokens_in, final_tokens_out, messages) =
        match session_manager.get_session(old_session_id, true).await {
            Ok(s) => {
                let tin = s.accumulated_input_tokens.or(s.input_tokens).unwrap_or(0) as i64;
                let tout = s.accumulated_output_tokens.or(s.output_tokens).unwrap_or(0) as i64;
                let msgs = s
                    .conversation
                    .map(|c| c.messages().clone())
                    .unwrap_or_default();
                (tin.max(tokens_in), tout, msgs)
            }
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "compaction: failed to read Goose session");
                (tokens_in, 0, vec![])
            }
        };

    // 2. Finalize old Djinn session record.
    if let Some(record_id) = old_record_id {
        let repo = SessionRepository::new(app_state.db().clone(), app_state.events().clone());
        if let Err(e) = repo
            .update(record_id, SessionStatus::Compacted, final_tokens_in, final_tokens_out)
            .await
        {
            tracing::warn!(record_id = %record_id, error = %e, "compaction: failed to finalize old session record");
        }
    }

    let goose_model = ModelConfig::new(model_name)
        .map_err(|e| format!("compaction: failed to build ModelConfig: {e}"))?
        .with_canonical_limits(goose_provider_id);

    // 3. Generate summary.
    let summary = if messages.is_empty() {
        tracing::warn!(task_id = %task_id, "compaction: empty conversation history; using fallback summary");
        "Context window was compacted. Please review the current state of the worktree and continue the task.".to_string()
    } else {
        let compaction_system = crate::agent::prompts::render_compaction_prompt();
        let summary_provider =
            providers::create(goose_provider_id, goose_model.clone(), vec![])
                .await
                .map_err(|e| {
                    app_state.health_tracker().record_failure(model_id);
                    format!("compaction: summary provider creation failed: {e}")
                })?;
        let model_config = summary_provider.get_model_config();
        summary_provider
            .complete(
                &model_config,
                old_session_id,
                compaction_system,
                &messages,
                &[],
            )
            .await
            .map(|(msg, _)| {
                tracing::info!(task_id = %task_id, "compaction: summary generated successfully");
                msg.as_concat_text()
            })
            .map_err(|e| {
                format!("compaction: summary generation failed: {e}")
            })?
    };

    // 4. Create new Goose session.
    let task_name = {
        let task_repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
        match task_repo.get(task_id).await {
            Ok(Some(t)) => format!("{} {} (compacted)", t.short_id, t.title),
            _ => format!("{task_id} (compacted)"),
        }
    };
    let new_goose_session = session_manager
        .create_session(worktree_path.to_owned(), task_name, SessionType::SubAgent)
        .await
        .map_err(|e| format!("compaction: failed to create new Goose session: {e}"))?;

    // 5. Create new Djinn session record.
    let session_repo = SessionRepository::new(app_state.db().clone(), app_state.events().clone());
    let new_record = session_repo
        .create(
            project_id,
            task_id,
            model_id,
            agent_type.as_str(),
            worktree_path.to_str(),
            Some(&new_goose_session.id),
            old_record_id,
        )
        .await
        .map_err(|e| format!("compaction: failed to create new session record: {e}"))?;

    // Log compaction activity.
    {
        let task_repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
        let usage_pct = if context_window > 0 {
            final_tokens_in as f64 / context_window as f64
        } else {
            0.0
        };
        let payload = serde_json::json!({
            "old_session_id": old_record_id.unwrap_or(""),
            "new_session_id": new_record.id,
            "tokens_in_at_compaction": final_tokens_in,
            "context_window": context_window,
            "usage_pct": usage_pct,
            "summary_token_count": summary.chars().count(),
        })
        .to_string();
        if let Err(e) = task_repo
            .log_activity(Some(task_id), "system", "system", "compaction", &payload)
            .await
        {
            tracing::warn!(task_id = %task_id, error = %e, "compaction: failed to log activity");
        }
    }

    // 6. Set up new agent.
    let extensions = extensions_for(agent_type);
    let provider = providers::create(goose_provider_id, goose_model, extensions.clone())
        .await
        .map_err(|e| {
            app_state.health_tracker().record_failure(model_id);
            format!("compaction: failed to create new agent provider: {e}")
        })?;

    let agent = Arc::new(GooseAgent::with_config(GooseAgentConfig::new(
        session_manager.clone(),
        PermissionManager::instance(),
        None,
        GooseMode::Auto,
        true,
        GoosePlatform::GooseCli,
    )));

    agent
        .update_provider(provider, &new_goose_session.id)
        .await
        .map_err(|e| {
            app_state.health_tracker().record_failure(model_id);
            format!("compaction: failed to set provider on new agent: {e}")
        })?;

    for ext in extensions {
        if let Err(e) = agent.add_extension(ext, &new_goose_session.id).await {
            tracing::warn!(task_id = %task_id, error = %e, "compaction: failed to add extension");
        }
    }

    let kickoff_summary = match resume_context {
        Some(ctx) => format!("{summary}\n\n---\n\n{ctx}"),
        None => summary,
    };

    Ok(CompactResult {
        new_session_id: new_goose_session.id,
        new_record_id: new_record.id,
        agent,
        kickoff_summary,
    })
}

// ─── Main task lifecycle function ─────────────────────────────────────────────

/// Standalone async function that runs the full per-task lifecycle:
/// load → worktree → session → reply loop → verification → post-session work → cleanup.
///
/// Compaction is handled as an inline loop (no supervisor messages). The reply
/// loop returns its result directly instead of sending SessionCompleted back to
/// an actor.
///
/// Sends `SlotEvent::Free` on normal completion and `SlotEvent::Killed` when
/// cancelled via `cancel`.
pub async fn run_task_lifecycle(
    task_id: String,
    project_path: String,
    model_id: String,
    app_state: AppState,
    session_manager: Arc<SessionManager>,
    cancel: CancellationToken,
    event_tx: mpsc::Sender<SlotEvent>,
) -> anyhow::Result<()> {
    // Helper macros for emitting slot events on exit.
    macro_rules! return_free {
        () => {{
            let _ = event_tx
                .send(SlotEvent::Free {
                    slot_id: 0,
                    model_id: model_id.clone(),
                    task_id: task_id.clone(),
                })
                .await;
            return Ok(());
        }};
    }
    macro_rules! return_killed {
        () => {{
            let _ = event_tx
                .send(SlotEvent::Killed {
                    slot_id: 0,
                    model_id: model_id.clone(),
                    task_id: task_id.clone(),
                })
                .await;
            return Ok(());
        }};
    }

    if cancel.is_cancelled() {
        return_killed!();
    }

    // ── Load task ──────────────────────────────────────────────────────────────
    let task = match load_task(&task_id, &app_state).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to load task");
            return_free!();
        }
    };

    // ── Determine agent type and context ──────────────────────────────────────
    let active_batch = active_epic_batch_for_task(&task.id, &app_state).await;
    let conflict_ctx = conflict_context_for_dispatch(&task.id, &app_state).await;
    let merge_validation_ctx = merge_validation_context_for_dispatch(&task.id, &app_state).await;
    let agent_type = if active_batch.is_some() {
        AgentType::EpicReviewer
    } else {
        agent_type_for_task(&task, conflict_ctx.is_some())
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
        "Lifecycle: dispatch accepted; preparing session"
    );

    // ── Transition task to in-progress ────────────────────────────────────────
    if let Err(e) = transition_start(&task, agent_type, &app_state).await {
        tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: transition_start failed");
        return_free!();
    }

    // ── Parse model ID and load credentials ───────────────────────────────────
    let (catalog_provider_id, model_name) = match parse_model_id(&model_id) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: invalid model ID");
            transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
            return_free!();
        }
    };
    let goose_provider_id = resolve_goose_provider_id(&catalog_provider_id).await;

    if !provider_supports_oauth(&catalog_provider_id, &goose_provider_id).await {
        match load_provider_api_key(&catalog_provider_id, &app_state).await {
            Ok((key_name, api_key)) => {
                if let Err(e) = GooseConfig::global().set_secret(&key_name, &api_key) {
                    tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to set API key");
                    transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
                    return_free!();
                }
            }
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: missing credential");
                transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
                return_free!();
            }
        }
    }

    // ── Prepare worktree ───────────────────────────────────────────────────────
    let session_name = format!("{} {}", task.short_id, task.title);
    let project_dir = PathBuf::from(&project_path);
    let worktree_path = if agent_type == AgentType::EpicReviewer {
        let batch_id = active_batch.as_deref().unwrap_or_default();
        match prepare_epic_reviewer_worktree(&project_dir, batch_id, &app_state).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: prepare_epic_reviewer_worktree failed");
                transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
                return_free!();
            }
        }
    } else {
        match prepare_worktree(&project_dir, &task, &app_state).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: prepare_worktree failed");
                transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
                return_free!();
            }
        }
    };

    // ── Conflict resolver: start merge for conflict markers ───────────────────
    if agent_type == AgentType::ConflictResolver {
        if let Some(ref ctx) = conflict_ctx {
            let target_ref = format!("origin/{}", ctx.merge_target);
            if let Ok(wt_git) = app_state.git_actor(&worktree_path).await {
                let _ = wt_git
                    .run_command(vec![
                        "fetch".into(),
                        "origin".into(),
                        ctx.merge_target.clone(),
                    ])
                    .await;
                let merge_result = wt_git
                    .run_command(vec!["merge".into(), target_ref.clone(), "--no-commit".into()])
                    .await;
                if merge_result.is_ok() {
                    let _ = wt_git
                        .run_command(vec!["merge".into(), "--abort".into()])
                        .await;
                } else {
                    tracing::info!(
                        task_id = %task.short_id,
                        target_ref = %target_ref,
                        "Lifecycle: started merge in worktree for conflict resolver"
                    );
                }
            }
        }
    }

    // ── Goose logs dir ────────────────────────────────────────────────────────
    let goose_logs_dir = goose::config::paths::Paths::in_state_dir("logs");
    if let Err(e) = std::fs::create_dir_all(&goose_logs_dir) {
        tracing::warn!(task_id = %task.short_id, path = %goose_logs_dir.display(), error = %e, "failed to ensure Goose logs directory");
    }
    if !worktree_path.exists() || !worktree_path.is_dir() {
        let diag = runtime_fs_diagnostics(&project_path, &worktree_path);
        tracing::warn!(task_id = %task_id, diag = %diag, "Lifecycle: worktree preflight failed");
        transition_interrupted(&task_id, agent_type, "worktree preflight failed", &app_state).await;
        return_free!();
    }

    // ── Project commands ──────────────────────────────────────────────────────
    let project_repo = ProjectRepository::new(app_state.db().clone(), app_state.events().clone());
    let (prompt_setup_commands, prompt_verification_commands) = {
        if let Ok(Some(ref p)) = project_repo.get(&task.project_id).await {
            let setup_names = format_command_names(&p.setup_commands);
            let verify_names = format_command_names(&p.verification_commands);
            (setup_names, verify_names)
        } else {
            (None, None)
        }
    };

    // ── Run setup commands before session ─────────────────────────────────────
    if let Ok(Some(project)) = project_repo.get(&task.project_id).await {
        let setup_specs: Vec<CommandSpec> =
            serde_json::from_str(&project.setup_commands).unwrap_or_default();
        if !setup_specs.is_empty() {
            let setup_start = std::time::Instant::now();
            tracing::info!(
                task_id = %task.short_id,
                command_count = setup_specs.len(),
                "Lifecycle: running setup commands"
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
                            "Lifecycle: setup command failed; releasing task"
                        );
                        let task_repo =
                            TaskRepository::new(app_state.db().clone(), app_state.events().clone());
                        let _ = task_repo
                            .transition(
                                &task.id,
                                TransitionAction::Release,
                                "agent-supervisor",
                                "system",
                                Some(&reason),
                                None,
                            )
                            .await;
                        cleanup_worktree(&task.id, &worktree_path, &app_state).await;
                        return_free!();
                    }
                    tracing::info!(
                        task_id = %task.short_id,
                        duration_ms = setup_start.elapsed().as_millis(),
                        "Lifecycle: setup commands completed"
                    );
                }
                Err(e) => {
                    let reason = format!("Setup commands error: {e}");
                    tracing::warn!(task_id = %task.short_id, error = %e, "Lifecycle: setup command error");
                    let task_repo =
                        TaskRepository::new(app_state.db().clone(), app_state.events().clone());
                    let _ = task_repo
                        .transition(
                            &task.id,
                            TransitionAction::Release,
                            "agent-supervisor",
                            "system",
                            Some(&reason),
                            None,
                        )
                        .await;
                    cleanup_worktree(&task.id, &worktree_path, &app_state).await;
                    return_free!();
                }
            }
        }
    }

    // ── Create Goose session ───────────────────────────────────────────────────
    let session = match session_manager
        .create_session(worktree_path.clone(), session_name, SessionType::SubAgent)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to create Goose session");
            transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
            cleanup_worktree(&task_id, &worktree_path, &app_state).await;
            return_free!();
        }
    };

    // ── Create Djinn session record ───────────────────────────────────────────
    let session_repo = SessionRepository::new(app_state.db().clone(), app_state.events().clone());
    let session_record = match session_repo
        .create(
            &task.project_id,
            &task.id,
            &model_id,
            agent_type.as_str(),
            worktree_path.to_str(),
            Some(session.id.as_str()),
            None,
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to create session record");
            transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
            cleanup_worktree(&task_id, &worktree_path, &app_state).await;
            return_free!();
        }
    };

    // Mark epic review batch as in_review.
    if agent_type == AgentType::EpicReviewer
        && let Some(batch_id) = active_batch.as_deref()
    {
        let batch_repo =
            EpicReviewBatchRepository::new(app_state.db().clone(), app_state.events().clone());
        if let Err(e) = batch_repo.mark_in_review(batch_id, &session.id).await {
            tracing::warn!(task_id = %task.short_id, batch_id = %batch_id, error = %e, "failed to mark epic review batch in_review");
        }
    }

    // ── Create agent ───────────────────────────────────────────────────────────
    let goose_model = match ModelConfig::new(&model_name) {
        Ok(m) => m.with_canonical_limits(&goose_provider_id),
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to build ModelConfig");
            transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
            cleanup_worktree(&task_id, &worktree_path, &app_state).await;
            return_free!();
        }
    };

    let exts = extensions_for(agent_type);
    let provider = match providers::create(&goose_provider_id, goose_model.clone(), exts.clone())
        .await
    {
        Ok(p) => p,
        Err(e) => {
            app_state.health_tracker().record_failure(&model_id);
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to create provider");
            transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
            cleanup_worktree(&task_id, &worktree_path, &app_state).await;
            return_free!();
        }
    };

    let agent = Arc::new(GooseAgent::with_config(GooseAgentConfig::new(
        session_manager.clone(),
        PermissionManager::instance(),
        None,
        GooseMode::Auto,
        true,
        GoosePlatform::GooseCli,
    )));

    if let Err(e) = agent.update_provider(provider, &session.id).await {
        app_state.health_tracker().record_failure(&model_id);
        tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to set provider");
        transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
        cleanup_worktree(&task_id, &worktree_path, &app_state).await;
        return_free!();
    }

    for ext in exts {
        if let Err(e) = agent.add_extension(ext, &session.id).await {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to add extension");
        }
    }

    // ── Build and set system prompt ───────────────────────────────────────────
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

    let context_window = app_state
        .catalog()
        .find_model(&model_id)
        .map(|m| m.context_window)
        .unwrap_or(0);

    // ── Main lifecycle loop (compaction + verification retry) ─────────────────
    let mut current_session_id = session.id.clone();
    let mut current_record_id = Some(session_record.id.clone());
    let mut current_agent = agent;
    let mut kickoff = GooseMessage::user().with_text(
        "Start by understanding the task context and execute it fully before stopping.",
    );

    let (final_result, final_output) = loop {
        let (reply_result, output, compaction_signal) = run_reply_loop(
            &current_agent,
            &current_session_id,
            &task_id,
            &project_path,
            &worktree_path,
            agent_type,
            kickoff.clone(),
            &cancel,
            &cancel, // global_cancel reuses task cancel (supervisor shuts down via same token)
            &app_state,
            context_window,
            &session_manager,
        )
        .await;

        // ── Handle cancellation ─────────────────────────────────────────────
        if cancel.is_cancelled() {
            tracing::info!(task_id = %task_id, "Lifecycle: cancelled; committing WIP and cleaning up");
            let (ti, to) = tokens_for_session(&current_session_id, &session_manager).await;
            update_session_record(
                current_record_id.as_deref(),
                SessionStatus::Interrupted,
                ti,
                to,
                &app_state,
            )
            .await;
            commit_wip_if_needed(&task_id, &worktree_path, &app_state).await;
            cleanup_worktree(&task_id, &worktree_path, &app_state).await;
            transition_interrupted(&task_id, agent_type, "session cancelled", &app_state).await;
            return_killed!();
        }

        // ── Handle compaction signal (80% threshold) ────────────────────────
        if let Some(sig) = compaction_signal {
            tracing::info!(
                task_id = %task_id,
                tokens_in = sig.tokens_in,
                context_window = sig.context_window,
                "Lifecycle: compaction threshold reached; running inline compaction"
            );
            match compact_inline(
                &task_id,
                agent_type,
                &task.project_id,
                &sig.session_id,
                current_record_id.as_deref(),
                &model_id,
                &goose_provider_id,
                &model_name,
                &worktree_path,
                sig.context_window,
                sig.tokens_in,
                &session_manager,
                &app_state,
                None,
            )
            .await
            {
                Ok(compact) => {
                    // Refresh system prompt on the new agent.
                    let new_prompt = render_prompt(
                        agent_type,
                        &task,
                        &TaskContext {
                            project_path: project_path.clone(),
                            workspace_path: worktree_path.display().to_string(),
                            diff: None, commits: None, start_commit: None,
                            end_commit: None, batch_num: None, task_count: None,
                            tasks_summary: None, common_labels: None,
                            conflict_files: None, merge_base_branch: None,
                            merge_target_branch: None, merge_failure_context: None,
                            setup_commands: None, verification_commands: None,
                        },
                    );
                    compact.agent.override_system_prompt(new_prompt).await;
                    current_session_id = compact.new_session_id;
                    current_record_id = Some(compact.new_record_id);
                    current_agent = compact.agent;
                    kickoff = GooseMessage::user().with_text(&compact.kickoff_summary);
                    continue;
                }
                Err(e) => {
                    tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: compaction failed; releasing task");
                    let (ti, to) = tokens_for_session(&current_session_id, &session_manager).await;
                    update_session_record(
                        current_record_id.as_deref(),
                        SessionStatus::Failed,
                        ti,
                        to,
                        &app_state,
                    )
                    .await;
                    cleanup_worktree(&task_id, &worktree_path, &app_state).await;
                    transition_interrupted(&task_id, agent_type, &e, &app_state).await;
                    return_free!();
                }
            }
        }

        // ── Handle context exhaustion (at session end) ──────────────────────
        let is_context_error = match &reply_result {
            Err(reason) => {
                let lower = reason.to_string().to_lowercase();
                lower.contains("context length exceeded")
                    || lower.contains("context_length_exceeded")
                    || lower.contains("context limit exceeded")
            }
            Ok(()) => output.runtime_error.as_deref().map_or(false, |e| {
                let lower = e.to_lowercase();
                lower.contains("context length exceeded")
                    || lower.contains("context limit exceeded")
            }),
        } || output.context_exhausted;

        if is_context_error {
            // Reviewers: compaction won't help (prompt too large). Block the task.
            if matches!(agent_type, AgentType::TaskReviewer | AgentType::EpicReviewer) {
                tracing::warn!(
                    task_id = %task_id,
                    agent_type = %agent_type.as_str(),
                    "Lifecycle: context_length_exceeded on reviewer — blocking task"
                );
                let (ti, to) = tokens_for_session(&current_session_id, &session_manager).await;
                update_session_record(
                    current_record_id.as_deref(),
                    SessionStatus::Failed,
                    ti,
                    to,
                    &app_state,
                )
                .await;
                cleanup_worktree(&task_id, &worktree_path, &app_state).await;
                app_state.health_tracker().record_failure(&model_id);
                app_state.persist_model_health_state().await;
                let repo =
                    TaskRepository::new(app_state.db().clone(), app_state.events().clone());
                let reason = "context_length_exceeded: review prompt too large for current model";
                let _ = repo
                    .transition(
                        &task_id,
                        TransitionAction::ReleaseTaskReview,
                        "agent-supervisor",
                        "system",
                        Some(reason),
                        None,
                    )
                    .await;
                return_free!();
            }

            // Worker: compact and retry.
            tracing::info!(
                task_id = %task_id,
                "Lifecycle: context exhaustion at session end; triggering fresh continuation"
            );
            let (ti, _) = tokens_for_session(&current_session_id, &session_manager).await;
            let cw = if context_window > 0 { context_window } else { 200_000 };
            match compact_inline(
                &task_id,
                agent_type,
                &task.project_id,
                &current_session_id,
                current_record_id.as_deref(),
                &model_id,
                &goose_provider_id,
                &model_name,
                &worktree_path,
                cw,
                cw, // signal we're at the limit
                &session_manager,
                &app_state,
                None,
            )
            .await
            {
                Ok(compact) => {
                    let new_prompt = render_prompt(
                        agent_type,
                        &task,
                        &TaskContext {
                            project_path: project_path.clone(),
                            workspace_path: worktree_path.display().to_string(),
                            diff: None, commits: None, start_commit: None,
                            end_commit: None, batch_num: None, task_count: None,
                            tasks_summary: None, common_labels: None,
                            conflict_files: None, merge_base_branch: None,
                            merge_target_branch: None, merge_failure_context: None,
                            setup_commands: None, verification_commands: None,
                        },
                    );
                    compact.agent.override_system_prompt(new_prompt).await;
                    current_session_id = compact.new_session_id;
                    current_record_id = Some(compact.new_record_id);
                    current_agent = compact.agent;
                    kickoff = GooseMessage::user().with_text(&compact.kickoff_summary);
                    continue;
                }
                Err(e) => {
                    let err_str = format!("context exhaustion compaction failed: {e}");
                    break (Err(anyhow::anyhow!("{}", err_str)), output);
                }
            }
        }

        // ── Verification pipeline for worker DONE ───────────────────────────
        let is_worker_done = reply_result.is_ok()
            && matches!(agent_type, AgentType::Worker | AgentType::ConflictResolver)
            && matches!(output.worker_signal, Some(WorkerSignal::Done));

        if is_worker_done {
            if let Some(feedback) =
                run_setup_commands_checked(&task_id, &worktree_path, &app_state).await
            {
                tracing::info!(task_id = %task_id, "Lifecycle: setup verification failed; resuming with feedback");
                // Log the feedback as a comment.
                let repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
                let payload = serde_json::json!({ "body": feedback }).to_string();
                let _ = repo
                    .log_activity(Some(&task_id), "agent-supervisor", "verification", "comment", &payload)
                    .await;
                kickoff = GooseMessage::user().with_text(&feedback);
                continue;
            }
            if let Some(feedback) =
                run_verification_commands(&task_id, &worktree_path, &app_state).await
            {
                tracing::info!(task_id = %task_id, "Lifecycle: verification failed; resuming with feedback");
                let repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
                let payload = serde_json::json!({ "body": feedback }).to_string();
                let _ = repo
                    .log_activity(Some(&task_id), "agent-supervisor", "verification", "comment", &payload)
                    .await;
                kickoff = GooseMessage::user().with_text(&feedback);
                continue;
            }
        }

        // ── Done ────────────────────────────────────────────────────────────
        break (reply_result, output);
    };

    // ── Post-loop: session record + health + transitions + cleanup ────────────
    let (tokens_in, tokens_out) = tokens_for_session(&current_session_id, &session_manager).await;

    // Health tracking.
    match &final_result {
        Ok(()) => app_state.health_tracker().record_success(&model_id),
        Err(_) => app_state.health_tracker().record_failure(&model_id),
    }
    app_state.persist_model_health_state().await;

    let is_worker_done = final_result.is_ok()
        && matches!(agent_type, AgentType::Worker | AgentType::ConflictResolver)
        && matches!(final_output.worker_signal, Some(WorkerSignal::Done));

    // Update session record.
    if is_worker_done {
        update_session_record_paused(current_record_id.as_deref(), tokens_in, tokens_out, &app_state).await;
    } else {
        let status = if final_result.is_ok() { SessionStatus::Completed } else { SessionStatus::Failed };
        update_session_record(current_record_id.as_deref(), status, tokens_in, tokens_out, &app_state).await;
    }

    // Worktree: commit and keep for worker done; cleanup otherwise.
    if let Some(worktree_ref) = Some(&worktree_path) {
        if is_worker_done {
            // Commit final work but keep worktree alive for review → resume cycle.
            if let Err(e) = commit_final_work_if_needed(&task_id, worktree_ref, &app_state).await {
                tracing::warn!(
                    task_id = %task_id,
                    error = %e,
                    "Lifecycle: failed to commit final work before pausing for review"
                );
            }
        } else {
            cleanup_worktree(&task_id, worktree_ref, &app_state).await;
        }

        // Post-DONE setup re-check is already handled in the main loop above.
        // (run_setup_commands_checked / run_verification_commands are called in the loop)
    }

    // Log reviewer feedback.
    let task_repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
    if let Some(feedback) = final_output.reviewer_feedback.as_deref() {
        let payload = serde_json::json!({ "body": feedback }).to_string();
        if let Err(e) = task_repo
            .log_activity(Some(&task_id), "agent-supervisor", "task_reviewer", "comment", &payload)
            .await
        {
            tracing::warn!(task_id = %task_id, error = %e, "failed to store reviewer feedback comment");
        }
    }

    // Log session errors.
    if let Err(reason) = &final_result {
        let payload = serde_json::json!({
            "error": reason.to_string(),
            "agent_type": agent_type.as_str(),
        })
        .to_string();
        let _ = task_repo
            .log_activity(Some(&task_id), "agent-supervisor", "system", "session_error", &payload)
            .await;
    }
    if final_result.is_ok()
        && let Some(reason) = final_output.runtime_error.as_deref()
    {
        let payload = serde_json::json!({
            "error": reason,
            "agent_type": agent_type.as_str(),
        })
        .to_string();
        let _ = task_repo
            .log_activity(Some(&task_id), "agent-supervisor", "system", "session_error", &payload)
            .await;
    }

    // Determine transition.
    let epic_error = final_result.as_ref().err().map(|e| e.to_string());
    let transition = match final_result {
        Ok(()) => {
            success_transition(&task_id, agent_type, &final_output, &app_state).await
        }
        Err(reason) => match agent_type {
            AgentType::Worker | AgentType::ConflictResolver => {
                Some((TransitionAction::Release, Some(reason.to_string())))
            }
            AgentType::TaskReviewer => {
                Some((TransitionAction::ReleaseTaskReview, Some(reason.to_string())))
            }
            AgentType::EpicReviewer => None,
        },
    };

    if agent_type == AgentType::EpicReviewer {
        finalize_epic_batch(&task_id, &final_output, epic_error.as_deref(), &app_state).await;
    }

    if let Some((action, reason)) = transition {
        tracing::info!(
            task_id = %task_id,
            agent_type = %agent_type.as_str(),
            transition_action = ?action,
            transition_reason = reason.as_deref().unwrap_or("<none>"),
            tokens_in,
            tokens_out,
            "Lifecycle: applying session transition"
        );
        let is_reviewer_rejection = matches!(
            action,
            TransitionAction::TaskReviewReject | TransitionAction::TaskReviewRejectConflict
        );
        if let Err(e) = task_repo
            .transition(
                &task_id,
                action,
                "agent-supervisor",
                "system",
                reason.as_deref(),
                None,
            )
            .await
        {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to transition task after session");
        }
        if is_reviewer_rejection {
            interrupt_paused_worker_session(&task_id, &app_state).await;
        }
    } else {
        tracing::info!(
            task_id = %task_id,
            agent_type = %agent_type.as_str(),
            tokens_in,
            tokens_out,
            "Lifecycle: session completed with no task transition"
        );
    }

    // Trigger dispatcher for the project so the next ready task starts promptly.
    if let Ok(task) = load_task(&task_id, &app_state).await
        && let Some(coordinator) = app_state.coordinator().await
    {
        let _ = coordinator
            .trigger_dispatch_for_project(&task.project_id)
            .await;
    }

    return_free!();
}
