//! Per-commit canonical SCIP graph cache (ADR-050 §3 Chunk C).
//!
//! This is a separate store from `repo_map_cache`: under ADR-050 the
//! interactive code graph used by `code_graph` is built once per
//! `origin/main` commit and shared across every architect/chat session and
//! every worker dispatch until `origin/main` advances.  The graph blob is
//! produced by `RepoDependencyGraph::serialize_artifact` (or any other
//! serde-compatible encoding the caller chooses).

use crate::Result;
use crate::database::Database;

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct CachedRepoGraph {
    pub project_id: String,
    pub commit_sha: String,
    pub graph_blob: Vec<u8>,
    pub built_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoGraphCacheInsert<'a> {
    pub project_id: &'a str,
    pub commit_sha: &'a str,
    pub graph_blob: &'a [u8],
}

pub struct RepoGraphCacheRepository {
    db: Database,
}

impl RepoGraphCacheRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn get(&self, project_id: &str, commit_sha: &str) -> Result<Option<CachedRepoGraph>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, CachedRepoGraph>(
            "SELECT project_id, commit_sha, graph_blob, built_at
             FROM repo_graph_cache
             WHERE project_id = ? AND commit_sha = ?",
        )
        .bind(project_id)
        .bind(commit_sha)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn upsert(&self, entry: RepoGraphCacheInsert<'_>) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "INSERT INTO repo_graph_cache
             (project_id, commit_sha, graph_blob)
             VALUES (?, ?, ?)
             ON DUPLICATE KEY UPDATE graph_blob=VALUES(graph_blob)",
        )
        .bind(entry.project_id)
        .bind(entry.commit_sha)
        .bind(entry.graph_blob)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;

    async fn fresh() -> RepoGraphCacheRepository {
        let db = Database::open_in_memory().expect("in-memory db");
        RepoGraphCacheRepository::new(db)
    }

    #[tokio::test]
    async fn upsert_and_get_round_trip() {
        let repo = fresh().await;
        let blob = b"\x00\x01\x02serialized-graph";
        repo.upsert(RepoGraphCacheInsert {
            project_id: "p1",
            commit_sha: "abc123",
            graph_blob: blob,
        })
        .await
        .expect("upsert");

        let cached = repo.get("p1", "abc123").await.expect("get").expect("hit");
        assert_eq!(cached.graph_blob, blob);
        assert_eq!(cached.project_id, "p1");
        assert_eq!(cached.commit_sha, "abc123");
        assert!(!cached.built_at.is_empty());
    }

    #[tokio::test]
    async fn get_returns_none_for_unknown_commit() {
        let repo = fresh().await;
        repo.upsert(RepoGraphCacheInsert {
            project_id: "p1",
            commit_sha: "abc123",
            graph_blob: b"x",
        })
        .await
        .expect("upsert");
        let miss = repo.get("p1", "def456").await.expect("get");
        assert!(miss.is_none());
    }

    #[tokio::test]
    async fn upsert_overwrites_existing_entry() {
        let repo = fresh().await;
        repo.upsert(RepoGraphCacheInsert {
            project_id: "p1",
            commit_sha: "abc",
            graph_blob: b"v1",
        })
        .await
        .expect("upsert v1");
        repo.upsert(RepoGraphCacheInsert {
            project_id: "p1",
            commit_sha: "abc",
            graph_blob: b"v2",
        })
        .await
        .expect("upsert v2");
        let cached = repo.get("p1", "abc").await.expect("get").expect("hit");
        assert_eq!(cached.graph_blob, b"v2");
    }
}
