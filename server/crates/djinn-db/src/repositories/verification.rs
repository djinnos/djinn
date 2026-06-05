//! Per-project verification rules (`project_verifications` table, migration 44).
//!
//! Verification rules left `projects.environment_config` so that editing a
//! verify command no longer rebuilds the image (the rule was part of the
//! hashed config, and `set_environment_config` nulls `image_hash` on every
//! write). Rules now live here, edited independently of the image.
//!
//! Like [`ProjectRepository::get_environment_config`], this repository is
//! JSON-string based — it stores/returns the raw `rules` array as text and
//! leaves typing + validation to the callers that depend on `djinn-stack`
//! (the MCP layer and the agent verification pipeline). It uses the
//! **non-macro** `sqlx::query` form so a fresh `project_verifications` table
//! doesn't require regenerating the offline `.sqlx` cache to compile.

use sqlx::Row;

use crate::Result;
use crate::database::Database;

pub struct VerificationRepository {
    db: Database,
}

impl VerificationRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Fetch the `rules` JSON array (as text) for a project. Returns `Ok(None)`
    /// when the project has no `project_verifications` row yet (treat as "no
    /// rules"). The text is a JSON array of `{match_pattern, commands[]}`.
    pub async fn get_rules(&self, project_id: &str) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query(
            r#"SELECT rules::text AS rules FROM project_verifications WHERE project_id = $1"#,
        )
        .bind(project_id)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(|r| r.get::<String, _>("rules")))
    }

    /// Upsert the `rules` array for a project. `rules_json` must be a JSON
    /// array string; callers validate (via `djinn_stack::environment::
    /// Verification::validate`) before calling. `source` is `"auto_detected"`
    /// or `"user_edited"`.
    pub async fn set_rules(&self, project_id: &str, rules_json: &str, source: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        // The `$2::jsonb` cast types the bind as `serde_json::Value`.
        let rules: serde_json::Value = serde_json::from_str(rules_json)
            .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()));
        sqlx::query(
            r#"INSERT INTO project_verifications (project_id, rules, source, updated_at)
               VALUES ($1, $2::jsonb, $3,
                       to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
               ON CONFLICT (project_id) DO UPDATE
                   SET rules = EXCLUDED.rules,
                       source = EXCLUDED.source,
                       updated_at = EXCLUDED.updated_at"#,
        )
        .bind(project_id)
        .bind(rules)
        .bind(source)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Insert default rules for a project only if it has no row yet. Used by
    /// the boot reseed / on-demand reset paths to seed stack-derived defaults
    /// without clobbering user edits. No-op when a row already exists.
    pub async fn seed_if_absent(&self, project_id: &str, rules_json: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        let rules: serde_json::Value = serde_json::from_str(rules_json)
            .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()));
        sqlx::query(
            r#"INSERT INTO project_verifications (project_id, rules, source)
               VALUES ($1, $2::jsonb, 'auto_detected')
               ON CONFLICT (project_id) DO NOTHING"#,
        )
        .bind(project_id)
        .bind(rules)
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
    async fn get_returns_none_when_absent() {
        let db = Database::open_in_memory().unwrap();
        seed_project(&db, "p1").await;
        let repo = VerificationRepository::new(db.clone());
        assert!(repo.get_rules("p1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn set_then_get_round_trips() {
        let db = Database::open_in_memory().unwrap();
        seed_project(&db, "p1").await;
        let repo = VerificationRepository::new(db.clone());
        let rules = r#"[{"match_pattern":"**/*.rs","commands":["cargo test"]}]"#;
        repo.set_rules("p1", rules, "user_edited").await.unwrap();
        let got = repo.get_rules("p1").await.unwrap().expect("row");
        let parsed: serde_json::Value = serde_json::from_str(&got).unwrap();
        assert_eq!(parsed[0]["match_pattern"], "**/*.rs");
    }

    #[tokio::test]
    async fn seed_if_absent_does_not_clobber() {
        let db = Database::open_in_memory().unwrap();
        seed_project(&db, "p1").await;
        let repo = VerificationRepository::new(db.clone());
        repo.set_rules(
            "p1",
            r#"[{"match_pattern":"x","commands":["a"]}]"#,
            "user_edited",
        )
        .await
        .unwrap();
        repo.seed_if_absent("p1", r#"[{"match_pattern":"y","commands":["b"]}]"#)
            .await
            .unwrap();
        let got = repo.get_rules("p1").await.unwrap().unwrap();
        assert!(
            got.contains('x') && !got.contains('y'),
            "seed must not clobber: {got}"
        );
    }

    #[tokio::test]
    async fn cascade_deletes_with_project() {
        let db = Database::open_in_memory().unwrap();
        seed_project(&db, "p1").await;
        let repo = VerificationRepository::new(db.clone());
        repo.set_rules(
            "p1",
            r#"[{"match_pattern":"x","commands":["a"]}]"#,
            "user_edited",
        )
        .await
        .unwrap();
        sqlx::query("DELETE FROM projects WHERE id = $1")
            .bind("p1")
            .execute(db.pool())
            .await
            .unwrap();
        assert!(repo.get_rules("p1").await.unwrap().is_none());
    }
}
