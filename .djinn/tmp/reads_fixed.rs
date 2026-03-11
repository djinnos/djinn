use super::*;

impl TaskRepository {
    /// List all tasks in a project (for peer reconciliation - SYNC-14).
    pub async fn list_by_project(&self, project_id: &str) -> Result<Vec<Task>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Task>(
            "SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, verification_failure_count, created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, memory_refs
             FROM tasks WHERE project_id = ?1 ORDER BY priority, created_at",
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn list_by_epic(&self, epic_id: &str) -> Result<Vec<Task>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Task>(
            "SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, verification_failure_count, created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, memory_refs
             FROM tasks WHERE epic_id = ?1 ORDER BY priority, created_at",
        )
        .bind(epic_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn list_by_status(&self, status: &str) -> Result<Vec<Task>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Task>(
            "SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, verification_failure_count, created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, memory_refs
             FROM tasks WHERE status = ?1 ORDER BY priority, created_at",
        )
        .bind(status)
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Task>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Task>(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_optional(self.db.pool())
            .await?)
    }

    pub async fn get_by_short_id(&self, short_id: &str) -> Result<Option<Task>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Task>(
            "SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, verification_failure_count, created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, memory_refs
             FROM tasks WHERE short_id = ?1",
        )
        .bind(short_id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Resolve a task by UUID or short_id.
    pub async fn resolve(&self, id_or_short: &str) -> Result<Option<Task>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Task>(
            "SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, verification_failure_count, created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, memory_refs
             FROM tasks WHERE id = ?1 OR short_id = ?1",
        )
        .bind(id_or_short)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn resolve_in_project(
        &self,
        project_id: &str,
        id_or_short: &str,
    ) -> Result<Option<Task>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Task>(
            "SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, verification_failure_count, created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, memory_refs
             FROM tasks WHERE project_id = ?1 AND (id = ?2 OR short_id = ?2)",
        )
        .bind(project_id)
        .bind(id_or_short)
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Find tasks whose `memory_refs` JSON array contains the given permalink.
    ///
    /// Uses a LIKE query on the JSON text — fast enough for the expected table
    /// sizes and avoids requiring a json_each virtual table scan.
    pub async fn list_by_memory_ref(&self, permalink: &str) -> Result<Vec<Task>> {
        let pattern = format!("%\"{permalink}\"%");
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Task>(
            "SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, verification_failure_count, created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, memory_refs
             FROM tasks WHERE memory_refs LIKE ?1
             ORDER BY priority, created_at",
        )
        .bind(&pattern)
        .fetch_all(self.db.pool())
        .await?)
    }

    /// List tasks eligible for sync export (SYNC-12).
    ///
    /// Includes all non-closed tasks plus tasks closed within the last hour.
    /// Tasks closed longer than 1 hour ago are evicted from the export to keep
    /// JSONL files small.
    pub async fn list_for_export(&self, project_id: Option<&str>) -> Result<Vec<Task>> {
        self.db.ensure_initialized().await?;
        let sql = if project_id.is_some() {
            "SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, verification_failure_count, created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, memory_refs
             FROM tasks
             WHERE project_id = ?1
               AND (status != 'closed' OR closed_at > datetime('now', '-1 hour'))
             ORDER BY priority, created_at"
        } else {
            "SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, verification_failure_count, created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, memory_refs
             FROM tasks
             WHERE (status != 'closed' OR closed_at > datetime('now', '-1 hour'))
             ORDER BY priority, created_at"
        };

        if let Some(pid) = project_id {
            Ok(sqlx::query_as::<_, Task>(sql)
                .bind(pid)
                .fetch_all(self.db.pool())
                .await?)
        } else {
            Ok(sqlx::query_as::<_, Task>(sql)
                .fetch_all(self.db.pool())
                .await?)
        }
    }

    /// Upsert a task received from a peer sync (last-writer-wins by updated_at).
    ///
    /// Returns `true` if the row was inserted or updated, `false` if:
    ///   - The task's `epic_id` doesn't exist locally (FK constraint).
    ///   - The local copy is already newer or equal (LWW check).
    ///   - Any other constraint violation (UNIQUE on short_id, CHECK, etc.).
    pub async fn upsert_peer(&self, task: &Task) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        // Verify epic exists before INSERT when task references one.
        if let Some(epic_id) = &task.epic_id {
            let epic_exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM epics WHERE id = ?1")
                .bind(epic_id)
                .fetch_one(&mut *tx)
                .await?;
            if epic_exists == 0 {
                tx.commit().await?;
                return Ok(false);
            }
        }

        let changed = match sqlx::query(
            "INSERT INTO tasks (
                id, project_id, short_id, epic_id, title, description, design,
                issue_type, status, priority, owner, labels,
                acceptance_criteria, reopen_count, continuation_count, verification_failure_count,
                created_at, updated_at, closed_at,
                close_reason, merge_commit_sha, memory_refs
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22
             )
             ON CONFLICT(id) DO UPDATE SET
                project_id          = excluded.project_id,
                title               = excluded.title,
                description         = excluded.description,
                design              = excluded.design,
                issue_type          = excluded.issue_type,
                status              = excluded.status,
                priority            = excluded.priority,
                owner               = excluded.owner,
                labels              = excluded.labels,
                acceptance_criteria = excluded.acceptance_criteria,
                reopen_count        = excluded.reopen_count,
                continuation_count  = excluded.continuation_count,
                verification_failure_count = excluded.verification_failure_count,
                updated_at          = excluded.updated_at,
                closed_at           = excluded.closed_at,
                close_reason        = excluded.close_reason,
                merge_commit_sha    = excluded.merge_commit_sha,
                memory_refs         = excluded.memory_refs
             WHERE excluded.updated_at > tasks.updated_at
               AND NOT (tasks.status = 'closed' AND excluded.status != 'closed')",
        )
        .bind(&task.id)
        .bind(&task.project_id)
        .bind(&task.short_id)
        .bind(&task.epic_id)
        .bind(&task.title)
        .bind(&task.description)
        .bind(&task.design)
        .bind(&task.issue_type)
        .bind(&task.status)
        .bind(task.priority)
        .bind(&task.owner)
        .bind(&task.labels)
        .bind(&task.acceptance_criteria)
        .bind(task.reopen_count)
        .bind(task.continuation_count)
        .bind(task.verification_failure_count)
        .bind(&task.created_at)
        .bind(&task.updated_at)
        .bind(&task.closed_at)
        .bind(&task.close_reason)
        .bind(&task.merge_commit_sha)
        .bind(&task.memory_refs)
        .execute(&mut *tx)
        .await
        {
            Ok(result) => result.rows_affected() > 0,
            Err(sqlx::Error::Database(db_err)) if is_constraint_violation(db_err.as_ref()) => false,
            Err(e) => return Err(Error::Database(e)),
        };
        tx.commit().await?;

