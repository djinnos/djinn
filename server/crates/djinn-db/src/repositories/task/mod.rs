// djinn:allow-oversize
use sqlx::PgPool;

use crate::database::Database;
use crate::{Error, Result};
use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::{
    ActivityEntry, IssueType, ReopenClass, ReopenLedgerEntry, Task, TaskStatus, TransitionAction,
    compute_transition_for_issue_type,
};

mod activity;
mod blockers;
mod board_health;
mod ci;
mod parent_disposition;
mod queries;
mod reads;
mod status;
mod writes;

// Re-export parent-disposition types so `EpicRepository::close` can
// construct the scope and inspect the classification plan.
pub use parent_disposition::{
    ChildDisposition, DispositionCounts, DispositionFinding, DispositionPlan, DispositionScope,
    classify_child_tx,
};

// ── Query / result types ──────────────────────────────────────────────────────

/// Filters and pagination for [`TaskRepository::list_filtered`].
pub struct ListQuery {
    pub project_id: Option<String>,
    pub status: Option<String>,
    pub issue_type: Option<String>,
    pub priority: Option<i64>,
    pub label: Option<String>,
    pub text: Option<String>,
    /// Filter by epic_id (already resolved to a UUID).
    pub parent: Option<String>,
    /// "priority" | "created" | "created_desc" | "updated" | "updated_desc" | "closed"
    pub sort: String,
    pub limit: i64,
    pub offset: i64,
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_core::models::{
        CiStatus, Project, Task, TaskPrCiSnapshotInput, TaskStatus, TransitionAction,
    };

    use crate::database::Database;
    use crate::repositories::task_attempt::{
        CreateTaskAttemptParams, FillTaskAttemptParams, TaskAttemptRepository,
    };

    use super::*;

