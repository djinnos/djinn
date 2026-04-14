use std::path::{Path, PathBuf};

use crate::actors::coordinator::pr_poller::PR_REVIEW_FEEDBACK_EVENT;
use crate::context::AgentContext;
use djinn_core::models::Task;
use djinn_core::models::parse_json_array;
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

/// Maximum number of new hygiene/exploration follow-up tasks the Planner should
/// create during a single patrol when no explicit override is configured.
const DEFAULT_PATROL_KNOWLEDGE_TASK_BUDGET: usize = 2;

/// Environment variable for overriding the patrol knowledge-task budget.
const PATROL_KNOWLEDGE_TASK_BUDGET_ENV: &str = "DJINN_PLANNER_PATROL_KNOWLEDGE_TASK_BUDGET";

fn planner_patrol_knowledge_task_budget() -> usize {
    std::env::var(PATROL_KNOWLEDGE_TASK_BUDGET_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_PATROL_KNOWLEDGE_TASK_BUDGET)
}

fn normalize_text_for_matching(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|part| !part.trim().is_empty())
        .map(|part| part.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_hygiene_knowledge_task(task: &Task) -> bool {
    if task.status == "closed" {
        return false;
    }

    let searchable = normalize_text_for_matching(&[&task.title, &task.description, &task.design]);
    let has_hygiene_keyword = [
        "orphan",
        "broken link",
        "duplicate cluster",
        "duplicate note",
        "consolidat",
        "stale note",
        "low-confidence",
        "low confidence",
        "memory hygiene",
        "extraction",
        "review_needed",
        "review needed",
    ]
    .iter()
    .any(|keyword| searchable.contains(keyword));

    has_hygiene_keyword && matches!(task.issue_type.as_str(), "planning" | "task" | "research")
}

fn is_exploration_knowledge_task(task: &Task) -> bool {
    if task.status == "closed" {
        return false;
    }

    let searchable = normalize_text_for_matching(&[&task.title, &task.description, &task.design]);
    let has_exploration_keyword = [
        "explore and document",
        "explore",
        "document",
        "subsystem overview",
        "overview",
        "undocumented",
        "knowledge gap",
        "architectural",
        "structural change",
        "new module",
    ]
    .iter()
    .any(|keyword| searchable.contains(keyword));

    has_exploration_keyword && matches!(task.issue_type.as_str(), "spike" | "research" | "planning")
}

fn format_open_knowledge_tasks(tasks: &[Task]) -> String {
    if tasks.is_empty() {
        return "none".to_string();
    }

    tasks
        .iter()
        .take(4)
        .map(|task| format!("`{}` ({})", task.short_id, task.title))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Return the most recent N high-signal comments (lead, reviewer, verification)
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

    // Last 3 high-signal comments (lead, reviewer, verification) in
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
/// log contains lead or reviewer feedback, include it prominently so the worker
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
    } else if lower.contains("codex")
        || model_id.contains("codex")
        || (is_openai_responses_model(model_id) && is_native_openai_provider(&lower))
    {
        FormatFamily::OpenAIResponses
    } else {
        FormatFamily::OpenAI
    }
}

/// Returns true for OpenAI models that support the Responses API (GPT-5.x,
/// o-series reasoning models).  These get better quality and reasoning
/// summaries when routed through `/responses` instead of `/chat/completions`.
fn is_openai_responses_model(model_id: &str) -> bool {
    let lower = model_id.to_lowercase();
    lower.starts_with("gpt-5")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
}

