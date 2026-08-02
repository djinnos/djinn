//! Durable lifecycle ledger for graph-warm attempts.
//!
//! Rows are attempts, not a current-state cache: every dispatch inserts a new
//! UUID-keyed row before it talks to Kubernetes. Terminal writers and the stale
//! reaper use `status = 'running'` compare-and-set predicates so they cannot
//! overwrite each other.

use chrono::Duration;
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::{Database, Error as DbError, Result as DbResult};

pub const MAX_WARM_GRAPH_ATTEMPT_DETAIL_CHARS: usize = 4096;

/// The complete lifecycle vocabulary enforced by migration 172.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WarmGraphAttemptStatus {
    Running,
    PublishedComplete,
    PublishedPartial,
    Failed,
    TimedOut,
    DispatchFailed,
}

impl WarmGraphAttemptStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::PublishedComplete => "published_complete",
            Self::PublishedPartial => "published_partial",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::DispatchFailed => "dispatch_failed",
        }
    }

    #[must_use]
    pub fn is_terminal(self) -> bool {
        self != Self::Running
    }

    fn parse(value: &str) -> DbResult<Self> {
        match value {
            "running" => Ok(Self::Running),
            "published_complete" => Ok(Self::PublishedComplete),
            "published_partial" => Ok(Self::PublishedPartial),
            "failed" => Ok(Self::Failed),
            "timed_out" => Ok(Self::TimedOut),
            "dispatch_failed" => Ok(Self::DispatchFailed),
            _ => Err(DbError::InvalidData(format!(
                "invalid warm graph attempt status `{value}`"
            ))),
        }
    }
}

/// One immutable dispatch identity plus its one-way lifecycle state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WarmGraphAttempt {
    pub attempt_id: String,
    pub project_id: String,
    pub revision: String,
    pub status: WarmGraphAttemptStatus,
    /// RFC3339 UTC timestamp with millisecond precision.
    pub started_at: String,
    /// RFC3339 UTC timestamp with millisecond precision.
    pub deadline_at: String,
    /// RFC3339 UTC timestamp with millisecond precision once terminal.
    pub finished_at: Option<String>,
    pub detail: Option<String>,
}

/// Durable answer shape used by consumers deciding whether a revision needs
/// recovery. Coverage-aware classification is intentionally added separately;
/// this lifecycle repository already exposes all four public states.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WarmGraphOutcome {
    NotTriedYet,
    InProgress(WarmGraphAttempt),
    TriedAndDidNotPublish(WarmGraphAttempt),
    Published(WarmGraphAttempt),
}

/// Repository for the append-only warm-attempt ledger.
pub struct WarmGraphAttemptRepository {
    db: Database,
}

