//! Durable Release N migration records. One row is the restart/reconciliation
//! authority for a `(project_id, family, release)` migration.

use serde_json::Value;

use crate::Result;
use crate::database::Database;
use crate::error::DbError;

pub const RESULT_PENDING: &str = "pending";
pub const RESULT_SUCCEEDED: &str = "succeeded";
pub const RESULT_FAILED: &str = "failed";
pub const RESULT_ROLLED_BACK: &str = "rolled_back";

#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct ProjectLiveStateMigration {
    pub project_id: String,
    pub family: String,
    pub release: String,
    pub source_inventory: Value,
    pub destination: String,
    pub pre_hash: Option<String>,
    pub post_hash: Option<String>,
    pub started_at: String,
    pub updated_at: String,
    pub finalized_at: Option<String>,
    pub result: String,
    pub detail: Option<String>,
    pub rollback_instruction: String,
}

#[derive(Clone, Debug)]
pub struct BeginProjectLiveStateMigration<'a> {
    pub project_id: &'a str,
    pub family: &'a str,
    pub release: &'a str,
    /// Structured inventory; in particular read-source families retain every
    /// owner/target/legacy input rather than reducing them to one path.
    pub source_inventory: &'a Value,
    pub destination: &'a str,
    pub pre_hash: Option<&'a str>,
    pub rollback_instruction: &'a str,
}

#[derive(Clone, Debug)]
pub struct MigrationKey<'a> {
    pub project_id: &'a str,
    pub family: &'a str,
    pub release: &'a str,
}

pub struct ProjectLiveStateMigrationRepository {
    db: Database,
}

impl ProjectLiveStateMigrationRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Begin is idempotent. A completed record is returned unchanged; failed
    /// and pending records are refreshed so a restart retains its audit trail.
    pub async fn begin(
        &self,
        input: BeginProjectLiveStateMigration<'_>,
    ) -> Result<ProjectLiveStateMigration> {
        self.db.ensure_initialized().await?;
        sqlx::query(r#"INSERT INTO project_live_state_migrations
            (project_id, family, release, source_inventory, destination, pre_hash, rollback_instruction, result)
            VALUES ($1,$2,$3,$4,$5,$6,$7,'pending')
            ON CONFLICT (project_id, family, release) DO UPDATE SET
              source_inventory = EXCLUDED.source_inventory, destination = EXCLUDED.destination,
              pre_hash = EXCLUDED.pre_hash, rollback_instruction = EXCLUDED.rollback_instruction,
              result = CASE WHEN project_live_state_migrations.result = 'failed' THEN 'pending' ELSE project_live_state_migrations.result END,
              detail = CASE WHEN project_live_state_migrations.result = 'failed' THEN NULL ELSE project_live_state_migrations.detail END,
              updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
            WHERE project_live_state_migrations.result IN ('pending', 'failed')"#)
            .bind(input.project_id).bind(input.family).bind(input.release).bind(input.source_inventory)
            .bind(input.destination).bind(input.pre_hash).bind(input.rollback_instruction)
            .execute(self.db.pool()).await?;
        self.get(MigrationKey {
            project_id: input.project_id,
            family: input.family,
            release: input.release,
        })
        .await?
        .ok_or_else(|| DbError::Internal("migration record disappeared after begin".into()))
    }

    pub async fn get(&self, key: MigrationKey<'_>) -> Result<Option<ProjectLiveStateMigration>> {
        self.db.ensure_initialized().await?;
        Ok(
            sqlx::query_as::<_, ProjectLiveStateMigration>(SELECT_RECORD)
                .bind(key.project_id)
                .bind(key.family)
                .bind(key.release)
                .fetch_optional(self.db.pool())
                .await?,
        )
    }

    /// Pending records are the explicit restart reconciliation worklist.
    pub async fn pending_for_project(
        &self,
        project_id: &str,
    ) -> Result<Vec<ProjectLiveStateMigration>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, ProjectLiveStateMigration>(&format!("{SELECT_RECORD_BASE} WHERE project_id = $1 AND result = 'pending' ORDER BY family, release"))
            .bind(project_id).fetch_all(self.db.pool()).await?)
    }

    pub async fn mark_pending(&self, key: MigrationKey<'_>, detail: Option<&str>) -> Result<()> {
        self.transition(key, RESULT_PENDING, None, detail, false)
            .await
    }
    pub async fn finalize(
        &self,
        key: MigrationKey<'_>,
        post_hash: Option<&str>,
        detail: Option<&str>,
    ) -> Result<()> {
        self.transition(key, RESULT_SUCCEEDED, post_hash, detail, true)
            .await
    }
    pub async fn fail(&self, key: MigrationKey<'_>, detail: &str) -> Result<()> {
        self.transition(key, RESULT_FAILED, None, Some(detail), true)
            .await
    }
    pub async fn rollback(&self, key: MigrationKey<'_>, detail: Option<&str>) -> Result<()> {
        self.transition(key, RESULT_ROLLED_BACK, None, detail, true)
            .await
    }

    async fn transition(
        &self,
        key: MigrationKey<'_>,
        result: &str,
        post_hash: Option<&str>,
        detail: Option<&str>,
        finalized: bool,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        let count = sqlx::query(r#"UPDATE project_live_state_migrations SET
            result = $4, post_hash = COALESCE($5, post_hash), detail = $6,
            finalized_at = CASE WHEN $7 THEN to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') ELSE NULL END,
            updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
            WHERE project_id = $1 AND family = $2 AND release = $3"#)
            .bind(key.project_id).bind(key.family).bind(key.release).bind(result).bind(post_hash).bind(detail).bind(finalized)
            .execute(self.db.pool()).await?.rows_affected();
        if count == 1 {
            Ok(())
        } else {
            Err(DbError::InvalidTransition(format!(
                "cannot transition missing migration {}/{}/{}",
                key.project_id, key.family, key.release
            )))
        }
    }
}

