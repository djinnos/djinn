//! Attempt lifecycle instrumentation helpers for the dispatch start and
//! submission advancement paths.
//!
//! These helpers are best-effort: write failures are logged but never fail the
//! user-facing dispatch or submit path. They consume the foundation APIs
//! delivered by epic `u74z`.

use djinn_db::{CreateTaskAttemptParams, SubmitTaskAttemptParams, TaskAttemptRepository};

/// Record the start of a dispatch attempt.
///
/// Creates or idempotently returns the pending `task_attempts` row for the
/// given `dispatch_key`. Returns the attempt id on success.
///
/// `session_id` is passed when the session identity is known at dispatch time
/// (e.g. a resume or continuation); `None` when the session has not been
/// created yet (the common case for fresh dispatches).
///
/// Best-effort: errors are logged and return `None` rather than propagating.
pub async fn record_dispatch_start(
    db: &djinn_db::Database,
    task_id: &str,
    role: &str,
    session_id: Option<&str>,
    dispatch_key: &str,
) -> Option<String> {
    let attempt_id = uuid::Uuid::now_v7().to_string();
    let repo = TaskAttemptRepository::new(db.clone());
    match repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &attempt_id,
            task_id,
            role,
            dispatch_key,
            session_id,
            attempt_seq: None,
        })
        .await
    {
        Ok(attempt) => {
            tracing::info!(
                task_id = %task_id,
                role,
                dispatch_key = %dispatch_key,
                attempt_id = %attempt.id,
                attempt_seq = attempt.attempt_seq,
                outcome = %attempt.outcome,
                "attempt_lifecycle: dispatch-start recorded"
            );
            Some(attempt.id)
        }
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                role,
                dispatch_key = %dispatch_key,
                error = %e,
                "attempt_lifecycle: failed to record dispatch-start (best-effort)"
            );
            None
        }
    }
}

