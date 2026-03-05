use tokio::sync::broadcast;

use sqlx::SqlitePool;

use crate::db::connection::Database;
use crate::db::repositories::epic::EpicRepository;
use crate::db::repositories::epic_review_batch::EpicReviewBatchRepository;
use crate::error::{Error, Result};
use crate::events::DjinnEvent;
use crate::models::task::{ActivityEntry, Task, TaskStatus, TransitionAction, compute_transition};

mod activity;
mod blockers;
mod queries;

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
    pub(super) events: broadcast::Sender<DjinnEvent>,
}

impl TaskRepository {
    pub fn new(db: Database, events: broadcast::Sender<DjinnEvent>) -> Self {
        Self { db, events }
    }

    pub async fn list_by_epic(&self, epic_id: &str) -> Result<Vec<Task>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Task>(
            "SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, memory_refs
             FROM tasks WHERE epic_id = ?1 ORDER BY priority, created_at",
        )
        .bind(epic_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn list_by_status(&self, status: &str) -> Result<Vec<Task>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Task>(
            "SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, memory_refs
             FROM tasks WHERE status = ?1 ORDER BY priority, created_at",
        )
        .bind(status)
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Task>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Task>(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_optional(self.db.pool())
            .await?)
    }

    pub async fn get_by_short_id(&self, short_id: &str) -> Result<Option<Task>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Task>(
            "SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, memory_refs
             FROM tasks WHERE short_id = ?1",
        )
        .bind(short_id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn create(
        &self,
        epic_id: &str,
        title: &str,
        description: &str,
        design: &str,
        issue_type: &str,
        priority: i64,
        owner: &str,
    ) -> Result<Task> {
        self.db.ensure_initialized().await?;
        let project_id =
            sqlx::query_scalar::<_, String>("SELECT project_id FROM epics WHERE id = ?1")
                .bind(epic_id)
                .fetch_optional(self.db.pool())
                .await?
                .ok_or_else(|| Error::Internal(format!("epic not found: {epic_id}")))?;
        self.create_in_project(
            &project_id,
            Some(epic_id),
            title,
            description,
            design,
            issue_type,
            priority,
            owner,
        )
        .await
    }

    pub async fn create_in_project(
        &self,
        project_id: &str,
        epic_id: Option<&str>,
        title: &str,
        description: &str,
        design: &str,
        issue_type: &str,
        priority: i64,
        owner: &str,
    ) -> Result<Task> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        let short_id = self.generate_short_id(&id).await?;
        sqlx::query(
            "INSERT INTO tasks
                (id, project_id, short_id, epic_id, title, description, design,
                 issue_type, priority, owner)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .bind(&id)
        .bind(project_id)
        .bind(&short_id)
        .bind(epic_id)
        .bind(title)
        .bind(description)
        .bind(design)
        .bind(issue_type)
        .bind(priority)
        .bind(owner)
        .execute(self.db.pool())
        .await?;
        let task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(&id)
            .fetch_one(self.db.pool())
            .await?;

        if let Some(epic_id) = epic_id {
            let epic_repo = EpicRepository::new(self.db.clone(), self.events.clone());
            if let Some(epic) = epic_repo.get(epic_id).await?
                && (epic.status == "closed" || epic.status == "in_review")
            {
                let _ = epic_repo.reopen(epic_id).await?;
            }
        }

        let _ = self.events.send(DjinnEvent::TaskCreated(task.clone()));
        Ok(task)
    }

    pub async fn update(
        &self,
        id: &str,
        title: &str,
        description: &str,
        design: &str,
        priority: i64,
        owner: &str,
        labels: &str,
        acceptance_criteria: &str,
    ) -> Result<Task> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE tasks SET
                title = ?2, description = ?3, design = ?4,
                priority = ?5, owner = ?6, labels = ?7,
                acceptance_criteria = ?8,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(id)
        .bind(title)
        .bind(description)
        .bind(design)
        .bind(priority)
        .bind(owner)
        .bind(labels)
        .bind(acceptance_criteria)
        .execute(self.db.pool())
        .await?;
        let task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(self.db.pool())
            .await?;

        let _ = self.events.send(DjinnEvent::TaskUpdated(task.clone()));
        Ok(task)
    }