const SELECT_RECORD_BASE: &str = "SELECT project_id, family, release, source_inventory, destination, pre_hash, post_hash, started_at, updated_at, finalized_at, result, detail, rollback_instruction FROM project_live_state_migrations";
const SELECT_RECORD: &str = "SELECT project_id, family, release, source_inventory, destination, pre_hash, post_hash, started_at, updated_at, finalized_at, result, detail, rollback_instruction FROM project_live_state_migrations WHERE project_id = $1 AND family = $2 AND release = $3";

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn begin_finalizes_and_reconciles_structured_read_sources() {
        let db = Database::open_in_memory().unwrap();
        let repo = ProjectLiveStateMigrationRepository::new(db);
        let inventory = serde_json::json!({"sources":[{"kind":"read_source","owner_project_id":"owner","target_project_id":"a","path":"old-a"},{"kind":"read_source","owner_project_id":"owner","target_project_id":"b","path":"old-b"}]});
        let row = repo
            .begin(BeginProjectLiveStateMigration {
                project_id: "owner",
                family: "read_source:a",
                release: "N",
                source_inventory: &inventory,
                destination: ".task-runtime/read-sources/a",
                pre_hash: Some("pre"),
                rollback_instruction: "retain old inputs",
            })
            .await
            .unwrap();
        assert_eq!(row.result, RESULT_PENDING);
        repo.finalize(
            MigrationKey {
                project_id: "owner",
                family: "read_source:a",
                release: "N",
            },
            Some("post"),
            None,
        )
        .await
        .unwrap();
        let final_row = repo
            .get(MigrationKey {
                project_id: "owner",
                family: "read_source:a",
                release: "N",
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(final_row.result, RESULT_SUCCEEDED);
        assert_eq!(final_row.post_hash.as_deref(), Some("post"));
        assert!(repo.pending_for_project("owner").await.unwrap().is_empty());
    }
}
