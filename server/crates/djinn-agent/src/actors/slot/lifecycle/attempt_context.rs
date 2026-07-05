use djinn_core::models::Task;
use djinn_core::models::task_attempt::TaskAttemptPromptSummary;
use djinn_db::{CompletedParentSummary, TaskAttemptRepository};

/// Maximum number of prior terminal attempt summaries loaded for the current task.
const MAX_PRIOR_ATTEMPTS: i64 = 3;
/// Maximum number of completed dependency-parent summaries loaded from blocker parents.
const MAX_COMPLETED_DEPENDENCY_PARENTS: i64 = 5;

/// Load the most recent terminal attempt summaries for the current task.
///
/// Bounded to 3 rows, newest first, excluding non-terminal `pending` and
/// `submitted` rows. `log_tail` is never requested from the repository (the
/// [`TaskAttemptPromptSummary`] DTO does not carry it). Returns `None` on any
/// repository error or when the result is empty so prompt assembly stays
/// non-fatal.
pub(crate) async fn load_prior_attempts(
    task: &Task,
    repo: &TaskAttemptRepository,
) -> Option<Vec<TaskAttemptPromptSummary>> {
    let rows = repo
        .prompt_summaries_for_task(&task.id, None, MAX_PRIOR_ATTEMPTS)
        .await
        .inspect_err(|e| {
            tracing::debug!(
                task_id = %task.short_id,
                error = %e,
                "Lifecycle: failed to load prior attempt summaries"
            );
        })
        .ok()?;

    let terminal: Vec<TaskAttemptPromptSummary> = rows
        .into_iter()
        .filter(|r| is_terminal_outcome(&r.outcome))
        .collect();

    if terminal.is_empty() {
        return None;
    }
    // `prompt_summaries_for_task` already orders newest-first by `created_at`,
    // but re-sort defensively by terminal time (falling back to created time)
    // so the contract holds even if repository ordering changes.
    let mut sorted = terminal;
    sorted.sort_by(|a, b| {
        b.terminal_at
            .as_deref()
            .unwrap_or(&a.created_at)
            .cmp(a.terminal_at.as_deref().unwrap_or(&b.created_at))
    });
    Some(sorted)
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
        .completed_blocker_parent_summaries(&task.id, MAX_COMPLETED_DEPENDENCY_PARENTS)
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

/// Returns `true` when the outcome string names a terminal attempt state
/// (anything other than `pending` or `submitted`).
fn is_terminal_outcome(outcome: &str) -> bool {
    !matches!(outcome, "pending" | "submitted")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_terminal_outcome_classifies_outcomes() {
        assert!(!is_terminal_outcome("pending"));
        assert!(!is_terminal_outcome("submitted"));
        assert!(is_terminal_outcome("completed"));
        assert!(is_terminal_outcome("reopened"));
        assert!(is_terminal_outcome("crashed"));
        assert!(is_terminal_outcome("timed_out"));
        assert!(is_terminal_outcome("cancelled"));
        assert!(is_terminal_outcome("loop_guard_tripped"));
        assert!(is_terminal_outcome("deferred"));
        assert!(is_terminal_outcome("force_closed"));
    }

    #[test]
    fn constants_match_acceptance_bounds() {
        assert_eq!(MAX_PRIOR_ATTEMPTS, 3);
        assert_eq!(MAX_COMPLETED_DEPENDENCY_PARENTS, 5);
    }
}