impl WarmGraphAttemptRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Insert a distinct running attempt before external dispatch.
    ///
    /// `deadline_at` is an RFC3339 timestamp interpreted by PostgreSQL. The
    /// UUIDv7 identity follows the crate's canonical persisted-ID convention.
    pub async fn start_attempt(
        &self,
        project_id: &str,
        revision: &str,
        deadline_at: &str,
    ) -> DbResult<String> {
        if project_id.trim().is_empty() || revision.trim().is_empty() {
            return Err(DbError::InvalidData(
                "warm graph attempt project and revision must not be blank".into(),
            ));
        }
        self.db.ensure_initialized().await?;
        let attempt_id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO warm_graph_attempt \
             (attempt_id, project_id, revision, status, started_at, deadline_at) \
             VALUES ($1::uuid, $2, $3, 'running', transaction_timestamp(), $4::timestamptz)",
        )
        .bind(&attempt_id)
        .bind(project_id)
        .bind(revision)
        .bind(deadline_at)
        .execute(self.db.pool())
        .await?;
        Ok(attempt_id)
    }

    /// Return one attempt, if it still exists (project deletion cascades it).
    pub async fn get_attempt(&self, attempt_id: &str) -> DbResult<Option<WarmGraphAttempt>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query(ATTEMPT_SELECT)
            .bind(attempt_id)
            .fetch_optional(self.db.pool())
            .await?;
        row.map(parse_attempt).transpose()
    }

    /// Return the retained history for one project/revision, newest first.
    pub async fn list_attempts(
        &self,
        project_id: &str,
        revision: &str,
    ) -> DbResult<Vec<WarmGraphAttempt>> {
        self.db.ensure_initialized().await?;
        sqlx::query(ATTEMPTS_FOR_REVISION_SELECT)
            .bind(project_id)
            .bind(revision)
            .fetch_all(self.db.pool())
            .await?
            .into_iter()
            .map(parse_attempt)
            .collect()
    }

    /// Atomically terminalize a running attempt. `false` means another terminal
    /// writer already won, or the attempt does not exist.
    pub async fn finish_attempt_if_running(
        &self,
        attempt_id: &str,
        terminal_status: WarmGraphAttemptStatus,
        detail: Option<&str>,
    ) -> DbResult<bool> {
        if !terminal_status.is_terminal() {
            return Err(DbError::InvalidTransition(
                "a warm graph attempt cannot finish as running".into(),
            ));
        }
        self.db.ensure_initialized().await?;
        let changed = sqlx::query(
            "UPDATE warm_graph_attempt \
             SET status = $2, detail = CASE WHEN $3 IS NULL THEN NULL ELSE left($3, 4096) END, \
                 finished_at = transaction_timestamp() \
             WHERE attempt_id = $1::uuid AND status = 'running'",
        )
        .bind(attempt_id)
        .bind(terminal_status.as_str())
        .bind(detail)
        .execute(self.db.pool())
        .await?
        .rows_affected();
        Ok(changed == 1)
    }

    /// Reconcile every running attempt whose deadline plus grace is strictly
    /// before `now`. Equality intentionally remains in progress: timeout begins
    /// only *after* the complete deadline/grace window has elapsed.
    ///
    /// Returned ids are precisely the CAS transitions won by this caller, so a
    /// repeated or racing reconciliation is naturally idempotent.
    pub async fn reconcile_stale_attempts(
        &self,
        now: &str,
        grace: Duration,
    ) -> DbResult<Vec<String>> {
        if grace < Duration::zero() {
            return Err(DbError::InvalidData(
                "warm graph attempt grace must not be negative".into(),
            ));
        }
        self.db.ensure_initialized().await?;
        // PostgreSQL intervals have microsecond precision. Do not silently
        // truncate a caller's fractional grace period while converting it for
        // SQL: reject durations that cannot be represented exactly instead.
        let microseconds = grace.num_microseconds().ok_or_else(|| {
            DbError::InvalidData("warm graph attempt grace is out of range".into())
        })?;
        if Duration::microseconds(microseconds) != grace {
            return Err(DbError::InvalidData(
                "warm graph attempt grace must have microsecond precision".into(),
            ));
        }
        let rows = sqlx::query(
            "UPDATE warm_graph_attempt \
             SET status = 'timed_out', detail = 'deadline exceeded', \
                 finished_at = $1::timestamptz \
             WHERE status = 'running' \
               AND deadline_at + ($2::bigint * interval '1 microsecond') < $1::timestamptz \
             RETURNING attempt_id::text AS attempt_id",
        )
        .bind(now)
        .bind(microseconds)
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows.into_iter().map(|row| row.get("attempt_id")).collect())
    }
}

const ATTEMPT_SELECT: &str = "SELECT attempt_id::text AS attempt_id, project_id, revision, status, \
    to_char(started_at AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS started_at, \
    to_char(deadline_at AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS deadline_at, \
    CASE WHEN finished_at IS NULL THEN NULL ELSE to_char(finished_at AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') END AS finished_at, \
    detail FROM warm_graph_attempt WHERE attempt_id = $1::uuid";

const ATTEMPTS_FOR_REVISION_SELECT: &str = "SELECT attempt_id::text AS attempt_id, project_id, revision, status, \
    to_char(started_at AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS started_at, \
    to_char(deadline_at AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS deadline_at, \
    CASE WHEN finished_at IS NULL THEN NULL ELSE to_char(finished_at AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') END AS finished_at, \
    detail FROM warm_graph_attempt WHERE project_id = $1 AND revision = $2 ORDER BY started_at DESC";

