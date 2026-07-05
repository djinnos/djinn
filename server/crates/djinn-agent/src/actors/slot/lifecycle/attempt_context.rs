use djinn_core::models::Task;
use djinn_db::{CompletedParentSummary, TaskAttemptRepository};

use crate::context::AgentContext;

/// Load the most recent terminal attempt summaries for the current task.
///
/// Bounded to 3 rows, newest first, excluding non-terminal `pending` and
/// `submitted` rows. `log_tail` is not requested from the repository. Returns
/// `None` on any repository error or when the result is empty so prompt assembly
/// stays non-fatal.
pub(crate) async fn load_prior_attempts(
    task: &Task,
    repo: &TaskAttemptRepository,
) -> Option<Vec<CompletedParentSummary>> {
    let mut rows = repo
        .prompt_summaries_for_task(&task.id, None, 3)
        .await
        .inspect_err(|e| {
            tracing::debug!(
                task_id = %task.short_id,
                error = %e,
                "Lifecycle: failed to load prior attempt summaries"
            );
        })
        .ok()?
        .into_iter()
        .filter(|r| is_terminal_outcome(&r.outcome))
        .map(summary_to_completed_parent)
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return None;
    }
    // `prompt_summaries_for_task` returns newest-first, but ensure that is the
    // contract in case repository ordering changes.
    rows.sort_by(|a, b| b.terminal_at.cmp(&a.terminal_at));
    Some(rows)
}

/// Load completed dependency-parent summaries from the task's blocker parents.
///
/// At most 5 completed blocker parents are returned, ordered by parent
/// completed-at descending then stable task id. Only each parent's latest
/// `completed` attempt summary and refs are included. Returns `None` on any
/// repository error or when the result is empty.
pub(crate) async fn load_completed_dependency_parents(
    task: &Task,
    repo: &TaskAttemptRepository,
) -> Option<Vec<CompletedParentSummary>> {
    let rows = repo
        .completed_blocker_parent_summaries(&task.id, 5)
        .await
        .inspect_err(|e| {
            tracing::debug!(
                task_id = %task.short_id,
                error = %e,
                "Lifecycle: failed to load completed dependency parents"
            );
        })
        .ok()?;
    if rows.is_empty() {
        return None;
    }
    Some(rows)
}

fn is_terminal_outcome(outcome: &str) -> bool {
    !matches!(outcome, "pending" | "submitted")
}

fn summary_to_completed_parent(
    summary: djinn_core::models::task_attempt::TaskAttemptPromptSummary,
) -> CompletedParentSummary {
    CompletedParentSummary {
        task_id: String::new(),
        short_id: summary.attempt_seq.to_string(),
        title: String::new(),
        terminal_at: summary
            .terminal_at
            .clone()
            .unwrap_or_else(|| summary.created_at.clone()),
        latest_completed_attempt: Some(summary),
    }
}