    /// Transition a task through the state machine.
    ///
    /// Validates the action against the current status, applies all side effects
    /// (reopen_count, continuation_count, closed_at, blocked_from_status, close_reason),
    /// writes an activity_log entry, and emits a `TaskUpdated` event — all atomically.
    pub async fn transition(
        &self,
        id: &str,
        action: TransitionAction,
        actor_id: &str,
        actor_role: &str,
        reason: Option<&str>,
        target_override: Option<TaskStatus>,
    ) -> Result<Task> {
        // Check reason requirement before touching the DB.
        if action.requires_reason() && reason.map(str::is_empty).unwrap_or(true) {
            return Err(Error::InvalidTransition(format!(
                "{action:?} requires a non-empty reason"
            )));
        }

        self.db.ensure_initialized().await?;
        let reason_str = reason.unwrap_or("").to_owned();
        let mut tx = self.db.pool().begin().await?;

        // Load current task.
        let current: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        let from = TaskStatus::parse(&current.status)?;

        // Validate and compute side effects.
        let apply = compute_transition(&action, &from, target_override.as_ref())?;

        // For Start: block if any unresolved blockers exist.
        // A blocker is unresolved if its blocking task has not reached post-merge states.
        if action == TransitionAction::Start {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM blockers b
                 JOIN tasks bt ON b.blocking_task_id = bt.id
                 WHERE b.task_id = ?1
                   AND bt.status != 'closed'",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            if count > 0 {
                return Err(Error::InvalidTransition(
                    "task has unresolved blockers".into(),
                ));
            }
        }

        let to_status = apply.to_status.expect("all transitions must have a target status");
        let to_str = to_status.as_str();
        let from_str = from.as_str();

        // Apply all side effects atomically.
        sqlx::query(
            "UPDATE tasks SET
                status = ?2,
                reopen_count = reopen_count + ?3,
                continuation_count = CASE WHEN ?4 THEN 0 ELSE continuation_count END,
                closed_at = CASE
                    WHEN ?5 THEN strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                    WHEN ?6 THEN NULL
                    ELSE closed_at
                END,
                close_reason = CASE
                    WHEN ?7 IS NOT NULL THEN ?7
                    WHEN ?8 THEN NULL
                    ELSE close_reason
                END,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(id)
        .bind(to_str)
        .bind(if apply.increment_reopen { 1i64 } else { 0 })
        .bind(apply.reset_continuation)
        .bind(apply.set_closed_at)
        .bind(apply.clear_closed_at)
        .bind(apply.close_reason)
        .bind(apply.clear_close_reason)
        .execute(&mut *tx)
        .await?;

        // Append activity log entry.
        let activity_id = uuid::Uuid::now_v7().to_string();
        let payload = serde_json::json!({
            "from_status": from_str,
            "to_status": to_str,
            "reason": if reason_str.is_empty() { None } else { Some(reason_str.as_str()) },
        })
        .to_string();

        sqlx::query(
            "INSERT INTO activity_log
                (id, task_id, actor_id, actor_role, event_type, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(&activity_id)
        .bind(id)
        .bind(actor_id)
        .bind(actor_role)
        .bind(apply.activity_type)
        .bind(&payload)
        .execute(&mut *tx)
        .await?;

        let task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;

        if task.status == "closed" {
            self.maybe_queue_epic_review_batch(&task).await?;
        }

        let _ = self.events.send(DjinnEvent::TaskUpdated(task.clone()));

        // Blocker resolution: when a task reaches post-merge/closed states, notify any tasks
        // it was blocking that are now fully unblocked, so coordinators can dispatch them.
        if task.status == "closed" {
            self.emit_unblocked_tasks(&task.id).await?;
        }

        Ok(task)
    }

    /// Raw status update, bypassing state machine validation.
    ///
    /// Only used for tests and admin tooling. Production code should use `transition`.
    pub async fn set_status(&self, id: &str, status: &str) -> Result<Task> {
        self.db.ensure_initialized().await?;
        let closed_at_sql = if status == "closed" {
            "closed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),"
        } else {
            ""
        };
        let reopen_inc: i64 = if status == "open" { 1 } else { 0 };
        sqlx::query(&format!(
            "UPDATE tasks SET status = ?2, {closed_at_sql}
                reopen_count = reopen_count + ?3,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1"
        ))
        .bind(id)
        .bind(status)
        .bind(reopen_inc)
        .execute(self.db.pool())
        .await?;
        let task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(self.db.pool())
            .await?;

        let _ = self.events.send(DjinnEvent::TaskUpdated(task.clone()));
        Ok(task)
    }

    /// Move a task to a different epic (or detach from epic with None).
    pub async fn move_to_epic(&self, id: &str, new_epic_id: Option<&str>) -> Result<Task> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE tasks SET epic_id = ?2,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(id)
        .bind(new_epic_id)
        .execute(self.db.pool())
        .await?;
        let task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(self.db.pool())
            .await?;

        if let Some(epic_id) = new_epic_id {
            let epic_repo = EpicRepository::new(self.db.clone(), self.events.clone());
            if let Some(epic) = epic_repo.get(epic_id).await?
                && (epic.status == "closed" || epic.status == "in_review")
            {
                let _ = epic_repo.reopen(epic_id).await?;
            }
        }

        let _ = self.events.send(DjinnEvent::TaskUpdated(task.clone()));
        Ok(task)
    }

    pub(super) async fn maybe_queue_epic_review_batch(&self, task: &Task) -> Result<()> {
        let Some(epic_id) = task.epic_id.as_deref() else {
            return Ok(());
        };

        let epic_repo = EpicRepository::new(self.db.clone(), self.events.clone());
        let Some(epic) = epic_repo.get(epic_id).await? else {
            return Ok(());
        };

        let tasks = self.list_by_epic(epic_id).await?;
        if tasks.is_empty() {
            return Ok(());
        }
        let all_closed = tasks.iter().all(|t| t.status == "closed");
        if !all_closed {
            return Ok(());
        }

        let batch_repo = EpicReviewBatchRepository::new(self.db.clone(), self.events.clone());
        if batch_repo.has_active_batch(epic_id).await? {
            if epic.status != "in_review" {
                let _ = epic_repo.mark_in_review(epic_id).await?;
            }
            return Ok(());
        }

        let reviewable_task_ids = batch_repo.list_unreviewed_closed_task_ids(epic_id).await?;
        if reviewable_task_ids.is_empty() {
            if epic.status == "in_review" {
                let _ = epic_repo.close(epic_id).await?;
            }
            return Ok(());
        }

        if epic.status != "in_review" {
            let _ = epic_repo.mark_in_review(epic_id).await?;
        }

        let _batch = batch_repo
            .create_batch(&task.project_id, epic_id, &reviewable_task_ids)
            .await?;

        Ok(())
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("DELETE FROM tasks WHERE id = ?1")
            .bind(id)
            .execute(self.db.pool())
            .await?;

        let _ = self
            .events
            .send(DjinnEvent::TaskDeleted { id: id.to_owned() });
        Ok(())
    }

    /// Resolve a task by UUID or short_id.
    pub async fn resolve(&self, id_or_short: &str) -> Result<Option<Task>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Task>(
            "SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, memory_refs
             FROM tasks WHERE id = ?1 OR short_id = ?1",
        )
        .bind(id_or_short)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn resolve_in_project(
        &self,
        project_id: &str,
        id_or_short: &str,
    ) -> Result<Option<Task>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Task>(
            "SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, memory_refs
             FROM tasks WHERE project_id = ?1 AND (id = ?2 OR short_id = ?2)",
        )
        .bind(project_id)
        .bind(id_or_short)
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Store the squash-merge commit SHA for a task after merge completes.
    pub async fn set_merge_commit_sha(&self, id: &str, sha: &str) -> Result<Task> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE tasks SET merge_commit_sha = ?2,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(id)
        .bind(sha)
        .execute(self.db.pool())
        .await?;

        let task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(self.db.pool())
            .await?;

        let _ = self.events.send(DjinnEvent::TaskUpdated(task.clone()));
        Ok(task)
    }