    fn capturing_bus() -> (EventBus, Arc<Mutex<Vec<DjinnEventEnvelope>>>) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let bus = EventBus::new({
            let captured = captured.clone();
            move |ev| captured.lock().unwrap().push(ev)
        });
        (bus, captured)
    }

    async fn make_project(db: &Database) -> Project {
        db.ensure_initialized().await.unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
            id,
            "task-project",
            "test",
            "task-project",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query_as!(
            Project,
            r#"SELECT id, name,
                  github_owner AS "github_owner!: String",
                  github_repo AS "github_repo!: String",
                  created_at, target_branch,
                  auto_merge AS "auto_merge!: bool",
                  sync_enabled AS "sync_enabled!: bool",
                  sync_remote
           FROM projects WHERE id = $1"#,
            id
        )
        .fetch_one(db.pool())
        .await
        .unwrap()
    }

    async fn make_epic(db: &Database, project_id: &str) -> String {
        let epic_id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, owner, memory_refs)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, '[]'::jsonb)",
            epic_id,
            project_id,
            "ep01",
            "Epic",
            "",
            "",
            "",
            ""
        )
        .execute(db.pool())
        .await
        .unwrap();
        epic_id
    }

    async fn make_task(
        repo: &TaskRepository,
        epic_id: &str,
        issue_type: &str,
        acceptance_criteria: Option<&str>,
    ) -> Task {
        repo.create_with_ac(
            epic_id,
            "Task title",
            "desc",
            "design",
            issue_type,
            1,
            "worker",
            None,
            acceptance_criteria,
        )
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transition_persists_valid_full_lifecycle_and_activity() {
        let db = Database::open_in_memory().unwrap();
        let (bus, captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac1"}]"#)).await;

        let in_progress = repo
            .transition(
                &task.id,
                TransitionAction::Start,
                "worker-1",
                "worker",
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(in_progress.status, TaskStatus::InProgress.as_str());

        let needs_review = repo
            .transition(
                &task.id,
                TransitionAction::SubmitTaskReview,
                "worker-1",
                "worker",
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(needs_review.status, TaskStatus::NeedsTaskReview.as_str());

        let in_review = repo
            .transition(
                &task.id,
                TransitionAction::TaskReviewStart,
                "reviewer-1",
                "reviewer",
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(in_review.status, TaskStatus::InTaskReview.as_str());

        let approved = repo
            .transition(
                &task.id,
                TransitionAction::TaskReviewApprove,
                "reviewer-1",
                "reviewer",
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(approved.status, TaskStatus::Approved.as_str());

        let persisted = task_select_where_id!(&task.id)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(persisted.status, TaskStatus::Approved.as_str());
        assert_eq!(persisted.reopen_count, 0);
        assert_eq!(persisted.continuation_count, 0);
        assert!(persisted.closed_at.is_none());

        let activity = repo.list_activity(&task.id).await.unwrap();
        assert_eq!(activity.len(), 4);
        let last_payload: serde_json::Value =
            serde_json::from_str(&activity.last().unwrap().payload).unwrap();
        assert_eq!(last_payload["from_status"], "in_task_review");
        assert_eq!(last_payload["to_status"], "approved");
        assert!(activity.iter().any(|entry| {
            serde_json::from_str::<serde_json::Value>(&entry.payload)
                .unwrap()
                .get("ac_snapshot")
                .is_some()
        }));

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 5);
        assert_eq!(events.last().unwrap().entity_type, "task");
        assert_eq!(events.last().unwrap().action, "updated");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transition_rejects_invalid_start_without_acceptance_criteria_and_keeps_state() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some("[]")).await;

        let err = repo
            .transition(
                &task.id,
                TransitionAction::Start,
                "worker-1",
                "worker",
                None,
                None,
            )
            .await
            .unwrap_err();

        assert!(matches!(err, Error::InvalidTransition(_)));
        assert!(err.to_string().contains("acceptance criteria"));

        let persisted = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(persisted.status, TaskStatus::Open.as_str());
        assert!(repo.list_activity(&task.id).await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transition_rejects_invalid_repository_state_transition_and_does_not_persist_changes() {
        let db = Database::open_in_memory().unwrap();
        let (bus, captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac1"}]"#)).await;

        let original = task_select_where_id!(&task.id)
            .fetch_one(db.pool())
            .await
            .unwrap();

        let err = repo
            .transition(
                &task.id,
                TransitionAction::TaskReviewStart,
                "reviewer-1",
                "reviewer",
                None,
                None,
            )
            .await
            .unwrap_err();

        assert!(matches!(err, Error::InvalidTransition(_)));

        let persisted = task_select_where_id!(&task.id)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(persisted.status, original.status);
        assert_eq!(persisted.reopen_count, original.reopen_count);
        assert_eq!(persisted.continuation_count, original.continuation_count);
        assert_eq!(persisted.total_reopen_count, original.total_reopen_count);
        assert_eq!(persisted.closed_at, original.closed_at);
        assert!(repo.list_activity(&task.id).await.unwrap().is_empty());
        assert_eq!(captured.lock().unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transition_persists_invalid_outcome_side_effects_for_rejection_and_reason_validation()
    {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac1"}]"#)).await;

        repo.transition(
            &task.id,
            TransitionAction::Start,
            "worker",
            "worker",
            None,
            None,
        )
        .await
        .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::SubmitTaskReview,
            "worker",
            "worker",
            None,
            None,
        )
        .await
        .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::TaskReviewStart,
            "reviewer",
            "reviewer",
            None,
            None,
        )
        .await
        .unwrap();

        let missing_reason = repo
            .transition(
                &task.id,
                TransitionAction::TaskReviewRejectStale,
                "reviewer",
                "reviewer",
                None,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(missing_reason, Error::InvalidTransition(_)));
        assert!(
            missing_reason
                .to_string()
                .contains("requires a non-empty reason")
        );

        let reopened = repo
            .transition(
                &task.id,
                TransitionAction::TaskReviewRejectStale,
                "reviewer",
                "reviewer",
                Some("stale implementation"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(reopened.status, TaskStatus::Open.as_str());
        assert_eq!(reopened.reopen_count, 1);
        assert_eq!(reopened.continuation_count, 1);

        let payload: serde_json::Value = serde_json::from_str(
            &repo
                .list_activity(&task.id)
                .await
                .unwrap()
                .last()
                .unwrap()
                .payload,
        )
        .unwrap();
        assert_eq!(payload["reason"], "stale implementation");
        assert_eq!(payload["to_status"], "open");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transition_force_close_succeeds_with_downstream_blockers() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;

        // Create two tasks: task_a blocks task_b.
        let task_a = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac1"}]"#)).await;
        let task_b = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac2"}]"#)).await;

        // task_b is blocked by task_a  (i.e. task_a blocks task_b).
        repo.add_blocker(&task_b.id, &task_a.id).await.unwrap();

        // Move task_a to in_lead_intervention so ForceClose is reachable.
        repo.set_status(&task_a.id, "in_lead_intervention")
            .await
            .unwrap();

        // ForceClose should succeed even though task_a still blocks task_b.
        let closed = repo
            .transition(
                &task_a.id,
                TransitionAction::ForceClose,
                "lead-1",
                "lead",
                Some("decomposed into subtasks"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(closed.status, TaskStatus::Closed.as_str());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transition_close_rejects_with_downstream_blockers() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;

        let task_a = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac1"}]"#)).await;
        let task_b = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac2"}]"#)).await;

        // task_b is blocked by task_a.
        repo.add_blocker(&task_b.id, &task_a.id).await.unwrap();

        // Move task_a to in_progress so Close is valid.
        repo.transition(
            &task_a.id,
            TransitionAction::Start,
            "worker",
            "worker",
            None,
            None,
        )
        .await
        .unwrap();

        // Normal Close should be rejected because task_a blocks task_b.
        let err = repo
            .transition(
                &task_a.id,
                TransitionAction::Close,
                "worker",
                "worker",
                None,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidTransition(_)));
        assert!(err.to_string().contains("blocks"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transition_allows_simple_lifecycle_start_without_acceptance_criteria() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db, bus);
        let project = make_project(&repo.db).await;
        let epic_id = make_epic(&repo.db, &project.id).await;
        let task = make_task(&repo, &epic_id, "research", Some("[]")).await;

        let started = repo
            .transition(
                &task.id,
                TransitionAction::Start,
                "worker",
                "worker",
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(started.status, TaskStatus::InProgress.as_str());
    }

    // ── Reactive conflict auto-blocker (feat/reactive-conflict-blocker) ────────
    //
    // These cover the DB invariants the pr_poller conflict auto-blocker relies
    // on: idempotent add, cycle rejection (caught + skipped, no edge), and the
    // readiness gate holding a freshly-conflict-blocked task until its blocker
    // reaches `closed`.

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn conflict_blocker_auto_add_is_idempotent() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;

        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac1"}]"#)).await;
        let sibling = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac2"}]"#)).await;

        // Add the same conflict-blocker edge twice (mirrors two poller ticks
        // both seeing the same unresolved conflict). The second must be a no-op,
        // not an error, and must not duplicate the edge.
        repo.update_blockers_atomic(&task.id, std::slice::from_ref(&sibling.id), &[])
            .await
            .unwrap();
        repo.update_blockers_atomic(&task.id, std::slice::from_ref(&sibling.id), &[])
            .await
            .unwrap();

        let blockers = repo.list_blockers(&task.id).await.unwrap();
        assert_eq!(
            blockers.len(),
            1,
            "duplicate auto-add must be idempotent (exactly one blocker edge)"
        );
        assert_eq!(blockers[0].task_id, sibling.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn conflict_blocker_cycle_is_skipped() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;

        let task_a = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac1"}]"#)).await;
        let task_b = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac2"}]"#)).await;

        // task_b is already blocked by task_a.
        repo.update_blockers_atomic(&task_b.id, std::slice::from_ref(&task_a.id), &[])
            .await
            .unwrap();

        // Now try to also block task_a on task_b — that would close a cycle.
        // The pr_poller catches this and skips; here we assert the repo rejects
        // it (so the caller's catch-and-skip path is exercised) and that NO edge
        // was added.
        let err = repo
            .update_blockers_atomic(&task_a.id, std::slice::from_ref(&task_b.id), &[])
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("circular"),
            "cycle add must be rejected as circular, got: {err}"
        );

        let a_blockers = repo.list_blockers(&task_a.id).await.unwrap();
        assert!(
            a_blockers.is_empty(),
            "a would-be cycle must add no edge (graceful skip)"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn conflict_blocker_holds_task_out_of_ready_until_blocker_closed() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;

        // `task` is the conflicting one (reopened → open). `sibling` is the
        // racing peer it now waits on.
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac1"}]"#)).await;
        let sibling = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac2"}]"#)).await;

        // Both start `open` → both ready.
        let ready = repo
            .list_ready(ReadyQuery {
                project_id: Some(project.id.clone()),
                limit: 50,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            ready.iter().any(|t| t.id == task.id),
            "task is ready before the conflict-blocker is added"
        );

        // Add the conflict-blocker edge (task waits on the unmerged sibling).
        repo.update_blockers_atomic(&task.id, std::slice::from_ref(&sibling.id), &[])
            .await
            .unwrap();

        let ready = repo
            .list_ready(ReadyQuery {
                project_id: Some(project.id.clone()),
                limit: 50,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            !ready.iter().any(|t| t.id == task.id),
            "a task with a fresh conflict-blocker must drop out of list_ready"
        );

        // Once the sibling reaches `closed` (merged), the gate releases.
        repo.set_status(&sibling.id, "closed").await.unwrap();
        let ready = repo
            .list_ready(ReadyQuery {
                project_id: Some(project.id.clone()),
                limit: 50,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            ready.iter().any(|t| t.id == task.id),
            "task becomes ready again once its conflict-blocker is closed"
        );
    }

    /// Regression: a task blocked by an open `review`-type hold (the
    /// human-review-hold shape observed in pdn6) must be excluded from ALL
    /// dispatch-readiness paths — `list_ready`, `claim`, and
    /// `list_by_status_filtered(…, true)` — until the hold is closed.
    ///
    /// The blocker predicate must treat every unresolved blocker equally,
    /// regardless of `issue_type`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn review_human_hold_blocks_source_from_all_dispatch_paths() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;

        // Source task — a normal work item.
        let source = make_task(&repo, &epic_id, "task", None).await;

        // Human-review hold task: issue_type=review, owner=system,
        // label=human-review-hold (matches the auto-park shape from pdn6).
        let hold = repo
            .create_in_project(
                &project.id,
                Some(&epic_id),
                "Human review hold",
                "",
                "",
                "review",
                0,
                "system",
                None,
                None,
            )
            .await
            .unwrap();
        // Stamp the label via raw update so the hold carries the exact
        // human-review-hold marker the auto-park mechanism uses.
        sqlx::query("UPDATE tasks SET labels = $1::jsonb WHERE id = $2")
            .bind(r#"["human-review-hold"]"#)
            .bind(&hold.id)
            .execute(db.pool())
            .await
            .unwrap();

        // ── Pre-condition: source is ready before the blocker is wired ──
        let ready = repo
            .list_ready(ReadyQuery {
                project_id: Some(project.id.clone()),
                limit: 50,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            ready.iter().any(|t| t.id == source.id),
            "source is ready before the review hold blocker is added"
        );

        // Wire the blocker edge: source is blocked by the hold task.
        repo.add_blocker(&source.id, &hold.id).await.unwrap();

        // ── list_ready must exclude the blocked source ──
        let ready = repo
            .list_ready(ReadyQuery {
                project_id: Some(project.id.clone()),
                limit: 50,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            !ready.iter().any(|t| t.id == source.id),
            "list_ready must exclude source while review hold is open"
        );

        // ── claim must not be able to claim the blocked source ──
        let claimed = repo
            .claim(
                ReadyQuery {
                    project_id: Some(project.id.clone()),
                    limit: 50,
                    ..Default::default()
                },
                "coordinator",
                "system",
            )
            .await
            .unwrap();
        assert!(
            claimed.is_none() || claimed.as_ref().unwrap().id != source.id,
            "claim must not return the blocked source while review hold is open"
        );

        // ── list_by_status_filtered("open", true) dispatch path ──
        let filtered = repo.list_by_status_filtered("open", true).await.unwrap();
        assert!(
            !filtered.iter().any(|t| t.id == source.id),
            "list_by_status_filtered(open, true) must exclude source while review hold is open"
        );

        // ── Closing the hold releases the source back to readiness ──
        repo.set_status(&hold.id, "closed").await.unwrap();

        let ready = repo
            .list_ready(ReadyQuery {
                project_id: Some(project.id.clone()),
                limit: 50,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            ready.iter().any(|t| t.id == source.id),
            "source must be ready again after the review hold is closed"
        );

        let filtered = repo.list_by_status_filtered("open", true).await.unwrap();
        assert!(
            filtered.iter().any(|t| t.id == source.id),
            "list_by_status_filtered(open, true) must include source after review hold is closed"
        );
    }

    /// Regression (pdn6 release-side): prove that a review hold and a normal
    /// (non-review) blocker have **identical** release semantics — the dispatch
    /// readiness gate must not special-case either `issue_type`.
    ///
    /// The test wires BOTH a review hold blocker AND a normal task blocker onto
    /// the same source, then releases them in sequence to prove:
    /// 1. With both blockers open the source is NOT ready.
    /// 2. Closing only the review hold does NOT release the source (normal blocker remains).
    /// 3. Closing only the normal blocker DOES release the source (all blockers resolved).
    /// 4. The order of release doesn't matter — all blockers must be closed.
    ///
    /// This guards against any predicate change that might accidentally
    /// special-case only `review` blockers or alter release behavior for
    /// ordinary task blockers.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn review_hold_and_normal_blocker_have_identical_release_semantics() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;

        // Source task — a normal work item.
        let source = make_task(&repo, &epic_id, "task", None).await;

        // Human-review hold task (review type, matching the auto-park shape from pdn6).
        let review_hold = repo
            .create_in_project(
                &project.id,
                Some(&epic_id),
                "Human review hold",
                "",
                "",
                "review",
                0,
                "system",
                None,
                None,
            )
            .await
            .unwrap();

        // Normal blocker task (a regular task that blocks the source).
        let normal_blocker = make_task(&repo, &epic_id, "task", None).await;

        // Wire both blocker edges.
        repo.add_blocker(&source.id, &review_hold.id).await.unwrap();
        repo.add_blocker(&source.id, &normal_blocker.id)
            .await
            .unwrap();

        // ── With both blockers open, source is NOT ready ──
        let ready = repo
            .list_ready(ReadyQuery {
                project_id: Some(project.id.clone()),
                limit: 50,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            !ready.iter().any(|t| t.id == source.id),
            "source must NOT be ready with both review hold and normal blocker open"
        );

        // ── Close only the review hold — source must still be NOT ready ──
        repo.set_status(&review_hold.id, "closed").await.unwrap();

        let ready = repo
            .list_ready(ReadyQuery {
                project_id: Some(project.id.clone()),
                limit: 50,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            !ready.iter().any(|t| t.id == source.id),
            "source must still NOT be ready after closing review hold while normal blocker remains open"
        );

        // ── Close the normal blocker — now all blockers are resolved, source IS ready ──
        repo.set_status(&normal_blocker.id, "closed").await.unwrap();

        let ready = repo
            .list_ready(ReadyQuery {
                project_id: Some(project.id.clone()),
                limit: 50,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            ready.iter().any(|t| t.id == source.id),
            "source must be ready after ALL blockers (both review hold and normal) are closed"
        );
    }

    /// Companion to the mixed-blocker test above, but releases in reverse order
    /// (normal blocker first, then review hold). Proves the gate is symmetric —
    /// the dispatch readiness predicate does not care about blocker issue_type
    /// or release ordering.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn normal_blocker_release_then_review_hold_release_symmetric() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;

        let source = make_task(&repo, &epic_id, "task", None).await;

        let review_hold = repo
            .create_in_project(
                &project.id,
                Some(&epic_id),
                "Human review hold",
                "",
                "",
                "review",
                0,
                "system",
                None,
                None,
            )
            .await
            .unwrap();
        let normal_blocker = make_task(&repo, &epic_id, "task", None).await;

        repo.add_blocker(&source.id, &review_hold.id).await.unwrap();
        repo.add_blocker(&source.id, &normal_blocker.id)
            .await
            .unwrap();

        // Source is NOT ready with both blockers.
        let ready = repo
            .list_ready(ReadyQuery {
                project_id: Some(project.id.clone()),
                limit: 50,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            !ready.iter().any(|t| t.id == source.id),
            "source must NOT be ready with both blockers open"
        );

        // ── Close the normal blocker first — source still NOT ready (review hold open) ──
        repo.set_status(&normal_blocker.id, "closed").await.unwrap();

        let ready = repo
            .list_ready(ReadyQuery {
                project_id: Some(project.id.clone()),
                limit: 50,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            !ready.iter().any(|t| t.id == source.id),
            "source must still NOT be ready after closing normal blocker while review hold remains"
        );

        // ── Close the review hold — NOW source IS ready ──
        repo.set_status(&review_hold.id, "closed").await.unwrap();

        let ready = repo
            .list_ready(ReadyQuery {
                project_id: Some(project.id.clone()),
                limit: 50,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            ready.iter().any(|t| t.id == source.id),
            "source must be ready after all blockers are closed (regardless of release order)"
        );
    }

    /// Regression (pdn6 release-side): prove that `emit_unblocked_tasks` fires
    /// for a normal (non-review) blocker when closed via `transition(Close)`,
    /// not just via the raw `set_status` path used in the tests above.
    ///
    /// The existing tests prove the SQL readiness predicate works for both
    /// blocker types, and the coordinator-level test proves
    /// `emit_unblocked_tasks` works for review holds. This test fills the
    /// remaining gap: the db-level event-driven release path for normal
    /// blockers, so any predicate change that special-cases `review` blockers
    /// is caught at this layer too.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn emit_unblocked_tasks_fires_for_normal_blocker_via_transition() {
        let db = Database::open_in_memory().unwrap();
        let (bus, captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;

        // Source task — a normal work item.
        let source = make_task(&repo, &epic_id, "task", None).await;

        // Normal blocker — `spike` uses simple lifecycle
        // (open → in_progress → closed), so `transition(Close)` from `open`
        // is a valid state-machine move.
        let normal_blocker = repo
            .create_in_project(
                &project.id,
                Some(&epic_id),
                "Dependency spike",
                "",
                "",
                "spike",
                0,
                "system",
                None,
                None,
            )
            .await
            .unwrap();

        // Wire the blocker edge.
        repo.add_blocker(&source.id, &normal_blocker.id)
            .await
            .unwrap();

        // ── Pre-condition: source is NOT ready ──
        let ready = repo
            .list_ready(ReadyQuery {
                project_id: Some(project.id.clone()),
                limit: 50,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            !ready.iter().any(|t| t.id == source.id),
            "source must NOT be ready while the normal blocker is open"
        );

        // Close the blocker via `transition(Close)` — this calls
        // `emit_unblocked_tasks` internally.
        repo.transition(
            &normal_blocker.id,
            TransitionAction::Close,
            "system",
            "coordinator",
            None,
            None,
        )
        .await
        .unwrap();

        // ── emit_unblocked_tasks must fire a TaskUpdated for the source ──
        let source_released = {
            let events = captured.lock().unwrap();
            events.iter().any(|ev| {
                ev.entity_type == "task"
                    && ev.action == "updated"
                    && ev
                        .payload
                        .get("task")
                        .and_then(|t| t.get("id"))
                        .and_then(|v| v.as_str())
                        == Some(source.id.as_str())
            })
        };
        assert!(
            source_released,
            "closing a normal blocker via transition(Close) must emit TaskUpdated \
             for the blocked source via emit_unblocked_tasks"
        );

        // ── The dispatch readiness query must now return the source ──
        let ready = repo
            .list_ready(ReadyQuery {
                project_id: Some(project.id.clone()),
                limit: 50,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            ready.iter().any(|t| t.id == source.id),
            "source must be ready after the normal blocker is closed via transition(Close)"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ci_snapshot_defaults_to_unknown_and_maps_on_task_reads() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac"}]"#)).await;

        let fetched = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(fetched.ci_status, "unknown");
        assert_eq!(fetched.ci_blocking_required_check_names, "[]");
        assert_eq!(fetched.ci_same_signature_count, 0);
        assert!(fetched.ci_head_sha.is_none());

        let listed = repo
            .list_by_project(&project.id)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.id == task.id)
            .unwrap();
        assert_eq!(listed.ci_status, "unknown");
        assert_eq!(listed.ci_blocking_required_check_names, "[]");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ci_snapshot_upsert_read_and_task_mapping() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac"}]"#)).await;

        let snapshot = repo
            .upsert_ci_snapshot(TaskPrCiSnapshotInput {
                task_id: task.id.clone(),
                pr_number: 123,
                head_sha: "abc123".to_string(),
                ci_status: CiStatus::Failing,
                blocking_required_check_names: vec![
                    "Quality Gate".to_string(),
                    "Tests".to_string(),
                ],
                failure_fingerprint: Some("fp-1".to_string()),
                same_signature_count: 2,
                last_remediation_base_sha: Some("base-1".to_string()),
            })
            .await
            .unwrap();

        assert_eq!(snapshot.ci_status, CiStatus::Failing);
        assert_eq!(
            snapshot.blocking_required_check_names,
            ["Quality Gate", "Tests"]
        );

        let read = repo
            .get_ci_snapshot_for_task_pr(&task.id, 123)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read.head_sha, "abc123");
        assert_eq!(read.failure_fingerprint.as_deref(), Some("fp-1"));

        let mapped = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(mapped.ci_status, "failing");
        assert_eq!(mapped.ci_head_sha.as_deref(), Some("abc123"));
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&mapped.ci_blocking_required_check_names).unwrap(),
            vec!["Quality Gate".to_string(), "Tests".to_string()]
        );
        assert_eq!(mapped.ci_same_signature_count, 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ci_snapshot_stale_head_reset_clears_old_signature_state() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac"}]"#)).await;

        repo.upsert_ci_snapshot(TaskPrCiSnapshotInput {
            task_id: task.id.clone(),
            pr_number: 55,
            head_sha: "old-head".to_string(),
            ci_status: CiStatus::Failing,
            blocking_required_check_names: vec!["Tests".to_string()],
            failure_fingerprint: Some("old-fp".to_string()),
            same_signature_count: 4,
            last_remediation_base_sha: Some("old-base".to_string()),
        })
        .await
        .unwrap();

        let reset = repo
            .reset_ci_snapshot_for_head(&task.id, 55, "new-head")
            .await
            .unwrap();

        assert_eq!(reset.head_sha, "new-head");
        assert_eq!(reset.ci_status, CiStatus::Unknown);
        assert!(reset.blocking_required_check_names.is_empty());
        assert!(reset.failure_fingerprint.is_none());
        assert_eq!(reset.same_signature_count, 0);
        assert!(reset.last_remediation_base_sha.is_none());

        let mapped = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(mapped.ci_status, "unknown");
        assert_eq!(mapped.ci_blocking_required_check_names, "[]");
        assert_eq!(mapped.ci_same_signature_count, 0);
        assert!(mapped.ci_failure_fingerprint.is_none());
    }

    // ── CI head reconciliation tests (m116) ─────────────────────────────

    /// Helper: create a task attempt row with optional head SHAs and
    /// publication error for CI head reconciliation testing.
    async fn seed_task_attempt(
        db: &Database,
        task_id: &str,
        mirror_head_sha: Option<&str>,
        github_head_sha: Option<&str>,
        github_publication_error: Option<&str>,
    ) {
        let repo = TaskAttemptRepository::new(db.clone());
        let attempt = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &uuid::Uuid::now_v7().to_string(),
                task_id,
                role: "worker",
                dispatch_key: &format!("dk-{}", uuid::Uuid::now_v7()),
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();

        repo.fill_nullable_fields(FillTaskAttemptParams {
            id: &attempt.id,
            checkpoint_ref: None,
            submit_ref: None,
            pr_url: None,
            mirror_head_sha,
            github_head_sha,
            github_publication_error,
            summary: None,
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ci_head_reconciliation_defaults_to_none_without_attempts() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac"}]"#)).await;

        let fetched = repo.get(&task.id).await.unwrap().unwrap();
        assert!(fetched.ci_mirror_head_sha.is_none());
        assert!(fetched.ci_github_head_sha.is_none());
        assert!(fetched.ci_heads_diverged.is_none());
        assert!(fetched.ci_head_observation_error.is_none());
        // Existing ci_head_sha must be untouched
        assert!(fetched.ci_head_sha.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ci_head_reconciliation_known_equal_heads() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac"}]"#)).await;

        seed_task_attempt(
            &db,
            &task.id,
            Some("abc123mirror"),
            Some("abc123mirror"),
            None,
        )
        .await;

        let fetched = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(fetched.ci_mirror_head_sha.as_deref(), Some("abc123mirror"));
        assert_eq!(fetched.ci_github_head_sha.as_deref(), Some("abc123mirror"));
        assert_eq!(fetched.ci_heads_diverged, Some(false));
        assert!(fetched.ci_head_observation_error.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ci_head_reconciliation_known_diverged_heads() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac"}]"#)).await;

        seed_task_attempt(
            &db,
            &task.id,
            Some("mirror-sha-abc"),
            Some("github-sha-xyz"),
            None,
        )
        .await;

        let fetched = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(
            fetched.ci_mirror_head_sha.as_deref(),
            Some("mirror-sha-abc")
        );
        assert_eq!(
            fetched.ci_github_head_sha.as_deref(),
            Some("github-sha-xyz")
        );
        assert_eq!(fetched.ci_heads_diverged, Some(true));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ci_head_reconciliation_unknown_mirror_head() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac"}]"#)).await;

        // Only GitHub head is known; mirror is unknown.
        seed_task_attempt(&db, &task.id, None, Some("github-only-sha"), None).await;

        let fetched = repo.get(&task.id).await.unwrap().unwrap();
        assert!(fetched.ci_mirror_head_sha.is_none());
        assert_eq!(
            fetched.ci_github_head_sha.as_deref(),
            Some("github-only-sha")
        );
        // Both heads must be known for heads_diverged to be non-None.
        assert!(fetched.ci_heads_diverged.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ci_head_reconciliation_unknown_github_no_pr() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac"}]"#)).await;

        // Only mirror head is known; GitHub is unknown (no PR branch).
        seed_task_attempt(&db, &task.id, Some("mirror-only-sha"), None, None).await;

        let fetched = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(
            fetched.ci_mirror_head_sha.as_deref(),
            Some("mirror-only-sha")
        );
        assert!(fetched.ci_github_head_sha.is_none());
        assert!(fetched.ci_heads_diverged.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ci_head_reconciliation_observation_error() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac"}]"#)).await;

        // Mirror head known, GitHub publication failed — error recorded.
        seed_task_attempt(
            &db,
            &task.id,
            Some("mirror-head-sha"),
            Some("stale-github-head"),
            Some("push_rejected: GH006 Protected branch update failed"),
        )
        .await;

        let fetched = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(
            fetched.ci_mirror_head_sha.as_deref(),
            Some("mirror-head-sha")
        );
        assert_eq!(
            fetched.ci_github_head_sha.as_deref(),
            Some("stale-github-head")
        );
        assert_eq!(fetched.ci_heads_diverged, Some(true));
        assert_eq!(
            fetched.ci_head_observation_error.as_deref(),
            Some("push_rejected: GH006 Protected branch update failed")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ci_head_reconciliation_latest_attempt_wins() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac"}]"#)).await;

        // Older attempt: equal heads.
        seed_task_attempt(&db, &task.id, Some("old-mirror"), Some("old-mirror"), None).await;

        // Slight delay so the second attempt has a later created_at.
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Newer attempt: diverged heads.
        seed_task_attempt(&db, &task.id, Some("new-mirror"), Some("new-github"), None).await;

        let fetched = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(fetched.ci_mirror_head_sha.as_deref(), Some("new-mirror"));
        assert_eq!(fetched.ci_github_head_sha.as_deref(), Some("new-github"));
        assert_eq!(fetched.ci_heads_diverged, Some(true));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ci_head_reconciliation_populated_on_list_reads() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac"}]"#)).await;

        seed_task_attempt(
            &db,
            &task.id,
            Some("mirror-list"),
            Some("github-list"),
            None,
        )
        .await;

        // Verify list_by_project also surfaces the new fields.
        let listed = repo
            .list_by_project(&project.id)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.id == task.id)
            .unwrap();
        assert_eq!(listed.ci_mirror_head_sha.as_deref(), Some("mirror-list"));
        assert_eq!(listed.ci_github_head_sha.as_deref(), Some("github-list"));
        assert_eq!(listed.ci_heads_diverged, Some(true));

        // Verify resolve also surfaces the new fields.
        let resolved = repo.resolve(&task.short_id).await.unwrap().unwrap();
        assert_eq!(resolved.ci_mirror_head_sha.as_deref(), Some("mirror-list"));
        assert_eq!(resolved.ci_github_head_sha.as_deref(), Some("github-list"));
        assert_eq!(resolved.ci_heads_diverged, Some(true));
    }

    // ── ReopenClass / quality ledger tests ─────────────────────────────────

    /// Helper: walk a task through the full review-reject path
    /// (open → in_progress → needs_task_review → in_task_review → open)
    /// using the given reject action and reason.
    async fn walk_review_reject(
        repo: &TaskRepository,
        task_id: &str,
        action: TransitionAction,
        reason: &str,
    ) -> Task {
        repo.transition(
            task_id,
            TransitionAction::Start,
            "worker",
            "worker",
            None,
            None,
        )
        .await
        .unwrap();
        repo.transition(
            task_id,
            TransitionAction::SubmitTaskReview,
            "worker",
            "worker",
            None,
            None,
        )
        .await
        .unwrap();
        repo.transition(
            task_id,
            TransitionAction::TaskReviewStart,
            "reviewer",
            "reviewer",
            None,
            None,
        )
        .await
        .unwrap();
        repo.transition(task_id, action, "reviewer", "reviewer", Some(reason), None)
            .await
            .unwrap()
    }

    /// Persist classified transitions through `TaskRepository::transition`,
    /// reload/list activity from DB, and assert the `reopen_class` survives.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reopen_class_persists_in_activity_payload() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac1"}]"#)).await;

        // Walk through TaskReviewReject — should persist as review_rejected.
        walk_review_reject(
            &repo,
            &task.id,
            TransitionAction::TaskReviewReject,
            "needs changes",
        )
        .await;

        let activity = repo.list_activity(&task.id).await.unwrap();
        // Expect 4 entries: start, submit, review_start, review_reject
        assert_eq!(activity.len(), 4);
        let reject_payload: serde_json::Value =
            serde_json::from_str(&activity.last().unwrap().payload).unwrap();
        assert_eq!(reject_payload["reopen_class"], "review_rejected");
        assert_eq!(reject_payload["to_status"], "open");
        assert_eq!(reject_payload["from_status"], "in_task_review");

        // Now do a TaskReviewRejectStale — should also be review_rejected.
        walk_review_reject(
            &repo,
            &task.id,
            TransitionAction::TaskReviewRejectStale,
            "stale work",
        )
        .await;

        let activity = repo.list_activity(&task.id).await.unwrap();
        let stale_payload: serde_json::Value =
            serde_json::from_str(&activity.last().unwrap().payload).unwrap();
        assert_eq!(stale_payload["reopen_class"], "review_rejected");
    }

    /// TaskReviewRejectConflict persists merge_conflict but does NOT
    /// increment raw reopen_count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn conflict_reject_classifies_merge_conflict_without_reopen_increment() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac1"}]"#)).await;

        walk_review_reject(
            &repo,
            &task.id,
            TransitionAction::TaskReviewRejectConflict,
            "merge_conflict:{}",
        )
        .await;

        let t = repo.get(&task.id).await.unwrap().unwrap();
        // Conflict does NOT increment reopen_count.
        assert_eq!(t.reopen_count, 0);
        assert_eq!(t.total_reopen_count, 0);

        let activity = repo.list_activity(&task.id).await.unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(&activity.last().unwrap().payload).unwrap();
        assert_eq!(payload["reopen_class"], "merge_conflict");
    }

    /// Non-reopen-like transitions (e.g. Close, Start) must NOT carry a
    /// `reopen_class` in their activity payload.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_reopen_transitions_omit_reopen_class() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac1"}]"#)).await;

        repo.transition(
            &task.id,
            TransitionAction::Start,
            "worker",
            "worker",
            None,
            None,
        )
        .await
        .unwrap();

        let activity = repo.list_activity(&task.id).await.unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(&activity.last().unwrap().payload).unwrap();
        assert!(
            payload.get("reopen_class").is_none(),
            "Start must not set reopen_class"
        );
    }

    /// quality_reopen_count returns 0 for a task with no reopen events.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn quality_reopen_count_zero_for_no_reopen() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac1"}]"#)).await;

        let count = repo.quality_reopen_count(&task.id).await.unwrap();
        assert_eq!(count, 0);
    }

    /// recent_reopen_ledger returns empty for a task with no reopens.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recent_reopen_ledger_empty_for_no_reopen() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac1"}]"#)).await;

        let ledger = repo.recent_reopen_ledger(&task.id, 10).await.unwrap();
        assert!(ledger.is_empty());
    }

    /// Full mixed-class scenario: quality count includes review_rejected,
    /// merge_queue_failed, and other; excludes merge_conflict and superseded.
    /// Raw reopen_count can diverge from quality_reopen_count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn quality_count_includes_and_excludes_correct_classes() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac1"}]"#)).await;

        // 1. TaskReviewReject → review_rejected (quality strike, increments reopen)
        walk_review_reject(
            &repo,
            &task.id,
            TransitionAction::TaskReviewReject,
            "bad code",
        )
        .await;

        // 2. TaskReviewRejectConflict → merge_conflict (NOT a quality strike, no reopen increment)
        walk_review_reject(
            &repo,
            &task.id,
            TransitionAction::TaskReviewRejectConflict,
            "merge_conflict:{}",
        )
        .await;

        // 3. TaskReviewRejectStale → review_rejected (quality strike, increments reopen)
        walk_review_reject(
            &repo,
            &task.id,
            TransitionAction::TaskReviewRejectStale,
            "no progress",
        )
        .await;

        // Check raw counts: TaskReviewReject and TaskReviewRejectStale each
        // incremented reopen_count; conflict did not. So reopen_count = 2.
        let t = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(
            t.reopen_count, 2,
            "raw reopen_count should be 2 (reject + stale)"
        );
        assert_eq!(t.total_reopen_count, 2);

        // Quality count: review_rejected (2) + merge_conflict excluded = 2
        let quality = repo.quality_reopen_count(&task.id).await.unwrap();
        assert_eq!(
            quality, 2,
            "quality count should be 2 (two review_rejected)"
        );

        // Ledger: should have 3 entries (most recent first).
        let ledger = repo.recent_reopen_ledger(&task.id, 10).await.unwrap();
        assert_eq!(ledger.len(), 3, "ledger should have 3 reopen entries");
        // Most recent first: stale, conflict, reject
        assert_eq!(ledger[0].reopen_class, ReopenClass::ReviewRejected);
        assert_eq!(ledger[1].reopen_class, ReopenClass::MergeConflict);
        assert_eq!(ledger[2].reopen_class, ReopenClass::ReviewRejected);
    }

    /// PrCiFailed classifies as merge_queue_failed (quality strike).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pr_ci_failed_classifies_as_merge_queue_failed() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac1"}]"#)).await;

        // Walk to pr_draft: start → submit → review_start → approve → pr_created
        repo.transition(&task.id, TransitionAction::Start, "w", "worker", None, None)
            .await
            .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::SubmitTaskReview,
            "w",
            "worker",
            None,
            None,
        )
        .await
        .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::TaskReviewStart,
            "r",
            "reviewer",
            None,
            None,
        )
        .await
        .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::TaskReviewApprove,
            "r",
            "reviewer",
            None,
            None,
        )
        .await
        .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::PrCreated,
            "sys",
            "system",
            None,
            None,
        )
        .await
        .unwrap();

        // Now PrCiFailed from pr_draft.
        repo.transition(
            &task.id,
            TransitionAction::PrCiFailed,
            "sys",
            "system",
            None,
            None,
        )
        .await
        .unwrap();

        let t = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(t.status, "open");
        assert_eq!(t.reopen_count, 1);

        let activity = repo.list_activity(&task.id).await.unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(&activity.last().unwrap().payload).unwrap();
        assert_eq!(payload["reopen_class"], "merge_queue_failed");

        // Quality count should be 1 (merge_queue_failed is a quality strike).
        let quality = repo.quality_reopen_count(&task.id).await.unwrap();
        assert_eq!(quality, 1);

        let ledger = repo.recent_reopen_ledger(&task.id, 10).await.unwrap();
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].reopen_class, ReopenClass::MergeQueueFailed);
    }

    /// PrConflict classifies as merge_conflict (NOT a quality strike)
    /// and does NOT increment reopen_count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pr_conflict_classifies_as_merge_conflict_no_quality_strike() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac1"}]"#)).await;

        // Walk to approved → PrConflict
        repo.transition(&task.id, TransitionAction::Start, "w", "worker", None, None)
            .await
            .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::SubmitTaskReview,
            "w",
            "worker",
            None,
            None,
        )
        .await
        .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::TaskReviewStart,
            "r",
            "reviewer",
            None,
            None,
        )
        .await
        .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::TaskReviewApprove,
            "r",
            "reviewer",
            None,
            None,
        )
        .await
        .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::PrConflict,
            "sys",
            "system",
            Some("merge_conflict:{\"head\":\"abc\"}"),
            None,
        )
        .await
        .unwrap();

        let t = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(t.status, "open");
        // PrConflict does NOT increment reopen_count.
        assert_eq!(t.reopen_count, 0);

        let activity = repo.list_activity(&task.id).await.unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(&activity.last().unwrap().payload).unwrap();
        assert_eq!(payload["reopen_class"], "merge_conflict");

        // Quality count = 0 (merge_conflict excluded).
        let quality = repo.quality_reopen_count(&task.id).await.unwrap();
        assert_eq!(quality, 0);

        let ledger = repo.recent_reopen_ledger(&task.id, 10).await.unwrap();
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].reopen_class, ReopenClass::MergeConflict);
    }

    /// PrChangesRequested classifies as review_rejected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pr_changes_requested_classifies_as_review_rejected() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac1"}]"#)).await;

        // Walk to pr_review: start → submit → review_start → approve → pr_created → pr_undraft
        repo.transition(&task.id, TransitionAction::Start, "w", "worker", None, None)
            .await
            .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::SubmitTaskReview,
            "w",
            "worker",
            None,
            None,
        )
        .await
        .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::TaskReviewStart,
            "r",
            "reviewer",
            None,
            None,
        )
        .await
        .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::TaskReviewApprove,
            "r",
            "reviewer",
            None,
            None,
        )
        .await
        .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::PrCreated,
            "sys",
            "system",
            None,
            None,
        )
        .await
        .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::PrUndraft,
            "sys",
            "system",
            None,
            None,
        )
        .await
        .unwrap();

        // PrChangesRequested from pr_review.
        repo.transition(
            &task.id,
            TransitionAction::PrChangesRequested,
            "r",
            "reviewer",
            Some("fix the tests"),
            None,
        )
        .await
        .unwrap();

        let t = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(t.status, "open");
        assert_eq!(t.reopen_count, 1);

        let activity = repo.list_activity(&task.id).await.unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(&activity.last().unwrap().payload).unwrap();
        assert_eq!(payload["reopen_class"], "review_rejected");

        let quality = repo.quality_reopen_count(&task.id).await.unwrap();
        assert_eq!(quality, 1);

        let ledger = repo.recent_reopen_ledger(&task.id, 10).await.unwrap();
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].reopen_class, ReopenClass::ReviewRejected);
        assert_eq!(ledger[0].reason.as_deref(), Some("fix the tests"));
    }

    /// Generic Reopen (closed → open) classifies as other (quality strike).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn generic_reopen_classifies_as_other() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac1"}]"#)).await;

        // Walk to closed: start → submit → review_start → approve → close
        repo.transition(&task.id, TransitionAction::Start, "w", "worker", None, None)
            .await
            .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::SubmitTaskReview,
            "w",
            "worker",
            None,
            None,
        )
        .await
        .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::TaskReviewStart,
            "r",
            "reviewer",
            None,
            None,
        )
        .await
        .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::TaskReviewApprove,
            "r",
            "reviewer",
            None,
            None,
        )
        .await
        .unwrap();
        repo.transition(&task.id, TransitionAction::Close, "w", "worker", None, None)
            .await
            .unwrap();

        // Reopen from closed.
        repo.transition(
            &task.id,
            TransitionAction::Reopen,
            "w",
            "worker",
            Some("need to fix something"),
            None,
        )
        .await
        .unwrap();

        let t = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(t.status, "open");
        assert_eq!(t.reopen_count, 1);

        let activity = repo.list_activity(&task.id).await.unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(&activity.last().unwrap().payload).unwrap();
        assert_eq!(payload["reopen_class"], "other");

        let quality = repo.quality_reopen_count(&task.id).await.unwrap();
        assert_eq!(quality, 1, "other is a quality strike");

        let ledger = repo.recent_reopen_ledger(&task.id, 10).await.unwrap();
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].reopen_class, ReopenClass::Other);
    }

    /// Historical activity rows that lack `reopen_class` default to `other`
    /// (conservative) in both quality_reopen_count and recent_reopen_ledger.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn historical_missing_reopen_class_defaults_to_other() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac1"}]"#)).await;

        // Manually insert a historical-style activity_log entry that looks like
        // a review-reject reopen but has no `reopen_class` field (simulating
        // pre-feature data).
        let historical_payload = serde_json::json!({
            "from_status": "in_task_review",
            "to_status": "open",
            "reason": "historical rejection"
        });
        repo.log_activity(
            Some(&task.id),
            "reviewer",
            "reviewer",
            "status_changed",
            &historical_payload.to_string(),
        )
        .await
        .unwrap();

        // quality_reopen_count must count this as `other` (quality strike).
        let quality = repo.quality_reopen_count(&task.id).await.unwrap();
        assert_eq!(
            quality, 1,
            "historical row without reopen_class must default to other (quality strike)"
        );

        // recent_reopen_ledger must return it with ReopenClass::Other.
        let ledger = repo.recent_reopen_ledger(&task.id, 10).await.unwrap();
        assert_eq!(ledger.len(), 1);
        assert_eq!(
            ledger[0].reopen_class,
            ReopenClass::Other,
            "historical row missing reopen_class must read as Other"
        );
        assert_eq!(ledger[0].from_status, "in_task_review");
        assert_eq!(ledger[0].reason.as_deref(), Some("historical rejection"));
    }

    /// Mixed scenario: raw reopen_count diverges from quality_reopen_count
    /// when conflict and superseded reopens are present alongside quality
    /// strikes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn raw_and_quality_counts_diverge_after_mixed_transitions() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac1"}]"#)).await;

        // Review reject → quality strike, increments reopen (reopen=1, quality=1)
        walk_review_reject(&repo, &task.id, TransitionAction::TaskReviewReject, "bad").await;
        // Conflict → NOT quality strike, does NOT increment reopen (reopen=1, quality=1)
        walk_review_reject(
            &repo,
            &task.id,
            TransitionAction::TaskReviewRejectConflict,
            "merge_conflict:{}",
        )
        .await;
        // Review reject stale → quality strike, increments reopen (reopen=2, quality=2)
        walk_review_reject(
            &repo,
            &task.id,
            TransitionAction::TaskReviewRejectStale,
            "stale",
        )
        .await;

        // Also inject a historical "superseded" reopen (no reopen_class).
        // This defaults to `other` = quality strike.
        let historical_superseded = serde_json::json!({
            "from_status": "closed",
            "to_status": "open",
            "reason": "superseded by newer PR"
        });
        repo.log_activity(
            Some(&task.id),
            "system",
            "system",
            "status_changed",
            &historical_superseded.to_string(),
        )
        .await
        .unwrap();

        let t = repo.get(&task.id).await.unwrap().unwrap();
        // Raw: reject (1) + stale (1) = 2. Conflict and historical don't increment.
        assert_eq!(t.reopen_count, 2, "raw reopen_count should be 2");
        assert_eq!(t.total_reopen_count, 2);

        // Quality: review_rejected (2) + historical-other (1) = 3.
        // merge_conflict excluded.
        let quality = repo.quality_reopen_count(&task.id).await.unwrap();
        assert_eq!(
            quality, 3,
            "quality count should be 3 (2 review_rejected + 1 historical-other)"
        );

        // Ledger: 4 entries (most recent first: historical, stale, conflict, reject).
        let ledger = repo.recent_reopen_ledger(&task.id, 10).await.unwrap();
        assert_eq!(ledger.len(), 4);
        assert_eq!(ledger[0].reopen_class, ReopenClass::Other); // historical
        assert_eq!(ledger[1].reopen_class, ReopenClass::ReviewRejected); // stale
        assert_eq!(ledger[2].reopen_class, ReopenClass::MergeConflict); // conflict
        assert_eq!(ledger[3].reopen_class, ReopenClass::ReviewRejected); // reject
    }

    /// recent_reopen_ledger respects the limit parameter.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reopen_ledger_respects_limit() {
        let db = Database::open_in_memory().unwrap();
        let (bus, _captured) = capturing_bus();
        let repo = TaskRepository::new(db.clone(), bus);
        let project = make_project(&db).await;
        let epic_id = make_epic(&db, &project.id).await;
        let task = make_task(&repo, &epic_id, "task", Some(r#"[{"title":"ac1"}]"#)).await;

        // Create 3 reopen events.
        walk_review_reject(&repo, &task.id, TransitionAction::TaskReviewReject, "r1").await;
        walk_review_reject(
            &repo,
            &task.id,
            TransitionAction::TaskReviewRejectStale,
            "r2",
        )
        .await;
        walk_review_reject(&repo, &task.id, TransitionAction::TaskReviewReject, "r3").await;

        // Limit to 2.
        let ledger = repo.recent_reopen_ledger(&task.id, 2).await.unwrap();
        assert_eq!(ledger.len(), 2, "limit=2 should return only 2 entries");
        // Most recent first.
        assert_eq!(ledger[0].reason.as_deref(), Some("r3"));
        assert_eq!(ledger[1].reason.as_deref(), Some("r2"));
    }
}

