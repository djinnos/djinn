use crate::Result;
use crate::database::Database;

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct CachedVerification {
    pub output: String,
    pub duration_ms: i64,
    pub created_at: String,
}

pub struct VerificationCacheRepository {
    db: Database,
}

impl VerificationCacheRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn get(
        &self,
        project_id: &str,
        commit_sha: &str,
    ) -> Result<Option<CachedVerification>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            CachedVerification,
            r#"SELECT output, duration_ms AS "duration_ms!: i64", created_at FROM verification_cache WHERE project_id = ? AND commit_sha = ?"#,
            project_id,
            commit_sha,
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn insert(
        &self,
        project_id: &str,
        commit_sha: &str,
        output_json: &str,
        duration_ms: i64,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "INSERT INTO verification_cache (project_id, commit_sha, output, duration_ms) VALUES (?, ?, ?, ?) \
             ON DUPLICATE KEY UPDATE output=VALUES(output), duration_ms=VALUES(duration_ms)",
            project_id,
            commit_sha,
            output_json,
            duration_ms,
        )
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn invalidate_project(&self, project_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "DELETE FROM verification_cache WHERE project_id = ?",
            project_id
        )
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn prune_older_than(&self, days: i64) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "DELETE FROM verification_cache WHERE created_at < DATE_SUB(NOW(3), INTERVAL ? DAY)",
            days,
        )
        .execute(self.db.pool())
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;

    async fn test_repo() -> VerificationCacheRepository {
        let db = Database::open_in_memory().expect("in-memory db");
        VerificationCacheRepository::new(db)
    }

    /// Create the project rows referenced by `verification_cache.project_id`.
    /// The cache table carries a FK to `projects` on MySQL/Dolt, so inserting
    /// a cache row for "p1"/"p2" without the parent row is a constraint
    /// violation (SQLite quietly allowed it in the previous backend).
    async fn seed_projects(db: &Database, ids: &[&str]) {
        db.ensure_initialized().await.unwrap();
        for id in ids {
            let path = format!("/tmp/verif-cache-{id}");
            sqlx::query!(
                "INSERT INTO projects (id, name, path, verification_rules) VALUES (?, ?, ?, ?)",
                id,
                id,
                path,
                "[]",
            )
            .execute(db.pool())
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn insert_and_get_round_trip() {
        let repo = test_repo().await;
        seed_projects(&repo.db, &["p1"]).await;
        repo.insert("p1", "abc123", "[{\"ok\":true}]", 42)
            .await
            .expect("insert");

        let cached = repo.get("p1", "abc123").await.expect("get").expect("hit");
        assert_eq!(cached.output, "[{\"ok\":true}]");
        assert_eq!(cached.duration_ms, 42);
        assert!(!cached.created_at.is_empty());
    }

    #[tokio::test]
    async fn cache_miss_returns_none() {
        let repo = test_repo().await;
        let cached = repo.get("missing", "sha").await.expect("get");
        assert!(cached.is_none());
    }

    #[tokio::test]
    async fn invalidate_project_deletes_only_project_rows() {
        let repo = test_repo().await;
        seed_projects(&repo.db, &["p1", "p2"]).await;
        repo.insert("p1", "a1", "[]", 10).await.expect("insert p1");
        repo.insert("p1", "a2", "[]", 20).await.expect("insert p1");
        repo.insert("p2", "b1", "[]", 30).await.expect("insert p2");

        repo.invalidate_project("p1").await.expect("invalidate");

        assert!(repo.get("p1", "a1").await.expect("get").is_none());
        assert!(repo.get("p1", "a2").await.expect("get").is_none());
        assert!(repo.get("p2", "b1").await.expect("get").is_some());
    }

    #[tokio::test]
    async fn prune_older_than_deletes_old_rows() {
        let repo = test_repo().await;
        seed_projects(&repo.db, &["p1"]).await;

        repo.insert("p1", "old", "[]", 1).await.expect("insert old");
        sqlx::query!(
            "UPDATE verification_cache SET created_at = DATE_SUB(NOW(3), INTERVAL 10 DAY) WHERE project_id = ? AND commit_sha = ?",
            "p1",
            "old",
        )
        .execute(repo.db.pool())
        .await
        .expect("age old row");

        repo.insert("p1", "new", "[]", 2).await.expect("insert new");

        repo.prune_older_than(5).await.expect("prune");

        assert!(repo.get("p1", "old").await.expect("get old").is_none());
        assert!(repo.get("p1", "new").await.expect("get new").is_some());
    }
}