        if changed && let Ok(Some(updated)) = self.get(&task.id).await {
            let _ = self.events.send(DjinnEvent::TaskUpdated { task: updated, from_sync: true });
        }
        Ok(changed)
    }

    /// Upsert a peer task within an existing transaction (SYNC-10).
    ///
    /// Same logic as `upsert_peer` but executes within the provided transaction
    /// and does NOT emit events. The caller is responsible for emitting events
    /// after the transaction commits.
    ///
    /// Returns `true` if the row was inserted or updated.
    pub async fn upsert_peer_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        task: &Task,
    ) -> Result<bool> {
        // Verify epic exists before INSERT when task references one.
        if let Some(epic_id) = &task.epic_id {
            let epic_exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM epics WHERE id = ?1")
                .bind(epic_id)
                .fetch_one(&mut **tx)
                .await?;
            if epic_exists == 0 {
                return Ok(false);
            }
        }

        let changed = match sqlx::query(
            "INSERT INTO tasks (
                id, project_id, short_id, epic_id, title, description, design,
                issue_type, status, priority, owner, labels,
                acceptance_criteria, reopen_count, continuation_count, verification_failure_count,
                created_at, updated_at, closed_at,
                close_reason, merge_commit_sha, memory_refs
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22
             )
             ON CONFLICT(id) DO UPDATE SET
                project_id          = excluded.project_id,
                title               = excluded.title,
                description         = excluded.description,
                design              = excluded.design,
                issue_type          = excluded.issue_type,
                status              = excluded.status,
                priority            = excluded.priority,
                owner               = excluded.owner,
                labels              = excluded.labels,
                acceptance_criteria = excluded.acceptance_criteria,
                reopen_count        = excluded.reopen_count,
                continuation_count  = excluded.continuation_count,
                verification_failure_count = excluded.verification_failure_count,
                updated_at          = excluded.updated_at,
                closed_at           = excluded.closed_at,
                close_reason        = excluded.close_reason,
                merge_commit_sha    = excluded.merge_commit_sha,
                memory_refs         = excluded.memory_refs
             WHERE excluded.updated_at > tasks.updated_at
               AND NOT (tasks.status = 'closed' AND excluded.status != 'closed')",
        )
        .bind(&task.id)
        .bind(&task.project_id)
        .bind(&task.short_id)
        .bind(&task.epic_id)
        .bind(&task.title)
        .bind(&task.description)
        .bind(&task.design)
        .bind(&task.issue_type)
        .bind(&task.status)
        .bind(task.priority)
        .bind(&task.owner)
        .bind(&task.labels)
        .bind(&task.acceptance_criteria)
        .bind(task.reopen_count)
        .bind(task.continuation_count)
        .bind(task.verification_failure_count)
        .bind(&task.created_at)
        .bind(&task.updated_at)
        .bind(&task.closed_at)
        .bind(&task.close_reason)
        .bind(&task.merge_commit_sha)
        .bind(&task.memory_refs)
        .execute(&mut **tx)
        .await
        {
            Ok(result) => result.rows_affected() > 0,
            Err(sqlx::Error::Database(db_err)) if is_constraint_violation(db_err.as_ref()) => false,
            Err(e) => return Err(Error::Database(e)),
        };
        Ok(changed)
    }
}
    /// Reconcile local tasks against a peer's exported task list (SYNC-14).
    /// Closes local tasks owned by the peer that are not present in the peer's export.
    /// 
    /// This implements peer reconciliation:
    /// - Finds tasks where owner = peer_user_id
    /// - Skips already-closed tasks (terminal state protection - SYNC-11)
    /// - Skips tasks whose IDs are in peer_task_ids
    /// - Closes remaining tasks with close_reason = 'peer_reconciled'
    /// 
    /// Returns the count of tasks that were reconciled (closed).
    pub async fn reconcile_peer_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        peer_user_id: &str,
        peer_task_ids: &[String],
    ) -> Result<usize> {
        // Safety guard: if peer export is empty, skip reconciliation
        if peer_task_ids.is_empty() {
            return Ok(0);
        }

        // Build placeholders for the NOT IN clause
        let placeholders: String = peer_task_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        
        // Find tasks owned by peer that are not in their export and not already closed
        let sql_select = format!(
            "SELECT id FROM tasks WHERE owner = ? AND status != 'closed' AND id NOT IN ({})",
            placeholders
        );
        
        let mut query = sqlx::query_scalar::<_, String>(&sql_select).bind(peer_user_id);
        for id in peer_task_ids {
            query = query.bind(id);
        }
        
        let tasks_to_close: Vec<String> = query.fetch_all(&mut **tx).await?;
        
        if tasks_to_close.is_empty() {
            return Ok(0);
        }
        
        // Close the tasks with peer_reconciled reason
        let now = crate::db::Db::now_utc().to_rfc3339();
        let placeholders: String = tasks_to_close.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        
        let sql_update = format!(
            "UPDATE tasks SET status = 'closed', close_reason = 'peer_reconciled', closed_at = ? WHERE id IN ({})",
            placeholders
        );
        
        let mut update_query = sqlx::query(&sql_update).bind(&now);
        for id in &tasks_to_close {
            update_query = update_query.bind(id);
        }
        
        let result = update_query.execute(&mut **tx).await?;
        
        Ok(result.rows_affected() as usize)
    }
