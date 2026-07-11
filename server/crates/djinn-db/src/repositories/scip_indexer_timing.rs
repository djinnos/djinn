//! Persistence for per-(project, workspace, indexer) SCIP indexer timings.
//!
//! Each warm run records the outcome and wall-clock elapsed of every indexer
//! invocation here; the next warm reads it back to adapt the per-invocation
//! timeout budget (see `djinn-graph::scip_indexer::budget`). Storage is a
//! single row per key (latest observation + rolling evidence), not an
//! append-only log — see `migrations_postgres/106_scip_indexer_timing.sql`.

use crate::Result;
use crate::database::Database;

/// Recorded status of a single indexer invocation. Stored verbatim in
/// `last_status`. Only these three values are written by the warm path.
pub const TIMING_STATUS_SUCCESS: &str = "success";
pub const TIMING_STATUS_FAILED: &str = "failed";
pub const TIMING_STATUS_TIMED_OUT: &str = "timed_out";

/// A persisted timing row for one (project, workspace, indexer) key.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct ScipIndexerTiming {
    pub project_id: String,
    pub workspace_slug: String,
    /// Stable indexer key (e.g. `rust`, `typescript`) — matches
    /// `SupportedIndexer::language()` on the graph side.
    pub indexer: String,
    pub last_status: String,
    pub last_elapsed_ms: i64,
    /// Elapsed of the last SUCCESSFUL run, if any.
    pub success_elapsed_ms: Option<i64>,
    /// High-water elapsed of TIMED-OUT runs since the last success, if any.
    pub timed_out_elapsed_ms: Option<i64>,
    pub observed_at: String,
}

/// A single observation to persist. `elapsed_ms` is the wall-clock duration of
/// the invocation; `status` must be one of the `TIMING_STATUS_*` constants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScipIndexerTimingObservation<'a> {
    pub project_id: &'a str,
    pub workspace_slug: &'a str,
    pub indexer: &'a str,
    pub status: &'a str,
    pub elapsed_ms: i64,
}

pub struct ScipIndexerTimingRepository {
    db: Database,
}