pub struct CreateTaskParams<'a> {
    pub epic_id: &'a str,
    pub title: &'a str,
    pub description: &'a str,
    pub design: &'a str,
    pub issue_type: &'a str,
    pub priority: i64,
    pub owner: &'a str,
    pub status: Option<&'a str>,
}

pub struct CreateTaskInProjectParams<'a> {
    pub project_id: &'a str,
    pub epic_id: Option<&'a str>,
    pub title: &'a str,
    pub description: &'a str,
    pub design: &'a str,
    pub issue_type: &'a str,
    pub priority: i64,
    pub owner: &'a str,
    pub status: Option<&'a str>,
}

pub struct UpdateTaskParams<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub description: &'a str,
    pub design: &'a str,
    pub priority: i64,
    pub owner: &'a str,
    pub labels: &'a str,
    pub acceptance_criteria: &'a str,
}

impl Default for ListQuery {
    fn default() -> Self {
        Self {
            status: None,
            project_id: None,
            issue_type: None,
            priority: None,
            label: None,
            text: None,
            parent: None,
            sort: "priority".to_owned(),
            limit: 25,
            offset: 0,
        }
    }
}

pub struct ListResult {
    pub tasks: Vec<Task>,
    pub total_count: i64,
}

/// Filters for [`TaskRepository::count_grouped`].
pub struct CountQuery {
    pub project_id: Option<String>,
    pub status: Option<String>,
    pub issue_type: Option<String>,
    pub priority: Option<i64>,
    pub label: Option<String>,
    pub text: Option<String>,
    pub parent: Option<String>,
    /// "status" | "priority" | "issue_type" | "parent"
    pub group_by: Option<String>,
}

