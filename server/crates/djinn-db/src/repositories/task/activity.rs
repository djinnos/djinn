use super::*;

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
        sqlx::query!(
            "INSERT INTO activity_log
                (id, task_id, actor_id, actor_role, event_type, payload)
             VALUES ($1, $2, $3, $4, $5, $6::jsonb)",
            id,
            task_id,
            actor_id,
            actor_role,
            event_type,
            payload,
        )
        .execute(&mut *tx)
        .await?;
        let entry = sqlx::query_as!(
            ActivityEntry,
            r#"SELECT id, task_id, actor_id, actor_role, event_type, payload::text AS "payload!", created_at
             FROM activity_log WHERE id = $1"#,
            id,
        )
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        let payload_value: serde_json::Value =
            serde_json::from_str(payload).unwrap_or(serde_json::Value::String(payload.to_owned()));
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
        Ok(sqlx::query_as!(
            ActivityEntry,
            r#"SELECT id, task_id, actor_id, actor_role, event_type, payload::text AS "payload!", created_at
             FROM activity_log WHERE task_id = $1 AND archived = FALSE ORDER BY created_at"#,
            task_id,
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Query activity log with optional filters: task_id, event_type, time range, pagination.
    pub async fn query_activity(&self, q: ActivityQuery) -> Result<Vec<ActivityEntry>> {
        self.db.ensure_initialized().await?;
        let mut clauses: Vec<String> = vec!["archived = FALSE".to_owned()];
        let mut params: Vec<SqlParam> = Vec::new();
        let mut next_param: usize = 1;
        let mut bind_idx = || {
            let i = next_param;
            next_param += 1;
            i
        };

        if let Some(ref pid) = q.project_id {
            let i = bind_idx();
            clauses.push(format!("EXISTS (SELECT 1 FROM tasks t WHERE t.id = activity_log.task_id AND t.project_id = ${i})"));
            params.push(SqlParam::Text(pid.clone()));
        }
        if let Some(ref tid) = q.task_id {
            let i = bind_idx();
            clauses.push(format!("task_id = ${i}"));
            params.push(SqlParam::Text(tid.clone()));
        }
        if let Some(ref et) = q.event_type {
            let i = bind_idx();
            clauses.push(format!("event_type = ${i}"));
            params.push(SqlParam::Text(et.clone()));
        }
        if let Some(ref ar) = q.actor_role {
            let i = bind_idx();
            clauses.push(format!("actor_role = ${i}"));
            params.push(SqlParam::Text(ar.clone()));
        }
        if let Some(ref ft) = q.from_time {
            let i = bind_idx();
            clauses.push(format!("created_at >= ${i}"));
            params.push(SqlParam::Text(ft.clone()));
        }
        if let Some(ref tt) = q.to_time {
            let i = bind_idx();
            clauses.push(format!("created_at <= ${i}"));
            params.push(SqlParam::Text(tt.clone()));
        }

        let where_sql = clauses.join(" AND ");
        let limit_idx = bind_idx();
        let offset_idx = bind_idx();

        // NOTE: dynamic SQL — compile-time check not possible (WHERE clauses assembled at runtime).
        let sql = format!(
            "SELECT id, task_id, actor_id, actor_role, event_type, payload::text AS payload, created_at
             FROM activity_log WHERE {where_sql}
             ORDER BY created_at DESC LIMIT ${limit_idx} OFFSET ${offset_idx}"
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
        let row = sqlx::query_scalar!(
            r#"SELECT payload::text AS "payload!" FROM activity_log
             WHERE task_id = $1 AND event_type = 'status_changed'
               AND payload ->> 'to_status' = 'in_task_review'
               AND archived = FALSE
             ORDER BY created_at DESC LIMIT 1"#,
            task_id,
        )
        .fetch_optional(self.db.pool())
        .await?;

        Ok(row.and_then(|payload: String| {
            serde_json::from_str::<serde_json::Value>(&payload)
                .ok()
                .and_then(|v| v.get("ac_snapshot").map(|s| s.to_string()))
        }))
    }

    /// Soft-delete all activity entries for a task (set archived = TRUE).
    pub async fn archive_activity_for_task(&self, task_id: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;
        let result = sqlx::query!(
            "UPDATE activity_log SET archived = TRUE WHERE task_id = $1 AND archived = FALSE",
            task_id,
        )
        .execute(self.db.pool())
        .await?;
        Ok(result.rows_affected())
    }
}
