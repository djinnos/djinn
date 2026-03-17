use super::*;

impl TaskRepository {
    /// Add a blocker relationship: `task_id` is blocked by `blocking_id`.
    ///
    /// Rejects self-loops and cycles detected via a recursive CTE walk of the
    /// existing blocking graph.
    pub async fn add_blocker(&self, task_id: &str, blocking_id: &str) -> Result<()> {
        if task_id == blocking_id {
            return Err(Error::Internal("task cannot block itself".into()));
        }
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        // Cycle detection: check if task_id already (transitively) blocks blocking_id.
        // If so, adding "blocking_id blocks task_id" would form a cycle.
        let would_cycle: i64 = sqlx::query_scalar(
            "WITH RECURSIVE reach(id) AS (
                 SELECT task_id FROM blockers WHERE blocking_task_id = ?1
                 UNION
                 SELECT b.task_id FROM blockers b JOIN reach r ON b.blocking_task_id = r.id
             )
             SELECT EXISTS(SELECT 1 FROM reach WHERE id = ?2)",
        )
        .bind(task_id)
        .bind(blocking_id)
        .fetch_one(&mut *tx)
        .await?;
        if would_cycle > 0 {
            return Err(Error::Internal(
                "would create circular blocker dependency".into(),
            ));
        }
        let result =
            sqlx::query("INSERT INTO blockers (task_id, blocking_task_id) VALUES (?1, ?2)")
                .bind(task_id)
                .bind(blocking_id)
                .execute(&mut *tx)
                .await;
        match result {
            Ok(_) => {}
            Err(sqlx::Error::Database(ref e))
                if e.message().contains("UNIQUE constraint failed") =>
            {
                // Duplicate blocker — idempotent, silently skip.
            }
            Err(e) => {
                return Err(Error::Internal(format!(
                    "failed to add blocker {blocking_id} → {task_id}: {e}"
                )));
            }
        }
        tx.commit().await?;

        if let Some(task) = self.get(task_id).await? {
            self.events
                .send(DjinnEventEnvelope::task_updated(&task, false));
        }

        if let Some(task) = self.get(blocking_id).await? {
            self.events
                .send(DjinnEventEnvelope::task_updated(&task, false));
        }

        Ok(())
    }

    pub async fn remove_blocker(&self, task_id: &str, blocking_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("DELETE FROM blockers WHERE task_id = ?1 AND blocking_task_id = ?2")
            .bind(task_id)
            .bind(blocking_id)
            .execute(self.db.pool())
            .await?;

        if let Some(task) = self.get(task_id).await? {
            self.events
                .send(DjinnEventEnvelope::task_updated(&task, false));
        }

        if let Some(task) = self.get(blocking_id).await? {
            self.events
                .send(DjinnEventEnvelope::task_updated(&task, false));
        }

        Ok(())
    }

    /// Atomically apply a batch of blocker additions and removals inside a
    /// single transaction.  This prevents a race where `list_ready` / `claim`
    /// can see the task with zero blockers between individual remove + add calls.
    pub async fn update_blockers_atomic(
        &self,
        task_id: &str,
        add: &[String],
        remove: &[String],
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;

        // Removals first (so adds can reference the freed edges if needed).
        for blocking_id in remove {
            sqlx::query("DELETE FROM blockers WHERE task_id = ?1 AND blocking_task_id = ?2")
                .bind(task_id)
                .bind(blocking_id)
                .execute(&mut *tx)
                .await?;
        }

        // Additions with cycle detection.
        for blocking_id in add {
            if task_id == blocking_id {
                return Err(Error::Internal("task cannot block itself".into()));
            }
            let would_cycle: i64 = sqlx::query_scalar(
                "WITH RECURSIVE reach(id) AS (
                     SELECT task_id FROM blockers WHERE blocking_task_id = ?1
                     UNION
                     SELECT b.task_id FROM blockers b JOIN reach r ON b.blocking_task_id = r.id
                 )
                 SELECT EXISTS(SELECT 1 FROM reach WHERE id = ?2)",
            )
            .bind(task_id)
            .bind(blocking_id)
            .fetch_one(&mut *tx)
            .await?;
            if would_cycle > 0 {
                return Err(Error::Internal(
                    "would create circular blocker dependency".into(),
                ));
            }
            let result =
                sqlx::query("INSERT INTO blockers (task_id, blocking_task_id) VALUES (?1, ?2)")
                    .bind(task_id)
                    .bind(blocking_id)
                    .execute(&mut *tx)
                    .await;
            match result {
                Ok(_) => {}
                Err(sqlx::Error::Database(ref e))
                    if e.message().contains("UNIQUE constraint failed") =>
                {
                    // Duplicate blocker — idempotent, silently skip.
                }
                Err(e) => {
                    return Err(Error::Internal(format!(
                        "failed to add blocker {blocking_id} → {task_id}: {e}"
                    )));
                }
            }
        }

        tx.commit().await?;

        // Emit events for all affected tasks (after commit).
        let mut notified = std::collections::HashSet::new();
        notified.insert(task_id.to_owned());
        for id in add.iter().chain(remove.iter()) {
            notified.insert(id.clone());
        }
        for id in &notified {
            if let Some(task) = self.get(id).await? {
                self.events
                    .send(DjinnEventEnvelope::task_updated(&task, false));
            }
        }

        Ok(())
    }

    /// List tasks that are blocking `task_id`, with title and status info.
    pub async fn list_blockers(&self, task_id: &str) -> Result<Vec<BlockerRef>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, BlockerRef>(
            "SELECT t.id AS task_id, t.short_id, t.title, t.status
             FROM blockers b
             JOIN tasks t ON t.id = b.blocking_task_id
             WHERE b.task_id = ?1
             ORDER BY t.created_at",
        )
        .bind(task_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    /// List tasks that are blocked BY `blocking_task_id`.
    pub async fn list_blocked_by(&self, blocking_task_id: &str) -> Result<Vec<BlockerRef>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, BlockerRef>(
            "SELECT t.id AS task_id, t.short_id, t.title, t.status
             FROM blockers b
             JOIN tasks t ON t.id = b.task_id
             WHERE b.blocking_task_id = ?1
             ORDER BY t.created_at",
        )
        .bind(blocking_task_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Emit `TaskUpdated` for tasks that were blocked by `completed_task_id` and
    /// are now fully unblocked (all blockers are in resolved post-merge/closed states).
    pub(super) async fn emit_unblocked_tasks(&self, completed_task_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        let unblocked = sqlx::query_as::<_, Task>(
            "SELECT t.id, t.project_id, t.short_id, t.epic_id, t.title, t.description, t.design,
                    t.issue_type, t.status, t.priority, t.owner, t.labels,
                    t.acceptance_criteria, t.reopen_count, t.continuation_count,
                    t.verification_failure_count,
                    t.created_at, t.updated_at, t.closed_at,
                    t.close_reason, t.merge_commit_sha, t.memory_refs
             FROM blockers b
             JOIN tasks t ON t.id = b.task_id
             WHERE b.blocking_task_id = ?1
               AND t.status = 'open'
               AND NOT EXISTS (
                   SELECT 1 FROM blockers b2
                   JOIN tasks bt ON bt.id = b2.blocking_task_id
                    WHERE b2.task_id = t.id
                       AND bt.status != 'closed'
                )",
        )
        .bind(completed_task_id)
        .fetch_all(self.db.pool())
        .await?;

        for t in unblocked {
            self.events
                .send(DjinnEventEnvelope::task_updated(&t, false));
        }
        Ok(())
    }
}
