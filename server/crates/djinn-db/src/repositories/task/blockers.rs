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

        let task_id_owned = task_id.to_owned();
        let blocking_id_owned = blocking_id.to_owned();

        // Retry on Dolt 1213: concurrent edits to `blockers` (plus the CTE
        // walk over `blockers`) are the exact overlap that trips MVCC
        // serialization failures; the transaction is idempotent modulo the
        // duplicate-INSERT branch which is already handled below.
        crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let task_id = task_id_owned.clone();
                let blocking_id = blocking_id_owned.clone();
                async move {
                    let mut tx = self.db.pool().begin().await?;
                    // Cycle detection: check if task_id already (transitively) blocks blocking_id.
                    // If so, adding "blocking_id blocks task_id" would form a cycle.
                    let would_cycle = sqlx::query_scalar!(
                        r#"WITH RECURSIVE reach(id) AS (
                             SELECT task_id FROM blockers WHERE blocking_task_id = $1
                             UNION
                             SELECT b.task_id FROM blockers b JOIN reach r ON b.blocking_task_id = r.id
                         )
                         SELECT EXISTS(SELECT 1 FROM reach WHERE id = $2) AS "exists!: bool""#,
                        task_id,
                        blocking_id
                    )
                    .fetch_one(&mut *tx)
                    .await?;
                    if would_cycle {
                        return Err(Error::Internal(
                            "would create circular blocker dependency".into(),
                        ));
                    }
                    let result = sqlx::query!(
                        "INSERT INTO blockers (task_id, blocking_task_id) VALUES ($1, $2)",
                        task_id,
                        blocking_id
                    )
                    .execute(&mut *tx)
                    .await;
                    match result {
                        Ok(_) => {}
                        // Duplicate blocker — idempotent, silently skip. Postgres
                        // phrases this as `duplicate key value violates unique
                        // constraint`; use sqlx's portable `is_unique_violation`
                        // rather than matching the (SQLite-only) message text.
                        Err(sqlx::Error::Database(ref e)) if e.is_unique_violation() => {}
                        Err(e) => {
                            return Err(Error::Internal(format!(
                                "failed to add blocker {blocking_id} → {task_id}: {e}"
                            )));
                        }
                    }
                    tx.commit().await?;
                    Ok::<_, crate::Error>(())
                }
            },
        )
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

    pub async fn remove_blocker(&self, task_id: &str, blocking_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        let task_id_owned = task_id.to_owned();
        let blocking_id_owned = blocking_id.to_owned();

        // DELETE is idempotent; retry on Dolt 1213.
        crate::retry::retry_on_serialization_failure(crate::retry::DEFAULT_MAX_TX_RETRIES, || {
            let task_id = task_id_owned.clone();
            let blocking_id = blocking_id_owned.clone();
            async move {
                sqlx::query!(
                    "DELETE FROM blockers WHERE task_id = $1 AND blocking_task_id = $2",
                    task_id,
                    blocking_id
                )
                .execute(self.db.pool())
                .await?;
                Ok::<_, crate::Error>(())
            }
        })
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

        // Retry on Dolt 1213. `add` / `remove` are borrowed slices; reborrow
        // them inside the closure so we avoid cloning the whole Vec on
        // every attempt — only the short task_id is cloned.
        let task_id_owned = task_id.to_owned();
        crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let task_id = task_id_owned.clone();
                async move {
                    let mut tx = self.db.pool().begin().await?;

                    // Removals first (so adds can reference the freed edges if needed).
                    for blocking_id in remove {
                        sqlx::query!(
                            "DELETE FROM blockers WHERE task_id = $1 AND blocking_task_id = $2",
                            task_id,
                            blocking_id
                        )
                        .execute(&mut *tx)
                        .await?;
                    }

                    // Additions with cycle detection.
                    for blocking_id in add {
                        if &task_id == blocking_id {
                            return Err(Error::Internal("task cannot block itself".into()));
                        }
                        let would_cycle = sqlx::query_scalar!(
                            r#"WITH RECURSIVE reach(id) AS (
                                 SELECT task_id FROM blockers WHERE blocking_task_id = $1
                                 UNION
                                 SELECT b.task_id FROM blockers b JOIN reach r ON b.blocking_task_id = r.id
                             )
                             SELECT EXISTS(SELECT 1 FROM reach WHERE id = $2) AS "exists!: bool""#,
                            task_id,
                            blocking_id
                        )
                        .fetch_one(&mut *tx)
                        .await?;
                        if would_cycle {
                            return Err(Error::Internal(
                                "would create circular blocker dependency".into(),
                            ));
                        }
                        let result = sqlx::query!(
                            "INSERT INTO blockers (task_id, blocking_task_id) VALUES ($1, $2)",
                            task_id,
                            blocking_id
                        )
                        .execute(&mut *tx)
                        .await;
                        match result {
                            Ok(_) => {}
                            // Duplicate blocker — idempotent, silently skip.
                            // Postgres phrases this as `duplicate key value
                            // violates unique constraint`; use sqlx's portable
                            // `is_unique_violation` rather than the SQLite text.
                            Err(sqlx::Error::Database(ref e)) if e.is_unique_violation() => {}
                            Err(e) => {
                                return Err(Error::Internal(format!(
                                    "failed to add blocker {blocking_id} → {task_id}: {e}"
                                )));
                            }
                        }
                    }

                    tx.commit().await?;
                    Ok::<_, crate::Error>(())
                }
            },
        )
        .await?;

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
        Ok(sqlx::query_as!(
            BlockerRef,
            r#"SELECT t.id AS task_id, t.short_id, t.title, t.status
             FROM blockers b
             JOIN tasks t ON t.id = b.blocking_task_id
             WHERE b.task_id = $1
             ORDER BY t.created_at"#,
            task_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// List tasks that are blocked BY `blocking_task_id`.
    pub async fn list_blocked_by(&self, blocking_task_id: &str) -> Result<Vec<BlockerRef>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            BlockerRef,
            r#"SELECT t.id AS task_id, t.short_id, t.title, t.status
             FROM blockers b
             JOIN tasks t ON t.id = b.task_id
             WHERE b.blocking_task_id = $1
             ORDER BY t.created_at"#,
            blocking_task_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Emit `TaskUpdated` for tasks that were blocked by `completed_task_id` and
    /// are now fully unblocked (all blockers are in resolved post-merge/closed states).
    pub(super) async fn emit_unblocked_tasks(&self, completed_task_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        // NOTE: Task has #[sqlx(default)] fields that a macro-typed query cannot satisfy without repeating the SELECT with NULLs; keep runtime.
        let unblocked = sqlx::query_as::<_, Task>(
            "SELECT t.id, t.project_id, t.short_id, t.epic_id, t.title, t.description, t.design,
                    t.issue_type, t.status, t.priority, t.owner, t.labels::text AS labels,
                    t.acceptance_criteria::text AS acceptance_criteria, t.reopen_count, t.continuation_count,
                    t.total_reopen_count,
                    t.intervention_count, t.last_intervention_at,
                    t.created_at, t.updated_at, t.closed_at,
                    t.close_reason, t.merge_commit_sha, t.pr_url, t.merge_conflict_metadata, t.memory_refs::text AS memory_refs,
                    t.agent_type, t.created_by_user_id,
                    t.refinement_run_id, t.refinement_intent_id, t.refinement_generation,
                    t.refinement_round, t.refinement_phase, t.refinement_role,
                    COALESCE((SELECT s.ci_status FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1), 'unknown') AS ci_status,
                    (SELECT s.head_sha FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_head_sha,
                    (SELECT s.pr_number FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_pr_number,
                    COALESCE((SELECT s.blocking_required_check_names::text FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1), '[]') AS ci_blocking_required_check_names,
                    (SELECT s.failure_fingerprint FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_failure_fingerprint,
                    (SELECT s.first_seen_at FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_first_seen_at,
                    (SELECT s.last_seen_at FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_last_seen_at,
                    COALESCE((SELECT s.same_signature_count FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1), 0) AS ci_same_signature_count,
                    (SELECT s.last_remediation_base_sha FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_last_remediation_base_sha,
                    (SELECT s.mq_state FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_state,
                    (SELECT s.mq_run_id FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_run_id,
                    (SELECT s.mq_head_sha FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_head_sha,
                    (SELECT s.mq_failed_check_names::text FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_failed_check_names,
                    (SELECT s.mq_failure_fingerprint FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_failure_fingerprint,
                    (SELECT s.mq_same_signature_count FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_same_signature_count,
                    (SELECT s.mq_first_seen_at FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_first_seen_at,
                    (SELECT s.mq_last_seen_at FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_last_seen_at,
                    (SELECT ta.mirror_head_sha FROM task_attempts ta WHERE ta.task_id = t.id AND (ta.mirror_head_sha IS NOT NULL OR ta.github_head_sha IS NOT NULL OR ta.github_publication_error IS NOT NULL) ORDER BY ta.created_at DESC LIMIT 1) AS ci_mirror_head_sha,
                    (SELECT ta.github_head_sha FROM task_attempts ta WHERE ta.task_id = t.id AND (ta.mirror_head_sha IS NOT NULL OR ta.github_head_sha IS NOT NULL OR ta.github_publication_error IS NOT NULL) ORDER BY ta.created_at DESC LIMIT 1) AS ci_github_head_sha,
                    (SELECT CASE WHEN ta.mirror_head_sha IS NOT NULL AND ta.github_head_sha IS NOT NULL THEN ta.mirror_head_sha != ta.github_head_sha END FROM task_attempts ta WHERE ta.task_id = t.id AND (ta.mirror_head_sha IS NOT NULL OR ta.github_head_sha IS NOT NULL OR ta.github_publication_error IS NOT NULL) ORDER BY ta.created_at DESC LIMIT 1) AS ci_heads_diverged,
                    (SELECT ta.github_publication_error FROM task_attempts ta WHERE ta.task_id = t.id AND ta.github_publication_error IS NOT NULL ORDER BY ta.created_at DESC LIMIT 1) AS ci_head_observation_error
             FROM blockers b
             JOIN tasks t ON t.id = b.task_id
             WHERE b.blocking_task_id = $1
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