fn parse_attempt(row: sqlx::postgres::PgRow) -> DbResult<WarmGraphAttempt> {
    let status: String = row.get("status");
    Ok(WarmGraphAttempt {
        attempt_id: row.get("attempt_id"),
        project_id: row.get("project_id"),
        revision: row.get("revision"),
        status: WarmGraphAttemptStatus::parse(&status)?,
        started_at: row.get("started_at"),
        deadline_at: row.get("deadline_at"),
        finished_at: row.get("finished_at"),
        detail: row.get("detail"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repositories::test_support::seed_project;

    async fn fresh() -> WarmGraphAttemptRepository {
        WarmGraphAttemptRepository::new(Database::open_in_memory().expect("test database"))
    }

    #[tokio::test]
    async fn start_preserves_distinct_historical_attempts() {
        let repo = fresh().await;
        seed_project(&repo.db, "p1", "p1").await;
        let first = repo
            .start_attempt("p1", "abc", "2030-01-01T00:00:00.000Z")
            .await
            .unwrap();
        let second = repo
            .start_attempt("p1", "abc", "2030-01-02T00:00:00.000Z")
            .await
            .unwrap();
        assert_ne!(first, second);
        let rows = repo.list_attempts("p1", "abc").await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|row| row.status == WarmGraphAttemptStatus::Running)
        );
        assert!(
            rows.iter()
                .any(|row| row.deadline_at == "2030-01-01T00:00:00.000Z")
        );
        assert!(
            rows.iter()
                .any(|row| row.deadline_at == "2030-01-02T00:00:00.000Z")
        );
    }

    #[tokio::test]
    async fn terminal_compare_and_set_preserves_winner_and_bounds_detail() {
        let repo = fresh().await;
        seed_project(&repo.db, "p1", "p1").await;
        let id = repo
            .start_attempt("p1", "abc", "2030-01-01T00:00:00Z")
            .await
            .unwrap();
        assert!(
            repo.finish_attempt_if_running(
                &id,
                WarmGraphAttemptStatus::Failed,
                Some(&"x".repeat(5000))
            )
            .await
            .unwrap()
        );
        assert!(
            !repo
                .finish_attempt_if_running(
                    &id,
                    WarmGraphAttemptStatus::PublishedComplete,
                    Some("later")
                )
                .await
                .unwrap()
        );
        let row = repo.get_attempt(&id).await.unwrap().unwrap();
        assert_eq!(row.status, WarmGraphAttemptStatus::Failed);
        assert_eq!(
            row.detail.as_deref().map(str::len),
            Some(MAX_WARM_GRAPH_ATTEMPT_DETAIL_CHARS)
        );
        assert!(row.finished_at.is_some());
        assert!(
            repo.finish_attempt_if_running(&id, WarmGraphAttemptStatus::Running, None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn reconciliation_observes_grace_boundary_and_is_idempotent() {
        let repo = fresh().await;
        seed_project(&repo.db, "p1", "p1").await;
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO warm_graph_attempt \
             (attempt_id, project_id, revision, status, started_at, deadline_at) \
             VALUES ($1::uuid, 'p1', 'abc', 'running', \
                     '2025-12-31T23:59:00.000Z'::timestamptz, \
                     '2026-01-01T00:00:00.000Z'::timestamptz)",
        )
        .bind(&id)
        .execute(repo.db.pool())
        .await
        .unwrap();
        let grace = Duration::seconds(30);
        assert!(
            repo.reconcile_stale_attempts("2025-12-31T23:59:59.999Z", grace)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            repo.reconcile_stale_attempts("2026-01-01T00:00:29.999Z", grace)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            repo.reconcile_stale_attempts("2026-01-01T00:00:30.000Z", grace)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            repo.reconcile_stale_attempts("2026-01-01T00:00:30.001Z", grace)
                .await
                .unwrap(),
            vec![id.clone()]
        );
        assert!(
            repo.reconcile_stale_attempts("2026-01-01T00:01:00.000Z", grace)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            repo.get_attempt(&id).await.unwrap().unwrap().status,
            WarmGraphAttemptStatus::TimedOut
        );
    }

    #[tokio::test]
    async fn reconciliation_preserves_fractional_grace_boundary() {
        let repo = fresh().await;
        seed_project(&repo.db, "p1", "p1").await;
        let id = repo
            .start_attempt("p1", "abc", "2026-01-01T00:00:00.000Z")
            .await
            .unwrap();
        let grace = Duration::milliseconds(1500);

        assert!(
            repo.reconcile_stale_attempts("2026-01-01T00:00:01.001Z", grace)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            repo.reconcile_stale_attempts("2026-01-01T00:00:01.500Z", grace)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            repo.reconcile_stale_attempts("2026-01-01T00:00:01.501Z", grace)
                .await
                .unwrap(),
            vec![id]
        );
    }

    #[tokio::test]
    async fn deleting_project_cascades_attempt_rows() {
        let repo = fresh().await;
        seed_project(&repo.db, "p1", "p1").await;
        let id = repo
            .start_attempt("p1", "abc", "2030-01-01T00:00:00Z")
            .await
            .unwrap();
        sqlx::query("DELETE FROM projects WHERE id = $1")
            .bind("p1")
            .execute(repo.db.pool())
            .await
            .unwrap();
        assert!(repo.get_attempt(&id).await.unwrap().is_none());
    }
}
