//! Durable per-step results for task verification runs
//! (`verification_results` table).
//!
//! Verification is being removed (epic sehj). The `verification_results`
//! table is dropped by migration 72 but this repository module remains
//! temporarily pending the full removal of all verification references. It
//! uses the non-macro `sqlx::query` form (like the other verification repos)
//! so it does not require the table to exist for offline `.sqlx` cache
//! compilation.
//!
//! All methods gracefully degrade when the table no longer exists (post
//! migration 72): reads return empty, writes are silent no-ops.

use sqlx::Row;

use crate::Result;
use crate::database::Database;
use crate::repositories::verification_common::ok_if_table_dropped;

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
        let res = sqlx::query("DELETE FROM verification_results WHERE task_id = $1")
            .bind(task_id)
            .execute(pool)
            .await;
        match res {
            Ok(_) => {}
            Err(e) if ok_if_table_dropped(&e) => return Ok(()),
            Err(e) => return Err(e.into()),
        }

        for step in steps {
            let res = sqlx::query(
                r#"INSERT INTO verification_results
                   (project_id, task_id, run_id, phase, step_index, name, command,
                    exit_code, stdout, stderr, duration_ms)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"#,
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
            .await;
            match res {
                Ok(_) => {}
                Err(e) if ok_if_table_dropped(&e) => return Ok(()),
                Err(e) => return Err(e.into()),
            }
        }

        Ok(())
    }

    /// List the latest verification results for a task, ordered by step_index.
    pub async fn list_for_task(&self, task_id: &str) -> Result<Vec<VerificationStepRow>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query(
            r#"SELECT id, project_id, task_id, run_id, phase, step_index, name, command,
                      exit_code, stdout, stderr, duration_ms, created_at
                 FROM verification_results
                WHERE task_id = $1
                ORDER BY step_index ASC"#,
        )
        .bind(task_id)
        .fetch_all(self.db.pool())
        .await;

        match rows {
            Ok(rows) => Ok(rows
                .into_iter()
                .map(|r| VerificationStepRow {
                    id: r.get("id"),
                    project_id: r.get("project_id"),
                    task_id: r.get("task_id"),
                    run_id: r.get("run_id"),
                    phase: r.get("phase"),
                    step_index: r.get("step_index"),
                    name: r.get("name"),
                    command: r.get("command"),
                    exit_code: r.get("exit_code"),
                    stdout: r.get("stdout"),
                    stderr: r.get("stderr"),
                    duration_ms: r.get("duration_ms"),
                    created_at: r.get("created_at"),
                })
                .collect()),
            Err(e) => {
                if ok_if_table_dropped(&e) {
                    Ok(Vec::new())
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Delete all results for a task.
    pub async fn delete_for_task(&self, task_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        let res = sqlx::query("DELETE FROM verification_results WHERE task_id = $1")
            .bind(task_id)
            .execute(self.db.pool())
            .await;
        match res {
            Ok(_) => Ok(()),
            Err(e) if ok_if_table_dropped(&e) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Prune results older than N days.
    pub async fn prune_older_than(&self, days: i64) -> Result<()> {
        self.db.ensure_initialized().await?;
        let res = sqlx::query(
            r#"DELETE FROM verification_results
                WHERE created_at < to_char((now() at time zone 'utc') - (interval '1 day' * $1),
                                           'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')"#,
        )
        .bind(days as f64)
        .execute(self.db.pool())
        .await;
        match res {
            Ok(_) => Ok(()),
            Err(e) if ok_if_table_dropped(&e) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}
