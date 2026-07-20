use serde::{Deserialize, Serialize};

use crate::Result;
use crate::database::Database;
use crate::repositories::pg_placeholders;

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

    /// Delete rows for terminal task statuses. Returns deleted task ids.
    pub async fn cleanup_terminal(&self, terminal_statuses: &[&str]) -> Result<Vec<String>> {
        self.db.ensure_initialized().await?;
        if terminal_statuses.is_empty() {
            return Ok(Vec::new());
        }

        let placeholders = pg_placeholders(terminal_statuses.len(), 1);
        let select_sql = format!(
            "SELECT ds.task_id
             FROM dispatch_state ds
             JOIN tasks t ON t.id = ds.task_id
             WHERE t.status IN ({placeholders})
             ORDER BY ds.task_id"
        );
        let mut select = sqlx::query_scalar::<_, String>(&select_sql);
        for status in terminal_statuses {
            select = select.bind(status);
        }
        let task_ids = select.fetch_all(self.db.pool()).await?;

        if task_ids.is_empty() {
            return Ok(task_ids);
        }

        sqlx::query("DELETE FROM dispatch_state WHERE task_id = ANY($1)")
            .bind(&task_ids)
            .execute(self.db.pool())
            .await?;

        Ok(task_ids)
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

#[cfg(test)]
mod tests {
    use djinn_core::events::EventBus;

    use super::*;
    use crate::repositories::epic::EpicRepository;
    use crate::repositories::test_support::seed_user_with_id;

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    /// Create a task via raw SQL (no TaskRepository dep), returns (project_id, task_id).
    async fn create_task(db: &Database, bus: EventBus) -> (String, String) {
        let epic_repo = EpicRepository::new(db.clone(), bus);
        let epic = epic_repo
            .create("Epic", "", "", "", "", None)
            .await
            .unwrap();

        let task_id = uuid::Uuid::now_v7().to_string();
        let short_id = format!("t{}{}", &task_id[..6], &task_id[task_id.len() - 6..]);
        let creator_uuid = uuid::Uuid::now_v7();
        let creator_id = creator_uuid.to_string();
        seed_user_with_id(
            db,
            &creator_id,
            (creator_uuid.as_u128() & i64::MAX as u128) as i64,
            &format!("dispatch-state-fixture-{creator_id}"),
        )
        .await;
        sqlx::query(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                                issue_type, priority, owner, status, continuation_count, labels, acceptance_criteria, memory_refs,
                                created_by_user_id)
             VALUES ($1, $2, $3, $4, 'Task', '', '', 'task', 0, '', 'open', 0, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $5)",
        )
        .bind(&task_id)
        .bind(&epic.project_id)
        .bind(&short_id)
        .bind(&epic.id)
        .bind(&creator_id)
        .execute(db.pool())
        .await
        .unwrap();

        (epic.project_id, task_id)
    }

    fn populated_record(task_id: &str) -> DispatchStateUpsert<'_> {
        DispatchStateUpsert {
            task_id,
            failure_streak: 3,
            cooldown_until: Some("2026-01-02T03:04:05.678Z"),
            escalation_count: 2,
            last_dispatched_at: Some("2026-01-02T02:04:05.123Z"),
            last_dispatched_role: Some("worker"),
            inflight_creator_user_id: Some("user-123"),
            inflight_model_id: Some("model-abc"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upsert_creates_row_when_missing() {
        let db = test_db();
        let (_project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = DispatchStateRepository::new(db);

        repo.upsert(populated_record(&task_id)).await.unwrap();

        let fetched = repo.get(&task_id).await.unwrap().expect("row must exist");
        assert_eq!(fetched.task_id, task_id);
        assert_eq!(fetched.failure_streak, 3);
        assert_eq!(
            fetched.cooldown_until.as_deref(),
            Some("2026-01-02T03:04:05.678Z")
        );
        assert_eq!(fetched.escalation_count, 2);
        assert_eq!(
            fetched.last_dispatched_at.as_deref(),
            Some("2026-01-02T02:04:05.123Z")
        );
        assert_eq!(fetched.last_dispatched_role.as_deref(), Some("worker"));
        assert_eq!(
            fetched.inflight_creator_user_id.as_deref(),
            Some("user-123")
        );
        assert_eq!(fetched.inflight_model_id.as_deref(), Some("model-abc"));
        assert!(!fetched.updated_at.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upsert_overwrites_fields_on_conflict() {
        let db = test_db();
        let (_project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = DispatchStateRepository::new(db);

        repo.upsert(populated_record(&task_id)).await.unwrap();
        let before = repo.get(&task_id).await.unwrap().expect("row must exist");

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        repo.upsert(DispatchStateUpsert {
            task_id: &task_id,
            failure_streak: 8,
            cooldown_until: Some("2026-02-03T04:05:06.789Z"),
            escalation_count: 5,
            last_dispatched_at: Some("2026-02-03T03:05:06.321Z"),
            last_dispatched_role: Some("reviewer"),
            inflight_creator_user_id: Some("user-456"),
            inflight_model_id: Some("model-def"),
        })
        .await
        .unwrap();

        let after = repo.get(&task_id).await.unwrap().expect("row must exist");
        assert_eq!(after.failure_streak, 8);
        assert_eq!(
            after.cooldown_until.as_deref(),
            Some("2026-02-03T04:05:06.789Z")
        );
        assert_eq!(after.escalation_count, 5);
        assert_eq!(
            after.last_dispatched_at.as_deref(),
            Some("2026-02-03T03:05:06.321Z")
        );
        assert_eq!(after.last_dispatched_role.as_deref(), Some("reviewer"));
        assert_eq!(after.inflight_creator_user_id.as_deref(), Some("user-456"));
        assert_eq!(after.inflight_model_id.as_deref(), Some("model-def"));
        assert!(
            after.updated_at > before.updated_at,
            "updated_at should advance on conflict update"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_returns_none_for_missing() {
        let db = test_db();
        let repo = DispatchStateRepository::new(db);

        let missing = repo
            .get("00000000-0000-0000-0000-000000000000")
            .await
            .unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clear_zeroes_fields_keeps_row() {
        let db = test_db();
        let (_project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = DispatchStateRepository::new(db.clone());

        repo.upsert(populated_record(&task_id)).await.unwrap();
        let before = repo.get(&task_id).await.unwrap().expect("row must exist");

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        repo.clear(&task_id).await.unwrap();

        let after = repo.get(&task_id).await.unwrap().expect("row must remain");
        assert_eq!(after.task_id, task_id);
        assert_eq!(after.failure_streak, 0);
        assert!(after.cooldown_until.is_none());
        assert_eq!(after.escalation_count, 0);
        assert!(after.last_dispatched_at.is_none());
        assert!(after.last_dispatched_role.is_none());
        assert!(after.inflight_creator_user_id.is_none());
        assert!(after.inflight_model_id.is_none());
        assert!(
            after.updated_at > before.updated_at,
            "updated_at should advance on clear"
        );

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        repo.upsert(DispatchStateUpsert {
            task_id: &task_id,
            failure_streak: 1,
            cooldown_until: None,
            escalation_count: 1,
            last_dispatched_at: None,
            last_dispatched_role: Some("planner"),
            inflight_creator_user_id: None,
            inflight_model_id: None,
        })
        .await
        .unwrap();
        let count: i64 =
            sqlx::query_scalar("SELECT count(*)::bigint FROM dispatch_state WHERE task_id = $1")
                .bind(&task_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(count, 1, "clear must keep one upsertable row");
        let reupserted = repo.get(&task_id).await.unwrap().expect("row must exist");
        assert_eq!(reupserted.failure_streak, 1);
        assert!(reupserted.cooldown_until.is_none());
        assert_eq!(reupserted.escalation_count, 1);
        assert_eq!(reupserted.last_dispatched_role.as_deref(), Some("planner"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cleanup_terminal_deletes_only_terminal_rows() {
        let db = test_db();
        let (_closed_project_id, closed_task_id) = create_task(&db, EventBus::noop()).await;
        let (_done_project_id, done_task_id) = create_task(&db, EventBus::noop()).await;
        let (_open_project_id, open_task_id) = create_task(&db, EventBus::noop()).await;
        let repo = DispatchStateRepository::new(db.clone());

        sqlx::query("UPDATE tasks SET status = $1 WHERE id = $2")
            .bind("closed")
            .bind(&closed_task_id)
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE tasks SET status = $1 WHERE id = $2")
            .bind("done")
            .bind(&done_task_id)
            .execute(db.pool())
            .await
            .unwrap();

        for task_id in [&closed_task_id, &done_task_id, &open_task_id] {
            repo.upsert(populated_record(task_id)).await.unwrap();
        }

        let mut expected = vec![closed_task_id.clone(), done_task_id.clone()];
        expected.sort();
        let deleted = repo.cleanup_terminal(&["closed", "done"]).await.unwrap();
        assert_eq!(deleted, expected);
        assert!(repo.get(&closed_task_id).await.unwrap().is_none());
        assert!(repo.get(&done_task_id).await.unwrap().is_none());
        assert!(repo.get(&open_task_id).await.unwrap().is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upsert_with_none_cooldown_is_idempotent() {
        let db = test_db();
        let (_project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = DispatchStateRepository::new(db);

        repo.upsert(DispatchStateUpsert {
            task_id: &task_id,
            failure_streak: 0,
            cooldown_until: None,
            escalation_count: 0,
            last_dispatched_at: None,
            last_dispatched_role: None,
            inflight_creator_user_id: None,
            inflight_model_id: None,
        })
        .await
        .unwrap();

        let fetched = repo.get(&task_id).await.unwrap().expect("row must exist");
        assert!(fetched.cooldown_until.is_none());
    }
}
