//! Per-project verification rules (`project_verifications` table, migration 44).
//!
//! Verification is being removed (epic sehj). The `project_verifications`
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
        .await;
        match row {
            Ok(row) => Ok(row.map(|r| r.get::<String, _>("rules"))),
            Err(e) => {
                if ok_if_table_dropped(&e) {
                    Ok(None)
                } else {
                    Err(e.into())
                }
            }
        }
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
        let res = sqlx::query(
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
        .await;
        match res {
            Ok(_) => Ok(()),
            Err(e) if ok_if_table_dropped(&e) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Insert default rules for a project only if it has no row yet. Used by
    /// the boot reseed / on-demand reset paths to seed stack-derived defaults
    /// without clobbering user edits. No-op when a row already exists.
    pub async fn seed_if_absent(&self, project_id: &str, rules_json: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        let rules: serde_json::Value = serde_json::from_str(rules_json)
            .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()));
        let res = sqlx::query(
            r#"INSERT INTO project_verifications (project_id, rules, source)
               VALUES ($1, $2::jsonb, 'auto_detected')
               ON CONFLICT (project_id) DO NOTHING"#,
        )
        .bind(project_id)
        .bind(rules)
        .execute(self.db.pool())
        .await;
        match res {
            Ok(_) => Ok(()),
            Err(e) if ok_if_table_dropped(&e) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}
