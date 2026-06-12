use djinn_core::models::TaskStatus;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::database::Database;

/// Durable coordinator dispatch-decision state for one task.
///
/// Wall-clock timestamps are exposed as strings so the coordinator can
/// translate them into process-local `Instant` values at startup. Runtime
/// `Instant`s are intentionally never serialized.
#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct DispatchStateRecord {
    pub task_id: String,
    pub failure_streak: i64,
    pub cooldown_until: Option<String>,
    pub escalation_count: i64,
    pub last_dispatched_at: Option<String>,
    pub last_dispatched_role: Option<String>,
    pub inflight_creator_user_id: Option<String>,
    pub inflight_model_id: Option<String>,
    pub updated_at: String,
}

pub struct DispatchStateRepository {
    db: Database,
}

pub struct DispatchStateUpsert<'a> {
    pub task_id: &'a str,
    pub failure_streak: i64,
    pub cooldown_until: Option<&'a str>,
    pub escalation_count: i64,
    pub last_dispatched_at: Option<&'a str>,
    pub last_dispatched_role: Option<&'a str>,
    pub inflight_creator_user_id: Option<&'a str>,
    pub inflight_model_id: Option<&'a str>,
}

impl DispatchStateRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn upsert(&self, record: DispatchStateUpsert<'_>) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            r#"INSERT INTO dispatch_state (
                    task_id, failure_streak, cooldown_until, escalation_count,
                    last_dispatched_at, last_dispatched_role,
                    inflight_creator_user_id, inflight_model_id, updated_at
                ) VALUES ($1, $2, $3::timestamptz, $4, $5::timestamptz, $6, $7, $8, now())
                ON CONFLICT (task_id) DO UPDATE SET
                    failure_streak = EXCLUDED.failure_streak,
                    cooldown_until = EXCLUDED.cooldown_until,
                    escalation_count = EXCLUDED.escalation_count,
                    last_dispatched_at = EXCLUDED.last_dispatched_at,
                    last_dispatched_role = EXCLUDED.last_dispatched_role,
                    inflight_creator_user_id = EXCLUDED.inflight_creator_user_id,
                    inflight_model_id = EXCLUDED.inflight_model_id,
                    updated_at = now()"#,
        )
        .bind(record.task_id)
        .bind(record.failure_streak as i32)
        .bind(record.cooldown_until)
        .bind(record.escalation_count as i32)
        .bind(record.last_dispatched_at)
        .bind(record.last_dispatched_role)
        .bind(record.inflight_creator_user_id)
        .bind(record.inflight_model_id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn get(&self, task_id: &str) -> Result<Option<DispatchStateRecord>> {
        self.db.ensure_initialized().await?;
        Ok(
            sqlx::query_as::<_, DispatchStateRecord>(DISPATCH_STATE_SELECT_BY_TASK_ID)
                .bind(task_id)
                .fetch_optional(self.db.pool())
                .await?,
        )
    }

    pub async fn list_all(&self) -> Result<Vec<DispatchStateRecord>> {
        self.db.ensure_initialized().await?;
        Ok(
            sqlx::query_as::<_, DispatchStateRecord>(DISPATCH_STATE_SELECT_ALL)
                .fetch_all(self.db.pool())
                .await?,
        )
    }

    /// Clear all durable dispatch-decision fields while retaining the row.
    pub async fn clear(&self, task_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            r#"INSERT INTO dispatch_state (task_id, updated_at)
               VALUES ($1, now())
               ON CONFLICT (task_id) DO UPDATE SET
                    failure_streak = 0,
                    cooldown_until = NULL,
                    escalation_count = 0,
                    last_dispatched_at = NULL,
                    last_dispatched_role = NULL,
                    inflight_creator_user_id = NULL,
                    inflight_model_id = NULL,
                    updated_at = now()"#,
        )
        .bind(task_id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Delete rows for terminal task statuses. Returns the number of rows pruned.
    pub async fn cleanup_terminal(&self) -> Result<u64> {
        self.db.ensure_initialized().await?;
        let result = sqlx::query(
            "DELETE FROM dispatch_state ds
             USING tasks t
             WHERE t.id = ds.task_id
               AND t.status = $1",
        )
        .bind(TaskStatus::Closed.as_str())
        .execute(self.db.pool())
        .await?;
        Ok(result.rows_affected())
    }

    /// Rows with an active cooldown after `after_iso`.
    pub async fn list_due_for_cooldown(&self, after_iso: &str) -> Result<Vec<DispatchStateRecord>> {
        self.db.ensure_initialized().await?;
        Ok(
            sqlx::query_as::<_, DispatchStateRecord>(DISPATCH_STATE_SELECT_ACTIVE_COOLDOWNS)
                .bind(after_iso)
                .fetch_all(self.db.pool())
                .await?,
        )
    }
}

const DISPATCH_STATE_SELECT_BY_TASK_ID: &str = r#"
    SELECT
        task_id,
        failure_streak::bigint AS failure_streak,
        to_char(cooldown_until AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS cooldown_until,
        escalation_count::bigint AS escalation_count,
        to_char(last_dispatched_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS last_dispatched_at,
        last_dispatched_role,
        inflight_creator_user_id,
        inflight_model_id,
        to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS updated_at
    FROM dispatch_state
    WHERE task_id = $1
"#;

const DISPATCH_STATE_SELECT_ALL: &str = r#"
    SELECT
        task_id,
        failure_streak::bigint AS failure_streak,
        to_char(cooldown_until AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS cooldown_until,
        escalation_count::bigint AS escalation_count,
        to_char(last_dispatched_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS last_dispatched_at,
        last_dispatched_role,
        inflight_creator_user_id,
        inflight_model_id,
        to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS updated_at
    FROM dispatch_state
    ORDER BY updated_at ASC
"#;

const DISPATCH_STATE_SELECT_ACTIVE_COOLDOWNS: &str = r#"
    SELECT
        task_id,
        failure_streak::bigint AS failure_streak,
        to_char(cooldown_until AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS cooldown_until,
        escalation_count::bigint AS escalation_count,
        to_char(last_dispatched_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS last_dispatched_at,
        last_dispatched_role,
        inflight_creator_user_id,
        inflight_model_id,
        to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS updated_at
    FROM dispatch_state
    WHERE cooldown_until IS NOT NULL
      AND cooldown_until > $1::timestamptz
    ORDER BY cooldown_until ASC
"#;