impl ScipIndexerTimingRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Record (upsert) a single invocation's timing.
    ///
    /// The rolling evidence columns follow these rules so the budget only ever
    /// relaxes and repeated timeouts grow headroom:
    ///   * on `success`  — refresh `success_elapsed_ms`; clear
    ///     `timed_out_elapsed_ms` (the current cap sufficed).
    ///   * on `timed_out` — raise `timed_out_elapsed_ms` to the high-water of
    ///     itself and the new elapsed; leave `success_elapsed_ms` untouched.
    ///   * on `failed`   — record the latest status/elapsed only; a hard
    ///     indexer error is not evidence about the timeout size either way.
    pub async fn record(&self, obs: ScipIndexerTimingObservation<'_>) -> Result<()> {
        self.db.ensure_initialized().await?;
        self.record_initialized(obs).await
    }

    /// Record many observations. Each is upserted independently; a single bad
    /// row does not roll back the rest (best-effort telemetry).
    pub async fn record_many(&self, rows: &[ScipIndexerTimingObservation<'_>]) -> Result<()> {
        self.db.ensure_initialized().await?;
        for row in rows {
            self.record_initialized(row.clone()).await?;
        }
        Ok(())
    }

    pub async fn get(
        &self,
        project_id: &str,
        workspace_slug: &str,
        indexer: &str,
    ) -> Result<Option<ScipIndexerTiming>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, ScipIndexerTiming>(
            r#"SELECT project_id, workspace_slug, indexer, last_status, last_elapsed_ms,
                      success_elapsed_ms, timed_out_elapsed_ms, observed_at
                 FROM scip_indexer_timing
                WHERE project_id = $1 AND workspace_slug = $2 AND indexer = $3"#,
        )
        .bind(project_id)
        .bind(workspace_slug)
        .bind(indexer)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn list_for_project(&self, project_id: &str) -> Result<Vec<ScipIndexerTiming>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, ScipIndexerTiming>(
            r#"SELECT project_id, workspace_slug, indexer, last_status, last_elapsed_ms,
                      success_elapsed_ms, timed_out_elapsed_ms, observed_at
                 FROM scip_indexer_timing
                WHERE project_id = $1
                ORDER BY workspace_slug ASC, indexer ASC"#,
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    async fn record_initialized(&self, obs: ScipIndexerTimingObservation<'_>) -> Result<()> {
        // `$4` = status, `$5` = elapsed_ms. The CASE expressions in both the
        // INSERT and the ON CONFLICT UPDATE derive the rolling evidence columns
        // so timed-out headroom only grows and a success clears it.
        sqlx::query(
            r#"INSERT INTO scip_indexer_timing
                   (project_id, workspace_slug, indexer, last_status, last_elapsed_ms,
                    success_elapsed_ms, timed_out_elapsed_ms, observed_at)
               VALUES
                   ($1, $2, $3, $4, $5,
                    CASE WHEN $4 = 'success'   THEN $5 ELSE NULL END,
                    CASE WHEN $4 = 'timed_out' THEN $5 ELSE NULL END,
                    to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
               ON CONFLICT (project_id, workspace_slug, indexer) DO UPDATE SET
                   last_status = $4,
                   last_elapsed_ms = $5,
                   observed_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                   success_elapsed_ms = CASE
                       WHEN $4 = 'success' THEN $5
                       ELSE scip_indexer_timing.success_elapsed_ms
                   END,
                   timed_out_elapsed_ms = CASE
                       WHEN $4 = 'success'   THEN NULL
                       WHEN $4 = 'timed_out' THEN GREATEST(
                           COALESCE(scip_indexer_timing.timed_out_elapsed_ms, 0), $5)
                       ELSE scip_indexer_timing.timed_out_elapsed_ms
                   END"#,
        )
        .bind(obs.project_id)
        .bind(obs.workspace_slug)
        .bind(obs.indexer)
        .bind(obs.status)
        .bind(obs.elapsed_ms)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fresh() -> ScipIndexerTimingRepository {
        let db = Database::open_in_memory().expect("in-memory db");
        ScipIndexerTimingRepository::new(db)
    }

    async fn seed_project(repo: &ScipIndexerTimingRepository, project_id: &str) {
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

    fn obs<'a>(
        project_id: &'a str,
        workspace_slug: &'a str,
        indexer: &'a str,
        status: &'a str,
        elapsed_ms: i64,
    ) -> ScipIndexerTimingObservation<'a> {
        ScipIndexerTimingObservation {
            project_id,
            workspace_slug,
            indexer,
            status,
            elapsed_ms,
        }
    }

    #[tokio::test]
    async fn success_records_success_elapsed_and_no_timeout_evidence() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;

        repo.record(obs("p1", "server", "rust", TIMING_STATUS_SUCCESS, 900_000))
            .await
            .expect("record");

        let row = repo
            .get("p1", "server", "rust")
            .await
            .expect("get")
            .expect("hit");
        assert_eq!(row.last_status, "success");
        assert_eq!(row.last_elapsed_ms, 900_000);
        assert_eq!(row.success_elapsed_ms, Some(900_000));
        assert_eq!(row.timed_out_elapsed_ms, None);
        assert!(!row.observed_at.is_empty());
    }

    #[tokio::test]
    async fn timed_out_grows_high_water_evidence() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;

        // First timeout at the 1200s cap.
        repo.record(obs(
            "p1",
            "server",
            "rust",
            TIMING_STATUS_TIMED_OUT,
            1_200_000,
        ))
        .await
        .expect("record 1");
        // A later timeout at a larger cap raises the high-water.
        repo.record(obs(
            "p1",
            "server",
            "rust",
            TIMING_STATUS_TIMED_OUT,
            1_800_000,
        ))
        .await
        .expect("record 2");
        // A smaller timeout must NOT lower the high-water.
        repo.record(obs(
            "p1",
            "server",
            "rust",
            TIMING_STATUS_TIMED_OUT,
            1_500_000,
        ))
        .await
        .expect("record 3");

        let row = repo
            .get("p1", "server", "rust")
            .await
            .expect("get")
            .expect("hit");
        assert_eq!(row.last_status, "timed_out");
        assert_eq!(row.last_elapsed_ms, 1_500_000);
        assert_eq!(row.timed_out_elapsed_ms, Some(1_800_000));
        assert_eq!(row.success_elapsed_ms, None);
    }

    #[tokio::test]
    async fn success_after_timeouts_clears_timeout_evidence() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;

        repo.record(obs(
            "p1",
            "server",
            "rust",
            TIMING_STATUS_TIMED_OUT,
            1_800_000,
        ))
        .await
        .expect("record timeout");
        repo.record(obs(
            "p1",
            "server",
            "rust",
            TIMING_STATUS_SUCCESS,
            1_400_000,
        ))
        .await
        .expect("record success");

        let row = repo
            .get("p1", "server", "rust")
            .await
            .expect("get")
            .expect("hit");
        assert_eq!(row.success_elapsed_ms, Some(1_400_000));
        assert_eq!(
            row.timed_out_elapsed_ms, None,
            "a success proves the cap sufficed — timeout evidence must reset"
        );
    }

    #[tokio::test]
    async fn failed_preserves_prior_evidence() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;

        repo.record(obs("p1", "server", "rust", TIMING_STATUS_SUCCESS, 500_000))
            .await
            .expect("record success");
        repo.record(obs("p1", "server", "rust", TIMING_STATUS_FAILED, 3_000))
            .await
            .expect("record failed");

        let row = repo
            .get("p1", "server", "rust")
            .await
            .expect("get")
            .expect("hit");
        assert_eq!(row.last_status, "failed");
        assert_eq!(row.last_elapsed_ms, 3_000);
        assert_eq!(
            row.success_elapsed_ms,
            Some(500_000),
            "a hard failure is not evidence — prior success elapsed survives"
        );
    }

    #[tokio::test]
    async fn record_many_and_list_for_project() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;

        repo.record_many(&[
            obs("p1", "server", "rust", TIMING_STATUS_SUCCESS, 900_000),
            obs("p1", "web", "typescript", TIMING_STATUS_SUCCESS, 60_000),
        ])
        .await
        .expect("record many");

        let rows = repo.list_for_project("p1").await.expect("list");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].workspace_slug, "server");
        assert_eq!(rows[0].indexer, "rust");
        assert_eq!(rows[1].workspace_slug, "web");
        assert_eq!(rows[1].indexer, "typescript");
    }

    #[tokio::test]
    async fn deleting_project_cascades_timing_rows() {
        let repo = fresh().await;
        seed_project(&repo, "p1").await;

        repo.record(obs("p1", "server", "rust", TIMING_STATUS_SUCCESS, 900_000))
            .await
            .expect("record");

        sqlx::query("DELETE FROM projects WHERE id = $1")
            .bind("p1")
            .execute(repo.db.pool())
            .await
            .expect("delete project");

        let rows = repo.list_for_project("p1").await.expect("list");
        assert!(rows.is_empty());
    }
}
