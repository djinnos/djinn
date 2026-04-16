use sqlx::MySqlPool;

use crate::database::Database;
use crate::{Error, Result};
use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::{
    ActivityEntry, IssueType, Task, TaskStatus, TransitionAction, compute_transition_for_issue_type,
};

mod activity;
mod blockers;
mod queries;
mod reads;
mod status;
mod writes;

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
    use djinn_core::models::{Project, Task, TaskStatus, TransitionAction};

    use crate::database::Database;

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
            "INSERT INTO projects (id, name, path) VALUES (?, ?, ?)",
            id,
            "task-project",
            "/tmp/task-project"
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query_as!(
            Project,
            r#"SELECT id, name, path, created_at, target_branch,
                  auto_merge AS "auto_merge!: bool",
                  sync_enabled AS "sync_enabled!: bool",
                  sync_remote
           FROM projects WHERE id = ?"#,
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
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            epic_id,
            project_id,
            "ep01",
            "Epic",
            "",
            "",
            "",
            "",
            "[]"
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

        let verifying = repo
            .transition(
                &task.id,
                TransitionAction::SubmitVerification,
                "worker-1",
                "worker",
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(verifying.status, TaskStatus::Verifying.as_str());

        let needs_review = repo
            .transition(
                &task.id,
                TransitionAction::VerificationPass,
                "verification-1",
                "verification",
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

        let persisted = sqlx::query_as::<_, Task>(TASK_SELECT_WHERE_ID)
            .bind(&task.id)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(persisted.status, TaskStatus::Approved.as_str());
        assert_eq!(persisted.reopen_count, 0);
        assert_eq!(persisted.continuation_count, 0);
        assert!(persisted.closed_at.is_none());

        let activity = repo.list_activity(&task.id).await.unwrap();
        assert_eq!(activity.len(), 5);
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
        assert_eq!(events.len(), 6);
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

        let original = sqlx::query_as::<_, Task>(TASK_SELECT_WHERE_ID)
            .bind(&task.id)
            .fetch_one(db.pool())
            .await
            .unwrap();

        let err = repo
            .transition(
                &task.id,
                TransitionAction::VerificationPass,
                "verification-1",
                "verification",
                None,
                None,
            )
            .await
            .unwrap_err();

        assert!(matches!(err, Error::InvalidTransition(_)));

        let persisted = sqlx::query_as::<_, Task>(TASK_SELECT_WHERE_ID)
            .bind(&task.id)
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

pub(super) const TASK_SELECT_WHERE_ID: &str =
    "SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
            status, priority, owner, labels, acceptance_criteria,
            reopen_count, continuation_count, verification_failure_count,
            total_reopen_count, total_verification_failure_count,
            intervention_count, last_intervention_at,
            created_at, updated_at, closed_at,
            close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs
     FROM tasks WHERE id = ?";

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

pub(super) async fn short_id_exists(
    pool: &MySqlPool,
    table: &str,
    short_id: &str,
) -> Result<bool> {
    let sql = format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE short_id = ?)");
    Ok(sqlx::query_scalar::<_, i64>(&sql)
        .bind(short_id)
        .fetch_one(pool)
        .await?
        > 0)
}

/// Reopen a closed epic when a task is added to it or moved to it.
/// Inlined from EpicRepository::reopen to avoid a circular dependency.
pub(super) async fn maybe_reopen_epic(
    db: &Database,
    events: &EventBus,
    epic_id: &str,
) -> Result<()> {
    let closed = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM epics WHERE id = ? AND `status` = 'closed'",
        epic_id
    )
    .fetch_one(db.pool())
    .await?;

    if closed == 0 {
        return Ok(());
    }

    sqlx::query!(
        "UPDATE epics SET `status` = 'open', closed_at = NULL,
             updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
         WHERE id = ?",
        epic_id
    )
    .execute(db.pool())
    .await?;

    if let Some(epic) = sqlx::query_as!(
        djinn_core::models::Epic,
        r#"SELECT id, project_id, short_id, title, description, emoji, color, `status`,
                owner, created_at, updated_at, closed_at, memory_refs,
                auto_breakdown AS "auto_breakdown!: bool",
                originating_adr_id
         FROM epics WHERE id = ?"#,
        epic_id
    )
    .fetch_optional(db.pool())
    .await?
    {
        events.send(DjinnEventEnvelope::epic_updated(&epic));
    }

    Ok(())
}
