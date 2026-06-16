//! Pre-PR verification runs executed in a one-shot Job (`verification_runs`,
//! migration 61).
//!
//! Verification commands need the project's toolchain + the shared `/cache`
//! PVC, neither of which exists on the djinn-server host. The production path
//! therefore dispatches a Job in the project's image that runs the real
//! verification pipeline (`verify_commit`) against the task branch and writes
//! its outcome here; the server polls this row, then runs the existing
//! pass/fail transitions against the results.
//!
//! Mirrors [`super::verification_test::VerificationTestRepository`] (non-macro
//! `sqlx::query` form) so a fresh table doesn't require regenerating the
//! offline `.sqlx` cache.

use sqlx::Row;

use crate::Result;
use crate::database::Database;

/// Terminal + in-flight states for a verification run.
pub struct VerificationRunStatus;
impl VerificationRunStatus {
    pub const PENDING: &'static str = "pending";
    pub const RUNNING: &'static str = "running";
    pub const PASSED: &'static str = "passed";
    pub const FAILED: &'static str = "failed";
    pub const ERROR: &'static str = "error";
}

/// A row of `verification_runs`. JSON fields are returned as raw text; callers
/// parse the per-command results against `djinn-core`'s `CommandResult`.
#[derive(Clone, Debug)]
pub struct VerificationRun {
    pub id: String,
    pub task_id: String,
    pub project_id: String,
    pub status: String,
    /// JSON array of setup-phase `CommandResult`s.
    pub setup_results: String,
    /// JSON array of verification-phase `CommandResult`s.
    pub verification_results: String,
    pub error: Option<String>,
}

pub struct VerificationRunRepository {
    db: Database,
}

impl VerificationRunRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Insert a fresh `pending` verification run for a task.
    pub async fn create(&self, id: &str, task_id: &str, project_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            r#"INSERT INTO verification_runs (id, task_id, project_id, status)
               VALUES ($1, $2, $3, 'pending')"#,
        )
        .bind(id)
        .bind(task_id)
        .bind(project_id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Fetch a verification run by id.
    pub async fn get(&self, id: &str) -> Result<Option<VerificationRun>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query(
            r#"SELECT id, task_id, project_id, status,
                      setup_results::text        AS setup_results,
                      verification_results::text AS verification_results,
                      error
                 FROM verification_runs WHERE id = $1"#,
        )
        .bind(id)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(|r| VerificationRun {
            id: r.get("id"),
            task_id: r.get("task_id"),
            project_id: r.get("project_id"),
            status: r.get("status"),
            setup_results: r.get("setup_results"),
            verification_results: r.get("verification_results"),
            error: r.get("error"),
        }))
    }

    /// Mark a run `running` (the Job pod picked it up).
    pub async fn mark_running(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("UPDATE verification_runs SET status = 'running' WHERE id = $1")
            .bind(id)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Write the terminal outcome (`passed` | `failed` | `error`) + per-command
    /// results for both phases + optional error, stamping `completed_at`.
    pub async fn complete(
        &self,
        id: &str,
        status: &str,
        setup_results_json: &str,
        verification_results_json: &str,
        error: Option<&str>,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        let setup: serde_json::Value = serde_json::from_str(setup_results_json)
            .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()));
        let verification: serde_json::Value = serde_json::from_str(verification_results_json)
            .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()));
        sqlx::query(
            r#"UPDATE verification_runs
                  SET status = $2,
                      setup_results = $3::jsonb,
                      verification_results = $4::jsonb,
                      error = $5,
                      completed_at = to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                WHERE id = $1"#,
        )
        .bind(id)
        .bind(status)
        .bind(setup)
        .bind(verification)
        .bind(error)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::events::EventBus;

    use crate::repositories::project::ProjectRepository;

    async fn seed_project(db: &Database, id: &str) {
        db.ensure_initialized().await.unwrap();
        ProjectRepository::new(db.clone(), EventBus::noop())
            .create_with_id(id, &format!("p-{id}"), "test", id)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn create_get_complete_lifecycle() {
        let db = Database::open_in_memory().unwrap();
        seed_project(&db, "p1").await;
        let repo = VerificationRunRepository::new(db.clone());
        repo.create("r1", "t1", "p1").await.unwrap();

        let run = repo.get("r1").await.unwrap().expect("row");
        assert_eq!(run.status, VerificationRunStatus::PENDING);
        assert_eq!(run.task_id, "t1");
        assert_eq!(run.project_id, "p1");

        repo.mark_running("r1").await.unwrap();
        assert_eq!(repo.get("r1").await.unwrap().unwrap().status, "running");

        let setup = r#"[{"name":"setup-1","command":"echo hi","exit_code":0,"stdout":"hi","stderr":"","duration_ms":1}]"#;
        let verify = r#"[{"name":"verify-1","command":"cargo test","exit_code":0,"stdout":"ok","stderr":"","duration_ms":12}]"#;
        repo.complete("r1", VerificationRunStatus::PASSED, setup, verify, None)
            .await
            .unwrap();
        let done = repo.get("r1").await.unwrap().unwrap();
        assert_eq!(done.status, "passed");
        assert!(done.setup_results.contains("echo hi"));
        assert!(done.verification_results.contains("cargo test"));
        assert!(done.error.is_none());
    }

    #[tokio::test]
    async fn cascade_deletes_with_project() {
        let db = Database::open_in_memory().unwrap();
        seed_project(&db, "p1").await;
        let repo = VerificationRunRepository::new(db.clone());
        repo.create("r1", "t1", "p1").await.unwrap();
        sqlx::query("DELETE FROM projects WHERE id = $1")
            .bind("p1")
            .execute(db.pool())
            .await
            .unwrap();
        assert!(repo.get("r1").await.unwrap().is_none());
    }
}
