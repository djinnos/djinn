//! Persistence for per-(project, workspace, language) index coverage.
//!
//! The richer sibling of [`super::project_workspace_graph`]: where that table
//! records a bare per-workspace freshness `status`, this one records the
//! *coverage contract* — the outcome AND extent of every SCIP indexer the warm
//! pipeline attempted, so agents and the UI can tell "indexed empty" from
//! "indexer wiped out" and name exactly which workspaces are missing.
//!
//! One row per key (project, workspace, language), written by the warm pipeline
//! as a replace-set (see [`Self::replace_for_project`]) on both the
//! partial-failure and total-failure paths. Read cheaply by the `code_graph
//! coverage` op and the per-response coverage advisory WITHOUT loading the graph
//! blob. See `migrations_postgres/118_project_workspace_coverage.sql`.

use crate::Result;
use crate::database::Database;

/// Coverage status enum, stored verbatim in `status`.
///
/// `indexed` — the indexer produced a SCIP artifact that merged into the graph.
/// `indexer_failed` — the indexer ran but errored / produced no artifact.
/// `timed_out` — the indexer hit its wall-clock cap (or the warm deadline).
/// `unsupported_language` — the workspace was detected but no working indexer
///   exists for it (reserved; e.g. the Ruby FIXME or a missing indexer binary).
/// `excluded` — intentionally skipped via graph-exclusions config. Consumers
///   MUST NOT raise a coverage advisory for an `excluded` workspace.
pub const COVERAGE_STATUS_INDEXED: &str = "indexed";
pub const COVERAGE_STATUS_INDEXER_FAILED: &str = "indexer_failed";
pub const COVERAGE_STATUS_TIMED_OUT: &str = "timed_out";
pub const COVERAGE_STATUS_UNSUPPORTED_LANGUAGE: &str = "unsupported_language";
pub const COVERAGE_STATUS_EXCLUDED: &str = "excluded";

/// True when `status` denotes a genuine coverage GAP — an in-scope workspace
/// that agents should not trust as fully indexed. `indexed` and the intentional
/// `excluded` are NOT gaps (no advisory).
pub fn coverage_status_is_gap(status: &str) -> bool {
    matches!(
        status,
        COVERAGE_STATUS_INDEXER_FAILED
            | COVERAGE_STATUS_TIMED_OUT
            | COVERAGE_STATUS_UNSUPPORTED_LANGUAGE
    )
}

/// A persisted coverage row for one (project, workspace, language) key.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct ProjectWorkspaceCoverage {
    pub project_id: String,
    pub workspace_slug: String,
    /// Stable per-language key (matches `SupportedIndexer::language()`).
    pub language: String,
    pub status: String,
    /// Indexer exit detail (stderr tail / exit code / timeout reason).
    pub detail: Option<String>,
    /// Workspace root RELATIVE to the project root (empty for the repo root).
    pub workspace_root: String,
    /// Marker file(s) whose presence caused workspace detection for this
    /// language (e.g. `Cargo.toml`).
    pub marker_evidence: Option<String>,
    /// Candidate source files found under the workspace root.
    pub discovered_files: Option<i64>,
    /// Distinct files that made it into the merged graph for this workspace.
    pub indexed_files: Option<i64>,
    pub commit_sha: String,
    pub warmed_at: String,
}

/// One coverage row to persist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectWorkspaceCoverageUpsert<'a> {
    pub project_id: &'a str,
    pub workspace_slug: &'a str,
    pub language: &'a str,
    pub status: &'a str,
    pub detail: Option<&'a str>,
    pub workspace_root: &'a str,
    pub marker_evidence: Option<&'a str>,
    pub discovered_files: Option<i64>,
    pub indexed_files: Option<i64>,
    pub commit_sha: &'a str,
}

pub struct ProjectWorkspaceCoverageRepository {
    db: Database,
}

impl ProjectWorkspaceCoverageRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn list_for_project(
        &self,
        project_id: &str,
    ) -> Result<Vec<ProjectWorkspaceCoverage>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, ProjectWorkspaceCoverage>(
            r#"SELECT project_id, workspace_slug, language, status, detail, workspace_root,
                      marker_evidence, discovered_files, indexed_files, commit_sha, warmed_at
                 FROM project_workspace_coverage
                WHERE project_id = $1
                ORDER BY workspace_slug ASC, language ASC"#,
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Persist a warm run's coverage rows as a *replace-set*: upsert every row
    /// in `rows` and, in the same transaction, delete any existing row for
    /// `project_id` whose `(workspace_slug, language)` is not among them.
    /// Mirrors [`super::project_workspace_graph::ProjectWorkspaceGraphRepository::replace_for_project`]
    /// so a vanished workspace/language never leaves a ghost coverage row.
    ///
    /// All `rows` must share the same `project_id` as the `project_id` argument.
    pub async fn replace_for_project(
        &self,
        project_id: &str,
        rows: &[ProjectWorkspaceCoverageUpsert<'_>],
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        // Keep-set expressed as parallel arrays the DELETE can test with a
        // row-wise membership check (a single `<> ALL` can't key on a pair).
        let keep_slugs: Vec<&str> = rows.iter().map(|r| r.workspace_slug).collect();
        let keep_langs: Vec<&str> = rows.iter().map(|r| r.language).collect();

        let mut tx = self.db.pool().begin().await?;
        for row in rows {
            sqlx::query(
                r#"INSERT INTO project_workspace_coverage
                       (project_id, workspace_slug, language, status, detail, workspace_root,
                        marker_evidence, discovered_files, indexed_files, commit_sha, warmed_at)
                   VALUES
                       ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                        to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
                   ON CONFLICT (project_id, workspace_slug, language) DO UPDATE SET
                       status = EXCLUDED.status,
                       detail = EXCLUDED.detail,
                       workspace_root = EXCLUDED.workspace_root,
                       marker_evidence = EXCLUDED.marker_evidence,
                       discovered_files = EXCLUDED.discovered_files,
                       indexed_files = EXCLUDED.indexed_files,
                       commit_sha = EXCLUDED.commit_sha,
                       warmed_at = EXCLUDED.warmed_at"#,
            )
            .bind(row.project_id)
            .bind(row.workspace_slug)
            .bind(row.language)
            .bind(row.status)
            .bind(row.detail)
            .bind(row.workspace_root)
            .bind(row.marker_evidence)
            .bind(row.discovered_files)
            .bind(row.indexed_files)
            .bind(row.commit_sha)
            .execute(&mut *tx)
            .await?;
        }
        // Prune rows whose (slug, language) pair is not in this run's set.
        // `unnest` zips the two keep arrays into a pair table for the NOT IN.
        sqlx::query(
            r#"DELETE FROM project_workspace_coverage
                WHERE project_id = $1
                  AND (workspace_slug, language) NOT IN (
                      SELECT s, l FROM unnest($2::text[], $3::text[]) AS t(s, l)
                  )"#,
        )
        .bind(project_id)
        .bind(&keep_slugs)
        .bind(&keep_langs)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fresh() -> ProjectWorkspaceCoverageRepository {
        let db = Database::open_in_memory().expect("in-memory db");
        ProjectWorkspaceCoverageRepository::new(db)
    }

    async fn seed_project(repo: &ProjectWorkspaceCoverageRepository, project_id: &str) {
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

    fn row<'a>(
        workspace_slug: &'a str,
        language: &'a str,
        status: &'a str,
    ) -> ProjectWorkspaceCoverageUpsert<'a> {
        ProjectWorkspaceCoverageUpsert {
            project_id: "p1",
            workspace_slug,
            language,
            status,
            detail: None,
            workspace_root: workspace_slug,
            marker_evidence: None,
            discovered_files: None,
            indexed_files: None,
            commit_sha: "abc",
        }
    }

    #[test]
    fn gap_classification() {
        assert!(!coverage_status_is_gap(COVERAGE_STATUS_INDEXED));
        assert!(!coverage_status_is_gap(COVERAGE_STATUS_EXCLUDED));
        assert!(coverage_status_is_gap(COVERAGE_STATUS_INDEXER_FAILED));
        assert!(coverage_status_is_gap(COVERAGE_STATUS_TIMED_OUT));
        assert!(coverage_status_is_gap(COVERAGE_STATUS_UNSUPPORTED_LANGUAGE));
    }

    #[tokio::test]
    async fn replace_and_list_round_trip() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;

        repo.replace_for_project(
            "p1",
            &[
                ProjectWorkspaceCoverageUpsert {
                    detail: None,
                    discovered_files: Some(120),
                    indexed_files: Some(120),
                    marker_evidence: Some("Cargo.toml"),
                    ..row("server", "rust", COVERAGE_STATUS_INDEXED)
                },
                ProjectWorkspaceCoverageUpsert {
                    detail: Some("scip-typescript: no tsconfig"),
                    discovered_files: Some(40),
                    indexed_files: Some(0),
                    marker_evidence: Some("package.json"),
                    ..row("ui", "typescript", COVERAGE_STATUS_INDEXER_FAILED)
                },
            ],
        )
        .await
        .expect("replace");

        let rows = repo.list_for_project("p1").await.expect("list");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].workspace_slug, "server");
        assert_eq!(rows[0].status, COVERAGE_STATUS_INDEXED);
        assert_eq!(rows[0].discovered_files, Some(120));
        assert_eq!(rows[1].workspace_slug, "ui");
        assert_eq!(rows[1].status, COVERAGE_STATUS_INDEXER_FAILED);
        assert_eq!(rows[1].indexed_files, Some(0));
        assert_eq!(
            rows[1].detail.as_deref(),
            Some("scip-typescript: no tsconfig")
        );
    }

    #[tokio::test]
    async fn replace_prunes_vanished_pairs() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;

        repo.replace_for_project(
            "p1",
            &[
                row("server", "rust", COVERAGE_STATUS_INDEXED),
                row("legacy", "python", COVERAGE_STATUS_INDEXED),
            ],
        )
        .await
        .expect("seed");

        // Second warm no longer sees `legacy`; `server` gains a `ui` sibling.
        repo.replace_for_project(
            "p1",
            &[
                row("server", "rust", COVERAGE_STATUS_INDEXED),
                row("ui", "typescript", COVERAGE_STATUS_TIMED_OUT),
            ],
        )
        .await
        .expect("replace");

        let slugs: Vec<_> = repo
            .list_for_project("p1")
            .await
            .expect("list")
            .into_iter()
            .map(|r| r.workspace_slug)
            .collect();
        assert_eq!(slugs, vec!["server", "ui"]);
    }

    #[tokio::test]
    async fn deleting_project_cascades_coverage_rows() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;
        repo.replace_for_project("p1", &[row("server", "rust", COVERAGE_STATUS_INDEXED)])
            .await
            .expect("seed");

        sqlx::query("DELETE FROM projects WHERE id = $1")
            .bind("p1")
            .execute(repo.db.pool())
            .await
            .expect("delete project");

        assert!(repo.list_for_project("p1").await.expect("list").is_empty());
    }
}
