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
        // NOTE: Dolt reports LONGBLOB columns without the binary-charset
        // flag that sqlx-mysql's macro relies on to pick a `Vec<u8>` decoder,
        // so the `query_as!` form attempts a UTF-8 decode and fails on
        // bincoded payloads. We pull the blob out of a row explicitly.
        use sqlx::Row;
        Ok(sqlx::query(
            "SELECT project_id, commit_sha, graph_blob, built_at
             FROM repo_graph_cache
             WHERE project_id = ? AND commit_sha = ?",
        )
        .bind(project_id)
        .bind(commit_sha)
        .fetch_optional(self.db.pool())
        .await?
        .map(|row| CachedRepoGraph {
            project_id: row.get("project_id"),
            commit_sha: row.get("commit_sha"),
            graph_blob: row.get("graph_blob"),
            built_at: row.get("built_at"),
        }))
    }

    pub async fn upsert(&self, entry: RepoGraphCacheInsert<'_>) -> Result<()> {
        self.db.ensure_initialized().await?;
        // `built_at` defaults to "" in the schema; stamp it explicitly so the
        // row carries a usable ISO-8601 timestamp on first insert.
        sqlx::query!(
            "INSERT INTO repo_graph_cache
             (project_id, commit_sha, graph_blob, built_at)
             VALUES (?, ?, ?, DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ'))
             ON DUPLICATE KEY UPDATE
                graph_blob=VALUES(graph_blob),
                built_at=VALUES(built_at)",
            entry.project_id,
            entry.commit_sha,
            entry.graph_blob,
        )
        .execute(self.db.pool())
        .await?;
        // Stamp the project row so the coordinator's dispatch gate can see
        // that the warm pipeline has completed at least once for this project.
        // Intentionally best-effort: a stamp failure must not roll back the
        // cache write, so we log and continue. Uses the non-macro
        // `sqlx::query` form so builds work on databases that haven't yet
        // applied migration 9.
        if let Err(e) = sqlx::query(
            "UPDATE projects
               SET graph_warmed_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
        )
        .bind(entry.project_id)
        .execute(self.db.pool())
        .await
        {
            tracing::warn!(
                project_id = %entry.project_id,
                error = %e,
                "RepoGraphCacheRepository::upsert: failed to stamp projects.graph_warmed_at"
            );
        }
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
    async fn upsert_stamps_projects_graph_warmed_at() {
        use crate::repositories::project::ProjectRepository;
        use djinn_core::events::EventBus;
        let db = Database::open_in_memory().expect("in-memory db");
        let repo = RepoGraphCacheRepository::new(db.clone());
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = project_repo
            .create("proj-warm-stamp", "/tmp/djinn-tests/warm-stamp")
            .await
            .expect("create project");

        let before = project_repo
            .get_dispatch_readiness(&project.id)
            .await
            .expect("readiness")
            .expect("exists");
        assert!(
            before.graph_warmed_at.is_none(),
            "graph_warmed_at should be NULL before the first warm"
        );

        repo.upsert(RepoGraphCacheInsert {
            project_id: &project.id,
            commit_sha: "abc",
            graph_blob: b"graph",
        })
        .await
        .expect("upsert");

        let after = project_repo
            .get_dispatch_readiness(&project.id)
            .await
            .expect("readiness")
            .expect("exists");
        assert!(
            after.graph_warmed_at.is_some(),
            "graph_warmed_at must be stamped after a cache upsert"
        );
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
