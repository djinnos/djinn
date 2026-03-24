use std::path::{Path, PathBuf};

use crate::actors::coordinator::pr_poller::PR_REVIEW_FEEDBACK_EVENT;
use crate::context::AgentContext;
use djinn_core::models::Task;
use djinn_core::models::{SessionRecord, SessionStatus, TransitionAction};
use djinn_db::ActivityQuery;
use djinn_db::ProjectRepository;
use djinn_db::SessionRepository;
use djinn_db::TaskRepository;
use djinn_provider::repos::CredentialRepository;

use super::*;

// ─── Constants ────────────────────────────────────────────────────────────────

/// Max characters for verification output included in user messages.
/// Keeps the user-message payload reasonable (clippy stderr can be huge).
const MAX_VERIFICATION_CHARS: usize = 3000;

/// Max characters for a single inline PR review comment included in the prompt.
const MAX_PR_COMMENT_CHARS: usize = 500;

/// Return the most recent N high-signal comments (PM, reviewer, verification)
/// from the activity log, in chronological order (oldest first).
/// Each entry is formatted as "**Label:** body".
pub(crate) fn recent_feedback(
    activity: &[djinn_core::models::ActivityEntry],
    max: usize,
) -> Vec<String> {
    let high_signal: Vec<&djinn_core::models::ActivityEntry> = activity
        .iter()
        .rev()
        .filter(|e| {
            e.event_type == "comment"
                && (e.actor_role == "lead"
                    || e.actor_role == "pm"
                    || e.actor_role == "architect"
                    || e.actor_role == "reviewer"
                    || e.actor_role == "task_reviewer"
                    || e.actor_role == "verification")
        })
        .take(max)
        .collect();

    // Reverse back to chronological order
    high_signal
        .into_iter()
        .rev()
        .filter_map(|e| {
            let payload = serde_json::from_str::<serde_json::Value>(&e.payload).ok()?;
            let body = payload.get("body").and_then(|v| v.as_str())?;
            let label = match e.actor_role.as_str() {
                "lead" | "pm" => "Lead guidance",
                "architect" => "Architect directive",
                "reviewer" | "task_reviewer" => "Reviewer feedback",
                "verification" => "Verification failure",
                _ => "Feedback",
            };
            let trimmed = if e.actor_role == "verification" {
                truncate_feedback(body, MAX_VERIFICATION_CHARS)
            } else {
                body.to_string()
            };
            Some(format!("**{label}:**\n{trimmed}"))
        })
        .collect()
}

/// Extract worker submission summary/concerns and the last verification failure
/// from the activity log so the reviewer sees why the worker made certain changes.
///
/// Returns `(worker_summary, worker_concerns, verification_failure)`.
pub(crate) fn extract_worker_context(
    activity: &Option<Vec<djinn_core::models::ActivityEntry>>,
) -> (Option<String>, Option<String>, Option<String>) {
    let Some(entries) = activity else {
        return (None, None, None);
    };

    // Last work_submitted entry — contains summary and remaining_concerns.
    let (worker_summary, worker_concerns) = entries
        .iter()
        .rev()
        .find(|e| e.event_type == "work_submitted")
        .and_then(|e| serde_json::from_str::<serde_json::Value>(&e.payload).ok())
        .map(|payload| {
            let summary = payload
                .get("summary")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_owned());
            let concerns = payload.get("remaining_concerns").and_then(|v| {
                if let Some(arr) = v.as_array() {
                    let items: Vec<&str> = arr.iter().filter_map(|i| i.as_str()).collect();
                    if items.is_empty() {
                        None
                    } else {
                        Some(
                            items
                                .iter()
                                .map(|c| format!("- {c}"))
                                .collect::<Vec<_>>()
                                .join("\n"),
                        )
                    }
                } else {
                    v.as_str().filter(|s| !s.is_empty()).map(|s| s.to_owned())
                }
            });
            (summary, concerns)
        })
        .unwrap_or((None, None));

    // Last verification failure comment.
    let verification_failure = entries
        .iter()
        .rev()
        .find(|e| e.event_type == "comment" && e.actor_role == "verification")
        .and_then(|e| serde_json::from_str::<serde_json::Value>(&e.payload).ok())
        .and_then(|payload| {
            payload
                .get("body")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| truncate_feedback(s, MAX_VERIFICATION_CHARS))
        });

    (worker_summary, worker_concerns, verification_failure)
}

