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
        Ok(entry)
    }

    pub async fn list_activity(&self, task_id: &str) -> Result<Vec<ActivityEntry>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, ActivityEntry>(
            "SELECT id, task_id, actor_id, actor_role, event_type, payload, created_at
             FROM activity_log WHERE task_id = ?1 ORDER BY created_at",
        )
        .bind(task_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Query activity log with optional filters: task_id, event_type, time range, pagination.
    pub async fn query_activity(&self, q: ActivityQuery) -> Result<Vec<ActivityEntry>> {
        self.db.ensure_initialized().await?;
        let mut clauses: Vec<String> = Vec::new();
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
        if let Some(ref ft) = q.from_time {
            clauses.push("created_at >= ?".to_owned());
            params.push(SqlParam::Text(ft.clone()));
        }
        if let Some(ref tt) = q.to_time {
            clauses.push("created_at <= ?".to_owned());
            params.push(SqlParam::Text(tt.clone()));
        }

        let where_sql = if clauses.is_empty() {
            "1=1".to_owned()
        } else {
            clauses.join(" AND ")
        };

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
}