/// Filters for [`TaskRepository::query_activity`].
pub struct ActivityQuery {
    pub project_id: Option<String>,
    pub task_id: Option<String>,
    pub event_type: Option<String>,
    pub actor_role: Option<String>,
    pub from_time: Option<String>,
    pub to_time: Option<String>,
    pub limit: i64,
    pub offset: i64,
}

impl Default for ActivityQuery {
    fn default() -> Self {
        Self {
            task_id: None,
            project_id: None,
            event_type: None,
            actor_role: None,
            from_time: None,
            to_time: None,
            limit: 50,
            offset: 0,
        }
    }
}

/// Minimal task reference returned by blocker listing queries.
#[derive(Debug, sqlx::FromRow)]
pub struct BlockerRef {
    pub task_id: String,
    pub short_id: String,
    pub title: String,
    pub status: String,
}

#[derive(Clone, Debug)]
pub(super) enum SqlParam {
    Text(String),
    Integer(i64),
}

/// Filters for [`TaskRepository::list_ready`].
pub struct ReadyQuery {
    pub project_id: Option<String>,
    pub issue_type: Option<String>,
    pub label: Option<String>,
    pub owner: Option<String>,
    pub priority_max: Option<i64>,
    pub limit: i64,
}

impl Default for ReadyQuery {
    fn default() -> Self {
        Self {
            issue_type: None,
            project_id: None,
            label: None,
            owner: None,
            priority_max: None,
            limit: 25,
        }
    }
}