/// Build a formatted PR review feedback section for the worker prompt.
///
/// Queries the task activity log for the most recent `pr_review_feedback` entry
/// (stored by the PR poller when CHANGES_REQUESTED is detected) and formats it
/// as a structured section with inline code comments so the worker knows exactly
/// what to fix.
///
/// Returns `None` when no PR review feedback exists for the task.
pub(crate) async fn pr_review_feedback_context(
    task_id: &str,
    app_state: &AgentContext,
) -> Option<String> {
    let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let entries = repo
        .query_activity(ActivityQuery {
            task_id: Some(task_id.to_owned()),
            event_type: Some(PR_REVIEW_FEEDBACK_EVENT.to_string()),
            actor_role: Some("system".to_string()),
            project_id: None,
            from_time: None,
            to_time: None,
            limit: 1,
            offset: 0,
        })
        .await
        .ok()?;

    let entry = entries.into_iter().next()?;
    let payload: serde_json::Value = serde_json::from_str(&entry.payload).ok()?;

    let round = payload.get("round").and_then(|v| v.as_u64()).unwrap_or(1);
    let pr_url = payload.get("pr_url").and_then(|v| v.as_str()).unwrap_or("");
    let pull_number = payload
        .get("pull_number")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let mut lines = Vec::new();
    lines.push(format!(
        "**PR Review Feedback (Round {round})** — [{pr_url}]({pr_url})"
    ));

    // Top-level change-request reviews.
    if let Some(reviews) = payload
        .get("change_request_reviews")
        .and_then(|v| v.as_array())
        && !reviews.is_empty()
    {
        lines.push(String::new());
        lines.push("**Review summaries (CHANGES_REQUESTED):**".to_string());
        for review in reviews {
            let reviewer = review
                .get("reviewer")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let html_url = review
                .get("html_url")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            lines.push(format!("- @{reviewer} — {html_url}"));
        }
    }

    // Inline code comments.
    if let Some(comments) = payload.get("inline_comments").and_then(|v| v.as_array())
        && !comments.is_empty()
    {
        lines.push(String::new());
        lines.push(format!(
            "**Inline review comments on PR #{}:**",
            pull_number
        ));
        for comment in comments {
            let reviewer = comment
                .get("reviewer")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let body = comment.get("body").and_then(|v| v.as_str()).unwrap_or("");
            let path = comment.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let line = comment.get("line").and_then(|v| v.as_u64());
            let location = if !path.is_empty() {
                if let Some(l) = line {
                    format!("`{path}:{l}`")
                } else {
                    format!("`{path}`")
                }
            } else {
                "(general comment)".to_string()
            };
            let truncated = truncate_feedback(body, MAX_PR_COMMENT_CHARS);
            lines.push(format!("- {location} (@{reviewer}): {truncated}"));
        }
    }

    if lines.len() <= 1 {
        return None;
    }

    Some(lines.join("\n"))
}

// ─── Utility functions ────────────────────────────────────────────────────────

/// Truncate feedback text using 60/40 head+tail split.
fn truncate_feedback(text: &str, max: usize) -> String {
    crate::truncate::smart_truncate(text, max)
}

#[cfg(test)]
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

