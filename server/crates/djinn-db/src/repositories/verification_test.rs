//! Verification "test-before-save" runs (`verification_test_runs`, migration 45).
//!
//! Verification is being removed (epic sehj). The `verification_test_runs`
//! table is dropped by migration 72 but this repository module remains
//! temporarily pending the full removal of all verification references. It
//! uses the non-macro `sqlx::query` form (like the other verification repos).
//!
//! All methods gracefully degrade when the table no longer exists (post
//! migration 72): reads return `None`, writes are silent no-ops.

use sqlx::Row;

use crate::Result;
use crate::database::Database;
use crate::repositories::verification_common::ok_if_table_dropped;

/// Terminal + in-flight states for a test run.
pub struct VerificationTestStatus;
impl VerificationTestStatus {
    pub const PENDING: &'static str = "pending";
    pub const RUNNING: &'static str = "running";
    pub const PASSED: &'static str = "passed";
    pub const FAILED: &'static str = "failed";
    pub const ERROR: &'static str = "error";
}

/// A row of `verification_test_runs`. JSON fields are returned as raw text;
/// callers that need typed values parse against `djinn-stack`.
#[derive(Clone, Debug)]
pub struct VerificationTestRun {
    pub id: String,
    pub project_id: String,
    pub rules_hash: String,
    pub candidate_rules: String,
    pub status: String,
    pub results: String,
    pub error: Option<String>,
}

pub struct VerificationTestRepository {
    db: Database,
}

impl VerificationTestRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Insert a fresh `pending` test run. `candidate_rules_json` is the rules
    /// array the Job will run; `rules_hash` is its canonical sha256 (the value
    /// the save-gate matches against).
    pub async fn create(
        &self,
        id: &str,
        project_id: &str,
        rules_hash: &str,
        candidate_rules_json: &str,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        let candidate: serde_json::Value = serde_json::from_str(candidate_rules_json)
            .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()));
        let res = sqlx::query(
            r#"INSERT INTO verification_test_runs (id, project_id, rules_hash, candidate_rules, status)
               VALUES ($1, $2, $3, $4::jsonb, 'pending')"#,
        )
        .bind(id)
        .bind(project_id)
        .bind(rules_hash)
        .bind(candidate)
        .execute(self.db.pool())
        .await;
        match res {
            Ok(_) => Ok(()),
            Err(e) if ok_if_table_dropped(&e) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Fetch a test run by id.
    pub async fn get(&self, id: &str) -> Result<Option<VerificationTestRun>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query(
            r#"SELECT id, project_id, rules_hash,
                      candidate_rules::text AS candidate_rules,
                      status,
                      results::text AS results,
                      error
                 FROM verification_test_runs WHERE id = $1"#,
        )
        .bind(id)
        .fetch_optional(self.db.pool())
        .await;
        match row {
            Ok(row) => Ok(row.map(|r| VerificationTestRun {
                id: r.get("id"),
                project_id: r.get("project_id"),
                rules_hash: r.get("rules_hash"),
                candidate_rules: r.get("candidate_rules"),
                status: r.get("status"),
                results: r.get("results"),
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

    /// Mark a run `running` (the Job picked it up).
    pub async fn mark_running(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        let res = sqlx::query("UPDATE verification_test_runs SET status = 'running' WHERE id = $1")
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
    /// results + optional error, stamping `completed_at`.
    pub async fn complete(
        &self,
        id: &str,
        status: &str,
        results_json: &str,
        error: Option<&str>,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        let results: serde_json::Value = serde_json::from_str(results_json)
            .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()));
        let res = sqlx::query(
            r#"UPDATE verification_test_runs
                  SET status = $2,
                      results = $3::jsonb,
                      error = $4,
                      completed_at = to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                WHERE id = $1"#,
        )
        .bind(id)
        .bind(status)
        .bind(results)
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
