use crate::Result;
use crate::database::Database;

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct CachedRepoMap {
    pub project_id: String,
    pub project_path: String,
    pub worktree_path: Option<String>,
    pub commit_sha: String,
    pub rendered_map: String,
    pub token_estimate: i64,
    pub included_entries: i64,
    pub graph_artifact: Option<String>,
    pub created_at: String,
}

/// Internal row shape that mirrors the MySQL/Dolt schema (non-null worktree_path).
#[derive(Debug, sqlx::FromRow)]
struct CachedRepoMapRow {
    project_id: String,
    project_path: String,
    worktree_path: String,
    commit_sha: String,
    rendered_map: String,
    token_estimate: i64,
    included_entries: i64,
    graph_artifact: Option<String>,
    created_at: String,
}

impl From<CachedRepoMapRow> for CachedRepoMap {
    fn from(row: CachedRepoMapRow) -> Self {
        CachedRepoMap {
            project_id: row.project_id,
            project_path: row.project_path,
            worktree_path: if row.worktree_path.is_empty() {
                None
            } else {
                Some(row.worktree_path)
            },
            commit_sha: row.commit_sha,
            rendered_map: row.rendered_map,
            token_estimate: row.token_estimate,
            included_entries: row.included_entries,
            graph_artifact: row.graph_artifact,
            created_at: row.created_at,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoMapCacheKey<'a> {
    pub project_id: &'a str,
    pub project_path: &'a str,
    pub worktree_path: Option<&'a str>,
    pub commit_sha: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoMapCacheInsert<'a> {
    pub key: RepoMapCacheKey<'a>,
    pub rendered_map: &'a str,
    pub token_estimate: i64,
    pub included_entries: i64,
    pub graph_artifact: Option<&'a str>,
}

pub struct RepoMapCacheRepository {
    db: Database,
}

impl RepoMapCacheRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn get(&self, key: RepoMapCacheKey<'_>) -> Result<Option<CachedRepoMap>> {
        self.db.ensure_initialized().await?;
        // `worktree_path` is stored as a non-null VARCHAR on MySQL/Dolt where
        // the empty string `""` encodes "no worktree" (NULL would break the
        // PK on (project_id, project_path, worktree_path, commit_sha)). The
        // Rust API keeps `Option<String>` for ergonomics so we normalize on
        // the way in and on the way out.
        let worktree_param = key.worktree_path.unwrap_or("");
        let row = sqlx::query_as!(
            CachedRepoMapRow,
            r#"SELECT project_id, project_path, worktree_path, commit_sha, rendered_map, token_estimate, included_entries, graph_artifact, created_at
             FROM repo_map_cache
             WHERE project_id = ?
               AND project_path = ?
               AND worktree_path = ?
               AND commit_sha = ?"#,
            key.project_id,
            key.project_path,
            worktree_param,
            key.commit_sha,
        )
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(CachedRepoMap::from))
    }

