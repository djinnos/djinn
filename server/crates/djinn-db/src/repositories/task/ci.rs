use super::*;
use djinn_core::models::{CiStatus, TaskPrCiSnapshot, TaskPrCiSnapshotInput};
use sqlx::Row;

fn ci_snapshot_from_row(row: sqlx::postgres::PgRow) -> Result<TaskPrCiSnapshot> {
    let names: serde_json::Value = row.try_get("blocking_required_check_names")?;
    let blocking_required_check_names = serde_json::from_value(names).map_err(|e| {
        Error::InvalidData(format!(
            "invalid task_pr_ci_snapshots.blocking_required_check_names: {e}"
        ))
    })?;
    let ci_status_raw: String = row.try_get("ci_status")?;

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
        let row = sqlx::query(
            r#"SELECT task_id, pr_number, head_sha, ci_status,
                      blocking_required_check_names, failure_fingerprint,
                      first_seen_at, last_seen_at, same_signature_count,
                      last_remediation_base_sha
                 FROM task_pr_ci_snapshots
                WHERE task_id = $1 AND pr_number = $2"#,
        )
        .bind(task_id)
        .bind(pr_number)
        .fetch_optional(self.db.pool())
        .await?;
        row.map(ci_snapshot_from_row).transpose()
    }

    /// Upsert a task/PR CI snapshot, resetting first-seen semantics whenever the
    /// observed head or failure signature changes.
    pub async fn upsert_ci_snapshot(
        &self,
        input: TaskPrCiSnapshotInput,
    ) -> Result<TaskPrCiSnapshot> {
        self.db.ensure_initialized().await?;
        let blocking_names = serde_json::to_value(&input.blocking_required_check_names)?;
        let row = sqlx::query(
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
                        ELSE to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
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
                    last_seen_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                    same_signature_count = CASE
                        WHEN EXCLUDED.same_signature_count > 0
                        THEN EXCLUDED.same_signature_count
                        ELSE task_pr_ci_snapshots.same_signature_count
                    END,
                    last_remediation_base_sha = CASE
                        WHEN COALESCE(task_pr_ci_snapshots.head_sha, '') = COALESCE(EXCLUDED.head_sha, '')
                        THEN EXCLUDED.last_remediation_base_sha
                        ELSE NULL
                    END
                RETURNING task_id, pr_number, head_sha, ci_status,
                          blocking_required_check_names, failure_fingerprint,
                          first_seen_at, last_seen_at, same_signature_count,
                          last_remediation_base_sha"#,
        )
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

    /// Reset stale CI state after observing a different PR head SHA.
    pub async fn reset_ci_snapshot_for_head(
        &self,
        task_id: &str,
        pr_number: i64,
        head_sha: &str,
    ) -> Result<TaskPrCiSnapshot> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query(
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
                        ELSE to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                    END,
                    last_seen_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                    same_signature_count = 0,
                    last_remediation_base_sha = NULL
                RETURNING task_id, pr_number, head_sha, ci_status,
                          blocking_required_check_names, failure_fingerprint,
                          first_seen_at, last_seen_at, same_signature_count,
                          last_remediation_base_sha"#,
        )
        .bind(task_id)
        .bind(pr_number)
        .bind(head_sha)
        .fetch_one(self.db.pool())
        .await?;
        ci_snapshot_from_row(row)
    }
}
