use djinn_core::models::{TaskRunRecord, TaskRunStatus};

use crate::Result;
use crate::database::Database;

pub struct TaskRunRepository {
    db: Database,
}

pub struct CreateTaskRunParams<'a> {
    pub id: &'a str,
    pub project_id: &'a str,
    pub task_id: &'a str,
    pub trigger_type: &'a str,
    /// Initial status; defaults to `"running"` when `None`.
    pub status: Option<&'a str>,
    pub workspace_path: Option<&'a str>,
    pub mirror_ref: Option<&'a str>,
}

impl TaskRunRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn create(&self, params: CreateTaskRunParams<'_>) -> Result<TaskRunRecord> {
        self.db.ensure_initialized().await?;

        let status = params.status.unwrap_or("running");
        sqlx::query!(
            "INSERT INTO task_runs
                (id, project_id, task_id, trigger_type, `status`, workspace_path, mirror_ref)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            params.id,
            params.project_id,
            params.task_id,
            params.trigger_type,
            status,
            params.workspace_path,
            params.mirror_ref,
        )
        .execute(self.db.pool())
        .await?;

        let run = sqlx::query_as!(
            TaskRunRecord,
            r#"SELECT id, project_id, task_id, trigger_type,
                `status` AS "status!", started_at, ended_at,
                workspace_path, mirror_ref
             FROM task_runs WHERE id = ?"#,
            params.id
        )
        .fetch_one(self.db.pool())
        .await?;

        Ok(run)
    }

    pub async fn get(&self, id: &str) -> Result<Option<TaskRunRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            TaskRunRecord,
            r#"SELECT id, project_id, task_id, trigger_type,
                `status` AS "status!", started_at, ended_at,
                workspace_path, mirror_ref
             FROM task_runs WHERE id = ?"#,
            id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Update the status of a run.  Terminal statuses (Completed / Failed /
    /// Interrupted) also stamp `ended_at`; the Running status leaves it NULL.
    pub async fn update_status(&self, id: &str, status: TaskRunStatus) -> Result<()> {
        self.db.ensure_initialized().await?;

        let status_str = status.as_str();
        if status.is_terminal() {
            sqlx::query!(
                "UPDATE task_runs
                 SET `status` = ?,
                     ended_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
                 WHERE id = ?",
                status_str,
                id
            )
            .execute(self.db.pool())
            .await?;
        } else {
            sqlx::query!(
                "UPDATE task_runs
                 SET `status` = ?,
                     ended_at = NULL
                 WHERE id = ?",
                status_str,
                id
            )
            .execute(self.db.pool())
            .await?;
        }

        Ok(())
    }

    pub async fn list_for_task(&self, task_id: &str) -> Result<Vec<TaskRunRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            TaskRunRecord,
            r#"SELECT id, project_id, task_id, trigger_type,
                `status` AS "status!", started_at, ended_at,
                workspace_path, mirror_ref
             FROM task_runs WHERE task_id = ? ORDER BY started_at DESC"#,
            task_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Return the most recent non-null `workspace_path` recorded for any
    /// `task_run` that belongs to the given task. Replaces the former
    /// `SessionRepository::latest_worktree_path_for_task` now that workspace
    /// lifetime is owned by `task_runs` rather than `sessions`.
    pub async fn latest_workspace_path_for_task(
        &self,
        task_id: &str,
    ) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;

        let row: Option<Option<String>> = sqlx::query_scalar!(
            "SELECT workspace_path FROM task_runs
             WHERE task_id = ? AND workspace_path IS NOT NULL
             ORDER BY started_at DESC LIMIT 1",
            task_id
        )
        .fetch_optional(self.db.pool())
        .await?;

        Ok(row.flatten())
    }
}

#[cfg(test)]
mod tests {
    use djinn_core::events::EventBus;
    use djinn_core::models::{TaskRunStatus, TaskRunTrigger};

    use super::*;
    use crate::repositories::epic::EpicRepository;

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    /// Create a task via raw SQL (no TaskRepository dep), returns (project_id, task_id).
    async fn create_task(db: &Database, bus: EventBus) -> (String, String) {
        let epic_repo = EpicRepository::new(db.clone(), bus);
        let epic = epic_repo
            .create("Epic", "", "", "", "", None)
            .await
            .unwrap();

        let task_id = uuid::Uuid::now_v7().to_string();
        let short_id = format!("t{}{}", &task_id[..6], &task_id[task_id.len() - 6..]);
        sqlx::query!(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                                issue_type, priority, owner, `status`, continuation_count, labels, acceptance_criteria, memory_refs)
             VALUES (?, ?, ?, ?, 'Task', '', '', 'task', 0, '', 'open', 0, '[]', '[]', '[]')",
            task_id,
            epic.project_id,
            short_id,
            epic.id
        )
        .execute(db.pool())
        .await
        .unwrap();

