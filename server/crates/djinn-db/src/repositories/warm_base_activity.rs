use crate::Result;
use crate::database::Database;
use sqlx::Row;

/// Activity summary used by the warm-base whole-base eviction path.
///
/// Distinguishes "no recorded activity" (`latest_activity = None`) from a query
/// failure (the whole `Result` is `Err`).  Callers that fall back to directory
/// mtime should only do so when `latest_activity` is `None`, not when the query
/// errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WarmBaseActivity {
    /// The most recent ISO-8601 timestamp derived from `task_runs` and
    /// `project_workspace_graph.warmed_at`, or `None` when the project has no
    /// recorded activity in either source.
    pub latest_activity: Option<String>,
    /// `true` when the project has at least one task run in a live
    /// (`starting` or `running`) non-terminal state.
    pub has_active_task_run: bool,
    /// `true` when the project was deleted through `ProjectRepository`.
    /// Tombstones remain after the project row is cascaded away so warm-base
    /// cleanup can distinguish deleted projects from arbitrary UUID paths.
    pub deleted_project: bool,
}

pub struct WarmBaseActivityRepository {
    db: Database,
}

impl WarmBaseActivityRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Return the warm-base activity snapshot for a single project.
    ///
    /// Returns `Ok(None)` when the project id is unknown. Returns
    /// `Ok(Some(activity))` with `activity.latest_activity = None` when the
    /// project exists but has no recorded task runs or warm activity. A
    /// deletion tombstone also returns `Some`, with `deleted_project = true`.
    pub async fn get(&self, project_id: &str) -> Result<Option<WarmBaseActivity>> {
        self.db.ensure_initialized().await?;

        let row = sqlx::query(
            r#"SELECT
                   p.id AS project_id,
                   (
                       SELECT MAX(ts)
                         FROM (
                           SELECT MAX(started_at) AS ts
                             FROM task_runs
                            WHERE project_id = p.id
                           UNION ALL
                           SELECT MAX(warmed_at) AS ts
                             FROM project_workspace_graph
                            WHERE project_id = p.id
                         ) sub
                        WHERE ts IS NOT NULL
                   ) AS latest_activity,
                   EXISTS(
                       SELECT 1
                         FROM task_runs
                        WHERE project_id = p.id
                          AND status IN ('starting', 'running')
                          AND ended_at IS NULL
                   ) AS has_active_task_run
              FROM projects p
             WHERE p.id = $1"#,
        )
        .bind(project_id)
        .fetch_optional(self.db.pool())
        .await?;

        if let Some(r) = row {
            let latest_activity: Option<String> = r.get("latest_activity");
            let has_active_task_run: bool = r.get("has_active_task_run");
            return Ok(Some(WarmBaseActivity {
                latest_activity,
                has_active_task_run,
                deleted_project: false,
            }));
        }

        let deleted: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM deleted_projects WHERE project_id = $1")
                .bind(project_id)
                .fetch_optional(self.db.pool())
                .await?;
        Ok(deleted.map(|_| WarmBaseActivity {
            latest_activity: None,
            has_active_task_run: false,
            deleted_project: true,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fresh() -> WarmBaseActivityRepository {
        let db = Database::open_in_memory().expect("in-memory db");
        WarmBaseActivityRepository::new(db)
    }

    async fn seed_project(repo: &WarmBaseActivityRepository, project_id: &str) {
        repo.db.ensure_initialized().await.expect("init db");
        sqlx::query(
            "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
        )
        .bind(project_id)
        .bind(project_id)
        .bind("test")
        .bind(format!("repo-{project_id}"))
        .execute(repo.db.pool())
        .await
        .expect("seed project");
    }

    async fn seed_task(
        repo: &WarmBaseActivityRepository,
        project_id: &str,
        task_id: &str,
        run_id: &str,
        status: &str,
    ) {
        let creator = crate::repositories::test_support::seed_test_user(&repo.db).await;
        sqlx::query(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                                issue_type, priority, owner, status, continuation_count, labels,
                                acceptance_criteria, memory_refs, created_by_user_id)
             VALUES ($1, $2, 'short', NULL, 't', '', '', 'task', 0, '', 'open', 0, '[]', '[]', '[]', $3)",
        )
        .bind(task_id)
        .bind(project_id)
        .bind(creator)
        .execute(repo.db.pool())
        .await
        .expect("seed task");

        sqlx::query(
            "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status, workspace_path, mirror_ref)
             VALUES ($1, $2, $3, 'new_task', $4, NULL, NULL)",
        )
        .bind(run_id)
        .bind(project_id)
        .bind(task_id)
        .bind(status)
        .execute(repo.db.pool())
        .await
        .expect("seed task run");
    }

    async fn seed_warm(repo: &WarmBaseActivityRepository, project_id: &str, warmed_at: &str) {
        sqlx::query(
            "INSERT INTO project_workspace_graph (project_id, workspace_slug, commit_sha, warmed_at, status)
             VALUES ($1, 'root', 'abc', $2, 'ready')",
        )
        .bind(project_id)
        .bind(warmed_at)
        .execute(repo.db.pool())
        .await
        .expect("seed warm");
    }

    #[tokio::test]
    async fn unknown_project_returns_none() {
        let repo = fresh().await;
        repo.db.ensure_initialized().await.unwrap();
        let activity = repo.get("does-not-exist").await.expect("query");
        assert!(activity.is_none());
    }

    #[tokio::test]
    async fn deleted_project_returns_tombstone_activity() {
        let repo = fresh().await;
        repo.db.ensure_initialized().await.unwrap();
        sqlx::query("INSERT INTO deleted_projects (project_id) VALUES ($1)")
            .bind("deleted-project")
            .execute(repo.db.pool())
            .await
            .expect("seed deletion tombstone");

        let activity = repo
            .get("deleted-project")
            .await
            .expect("query")
            .expect("tombstone exists");
        assert!(activity.deleted_project);
        assert!(!activity.has_active_task_run);
        assert!(activity.latest_activity.is_none());
    }

    #[tokio::test]
    async fn project_without_activity_has_no_latest() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;

        let activity = repo
            .get("p1")
            .await
            .expect("query")
            .expect("project exists");
        assert!(activity.latest_activity.is_none());
        assert!(!activity.has_active_task_run);
    }

    #[tokio::test]
    async fn task_run_activity_is_detected() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;
        seed_task(&repo, "p1", "task-1", "run-1", "running").await;

        let activity = repo
            .get("p1")
            .await
            .expect("query")
            .expect("project exists");
        assert!(activity.latest_activity.is_some());
        assert!(activity.has_active_task_run);
    }

    #[tokio::test]
    async fn warm_activity_is_detected() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;
        seed_warm(&repo, "p1", "2026-01-01T00:00:00.000Z").await;

        let activity = repo
            .get("p1")
            .await
            .expect("query")
            .expect("project exists");
        assert_eq!(
            activity.latest_activity.as_deref(),
            Some("2026-01-01T00:00:00.000Z")
        );
        assert!(!activity.has_active_task_run);
    }

    #[tokio::test]
    async fn activity_precedence_picks_latest() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;
        seed_task(&repo, "p1", "task-1", "run-1", "completed").await;
        seed_warm(&repo, "p1", "2026-01-15T00:00:00.000Z").await;

        // Backdate the task run so the warm activity is strictly later.
        sqlx::query("UPDATE task_runs SET started_at = '2026-01-01T00:00:00.000Z' WHERE id = $1")
            .bind("run-1")
            .execute(repo.db.pool())
            .await
            .expect("backdate task run");

        let activity = repo
            .get("p1")
            .await
            .expect("query")
            .expect("project exists");
        assert_eq!(
            activity.latest_activity.as_deref(),
            Some("2026-01-15T00:00:00.000Z")
        );
    }

    #[tokio::test]
    async fn task_run_later_than_warm() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;
        seed_warm(&repo, "p1", "2026-01-01T00:00:00.000Z").await;
        seed_task(&repo, "p1", "task-1", "run-1", "completed").await;

        // The task run's default started_at is `now()`, so it should be later
        // than the synthetic warm timestamp. We just assert it is non-empty and
        // differs from the warm timestamp.
        let activity = repo
            .get("p1")
            .await
            .expect("query")
            .expect("project exists");
        let latest = activity.latest_activity.expect("latest activity");
        assert_ne!(latest, "2026-01-01T00:00:00.000Z");
    }

    #[tokio::test]
    async fn terminal_task_run_is_not_active() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;
        seed_task(&repo, "p1", "task-1", "run-1", "completed").await;

        let activity = repo
            .get("p1")
            .await
            .expect("query")
            .expect("project exists");
        assert!(!activity.has_active_task_run);
    }

    #[tokio::test]
    async fn starting_task_run_is_active() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;
        seed_task(&repo, "p1", "task-1", "run-1", "starting").await;

        let activity = repo
            .get("p1")
            .await
            .expect("query")
            .expect("project exists");
        assert!(activity.has_active_task_run);
    }

    #[tokio::test]
    async fn active_flag_is_project_scoped() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;
        seed_project(&repo, "p2").await;
        seed_task(&repo, "p1", "task-1", "run-1", "running").await;

        let p1 = repo.get("p1").await.expect("query").expect("p1 exists");
        let p2 = repo.get("p2").await.expect("query").expect("p2 exists");
        assert!(p1.has_active_task_run);
        assert!(!p2.has_active_task_run);
    }
}
