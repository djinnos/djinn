use super::*;
use crate::events::DjinnEventEnvelope;

impl TaskRepository {
    pub async fn log_activity(
        &self,
        task_id: Option<&str>,
        actor_id: &str,
        actor_role: &str,
        event_type: &str,
        payload: &str,
    ) -> Result<ActivityEntry> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        let mut tx = self.db.pool().begin().await?;
        sqlx::query(
            "INSERT INTO activity_log
                (id, task_id, actor_id, actor_role, event_type, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(&id)
        .bind(task_id)
        .bind(actor_id)
        .bind(actor_role)
        .bind(event_type)
        .bind(payload)
        .execute(&mut *tx)
        .await?;
        let entry = sqlx::query_as::<_, ActivityEntry>(
            "SELECT id, task_id, actor_id, actor_role, event_type, payload, created_at
             FROM activity_log WHERE id = ?1",
        )
        .bind(&id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        let payload_value: serde_json::Value = serde_json::from_str(payload)
            .unwrap_or(serde_json::Value::String(payload.to_owned()));
        self.events.send(DjinnEventEnvelope::activity_logged(
            task_id,
            event_type,
            actor_id,
            actor_role,
            &payload_value,
        ));
        Ok(entry)
    }

    pub async fn list_activity(&self, task_id: &str) -> Result<Vec<ActivityEntry>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, ActivityEntry>(
            "SELECT id, task_id, actor_id, actor_role, event_type, payload, created_at
             FROM activity_log WHERE task_id = ?1 AND archived = 0 ORDER BY created_at",
        )
        .bind(task_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Query activity log with optional filters: task_id, event_type, time range, pagination.
    pub async fn query_activity(&self, q: ActivityQuery) -> Result<Vec<ActivityEntry>> {
        self.db.ensure_initialized().await?;
        let mut clauses: Vec<String> = vec!["archived = 0".to_owned()];
        let mut params: Vec<SqlParam> = Vec::new();

        if let Some(ref pid) = q.project_id {
            clauses.push("EXISTS (SELECT 1 FROM tasks t WHERE t.id = activity_log.task_id AND t.project_id = ?)".to_owned());
            params.push(SqlParam::Text(pid.clone()));
        }

        if let Some(ref tid) = q.task_id {
            clauses.push("task_id = ?".to_owned());
            params.push(SqlParam::Text(tid.clone()));
        }
        if let Some(ref et) = q.event_type {
            clauses.push("event_type = ?".to_owned());
            params.push(SqlParam::Text(et.clone()));
        }
        if let Some(ref ar) = q.actor_role {
            clauses.push("actor_role = ?".to_owned());
            params.push(SqlParam::Text(ar.clone()));
        }
        if let Some(ref ft) = q.from_time {
            clauses.push("created_at >= ?".to_owned());
            params.push(SqlParam::Text(ft.clone()));
        }
        if let Some(ref tt) = q.to_time {
            clauses.push("created_at <= ?".to_owned());
            params.push(SqlParam::Text(tt.clone()));
        }

        let where_sql = clauses.join(" AND ");

        let sql = format!(
            "SELECT id, task_id, actor_id, actor_role, event_type, payload, created_at
             FROM activity_log WHERE {where_sql}
             ORDER BY created_at DESC LIMIT ? OFFSET ?"
        );
        let mut query = sqlx::query_as::<_, ActivityEntry>(&sql);
        for p in params {
            query = match p {
                SqlParam::Text(s) => query.bind(s),
                SqlParam::Integer(i) => query.bind(i),
            };
        }
        Ok(query
            .bind(q.limit)
            .bind(q.offset)
            .fetch_all(self.db.pool())
            .await?)
    }

    /// Fetch the AC snapshot from the last `task_review_start` event for a task.
    pub async fn last_review_start_ac_snapshot(&self, task_id: &str) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT payload FROM activity_log
             WHERE task_id = ?1 AND event_type = 'status_changed'
               AND json_extract(payload, '$.to_status') = 'in_task_review'
               AND archived = 0
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(task_id)
        .fetch_optional(self.db.pool())
        .await?;

        Ok(row.and_then(|(payload,)| {
            serde_json::from_str::<serde_json::Value>(&payload)
                .ok()
                .and_then(|v| v.get("ac_snapshot").map(|s| s.to_string()))
        }))
    }

    /// Soft-delete all activity entries for a task (set archived = 1).
    pub async fn archive_activity_for_task(&self, task_id: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;
        let result =
            sqlx::query("UPDATE activity_log SET archived = 1 WHERE task_id = ?1 AND archived = 0")
                .bind(task_id)
                .execute(self.db.pool())
                .await?;
        Ok(result.rows_affected())
    }
}
