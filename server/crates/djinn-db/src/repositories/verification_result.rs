use crate::Result;
use crate::database::Database;

#[derive(Clone, Debug, sqlx::FromRow, serde::Serialize)]
pub struct VerificationStepRow {
    pub id: String,
    pub project_id: String,
    pub task_id: Option<String>,
    pub run_id: String,
    pub phase: String,
    pub step_index: i32,
    pub name: String,
    pub command: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: i64,
    pub created_at: String,
}

/// Input for inserting a verification step (no id/created_at — those are DB-generated).
pub struct VerificationStepInsert {
    pub project_id: String,
    pub task_id: Option<String>,
    pub run_id: String,
    pub phase: String,
    pub step_index: i32,
    pub name: String,
    pub command: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: i64,
}

pub struct VerificationResultRepository {
    db: Database,
}

impl VerificationResultRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Replace all results for a task with a new set (latest-run-wins).
    pub async fn replace_for_task(
        &self,
        task_id: &str,
        steps: &[VerificationStepInsert],
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        let pool = self.db.pool();

        // Delete previous results for this task.
        sqlx::query("DELETE FROM verification_results WHERE task_id = ?1")
            .bind(task_id)
            .execute(pool)
            .await?;

        for step in steps {
            sqlx::query(
                "INSERT INTO verification_results \
                 (project_id, task_id, run_id, phase, step_index, name, command, exit_code, stdout, stderr, duration_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )
            .bind(&step.project_id)
            .bind(&step.task_id)
            .bind(&step.run_id)
            .bind(&step.phase)
            .bind(step.step_index)
            .bind(&step.name)
            .bind(&step.command)
            .bind(step.exit_code)
            .bind(&step.stdout)
            .bind(&step.stderr)
            .bind(step.duration_ms)
            .execute(pool)
            .await?;
        }

        Ok(())
    }

    /// List the latest verification results for a task, ordered by step_index.
    pub async fn list_for_task(&self, task_id: &str) -> Result<Vec<VerificationStepRow>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, VerificationStepRow>(
            "SELECT id, project_id, task_id, run_id, phase, step_index, name, command, \
             exit_code, stdout, stderr, duration_ms, created_at \
             FROM verification_results WHERE task_id = ?1 ORDER BY step_index ASC",
        )
        .bind(task_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Delete all results for a task.
    pub async fn delete_for_task(&self, task_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("DELETE FROM verification_results WHERE task_id = ?1")
            .bind(task_id)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Prune results older than N days.
    pub async fn prune_older_than(&self, days: i64) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "DELETE FROM verification_results WHERE created_at < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-' || ?1 || ' days')",
        )
        .bind(days)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;

    async fn test_repo() -> VerificationResultRepository {
        let db = Database::open_in_memory().expect("in-memory db");
        VerificationResultRepository::new(db)
    }

    fn sample_steps(project_id: &str, task_id: &str, run_id: &str) -> Vec<VerificationStepInsert> {
        vec![
            VerificationStepInsert {
                project_id: project_id.to_string(),
                task_id: Some(task_id.to_string()),
                run_id: run_id.to_string(),
                phase: "setup".to_string(),
                step_index: 1,
                name: "cargo-build".to_string(),
                command: "cargo build --workspace".to_string(),
                exit_code: 0,
                stdout: "Compiling...".to_string(),
                stderr: String::new(),
                duration_ms: 5000,
            },
            VerificationStepInsert {
                project_id: project_id.to_string(),
                task_id: Some(task_id.to_string()),
                run_id: run_id.to_string(),
                phase: "verification".to_string(),
                step_index: 2,
                name: "verify-1".to_string(),
                command: "cargo check -p alt-db".to_string(),
                exit_code: 0,
                stdout: "ok".to_string(),
                stderr: String::new(),
                duration_ms: 3000,
            },
        ]
    }

    #[tokio::test]
    async fn insert_and_list_round_trip() {
        let repo = test_repo().await;
        let steps = sample_steps("p1", "t1", "run1");
        repo.replace_for_task("t1", &steps).await.expect("insert");

        let rows = repo.list_for_task("t1").await.expect("list");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].phase, "setup");
        assert_eq!(rows[0].name, "cargo-build");
        assert_eq!(rows[1].phase, "verification");
        assert_eq!(rows[1].name, "verify-1");
    }

    #[tokio::test]
    async fn replace_deletes_old_results() {
        let repo = test_repo().await;
        let steps = sample_steps("p1", "t1", "run1");
        repo.replace_for_task("t1", &steps).await.expect("insert");

        let new_steps = vec![VerificationStepInsert {
            project_id: "p1".to_string(),
            task_id: Some("t1".to_string()),
            run_id: "run2".to_string(),
            phase: "verification".to_string(),
            step_index: 1,
            name: "verify-only".to_string(),
            command: "cargo test".to_string(),
            exit_code: 1,
            stdout: "FAILED".to_string(),
            stderr: "error".to_string(),
            duration_ms: 2000,
        }];
        repo.replace_for_task("t1", &new_steps)
            .await
            .expect("replace");

        let rows = repo.list_for_task("t1").await.expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "verify-only");
        assert_eq!(rows[0].run_id, "run2");
    }

    #[tokio::test]
    async fn list_empty_returns_empty() {
        let repo = test_repo().await;
        let rows = repo.list_for_task("missing").await.expect("list");
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn delete_for_task_removes_only_that_task() {
        let repo = test_repo().await;
        repo.replace_for_task("t1", &sample_steps("p1", "t1", "r1"))
            .await
            .expect("insert t1");
        repo.replace_for_task("t2", &sample_steps("p1", "t2", "r2"))
            .await
            .expect("insert t2");

        repo.delete_for_task("t1").await.expect("delete t1");

        assert!(repo.list_for_task("t1").await.expect("list t1").is_empty());
        assert_eq!(repo.list_for_task("t2").await.expect("list t2").len(), 2);
    }
}
