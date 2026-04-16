use super::task_select_where_id;
use super::*;

use tracing::warn;

impl TaskRepository {
    /// List all tasks in a project (for peer reconciliation - SYNC-14).
    pub async fn list_by_project(&self, project_id: &str) -> Result<Vec<Task>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Task,
            r#"SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    `status` AS "status!", priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, verification_failure_count,
                    total_reopen_count, total_verification_failure_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs,
                    agent_type,
                    CAST(0 AS SIGNED) AS "unresolved_blocker_count!: i64"
             FROM tasks WHERE project_id = ? ORDER BY priority, created_at"#,
            project_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn list_by_epic(&self, epic_id: &str) -> Result<Vec<Task>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Task,
            r#"SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    `status` AS "status!", priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, verification_failure_count,
                    total_reopen_count, total_verification_failure_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs,
                    agent_type,
                    CAST(0 AS SIGNED) AS "unresolved_blocker_count!: i64"
             FROM tasks WHERE epic_id = ? ORDER BY priority, created_at"#,
            epic_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn list_by_status(&self, status: &str) -> Result<Vec<Task>> {
        self.list_by_status_filtered(status, false).await
    }

    /// Like `list_by_status`, but when `exclude_blocked` is true, omits tasks
    /// that have unresolved blockers (blocking tasks not yet closed).
    pub async fn list_by_status_filtered(
        &self,
        status: &str,
        exclude_blocked: bool,
    ) -> Result<Vec<Task>> {
        self.db.ensure_initialized().await?;
        let blocker_filter = if exclude_blocked {
            "AND NOT EXISTS (SELECT 1 FROM blockers b JOIN tasks bt ON b.blocking_task_id = bt.id WHERE b.task_id = tasks.id AND bt.`status` != 'closed')"
        } else {
            ""
        };
        // NOTE: dynamic SQL (optional blocker_filter fragment) — compile-time check not possible
        let sql = format!(
            "SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    `status`, priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, verification_failure_count,
                    total_reopen_count, total_verification_failure_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs
             FROM tasks WHERE `status` = ? {blocker_filter} ORDER BY priority, created_at"
        );
        Ok(sqlx::query_as::<_, Task>(&sql)
            .bind(status)
            .fetch_all(self.db.pool())
            .await?)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Task>> {
        self.db.ensure_initialized().await?;
        Ok(task_select_where_id!(id)
            .fetch_optional(self.db.pool())
            .await?)
    }

    pub async fn get_by_short_id(&self, short_id: &str) -> Result<Option<Task>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Task,
            r#"SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    `status` AS "status!", priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, verification_failure_count,
                    total_reopen_count, total_verification_failure_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs,
                    agent_type,
                    CAST(0 AS SIGNED) AS "unresolved_blocker_count!: i64"
             FROM tasks WHERE short_id = ?"#,
            short_id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Resolve a task by UUID or short_id.
    pub async fn resolve(&self, id_or_short: &str) -> Result<Option<Task>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Task,
            r#"SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    `status` AS "status!", priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, verification_failure_count,
                    total_reopen_count, total_verification_failure_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs,
                    agent_type,
                    CAST(0 AS SIGNED) AS "unresolved_blocker_count!: i64"
             FROM tasks WHERE id = ? OR short_id = ?"#,
            id_or_short,
            id_or_short
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn resolve_in_project(
        &self,
        project_id: &str,
        id_or_short: &str,
    ) -> Result<Option<Task>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Task,
            r#"SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    `status` AS "status!", priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, verification_failure_count,
                    total_reopen_count, total_verification_failure_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs,
                    agent_type,
                    CAST(0 AS SIGNED) AS "unresolved_blocker_count!: i64"
             FROM tasks WHERE project_id = ? AND (id = ? OR short_id = ?)"#,
            project_id,
            id_or_short,
            id_or_short
        )
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
        Ok(sqlx::query_as!(
            Task,
            r#"SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    `status` AS "status!", priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, verification_failure_count,
                    total_reopen_count, total_verification_failure_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs,
                    agent_type,
                    CAST(0 AS SIGNED) AS "unresolved_blocker_count!: i64"
             FROM tasks WHERE memory_refs LIKE ?
             ORDER BY priority, created_at"#,
            pattern
        )
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
        // NOTE: dynamic SQL (SELECT variant depends on project filter) — compile-time check not possible
        let sql = if project_id.is_some() {
            "SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    `status`, priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, verification_failure_count,
                    total_reopen_count, total_verification_failure_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs
             FROM tasks
             WHERE project_id = ?
               AND (`status` != 'closed' OR closed_at > DATE_SUB(NOW(3), INTERVAL 1 HOUR))
             ORDER BY priority, created_at"
        } else {
            "SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    `status`, priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, verification_failure_count,
                    total_reopen_count, total_verification_failure_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs
             FROM tasks
             WHERE (`status` != 'closed' OR closed_at > DATE_SUB(NOW(3), INTERVAL 1 HOUR))
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
    ///
    /// On UNIQUE(short_id) constraint violation, the incoming short_id is
    /// extended by one character from the task UUID hex and retried (SYNC-15).
    pub async fn upsert_peer(&self, task: &Task) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        // Verify epic exists before INSERT when task references one.
        if let Some(epic_id) = &task.epic_id {
            let epic_exists =
                sqlx::query_scalar!("SELECT COUNT(*) FROM epics WHERE id = ?", epic_id)
                    .fetch_one(&mut *tx)
                    .await?;
            if epic_exists == 0 {
                tx.commit().await?;
                return Ok(false);
            }
        }

        // Clone task for mutation if we need to extend short_id
        let mut task = task.clone();
        let mut retry_count = 0;
        const MAX_RETRIES: usize = 3;

        let changed = loop {
            let result = sqlx::query(
                "INSERT INTO tasks (
                    id, project_id, short_id, epic_id, title, description, design,
                    issue_type, `status`, priority, owner, labels,
                    acceptance_criteria, reopen_count, continuation_count, verification_failure_count,
                    total_reopen_count, total_verification_failure_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs
                 ) VALUES (
                    ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                    ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
                 )
                 ON DUPLICATE KEY UPDATE
                    project_id          = VALUES(project_id),
                    title               = VALUES(title),
                    description         = VALUES(description),
                    design              = VALUES(design),
                    issue_type          = VALUES(issue_type),
                    `status`            = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(`status`), tasks.`status`),
                    priority            = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(priority), tasks.priority),
                    owner               = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(owner), tasks.owner),
                    labels              = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(labels), tasks.labels),
                    acceptance_criteria = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(acceptance_criteria), tasks.acceptance_criteria),
                    reopen_count        = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(reopen_count), tasks.reopen_count),
                    continuation_count  = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(continuation_count), tasks.continuation_count),
                    verification_failure_count = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(verification_failure_count), tasks.verification_failure_count),
                    total_reopen_count  = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(total_reopen_count), tasks.total_reopen_count),
                    total_verification_failure_count = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(total_verification_failure_count), tasks.total_verification_failure_count),
                    intervention_count  = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(intervention_count), tasks.intervention_count),
                    last_intervention_at = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(last_intervention_at), tasks.last_intervention_at),
                    closed_at           = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(closed_at), tasks.closed_at),
                    close_reason        = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(close_reason), tasks.close_reason),
                    merge_commit_sha    = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(merge_commit_sha), tasks.merge_commit_sha),
                    pr_url              = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(pr_url), tasks.pr_url),
                    merge_conflict_metadata = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(merge_conflict_metadata), tasks.merge_conflict_metadata),
                    memory_refs         = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(memory_refs), tasks.memory_refs),
                    updated_at          = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(updated_at), tasks.updated_at)",
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
            .bind(task.total_reopen_count)
            .bind(task.total_verification_failure_count)
            .bind(task.intervention_count)
            .bind(&task.last_intervention_at)
            .bind(&task.created_at)
            .bind(&task.updated_at)
            .bind(&task.closed_at)
            .bind(&task.close_reason)
            .bind(&task.merge_commit_sha)
            .bind(&task.pr_url)
            .bind(&task.merge_conflict_metadata)
            .bind(&task.memory_refs)
            .execute(&mut *tx)
            .await;

            match result {
                Ok(res) => break res.rows_affected() > 0,
                Err(sqlx::Error::Database(db_err)) if is_constraint_violation(db_err.as_ref()) => {
                    // Check if this is a short_id collision we can handle
                    let constraint_name = extract_constraint_name(db_err.as_ref());

                    if constraint_name.as_deref() == Some("short_id") {
                        retry_count += 1;

                        if retry_count > MAX_RETRIES {
                            warn!(
                                task_id = %task.id,
                                short_id = %task.short_id,
                                retry_count,
                                "Short ID collision retry limit exceeded after {MAX_RETRIES} attempts"
                            );
                            return Err(Error::Sqlx(sqlx::Error::Database(db_err)));
                        }

                        // Get the next character from the UUID hex string
                        let uuid_hex_chars: Vec<char> = task.id.chars().collect();
                        if let Some(next_char) = uuid_hex_chars.get(retry_count - 1) {
                            warn!(
                                task_id = %task.id,
                                short_id = %task.short_id,
                                retry_count,
                                "Short ID collision detected, extending with char '{next_char}'"
                            );
                            task.short_id.push(*next_char);
                        } else {
                            // Shouldn't happen with valid UUIDs, but handle gracefully
                            warn!(
                                task_id = %task.id,
                                short_id = %task.short_id,
                                retry_count,
                                "Short ID collision but UUID exhausted, cannot extend further"
                            );
                            return Err(Error::Sqlx(sqlx::Error::Database(db_err)));
                        }
                        // Continue to next loop iteration with extended short_id
                    } else {
                        // Other constraint violations (FK, etc.) should not be retried
                        warn!(
                            constraint = %db_err.message(),
                            task_id = %task.id,
                            "Non-retriable constraint violation during peer upsert"
                        );
                        return Err(Error::Sqlx(sqlx::Error::Database(db_err)));
                    }
                }
                Err(e) => return Err(Error::Sqlx(e)),
            }
        };

        tx.commit().await?;

        if changed && let Ok(Some(updated)) = self.get(&task.id).await {
            self.events
                .send(DjinnEventEnvelope::task_updated(&updated, true));
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
    ///
    /// On UNIQUE(short_id) constraint violation, the incoming short_id is
    /// extended by one character from the task UUID hex and retried (SYNC-15).
    pub async fn upsert_peer_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        task: &Task,
    ) -> Result<bool> {
        // Verify epic exists before INSERT when task references one.
        if let Some(epic_id) = &task.epic_id {
            let epic_exists: i64 =
                sqlx::query_scalar!("SELECT COUNT(*) FROM epics WHERE id = ?", epic_id)
                    .fetch_one(&mut **tx)
                    .await?;
            if epic_exists == 0 {
                return Ok(false);
            }
        }

        // Clone task for mutation if we need to extend short_id
        let mut task = task.clone();
        let mut retry_count = 0;
        const MAX_RETRIES: usize = 3;

        loop {
            let result = sqlx::query(
                "INSERT INTO tasks (
                    id, project_id, short_id, epic_id, title, description, design,
                    issue_type, `status`, priority, owner, labels,
                    acceptance_criteria, reopen_count, continuation_count, verification_failure_count,
                    total_reopen_count, total_verification_failure_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs
                 ) VALUES (
                    ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                    ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
                 )
                 ON DUPLICATE KEY UPDATE
                    project_id          = VALUES(project_id),
                    title               = VALUES(title),
                    description         = VALUES(description),
                    design              = VALUES(design),
                    issue_type          = VALUES(issue_type),
                    `status`            = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(`status`), tasks.`status`),
                    priority            = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(priority), tasks.priority),
                    owner               = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(owner), tasks.owner),
                    labels              = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(labels), tasks.labels),
                    acceptance_criteria = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(acceptance_criteria), tasks.acceptance_criteria),
                    reopen_count        = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(reopen_count), tasks.reopen_count),
                    continuation_count  = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(continuation_count), tasks.continuation_count),
                    verification_failure_count = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(verification_failure_count), tasks.verification_failure_count),
                    total_reopen_count  = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(total_reopen_count), tasks.total_reopen_count),
                    total_verification_failure_count = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(total_verification_failure_count), tasks.total_verification_failure_count),
                    intervention_count  = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(intervention_count), tasks.intervention_count),
                    last_intervention_at = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(last_intervention_at), tasks.last_intervention_at),
                    closed_at           = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(closed_at), tasks.closed_at),
                    close_reason        = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(close_reason), tasks.close_reason),
                    merge_commit_sha    = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(merge_commit_sha), tasks.merge_commit_sha),
                    pr_url              = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(pr_url), tasks.pr_url),
                    merge_conflict_metadata = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(merge_conflict_metadata), tasks.merge_conflict_metadata),
                    memory_refs         = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(memory_refs), tasks.memory_refs),
                    updated_at          = IF(VALUES(updated_at) > tasks.updated_at AND NOT (tasks.`status` = 'closed' AND VALUES(`status`) != 'closed'), VALUES(updated_at), tasks.updated_at)",
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
            .bind(task.total_reopen_count)
            .bind(task.total_verification_failure_count)
            .bind(task.intervention_count)
            .bind(&task.last_intervention_at)
            .bind(&task.created_at)
            .bind(&task.updated_at)
            .bind(&task.closed_at)
            .bind(&task.close_reason)
            .bind(&task.merge_commit_sha)
            .bind(&task.pr_url)
            .bind(&task.merge_conflict_metadata)
            .bind(&task.memory_refs)
            .execute(&mut **tx)
            .await;

            match result {
                Ok(res) => return Ok(res.rows_affected() > 0),
                Err(sqlx::Error::Database(db_err)) if is_constraint_violation(db_err.as_ref()) => {
                    // Check if this is a short_id collision we can handle
                    let constraint_name = extract_constraint_name(db_err.as_ref());

                    if constraint_name.as_deref() == Some("short_id") {
                        retry_count += 1;

                        if retry_count > MAX_RETRIES {
                            warn!(
                                task_id = %task.id,
                                short_id = %task.short_id,
                                retry_count,
                                "Short ID collision retry limit exceeded after {MAX_RETRIES} attempts"
                            );
                            return Err(Error::Sqlx(sqlx::Error::Database(db_err)));
                        }

                        // Get the next character from the UUID hex string
                        let uuid_hex_chars: Vec<char> = task.id.chars().collect();
                        if let Some(next_char) = uuid_hex_chars.get(retry_count - 1) {
                            warn!(
                                task_id = %task.id,
                                short_id = %task.short_id,
                                retry_count,
                                "Short ID collision detected, extending with char '{next_char}'"
                            );
                            task.short_id.push(*next_char);
                        } else {
                            // Shouldn't happen with valid UUIDs, but handle gracefully
                            warn!(
                                task_id = %task.id,
                                short_id = %task.short_id,
                                retry_count,
                                "Short ID collision but UUID exhausted, cannot extend further"
                            );
                            return Err(Error::Sqlx(sqlx::Error::Database(db_err)));
                        }
                        // Continue to next loop iteration with extended short_id
                    } else {
                        // Other constraint violations (FK, etc.) should not be retried
                        warn!(
                            constraint = %db_err.message(),
                            task_id = %task.id,
                            "Non-retriable constraint violation during peer upsert"
                        );
                        return Err(Error::Sqlx(sqlx::Error::Database(db_err)));
                    }
                }
                Err(e) => return Err(Error::Sqlx(e)),
            }
        }
    }

    /// Reconciles tasks for a specific peer within a transaction.
    ///
    /// - Finds tasks where owner = peer_user_id
    /// - Skips already-closed tasks (terminal state protection - SYNC-11)
    /// - Skips tasks whose IDs are in peer_task_ids
    /// - Closes remaining tasks with close_reason = 'peer_reconciled'
    ///
    /// Returns the count of tasks that were reconciled (closed).
    pub async fn reconcile_peer_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        peer_user_id: &str,
        peer_task_ids: &[String],
    ) -> Result<usize> {
        // Safety guard: if peer export is empty, skip reconciliation
        if peer_task_ids.is_empty() {
            return Ok(0);
        }

        // Build placeholders for the NOT IN clause
        let placeholders: String = peer_task_ids
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");

        // Find tasks owned by peer that are not in their export and not already closed
        let sql_select = format!(
            "SELECT id FROM tasks WHERE owner = ? AND `status` != 'closed' AND id NOT IN ({})",
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

        // Close the tasks with peer_reconciled reason using SQLite's built-in timestamp.
        let placeholders: String = tasks_to_close
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");

        let sql_update = format!(
            "UPDATE tasks SET `status` = 'closed', close_reason = 'peer_reconciled',
             closed_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ') WHERE id IN ({})",
            placeholders
        );

        let mut update_query = sqlx::query(&sql_update);
        for id in &tasks_to_close {
            update_query = update_query.bind(id);
        }

        let result = update_query.execute(&mut **tx).await?;

        Ok(result.rows_affected() as usize)
    }
}