/// Format command specs as `- **name**: \`command\`` for display in prompts.
pub(crate) fn format_command_details(
    specs: &[djinn_core::commands::CommandSpec],
) -> Option<String> {
    if specs.is_empty() {
        return None;
    }
    Some(
        specs
            .iter()
            .map(|s| format!("- **{}**: `{}`", s.name, s.command))
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
    app_state: &AgentContext,
) {
    let Some(record_id) = record_id else {
        return;
    };
    let repo = SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    if let Err(e) = repo.update(record_id, status, tokens_in, tokens_out).await {
        tracing::warn!(record_id = %record_id, error = %e, "failed to update session record");
    }
}

pub(crate) async fn update_session_record_paused(
    record_id: Option<&str>,
    tokens_in: i64,
    tokens_out: i64,
    app_state: &AgentContext,
) {
    let Some(record_id) = record_id else {
        return;
    };
    let repo = SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    if let Err(e) = repo.pause(record_id, tokens_in, tokens_out).await {
        tracing::warn!(record_id = %record_id, error = %e, "failed to pause session record");
    }
}

// ─── Task / project helpers ───────────────────────────────────────────────────

pub(crate) async fn load_task(task_id: &str, app_state: &AgentContext) -> anyhow::Result<Task> {
    let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let task = repo
        .get(task_id)
        .await
        .map_err(|e| anyhow::anyhow!("db error loading task: {e}"))?;
    task.ok_or_else(|| anyhow::anyhow!("task not found: {task_id}"))
}

pub(crate) async fn default_target_branch(project_id: &str, app_state: &AgentContext) -> String {
    let repo = ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    if let Ok(Some(config)) = repo.get_config(project_id).await {
        return config.target_branch;
    }
    "main".to_string()
}

pub(crate) async fn find_paused_session_record(
    task_id: &str,
    role_name: &str,
    app_state: &AgentContext,
) -> Option<SessionRecord> {
    let repo = SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    repo.paused_for_task_by_type(task_id, role_name)
        .await
        .ok()
        .flatten()
}

/// Extract the `reason` field from the most recent `status_changed` activity
/// entry that represents a review-to-open rejection (from_status =
/// "in_task_review", to_status = "open"). Searches backwards through ALL
/// status_changed events, not just the very last one, so that intervening
/// transitions (e.g. verification failures cycling through verifying→open)
/// don't obscure the original rejection reason.
async fn last_review_rejection_reason(task_id: &str, app_state: &AgentContext) -> Option<String> {
    let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let activity = repo.list_activity(task_id).await.ok()?;
    let rejection = activity.iter().rev().find(|e| {
        if e.event_type != "status_changed" {
            return false;
        }
        let Ok(p) = serde_json::from_str::<serde_json::Value>(&e.payload) else {
            return false;
        };
        p.get("from_status").and_then(|v| v.as_str()) == Some("in_task_review")
            && p.get("to_status").and_then(|v| v.as_str()) == Some("open")
    })?;
    let payload: serde_json::Value = serde_json::from_str(&rejection.payload).ok()?;
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
    app_state: &AgentContext,
) -> Option<MergeConflictMetadata> {
    // Fast path: check the task's persistent merge_conflict_metadata field.
    let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    if let Ok(Some(task)) = repo.get(task_id).await
        && let Some(ref meta_json) = task.merge_conflict_metadata
        && let Ok(meta) = serde_json::from_str(meta_json)
    {
        return Some(meta);
    }
    // Fallback: scan activity log for backward compat with tasks that
    // existed before the merge_conflict_metadata column was added.
    if let Some(reason) = last_review_rejection_reason(task_id, app_state).await
        && let Some(meta) = parse_conflict_metadata(&reason)
    {
        return Some(meta);
    }
    let activity = repo.list_activity(task_id).await.ok()?;
    activity
        .iter()
        .rev()
        .find(|e| e.event_type == "merge_conflict")
        .and_then(|e| serde_json::from_str(&e.payload).ok())
}

pub(crate) async fn merge_validation_context_for_dispatch(
    task_id: &str,
    app_state: &AgentContext,
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

pub(crate) async fn resume_context_for_task(task_id: &str, app_state: &AgentContext) -> String {
    let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let activity = repo.list_activity(task_id).await.ok().unwrap_or_default();

    // Preamble reminding the model that any prior "I'm done" statements are
    // stale and it MUST use tools to make real changes.
    const RESUME_PREAMBLE: &str = "\
**IMPORTANT: Your session is being resumed after a review rejection.** \
Disregard any prior statements where you claimed the work was complete — the \
reviewer determined it was NOT. You MUST use your tools (shell, editor, etc.) \
to make concrete changes before stopping. A text-only response with no tool \
calls will be treated as a failure.\n\n";

    // PR review feedback takes priority — it's the most actionable signal.
    // When a human reviewer has left specific inline comments on the PR, include
    // them prominently so the worker addresses each one.
    if let Some(pr_feedback) = pr_review_feedback_context(task_id, app_state).await {
        return format!(
            "{RESUME_PREAMBLE}{pr_feedback}\n\nAddress every reviewer comment listed above. \
            Push fixup commits to the same branch. Do not open a new PR."
        );
    }

    // Last 3 high-signal comments (PM, reviewer, verification) in
    // chronological order. Simple and gives the worker full recent context.
    let sections = recent_feedback(&activity, 3);

    if !sections.is_empty() {
        return format!(
            "{RESUME_PREAMBLE}{}\n\nAddress this feedback, make the necessary changes, then stop.",
            sections.join("\n\n---\n\n")
        );
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
                "A merge conflict was detected when merging your branch into `{}`. Resolve the conflicts in these files:\n\n{files}\n\nAfter resolving, commit and stop.",
                meta.merge_target
            );
        }
    }

    // Also check the transition reason as a fallback — the status_changed
    // event from TaskReviewReject/TaskReviewRejectStale stores a reason.
    for entry in activity.iter().rev() {
        if entry.event_type == "status_changed"
            && let Ok(payload) = serde_json::from_str::<serde_json::Value>(&entry.payload)
            && payload.get("to_status").and_then(|v| v.as_str()) == Some("open")
            && let Some(reason) = payload.get("reason").and_then(|v| v.as_str())
            && !reason.is_empty()
        {
            return format!(
                "{RESUME_PREAMBLE}Your work was returned with this reason:\n\n{reason}\n\nAddress the issues, make the necessary changes, then stop."
            );
        }
    }

    format!(
        "{RESUME_PREAMBLE}Your previous submission was rejected. Re-read the task acceptance criteria with `task_show`, identify what is unmet, make changes, then stop."
    )
}

