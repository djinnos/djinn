//! Attempt lifecycle instrumentation helpers for the dispatch start and
//! submission advancement paths.
//!
//! These helpers are best-effort: write failures are logged but never fail the
//! user-facing dispatch or submit path. They consume the foundation APIs
//! delivered by epic `u74z`.

use djinn_core::models::task_attempt::TaskAttemptOutcome;
use djinn_db::{
    CreateTaskAttemptParams, ReworkMarkerTaskAttemptParams, SubmitTaskAttemptParams,
    TaskAttemptRepository, TerminalTaskAttemptParams,
};

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
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
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

/// Parameters for the coordinator-side terminal advancement helper.
#[allow(dead_code)]
pub struct TerminalAdvancementParams<'a> {
    pub task_id: &'a str,
    pub role: &'a str,
    pub outcome: TaskAttemptOutcome,
    pub pr_url: Option<&'a str>,
    pub submit_ref: Option<&'a str>,
    pub checkpoint_ref: Option<&'a str>,
    pub mirror_head_sha: Option<&'a str>,
    pub github_head_sha: Option<&'a str>,
    pub summary: Option<&'a str>,
    pub summary_json: Option<&'a str>,
    pub log_tail: Option<&'a str>,
}

/// Advance the latest pending/submitted attempt for a task+role to a terminal
/// outcome, filling available refs, summary fields, and log tail.
///
/// Looks up the matching attempt via `latest_pending_or_submitted`; if no live
/// attempt exists (e.g. the dispatch-start write failed, or the attempt is
/// already terminal), this is a no-op.
///
/// Best-effort: lookup/write errors are logged and never propagated.
#[allow(dead_code)]
pub async fn advance_latest_to_terminal(
    db: &djinn_db::Database,
    params: TerminalAdvancementParams<'_>,
) {
    let repo = TaskAttemptRepository::new(db.clone());
    let requested_outcome = params.outcome;
    let attempt = match repo
        .latest_pending_or_submitted(params.task_id, Some(params.role))
        .await
    {
        Ok(Some(a)) => a,
        Ok(None) => {
            tracing::debug!(
                task_id = %params.task_id,
                role = %params.role,
                requested_outcome = %requested_outcome,
                "attempt_lifecycle: no pending/submitted attempt found for terminal advancement; skipping"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                task_id = %params.task_id,
                role = %params.role,
                requested_outcome = %requested_outcome,
                error = %e,
                "attempt_lifecycle: failed to look up attempt for terminal advancement"
            );
            return;
        }
    };

    let current_outcome = attempt.outcome.clone();
    match repo
        .advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: params.outcome,
            pr_url: params.pr_url,
            submit_ref: params.submit_ref,
            checkpoint_ref: params.checkpoint_ref,
            mirror_head_sha: params.mirror_head_sha,
            github_head_sha: params.github_head_sha,
            summary: params.summary,
            summary_json: params.summary_json,
            log_tail: params.log_tail,
        })
        .await
    {
        Ok(updated) => {
            tracing::info!(
                task_id = %params.task_id,
                role = %params.role,
                attempt_id = %attempt.id,
                dispatch_key = %attempt.dispatch_key,
                requested_outcome = %requested_outcome,
                current_outcome = %current_outcome,
                outcome = %updated.outcome,
                "attempt_lifecycle: terminal advancement recorded"
            );
        }
        Err(e) => {
            tracing::warn!(
                task_id = %params.task_id,
                role = %params.role,
                attempt_id = %attempt.id,
                dispatch_key = %attempt.dispatch_key,
                requested_outcome = %requested_outcome,
                current_outcome = %current_outcome,
                error = %e,
                "attempt_lifecycle: failed to advance attempt to terminal (best-effort)"
            );
        }
    }
}