pub struct TaskRepository {
    pub(super) db: Database,
    pub(super) events: EventBus,
}

impl TaskRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self { db, events }
    }

    pub(super) async fn generate_short_id(&self, seed_id: &str) -> Result<String> {
        self.db.ensure_initialized().await?;
        let seed = uuid::Uuid::parse_str(seed_id).map_err(|e| Error::Internal(e.to_string()))?;
        let candidate = short_id_from_uuid(&seed);
        if !short_id_exists(self.db.pool(), "tasks", &candidate).await? {
            return Ok(candidate);
        }
        for _ in 0..16 {
            let candidate = short_id_from_uuid(&uuid::Uuid::now_v7());
            if !short_id_exists(self.db.pool(), "tasks", &candidate).await? {
                return Ok(candidate);
            }
        }
        Err(Error::Internal(
            "short_id collision after 16 retries".into(),
        ))
    }
}

/// Expands to a `sqlx::query_as!(Task, "...", $id)` call with the full
/// SELECT projection for a `Task` row keyed by id.
///
/// Defined as a `macro_rules!` (rather than `const &str`) because
/// `sqlx::query_as!` requires a string-literal SQL argument at the token
/// level and will not accept `concat!`/macro expansions that resolve to
/// a literal. The macro must also project every field of `Task` including
/// `pr_url`, `agent_type` (both real columns) and a zero-valued
/// `unresolved_blocker_count` (computed elsewhere via subquery) because
/// `query_as!` does not honor `#[sqlx(default)]`.
macro_rules! task_select_where_id {
    ($id:expr) => {
        ::sqlx::query_as::<_, ::djinn_core::models::Task>(
            r#"SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                status, priority, owner, labels::text AS labels, acceptance_criteria::text AS acceptance_criteria,
                reopen_count, continuation_count,
                total_reopen_count,
                intervention_count, last_intervention_at,
                created_at, updated_at, closed_at,
                close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs::text AS memory_refs,
                agent_type, created_by_user_id,
                COALESCE((SELECT s.ci_status FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1), 'unknown') AS ci_status,
                (SELECT s.head_sha FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_head_sha,
                (SELECT s.pr_number FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_pr_number,
                COALESCE((SELECT s.blocking_required_check_names::text FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1), '[]') AS ci_blocking_required_check_names,
                (SELECT s.failure_fingerprint FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_failure_fingerprint,
                (SELECT s.first_seen_at FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_first_seen_at,
                (SELECT s.last_seen_at FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_last_seen_at,
                COALESCE((SELECT s.same_signature_count FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1), 0) AS ci_same_signature_count,
                (SELECT s.last_remediation_base_sha FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_last_remediation_base_sha,
                    (SELECT ta.mirror_head_sha FROM task_attempts ta WHERE ta.task_id = tasks.id AND (ta.mirror_head_sha IS NOT NULL OR ta.github_head_sha IS NOT NULL OR ta.github_publication_error IS NOT NULL) ORDER BY ta.created_at DESC LIMIT 1) AS ci_mirror_head_sha,
                    (SELECT ta.github_head_sha FROM task_attempts ta WHERE ta.task_id = tasks.id AND (ta.mirror_head_sha IS NOT NULL OR ta.github_head_sha IS NOT NULL OR ta.github_publication_error IS NOT NULL) ORDER BY ta.created_at DESC LIMIT 1) AS ci_github_head_sha,
                    (SELECT CASE WHEN ta.mirror_head_sha IS NOT NULL AND ta.github_head_sha IS NOT NULL THEN ta.mirror_head_sha != ta.github_head_sha END FROM task_attempts ta WHERE ta.task_id = tasks.id AND (ta.mirror_head_sha IS NOT NULL OR ta.github_head_sha IS NOT NULL OR ta.github_publication_error IS NOT NULL) ORDER BY ta.created_at DESC LIMIT 1) AS ci_heads_diverged,
                    (SELECT ta.github_publication_error FROM task_attempts ta WHERE ta.task_id = tasks.id AND ta.github_publication_error IS NOT NULL ORDER BY ta.created_at DESC LIMIT 1) AS ci_head_observation_error,
                CAST(0 AS BIGINT) AS unresolved_blocker_count
             FROM tasks WHERE id = $1"#,
        )
        .bind(&$id)
    };
}
pub(super) use task_select_where_id;

pub(super) fn short_id_from_uuid(id: &uuid::Uuid) -> String {
    let bytes = id.as_bytes();
    let n = u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    encode_base36(n % 1_679_616)
}

pub(super) fn encode_base36(mut n: u32) -> String {
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = [b'0'; 4];
    for i in (0..4).rev() {
        buf[i] = CHARS[(n % 36) as usize];
        n /= 36;
    }
    String::from_utf8(buf.to_vec()).unwrap()
}

/// Check if a constraint violation occurred.
pub(super) fn is_constraint_violation(db_err: &dyn sqlx::error::DatabaseError) -> bool {
    db_err.is_unique_violation()
        || db_err.is_foreign_key_violation()
        || db_err.message().contains("constraint failed")
}

/// Extract the constraint name from a database error message.
pub(super) fn extract_constraint_name(db_err: &dyn sqlx::error::DatabaseError) -> Option<String> {
    let message = db_err.message();
    // SQLite constraint messages follow patterns like:
    // "UNIQUE constraint failed: tasks.short_id"
    // "FOREIGN KEY constraint failed"
    if message.contains("short_id") {
        Some("short_id".to_string())
    } else {
        None
    }
}

pub(super) async fn short_id_exists(pool: &PgPool, table: &str, short_id: &str) -> Result<bool> {
    // Postgres `EXISTS(...)` returns BOOLEAN — decode as `bool`, not i64
    // (MySQL returned 0/1, so this was `<_, i64> > 0` pre-cutover and 500d
    // with "i64 (INT8) is not compatible with SQL type BOOL", breaking task
    // creation, which mints short_ids via this helper). Mirrors the sibling
    // fix in epic.rs::short_id_exists (commit 511888d0f).
    let sql = format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE short_id = $1)");
    Ok(sqlx::query_scalar::<_, bool>(&sql)
        .bind(short_id)
        .fetch_one(pool)
        .await?)
}

/// Reopen a closed epic when a task is added to it or moved to it.
/// Inlined from EpicRepository::reopen to avoid a circular dependency.
pub(super) async fn maybe_reopen_epic(
    db: &Database,
    events: &EventBus,
    epic_id: &str,
) -> Result<()> {
    let closed: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!: i64" FROM epics WHERE id = $1 AND status = 'closed'"#,
        epic_id
    )
    .fetch_one(db.pool())
    .await?;

    if closed == 0 {
        return Ok(());
    }

    sqlx::query!(
        r#"UPDATE epics SET status = 'open', closed_at = NULL,
             updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
         WHERE id = $1"#,
        epic_id
    )
    .execute(db.pool())
    .await?;

    if let Some(epic) = sqlx::query_as!(
        djinn_core::models::Epic,
        r#"SELECT id, project_id, short_id, title, description, emoji, color, status,
                owner, created_at, updated_at, closed_at, memory_refs::text AS "memory_refs!",
                auto_breakdown AS "auto_breakdown!: bool",
                originating_adr_id, created_by_user_id
         FROM epics WHERE id = $1"#,
        epic_id
    )
    .fetch_optional(db.pool())
    .await?
    {
        events.send(DjinnEventEnvelope::epic_updated(&epic));
    }

    Ok(())
}
