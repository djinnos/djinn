use std::path::{Path, PathBuf};

use crate::agent::AgentType;
use crate::db::repositories::credential::CredentialRepository;
use crate::db::repositories::project::ProjectRepository;
use crate::db::repositories::session::SessionRepository;
use crate::db::repositories::task::TaskRepository;
use crate::models::session::{SessionRecord, SessionStatus};
use crate::models::task::Task;
use crate::server::AppState;

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
    let repo = ProjectRepository::new(app_state.db().clone(), app_state.events().clone());
    repo.get_path(project_id).await.ok().flatten()
}

pub(crate) async fn find_paused_session_record(
    task_id: &str,
    app_state: &AppState,
) -> Option<SessionRecord> {
    let repo = SessionRepository::new(app_state.db().clone(), app_state.events().clone());
    repo.paused_for_task(task_id).await.ok().flatten()
}

/// Extract the `reason` field from the last `status_changed` activity entry
/// that represents a review-to-open rejection (from_status = "in_task_review",
/// to_status = "open"). Returns `None` if no such transition exists.
async fn last_review_rejection_reason(
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
    Some(
        payload
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    )
}

pub(crate) async fn conflict_context_for_dispatch(
    task_id: &str,
    app_state: &AppState,
) -> Option<MergeConflictMetadata> {
    let reason = last_review_rejection_reason(task_id, app_state).await?;
    parse_conflict_metadata(&reason)
}

pub(crate) async fn merge_validation_context_for_dispatch(
    task_id: &str,
    app_state: &AppState,
) -> Option<String> {
    let reason = last_review_rejection_reason(task_id, app_state).await?;
    let metadata = parse_merge_validation_metadata(&reason)?;
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
    if let Some(action) = agent_type.start_action(task.status.as_str()) {
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
    let action = agent_type.release_action();

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

pub(crate) fn format_family_for_provider(
    provider_id: &str,
    model_id: &str,
) -> crate::agent::provider::FormatFamily {
    use crate::agent::provider::FormatFamily;
    let lower = provider_id.to_lowercase();
    if lower.contains("anthropic") {
        FormatFamily::Anthropic
    } else if lower.contains("google") || lower.contains("gemini") || lower.contains("vertex") {
        FormatFamily::Google
    } else if lower.contains("codex") || model_id.contains("codex") {
        FormatFamily::OpenAIResponses
    } else {
        FormatFamily::OpenAI
    }
}

pub(crate) fn auth_method_for_provider(
    provider_id: &str,
    api_key: &str,
) -> crate::agent::provider::AuthMethod {
    use crate::agent::provider::AuthMethod;
    if provider_id.to_lowercase().contains("anthropic") {
        AuthMethod::ApiKeyHeader {
            header: "x-api-key".to_string(),
            key: api_key.to_string(),
        }
    } else {
        AuthMethod::BearerToken(api_key.to_string())
    }
}

pub(crate) fn default_base_url(provider_id: &str) -> String {
    let lower = provider_id.to_lowercase();
    if lower.contains("anthropic") {
        "https://api.anthropic.com".to_string()
    } else if lower.contains("google") || lower.contains("gemini") {
        "https://generativelanguage.googleapis.com".to_string()
    } else {
        "https://api.openai.com".to_string()
    }
}

/// Resolved provider credentials — either an API key from the vault or an
/// OAuth-derived `ProviderConfig` that already carries the right base URL,
/// auth method, and model defaults.
pub(crate) enum ProviderCredential {
    /// Traditional API-key credential (key_name, decrypted key).
    ApiKey(String, String),
    /// OAuth-derived full provider config (base_url, auth, model already set).
    OAuthConfig(crate::agent::provider::ProviderConfig),
}

pub(crate) async fn load_provider_credential(
    provider_id: &str,
    app_state: &AppState,
) -> anyhow::Result<ProviderCredential> {
    // 1. Try OAuth tokens first for OAuth-capable providers.
    // Also resolve merged children: e.g. "openai" → "chatgpt_codex".
    let effective_oauth_id = match provider_id {
        "chatgpt_codex" | "githubcopilot" => provider_id,
        other => crate::provider::builtin::resolve_oauth_provider(other).unwrap_or(other),
    };
    match effective_oauth_id {
        "chatgpt_codex" => {
            if let Some(tokens) = crate::agent::oauth::codex::CodexTokens::load_cached() {
                if tokens.is_expired() {
                    // Attempt silent refresh.
                    match crate::agent::oauth::codex::refresh_cached_token(&tokens).await {
                        Ok(refreshed) => {
                            return Ok(ProviderCredential::OAuthConfig(
                                crate::agent::oauth::codex_provider_config(&refreshed),
                            ));
                        }
                        Err(e) => {
                            tracing::warn!(
                                provider = provider_id,
                                error = %e,
                                "OAuth token refresh failed; falling back to credential vault"
                            );
                        }
                    }
                } else {
                    return Ok(ProviderCredential::OAuthConfig(
                        crate::agent::oauth::codex_provider_config(&tokens),
                    ));
                }
            }
        }
        "githubcopilot" => {
            if let Some(tokens) = crate::agent::oauth::copilot::CopilotTokens::load_cached() {
                if !tokens.is_expired() {
                    return Ok(ProviderCredential::OAuthConfig(
                        crate::agent::oauth::copilot_provider_config(&tokens),
                    ));
                }
                // Copilot refresh requires the github_token → try exchange.
                match crate::agent::oauth::copilot::refresh_copilot_token(&tokens).await {
                    Ok(refreshed) => {
                        return Ok(ProviderCredential::OAuthConfig(
                            crate::agent::oauth::copilot_provider_config(&refreshed),
                        ));
                    }
                    Err(e) => {
                        tracing::warn!(
                            provider = provider_id,
                            error = %e,
                            "Copilot token refresh failed; falling back to credential vault"
                        );
                    }
                }
            }
        }
        _ => {}
    }

    // 2. Fall back to credential vault (DB).
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
        Some(v) => Ok(ProviderCredential::ApiKey(key_name, v)),
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

pub(crate) fn agent_type_for_task(task: &Task, has_conflict_context: bool) -> AgentType {
    AgentType::for_task_status(task.status.as_str(), has_conflict_context)
}

/// Build telemetry metadata for OTel span instrumentation.
pub(crate) fn build_telemetry_meta(
    agent_type: AgentType,
    task_id: &str,
) -> crate::agent::provider::TelemetryMeta {
    crate::agent::provider::TelemetryMeta {
        task_id: Some(task_id.to_owned()),
        agent_type: Some(agent_type.as_str().to_owned()),
        session_id: Some(task_id.to_owned()),
    }
}