/// Returns true for provider IDs that point to OpenAI's own API (not
/// third-party OpenAI-compatible endpoints like Fireworks, Together, etc.).
fn is_native_openai_provider(provider_id_lower: &str) -> bool {
    provider_id_lower == "openai"
        || provider_id_lower.starts_with("openai")
        || provider_id_lower.contains("chatgpt")
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
            max_tokens_default: Some(64_000),
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

// ─── Knowledge context helpers ──────────────────────────────────────────────

/// Extract crate/module path prefixes from a task's description, design, and epic context.
pub(crate) fn derive_task_scope_paths(
    task: &djinn_core::models::Task,
    epic_context: Option<&str>,
) -> Vec<String> {
    use regex::Regex;
    // Match paths like: crates/foo, src/bar/baz, server/crates/djinn-db
    // Looking for patterns with at least 2 slash-separated segments
    let re =
        Regex::new(r#"(?:^|[\s`"(])([a-zA-Z0-9_-]+(?:/[a-zA-Z0-9_.-]+){1,6})(?:[\s`")\.,:]|$)"#)
            .unwrap_or_else(|_| Regex::new(r"[a-z]+/[a-z]+").unwrap());

    let mut paths = std::collections::HashSet::new();

    for text in [&task.description, &task.design] {
        for cap in re.captures_iter(text) {
            if let Some(m) = cap.get(1) {
                let path = m.as_str();
                // Filter to paths that look like code paths (not URLs, not short fragments)
                if path.contains('/') && !path.starts_with("http") && !path.starts_with("//") {
                    // Derive scope: split on /src/ or take up to 3 components
                    if let Some(idx) = path.find("/src/") {
                        paths.insert(path[..idx].to_string());
                    } else {
                        paths.insert(path.to_string());
                    }
                }
            }
        }
    }

    if let Some(epic) = epic_context {
        for cap in re.captures_iter(epic) {
            if let Some(m) = cap.get(1) {
                let path = m.as_str();
                if path.contains('/') && !path.starts_with("http") && !path.starts_with("//") {
                    if let Some(idx) = path.find("/src/") {
                        paths.insert(path[..idx].to_string());
                    } else {
                        paths.insert(path.to_string());
                    }
                }
            }
        }
    }

    paths.into_iter().collect()
}

/// Format knowledge notes for injection into the system prompt.
/// Uses L0 (abstract) for most notes, L1 (overview) for high-confidence ones.
/// Budget-capped at `budget_chars`, dropping lowest-confidence notes first.
pub(crate) fn format_knowledge_notes(
    notes: &[djinn_core::models::Note],
    budget_chars: usize,
) -> String {
    let mut lines = Vec::new();
    let mut used = 0;

    for note in notes {
        let label = match note.note_type.as_str() {
            "pitfall" => "Pitfall",
            "pattern" => "Pattern",
            "case" => "Case",
            _ => "Note",
        };

        let summary = if note.confidence > 0.8 {
            // High confidence: use overview (L1) if available
            note.overview
                .as_deref()
                .or(note.abstract_.as_deref())
                .unwrap_or_else(|| &note.content[..note.content.len().min(200)])
        } else {
            // Lower confidence: use abstract (L0) if available
            note.abstract_
                .as_deref()
                .unwrap_or_else(|| &note.content[..note.content.len().min(100)])
        };

        let line = format!("- **[{}] {}**: {}", label, note.title, summary);

        if used + line.len() > budget_chars {
            break;
        }
        used += line.len() + 1; // +1 for newline
        lines.push(line);
    }

    lines.join("\n")
}

fn graph_diff_module_paths(diff: &djinn_mcp::bridge::GraphDiff) -> (Vec<String>, Vec<String>) {
    let mut added = std::collections::BTreeSet::new();
    let mut removed = std::collections::BTreeSet::new();

    for node in &diff.added_nodes {
        if node.kind == "file"
            && let Some(path) = node.key.strip_prefix("file:")
        {
            added.insert(path.to_string());
        }
    }
    for node in &diff.removed_nodes {
        if node.kind == "file"
            && let Some(path) = node.key.strip_prefix("file:")
        {
            removed.insert(path.to_string());
        }
    }

    (added.into_iter().collect(), removed.into_iter().collect())
}

fn note_scope_covers_path(note_scope_paths: &[String], path: &str) -> bool {
    note_scope_paths.iter().any(|scope| {
        path == scope
            || path.starts_with(&format!("{scope}/"))
            || scope.starts_with(&format!("{path}/"))
    })
}

fn graph_diff_changed_file_paths(diff: &djinn_mcp::bridge::GraphDiff) -> Vec<String> {
    let mut changed = std::collections::BTreeSet::new();

    for node in diff.added_nodes.iter().chain(diff.removed_nodes.iter()) {
        if node.kind == "file"
            && let Some(path) = node.key.strip_prefix("file:")
        {
            changed.insert(path.to_string());
        }
    }

    for edge in diff.added_edges.iter().chain(diff.removed_edges.iter()) {
        for endpoint in [&edge.from, &edge.to] {
            if let Some(path) = endpoint.strip_prefix("file:") {
                changed.insert(path.to_string());
            }
        }
    }

    changed.into_iter().collect()
}

pub(crate) async fn build_planner_patrol_context(
    task: &Task,
    app_state: &AgentContext,
    project_path: &str,
) -> Option<String> {
    if task.issue_type != "review" || !task.title.to_ascii_lowercase().contains("patrol") {
        return None;
    }

    let graph_ops = app_state.repo_graph_ops.clone()?;
    let diff = graph_ops
        .diff(project_path, Some("previous"))
        .await
        .ok()
        .flatten();
    let ranked = graph_ops
        .ranked(project_path, Some("file"), Some("pagerank"), 20)
        .await
        .ok()
        .unwrap_or_default();

    let project_repo = ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let project_id = project_repo
        .resolve_id_by_path_fuzzy(project_path)
        .await
        .ok()
        .flatten()?;
    let note_repo =
        djinn_db::NoteRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let notes = note_repo
        .list(&project_id, None)
        .await
        .ok()
        .unwrap_or_default();
    let memory_health = note_repo.health(&project_id).await.ok();
    let open_tasks = task_repo
        .list_by_project(&project_id)
        .await
        .ok()
        .unwrap_or_default()
        .into_iter()
        .filter(|candidate| candidate.status != "closed" && candidate.id != task.id)
        .collect::<Vec<_>>();

    let mut documented_paths = Vec::new();
    let changed_paths = diff
        .as_ref()
        .map(graph_diff_changed_file_paths)
        .unwrap_or_default();
    let mut stale_scoped_areas = Vec::new();
    for note in &notes {
        let scopes = parse_json_array(&note.scope_paths);
        if !scopes.is_empty() {
            let note_tags = parse_json_array(&note.tags);
            if changed_paths
                .iter()
                .any(|changed| note_scope_covers_path(&scopes, changed))
            {
                let is_review_needed = note_tags.iter().any(|tag| tag == "review_needed");
                let scope_display = scopes
                    .iter()
                    .take(3)
                    .map(|scope| format!("`{scope}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                stale_scoped_areas.push(format!(
                    "{} scoped to {} (confidence {:.3}, review_needed: {})",
                    note.title,
                    scope_display,
                    note.confidence,
                    if is_review_needed || note.confidence <= djinn_db::STALE_CITATION {
                        "yes"
                    } else {
                        "pending decay"
                    }
                ));
            }
            documented_paths.extend(scopes);
        }
    }

    let mut lines = Vec::new();
    if let Some(health) = memory_health {
        lines.push("### Memory Health Signals".to_string());
        lines.push(format!(
            "- Notes: {} total, {} low-confidence, {} stale, {} duplicate clusters, {} broken links, {} orphans",
            health.total_notes,
            health.low_confidence_note_count,
            health.stale_note_count,
            health.duplicate_cluster_count,
            health.broken_link_count,
            health.orphan_note_count
        ));
        lines.push(format!(
            "- Stale-note folders: {}",
            if health.stale_notes_by_folder.is_empty() {
                "none".to_string()
            } else {
                health
                    .stale_notes_by_folder
                    .iter()
                    .take(4)
                    .map(|folder| format!("`{}` ({})", folder.folder, folder.count))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ));
        lines.push(String::new());
    }
    lines.push("### Code Graph Diff Summary".to_string());

    if let Some(diff) = diff {
        let (new_modules, removed_modules) = graph_diff_module_paths(&diff);
        let new_modules_display = if new_modules.is_empty() {
            "none".to_string()
        } else {
            new_modules
                .iter()
                .take(8)
                .map(|m| format!("`{m}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let removed_modules_display = if removed_modules.is_empty() {
            "none".to_string()
        } else {
            removed_modules
                .iter()
                .take(8)
                .map(|m| format!("`{m}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };

        lines.push(format!(
            "- Commits: {} → {}",
            diff.base_commit.as_deref().unwrap_or("unknown"),
            diff.head_commit.as_deref().unwrap_or("unknown")
        ));
        lines.push(format!("- New modules: {new_modules_display}"));
        lines.push(format!("- Removed modules: {removed_modules_display}"));
        lines.push(format!(
            "- Structural delta: {} added nodes, {} removed nodes, {} added edges, {} removed edges",
            diff.added_nodes.len(),
            diff.removed_nodes.len(),
            diff.added_edges.len(),
            diff.removed_edges.len()
        ));
    } else {
        lines.push("- No previous canonical graph diff is available yet.".to_string());
    }

    let mut undocumented_hotspots = Vec::new();
    let mut weakly_documented_hotspots = Vec::new();
    for node in ranked.into_iter().take(12) {
        let Some(path) = node.key.strip_prefix("file:") else {
            continue;
        };
        let coverage_count = documented_paths
            .iter()
            .filter(|scope| note_scope_covers_path(std::slice::from_ref(scope), path))
            .count();
        let item = format!(
            "`{path}` (score {:.3}, coverage {coverage_count})",
            node.page_rank
        );
        if coverage_count == 0 {
            undocumented_hotspots.push(item);
        } else if coverage_count <= 1 {
            weakly_documented_hotspots.push(item);
        }
    }

    lines.push("\n### Knowledge Coverage Gaps".to_string());
    lines.push(format!(
        "- Undocumented hotspots: {}",
        if undocumented_hotspots.is_empty() {
            "none".to_string()
        } else {
            undocumented_hotspots
                .iter()
                .take(6)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        }
    ));
    lines.push(format!(
        "- Weakly documented hotspots: {}",
        if weakly_documented_hotspots.is_empty() {
            "none".to_string()
        } else {
            weakly_documented_hotspots
                .iter()
                .take(6)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        }
    ));
    lines.push(format!(
        "- Stale scoped-note areas affected by changed code: {}",
        if stale_scoped_areas.is_empty() {
            "none".to_string()
        } else {
            stale_scoped_areas
                .iter()
                .take(4)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        }
    ));

    let budget = planner_patrol_knowledge_task_budget();
    let open_hygiene_tasks = open_tasks
        .iter()
        .filter(|task| is_hygiene_knowledge_task(task))
        .cloned()
        .collect::<Vec<_>>();
    let open_exploration_tasks = open_tasks
        .iter()
        .filter(|task| is_exploration_knowledge_task(task))
        .cloned()
        .collect::<Vec<_>>();

    lines.push("\n### Knowledge Task Guard Rails".to_string());
    lines.push(format!(
        "- Patrol knowledge-task budget: create at most {budget} new hygiene/exploration follow-up tasks this patrol (override with `{PATROL_KNOWLEDGE_TASK_BUDGET_ENV}`, default {DEFAULT_PATROL_KNOWLEDGE_TASK_BUDGET})."
    ));
    lines.push(format!(
        "- Open hygiene knowledge tasks already on the board: {}",
        format_open_knowledge_tasks(&open_hygiene_tasks)
    ));
    lines.push(format!(
        "- Open exploration knowledge tasks already on the board: {}",
        format_open_knowledge_tasks(&open_exploration_tasks)
    ));
    lines.push(
        "- If a relevant hygiene or exploration task is already open for the same area/problem, suppress creating another one and mention the existing task in your patrol summary instead.".to_string(),
    );
    lines.push(format!(
        "- If no similar open knowledge task exists, you may still create eligible follow-up work, but never exceed {budget} total new knowledge tasks in this patrol."
    ));

    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use djinn_core::events::EventBus;
    use djinn_core::models::Project;
    use djinn_db::{Database, NoteRepository, ProjectRepository};
    use djinn_mcp::bridge::{
        CycleGroup, EdgeEntry, GraphDiff, GraphDiffEdge, GraphDiffNode, GraphStatus, ImpactResult,
        NeighborsResult, OrphanEntry, PathResult, RankedNode, RepoGraphOps, SearchHit,
        SymbolDescription,
    };
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    #[derive(Clone)]
    struct FakeRepoGraphOps {
        diff: Option<GraphDiff>,
        ranked: Vec<RankedNode>,
    }

    #[async_trait]
    impl RepoGraphOps for FakeRepoGraphOps {
        async fn neighbors(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<NeighborsResult, String> {
            Err("unused in test".into())
        }

        async fn ranked(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<RankedNode>, String> {
            Ok(self.ranked.clone())
        }

        async fn implementations(&self, _: &str, _: &str) -> Result<Vec<String>, String> {
            Err("unused in test".into())
        }

        async fn impact(
            &self,
            _: &str,
            _: &str,
            _: usize,
            _: Option<&str>,
        ) -> Result<ImpactResult, String> {
            Err("unused in test".into())
        }

        async fn search(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<SearchHit>, String> {
            Err("unused in test".into())
        }

        async fn cycles(
            &self,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<CycleGroup>, String> {
            Err("unused in test".into())
        }

        async fn orphans(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<OrphanEntry>, String> {
            Err("unused in test".into())
        }

        async fn path(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: Option<usize>,
        ) -> Result<Option<PathResult>, String> {
            Err("unused in test".into())
        }

        async fn edges(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<EdgeEntry>, String> {
            Err("unused in test".into())
        }

        async fn diff(&self, _: &str, _: Option<&str>) -> Result<Option<GraphDiff>, String> {
            Ok(self.diff.clone())
        }

        async fn describe(&self, _: &str, _: &str) -> Result<Option<SymbolDescription>, String> {
            Err("unused in test".into())
        }

        async fn status(&self, _: &str) -> Result<GraphStatus, String> {
            Err("unused in test".into())
        }
    }

    async fn setup_project() -> (Database, AgentContext, Project, tempfile::TempDir) {
        let db = Database::open_in_memory().expect("db");
        db.ensure_initialized().await.expect("init db");
        let tmp = crate::test_helpers::test_tempdir("planner-patrol-context-");
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = project_repo
            .create("test-project", tmp.path().to_str().expect("tmp path"))
            .await
            .expect("create project");
        let ctx = crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
        (db, ctx, project, tmp)
    }

    fn patrol_task(project_id: &str) -> Task {
        Task {
            id: uuid::Uuid::now_v7().to_string(),
            project_id: project_id.to_string(),
            short_id: "ptst".to_string(),
            epic_id: None,
            title: "Planner patrol: board health review".to_string(),
            description: String::new(),
            design: String::new(),
            issue_type: "review".to_string(),
            status: "open".to_string(),
            priority: 1,
            owner: "planner".to_string(),
            labels: "[]".to_string(),
            acceptance_criteria: "[]".to_string(),
            reopen_count: 0,
            continuation_count: 0,
            verification_failure_count: 0,
            total_reopen_count: 0,
            total_verification_failure_count: 0,
            intervention_count: 0,
            last_intervention_at: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            closed_at: None,
            close_reason: None,
            merge_commit_sha: None,
            pr_url: None,
            merge_conflict_metadata: None,
            memory_refs: "[]".to_string(),
            agent_type: None,
            unresolved_blocker_count: 0,
        }
    }

    #[tokio::test]
    async fn planner_patrol_context_reports_diff_and_coverage_gaps() {
        let (db, mut ctx, project, tmp) = setup_project().await;
        let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
        note_repo
            .create_with_scope(
                &project.id,
                tmp.path(),
                "Server subsystem overview",
                "documents server source",
                "reference",
                None,
                "[]",
                r#"["server/src/existing.rs"]"#,
            )
            .await
            .expect("create scoped note");
        let stale_note = note_repo
            .create_with_scope(
                &project.id,
                tmp.path(),
                "Stale changed-area note",
                "needs review after canonical graph changes",
                "reference",
                None,
                r#"["review_needed"]"#,
                r#"["server/src/new_area.rs"]"#,
            )
            .await
            .expect("create stale scoped note");
        note_repo
            .set_confidence(&stale_note.id, 0.2)
            .await
            .expect("lower stale note confidence");

        ctx.repo_graph_ops = Some(Arc::new(FakeRepoGraphOps {
            diff: Some(GraphDiff {
                base_commit: Some("abc123".to_string()),
                head_commit: Some("def456".to_string()),
                added_nodes: vec![GraphDiffNode {
                    key: "file:server/src/new_area.rs".to_string(),
                    kind: "file".to_string(),
                    display_name: "server/src/new_area.rs".to_string(),
                }],
                removed_nodes: vec![GraphDiffNode {
                    key: "file:server/src/old_area.rs".to_string(),
                    kind: "file".to_string(),
                    display_name: "server/src/old_area.rs".to_string(),
                }],
                added_edges: vec![GraphDiffEdge {
                    from: "file:server/src/new_area.rs".to_string(),
                    to: "file:server/src/lib.rs".to_string(),
                    edge_kind: "FileReference".to_string(),
                }],
                removed_edges: vec![],
            }),
            ranked: vec![
                RankedNode {
                    key: "file:server/src/new_area.rs".to_string(),
                    kind: "file".to_string(),
                    display_name: "server/src/new_area.rs".to_string(),
                    score: 10.0,
                    page_rank: 0.91,
                    structural_weight: 1.0,
                    inbound_edge_weight: 1.0,
                    outbound_edge_weight: 1.0,
                },
                RankedNode {
                    key: "file:server/src/existing.rs".to_string(),
                    kind: "file".to_string(),
                    display_name: "server/src/existing.rs".to_string(),
                    score: 9.0,
                    page_rank: 0.75,
                    structural_weight: 1.0,
                    inbound_edge_weight: 1.0,
                    outbound_edge_weight: 1.0,
                },
            ],
        }));

        let summary = build_planner_patrol_context(&patrol_task(&project.id), &ctx, &project.path)
            .await
            .expect("planner patrol context");

        assert!(summary.contains("Memory Health Signals"));
        assert!(summary.contains("1 low-confidence"));
        assert!(summary.contains("Code Graph Diff Summary"));
        assert!(summary.contains("`server/src/new_area.rs`"));
        assert!(summary.contains("`server/src/old_area.rs`"));
        assert!(summary.contains("Weakly documented hotspots: `server/src/new_area.rs`"));
        assert!(summary.contains("`server/src/existing.rs` (score 0.750, coverage 1)"));
        assert!(summary.contains("Stale scoped-note areas affected by changed code:"));
        assert!(summary.contains("Stale changed-area note scoped to `server/src/new_area.rs`"));
        assert!(summary.contains("Knowledge Task Guard Rails"));
        assert!(
            summary
                .contains("create at most 2 new hygiene/exploration follow-up tasks this patrol")
        );
        assert!(summary.contains("you may still create eligible follow-up work"));
    }

    #[tokio::test]
    async fn planner_patrol_context_suppresses_duplicate_knowledge_follow_ups_when_similar_tasks_are_open()
     {
        let (db, mut ctx, project, _tmp) = setup_project().await;
        let task_repo = TaskRepository::new(db.clone(), EventBus::noop());

        ctx.repo_graph_ops = Some(Arc::new(FakeRepoGraphOps {
            diff: None,
            ranked: vec![],
        }));

        task_repo
            .create_in_project(
                &project.id,
                None,
                "Consolidate duplicate notes about planner patrol",
                "memory hygiene cleanup for duplicate cluster",
                "",
                "planning",
                1,
                "planner",
                Some("open"),
                None,
            )
            .await
            .expect("create hygiene task");

        task_repo
            .create_in_project(
                &project.id,
                None,
                "Explore and document server/src/new_area.rs",
                "undocumented subsystem knowledge gap",
                "",
                "spike",
                1,
                "architect",
                Some("open"),
                None,
            )
            .await
            .expect("create exploration task");

        let summary = build_planner_patrol_context(&patrol_task(&project.id), &ctx, &project.path)
            .await
            .expect("planner patrol context");

        assert!(summary.contains("Open hygiene knowledge tasks already on the board: `"));
        assert!(summary.contains("Consolidate duplicate notes about planner patrol"));
        assert!(summary.contains("Open exploration knowledge tasks already on the board: `"));
        assert!(summary.contains("Explore and document server/src/new_area.rs"));
        assert!(summary.contains(
            "If a relevant hygiene or exploration task is already open for the same area/problem, suppress creating another one"
        ));
    }
}
