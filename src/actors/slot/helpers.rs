use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use crate::agent::{AgentType, SessionManager};
use crate::db::repositories::credential::CredentialRepository;
use crate::db::repositories::project::ProjectRepository;
use crate::db::repositories::session::SessionRepository;
use crate::db::repositories::task::TaskRepository;
use crate::models::session::{SessionRecord, SessionStatus};
use crate::models::task::{Task, TransitionAction};
use crate::server::AppState;
use goose::providers;

use super::*;

// ─── Utility functions ────────────────────────────────────────────────────────

#[allow(dead_code)]
pub(crate) fn log_snippet(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    let mut out = String::new();
    for ch in trimmed.chars().take(max_chars) {
        out.push(ch);
    }
    if trimmed.chars().count() > max_chars {
        out.push('…');
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
        let Some(text) = last_assistant_text_from_goose_sqlite_at(&db_path, goose_session_id).await
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

pub(crate) async fn load_task(task_id: &str, app_state: &AppState) -> anyhow::Result<Task> {
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

#[allow(dead_code)]
pub(crate) async fn find_paused_session_record(
    task_id: &str,
    app_state: &AppState,
) -> Option<SessionRecord> {
    let repo = SessionRepository::new(app_state.db().clone(), app_state.events().clone());
    repo.paused_for_task(task_id).await.ok().flatten()
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

#[allow(dead_code)]
pub(crate) async fn resume_context_for_task(task_id: &str, app_state: &AppState) -> String {
    let repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
    let activity = repo.list_activity(task_id).await.ok().unwrap_or_default();

    for entry in activity.iter().rev() {
        if entry.event_type == "comment"
            && entry.actor_role == "task_reviewer"
            && let Ok(payload) = serde_json::from_str::<serde_json::Value>(&entry.payload)
            && let Some(body) = payload.get("body").and_then(|v| v.as_str())
        {
            return format!(
                "Your previous work was reviewed and returned with this feedback:\n\n{body}\n\nAddress this feedback, make the necessary changes, then emit:\nWORKER_RESULT: DONE"
            );
        }
    }

    if let Some(context) = merge_validation_context_for_dispatch(task_id, app_state).await {
        return context;
    }

    for entry in activity.iter().rev() {
        if entry.event_type == "merge_conflict"
            && let Ok(meta) = serde_json::from_str::<MergeConflictMetadata>(&entry.payload)
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
        (AgentType::TaskReviewer, "needs_task_review") => Some(TransitionAction::TaskReviewStart),
        (AgentType::PM, "needs_pm_intervention") => Some(TransitionAction::PmInterventionStart),
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
        AgentType::Worker | AgentType::ConflictResolver => TransitionAction::Release,
        AgentType::TaskReviewer => TransitionAction::ReleaseTaskReview,
        AgentType::PM => TransitionAction::PmInterventionRelease,
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

pub(crate) async fn provider_supports_oauth(_provider_id: &str, goose_provider_id: &str) -> bool {
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
        return Err(anyhow::anyhow!(
            "invalid model id '{model_id}', expected provider/model"
        ));
    };
    Ok((provider_id.to_owned(), model_name.to_owned()))
}

pub(crate) fn extensions_for(agent_type: AgentType) -> Vec<goose::config::ExtensionConfig> {
    use goose::config::ExtensionConfig;

    let mut exts = vec![
        // Djinn frontend extension (task_show, task_update, memory_read, etc.)
        crate::agent::extension::config(agent_type),
        // Tree-sitter code analysis (read-only, all agent types)
        ExtensionConfig::Platform {
            name: "analyze".to_string(),
            description: "Analyze code structure with tree-sitter".to_string(),
            display_name: None,
            bundled: None,
            available_tools: vec![],
        },
        // Persistent todo list (survives compaction via extension_data, all agent types)
        ExtensionConfig::Platform {
            name: "todo".to_string(),
            description: "Persistent task checklist".to_string(),
            display_name: None,
            bundled: None,
            available_tools: vec![],
        },
    ];

    match agent_type {
        // Workers and conflict resolvers: full developer tools + subagent delegation
        AgentType::Worker | AgentType::ConflictResolver => {
            exts.push(ExtensionConfig::Platform {
                name: "developer".to_string(),
                description: "Write and edit files, list directory trees".to_string(),
                display_name: None,
                bundled: None,
                available_tools: vec!["write".to_string(), "edit".to_string(), "tree".to_string()],
            });
            exts.push(ExtensionConfig::Platform {
                name: "summon".to_string(),
                description: "Load knowledge and delegate tasks to subagents".to_string(),
                display_name: None,
                bundled: None,
                available_tools: vec![],
            });
        }
        // Reviewers: read-only developer tools (tree only), no subagents
        AgentType::TaskReviewer => {
            exts.push(ExtensionConfig::Platform {
                name: "developer".to_string(),
                description: "List directory trees".to_string(),
                display_name: None,
                bundled: None,
                available_tools: vec!["tree".to_string()],
            });
        }
        // PM: no worktree tools — task management only
        AgentType::PM => {}
    }

    exts
}

pub(crate) fn agent_type_for_task(task: &Task, has_conflict_context: bool) -> AgentType {
    match task.status.as_str() {
        "needs_task_review" | "in_task_review" => AgentType::TaskReviewer,
        "needs_pm_intervention" | "in_pm_intervention" => AgentType::PM,
        "open" if has_conflict_context => AgentType::ConflictResolver,
        _ => AgentType::Worker,
    }
}

