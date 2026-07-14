use super::*;
use djinn_core::models::{
    CiStatus, MergeQueueLane, TaskPrCiSnapshot, TaskPrCiSnapshotInput, TaskPrCiSnapshotMqLaneInput,
};
use sqlx::Row;

/// The full column list read back by [`ci_snapshot_from_row`]. Shared between
/// the SELECT, the upsert `RETURNING`, and the reset `RETURNING` so every path
/// that maps a row through `ci_snapshot_from_row` returns the same shape —
/// including the merge-queue (`mq_*`) lane.
const CI_SNAPSHOT_COLUMNS: &str = "task_id, pr_number, head_sha, ci_status, \
     blocking_required_check_names, failure_fingerprint, \
     first_seen_at, last_seen_at, same_signature_count, \
     last_remediation_base_sha, \
     mq_state, mq_run_id, mq_head_sha, mq_failed_check_names, \
     mq_failure_fingerprint, mq_same_signature_count, \
     mq_first_seen_at, mq_last_seen_at";

/// SQL expression producing the canonical UTC text timestamp used by every
/// snapshot column (`YYYY-MM-DDThh:mm:ss.mmmZ`).
const UTC_NOW_TEXT: &str = "to_char(now() at time zone 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')";

fn ci_snapshot_from_row(row: sqlx::postgres::PgRow) -> Result<TaskPrCiSnapshot> {
    let names: serde_json::Value = row.try_get("blocking_required_check_names")?;
    let blocking_required_check_names = serde_json::from_value(names).map_err(|e| {
        Error::InvalidData(format!(
            "invalid task_pr_ci_snapshots.blocking_required_check_names: {e}"
        ))
    })?;
    let ci_status_raw: String = row.try_get("ci_status")?;

    // The merge-queue lane is present only when a lane state has been recorded
    // (`mq_state IS NOT NULL`). All other `mq_*` columns are nullable and are
    // cleared together with `mq_state` when a new PR head is observed.
    let merge_queue = match row.try_get::<Option<String>, _>("mq_state")? {
        Some(state) => {
            let failed_check_names =
                match row.try_get::<Option<serde_json::Value>, _>("mq_failed_check_names")? {
                    Some(v) => serde_json::from_value(v).map_err(|e| {
                        Error::InvalidData(format!(
                            "invalid task_pr_ci_snapshots.mq_failed_check_names: {e}"
                        ))
                    })?,
                    None => Vec::new(),
                };
            Some(MergeQueueLane {
                state,
                run_id: row.try_get("mq_run_id")?,
                head_sha: row.try_get("mq_head_sha")?,
                failed_check_names,
                failure_fingerprint: row.try_get("mq_failure_fingerprint")?,
                same_signature_count: row
                    .try_get::<Option<i64>, _>("mq_same_signature_count")?
                    .unwrap_or(0),
                first_seen_at: row.try_get("mq_first_seen_at")?,
                last_seen_at: row.try_get("mq_last_seen_at")?,
            })
        }
        None => None,
    };

    Ok(TaskPrCiSnapshot {
        task_id: row.try_get("task_id")?,
        pr_number: row.try_get("pr_number")?,
        head_sha: row
            .try_get::<Option<String>, _>("head_sha")?
            .unwrap_or_default(),
        ci_status: CiStatus::parse(&ci_status_raw)?,
        blocking_required_check_names,
        failure_fingerprint: row.try_get("failure_fingerprint")?,
        first_seen_at: row.try_get("first_seen_at")?,
        last_seen_at: row.try_get("last_seen_at")?,
        same_signature_count: row.try_get("same_signature_count")?,
        last_remediation_base_sha: row.try_get("last_remediation_base_sha")?,
        merge_queue,
    })
}

impl TaskRepository {
    /// Read the durable current-head CI snapshot for a task/PR pair.
    pub async fn get_ci_snapshot_for_task_pr(
        &self,
        task_id: &str,
        pr_number: i64,
    ) -> Result<Option<TaskPrCiSnapshot>> {
        self.db.ensure_initialized().await?;
        let sql = format!(
            "SELECT {CI_SNAPSHOT_COLUMNS} \
               FROM task_pr_ci_snapshots \
              WHERE task_id = $1 AND pr_number = $2"
        );
        let row = sqlx::query(&sql)
            .bind(task_id)
            .bind(pr_number)
            .fetch_optional(self.db.pool())
            .await?;
        row.map(ci_snapshot_from_row).transpose()
    }

