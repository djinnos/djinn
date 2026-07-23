use djinn_core::models::{TaskRunRecord, TaskRunStatus};

use crate::Result;
use crate::database::Database;
use crate::error::DbError;
use sqlx::Row;
use uuid::Uuid;

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
    pub dispatch_group_id: Option<&'a str>,
}

impl TaskRunRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn create(&self, params: CreateTaskRunParams<'_>) -> Result<TaskRunRecord> {
        self.db.ensure_initialized().await?;

        let status = params.status.unwrap_or("running");
        if let Some(group_id) = params.dispatch_group_id {
            Uuid::parse_str(group_id)
                .map_err(|_| DbError::InvalidData("dispatch_group_id must be a UUID".to_owned()))?;
        }
        // Runtime query: `catalog_image_id` evolves with the task-run schema
        // and must not require regenerating sqlx offline metadata.
        sqlx::query(
            "INSERT INTO task_runs
                (id, project_id, task_id, trigger_type, status, workspace_path, mirror_ref, dispatch_group_id, catalog_image_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8,
                (SELECT selected_image_id FROM projects WHERE id = $2))",
        )
        .bind(params.id)
        .bind(params.project_id)
        .bind(params.task_id)
        .bind(params.trigger_type)
        .bind(status)
        .bind(params.workspace_path)
        .bind(params.mirror_ref)
        .bind(params.dispatch_group_id)
        .execute(self.db.pool())
        .await?;

        let run = sqlx::query_as!(
            TaskRunRecord,
            r#"SELECT id, project_id, task_id, trigger_type,
                status AS "status!", started_at, ended_at,
                workspace_path, mirror_ref, dispatch_group_id
             FROM task_runs WHERE id = $1"#,
            params.id
        )
        .fetch_one(self.db.pool())
        .await?;

        Ok(run)
    }

    /// Immutable catalog image selected at dispatch. NULL is a legacy or
    /// non-catalog dispatch and must not be substituted with current project state.
    pub async fn catalog_image_id(&self, id: &str) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query("SELECT catalog_image_id FROM task_runs WHERE id = $1")
            .bind(id).fetch_optional(self.db.pool()).await?;
        Ok(row.and_then(|row| row.try_get("catalog_image_id").ok().flatten()))
    }

    pub async fn get(&self, id: &str) -> Result<Option<TaskRunRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            TaskRunRecord,
            r#"SELECT id, project_id, task_id, trigger_type,
                status AS "status!", started_at, ended_at,
                workspace_path, mirror_ref, dispatch_group_id
             FROM task_runs WHERE id = $1"#,
            id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Every recorded workspace is migration inventory, not just a live
    /// workspace: terminal runs can retain a legacy checkout on disk.
    pub async fn workspace_paths_for_project(&self, project_id: &str) -> Result<Vec<String>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT workspace_path FROM task_runs
             WHERE project_id = $1 AND workspace_path IS NOT NULL
             ORDER BY started_at",
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Update the status of a run.  Terminal statuses (Completed / Failed /
    /// Interrupted) also stamp `ended_at`; the Running status leaves it NULL.
    pub async fn update_status(&self, id: &str, status: TaskRunStatus) -> Result<()> {
        self.db.ensure_initialized().await?;

        let status_str = status.as_str();
        if status.is_terminal() {
            sqlx::query!(
                r#"UPDATE task_runs
                 SET status = $1,
                     ended_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                 WHERE id = $2"#,
                status_str,
                id
            )
            .execute(self.db.pool())
            .await?;
        } else {
            sqlx::query!(
                "UPDATE task_runs
                 SET status = $1,
                     ended_at = NULL
                 WHERE id = $2",
                status_str,
                id
            )
            .execute(self.db.pool())
            .await?;
        }

        Ok(())
    }

    /// Record the workspace path for a run once it is known.
    ///
    /// K8s pod runs are created by the coordinator with `workspace_path =
    /// NULL` because the in-pod supervisor clones its ephemeral workspace
    /// after dispatch. The in-pod worker calls this on its first
    /// `execute_stage` so consumers that resolve the run's worktree from the
    /// row (final-verification resolution, auto-submit diff fingerprinting,
    /// rejected-submission integrity) see the real path instead of NULL.
    pub async fn set_workspace_path(&self, id: &str, workspace_path: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        // The first in-pod stage owns this value. Keep it stable if a later
        // stage (or a retry racing after the first write) has another clone.
        let result = sqlx::query(
            "UPDATE task_runs
             SET workspace_path = COALESCE(workspace_path, $2)
             WHERE id = $1",
        )
        .bind(id)
        .bind(workspace_path)
        .execute(self.db.pool())
        .await?;
        if result.rows_affected() != 1 {
            return Err(crate::Error::Internal(format!(
                "task run {id} does not exist while recording workspace path"
            )));
        }
        Ok(())
    }

    pub async fn list_for_task(&self, task_id: &str) -> Result<Vec<TaskRunRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            TaskRunRecord,
            r#"SELECT id, project_id, task_id, trigger_type,
                status AS "status!", started_at, ended_at,
                workspace_path, mirror_ref, dispatch_group_id
             FROM task_runs WHERE task_id = $1 ORDER BY started_at DESC"#,
            task_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Flip the most-recent `running` row for `task_id` to `terminal_status`,
    /// stamping `ended_at`. No-op if no such row exists.
    ///
    /// Called from the host-side teardown path after a K8s task-run finishes:
    /// when the in-pod supervisor dies before sending its terminal
    /// `update_task_run_status` RPC (OOM, eviction, SIGKILL past the
    /// terminationGracePeriod), the row would otherwise stay `running` forever.
    /// Operates on the most-recent row because slot-level serialization
    /// guarantees at most one in-flight run per task at a time.
    ///
    /// Returns the id of the row that was flipped, if any.
    pub async fn reap_running_for_task(
        &self,
        task_id: &str,
        terminal_status: TaskRunStatus,
    ) -> Result<Option<String>> {
        if !terminal_status.is_terminal() {
            return Ok(None);
        }
        self.db.ensure_initialized().await?;

        // Both live states (`starting` pre-session, `running` post-session)
        // are reapable: a run wedged in stage-init before its first session
        // must be flipped terminal too, not just a `running` one.
        let row: Option<String> = sqlx::query_scalar!(
            "SELECT id FROM task_runs
             WHERE task_id = $1 AND status IN ('starting', 'running') AND ended_at IS NULL
             ORDER BY started_at DESC LIMIT 1",
            task_id
        )
        .fetch_optional(self.db.pool())
        .await?;

        if let Some(ref id) = row {
            self.update_status(id, terminal_status).await?;
        }
        Ok(row)
    }

    /// Flip every `running` row whose `started_at` is older than
    /// `stale_threshold` to `Interrupted`, stamping `ended_at`.
    ///
    /// Returns the ids of the rows that were flipped.
    ///
    /// Used by the coordinator's periodic sweep as a safety net for runs whose
    /// pod was terminated without flushing a terminal RPC (and the per-task
    /// teardown reap missed for any reason). The threshold should be larger
    /// than the K8s Job `activeDeadlineSeconds` + termination grace so we
    /// never reap a still-live run.
    pub async fn reap_stale_running(&self, stale_threshold_iso: &str) -> Result<Vec<String>> {
        self.db.ensure_initialized().await?;

        let ids: Vec<String> = sqlx::query_scalar!(
            "SELECT id FROM task_runs
             WHERE status IN ('starting', 'running')
               AND ended_at IS NULL
               AND started_at < $1",
            stale_threshold_iso
        )
        .fetch_all(self.db.pool())
        .await?;

        for id in &ids {
            self.update_status(id, TaskRunStatus::Interrupted).await?;
        }
        Ok(ids)
    }

    /// Return the most recent non-null `workspace_path` recorded for any
    /// `task_run` that belongs to the given task. Replaces the former
    /// `SessionRepository::latest_worktree_path_for_task` now that workspace
    /// lifetime is owned by `task_runs` rather than `sessions`.
    pub async fn latest_workspace_path_for_task(&self, task_id: &str) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;

        let row: Option<Option<String>> = sqlx::query_scalar!(
            "SELECT workspace_path FROM task_runs
             WHERE task_id = $1 AND workspace_path IS NOT NULL
             ORDER BY started_at DESC LIMIT 1",
            task_id
        )
        .fetch_optional(self.db.pool())
        .await?;

        Ok(row.flatten())
    }

    /// Latest live `starting` (pre-session) run for a task, if any.
    ///
    /// A run holds `starting` from dispatch until its first reply-loop session
    /// is created (which flips it to `running`). `task_show`/`task_list` surface
    /// this as the task's active state so the UI renders the real "starting"
    /// status instead of inferring a "setting up" pseudo-status from a missing
    /// session.
    pub async fn latest_starting_for_task(&self, task_id: &str) -> Result<Option<TaskRunRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            TaskRunRecord,
            r#"SELECT id, project_id, task_id, trigger_type,
                status AS "status!", started_at, ended_at,
                workspace_path, mirror_ref, dispatch_group_id
             FROM task_runs
             WHERE task_id = $1 AND status = 'starting' AND ended_at IS NULL
             ORDER BY started_at DESC LIMIT 1"#,
            task_id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Batch variant of [`latest_starting_for_task`]: the latest live
    /// `starting` run per task id, keyed by task id. Tasks with no starting run
    /// are absent from the map. Used by `task_list` to avoid an N+1 query.
    pub async fn latest_starting_by_tasks(
        &self,
        task_ids: &[&str],
    ) -> Result<std::collections::HashMap<String, TaskRunRecord>> {
        if task_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        self.db.ensure_initialized().await?;
        let owned: Vec<String> = task_ids.iter().map(|s| (*s).to_string()).collect();
        // DISTINCT ON (task_id) with the ORDER BY picks the newest starting run
        // for each task in one pass.
        let rows = sqlx::query_as!(
            TaskRunRecord,
            r#"SELECT DISTINCT ON (task_id)
                id, project_id, task_id, trigger_type,
                status AS "status!", started_at, ended_at,
                workspace_path, mirror_ref, dispatch_group_id
             FROM task_runs
             WHERE task_id = ANY($1) AND status = 'starting' AND ended_at IS NULL
             ORDER BY task_id, started_at DESC"#,
            &owned
        )
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows.into_iter().map(|r| (r.task_id.clone(), r)).collect())
    }

    /// Return IDs of task-runs that are still `running` with `ended_at IS NULL`.
    ///
    /// Used by the coordinator's cargo-target-run-dir sweep to build the set
    /// of "protected" run directories that must not be cleaned up.
    pub async fn running_ids(&self) -> Result<Vec<String>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_scalar(
            "SELECT id FROM task_runs WHERE status IN ('starting', 'running') AND ended_at IS NULL",
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Workspace paths held by live owner-project runs. Errors are intentionally
    /// propagated so migration callers cannot mistake DB uncertainty for idle.
    pub async fn live_workspace_paths_for_project(&self, project_id: &str) -> Result<Vec<String>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_scalar(
            "SELECT workspace_path FROM task_runs WHERE project_id = $1
             AND status IN ('starting', 'running') AND ended_at IS NULL
             AND workspace_path IS NOT NULL",
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Backdate the `started_at` timestamp of a task_run by the given SQL
    /// interval (e.g. "20 minutes"). Used by coordinator liveness tests to
    /// simulate a task_run whose hard runtime deadline has been exceeded.
    pub async fn backdate_started_at(&self, id: &str, interval: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE task_runs SET started_at = to_char(
                 now() AT TIME ZONE 'utc' - CAST($1 AS interval),
                 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
             WHERE id = $2",
        )
        .bind(interval)
        .bind(id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use djinn_core::events::EventBus;
    use djinn_core::models::{TaskRunStatus, TaskRunTrigger};

    use super::*;
    use crate::repositories::epic::EpicRepository;
    use crate::test_support::{UsageTestTaskSeed, seed_project, seed_task_row};

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
        let creator = crate::repositories::test_support::seed_test_user(db).await;
        sqlx::query!(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                                issue_type, priority, owner, status, continuation_count, labels, acceptance_criteria, memory_refs, created_by_user_id)
             VALUES ($1, $2, $3, $4, 'Task', '', '', 'task', 0, '', 'open', 0, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $5)",
            task_id,
            epic.project_id,
            short_id,
            epic.id,
            creator
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
                dispatch_group_id: None,
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
        assert!(run.dispatch_group_id.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_and_read_round_trip_dispatch_group_id() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskRunRepository::new(db);
        let id = new_run_id();
        let group_id = uuid::Uuid::now_v7().to_string();

        let created = repo
            .create(CreateTaskRunParams {
                id: &id,
                project_id: &project_id,
                task_id: &task_id,
                trigger_type: TaskRunTrigger::NewTask.as_str(),
                status: None,
                workspace_path: None,
                mirror_ref: None,
                dispatch_group_id: Some(&group_id),
            })
            .await
            .unwrap();
        assert_eq!(
            created.dispatch_group_id.as_deref(),
            Some(group_id.as_str())
        );
        assert_eq!(
            repo.get(&id)
                .await
                .unwrap()
                .unwrap()
                .dispatch_group_id
                .as_deref(),
            Some(group_id.as_str())
        );
        assert_eq!(
            repo.list_for_task(&task_id)
                .await
                .unwrap()
                .pop()
                .unwrap()
                .dispatch_group_id
                .as_deref(),
            Some(group_id.as_str())
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_returns_none_for_missing_id() {
        let db = test_db();
        let repo = TaskRunRepository::new(db);
        let missing = repo
            .get("00000000-0000-0000-0000-000000000000")
            .await
            .unwrap();
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
                dispatch_group_id: None,
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
    async fn set_workspace_path_populates_null_row() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskRunRepository::new(db);

        // K8s pod-run shape: the coordinator inserts the row with no
        // workspace_path.
        let id = new_run_id();
        repo.create(CreateTaskRunParams {
            id: &id,
            project_id: &project_id,
            task_id: &task_id,
            trigger_type: TaskRunTrigger::NewTask.as_str(),
            status: Some("starting"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();
        assert!(
            repo.get(&id)
                .await
                .unwrap()
                .unwrap()
                .workspace_path
                .is_none()
        );

        repo.set_workspace_path(&id, "/workspace/run-clone")
            .await
            .unwrap();
        // A later stage must retain the first in-pod clone, even if it sees a
        // different workspace path.
        repo.set_workspace_path(&id, "/workspace/second-stage-clone")
            .await
            .unwrap();
        let after = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(
            after.workspace_path.as_deref(),
            Some("/workspace/run-clone")
        );
        // Nothing else on the row changes.
        assert_eq!(after.status, "starting");
        assert!(after.ended_at.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_workspace_path_rejects_missing_row() {
        let db = test_db();
        let repo = TaskRunRepository::new(db);

        let error = repo
            .set_workspace_path("missing-task-run", "/workspace/run-clone")
            .await
            .expect_err("a missing task-run row must not report persistence success");
        assert!(
            error.to_string().contains("missing-task-run"),
            "error must identify the row that was not updated: {error}"
        );
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
            dispatch_group_id: None,
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
                dispatch_group_id: None,
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
                dispatch_group_id: None,
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
            dispatch_group_id: None,
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reap_running_for_task_flips_most_recent_running_row() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskRunRepository::new(db);

        // Older completed run + newer running run for the same task.
        let completed_id = new_run_id();
        repo.create(CreateTaskRunParams {
            id: &completed_id,
            project_id: &project_id,
            task_id: &task_id,
            trigger_type: TaskRunTrigger::NewTask.as_str(),
            status: None,
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();
        repo.update_status(&completed_id, TaskRunStatus::Completed)
            .await
            .unwrap();

        let running_id = new_run_id();
        repo.create(CreateTaskRunParams {
            id: &running_id,
            project_id: &project_id,
            task_id: &task_id,
            trigger_type: TaskRunTrigger::NewTask.as_str(),
            status: None,
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

        let reaped = repo
            .reap_running_for_task(&task_id, TaskRunStatus::Interrupted)
            .await
            .unwrap();
        assert_eq!(reaped.as_deref(), Some(running_id.as_str()));

        let after = repo.get(&running_id).await.unwrap().unwrap();
        assert_eq!(after.status, "interrupted");
        assert!(after.ended_at.is_some());

        // Already-terminal row untouched.
        let completed = repo.get(&completed_id).await.unwrap().unwrap();
        assert_eq!(completed.status, "completed");

        // Second call is a no-op (nothing in 'running' anymore).
        let again = repo
            .reap_running_for_task(&task_id, TaskRunStatus::Interrupted)
            .await
            .unwrap();
        assert!(again.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reap_running_for_task_ignores_non_terminal_status() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskRunRepository::new(db);

        let reaped = repo
            .reap_running_for_task(&task_id, TaskRunStatus::Running)
            .await
            .unwrap();
        assert!(
            reaped.is_none(),
            "non-terminal status must be a guard-no-op"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_workspace_paths_are_owner_scoped_and_only_include_live_runs() {
        let db = test_db();
        let project_id = "workspace-owner-project".to_string();
        let other_project_id = "workspace-other-project".to_string();
        seed_project(&db, &project_id, "workspace-owner").await;
        seed_project(&db, &other_project_id, "workspace-other").await;
        let task_id = seed_task_row(
            &db,
            UsageTestTaskSeed {
                project_id: &project_id,
                status: "open",
                close_reason: None,
                total_reopen_count: 0,
            },
        )
        .await;
        let other_task_id = seed_task_row(
            &db,
            UsageTestTaskSeed {
                project_id: &other_project_id,
                status: "open",
                close_reason: None,
                total_reopen_count: 0,
            },
        )
        .await;
        assert_ne!(project_id, other_project_id);
        assert_ne!(task_id, other_task_id);
        let repo = TaskRunRepository::new(db);
        let live_id = new_run_id();
        repo.create(CreateTaskRunParams {
            id: &live_id,
            project_id: &project_id,
            task_id: &task_id,
            trigger_type: TaskRunTrigger::NewTask.as_str(),
            status: Some("starting"),
            workspace_path: Some("/owner/live"),
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();
        let terminal_id = new_run_id();
        repo.create(CreateTaskRunParams {
            id: &terminal_id,
            project_id: &project_id,
            task_id: &task_id,
            trigger_type: TaskRunTrigger::NewTask.as_str(),
            status: None,
            workspace_path: Some("/owner/old"),
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();
        repo.update_status(&terminal_id, TaskRunStatus::Completed)
            .await
            .unwrap();
        let other_id = new_run_id();
        repo.create(CreateTaskRunParams {
            id: &other_id,
            project_id: &other_project_id,
            task_id: &other_task_id,
            trigger_type: TaskRunTrigger::NewTask.as_str(),
            status: None,
            workspace_path: Some("/other/live"),
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();
        assert_eq!(
            repo.live_workspace_paths_for_project(&project_id)
                .await
                .unwrap(),
            vec!["/owner/live"]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reap_stale_running_flips_rows_older_than_threshold() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskRunRepository::new(db);

        let stale_id = new_run_id();
        repo.create(CreateTaskRunParams {
            id: &stale_id,
            project_id: &project_id,
            task_id: &task_id,
            trigger_type: TaskRunTrigger::NewTask.as_str(),
            status: None,
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

        // Threshold in the future → every 'running' row is stale.
        let future = "9999-01-01T00:00:00.000Z";
        let ids = repo.reap_stale_running(future).await.unwrap();
        assert_eq!(ids, vec![stale_id.clone()]);

        let after = repo.get(&stale_id).await.unwrap().unwrap();
        assert_eq!(after.status, "interrupted");
        assert!(after.ended_at.is_some());

        // Threshold in the past → no-op.
        let past = "1970-01-01T00:00:00.000Z";
        let ids = repo.reap_stale_running(past).await.unwrap();
        assert!(ids.is_empty());
    }
}