/// Build an initial user message for a fresh worker session. If the activity
/// log contains PM or reviewer feedback, include it prominently so the worker
/// acts on it immediately rather than discovering it buried in the system prompt.
///
/// PR review feedback (from GitHub reviewer inline comments) is surfaced first
/// when present — this is the most specific, actionable signal available.
pub(crate) async fn initial_user_message_for_task(
    task_id: &str,
    app_state: &AgentContext,
) -> String {
    // PR review feedback takes priority over generic activity log comments.
    if let Some(pr_feedback) = pr_review_feedback_context(task_id, app_state).await {
        return format!(
            "A human reviewer has requested changes on the PR. Address every reviewer comment below:\n\n\
            {pr_feedback}\n\n\
            Push fixup commits to the same branch. Do not open a new PR."
        );
    }

    let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let activity = repo.list_activity(task_id).await.ok().unwrap_or_default();

    let sections = recent_feedback(&activity, 3);

    if sections.is_empty() {
        "Start by understanding the task context and execute it fully before stopping.".to_string()
    } else {
        format!(
            "The activity log contains important feedback from prior sessions. Read it carefully and act on it:\n\n{}\n\nAddress this feedback, make the necessary changes, then stop.",
            sections.join("\n\n---\n\n")
        )
    }
}

// ─── Retry helper for SQLite lock contention ─────────────────────────────────

fn is_database_locked(e: &anyhow::Error) -> bool {
    if let Some(djinn_db::Error::Sqlx(sqlx_err)) = e.downcast_ref::<djinn_db::Error>()
        && let Some(db_err) = sqlx_err.as_database_error()
    {
        return db_err
            .code()
            .map(|c| matches!(c.as_ref(), "5" | "6"))
            .unwrap_or(false);
    }
    false
}