    /// Replace the `memory_refs` JSON array on a task.
    pub async fn update_memory_refs(&self, id: &str, memory_refs_json: &str) -> Result<Task> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE tasks SET memory_refs = ?2,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(id)
        .bind(memory_refs_json)
        .execute(self.db.pool())
        .await?;
        let task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(self.db.pool())
            .await?;

        let _ = self.events.send(DjinnEvent::TaskUpdated(task.clone()));
        Ok(task)
    }

    /// Find tasks whose `memory_refs` JSON array contains the given permalink.
    ///
    /// Uses a LIKE query on the JSON text — fast enough for the expected table
    /// sizes and avoids requiring a json_each virtual table scan.
    pub async fn list_by_memory_ref(&self, permalink: &str) -> Result<Vec<Task>> {
        let pattern = format!("%\"{permalink}\"%");
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Task>(
            "SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, memory_refs
             FROM tasks WHERE memory_refs LIKE ?1
             ORDER BY priority, created_at",
        )
        .bind(&pattern)
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Upsert a task received from a peer sync (last-writer-wins by updated_at).
    ///
    /// Returns `true` if the row was inserted or updated, `false` if:
    ///   - The task's `epic_id` doesn't exist locally (FK constraint).
    ///   - The local copy is already newer or equal (LWW check).
    ///   - Any other constraint violation (UNIQUE on short_id, CHECK, etc.).
    pub async fn upsert_peer(&self, task: &Task) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        // Verify epic exists before INSERT when task references one.
        if let Some(epic_id) = &task.epic_id {
            let epic_exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM epics WHERE id = ?1")
                .bind(epic_id)
                .fetch_one(&mut *tx)
                .await?;
            if epic_exists == 0 {
                tx.commit().await?;
                return Ok(false);
            }
        }

        let changed = match sqlx::query(
            "INSERT INTO tasks (
                id, project_id, short_id, epic_id, title, description, design,
                issue_type, status, priority, owner, labels,
                acceptance_criteria, reopen_count, continuation_count,
                created_at, updated_at, closed_at,
                close_reason, merge_commit_sha, memory_refs
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21
             )
             ON CONFLICT(id) DO UPDATE SET
                project_id          = excluded.project_id,
                title               = excluded.title,
                description         = excluded.description,
                design              = excluded.design,
                issue_type          = excluded.issue_type,
                status              = excluded.status,
                priority            = excluded.priority,
                owner               = excluded.owner,
                labels              = excluded.labels,
                acceptance_criteria = excluded.acceptance_criteria,
                reopen_count        = excluded.reopen_count,
                continuation_count  = excluded.continuation_count,
                updated_at          = excluded.updated_at,
                closed_at           = excluded.closed_at,
                close_reason        = excluded.close_reason,
                merge_commit_sha    = excluded.merge_commit_sha,
                memory_refs         = excluded.memory_refs
             WHERE excluded.updated_at > tasks.updated_at",
        )
        .bind(&task.id)
        .bind(&task.project_id)
        .bind(&task.short_id)
        .bind(&task.epic_id)
        .bind(&task.title)
        .bind(&task.description)
        .bind(&task.design)
        .bind(&task.issue_type)
        .bind(&task.status)
        .bind(task.priority)
        .bind(&task.owner)
        .bind(&task.labels)
        .bind(&task.acceptance_criteria)
        .bind(task.reopen_count)
        .bind(task.continuation_count)
        .bind(&task.created_at)
        .bind(&task.updated_at)
        .bind(&task.closed_at)
        .bind(&task.close_reason)
        .bind(&task.merge_commit_sha)
        .bind(&task.memory_refs)
        .execute(&mut *tx)
        .await
        {
            Ok(result) => result.rows_affected() > 0,
            Err(sqlx::Error::Database(db_err)) if is_constraint_violation(db_err.as_ref()) => false,
            Err(e) => return Err(Error::Database(e)),
        };
        tx.commit().await?;

        if changed {
            if let Ok(Some(updated)) = self.get(&task.id).await {
                let _ = self.events.send(DjinnEvent::TaskUpdated(updated));
            }
        }
        Ok(changed)
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
            reopen_count, continuation_count, created_at, updated_at, closed_at,
            close_reason, merge_commit_sha, memory_refs
     FROM tasks WHERE id = ?1";

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

