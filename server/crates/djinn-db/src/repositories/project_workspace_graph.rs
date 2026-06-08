use crate::Result;
use crate::database::Database;

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct ProjectWorkspaceGraph {
    pub project_id: String,
    pub workspace_slug: String,
    pub commit_sha: String,
    pub warmed_at: String,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectWorkspaceGraphUpsert<'a> {
    pub project_id: &'a str,
    pub workspace_slug: &'a str,
    pub commit_sha: &'a str,
    pub status: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct ProjectWorkspaceGraphLatest {
    pub warmed_at: String,
    pub status: String,
}

pub struct ProjectWorkspaceGraphRepository {
    db: Database,
}

impl ProjectWorkspaceGraphRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn get(
        &self,
        project_id: &str,
        workspace_slug: &str,
    ) -> Result<Option<ProjectWorkspaceGraph>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, ProjectWorkspaceGraph>(
            r#"SELECT project_id, workspace_slug, commit_sha, warmed_at, status
               FROM project_workspace_graph
              WHERE project_id = $1 AND workspace_slug = $2"#,
        )
        .bind(project_id)
        .bind(workspace_slug)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn upsert(&self, row: ProjectWorkspaceGraphUpsert<'_>) -> Result<()> {
        self.db.ensure_initialized().await?;
        self.upsert_initialized(row).await
    }

    pub async fn upsert_many(&self, rows: &[ProjectWorkspaceGraphUpsert<'_>]) -> Result<()> {
        self.db.ensure_initialized().await?;
        for row in rows {
            self.upsert_initialized(ProjectWorkspaceGraphUpsert {
                project_id: row.project_id,
                workspace_slug: row.workspace_slug,
                commit_sha: row.commit_sha,
                status: row.status,
            })
            .await?;
        }
        Ok(())
    }

    pub async fn has_any_for_project(&self, project_id: &str) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let exists = sqlx::query_scalar::<_, bool>(
            r#"SELECT EXISTS(
                   SELECT 1 FROM project_workspace_graph WHERE project_id = $1
               )"#,
        )
        .bind(project_id)
        .fetch_one(self.db.pool())
        .await?;
        Ok(exists)
    }

    pub async fn latest_for_project(
        &self,
        project_id: &str,
    ) -> Result<Option<ProjectWorkspaceGraph>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, ProjectWorkspaceGraph>(
            r#"SELECT project_id, workspace_slug, commit_sha, warmed_at, status
               FROM project_workspace_graph
              WHERE project_id = $1
              ORDER BY warmed_at DESC, workspace_slug ASC
              LIMIT 1"#,
        )
        .bind(project_id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn latest_warmed_state(
        &self,
        project_id: &str,
    ) -> Result<Option<ProjectWorkspaceGraphLatest>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, ProjectWorkspaceGraphLatest>(
            r#"SELECT warmed_at, status
               FROM project_workspace_graph
              WHERE project_id = $1
              ORDER BY warmed_at DESC, workspace_slug ASC
              LIMIT 1"#,
        )
        .bind(project_id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    async fn upsert_initialized(&self, row: ProjectWorkspaceGraphUpsert<'_>) -> Result<()> {
        sqlx::query(r#"INSERT INTO project_workspace_graph
                   (project_id, workspace_slug, commit_sha, warmed_at, status)
               VALUES
                   ($1, $2, $3, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'), $4)
               ON CONFLICT (project_id, workspace_slug) DO UPDATE SET
                   commit_sha = EXCLUDED.commit_sha,
                   warmed_at = EXCLUDED.warmed_at,
                   status = EXCLUDED.status"#)
        .bind(row.project_id)
        .bind(row.workspace_slug)
        .bind(row.commit_sha)
        .bind(row.status)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fresh() -> ProjectWorkspaceGraphRepository {
        let db = Database::open_in_memory().expect("in-memory db");
        ProjectWorkspaceGraphRepository::new(db)
    }

    async fn seed_project(repo: &ProjectWorkspaceGraphRepository, project_id: &str) {
        repo.db.ensure_initialized().await.expect("init db");
        sqlx::query(
            "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
        )
        .bind(project_id)
        .bind(project_id)
        .bind("test")
        .bind(format!("repo-{project_id}"))
        .execute(repo.db.pool())
        .await
        .expect("seed project");
    }

    #[tokio::test]
    async fn upsert_and_get_round_trip() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;

        repo.upsert(ProjectWorkspaceGraphUpsert {
            project_id: "p1",
            workspace_slug: "root",
            commit_sha: "abc123",
            status: "ready",
        })
        .await
        .expect("upsert");

        let row = repo.get("p1", "root").await.expect("get").expect("hit");
        assert_eq!(row.project_id, "p1");
        assert_eq!(row.workspace_slug, "root");
        assert_eq!(row.commit_sha, "abc123");
        assert_eq!(row.status, "ready");
        assert!(!row.warmed_at.is_empty());
    }

    #[tokio::test]
    async fn upsert_replaces_row_for_same_project_and_workspace() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;

        repo.upsert(ProjectWorkspaceGraphUpsert {
            project_id: "p1",
            workspace_slug: "root",
            commit_sha: "old",
            status: "ready",
        })
        .await
        .expect("upsert old");
        repo.upsert(ProjectWorkspaceGraphUpsert {
            project_id: "p1",
            workspace_slug: "root",
            commit_sha: "new",
            status: "ready",
        })
        .await
        .expect("upsert new");

        let row = repo.get("p1", "root").await.expect("get").expect("hit");
        assert_eq!(row.commit_sha, "new");
        assert_eq!(row.status, "ready");
    }

    #[tokio::test]
    async fn upsert_many_inserts_multiple_workspace_rows() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;

        repo.upsert_many(&[
            ProjectWorkspaceGraphUpsert {
                project_id: "p1",
                workspace_slug: "root",
                commit_sha: "abc",
                status: "ready",
            },
            ProjectWorkspaceGraphUpsert {
                project_id: "p1",
                workspace_slug: "api",
                commit_sha: "abc",
                status: "ready",
            },
        ])
        .await
        .expect("upsert many");

        assert!(repo.get("p1", "root").await.expect("get root").is_some());
        assert!(repo.get("p1", "api").await.expect("get api").is_some());
    }

    #[tokio::test]
    async fn project_existence_and_no_row_behavior() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;

        assert!(!repo.has_any_for_project("p1").await.expect("has any"));
        assert!(repo.get("p1", "missing").await.expect("get").is_none());
        assert!(
            repo.latest_for_project("p1")
                .await
                .expect("latest")
                .is_none()
        );
        assert!(
            repo.latest_warmed_state("p1")
                .await
                .expect("latest state")
                .is_none()
        );

        repo.upsert(ProjectWorkspaceGraphUpsert {
            project_id: "p1",
            workspace_slug: "root",
            commit_sha: "abc",
            status: "ready",
        })
        .await
        .expect("upsert");

        assert!(repo.has_any_for_project("p1").await.expect("has any"));
    }

    #[tokio::test]
    async fn latest_warmed_state_returns_most_recent_workspace_status() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;

        repo.upsert(ProjectWorkspaceGraphUpsert {
            project_id: "p1",
            workspace_slug: "old",
            commit_sha: "old-sha",
            status: "ready",
        })
        .await
        .expect("upsert old");
        sqlx::query(r#"UPDATE project_workspace_graph
                  SET warmed_at = to_char((now() at time zone 'utc') - interval '1 day', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                WHERE project_id = $1 AND workspace_slug = $2"#)
        .bind("p1")
        .bind("old")
        .execute(repo.db.pool())
        .await
        .expect("age old row");

        repo.upsert(ProjectWorkspaceGraphUpsert {
            project_id: "p1",
            workspace_slug: "new",
            commit_sha: "new-sha",
            status: "ready",
        })
        .await
        .expect("upsert new");

        let latest = repo
            .latest_for_project("p1")
            .await
            .expect("latest")
            .expect("latest row");
        assert_eq!(latest.workspace_slug, "new");
        assert_eq!(latest.commit_sha, "new-sha");
        assert_eq!(latest.status, "ready");

        let state = repo
            .latest_warmed_state("p1")
            .await
            .expect("latest state")
            .expect("state");
        assert_eq!(state.warmed_at, latest.warmed_at);
        assert_eq!(state.status, "ready");
    }
}
