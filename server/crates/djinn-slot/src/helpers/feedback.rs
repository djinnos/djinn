use super::*;
use crate::host::SlotContext;
use djinn_orchestration_types::slot::{MERGE_CONFLICT_PREFIX, MergeConflictMetadata};

/// Return the most recent N high-signal comments (lead, reviewer, verification)
/// from the activity log, in chronological order (oldest first).
/// Includes structured rejected `review_submitted` payloads even when there is no
/// matching `comment`-typed twin.
/// Each entry is formatted as "**Label:** body".
pub fn recent_feedback(activity: &[djinn_core::models::ActivityEntry], max: usize) -> Vec<String> {
    let high_signal: Vec<&djinn_core::models::ActivityEntry> = activity
        .iter()
        .rev()
        .filter(|e| {
            (e.event_type == "comment"
                && (e.actor_role == "lead"
                    || e.actor_role == "pm"
                    || e.actor_role == "architect"
                    || e.actor_role == "reviewer"
                    || e.actor_role == "task_reviewer"
                    || e.actor_role == "verification"))
                || (e.event_type == "review_submitted"
                    && e.actor_role == "reviewer"
                    && is_rejected_review(e))
        })
        .take(max)
        .collect();
    // Reverse back to chronological order
    high_signal
        .into_iter()
        .rev()
        .filter_map(|e| {
            if e.event_type == "review_submitted" {
                let payload = serde_json::from_str::<serde_json::Value>(&e.payload).ok()?;
                let body = payload.get("feedback").and_then(|v| v.as_str())?;
                if body.is_empty() {
                    return None;
                }
                return Some(format!("**Reviewer rejection:**\n{body}"));
            }
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

/// Return true when the activity entry is a structured rejected review submission.
/// The `review_submitted` event is logged by `submit_review` with a `verdict` field.
fn is_rejected_review(entry: &djinn_core::models::ActivityEntry) -> bool {
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(&entry.payload) else {
        return false;
    };
    payload.get("verdict").and_then(|v| v.as_str()) == Some("rejected")
}

/// Extract worker submission summary/concerns from the activity log so the
/// reviewer sees why the worker made certain changes.
///
/// Returns `(worker_summary, worker_concerns)`.
pub fn extract_worker_context(
    activity: &Option<Vec<djinn_core::models::ActivityEntry>>,
) -> (Option<String>, Option<String>) {
    let Some(entries) = activity else {
        return (None, None);
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
    (worker_summary, worker_concerns)
}

/// Build a formatted PR review feedback section for the worker prompt.
///
/// Queries the task activity log for the most recent `pr_review_feedback` entry
/// (stored by the PR poller when CHANGES_REQUESTED is detected) and formats it
/// as a structured section with inline code comments so the worker knows exactly
/// what to fix.
///
/// Returns `None` when no PR review feedback exists for the task.
pub async fn pr_review_feedback_context(task_id: &str, app_state: &SlotContext) -> Option<String> {
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

/// Truncate feedback text using 60/40 head+tail split.
fn truncate_feedback(text: &str, max: usize) -> String {
    crate::truncate::smart_truncate(text, max)
}

/// Per-reason character cap inside a single ledger bullet.
const LEDGER_REASON_MAX_CHARS: usize = 120;

/// Character budget for the entire formatted reopen-ledger section.
pub const LEDGER_BUDGET_CHARS: usize = 2000;

/// Format up to six recent reopen ledger entries as a compact, worker-facing
/// section. Entries are presented in chronological order (oldest first / newest
/// last) with ascending round numbers so the worker can see progression.
///
/// Each line is formatted as:
///   `Round N — reopen_class (from: from_status): truncated_reason`
///
/// The entire output is bounded by [`LEDGER_BUDGET_CHARS`] via
/// [`truncate_feedback`].
pub fn format_reopen_ledger(entries: &[ReopenLedgerEntry]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    // DB returns newest-first; reverse so oldest → newest for worker context.
    let mut lines: Vec<String> = Vec::with_capacity(entries.len() + 1);
    lines.push("**Reopen history (newest last):**".to_string());
    for (i, entry) in entries.iter().rev().enumerate() {
        let round = i + 1;
        let class = &entry.reopen_class;
        let from = if entry.from_status.is_empty() {
            "—"
        } else {
            entry.from_status.as_str()
        };
        let reason_snippet = match &entry.reason {
            Some(r) if !r.is_empty() => {
                let trimmed = r.trim();
                let snippet: String = trimmed.chars().take(LEDGER_REASON_MAX_CHARS).collect();
                if trimmed.chars().count() > LEDGER_REASON_MAX_CHARS {
                    format!("{snippet}…")
                } else {
                    snippet
                }
            }
            _ => "—".to_string(),
        };
        lines.push(format!(
            "Round {round} — {class} (from: {from}): {reason_snippet}"
        ));
    }
    let raw = lines.join("\n");
    Some(truncate_feedback(&raw, LEDGER_BUDGET_CHARS))
}

/// Character budget for the formatted attempt-history section.
///
/// Sized so the attempt context fits within the combined brief budget and
/// does not dominate the feedback block.
pub const ATTEMPT_HISTORY_BUDGET_CHARS: usize = 3000;

/// Format prior attempt history and completed dependency-parent summaries
/// as an extension of the existing feedback/activity narrative.
///
/// Returns `None` when both `prior_attempts` and `completed_parents` are
/// empty, so callers can skip the section entirely.
pub fn format_attempt_history(
    prior_attempts: &[djinn_core::models::task_attempt::TaskAttemptPromptSummary],
    completed_parents: &[djinn_db::CompletedParentSummary],
) -> Option<String> {
    if prior_attempts.is_empty() && completed_parents.is_empty() {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();
    lines.push("**Prior attempts (newest first):**".to_string());
    for attempt in prior_attempts {
        lines.push(format_single_attempt(attempt));
    }
    if !completed_parents.is_empty() {
        lines.push(String::new());
        lines.push("**Completed dependency parents:**".to_string());
        for parent in completed_parents {
            lines.push(format_completed_parent(parent));
        }
    }
    let raw = lines.join("\n");
    Some(truncate_feedback(&raw, ATTEMPT_HISTORY_BUDGET_CHARS))
}

/// Format a single prior attempt entry for prompt context.
fn format_single_attempt(a: &djinn_core::models::task_attempt::TaskAttemptPromptSummary) -> String {
    let mut parts = Vec::new();
    parts.push(format!(
        "- Attempt #{} ({}): {}",
        a.attempt_seq, a.role, a.outcome
    ));

    // Guard decision/reason (present for deferred attempts).
    if let Some(decision) = &a.guard_decision
        && !decision.is_empty()
    {
        let reason = a
            .guard_reason
            .as_deref()
            .filter(|r| !r.is_empty())
            .unwrap_or("—");
        parts.push(format!("  guard: {decision} ({reason})"));
    }

    // Timestamps.
    parts.push(format!("  created: {}", a.created_at));
    if let Some(terminal) = &a.terminal_at {
        parts.push(format!("  terminal: {terminal}"));
    }

    // Summary or deterministic fallback.
    let summary_text = match &a.summary {
        Some(s) if !s.is_empty() => s.clone(),
        _ => attempt_fallback_for_outcome(&a.outcome),
    };
    parts.push(format!("  summary: {summary_text}"));

    // Selected summary_json fields.
    if let Some(json_str) = &a.summary_json
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(json_str)
    {
        if let Some(fc) = json.get("failure_class").and_then(|v| v.as_str()) {
            parts.push(format!("  failure_class: {fc}"));
        }
        if let Some(lv) = json.get("last_verify").and_then(|v| v.as_str()) {
            parts.push(format!("  last_verify: {lv}"));
        }
    }

    // Checkpoint/submit/PR refs.
    if let Some(cr) = &a.checkpoint_ref
        && !cr.is_empty()
    {
        parts.push(format!("  checkpoint: `{cr}`"));
    }
    if let Some(sr) = &a.submit_ref
        && !sr.is_empty()
    {
        parts.push(format!("  submit_ref: `{sr}`"));
    }
    if let Some(pr) = &a.pr_url
        && !pr.is_empty()
    {
        parts.push(format!("  PR: {pr}"));
    }

    parts.join("\n")
}

/// Format a completed dependency-parent entry for prompt context.
fn format_completed_parent(parent: &djinn_db::CompletedParentSummary) -> String {
    let mut parts = Vec::new();
    parts.push(format!(
        "- Parent {} ({}): closed {}",
        parent.short_id, parent.title, parent.terminal_at
    ));
    if let Some(attempt) = &parent.latest_completed_attempt {
        let summary_text = match &attempt.summary {
            Some(s) if !s.is_empty() => s.as_str(),
            _ => "completed (no summary available)",
        };
        parts.push(format!(
            "  latest completed attempt #{attempt_seq}: {summary_text}",
            attempt_seq = attempt.attempt_seq
        ));
        if let Some(sr) = &attempt.submit_ref
            && !sr.is_empty()
        {
            parts.push(format!("  submit_ref: `{sr}`"));
        }
        if let Some(pr) = &attempt.pr_url
            && !pr.is_empty()
        {
            parts.push(format!("  PR: {pr}"));
        }
        // Summary_json fields for parent attempts.
        if let Some(json_str) = &attempt.summary_json
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(json_str)
        {
            if let Some(fc) = json.get("failure_class").and_then(|v| v.as_str()) {
                parts.push(format!("  failure_class: {fc}"));
            }
            if let Some(lv) = json.get("last_verify").and_then(|v| v.as_str()) {
                parts.push(format!("  last_verify: {lv}"));
            }
        }
    }
    parts.join("\n")
}

/// Deterministic fallback text for attempts without a summary, differentiated
/// by outcome.
fn attempt_fallback_for_outcome(outcome: &str) -> String {
    match outcome {
        "crashed" => "attempt crashed (no summary recorded)".to_string(),
        "timed_out" => "attempt timed out (no summary recorded)".to_string(),
        "spawn_failed" => {
            "attempt spawn failed — worker process did not start (no summary recorded)".to_string()
        }
        "deferred" => "attempt deferred by guard (no summary recorded)".to_string(),
        "completed" => "completed (no summary recorded)".to_string(),
        "reopened" => "attempt reopened (no summary recorded)".to_string(),
        "cancelled" => "attempt cancelled (no summary recorded)".to_string(),
        "loop_guard_tripped" => {
            "attempt terminated by loop guard (no summary recorded)".to_string()
        }
        "force_closed" => "attempt force-closed (no summary recorded)".to_string(),
        "handoff" => "attempt handed off (no summary recorded)".to_string(),
        "adopted_pr" => "attempt adopted existing PR (no summary recorded)".to_string(),
        _ => format!("attempt {outcome} (no summary recorded)"),
    }
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
pub fn format_command_details(specs: &[djinn_core::commands::CommandSpec]) -> Option<String> {
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

pub fn runtime_fs_diagnostics(project_path: &str, worktree_path: &Path) -> String {
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

pub fn runtime_env_diagnostics(
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

pub async fn load_task(task_id: &str, app_state: &SlotContext) -> anyhow::Result<Task> {
    let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let task = repo
        .get(task_id)
        .await
        .map_err(|e| anyhow::anyhow!("db error loading task: {e}"))?;
    task.ok_or_else(|| anyhow::anyhow!("task not found: {task_id}"))
}

pub async fn default_target_branch(project_id: &str, app_state: &SlotContext) -> String {
    let repo = ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    if let Ok(Some(config)) = repo.get_config(project_id).await {
        return config.target_branch;
    }
    "main".to_string()
}

/// Extract the `reason` field from the most recent `status_changed` activity
/// entry that represents a review-to-open rejection (from_status =
/// "in_task_review", to_status = "open"). Searches backwards through ALL
/// status_changed events, not just the very last one, so that intervening
/// transitions (e.g. review failures cycling back to open)
/// don't obscure the original rejection reason.
async fn last_review_rejection_reason(task_id: &str, app_state: &SlotContext) -> Option<String> {
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

pub async fn conflict_context_for_dispatch(
    task_id: &str,
    app_state: &SlotContext,
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

pub fn parse_conflict_metadata(reason: &str) -> Option<MergeConflictMetadata> {
    let raw = reason.strip_prefix(MERGE_CONFLICT_PREFIX)?;
    serde_json::from_str(raw).ok()
}

/// Per-section char budget for a SINGLE-source initial directive (reviewer-only
/// or CI-only). Mirrors `MAX_VERIFICATION_CHARS` magnitude.
const MAX_COMBINED_SECTION_CHARS: usize = 3000;

/// Total char budget shared by the two sections (reviewer feedback + CI
/// feedback) of the COMBINED rework brief. Sized so a worker turn carries
/// actionable detail from both sources without an unbounded payload.
pub const COMBINED_BRIEF_TOTAL_CHARS: usize = 14_000;

/// Guaranteed floor each section keeps when BOTH are present in the combined
/// brief, so a huge blob in one source can never fully starve the other. The
/// budget above the two floors is shared; whatever a small section doesn't use
/// is lent to the large one (up to the full total).
pub const COMBINED_BRIEF_SECTION_FLOOR_CHARS: usize = 3000;

/// Fairly split `COMBINED_BRIEF_TOTAL_CHARS` between the reviewer section and
/// the CI section, then `smart_truncate` each to its allotment.
///
/// Budgeting rules:
/// - Each section is guaranteed at least `COMBINED_BRIEF_SECTION_FLOOR_CHARS`.
/// - Whatever a section doesn't need (it's shorter than its fair share) is lent
///   to the other section — a small reviewer blob frees room for a large CI log
///   and vice versa, up to the full total.
/// - Neither section is starved by an oversized peer; when both overflow the
///   shared pool it is split evenly.
///
/// Returns `(reviewer_out, ci_out)`, each already truncated with a clear
/// `[truncated …]` marker (via `smart_truncate`) when it exceeded its budget.
pub fn budget_combined_sections(reviewer: &str, ci: &str) -> (String, String) {
    let total = COMBINED_BRIEF_TOTAL_CHARS;
    let floor = COMBINED_BRIEF_SECTION_FLOOR_CHARS.min(total / 2);
    // Shared pool available above the two guaranteed floors.
    let shared = total.saturating_sub(2 * floor);
    // How much each section wants ABOVE its floor.
    let rev_extra_want = reviewer.len().saturating_sub(floor);
    let ci_extra_want = ci.len().saturating_sub(floor);
    let (rev_extra, ci_extra) = if rev_extra_want + ci_extra_want <= shared {
        // Both fit within the shared pool — give each exactly what it wants.
        (rev_extra_want, ci_extra_want)
    } else if rev_extra_want == 0 {
        // Reviewer fits in its floor; lend the whole shared pool to CI.
        (0, shared)
    } else if ci_extra_want == 0 {
        (shared, 0)
    } else {
        // Both want more than the shared pool can satisfy → split it evenly,
        // but never hand a section more than it can use (lend the surplus back).
        let half = shared / 2;
        if rev_extra_want <= half {
            (rev_extra_want, shared - rev_extra_want)
        } else if ci_extra_want <= half {
            (shared - ci_extra_want, ci_extra_want)
        } else {
            (half, shared - half)
        }
    };
    (
        truncate_feedback(reviewer, floor + rev_extra),
        truncate_feedback(ci, floor + ci_extra),
    )
}

/// Find the most recent CI-failure / verification feedback that belongs to the
/// CURRENT rework cycle — i.e. logged at or after the latest PR-review-feedback
/// entry (the two are written in the same poller tick when a PR has both
/// reviewer changes-requested and failing CI). This guards against surfacing a
/// stale CI comment from an earlier head SHA.
///
/// Find the RAW (untruncated) CI/verification body for the CURRENT rework cycle.
/// The combined-brief path applies its own fair per-section budget downstream
/// (`budget_combined_sections`), so no pre-clipping happens here.
///
/// Returns the comment body, or `None` when there's no in-cycle CI/verification comment.
pub fn raw_ci_feedback_in_cycle(
    activity: &[djinn_core::models::ActivityEntry],
    not_before: Option<&str>,
) -> Option<String> {
    let entry = activity
        .iter()
        .rev()
        .filter(|e| e.event_type == "comment" && e.actor_role == "verification")
        .find(|e| match not_before {
            // `created_at` is an ISO-8601 string → lexicographic compare is
            // chronological. Only accept feedback from this cycle onward.
            Some(floor) => e.created_at.as_str() >= floor,
            None => true,
        })?;
    let payload = serde_json::from_str::<serde_json::Value>(&entry.payload).ok()?;
    payload
        .get("body")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Build an initial user message for a fresh worker session. If the activity
/// log contains lead or reviewer feedback, include it prominently so the worker
/// acts on it immediately rather than discovering it buried in the system prompt.
///
/// PR review feedback (from GitHub reviewer inline comments) is surfaced first
/// when present — this is the most specific, actionable signal available. When
/// the same rework cycle ALSO carries failing-CI feedback (reviewer requested
/// changes AND required checks are red), both are combined into a single
/// "address all of the following in one pass" directive so the worker fixes
/// everything at once instead of one source per rework cycle.
pub async fn initial_user_message_for_task(task_id: &str, app_state: &SlotContext) -> String {
    let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let activity = repo.list_activity(task_id).await.ok().unwrap_or_default();
    // PR review feedback takes priority over generic activity log comments.
    let mut msg = if let Some(pr_feedback) = pr_review_feedback_context(task_id, app_state).await {
        // Find when the current reviewer-feedback cycle was logged so we only
        // pull CI feedback from the same cycle (not a stale earlier-head comment).
        let review_cycle_floor = activity
            .iter()
            .rev()
            .find(|e| e.event_type == PR_REVIEW_FEEDBACK_EVENT && e.actor_role == "system")
            .map(|e| e.created_at.clone());
        // When this same cycle also produced a CI failure, compose BOTH sources
        // into one directive. Otherwise keep today's reviewer-only behavior.
        if let Some(ci_raw) = raw_ci_feedback_in_cycle(&activity, review_cycle_floor.as_deref()) {
            // Fairly budget the two sections so neither a huge reviewer blob nor
            // a huge CI log can starve the other (E5). Unused budget is lent.
            let (reviewer_section, ci_section) = budget_combined_sections(&pr_feedback, &ci_raw);
            format!(
                "This PR has TWO blocking problems. Address ALL of the following in one pass before responding:\n\n\
                **(A) A human reviewer requested changes.** Address every reviewer comment below:\n\n\
                {reviewer_section}\n\n\
                ---\n\n\
                **(B) Required CI checks are failing.** Fix every failure below:\n\n\
                {ci_section}\n\n\
                ---\n\n\
                Resolve (A) and (B) together, then push fixup commits to the same branch. Do not open a new PR."
            )
        } else {
            // Reviewer-only: the single section gets the full single-source budget.
            let reviewer_section = truncate_feedback(&pr_feedback, MAX_COMBINED_SECTION_CHARS);
            format!(
                "A human reviewer has requested changes on the PR. Address every reviewer comment below:\n\n\
                {reviewer_section}\n\n\
                Push fixup commits to the same branch. Do not open a new PR."
            )
        }
    } else {
        let sections = recent_feedback(&activity, 3);
        if sections.is_empty() {
            "Start by understanding the task context and execute it fully before stopping."
                .to_string()
        } else {
            format!(
                "The activity log contains important feedback from prior sessions. Read it carefully and act on it:\n\n{}\n\nAddress this feedback, make the necessary changes, then stop.",
                sections.join("\n\n---\n\n")
            )
        }
    };

    // Append compact reopen failure ledger when available. Query failures are
    // tolerated — the dispatch proceeds without the ledger section.
    if let Ok(ledger) = repo.recent_reopen_ledger(task_id, 6).await
        && let Some(section) = format_reopen_ledger(&ledger)
    {
        msg.push_str("\n\n---\n\n");
        msg.push_str(&section);
    }

    msg
}