/// Retry an async database operation up to 5 times with exponential backoff
/// when SQLite returns "database is locked" errors.
async fn retry_on_locked<F, Fut, T>(mut f: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    const MAX_RETRIES: u32 = 5;
    const BASE_DELAY_MS: u64 = 200;

    let mut attempt = 0;
    loop {
        match f().await {
            Ok(val) => return Ok(val),
            Err(e) if is_database_locked(&e) && attempt < MAX_RETRIES => {
                attempt += 1;
                let delay = BASE_DELAY_MS * 2u64.pow(attempt - 1);
                tracing::debug!(
                    attempt,
                    delay_ms = delay,
                    "database locked, retrying after backoff"
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

// ─── Transition helpers ───────────────────────────────────────────────────────

pub(crate) async fn transition_start(
    task: &Task,
    start_action: fn(&str) -> Option<TransitionAction>,
    app_state: &AgentContext,
) -> anyhow::Result<()> {
    if let Some(action) = start_action(task.status.as_str()) {
        let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
        retry_on_locked(|| async {
            repo.transition(
                &task.id,
                action.clone(),
                "agent-supervisor",
                "system",
                None,
                None,
            )
            .await
            .map_err(anyhow::Error::from)
        })
        .await
        .map_err(|e| anyhow::anyhow!("task transition failed for {}: {e}", task.id))?;
    }
    Ok(())
}

pub(crate) async fn transition_interrupted(
    task_id: &str,
    release_action: fn() -> TransitionAction,
    reason: &str,
    app_state: &AgentContext,
) {
    let action = release_action();

    let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let reason = reason.to_owned();
    if let Err(e) = retry_on_locked(|| async {
        repo.transition(
            task_id,
            action.clone(),
            "agent-supervisor",
            "system",
            Some(&reason),
            None,
        )
        .await
        .map_err(anyhow::Error::from)
    })
    .await
    {
        tracing::warn!(task_id = %task_id, error = %e, "failed to transition interrupted task");
    }
}

// ─── Provider helpers ─────────────────────────────────────────────────────────

pub fn format_family_for_provider(
    provider_id: &str,
    model_id: &str,
) -> crate::provider::FormatFamily {
    use crate::provider::FormatFamily;
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

pub fn capabilities_for_provider(provider_id: &str) -> crate::provider::ProviderCapabilities {
    use crate::provider::ProviderCapabilities;
    let lower = provider_id.to_lowercase();
    if lower.contains("synthetic") || lower.contains("local") {
        ProviderCapabilities {
            streaming: false,
            max_tokens_default: None,
        }
    } else if lower.contains("anthropic") {
        ProviderCapabilities {
            streaming: true,
            max_tokens_default: Some(8192),
        }
    } else {
        ProviderCapabilities::default()
    }
}

pub fn auth_method_for_provider(provider_id: &str, api_key: &str) -> crate::provider::AuthMethod {
    use crate::provider::AuthMethod;
    if provider_id.to_lowercase().contains("anthropic") {
        AuthMethod::ApiKeyHeader {
            header: "x-api-key".to_string(),
            key: api_key.to_string(),
        }
    } else {
        AuthMethod::BearerToken(api_key.to_string())
    }
}

pub fn default_base_url(provider_id: &str) -> String {
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
pub enum ProviderCredential {
    /// Traditional API-key credential (key_name, decrypted key).
    ApiKey(String, String),
    /// OAuth-derived full provider config (base_url, auth, model already set).
    OAuthConfig(Box<crate::provider::ProviderConfig>),
}

pub async fn load_provider_credential(
    provider_id: &str,
    app_state: &AgentContext,
) -> anyhow::Result<ProviderCredential> {
    // 1. Try OAuth tokens first for OAuth-capable providers.
    // Also resolve merged children: e.g. "openai" → "chatgpt_codex".
    let effective_oauth_id = match provider_id {
        "chatgpt_codex" | "githubcopilot" => provider_id,
        other => djinn_provider::catalog::builtin::resolve_oauth_provider(other).unwrap_or(other),
    };
    let credential_repo =
        CredentialRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    match effective_oauth_id {
        "chatgpt_codex" => {
            if let Some(tokens) =
                crate::oauth::codex::CodexTokens::load_from_db(&credential_repo).await
            {
                if tokens.is_expired() {
                    // Attempt silent refresh.
                    match crate::oauth::codex::refresh_cached_token(&tokens, &credential_repo).await
                    {
                        Ok(refreshed) => {
                            return Ok(ProviderCredential::OAuthConfig(Box::new(
                                crate::oauth::codex_provider_config(&refreshed),
                            )));
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
                    return Ok(ProviderCredential::OAuthConfig(Box::new(
                        crate::oauth::codex_provider_config(&tokens),
                    )));
                }
            }
        }
        "githubcopilot" => {
            if let Some(tokens) =
                crate::oauth::copilot::CopilotTokens::load_from_db(&credential_repo).await
            {
                if !tokens.is_expired() {
                    return Ok(ProviderCredential::OAuthConfig(Box::new(
                        crate::oauth::copilot_provider_config(&tokens),
                    )));
                }
                // Copilot refresh requires the github_token → try exchange.
                match crate::oauth::copilot::refresh_copilot_token(&tokens, &credential_repo).await
                {
                    Ok(refreshed) => {
                        return Ok(ProviderCredential::OAuthConfig(Box::new(
                            crate::oauth::copilot_provider_config(&refreshed),
                        )));
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
        .catalog
        .list_providers()
        .into_iter()
        .find(|p| p.id == provider_id)
        .and_then(|p| p.env_vars.into_iter().next())
        .unwrap_or_else(|| format!("{}_API_KEY", provider_id.to_ascii_uppercase()));

    let key = credential_repo
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

pub fn parse_model_id(model_id: &str) -> anyhow::Result<(String, String)> {
    let Some((provider_id, model_name)) = model_id.split_once('/') else {
        return Err(anyhow::anyhow!(
            "invalid model id '{model_id}', expected provider/model"
        ));
    };
    Ok((provider_id.to_owned(), model_name.to_owned()))
}

/// Build telemetry metadata for OTel span instrumentation.
pub(crate) fn build_telemetry_meta(
    agent_type_str: &str,
    task_id: &str,
) -> crate::provider::TelemetryMeta {
    crate::provider::TelemetryMeta {
        task_id: Some(task_id.to_owned()),
        agent_type: Some(agent_type_str.to_owned()),
        session_id: Some(task_id.to_owned()),
    }
}
