//! Pre-PR verification runs executed in a one-shot Job (`verification_runs`,
//! migration 62).
//!
//! Verification is being removed (epic sehj). The `verification_runs` table
//! is dropped by migration 72 but this repository module remains temporarily
//! pending the full removal of all verification references. It uses the
//! non-macro `sqlx::query` form (like the other verification repos).
//!
//! All methods gracefully degrade when the table no longer exists (post
//! migration 72): reads return `None`/empty, writes are silent no-ops.

use sqlx::Row;

use crate::Result;
use crate::database::Database;
use crate::repositories::verification_common::ok_if_table_dropped;

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
        let res = sqlx::query(
            r#"INSERT INTO verification_runs (id, task_id, project_id, status)
               VALUES ($1, $2, $3, 'pending')"#,
        )
        .bind(id)
        .bind(task_id)
        .bind(project_id)
        .execute(self.db.pool())
        .await;
        match res {
            Ok(_) => Ok(()),
            Err(e) if ok_if_table_dropped(&e) => Ok(()),
            Err(e) => Err(e.into()),
        }
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
        .await;
        match row {
            Ok(row) => Ok(row.map(|r| VerificationRun {
                id: r.get("id"),
                task_id: r.get("task_id"),
                project_id: r.get("project_id"),
                status: r.get("status"),
                setup_results: r.get("setup_results"),
                verification_results: r.get("verification_results"),
                error: r.get("error"),
            })),
            Err(e) => {
                if ok_if_table_dropped(&e) {
                    Ok(None)
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Latest verification run for `task_id` that reached a USABLE terminal
    /// state (`passed` | `failed`). Returns the row id (newest by `created_at`)
    /// or `None` when no such row exists.
    pub async fn latest_terminal_for_task(&self, task_id: &str) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query(
            r#"SELECT id
                 FROM verification_runs
                WHERE task_id = $1
                  AND status IN ('passed', 'failed')
                ORDER BY created_at DESC
                LIMIT 1"#,
        )
        .bind(task_id)
        .fetch_optional(self.db.pool())
        .await;
        match row {
            Ok(row) => Ok(row.map(|r| r.get("id"))),
            Err(e) => {
                if ok_if_table_dropped(&e) {
                    Ok(None)
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Mark a run `running` (the Job pod picked it up).
    pub async fn mark_running(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        let res = sqlx::query("UPDATE verification_runs SET status = 'running' WHERE id = $1")
            .bind(id)
            .execute(self.db.pool())
            .await;
        match res {
            Ok(_) => Ok(()),
            Err(e) if ok_if_table_dropped(&e) => Ok(()),
            Err(e) => Err(e.into()),
        }
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
        let res = sqlx::query(
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
        .await;
        match res {
            Ok(_) => Ok(()),
            Err(e) if ok_if_table_dropped(&e) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}