pub(super) async fn short_id_exists(pool: &SqlitePool, table: &str, short_id: &str) -> Result<bool> {
    let sql = format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE short_id = ?1)");
    Ok(sqlx::query_scalar::<_, i64>(&sql)
        .bind(short_id)
        .fetch_one(pool)
        .await?
        > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repositories::epic::EpicRepository;
    use crate::models::task::{TaskStatus, TransitionAction};
    use crate::test_helpers;

    async fn make_epic(
        db: &Database,
        tx: broadcast::Sender<DjinnEvent>,
    ) -> crate::models::epic::Epic {
        EpicRepository::new(db.clone(), tx)
            .create("Test Epic", "", "", "", "")
            .await
            .unwrap()
    }

    async fn open_task(repo: &TaskRepository, epic_id: &str) -> Task {
        repo.create(epic_id, "T", "", "", "task", 0, "")
            .await
            .unwrap()
    }

    // ── Existing tests ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn create_and_get_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = repo
            .create(&epic.id, "My Task", "", "", "task", 0, "user@example.com")
            .await
            .unwrap();
        assert_eq!(task.title, "My Task");
        assert_eq!(task.status, "open");
        assert_eq!(task.short_id.len(), 4);

        let fetched = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(fetched.title, "My Task");
    }

    #[tokio::test]
    async fn creating_task_reopens_closed_epic() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic_repo = EpicRepository::new(db.clone(), tx.clone());
        let epic = epic_repo.create("Test Epic", "", "", "", "").await.unwrap();
        epic_repo.close(&epic.id).await.unwrap();

        let repo = TaskRepository::new(db.clone(), tx);
        let _task = repo
            .create(&epic.id, "New Task", "", "", "task", 0, "")
            .await
            .unwrap();

        let reopened = epic_repo.get(&epic.id).await.unwrap().unwrap();
        assert_eq!(reopened.status, "open");
        assert!(reopened.closed_at.is_none());
    }

    #[tokio::test]
    async fn short_id_lookup() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = repo
            .create(&epic.id, "T", "", "", "task", 0, "")
            .await
            .unwrap();
        let found = repo.get_by_short_id(&task.short_id).await.unwrap().unwrap();
        assert_eq!(found.id, task.id);
    }

    #[tokio::test]
    async fn create_emits_event() {
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let _ = rx.recv().await.unwrap(); // consume EpicCreated
        let repo = TaskRepository::new(db, tx);

        repo.create(&epic.id, "Event Task", "", "", "task", 0, "")
            .await
            .unwrap();
        match rx.recv().await.unwrap() {
            DjinnEvent::TaskCreated(t) => assert_eq!(t.title, "Event Task"),
            _ => panic!("expected TaskCreated"),
        }
    }

    #[tokio::test]
    async fn set_status_transitions() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = repo
            .create(&epic.id, "T", "", "", "task", 0, "")
            .await
            .unwrap();
        let updated = repo.set_status(&task.id, "in_progress").await.unwrap();
        assert_eq!(updated.status, "in_progress");

        let closed = repo.set_status(&task.id, "closed").await.unwrap();
        assert_eq!(closed.status, "closed");
        assert!(closed.closed_at.is_some());
    }

    #[tokio::test]
    async fn reopen_increments_counter() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = repo
            .create(&epic.id, "T", "", "", "task", 0, "")
            .await
            .unwrap();
        repo.set_status(&task.id, "closed").await.unwrap();
        let reopened = repo.set_status(&task.id, "open").await.unwrap();
        assert_eq!(reopened.reopen_count, 1);
    }

    #[tokio::test]
    async fn blocker_management() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let t1 = repo
            .create(&epic.id, "T1", "", "", "task", 0, "")
            .await
            .unwrap();
        let t2 = repo
            .create(&epic.id, "T2", "", "", "task", 1, "")
            .await
            .unwrap();

        // add blocker: t2 is blocked by t1
        repo.add_blocker(&t2.id, &t1.id).await.unwrap();
        let blockers = repo.list_blockers(&t2.id).await.unwrap();
        assert_eq!(blockers.len(), 1);
        assert_eq!(blockers[0].task_id, t1.id);
        assert_eq!(blockers[0].status, "open");
        assert!(!matches!(blockers[0].status.as_str(), "closed"));

        // inverse: t1 blocks t2
        let blocked = repo.list_blocked_by(&t1.id).await.unwrap();
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].task_id, t2.id);

        // self-loop rejected
        assert!(repo.add_blocker(&t1.id, &t1.id).await.is_err());

        // remove blocker
        repo.remove_blocker(&t2.id, &t1.id).await.unwrap();
        assert!(repo.list_blockers(&t2.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn blocker_cycle_detection() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let t1 = repo
            .create(&epic.id, "T1", "", "", "task", 0, "")
            .await
            .unwrap();
        let t2 = repo
            .create(&epic.id, "T2", "", "", "task", 1, "")
            .await
            .unwrap();
        let t3 = repo
            .create(&epic.id, "T3", "", "", "task", 2, "")
            .await
            .unwrap();

        // t2 is blocked by t1; t3 is blocked by t2
        repo.add_blocker(&t2.id, &t1.id).await.unwrap();
        repo.add_blocker(&t3.id, &t2.id).await.unwrap();

        // Adding t1 blocked by t3 would create a cycle: t1 → t2 → t3 → t1
        let result = repo.add_blocker(&t1.id, &t3.id).await;
        assert!(result.is_err(), "expected cycle detection to reject this");
    }

    #[tokio::test]
    async fn start_blocked_by_unresolved_blocker() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let t1 = repo
            .create(&epic.id, "T1", "", "", "task", 0, "")
            .await
            .unwrap();
        let t2 = repo
            .create(&epic.id, "T2", "", "", "task", 1, "")
            .await
            .unwrap();

        // t2 blocked by t1 (which is open = unresolved)
        repo.add_blocker(&t2.id, &t1.id).await.unwrap();
        let result = repo
            .transition(&t2.id, TransitionAction::Start, "", "system", None, None)
            .await;
        assert!(result.is_err(), "should not start with unresolved blocker");

        // Close t1 → t2 should now be startable
        repo.set_status(&t1.id, "closed").await.unwrap();
        repo.transition(&t2.id, TransitionAction::Start, "", "system", None, None)
            .await
            .expect("should start after blocker resolved");
    }

    #[tokio::test]
    async fn list_ready_excludes_blocked_tasks() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let t1 = repo
            .create(&epic.id, "T1", "", "", "task", 0, "")
            .await
            .unwrap();
        let t2 = repo
            .create(&epic.id, "T2", "", "", "task", 1, "")
            .await
            .unwrap();

        // t2 blocked by t1
        repo.add_blocker(&t2.id, &t1.id).await.unwrap();

        let ready = repo.list_ready(ReadyQuery::default()).await.unwrap();
        let ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
        assert!(ids.contains(&t1.id.as_str()), "t1 should be ready");
        assert!(
            !ids.contains(&t2.id.as_str()),
            "t2 should not be ready (blocked)"
        );

        // Close t1 → t2 becomes ready
        repo.set_status(&t1.id, "closed").await.unwrap();
        let ready2 = repo.list_ready(ReadyQuery::default()).await.unwrap();
        let ids2: Vec<&str> = ready2.iter().map(|t| t.id.as_str()).collect();
        assert!(
            ids2.contains(&t2.id.as_str()),
            "t2 should be ready after t1 closed"
        );
    }

    #[tokio::test]
    async fn activity_log() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = repo
            .create(&epic.id, "T", "", "", "task", 0, "")
            .await
            .unwrap();
        repo.log_activity(
            Some(&task.id),
            "user@example.com",
            "user",
            "comment",
            r#"{"body":"hello"}"#,
        )
        .await
        .unwrap();

        let entries = repo.list_activity(&task.id).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event_type, "comment");
        assert_eq!(entries[0].task_id.as_deref(), Some(task.id.as_str()));
    }

    #[tokio::test]
    async fn list_by_epic() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        repo.create(&epic.id, "A", "", "", "task", 1, "")
            .await
            .unwrap();
        repo.create(&epic.id, "B", "", "", "feature", 0, "")
            .await
            .unwrap();

        let tasks = repo.list_by_epic(&epic.id).await.unwrap();
        assert_eq!(tasks.len(), 2);
        // Ordered by priority then created_at — B (priority 0) first.
        assert_eq!(tasks[0].title, "B");
    }

    #[tokio::test]
    async fn delete_task_emits_event() {
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let _ = rx.recv().await.unwrap();
        let repo = TaskRepository::new(db, tx);

        let task = repo
            .create(&epic.id, "Del", "", "", "task", 0, "")
            .await
            .unwrap();
        let _ = rx.recv().await.unwrap();

        repo.delete(&task.id).await.unwrap();
        match rx.recv().await.unwrap() {
            DjinnEvent::TaskDeleted { id } => assert_eq!(id, task.id),
            _ => panic!("expected TaskDeleted"),
        }
    }

    // ── State machine tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn status_enum_roundtrips() {
        let statuses = [
            "draft",
            "open",
            "in_progress",
            "needs_task_review",
            "in_task_review",
            "closed",
        ];
        for s in statuses {
            let parsed = TaskStatus::parse(s).unwrap();
            assert_eq!(parsed.as_str(), s, "round-trip failed for {s}");
        }
        assert!(TaskStatus::parse("unknown").is_err());
    }

    #[tokio::test]
    async fn full_happy_path() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        // Tasks are created as "open".
        let task = open_task(&repo, &epic.id).await;
        assert_eq!(task.status, "open");

        // start
        let t = repo
            .transition(&task.id, TransitionAction::Start, "", "system", None, None)
            .await
            .unwrap();
        assert_eq!(t.status, "in_progress");

        // submit_task_review
        let t = repo
            .transition(
                &t.id,
                TransitionAction::SubmitTaskReview,
                "",
                "system",
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(t.status, "needs_task_review");

        // task_review_start
        let t = repo
            .transition(
                &t.id,
                TransitionAction::TaskReviewStart,
                "",
                "task_reviewer",
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(t.status, "in_task_review");

        // task_review_approve closes the task.
        let t = repo
            .transition(
                &t.id,
                TransitionAction::TaskReviewApprove,
                "",
                "task_reviewer",
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(t.status, "closed");
        assert!(t.closed_at.is_some());
        assert_eq!(t.close_reason.as_deref(), Some("completed"));
    }

    #[tokio::test]
    async fn invalid_transition_returns_error() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = open_task(&repo, &epic.id).await;

        // Can't submit_task_review from open (must be in_progress).
        let err = repo
            .transition(
                &task.id,
                TransitionAction::SubmitTaskReview,
                "",
                "system",
                None,
                None,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::InvalidTransition(_)),
            "expected InvalidTransition, got {err:?}"
        );

        // Can't accept from open (must be draft).
        let err = repo
            .transition(&task.id, TransitionAction::Accept, "", "system", None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidTransition(_)));
    }

    #[tokio::test]
    async fn task_review_reject_increments_reopen() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = open_task(&repo, &epic.id).await;
        let t = repo
            .transition(&task.id, TransitionAction::Start, "", "system", None, None)
            .await
            .unwrap();
        let t = repo
            .transition(
                &t.id,
                TransitionAction::SubmitTaskReview,
                "",
                "system",
                None,
                None,
            )
            .await
            .unwrap();
        let t = repo
            .transition(
                &t.id,
                TransitionAction::TaskReviewStart,
                "",
                "task_reviewer",
                None,
                None,
            )
            .await
            .unwrap();

        let t = repo
            .transition(
                &t.id,
                TransitionAction::TaskReviewReject,
                "reviewer@example.com",
                "task_reviewer",
                Some("needs more tests"),
                None,
            )
            .await
            .unwrap();

        assert_eq!(t.status, "open");
        assert_eq!(t.reopen_count, 1);
        assert_eq!(t.continuation_count, 0);
    }

    #[tokio::test]
    async fn task_review_reject_conflict_does_not_increment_reopen() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = open_task(&repo, &epic.id).await;
        let t = repo
            .transition(&task.id, TransitionAction::Start, "", "system", None, None)
            .await
            .unwrap();
        let t = repo
            .transition(
                &t.id,
                TransitionAction::SubmitTaskReview,
                "",
                "system",
                None,
                None,
            )
            .await
            .unwrap();
        let t = repo
            .transition(
                &t.id,
                TransitionAction::TaskReviewStart,
                "",
                "task_reviewer",
                None,
                None,
            )
            .await
            .unwrap();

        let t = repo
            .transition(
                &t.id,
                TransitionAction::TaskReviewRejectConflict,
                "reviewer@example.com",
                "task_reviewer",
                Some("merge conflict"),
                None,
            )
            .await
            .unwrap();

        assert_eq!(t.status, "open");
        assert_eq!(t.reopen_count, 0); // conflict doesn't count against budget
        assert_eq!(t.continuation_count, 0);
    }

    #[tokio::test]
    async fn force_close_from_any_state() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = open_task(&repo, &epic.id).await;
        let t = repo
            .transition(&task.id, TransitionAction::Start, "", "system", None, None)
            .await
            .unwrap();

        let t = repo
            .transition(
                &t.id,
                TransitionAction::ForceClose,
                "admin",
                "user",
                Some("cancelled"),
                None,
            )
            .await
            .unwrap();

        assert_eq!(t.status, "closed");
        assert!(t.closed_at.is_some());
        assert_eq!(t.close_reason.as_deref(), Some("force_closed"));
    }

    #[tokio::test]
    async fn reopen_clears_closed_at_and_increments_reopen() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = open_task(&repo, &epic.id).await;
        // Force-close it directly.
        let t = repo
            .transition(
                &task.id,
                TransitionAction::ForceClose,
                "admin",
                "user",
                Some("testing"),
                None,
            )
            .await
            .unwrap();
        assert!(t.closed_at.is_some());
        assert_eq!(t.close_reason.as_deref(), Some("force_closed"));

        // Reopen.
        let t = repo
            .transition(
                &t.id,
                TransitionAction::Reopen,
                "user",
                "user",
                Some("still needed"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(t.status, "open");
        assert!(t.closed_at.is_none());
        assert!(t.close_reason.is_none());
        assert_eq!(t.reopen_count, 1);
    }

    #[tokio::test]
    async fn start_blocked_by_unresolved_blockers() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let t1 = open_task(&repo, &epic.id).await;
        let t2 = open_task(&repo, &epic.id).await;
        repo.add_blocker(&t2.id, &t1.id).await.unwrap(); // t2 blocked by t1

        let err = repo
            .transition(&t2.id, TransitionAction::Start, "", "system", None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidTransition(_)));

        // After removing the blocker, start succeeds.
        repo.remove_blocker(&t2.id, &t1.id).await.unwrap();
        let t = repo
            .transition(&t2.id, TransitionAction::Start, "", "system", None, None)
            .await
            .unwrap();
        assert_eq!(t.status, "in_progress");
    }

    #[tokio::test]
    async fn start_allowed_when_blocker_is_closed() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let blocker = open_task(&repo, &epic.id).await;
        let blocked = open_task(&repo, &epic.id).await;
        repo.add_blocker(&blocked.id, &blocker.id).await.unwrap();

        // Closed blockers are considered resolved.
        repo.set_status(&blocker.id, "closed").await.unwrap();

        let started = repo
            .transition(
                &blocked.id,
                TransitionAction::Start,
                "",
                "system",
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(started.status, "in_progress");
    }

    #[tokio::test]
    async fn user_override_to_closed() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = open_task(&repo, &epic.id).await;
        let t = repo
            .transition(
                &task.id,
                TransitionAction::UserOverride,
                "admin",
                "user",
                None,
                Some(TaskStatus::Closed),
            )
            .await
            .unwrap();

        assert_eq!(t.status, "closed");
        assert!(t.closed_at.is_some());
        assert_eq!(t.close_reason.as_deref(), Some("force_closed"));
    }

    #[tokio::test]
    async fn requires_reason_enforced() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = open_task(&repo, &epic.id).await;
        // ForceClose requires a reason.
        let err = repo
            .transition(
                &task.id,
                TransitionAction::ForceClose,
                "",
                "user",
                None,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidTransition(_)));

        // With a reason it works.
        let t = repo
            .transition(
                &task.id,
                TransitionAction::ForceClose,
                "",
                "user",
                Some("testing"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(t.status, "closed");
    }

    #[tokio::test]
    async fn transition_writes_activity_log() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = open_task(&repo, &epic.id).await;
        repo.transition(
            &task.id,
            TransitionAction::Start,
            "agent-1",
            "system",
            None,
            None,
        )
        .await
        .unwrap();

        let entries = repo.list_activity(&task.id).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event_type, "status_changed");
        assert_eq!(entries[0].actor_id, "agent-1");

        let payload: serde_json::Value = serde_json::from_str(&entries[0].payload).unwrap();
        assert_eq!(payload["from_status"], "open");
        assert_eq!(payload["to_status"], "in_progress");
    }

    #[tokio::test]
    async fn query_activity_filters() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let t1 = open_task(&repo, &epic.id).await;
        let t2 = open_task(&repo, &epic.id).await;

        // Log a comment on t1 and a status_changed on t2.
        repo.log_activity(Some(&t1.id), "u1", "user", "comment", r#"{"body":"hello"}"#)
            .await
            .unwrap();
        repo.log_activity(
            Some(&t2.id),
            "sys",
            "system",
            "status_changed",
            r#"{"from":"open"}"#,
        )
        .await
        .unwrap();

        // Filter by task_id.
        let results = repo
            .query_activity(ActivityQuery {
                task_id: Some(t1.id.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].event_type, "comment");

        // Filter by event_type across all tasks.
        let results = repo
            .query_activity(ActivityQuery {
                event_type: Some("status_changed".to_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].task_id.as_deref(), Some(t2.id.as_str()));

        // No filters — returns both.
        let all = repo.query_activity(ActivityQuery::default()).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn set_merge_commit_sha_persists_value() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = open_task(&repo, &epic.id).await;
        let updated = repo
            .set_merge_commit_sha(&task.id, "0123456789abcdef0123456789abcdef01234567")
            .await
            .unwrap();

        assert_eq!(
            updated.merge_commit_sha.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
    }

    #[tokio::test]
    async fn board_health_report() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db.clone(), tx.clone());

        // Create tasks: one open, one in_progress.
        let _t1 = open_task(&repo, &epic.id).await;
        let t2 = open_task(&repo, &epic.id).await;
        repo.transition(&t2.id, TransitionAction::Start, "", "system", None, None)
            .await
            .unwrap();

        let report = repo.board_health(24).await.unwrap();
        let epic_stats = report["epic_stats"].as_array().unwrap();
        assert_eq!(epic_stats.len(), 1);
        assert_eq!(epic_stats[0]["total"], 2);

        // Backdate t2's updated_at to simulate staleness.
        let t2_id = t2.id.clone();
        sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00.000Z' WHERE id = ?1")
            .bind(&t2_id)
            .execute(db.pool())
            .await
            .unwrap();

        let report2 = repo.board_health(24).await.unwrap();
        let stale = report2["stale_tasks"].as_array().unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0]["short_id"], t2.short_id.as_str());
    }

    #[tokio::test]
    async fn reconcile_heals_stale_tasks() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db.clone(), tx);

        let t = open_task(&repo, &epic.id).await;
        repo.transition(&t.id, TransitionAction::Start, "", "system", None, None)
            .await
            .unwrap();

        // Backdate updated_at so the task is considered stale (> 24h).
        let t_id = t.id.clone();
        sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00.000Z' WHERE id = ?1")
            .bind(&t_id)
            .execute(db.pool())
            .await
            .unwrap();

        let result = repo.reconcile(24).await.unwrap();
        assert_eq!(result["healed_tasks"], 1);

        // Task should now be open again.
        let updated = repo.resolve(&t.id).await.unwrap().unwrap();
        assert_eq!(updated.status, "open");

        // Activity log should have a reconcile_stale entry.
        let entries = repo.list_activity(&t.id).await.unwrap();
        let reconcile_entry = entries.iter().find(|e| {
            let p: serde_json::Value = serde_json::from_str(&e.payload).unwrap_or_default();
            p["reason"] == "reconcile_stale"
        });
        assert!(
            reconcile_entry.is_some(),
            "expected reconcile_stale activity entry"
        );
    }
}
