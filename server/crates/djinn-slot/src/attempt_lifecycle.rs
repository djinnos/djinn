//! Attempt lifecycle instrumentation helpers for the submission advancement
//! paths in the slot/agent side.
//!
//! These helpers are best-effort: write failures are logged but never fail the
//! user-facing submit path. They consume the foundation APIs delivered by epic
//! `u74z`.

use djinn_db::{SubmitTaskAttemptParams, TaskAttemptRepository};

use crate::host::SlotContext;

/// Parameters for the submission advancement helper.
pub struct SubmitAdvancementParams<'a> {
    pub task_id: &'a str,
    pub role: &'a str,
    pub submit_ref: Option<&'a str>,
    pub checkpoint_ref: Option<&'a str>,
    pub mirror_head_sha: Option<&'a str>,
    pub github_head_sha: Option<&'a str>,
    pub summary: Option<&'a str>,
    pub summary_json: Option<&'a str>,
}

/// Advance the latest pending/submitted attempt for a task+role to
/// `submitted`, filling available refs and summary fields.
///
/// Looks up the matching attempt via `latest_pending_or_submitted`; if no
/// pending attempt exists (e.g. the dispatch-start write failed or was
/// skipped), this is a silent no-op.
///
/// Best-effort: errors are logged and never propagated.
pub async fn advance_to_submitted(ctx: &SlotContext, params: SubmitAdvancementParams<'_>) {
    let repo = TaskAttemptRepository::new(ctx.db.clone());
    let attempt = match repo
        .latest_pending_or_submitted(params.task_id, Some(params.role))
        .await
    {
        Ok(Some(a)) => a,
        Ok(None) => {
            tracing::debug!(
                task_id = %params.task_id,
                role = %params.role,
                "attempt_lifecycle: no pending/submitted attempt found for submission advancement; skipping"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                task_id = %params.task_id,
                role = %params.role,
                error = %e,
                "attempt_lifecycle: failed to look up attempt for submission advancement"
            );
            return;
        }
    };

    // Already terminal — forward-only lifecycle; do not move backward.
    if attempt.outcome != "pending" && attempt.outcome != "submitted" {
        tracing::debug!(
            task_id = %params.task_id,
            role = %params.role,
            attempt_id = %attempt.id,
            current_outcome = %attempt.outcome,
            "attempt_lifecycle: attempt is already terminal; skipping submission advancement"
        );
        return;
    }

    match repo
        .advance_to_submitted(SubmitTaskAttemptParams {
            id: &attempt.id,
            submit_ref: params.submit_ref,
            checkpoint_ref: params.checkpoint_ref,
            mirror_head_sha: params.mirror_head_sha,
            github_head_sha: params.github_head_sha,
            summary: params.summary,
            summary_json: params.summary_json,
            log_tail: None,
        })
        .await
    {
        Ok(updated) => {
            tracing::info!(
                task_id = %params.task_id,
                role = %params.role,
                attempt_id = %updated.id,
                dispatch_key = %updated.dispatch_key,
                outcome = %updated.outcome,
                submitted_at = ?updated.submitted_at,
                "attempt_lifecycle: submission advancement recorded"
            );
        }
        Err(e) => {
            tracing::warn!(
                task_id = %params.task_id,
                role = %params.role,
                attempt_id = %attempt.id,
                error = %e,
                "attempt_lifecycle: failed to advance attempt to submitted (best-effort)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;
    use djinn_db::{CreateTaskAttemptParams, Database, TaskAttemptRepository};

    fn create_slot_ctx() -> (Database, SlotContext) {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        (db, ctx)
    }

    async fn create_task(db: &Database) -> djinn_core::models::Task {
        let project = test_helpers::create_test_project(db).await;
        let epic = test_helpers::create_test_epic(db, &project.id).await;
        test_helpers::create_test_task(db, &project.id, &epic.id).await
    }

    async fn seed_pending(db: &Database, task_id: &str, role: &str, dispatch_key: &str) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        TaskAttemptRepository::new(db.clone())
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id,
                role,
                dispatch_key,
                session_id: None,
                dispatch_owner_incarnation_id: None,
                dispatch_group_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap()
            .id
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slot_advance_moves_pending_to_submitted() {
        let (db, ctx) = create_slot_ctx();
        let task = create_task(&db).await;
        let attempt_id = seed_pending(&db, &task.id, "worker", "dk-slot-1").await;

        advance_to_submitted(
            &ctx,
            SubmitAdvancementParams {
                task_id: &task.id,
                role: "worker",
                submit_ref: Some("submit-ref"),
                checkpoint_ref: None,
                mirror_head_sha: Some("sha-abc"),
                github_head_sha: None,
                summary: Some("work done"),
                summary_json: None,
            },
        )
        .await;

        let repo = TaskAttemptRepository::new(db);
        let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
        assert_eq!(attempt.outcome, "submitted");
        assert!(attempt.submitted_at.is_some());
        assert_eq!(attempt.submit_ref.as_deref(), Some("submit-ref"));
        assert_eq!(attempt.mirror_head_sha.as_deref(), Some("sha-abc"));
        assert_eq!(attempt.summary.as_deref(), Some("work done"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slot_advance_noop_when_no_pending_attempt() {
        let (_db, ctx) = create_slot_ctx();
        // No attempt seeded — should be a silent no-op.
        advance_to_submitted(
            &ctx,
            SubmitAdvancementParams {
                task_id: "nonexistent-task",
                role: "worker",
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: None,
                summary_json: None,
            },
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slot_advance_idempotent_does_not_overwrite_refs() {
        let (db, ctx) = create_slot_ctx();
        let task = create_task(&db).await;
        let attempt_id = seed_pending(&db, &task.id, "worker", "dk-slot-idem").await;

        // First advance with refs.
        advance_to_submitted(
            &ctx,
            SubmitAdvancementParams {
                task_id: &task.id,
                role: "worker",
                submit_ref: Some("original"),
                checkpoint_ref: Some("cp-1"),
                mirror_head_sha: Some("mirror-sha"),
                github_head_sha: None,
                summary: Some("original summary"),
                summary_json: None,
            },
        )
        .await;

        // Second advance with None refs — must NOT overwrite.
        advance_to_submitted(
            &ctx,
            SubmitAdvancementParams {
                task_id: &task.id,
                role: "worker",
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: None,
                summary_json: None,
            },
        )
        .await;

        let repo = TaskAttemptRepository::new(db);
        let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
        assert_eq!(attempt.outcome, "submitted");
        assert_eq!(attempt.submit_ref.as_deref(), Some("original"));
        assert_eq!(attempt.checkpoint_ref.as_deref(), Some("cp-1"));
        assert_eq!(attempt.mirror_head_sha.as_deref(), Some("mirror-sha"));
        assert_eq!(attempt.summary.as_deref(), Some("original summary"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slot_advance_does_not_move_terminal_backward() {
        let (db, ctx) = create_slot_ctx();
        let task = create_task(&db).await;
        let attempt_id = seed_pending(&db, &task.id, "worker", "dk-slot-term").await;

        let repo = TaskAttemptRepository::new(db.clone());
        // Submit then terminalize.
        repo.advance_to_submitted(djinn_db::SubmitTaskAttemptParams {
            id: &attempt_id,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: None,
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();
        repo.advance_to_terminal(djinn_db::TerminalTaskAttemptParams {
            id: &attempt_id,
            outcome: djinn_core::models::task_attempt::TaskAttemptOutcome::Completed,
            pr_url: None,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("terminal summary"),
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();

        // Late submit — must not move backward.
        advance_to_submitted(
            &ctx,
            SubmitAdvancementParams {
                task_id: &task.id,
                role: "worker",
                submit_ref: Some("late-ref"),
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: Some("late summary"),
                summary_json: None,
            },
        )
        .await;

        let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
        assert_eq!(attempt.outcome, "completed");
        assert_eq!(attempt.summary.as_deref(), Some("terminal summary"));
    }
}
