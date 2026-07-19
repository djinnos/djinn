use crate::Result;
use crate::database::Database;

/// Synthetic workspace slug used when a project has no indexable code stack.
///
/// Such projects have no SCIP workspace to warm and no merged graph blob to
/// cache, but consumers still need durable freshness showing that the warm
/// pipeline intentionally found "nothing to index" rather than remaining cold
/// forever.
pub const CODELESS_WORKSPACE_SLUG: &str = "__djinn_no_code__";

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

    /// Persist a warm run's freshness rows as a *replace-set*: upsert every
    /// row in `rows` and, in the same transaction, delete any existing row for
    /// `project_id` whose `workspace_slug` is not among them.
    ///
    /// Plain [`Self::upsert_many`] only ever writes — it can never retire a
    /// workspace that no longer exists. When workspace discovery stops emitting
    /// a slug (e.g. a `root` row from before per-workspace discovery, or a
    /// folder that was deleted/renamed), that slug's last stamp survives forever
    /// and the `code_graph workspaces` op keeps returning a ghost row. Warm runs
    /// call this so each run's persisted set exactly mirrors the workspaces it
    /// actually indexed (ready) plus the ones it explicitly marked
    /// failed/timed_out — a timed-out workspace is in `rows`, so it keeps its
    /// row; only truly-vanished slugs are pruned.
    ///
    /// Upsert and delete run in one transaction so a crash can never leave a
    /// half-applied set (some new rows written, stale rows not yet pruned).
    ///
    /// All `rows` must share the same `project_id` as the `project_id`
    /// argument; the delete is scoped to that project.
    pub async fn replace_for_project(
        &self,
        project_id: &str,
        rows: &[ProjectWorkspaceGraphUpsert<'_>],
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        let keep: Vec<&str> = rows.iter().map(|row| row.workspace_slug).collect();

        let mut tx = self.db.pool().begin().await?;
        for row in rows {
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
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "DELETE FROM project_workspace_graph
              WHERE project_id = $1 AND workspace_slug <> ALL($2)",
        )
        .bind(project_id)
        .bind(&keep)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Remove a single workspace freshness row. Used to retire the
    /// [`CODELESS_WORKSPACE_SLUG`] sentinel once a real warm succeeds — a
    /// project can't simultaneously be "code-less" and have indexed
    /// workspaces.
    pub async fn delete(&self, project_id: &str, workspace_slug: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "DELETE FROM project_workspace_graph WHERE project_id = $1 AND workspace_slug = $2",
        )
        .bind(project_id)
        .bind(workspace_slug)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn list_for_project(&self, project_id: &str) -> Result<Vec<ProjectWorkspaceGraph>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, ProjectWorkspaceGraph>(
            r#"SELECT project_id, workspace_slug, commit_sha, warmed_at, status
               FROM project_workspace_graph
              WHERE project_id = $1
              ORDER BY workspace_slug ASC"#,
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?)
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

        let rows = repo.list_for_project("p1").await.expect("list");
        assert_eq!(
            rows.iter()
                .map(|r| r.workspace_slug.as_str())
                .collect::<Vec<_>>(),
            vec!["api", "root"]
        );
    }

    #[tokio::test]
    async fn list_for_project_returns_empty_for_project_without_workspace_rows() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;

        let rows = repo.list_for_project("p1").await.expect("list");
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn list_for_project_returns_rows_sorted_by_workspace_slug() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;

        repo.upsert_many(&[
            ProjectWorkspaceGraphUpsert {
                project_id: "p1",
                workspace_slug: "server",
                commit_sha: "sha-server",
                status: "ready",
            },
            ProjectWorkspaceGraphUpsert {
                project_id: "p1",
                workspace_slug: "api",
                commit_sha: "sha-api",
                status: "warming",
            },
        ])
        .await
        .expect("upsert many");

        let rows = repo.list_for_project("p1").await.expect("list rows");
        let slugs: Vec<_> = rows.iter().map(|row| row.workspace_slug.as_str()).collect();
        assert_eq!(slugs, vec!["api", "server"]);
        assert_eq!(rows[0].commit_sha, "sha-api");
        assert_eq!(rows[0].status, "warming");
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

    #[tokio::test]
    async fn replace_for_project_prunes_vanished_slugs_and_keeps_timed_out() {
        // Pins the live djinnos/djinn scenario: prior rows {root, server, ui,
        // website}; a new warm emits ready {ui, website} and marks {server}
        // timed_out. The `root` slug no longer exists, so its stale row must be
        // deleted; `server` is in the replace-set (timed_out), so it must keep
        // its row; `ui`/`website` are re-stamped ready at the new commit.
        let repo = fresh().await;
        seed_project(&repo, "p1").await;

        repo.upsert_many(&[
            ProjectWorkspaceGraphUpsert {
                project_id: "p1",
                workspace_slug: "root",
                commit_sha: "old",
                status: "ready",
            },
            ProjectWorkspaceGraphUpsert {
                project_id: "p1",
                workspace_slug: "server",
                commit_sha: "old",
                status: "ready",
            },
            ProjectWorkspaceGraphUpsert {
                project_id: "p1",
                workspace_slug: "ui",
                commit_sha: "old",
                status: "ready",
            },
            ProjectWorkspaceGraphUpsert {
                project_id: "p1",
                workspace_slug: "website",
                commit_sha: "old",
                status: "ready",
            },
        ])
        .await
        .expect("seed prior rows");

        repo.replace_for_project(
            "p1",
            &[
                ProjectWorkspaceGraphUpsert {
                    project_id: "p1",
                    workspace_slug: "ui",
                    commit_sha: "new",
                    status: "ready",
                },
                ProjectWorkspaceGraphUpsert {
                    project_id: "p1",
                    workspace_slug: "website",
                    commit_sha: "new",
                    status: "ready",
                },
                ProjectWorkspaceGraphUpsert {
                    project_id: "p1",
                    workspace_slug: "server",
                    commit_sha: "new",
                    status: "timed_out",
                },
            ],
        )
        .await
        .expect("replace");

        // `root` vanished → pruned.
        assert!(repo.get("p1", "root").await.expect("get root").is_none());

        // `server` timed out but is in the set → kept, re-stamped.
        let server = repo
            .get("p1", "server")
            .await
            .expect("get server")
            .expect("server row kept");
        assert_eq!(server.status, "timed_out");
        assert_eq!(server.commit_sha, "new");

        // `ui`/`website` re-stamped ready at the new commit.
        for slug in ["ui", "website"] {
            let row = repo
                .get("p1", slug)
                .await
                .expect("get")
                .unwrap_or_else(|| panic!("{slug} row"));
            assert_eq!(row.status, "ready");
            assert_eq!(row.commit_sha, "new");
        }

        let slugs: Vec<_> = repo
            .list_for_project("p1")
            .await
            .expect("list")
            .into_iter()
            .map(|r| r.workspace_slug)
            .collect();
        assert_eq!(slugs, vec!["server", "ui", "website"]);
    }

    #[tokio::test]
    async fn replace_for_project_is_scoped_to_the_project() {
        // Pruning must never touch another project's rows even though the new
        // set omits that project's slugs.
        let repo = fresh().await;
        seed_project(&repo, "p1").await;
        seed_project(&repo, "p2").await;

        repo.upsert(ProjectWorkspaceGraphUpsert {
            project_id: "p2",
            workspace_slug: "root",
            commit_sha: "abc",
            status: "ready",
        })
        .await
        .expect("seed p2");
        repo.upsert(ProjectWorkspaceGraphUpsert {
            project_id: "p1",
            workspace_slug: "old",
            commit_sha: "abc",
            status: "ready",
        })
        .await
        .expect("seed p1");

        repo.replace_for_project(
            "p1",
            &[ProjectWorkspaceGraphUpsert {
                project_id: "p1",
                workspace_slug: "server",
                commit_sha: "new",
                status: "ready",
            }],
        )
        .await
        .expect("replace p1");

        // p1's stale `old` pruned, `server` present.
        assert!(repo.get("p1", "old").await.expect("get").is_none());
        assert!(repo.get("p1", "server").await.expect("get").is_some());
        // p2 untouched.
        assert!(repo.get("p2", "root").await.expect("get").is_some());
    }

    #[tokio::test]
    async fn replace_for_project_retires_codeless_sentinel() {
        // A prior code-less sentinel row is not in a real warm's replace-set, so
        // it is pruned automatically — no separate delete call needed.
        let repo = fresh().await;
        seed_project(&repo, "p1").await;

        repo.upsert(ProjectWorkspaceGraphUpsert {
            project_id: "p1",
            workspace_slug: CODELESS_WORKSPACE_SLUG,
            commit_sha: "abc",
            status: "ready",
        })
        .await
        .expect("seed sentinel");

        repo.replace_for_project(
            "p1",
            &[ProjectWorkspaceGraphUpsert {
                project_id: "p1",
                workspace_slug: "server",
                commit_sha: "new",
                status: "ready",
            }],
        )
        .await
        .expect("replace");

        assert!(
            repo.get("p1", CODELESS_WORKSPACE_SLUG)
                .await
                .expect("get sentinel")
                .is_none()
        );
        assert!(repo.get("p1", "server").await.expect("get").is_some());
    }

    #[tokio::test]
    async fn repository_reinstantiation_preserves_rows_and_isolates_projects() {
        // `open_in_memory` is the template-cloned PostgreSQL harness, so this
        // exercises the durable database boundary rather than an in-process map.
        let repo = fresh().await;
        seed_project(&repo, "p1").await;
        seed_project(&repo, "p2").await;
        let db = repo.db.clone();

        repo.upsert(ProjectWorkspaceGraphUpsert {
            project_id: "p1",
            workspace_slug: "server",
            commit_sha: "p1-sha",
            status: "ready",
        })
        .await
        .expect("write p1");
        drop(repo);

        // Simulate a server restart: a newly constructed repository over the
        // same Postgres pool must observe only its project durable rows.
        let restarted = ProjectWorkspaceGraphRepository::new(db);
        let p1 = restarted
            .get("p1", "server")
            .await
            .expect("read p1")
            .expect("p1 row persists");
        assert_eq!(p1.commit_sha, "p1-sha");
        assert!(restarted.get("p2", "server").await.expect("read p2").is_none());

        restarted
            .upsert(ProjectWorkspaceGraphUpsert {
                project_id: "p2",
                workspace_slug: "server",
                commit_sha: "p2-sha",
                status: "timed_out",
            })
            .await
            .expect("write p2");
        assert_eq!(
            restarted
                .get("p1", "server")
                .await
                .expect("re-read p1")
                .expect("p1 remains")
                .status,
            "ready"
        );
    }

    #[tokio::test]
    async fn deleting_project_cascades_workspace_graph_rows() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;

        repo.upsert(ProjectWorkspaceGraphUpsert {
            project_id: "p1",
            workspace_slug: "root",
            commit_sha: "abc",
            status: "ready",
        })
        .await
        .expect("upsert");
        assert!(repo.has_any_for_project("p1").await.expect("has any"));

        sqlx::query("DELETE FROM projects WHERE id = $1")
            .bind("p1")
            .execute(repo.db.pool())
            .await
            .expect("delete project");

        assert!(!repo.has_any_for_project("p1").await.expect("has any"));
        assert!(repo.get("p1", "root").await.expect("get").is_none());
    }
}
