use super::*;
use sqlx::Row;

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
        let payload_value: serde_json::Value = serde_json::from_str(payload).map_err(|e| {
            crate::Error::InvalidData(format!("invalid json for activity_log.payload: {e}"))
        })?;
        let mut tx = self.db.pool().begin().await?;
        sqlx::query!(
            "INSERT INTO activity_log
                (id, task_id, actor_id, actor_role, event_type, payload)
             VALUES ($1, $2, $3, $4, $5, $6)",
            id,
            task_id,
            actor_id,
            actor_role,
            event_type,
            payload_value,
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

    /// Count the quality-strike reopen events for a task.
    ///
    /// A quality strike is a reopen-like status transition whose effective
    /// `reopen_class` is `review_rejected`, `merge_queue_failed`, or
    /// `other` (the conservative default for historical rows that lack
    /// the field).  `merge_conflict` and `superseded` reopens are
    /// **excluded** — they do not count toward intervention thresholds.
    ///
    /// Reopen-like transitions are identified heuristically from the
    /// activity payload: `to_status = 'open'` from one of the
    /// review/PR/closed source states.
    ///
    /// uv3p Part C: reopen events whose `activity_log.created_at` predates the
    /// task's `human_review_resolved_at` marker are excluded. When a human
    /// closes a human-review hold the strike ledger it just adjudicated must
    /// read as consumed — only strikes accrued AFTER the release count, so the
    /// park evaluator cannot re-park on the stale pre-release counters (ygj0).
    /// The subquery is NULL for tasks that were never held, in which case every
    /// reopen counts (`created_at > NULL` is NULL → the OR keeps the row).
    pub async fn quality_reopen_count(&self, task_id: &str) -> Result<i64> {
        self.db.ensure_initialized().await?;
        // NOTE: dynamic SQL with JSONB operators — compile-time check not possible.
        //
        // The `reopen_class` allow-list excludes `merge_conflict`, `superseded`,
        // AND `infra` (infrastructure / provider-attempt failures that must not
        // count as worker/task-quality strikes).
        let count = sqlx::query_scalar::<_, i64>(
            r#"SELECT COUNT(*)::bigint FROM activity_log
             WHERE task_id = $1
               AND event_type = 'status_changed'
               AND archived = FALSE
               AND (payload ->> 'to_status') = 'open'
               AND (payload ->> 'from_status') IN ('in_task_review', 'pr_draft', 'pr_review', 'closed', 'approved')
               AND COALESCE(payload ->> 'reopen_class', 'other') IN ('review_rejected', 'merge_queue_failed', 'other')
               AND (
                   (SELECT t.human_review_resolved_at FROM tasks t WHERE t.id = $1) IS NULL
                   OR created_at > (SELECT t.human_review_resolved_at FROM tasks t WHERE t.id = $1)
               )"#,
        )
        .bind(task_id)
        .fetch_one(self.db.pool())
        .await?;
        Ok(count)
    }

    /// uv3p Part C: read the consumed-hold marker for a task.
    ///
    /// `Some(ts)` is the UTC instant the most recent human-review hold on this
    /// task was resolved (see [`Self::mark_human_review_resolved`]); `None` when
    /// the task has never been held. The park guard uses it as the floor for
    /// "post-release" evidence so it never re-parks on strikes a human already
    /// adjudicated.
    pub async fn human_review_resolved_at(&self, task_id: &str) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;
        let ts = sqlx::query_scalar::<_, Option<String>>(
            "SELECT human_review_resolved_at FROM tasks WHERE id = $1",
        )
        .bind(task_id)
        .fetch_optional(self.db.pool())
        .await?
        .flatten();
        Ok(ts)
    }

    /// uv3p Part C: stamp the consumed-hold marker on every source task that
    /// `hold_task_id` was blocking.
    ///
    /// Called when a `human-review-hold` remediation task closes. Records the
    /// resolution instant so `quality_reopen_count` (and the park guard's
    /// evidence floor) discount every strike accrued before the human released
    /// the source — the ygj0 duplicate-hold fix. Returns the number of source
    /// tasks stamped. Idempotent by construction: re-closing writes a later
    /// timestamp, never a duplicate hold.
    pub async fn mark_human_review_resolved(&self, hold_task_id: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;
        let result = sqlx::query(
            r#"UPDATE tasks SET
                    human_review_resolved_at =
                        to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                    updated_at =
                        to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                 WHERE id IN (SELECT task_id FROM blockers WHERE blocking_task_id = $1)"#,
        )
        .bind(hold_task_id)
        .execute(self.db.pool())
        .await?;
        Ok(result.rows_affected())
    }

    /// Return the most recent reopen ledger entries for a task.
    ///
    /// Each entry carries its typed [`ReopenClass`]; historical rows that
    /// lack the `reopen_class` payload field default to
    /// [`ReopenClass::Other`].
    pub async fn recent_reopen_ledger(
        &self,
        task_id: &str,
        limit: i64,
    ) -> Result<Vec<ReopenLedgerEntry>> {
        self.db.ensure_initialized().await?;
        // NOTE: dynamic SQL with JSONB operators — compile-time check not possible.
        let rows = sqlx::query(
            r#"SELECT
                 COALESCE(payload ->> 'reopen_class', 'other') AS reopen_class,
                 created_at,
                 payload ->> 'from_status' AS from_status,
                 payload ->> 'reason' AS reason
             FROM activity_log
             WHERE task_id = $1
               AND event_type = 'status_changed'
               AND archived = FALSE
               AND (payload ->> 'to_status') = 'open'
               AND (payload ->> 'from_status') IN ('in_task_review', 'pr_draft', 'pr_review', 'closed', 'approved')
             ORDER BY created_at DESC
             LIMIT $2"#,
        )
        .bind(task_id)
        .bind(limit)
        .fetch_all(self.db.pool())
        .await?;

        let entries = rows
            .into_iter()
            .map(|row| {
                let class_str: Option<String> = row.get("reopen_class");
                let created_at: Option<String> = row.get("created_at");
                let from_status: Option<String> = row.get("from_status");
                let reason: Option<String> = row.get("reason");
                ReopenLedgerEntry {
                    reopen_class: ReopenClass::parse(class_str.as_deref().unwrap_or("other")),
                    created_at: created_at.unwrap_or_default(),
                    from_status: from_status.unwrap_or_default(),
                    reason,
                }
            })
            .collect();
        Ok(entries)
    }
}