    /// Upsert a task/PR CI snapshot, resetting first-seen semantics whenever the
    /// observed head or failure signature changes.
    ///
    /// This is the sole PR-head lane writer. It NEVER writes the merge-queue
    /// (`mq_*`) lane values; it only PRESERVES them on a same-head observation
    /// and CLEARS them when a new head SHA is observed (a new head invalidates
    /// the prior queue verdict). The merge-queue lane is written exclusively by
    /// [`Self::upsert_ci_snapshot_mq_lane`].
    pub async fn upsert_ci_snapshot(
        &self,
        input: TaskPrCiSnapshotInput,
    ) -> Result<TaskPrCiSnapshot> {
        self.db.ensure_initialized().await?;
        let blocking_names = serde_json::to_value(&input.blocking_required_check_names)?;
        // `same_head` is true when this observation is for the SAME head SHA as
        // the stored row; it gates both the existing downgrade/first-seen
        // guards and the merge-queue lane preservation.
        let sql = format!(
            r#"INSERT INTO task_pr_ci_snapshots (
                    task_id, pr_number, head_sha, ci_status,
                    blocking_required_check_names, failure_fingerprint,
                    same_signature_count, last_remediation_base_sha
                ) VALUES ($1, $2, $3, $4, $5::jsonb, $6, $7, $8)
                ON CONFLICT (task_id, pr_number) DO UPDATE SET
                    first_seen_at = CASE
                        WHEN COALESCE(task_pr_ci_snapshots.head_sha, '') = COALESCE(EXCLUDED.head_sha, '')
                         AND task_pr_ci_snapshots.ci_status = EXCLUDED.ci_status
                         AND task_pr_ci_snapshots.blocking_required_check_names = EXCLUDED.blocking_required_check_names
                         AND COALESCE(task_pr_ci_snapshots.failure_fingerprint, '') = COALESCE(EXCLUDED.failure_fingerprint, '')
                        THEN task_pr_ci_snapshots.first_seen_at
                        ELSE {UTC_NOW_TEXT}
                    END,
                    head_sha = EXCLUDED.head_sha,
                    -- Downgrade guard: a later `unknown` observation (empty
                    -- check-runs — GitHub data momentarily unavailable, or a
                    -- no-CI repo re-observed on the review path) must NOT clobber
                    -- an established `passing` for the SAME head SHA. Without this
                    -- the CI merge gate reads the downgraded `unknown` and holds
                    -- the merge forever on vanilla (no-CI) repos. Failing/pending
                    -- may still overwrite passing (real state changes); only the
                    -- passing→unknown transition on an unchanged head is blocked.
                    ci_status = CASE
                        WHEN COALESCE(task_pr_ci_snapshots.head_sha, '') = COALESCE(EXCLUDED.head_sha, '')
                         AND task_pr_ci_snapshots.ci_status = 'passing'
                         AND EXCLUDED.ci_status = 'unknown'
                        THEN task_pr_ci_snapshots.ci_status
                        ELSE EXCLUDED.ci_status
                    END,
                    blocking_required_check_names = EXCLUDED.blocking_required_check_names,
                    failure_fingerprint = EXCLUDED.failure_fingerprint,
                    last_seen_at = {UTC_NOW_TEXT},
                    same_signature_count = CASE
                        WHEN EXCLUDED.same_signature_count > 0
                        THEN EXCLUDED.same_signature_count
                        ELSE task_pr_ci_snapshots.same_signature_count
                    END,
                    last_remediation_base_sha = CASE
                        WHEN COALESCE(task_pr_ci_snapshots.head_sha, '') = COALESCE(EXCLUDED.head_sha, '')
                        THEN EXCLUDED.last_remediation_base_sha
                        ELSE NULL
                    END,
                    -- Merge-queue lane: PRESERVE on a same-head observation
                    -- (this PR-head writer must never clobber the dequeue-path
                    -- lane and cause the snapshot to flip-flop), CLEAR on a new
                    -- head (the prior queue verdict was for a head that no
                    -- longer exists). Never SET to a fresh value here — the lane
                    -- is written solely by `upsert_ci_snapshot_mq_lane`.
                    mq_state = CASE WHEN COALESCE(task_pr_ci_snapshots.head_sha, '') = COALESCE(EXCLUDED.head_sha, '') THEN task_pr_ci_snapshots.mq_state ELSE NULL END,
                    mq_run_id = CASE WHEN COALESCE(task_pr_ci_snapshots.head_sha, '') = COALESCE(EXCLUDED.head_sha, '') THEN task_pr_ci_snapshots.mq_run_id ELSE NULL END,
                    mq_head_sha = CASE WHEN COALESCE(task_pr_ci_snapshots.head_sha, '') = COALESCE(EXCLUDED.head_sha, '') THEN task_pr_ci_snapshots.mq_head_sha ELSE NULL END,
                    mq_failed_check_names = CASE WHEN COALESCE(task_pr_ci_snapshots.head_sha, '') = COALESCE(EXCLUDED.head_sha, '') THEN task_pr_ci_snapshots.mq_failed_check_names ELSE NULL END,
                    mq_failure_fingerprint = CASE WHEN COALESCE(task_pr_ci_snapshots.head_sha, '') = COALESCE(EXCLUDED.head_sha, '') THEN task_pr_ci_snapshots.mq_failure_fingerprint ELSE NULL END,
                    mq_same_signature_count = CASE WHEN COALESCE(task_pr_ci_snapshots.head_sha, '') = COALESCE(EXCLUDED.head_sha, '') THEN task_pr_ci_snapshots.mq_same_signature_count ELSE NULL END,
                    mq_first_seen_at = CASE WHEN COALESCE(task_pr_ci_snapshots.head_sha, '') = COALESCE(EXCLUDED.head_sha, '') THEN task_pr_ci_snapshots.mq_first_seen_at ELSE NULL END,
                    mq_last_seen_at = CASE WHEN COALESCE(task_pr_ci_snapshots.head_sha, '') = COALESCE(EXCLUDED.head_sha, '') THEN task_pr_ci_snapshots.mq_last_seen_at ELSE NULL END
                RETURNING {CI_SNAPSHOT_COLUMNS}"#
        );
        let row = sqlx::query(&sql)
            .bind(&input.task_id)
            .bind(input.pr_number)
            .bind(&input.head_sha)
            .bind(input.ci_status.as_str())
            .bind(blocking_names)
            .bind(&input.failure_fingerprint)
            .bind(input.same_signature_count)
            .bind(&input.last_remediation_base_sha)
            .fetch_one(self.db.pool())
            .await?;
        ci_snapshot_from_row(row)
    }

    /// Upsert ONLY the merge-queue (`merge_group`) failure lane for a task/PR.
    ///
    /// Written exclusively by the PR-poller dequeue path when GitHub's merge
    /// queue rejects the PR. The PR-head lane fields are left at their defaults
    /// when the row is created here (an mq observation can precede any PR-head
    /// observation). Per-lane same-signature semantics mirror the PR-head lane:
    /// the same fingerprint increments `mq_same_signature_count` and keeps
    /// `mq_first_seen_at`; a different fingerprint resets the count to 1 and
    /// resets `mq_first_seen_at`.
    pub async fn upsert_ci_snapshot_mq_lane(
        &self,
        input: TaskPrCiSnapshotMqLaneInput,
    ) -> Result<TaskPrCiSnapshot> {
        self.db.ensure_initialized().await?;
        let failed_names = serde_json::to_value(&input.failed_check_names)?;
        // `same_fp` is true when the incoming fingerprint matches the stored
        // lane fingerprint (NULL-safe): same signature → increment + keep
        // first_seen; different → reset count to 1 + reset first_seen.
        let sql = format!(
            r#"INSERT INTO task_pr_ci_snapshots (
                    task_id, pr_number,
                    mq_state, mq_run_id, mq_head_sha, mq_failed_check_names,
                    mq_failure_fingerprint, mq_same_signature_count,
                    mq_first_seen_at, mq_last_seen_at
                ) VALUES (
                    $1, $2,
                    $3, $4, $5, $6::jsonb,
                    $7, 1,
                    {UTC_NOW_TEXT}, {UTC_NOW_TEXT}
                )
                ON CONFLICT (task_id, pr_number) DO UPDATE SET
                    mq_state = EXCLUDED.mq_state,
                    mq_run_id = EXCLUDED.mq_run_id,
                    mq_head_sha = EXCLUDED.mq_head_sha,
                    mq_failed_check_names = EXCLUDED.mq_failed_check_names,
                    mq_failure_fingerprint = EXCLUDED.mq_failure_fingerprint,
                    mq_first_seen_at = CASE
                        WHEN COALESCE(task_pr_ci_snapshots.mq_failure_fingerprint, '') = COALESCE(EXCLUDED.mq_failure_fingerprint, '')
                        THEN COALESCE(task_pr_ci_snapshots.mq_first_seen_at, {UTC_NOW_TEXT})
                        ELSE {UTC_NOW_TEXT}
                    END,
                    mq_last_seen_at = {UTC_NOW_TEXT},
                    mq_same_signature_count = CASE
                        WHEN COALESCE(task_pr_ci_snapshots.mq_failure_fingerprint, '') = COALESCE(EXCLUDED.mq_failure_fingerprint, '')
                        THEN COALESCE(task_pr_ci_snapshots.mq_same_signature_count, 0) + 1
                        ELSE 1
                    END
                RETURNING {CI_SNAPSHOT_COLUMNS}"#
        );
        let row = sqlx::query(&sql)
            .bind(&input.task_id)
            .bind(input.pr_number)
            .bind(&input.state)
            .bind(input.run_id)
            .bind(&input.head_sha)
            .bind(failed_names)
            .bind(&input.failure_fingerprint)
            .fetch_one(self.db.pool())
            .await?;
        ci_snapshot_from_row(row)
    }

    /// Reset stale CI state after observing a different PR head SHA.
    ///
    /// Clears both the PR-head lane (back to `unknown`) and the merge-queue
    /// (`mq_*`) lane — a new head invalidates any prior queue verdict.
    pub async fn reset_ci_snapshot_for_head(
        &self,
        task_id: &str,
        pr_number: i64,
        head_sha: &str,
    ) -> Result<TaskPrCiSnapshot> {
        self.db.ensure_initialized().await?;
        let sql = format!(
            r#"INSERT INTO task_pr_ci_snapshots (
                    task_id, pr_number, head_sha, ci_status,
                    blocking_required_check_names, failure_fingerprint,
                    same_signature_count, last_remediation_base_sha
                ) VALUES ($1, $2, $3, 'unknown', '[]'::jsonb, NULL, 0, NULL)
                ON CONFLICT (task_id, pr_number) DO UPDATE SET
                    head_sha = EXCLUDED.head_sha,
                    ci_status = 'unknown',
                    blocking_required_check_names = '[]'::jsonb,
                    failure_fingerprint = NULL,
                    first_seen_at = CASE
                        WHEN COALESCE(task_pr_ci_snapshots.head_sha, '') = COALESCE(EXCLUDED.head_sha, '')
                        THEN task_pr_ci_snapshots.first_seen_at
                        ELSE {UTC_NOW_TEXT}
                    END,
                    last_seen_at = {UTC_NOW_TEXT},
                    same_signature_count = 0,
                    last_remediation_base_sha = NULL,
                    -- New head → drop the prior merge-queue verdict entirely.
                    mq_state = NULL,
                    mq_run_id = NULL,
                    mq_head_sha = NULL,
                    mq_failed_check_names = NULL,
                    mq_failure_fingerprint = NULL,
                    mq_same_signature_count = NULL,
                    mq_first_seen_at = NULL,
                    mq_last_seen_at = NULL
                RETURNING {CI_SNAPSHOT_COLUMNS}"#
        );
        let row = sqlx::query(&sql)
            .bind(task_id)
            .bind(pr_number)
            .bind(head_sha)
            .fetch_one(self.db.pool())
            .await?;
        ci_snapshot_from_row(row)
    }
}
