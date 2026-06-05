//! Verification "test-before-save" runs (`verification_test_runs`, migration 45).
//!
//! A candidate set of verification rules is tested by running its commands in
//! the project's image (a one-shot Job — the toolchain lives in the image, not
//! the server host). The Job writes its outcome here; the MCP layer polls for
//! it and `project_verification_set` refuses to persist rules unless a matching
//! `passed` row exists.
//!
//! Non-macro `sqlx::query` form (like [`VerificationRepository`]) so a fresh
//! table doesn't require regenerating the offline `.sqlx` cache.

use sqlx::Row;

use crate::Result;
use crate::database::Database;

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
        sqlx::query(
            r#"INSERT INTO verification_test_runs (id, project_id, rules_hash, candidate_rules, status)
               VALUES ($1, $2, $3, $4::jsonb, 'pending')"#,
        )
        .bind(id)
        .bind(project_id)
        .bind(rules_hash)
        .bind(candidate)
        .execute(self.db.pool())
        .await?;
        Ok(())
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
        .await?;
        Ok(row.map(|r| VerificationTestRun {
            id: r.get("id"),
            project_id: r.get("project_id"),
            rules_hash: r.get("rules_hash"),
            candidate_rules: r.get("candidate_rules"),
            status: r.get("status"),
            results: r.get("results"),
            error: r.get("error"),
        }))
    }

    /// Mark a run `running` (the Job picked it up).
    pub async fn mark_running(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("UPDATE verification_test_runs SET status = 'running' WHERE id = $1")
            .bind(id)
            .execute(self.db.pool())
            .await?;
        Ok(())
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
        sqlx::query(
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
        let repo = VerificationTestRepository::new(db.clone());
        let rules = r#"[{"match_pattern":"**/*.rs","commands":["cargo test"]}]"#;
        repo.create("t1", "p1", "abc123", rules).await.unwrap();

        let run = repo.get("t1").await.unwrap().expect("row");
        assert_eq!(run.status, VerificationTestStatus::PENDING);
        assert_eq!(run.rules_hash, "abc123");
        assert!(run.candidate_rules.contains("cargo test"));

        repo.mark_running("t1").await.unwrap();
        assert_eq!(repo.get("t1").await.unwrap().unwrap().status, "running");

        let results = r#"[{"name":"v-1","command":"cargo test","exit_code":0,"stdout":"ok","stderr":"","duration_ms":12}]"#;
        repo.complete("t1", VerificationTestStatus::PASSED, results, None)
            .await
            .unwrap();
        let done = repo.get("t1").await.unwrap().unwrap();
        assert_eq!(done.status, "passed");
        assert!(done.results.contains("cargo test"));
    }

    #[tokio::test]
    async fn cascade_deletes_with_project() {
        let db = Database::open_in_memory().unwrap();
        seed_project(&db, "p1").await;
        let repo = VerificationTestRepository::new(db.clone());
        repo.create("t1", "p1", "h", "[]").await.unwrap();
        sqlx::query("DELETE FROM projects WHERE id = $1")
            .bind("p1")
            .execute(db.pool())
            .await
            .unwrap();
        assert!(repo.get("t1").await.unwrap().is_none());
    }
}