/// Deterministic dispatch key for a rework-marker attempt row.
///
/// Mirrors the `open_pr_adoption` key convention so repeated marker inserts for
/// the same (task, role) resolve to one row via `ON CONFLICT DO NOTHING`.
pub fn rework_marker_dispatch_key(task_id: &str, role: &str) -> String {
    format!("{task_id}:{role}:rework_marker")
}

/// Guarantee a durable `reopened` signal for `(task, role)` after a rework
/// reopen, inserting an idempotent terminal `reopened` marker row when there is
/// nothing live to terminalize.
///
/// Ordering (all best-effort; errors are logged, never propagated):
/// 1. If an in-flight `pending`/`submitted` attempt still exists, the caller's
///    normal terminalization owns the `reopened` signal — no marker needed.
/// 2. Else if the latest non-guard attempt for the pair is already `reopened`
///    (an in-flight attempt was just terminalized, or a marker already exists
///    AND is still the newest attempt — rework is already pending), this is an
///    idempotent no-op.
/// 3. Else insert OR re-assert a synthetic terminal `reopened` marker row so the
///    respawn guard's latest-attempt gate treats the task as rework and
///    dispatches a worker instead of adopting the still-open PR.  This closes
///    the kv6i invisible-reopen gap (reopens that leave no live attempt behind)
///    AND the incident-gton merge-queue requeue loop: when a NEWER non-reopened
///    attempt has landed since the marker was first written (e.g. a rework
///    worker ran and `completed`, then the merge queue rejected the PR again),
///    the latest non-guard attempt is that non-reopened row, so this branch
///    fires and [`TaskAttemptRepository::insert_rework_marker`] REFRESHES the
///    fixed-key marker's `created_at` to make it the newest attempt again.
///    Without the refresh the stale marker stayed pinned behind the
///    `completed` row and the guard re-adopted the open PR forever.
pub async fn ensure_rework_marker(
    db: &djinn_db::Database,
    task_id: &str,
    role: &str,
    summary: Option<&str>,
) {
    let repo = TaskAttemptRepository::new(db.clone());

    // 1. A live attempt is present → its terminalization carries the signal.
    match repo.latest_pending_or_submitted(task_id, Some(role)).await {
        Ok(Some(_)) => return,
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                role = %role,
                error = %e,
                "attempt_lifecycle: rework-marker in-flight lookup failed; skipping marker"
            );
            return;
        }
    }

    // 2. Latest non-guard attempt already `reopened` → idempotent no-op.
    match repo.list_for_task(task_id).await {
        Ok(attempts) => {
            let latest_non_guard = attempts.iter().filter(|a| a.role == role).find(|a| {
                a.outcome != TaskAttemptOutcome::Deferred.as_str()
                    && a.outcome != TaskAttemptOutcome::AdoptedPr.as_str()
            });
            if latest_non_guard
                .is_some_and(|latest| latest.outcome == TaskAttemptOutcome::Reopened.as_str())
            {
                return;
            }
        }
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                role = %role,
                error = %e,
                "attempt_lifecycle: rework-marker latest-attempt lookup failed; skipping marker"
            );
            return;
        }
    }

    // 3. Insert the idempotent marker.
    let dispatch_key = rework_marker_dispatch_key(task_id, role);
    let id = uuid::Uuid::now_v7().to_string();
    let summary_json = r#"{"source":"rework_reopen_marker"}"#;
    match repo
        .insert_rework_marker(ReworkMarkerTaskAttemptParams {
            id: &id,
            task_id,
            role,
            dispatch_key: &dispatch_key,
            summary,
            summary_json: Some(summary_json),
        })
        .await
    {
        Ok(attempt) => {
            tracing::info!(
                task_id = %task_id,
                role = %role,
                attempt_id = %attempt.id,
                dispatch_key = %attempt.dispatch_key,
                "attempt_lifecycle: durable rework-reopen marker recorded (no in-flight attempt to terminalize)"
            );
        }
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                role = %role,
                error = %e,
                "attempt_lifecycle: failed to record rework-reopen marker (best-effort)"
            );
        }
    }
}

