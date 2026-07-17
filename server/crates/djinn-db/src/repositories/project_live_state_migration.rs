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

    async fn fresh() -> ProjectLiveStateMigrationRepository {
        ProjectLiveStateMigrationRepository::new(Database::open_in_memory().expect("in-memory db"))
    }

    async fn seed_project(repo: &ProjectLiveStateMigrationRepository, project_id: &str) {
        repo.db.ensure_initialized().await.expect("initialize db");
        sqlx::query(
            "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, 'test', $2)",
        )
        .bind(project_id)
        .bind(project_id)
        .execute(repo.db.pool())
        .await
        .expect("seed project");
    }

    fn key<'a>(project_id: &'a str, family: &'a str) -> MigrationKey<'a> {
        MigrationKey { project_id, family, release: "N" }
    }

    async fn begin_read_source(
        repo: &ProjectLiveStateMigrationRepository,
        owner: &str,
        target: &str,
        inventory: &Value,
    ) -> ProjectLiveStateMigration {
        repo.begin(BeginProjectLiveStateMigration {
            project_id: owner,
            family: &format!("read_source:{target}"),
            release: "N",
            source_inventory: inventory,
            destination: &format!(".task-runtime/read-sources/{target}"),
            pre_hash: Some("pre"),
            rollback_instruction: "retain old inputs",
        })
        .await
        .expect("begin migration")
    }

    #[tokio::test]
    async fn lifecycle_covers_pending_failure_restart_finalize_and_rollback() {
        let repo = fresh().await;
        seed_project(&repo, "owner").await;
        let inventory = serde_json::json!({"sources":[{"path":"old-a"}]});
        let row = begin_read_source(&repo, "owner", "a", &inventory).await;
        assert_eq!(row.result, RESULT_PENDING);
        let persisted = sqlx::query!(
            "SELECT result FROM project_live_state_migrations WHERE project_id = $1 AND family = $2 AND release = $3",
            "owner",
            "read_source:a",
            "N",
        )
        .fetch_one(repo.db.pool())
        .await
        .expect("read persisted pending migration");
        assert_eq!(persisted.result, RESULT_PENDING);

        repo.mark_pending(key("owner", "read_source:a"), Some("restart inspection"))
            .await
            .expect("mark pending");
        assert_eq!(repo.pending_for_project("owner").await.unwrap().len(), 1);

        repo.fail(key("owner", "read_source:a"), "copy failed")
            .await
            .expect("record failure");
        let failed = repo.get(key("owner", "read_source:a")).await.unwrap().unwrap();
        assert_eq!(failed.result, RESULT_FAILED);
        assert_eq!(failed.detail.as_deref(), Some("copy failed"));

        let restarted = begin_read_source(&repo, "owner", "a", &inventory).await;
        assert_eq!(restarted.result, RESULT_PENDING);
        assert!(restarted.detail.is_none());

        repo.finalize(
            key("owner", "read_source:a"),
            Some("post"),
            None,
        )
        .await
        .expect("finalize migration");
        let final_row = repo
            .get(key("owner", "read_source:a"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(final_row.result, RESULT_SUCCEEDED);
        assert_eq!(final_row.post_hash.as_deref(), Some("post"));
        assert!(repo.pending_for_project("owner").await.unwrap().is_empty());

        repo.rollback(key("owner", "read_source:a"), Some("restore retained input"))
            .await
            .expect("record rollback");
        let rolled_back = repo.get(key("owner", "read_source:a")).await.unwrap().unwrap();
        assert_eq!(rolled_back.result, RESULT_ROLLED_BACK);
        assert_eq!(rolled_back.detail.as_deref(), Some("restore retained input"));
    }

    #[tokio::test]
    async fn completed_reentry_is_idempotent_and_retains_its_audit_inputs() {
        let repo = fresh().await;
        seed_project(&repo, "owner").await;
        let original = serde_json::json!({"sources":[{"path":"old-a"}]});
        begin_read_source(&repo, "owner", "a", &original).await;
        repo.finalize(key("owner", "read_source:a"), Some("post"), Some("published"))
            .await
            .expect("finalize migration");

        let replacement = serde_json::json!({"sources":[{"path":"different-input"}]});
        let row = begin_read_source(&repo, "owner", "a", &replacement).await;
        assert_eq!(row.result, RESULT_SUCCEEDED);
        assert_eq!(row.source_inventory, original);
        assert_eq!(row.destination, ".task-runtime/read-sources/a");
        assert_eq!(row.post_hash.as_deref(), Some("post"));
    }

    #[tokio::test]
    async fn read_source_records_distinguish_targets_owners_and_dual_inputs() {
        let repo = fresh().await;
        seed_project(&repo, "owner-one").await;
        seed_project(&repo, "owner-two").await;
        let dual_source_inventory = serde_json::json!({"sources":[
            {"kind":"project_legacy","owner_project_id":"owner-one","target_project_id":"target-a","path":".djinn/read-sources/target-a"},
            {"kind":"task_legacy","owner_project_id":"owner-one","target_project_id":"target-a","path":"worktree/.djinn-read-sources/target-a"}
        ]});
        let target_b_inventory = serde_json::json!({"sources":[
            {"kind":"read_source","owner_project_id":"owner-one","target_project_id":"target-b","path":"old-b"}
        ]});
        let second_owner_inventory = serde_json::json!({"sources":[
            {"kind":"read_source","owner_project_id":"owner-two","target_project_id":"target-a","path":"other-old-a"}
        ]});

        let dual = begin_read_source(&repo, "owner-one", "target-a", &dual_source_inventory).await;
        begin_read_source(&repo, "owner-one", "target-b", &target_b_inventory).await;
        begin_read_source(&repo, "owner-two", "target-a", &second_owner_inventory).await;

        assert_eq!(dual.source_inventory["sources"].as_array().unwrap().len(), 2);
        assert_eq!(repo.pending_for_project("owner-one").await.unwrap().len(), 2);
        assert_eq!(repo.pending_for_project("owner-two").await.unwrap().len(), 1);
        assert_eq!(
            repo.get(key("owner-two", "read_source:target-a"))
                .await
                .unwrap()
                .unwrap()
                .destination,
            ".task-runtime/read-sources/target-a"
        );
    }

    #[tokio::test]
    async fn failed_finalization_remains_pending_and_can_be_reconciled() {
        let repo = fresh().await;
        seed_project(&repo, "owner").await;
        let inventory = serde_json::json!({"sources":[{"path":"old-a"}]});
        begin_read_source(&repo, "owner", "a", &inventory).await;

        sqlx::query(
            "ALTER TABLE project_live_state_migrations ADD CONSTRAINT reject_success_for_test CHECK (result <> 'succeeded')",
        )
        .execute(repo.db.pool())
        .await
        .expect("inject finalization failure");
        assert!(repo
            .finalize(key("owner", "read_source:a"), Some("post"), None)
            .await
            .is_err());
        let pending = repo.get(key("owner", "read_source:a")).await.unwrap().unwrap();
        assert_eq!(pending.result, RESULT_PENDING);
        assert!(pending.post_hash.is_none());

        sqlx::query(
            "ALTER TABLE project_live_state_migrations DROP CONSTRAINT reject_success_for_test",
        )
        .execute(repo.db.pool())
        .await
        .expect("remove injected failure");
        repo.finalize(key("owner", "read_source:a"), Some("post"), None)
            .await
            .expect("restart reconciliation finalizes pending record");
    }
}