    pub async fn get_by_commit_prefer_canonical(
        &self,
        project_id: &str,
        project_path: &str,
        commit_sha: &str,
    ) -> Result<Option<CachedRepoMap>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query_as!(
            CachedRepoMapRow,
            // Canonical entries use `worktree_path = ''`; worktree-scoped
            // entries use a non-empty path. Prefer canonical first.
            r#"SELECT project_id, project_path, worktree_path, commit_sha, rendered_map, token_estimate, included_entries, graph_artifact, created_at
             FROM repo_map_cache
             WHERE project_id = ?
               AND project_path = ?
               AND commit_sha = ?
             ORDER BY CASE WHEN worktree_path = '' THEN 0 ELSE 1 END, created_at DESC
             LIMIT 1"#,
            project_id,
            project_path,
            commit_sha,
        )
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(CachedRepoMap::from))
    }

    pub async fn insert(&self, entry: RepoMapCacheInsert<'_>) -> Result<()> {
        self.db.ensure_initialized().await?;
        // `worktree_path` is part of the PK and NOT NULL on MySQL/Dolt; we
        // use the empty string as the canonical-entry sentinel.
        let worktree_param = entry.key.worktree_path.unwrap_or("");
        sqlx::query!(
            "INSERT INTO repo_map_cache (
                project_id,
                project_path,
                worktree_path,
                commit_sha,
                rendered_map,
                token_estimate,
                included_entries,
                graph_artifact
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON DUPLICATE KEY UPDATE
                rendered_map=VALUES(rendered_map),
                token_estimate=VALUES(token_estimate),
                included_entries=VALUES(included_entries),
                graph_artifact=VALUES(graph_artifact)",
            entry.key.project_id,
            entry.key.project_path,
            worktree_param,
            entry.key.commit_sha,
            entry.rendered_map,
            entry.token_estimate,
            entry.included_entries,
            entry.graph_artifact,
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

    async fn test_repo() -> RepoMapCacheRepository {
        let db = Database::open_in_memory().expect("in-memory db");
        RepoMapCacheRepository::new(db)
    }

    #[tokio::test]
    async fn insert_and_get_round_trip() {
        let repo = test_repo().await;
        repo.insert(RepoMapCacheInsert {
            key: RepoMapCacheKey {
                project_id: "p1",
                project_path: "/repo",
                worktree_path: None,
                commit_sha: "abc123",
            },
            rendered_map: "src/main.rs\n  fn main()",
            token_estimate: 24,
            included_entries: 2,
            graph_artifact: None,
        })
        .await
        .expect("insert");

        let cached = repo
            .get(RepoMapCacheKey {
                project_id: "p1",
                project_path: "/repo",
                worktree_path: None,
                commit_sha: "abc123",
            })
            .await
            .expect("get")
            .expect("hit");
        assert_eq!(cached.rendered_map, "src/main.rs\n  fn main()");
        assert_eq!(cached.token_estimate, 24);
        assert_eq!(cached.included_entries, 2);
        assert_eq!(cached.project_id, "p1");
        assert_eq!(cached.project_path, "/repo");
        assert_eq!(cached.worktree_path, None);
        assert_eq!(cached.commit_sha, "abc123");
        assert!(!cached.created_at.is_empty());
    }

    #[tokio::test]
    async fn cache_miss_returns_none_for_different_commit_hash() {
        let repo = test_repo().await;
        repo.insert(RepoMapCacheInsert {
            key: RepoMapCacheKey {
                project_id: "p1",
                project_path: "/repo",
                worktree_path: Some("/repo/.djinn/worktrees/t1"),
                commit_sha: "abc123",
            },
            rendered_map: "map",
            token_estimate: 10,
            included_entries: 1,
            graph_artifact: None,
        })
        .await
        .expect("insert");

        let cached = repo
            .get(RepoMapCacheKey {
                project_id: "p1",
                project_path: "/repo",
                worktree_path: Some("/repo/.djinn/worktrees/t1"),
                commit_sha: "def456",
            })
            .await
            .expect("get");
        assert!(cached.is_none());
    }

    #[tokio::test]
    async fn cache_miss_returns_none_for_different_worktree_identity() {
        let repo = test_repo().await;
        repo.insert(RepoMapCacheInsert {
            key: RepoMapCacheKey {
                project_id: "p1",
                project_path: "/repo",
                worktree_path: Some("/repo/.djinn/worktrees/t1"),
                commit_sha: "abc123",
            },
            rendered_map: "map",
            token_estimate: 10,
            included_entries: 1,
            graph_artifact: None,
        })
        .await
        .expect("insert");

        let cached = repo
            .get(RepoMapCacheKey {
                project_id: "p1",
                project_path: "/repo",
                worktree_path: Some("/repo/.djinn/worktrees/t2"),
                commit_sha: "abc123",
            })
            .await
            .expect("get");
        assert!(cached.is_none());
    }

    #[tokio::test]
    async fn commit_lookup_prefers_canonical_entry_over_worktree_entry() {
        let repo = test_repo().await;
        repo.insert(RepoMapCacheInsert {
            key: RepoMapCacheKey {
                project_id: "p1",
                project_path: "/repo",
                worktree_path: Some("/repo/.djinn/worktrees/t1"),
                commit_sha: "abc123",
            },
            rendered_map: "worktree-map",
            token_estimate: 10,
            included_entries: 1,
            graph_artifact: None,
        })
        .await
        .expect("insert worktree entry");
        repo.insert(RepoMapCacheInsert {
            key: RepoMapCacheKey {
                project_id: "p1",
                project_path: "/repo",
                worktree_path: None,
                commit_sha: "abc123",
            },
            rendered_map: "canonical-map",
            token_estimate: 12,
            included_entries: 2,
            graph_artifact: None,
        })
        .await
        .expect("insert canonical entry");

        let cached = repo
            .get_by_commit_prefer_canonical("p1", "/repo", "abc123")
            .await
            .expect("lookup")
            .expect("hit");

        assert_eq!(cached.rendered_map, "canonical-map");
        assert_eq!(cached.worktree_path, None);
    }

    #[tokio::test]
    async fn commit_lookup_returns_worktree_entry_when_canonical_missing() {
        let repo = test_repo().await;
        repo.insert(RepoMapCacheInsert {
            key: RepoMapCacheKey {
                project_id: "p1",
                project_path: "/repo",
                worktree_path: Some("/repo/.djinn/worktrees/t1"),
                commit_sha: "abc123",
            },
            rendered_map: "worktree-map",
            token_estimate: 10,
            included_entries: 1,
            graph_artifact: None,
        })
        .await
        .expect("insert worktree entry");

        let cached = repo
            .get_by_commit_prefer_canonical("p1", "/repo", "abc123")
            .await
            .expect("lookup")
            .expect("hit");

        assert_eq!(cached.rendered_map, "worktree-map");
        assert_eq!(
            cached.worktree_path,
            Some("/repo/.djinn/worktrees/t1".to_string())
        );
    }

    #[tokio::test]
    async fn commit_lookup_is_scoped_to_project_path_and_commit() {
        let repo = test_repo().await;
        repo.insert(RepoMapCacheInsert {
            key: RepoMapCacheKey {
                project_id: "p1",
                project_path: "/repo",
                worktree_path: None,
                commit_sha: "abc123",
            },
            rendered_map: "canonical-map",
            token_estimate: 12,
            included_entries: 2,
            graph_artifact: None,
        })
        .await
        .expect("insert canonical entry");

        assert!(
            repo.get_by_commit_prefer_canonical("p1", "/repo", "other-commit")
                .await
                .expect("lookup other commit")
                .is_none()
        );
        assert!(
            repo.get_by_commit_prefer_canonical("p1", "/other-repo", "abc123")
                .await
                .expect("lookup other project path")
                .is_none()
        );
    }

    #[tokio::test]
    async fn entry_without_graph_artifact_returns_none_artifact() {
        let repo = test_repo().await;
        repo.insert(RepoMapCacheInsert {
            key: RepoMapCacheKey {
                project_id: "p1",
                project_path: "/repo",
                worktree_path: None,
                commit_sha: "abc123",
            },
            rendered_map: "map",
            token_estimate: 10,
            included_entries: 1,
            graph_artifact: None,
        })
        .await
        .expect("insert");

        let cached = repo
            .get(RepoMapCacheKey {
                project_id: "p1",
                project_path: "/repo",
                worktree_path: None,
                commit_sha: "abc123",
            })
            .await
            .expect("get")
            .expect("hit");
        assert!(cached.graph_artifact.is_none());
    }

    #[tokio::test]
    async fn graph_artifact_persisted_and_loaded() {
        let repo = test_repo().await;
        let artifact_json = r#"{"nodes":[],"edges":[]}"#;
        repo.insert(RepoMapCacheInsert {
            key: RepoMapCacheKey {
                project_id: "p1",
                project_path: "/repo",
                worktree_path: None,
                commit_sha: "abc123",
            },
            rendered_map: "map",
            token_estimate: 10,
            included_entries: 1,
            graph_artifact: Some(artifact_json),
        })
        .await
        .expect("insert");

        let cached = repo
            .get(RepoMapCacheKey {
                project_id: "p1",
                project_path: "/repo",
                worktree_path: None,
                commit_sha: "abc123",
            })
            .await
            .expect("get")
            .expect("hit");
        assert_eq!(cached.graph_artifact.as_deref(), Some(artifact_json));
    }
}