        (epic.project_id, task_id)
    }

    fn new_run_id() -> String {
        uuid::Uuid::now_v7().to_string()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_persists_defaults_and_returns_record() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskRunRepository::new(db);

        let id = new_run_id();
        let run = repo
            .create(CreateTaskRunParams {
                id: &id,
                project_id: &project_id,
                task_id: &task_id,
                trigger_type: TaskRunTrigger::NewTask.as_str(),
                status: None,
                workspace_path: Some("/tmp/djinn-workspace"),
                mirror_ref: Some("refs/djinn/runs/abc"),
            })
            .await
            .unwrap();

        assert_eq!(run.id, id);
        assert_eq!(run.project_id, project_id);
        assert_eq!(run.task_id, task_id);
        assert_eq!(run.trigger_type, TaskRunTrigger::NewTask.as_str());
        assert_eq!(run.status, TaskRunStatus::Running.as_str());
        assert!(
            run.ended_at.is_none(),
            "new runs must not have ended_at set"
        );
        assert!(
            !run.started_at.is_empty(),
            "started_at should be populated by the DB default"
        );
        assert_eq!(run.workspace_path.as_deref(), Some("/tmp/djinn-workspace"));
        assert_eq!(run.mirror_ref.as_deref(), Some("refs/djinn/runs/abc"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_returns_none_for_missing_id() {
        let db = test_db();
        let repo = TaskRunRepository::new(db);
        let missing = repo.get("00000000-0000-0000-0000-000000000000").await.unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_fetches_created_row() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskRunRepository::new(db);

        let id = new_run_id();
        let created = repo
            .create(CreateTaskRunParams {
                id: &id,
                project_id: &project_id,
                task_id: &task_id,
                trigger_type: TaskRunTrigger::ConflictRetry.as_str(),
                status: None,
                workspace_path: None,
                mirror_ref: None,
            })
            .await
            .unwrap();

        let fetched = repo.get(&id).await.unwrap().expect("row must exist");
        assert_eq!(fetched.id, created.id);
        assert_eq!(fetched.trigger_type, "conflict_retry");
        assert!(fetched.workspace_path.is_none());
        assert!(fetched.mirror_ref.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_status_stamps_ended_at_only_for_terminal() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskRunRepository::new(db);

        // Running → stays open.
        let running_id = new_run_id();
        repo.create(CreateTaskRunParams {
            id: &running_id,
            project_id: &project_id,
            task_id: &task_id,
            trigger_type: TaskRunTrigger::NewTask.as_str(),
            status: None,
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();
        repo.update_status(&running_id, TaskRunStatus::Running)
            .await
            .unwrap();
        let still_running = repo.get(&running_id).await.unwrap().unwrap();
        assert_eq!(still_running.status, "running");
        assert!(
            still_running.ended_at.is_none(),
            "running runs must not have ended_at"
        );

        // Each terminal variant stamps ended_at.
        for terminal in [
            TaskRunStatus::Completed,
            TaskRunStatus::Failed,
            TaskRunStatus::Interrupted,
        ] {
            let id = new_run_id();
            repo.create(CreateTaskRunParams {
                id: &id,
                project_id: &project_id,
                task_id: &task_id,
                trigger_type: TaskRunTrigger::NewTask.as_str(),
                status: None,
                workspace_path: None,
                mirror_ref: None,
            })
            .await
            .unwrap();

            repo.update_status(&id, terminal).await.unwrap();
            let after = repo.get(&id).await.unwrap().unwrap();
            assert_eq!(after.status, terminal.as_str());
            assert!(
                after.ended_at.is_some(),
                "terminal status {terminal:?} should stamp ended_at",
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_for_task_returns_descending_by_started_at() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let (other_project_id, other_task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskRunRepository::new(db);

        // Three runs on target task, one on an unrelated task.
        let mut ids: Vec<String> = Vec::new();
        for trigger in [
            TaskRunTrigger::NewTask,
            TaskRunTrigger::ConflictRetry,
            TaskRunTrigger::ReviewResponse,
        ] {
            let id = new_run_id();
            repo.create(CreateTaskRunParams {
                id: &id,
                project_id: &project_id,
                task_id: &task_id,
                trigger_type: trigger.as_str(),
                status: None,
                workspace_path: None,
                mirror_ref: None,
            })
            .await
            .unwrap();
            ids.push(id);
            // Small stagger so started_at ordering is deterministic even at
            // millisecond granularity.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // Noise row on a different task — must NOT appear in results.
        let noise_id = new_run_id();
        repo.create(CreateTaskRunParams {
            id: &noise_id,
            project_id: &other_project_id,
            task_id: &other_task_id,
            trigger_type: TaskRunTrigger::NewTask.as_str(),
            status: None,
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

        let runs = repo.list_for_task(&task_id).await.unwrap();
        assert_eq!(runs.len(), 3);
        // Newest-first ordering → the last id we inserted should be first.
        assert_eq!(runs[0].id, ids[2]);
        assert_eq!(runs[2].id, ids[0]);
        for run in &runs {
            assert_eq!(run.task_id, task_id);
            assert_ne!(run.id, noise_id);
        }
    }
}