/// Record a rework reopen for `(task, role)`: terminalize any in-flight
/// `pending`/`submitted` attempt to `reopened`, and — when nothing was
/// in-flight — insert a durable `reopened` marker (see [`ensure_rework_marker`]).
///
/// This is the single entry point for rework transitions that the PR poller's
/// `apply_pr_transition` does NOT own (the supervisor-driven
/// `task_review_reject*` / `lead_approve_conflict` paths — the ylme bug, where
/// the worker's `submitted` attempt was never terminalized).  Best-effort.
pub async fn record_rework_reopen(
    db: &djinn_db::Database,
    task_id: &str,
    role: &str,
    pr_url: Option<&str>,
    summary: Option<&str>,
    summary_json: Option<&str>,
) {
    // Terminalize any in-flight pending/submitted attempt to `reopened` (no-op
    // if none is live).
    advance_latest_to_terminal(
        db,
        TerminalAdvancementParams {
            task_id,
            role,
            outcome: TaskAttemptOutcome::Reopened,
            pr_url,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary,
            summary_json,
            log_tail: None,
        },
    )
    .await;
    // Guarantee a durable `reopened` signal even when nothing was in-flight.
    ensure_rework_marker(db, task_id, role, summary).await;
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

    // ─── advance_latest_to_terminal tests ────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_advancement_records_all_terminal_fields() {
        let db = test_db();
        let task = create_task(&db).await;
        let dk = make_dispatch_key(&task.id, "worker");
        let attempt_id = record_dispatch_start(&db, &task.id, "worker", None, &dk)
            .await
            .unwrap();

        advance_latest_to_terminal(
            &db,
            TerminalAdvancementParams {
                task_id: &task.id,
                role: "worker",
                outcome: TaskAttemptOutcome::Completed,
                pr_url: Some("https://github.example/pr/1"),
                submit_ref: Some("refs/submitted/1"),
                checkpoint_ref: Some("refs/checkpoints/1"),
                mirror_head_sha: Some("mirror-sha-1"),
                github_head_sha: Some("github-sha-1"),
                summary: Some("completed summary"),
                summary_json: Some(r#"{"status": "completed"}"#),
                log_tail: Some("last log lines"),
            },
        )
        .await;

        let repo = TaskAttemptRepository::new(db);
        let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
        assert_eq!(attempt.outcome, "completed");
        assert!(attempt.terminal_at.is_some());
        assert_eq!(
            attempt.pr_url.as_deref(),
            Some("https://github.example/pr/1")
        );
        assert_eq!(attempt.submit_ref.as_deref(), Some("refs/submitted/1"));
        assert_eq!(
            attempt.checkpoint_ref.as_deref(),
            Some("refs/checkpoints/1")
        );
        assert_eq!(attempt.mirror_head_sha.as_deref(), Some("mirror-sha-1"));
        assert_eq!(attempt.github_head_sha.as_deref(), Some("github-sha-1"));
        assert_eq!(attempt.summary.as_deref(), Some("completed summary"));
        assert_eq!(
            attempt.summary_json.as_deref(),
            Some(r#"{"status": "completed"}"#)
        );
        assert_eq!(attempt.log_tail.as_deref(), Some("last log lines"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_advancement_is_idempotent_and_does_not_create_rows() {
        let db = test_db();
        let task = create_task(&db).await;
        let dk = make_dispatch_key(&task.id, "worker");
        let attempt_id = record_dispatch_start(&db, &task.id, "worker", None, &dk)
            .await
            .unwrap();

        for _ in 0..2 {
            advance_latest_to_terminal(
                &db,
                TerminalAdvancementParams {
                    task_id: &task.id,
                    role: "worker",
                    outcome: TaskAttemptOutcome::Completed,
                    pr_url: Some("https://github.example/pr/1"),
                    submit_ref: Some("submit-ref"),
                    checkpoint_ref: None,
                    mirror_head_sha: None,
                    github_head_sha: None,
                    summary: Some("done"),
                    summary_json: None,
                    log_tail: None,
                },
            )
            .await;
        }

        let repo = TaskAttemptRepository::new(db);
        let all = repo.list_for_task(&task.id).await.unwrap();
        assert_eq!(
            all.len(),
            1,
            "duplicate terminalization must not create rows"
        );
        let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
        assert_eq!(attempt.outcome, "completed");
        assert_eq!(
            attempt.pr_url.as_deref(),
            Some("https://github.example/pr/1")
        );
        assert_eq!(attempt.submit_ref.as_deref(), Some("submit-ref"));
        assert_eq!(attempt.summary.as_deref(), Some("done"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_advancement_does_not_move_completed_backward_or_overwrite_fields() {
        let db = test_db();
        let task = create_task(&db).await;
        let dk = make_dispatch_key(&task.id, "worker");
        let attempt_id = record_dispatch_start(&db, &task.id, "worker", None, &dk)
            .await
            .unwrap();

        advance_latest_to_terminal(
            &db,
            TerminalAdvancementParams {
                task_id: &task.id,
                role: "worker",
                outcome: TaskAttemptOutcome::Completed,
                pr_url: Some("https://github.example/pr/original"),
                submit_ref: Some("submit-original"),
                checkpoint_ref: Some("checkpoint-original"),
                mirror_head_sha: Some("mirror-original"),
                github_head_sha: Some("github-original"),
                summary: Some("original summary"),
                summary_json: Some(r#"{"status":"original"}"#),
                log_tail: Some("original logs"),
            },
        )
        .await;

        advance_latest_to_terminal(
            &db,
            TerminalAdvancementParams {
                task_id: &task.id,
                role: "worker",
                outcome: TaskAttemptOutcome::Reopened,
                pr_url: Some("https://github.example/pr/late"),
                submit_ref: Some("submit-late"),
                checkpoint_ref: Some("checkpoint-late"),
                mirror_head_sha: Some("mirror-late"),
                github_head_sha: Some("github-late"),
                summary: Some("late summary"),
                summary_json: Some(r#"{"status":"late"}"#),
                log_tail: Some("late logs"),
            },
        )
        .await;

        let repo = TaskAttemptRepository::new(db);
        let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
        assert_eq!(attempt.outcome, "completed");
        assert_eq!(
            attempt.pr_url.as_deref(),
            Some("https://github.example/pr/original")
        );
        assert_eq!(attempt.submit_ref.as_deref(), Some("submit-original"));
        assert_eq!(
            attempt.checkpoint_ref.as_deref(),
            Some("checkpoint-original")
        );
        assert_eq!(attempt.mirror_head_sha.as_deref(), Some("mirror-original"));
        assert_eq!(attempt.github_head_sha.as_deref(), Some("github-original"));
        assert_eq!(attempt.summary.as_deref(), Some("original summary"));
        assert_eq!(attempt.log_tail.as_deref(), Some("original logs"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_advancement_no_current_attempt_is_noop() {
        let db = test_db();
        let task = create_task(&db).await;

        advance_latest_to_terminal(
            &db,
            TerminalAdvancementParams {
                task_id: &task.id,
                role: "worker",
                outcome: TaskAttemptOutcome::Completed,
                pr_url: Some("https://github.example/pr/missing"),
                submit_ref: Some("missing-submit"),
                checkpoint_ref: Some("missing-checkpoint"),
                mirror_head_sha: Some("missing-mirror"),
                github_head_sha: Some("missing-github"),
                summary: Some("missing summary"),
                summary_json: Some(r#"{"status":"missing"}"#),
                log_tail: Some("missing logs"),
            },
        )
        .await;

        let repo = TaskAttemptRepository::new(db);
        let all = repo.list_for_task(&task.id).await.unwrap();
        assert!(all.is_empty(), "no-op must not create an attempt row");
    }
}