/// Parameters for the coordinator-side submission advancement helper.
#[allow(dead_code)]
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
#[allow(dead_code)]
pub async fn advance_to_submitted(db: &djinn_db::Database, params: SubmitAdvancementParams<'_>) {
    let repo = TaskAttemptRepository::new(db.clone());
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

    // Already submitted or terminal — idempotent no-op, but fill any newly
    // available refs that were previously NULL.
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

/// Generate a stable dispatch key for a new dispatch event.
///
/// The key encodes `task_id`, `role`, and a time-sorted UUID so that:
/// - The same dispatch event always produces the same key (stable).
/// - Different dispatch events for the same task+role produce different keys
///   (unique).
/// - The key fits within `TASK_ATTEMPT_DISPATCH_KEY_MAX_LEN` (255).
pub fn make_dispatch_key(task_id: &str, role: &str) -> String {
    let uuid = uuid::Uuid::now_v7().to_string();
    let key = format!("{task_id}:{role}:{uuid}");
    // Truncate to the column bound (defensive; a UUID + short ids should never
    // approach 255).
    if key.len() > djinn_core::models::task_attempt::TASK_ATTEMPT_DISPATCH_KEY_MAX_LEN {
        key[..djinn_core::models::task_attempt::TASK_ATTEMPT_DISPATCH_KEY_MAX_LEN].to_string()
    } else {
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::events::EventBus;
    use djinn_db::{Database, EpicRepository, TaskAttemptRepository, TaskRepository};

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    /// Create a minimal task row for FK satisfaction.
    async fn create_task(db: &Database) -> djinn_core::models::Task {
        let event_bus = EventBus::noop();
        let epic_repo = EpicRepository::new(db.clone(), event_bus.clone());
        let epic = epic_repo
            .create("Epic", "", "", "", "", None)
            .await
            .unwrap();
        let task_repo = TaskRepository::new(db.clone(), event_bus);
        task_repo
            .create(&epic.id, "Test task", "", "", "task", 0, "", None)
            .await
            .unwrap()
    }

    // ─── make_dispatch_key tests ────────────────────────────────────────

    #[test]
    fn make_dispatch_key_is_unique_per_call() {
        let k1 = make_dispatch_key("task-1", "worker");
        let k2 = make_dispatch_key("task-1", "worker");
        assert_ne!(k1, k2, "dispatch keys must be unique per call");
    }

    #[test]
    fn make_dispatch_key_contains_task_and_role() {
        let key = make_dispatch_key("task-42", "reviewer");
        assert!(key.starts_with("task-42:reviewer:"), "key = {key}");
    }

    #[test]
    fn make_dispatch_key_fits_column_bound() {
        let key = make_dispatch_key(&"x".repeat(200), &"y".repeat(50));
        assert!(key.len() <= djinn_core::models::task_attempt::TASK_ATTEMPT_DISPATCH_KEY_MAX_LEN);
    }

    // ─── record_dispatch_start tests ────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_start_creates_pending_row() {
        let db = test_db();
        let task = create_task(&db).await;
        let dk = make_dispatch_key(&task.id, "worker");

        let attempt_id = record_dispatch_start(&db, &task.id, "worker", None, &dk)
            .await
            .expect("should return attempt id");

        let repo = TaskAttemptRepository::new(db);
        let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
        assert_eq!(attempt.task_id, task.id);
        assert_eq!(attempt.role, "worker");
        assert_eq!(attempt.outcome, "pending");
        assert_eq!(attempt.dispatch_key, dk);
        assert!(attempt.session_id.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_start_records_role_and_none_session_by_default() {
        let db = test_db();
        let task = create_task(&db).await;
        let dk = make_dispatch_key(&task.id, "planner");

        let attempt_id = record_dispatch_start(&db, &task.id, "planner", None, &dk)
            .await
            .unwrap();

        let repo = TaskAttemptRepository::new(db);
        let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
        // session_id is None because the session doesn't exist yet at dispatch
        // time (FK constraint prevents arbitrary IDs).
        assert!(attempt.session_id.is_none());
        assert_eq!(attempt.role, "planner");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_start_is_idempotent_on_same_key() {
        let db = test_db();
        let task = create_task(&db).await;
        let dk = make_dispatch_key(&task.id, "worker");

        let id1 = record_dispatch_start(&db, &task.id, "worker", None, &dk)
            .await
            .unwrap();
        let id2 = record_dispatch_start(&db, &task.id, "worker", None, &dk)
            .await
            .unwrap();

        assert_eq!(id1, id2, "same dispatch key must return same attempt id");
        let repo = TaskAttemptRepository::new(db);
        let all = repo.list_for_task(&task.id).await.unwrap();
        assert_eq!(all.len(), 1, "must not create duplicate rows");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn different_roles_create_separate_attempts() {
        let db = test_db();
        let task = create_task(&db).await;

        let dk_w = make_dispatch_key(&task.id, "worker");
        let dk_r = make_dispatch_key(&task.id, "reviewer");
        let id_w = record_dispatch_start(&db, &task.id, "worker", None, &dk_w)
            .await
            .unwrap();
        let id_r = record_dispatch_start(&db, &task.id, "reviewer", None, &dk_r)
            .await
            .unwrap();

        assert_ne!(id_w, id_r);
        let repo = TaskAttemptRepository::new(db);
        let all = repo.list_for_task(&task.id).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    // ─── advance_to_submitted tests ─────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_advancement_moves_pending_to_submitted() {
        let db = test_db();
        let task = create_task(&db).await;
        let dk = make_dispatch_key(&task.id, "worker");
        let attempt_id = record_dispatch_start(&db, &task.id, "worker", None, &dk)
            .await
            .unwrap();

        advance_to_submitted(
            &db,
            SubmitAdvancementParams {
                task_id: &task.id,
                role: "worker",
                submit_ref: Some("submit-ref-1"),
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: Some("did the work"),
                summary_json: None,
            },
        )
        .await;

        let repo = TaskAttemptRepository::new(db);
        let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
        assert_eq!(attempt.outcome, "submitted");
        assert!(attempt.submitted_at.is_some());
        assert_eq!(attempt.submit_ref.as_deref(), Some("submit-ref-1"));
        assert_eq!(attempt.summary.as_deref(), Some("did the work"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_advancement_is_idempotent_and_does_not_overwrite_refs() {
        let db = test_db();
        let task = create_task(&db).await;
        let dk = make_dispatch_key(&task.id, "worker");
        let attempt_id = record_dispatch_start(&db, &task.id, "worker", None, &dk)
            .await
            .unwrap();

        // First submit with refs.
        advance_to_submitted(
            &db,
            SubmitAdvancementParams {
                task_id: &task.id,
                role: "worker",
                submit_ref: Some("original-ref"),
                checkpoint_ref: Some("cp-1"),
                mirror_head_sha: Some("mirror-sha"),
                github_head_sha: None,
                summary: Some("original summary"),
                summary_json: None,
            },
        )
        .await;

        // Second submit with null refs — must NOT overwrite existing.
        advance_to_submitted(
            &db,
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
        // Original refs must be preserved.
        assert_eq!(attempt.submit_ref.as_deref(), Some("original-ref"));
        assert_eq!(attempt.checkpoint_ref.as_deref(), Some("cp-1"));
        assert_eq!(attempt.mirror_head_sha.as_deref(), Some("mirror-sha"));
        assert_eq!(attempt.summary.as_deref(), Some("original summary"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_row_not_moved_backward_by_late_submit() {
        use djinn_core::models::task_attempt::TaskAttemptOutcome;
        use djinn_db::TerminalTaskAttemptParams;

        let db = test_db();
        let task = create_task(&db).await;
        let dk = make_dispatch_key(&task.id, "worker");
        let attempt_id = record_dispatch_start(&db, &task.id, "worker", None, &dk)
            .await
            .unwrap();

        // Advance to submitted first.
        let repo = TaskAttemptRepository::new(db.clone());
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

        // Terminalize to completed.
        repo.advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt_id,
            outcome: TaskAttemptOutcome::Completed,
            pr_url: Some("http://pr"),
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("done"),
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();

        // Late submit attempt — must not move backward.
        advance_to_submitted(
            &db,
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
        assert_eq!(attempt.outcome, "completed", "must remain terminal");
        assert_eq!(
            attempt.summary.as_deref(),
            Some("done"),
            "must not overwrite terminal summary"
        );
    }
}
