use super::*;

impl TaskRepository {
    /// Transition a task through the state machine.
    ///
    /// Validates the action against the current status, applies all side effects
    /// (reopen_count, continuation_count, closed_at, blocked_from_status, close_reason),
    /// writes an activity_log entry, and emits a `TaskUpdated` event — all atomically.
    pub async fn transition(
        &self,
        id: &str,
        action: TransitionAction,
        actor_id: &str,
        actor_role: &str,
        reason: Option<&str>,
        target_override: Option<TaskStatus>,
    ) -> Result<Task> {
        // Check reason requirement before touching the DB.
        if action.requires_reason() && reason.map(str::is_empty).unwrap_or(true) {
            return Err(Error::InvalidTransition(format!(
                "{action:?} requires a non-empty reason"
            )));
        }

        self.db.ensure_initialized().await?;
        let reason_str = reason.unwrap_or("").to_owned();
        let mut tx = self.db.pool().begin().await?;

        // Load current task.
        let current: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        let from = TaskStatus::parse(&current.status)?;

        // Snapshot AC when reviewer starts so we can detect stale cycles later.
        let ac_snapshot = if action == TransitionAction::TaskReviewStart {
            Some(current.acceptance_criteria.clone())
        } else {
            None
        };

        // Validate and compute side effects.
        let apply = compute_transition(&action, &from, target_override.as_ref())?;

        // For Start: block if any unresolved blockers exist.
        // A blocker is unresolved if its blocking task has not reached post-merge states.
        if action == TransitionAction::Start {
            let ac = current.acceptance_criteria.trim();
            if ac.is_empty() || ac == "[]" {
                return Err(Error::InvalidTransition(
                    "task has no acceptance criteria".into(),
                ));
            }
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM blockers b
                 JOIN tasks bt ON b.blocking_task_id = bt.id
                 WHERE b.task_id = ?1
                   AND bt.status != 'closed'",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            if count > 0 {
                return Err(Error::InvalidTransition(
                    "task has unresolved blockers".into(),
                ));
            }
        }

        let to_status = apply
            .to_status
            .expect("all transitions must have a target status");
        let to_str = to_status.as_str();
        let from_str = from.as_str();

        // Apply all side effects atomically.
        sqlx::query(
            "UPDATE tasks SET
                status = ?2,
                reopen_count = reopen_count + ?3,
                continuation_count = CASE WHEN ?4 THEN 0 WHEN ?5 THEN continuation_count + 1 ELSE continuation_count END,
                verification_failure_count = CASE WHEN ?10 THEN 0 WHEN ?11 THEN verification_failure_count + 1 ELSE verification_failure_count END,
                closed_at = CASE
                    WHEN ?6 THEN strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                    WHEN ?7 THEN NULL
                    ELSE closed_at
                END,
                close_reason = CASE
                    WHEN ?8 IS NOT NULL THEN ?8
                    WHEN ?9 THEN NULL
                    ELSE close_reason
                END,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(id)
        .bind(to_str)
        .bind(if apply.increment_reopen { 1i64 } else { 0 })
        .bind(apply.reset_continuation)
        .bind(apply.increment_continuation)
        .bind(apply.set_closed_at)
        .bind(apply.clear_closed_at)
        .bind(apply.close_reason)
        .bind(apply.clear_close_reason)
        .bind(apply.reset_verification_failure)
        .bind(apply.increment_verification_failure)
        .execute(&mut *tx)
        .await?;

        // Append activity log entry.
        let activity_id = uuid::Uuid::now_v7().to_string();
        let mut payload_obj = serde_json::json!({
            "from_status": from_str,
            "to_status": to_str,
            "reason": if reason_str.is_empty() { None } else { Some(reason_str.as_str()) },
        });
        if let Some(ref ac) = ac_snapshot {
            let ac_value: serde_json::Value = serde_json::from_str(ac).unwrap_or(serde_json::json!([]));
            payload_obj["ac_snapshot"] = ac_value;
        }
        let payload = payload_obj.to_string();

        sqlx::query(
            "INSERT INTO activity_log
                (id, task_id, actor_id, actor_role, event_type, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(&activity_id)
        .bind(id)
        .bind(actor_id)
        .bind(actor_role)
        .bind(apply.activity_type)
        .bind(&payload)
        .execute(&mut *tx)
        .await?;

        let task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;

        let _ = self.events.send(DjinnEvent::TaskUpdated { task: task.clone(), from_sync: false });

        // Blocker resolution: when a task reaches post-merge/closed states, notify any tasks
        // it was blocking that are now fully unblocked, so coordinators can dispatch them.
        if task.status == "closed" {
            self.emit_unblocked_tasks(&task.id).await?;
        }

        Ok(task)
    }

    /// Raw status update, bypassing state machine validation.
    ///
    /// Only used for tests and admin tooling. Production code should use `transition`.
    pub async fn set_status(&self, id: &str, status: &str) -> Result<Task> {
        self.db.ensure_initialized().await?;
        let closed_at_sql = if status == "closed" {
            "closed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),"
        } else {
            ""
        };
        let reopen_inc: i64 = if status == "open" { 1 } else { 0 };
        sqlx::query(&format!(
            "UPDATE tasks SET status = ?2, {closed_at_sql}
                reopen_count = reopen_count + ?3,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1"
        ))
        .bind(id)
        .bind(status)
        .bind(reopen_inc)
        .execute(self.db.pool())
        .await?;
        let task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(self.db.pool())
            .await?;

        let _ = self.events.send(DjinnEvent::TaskUpdated { task: task.clone(), from_sync: false });
        Ok(task)
    }

    /// Move a task to a different epic (or detach from epic with None).
    pub async fn move_to_epic(&self, id: &str, new_epic_id: Option<&str>) -> Result<Task> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE tasks SET epic_id = ?2,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(id)
        .bind(new_epic_id)
        .execute(self.db.pool())
        .await?;
        let task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(self.db.pool())
            .await?;

        if let Some(epic_id) = new_epic_id {
            let epic_repo = EpicRepository::new(self.db.clone(), self.events.clone());
            if let Some(epic) = epic_repo.get(epic_id).await?
                && epic.status == "closed"
            {
                let _ = epic_repo.reopen(epic_id).await?;
            }
        }

        let _ = self.events.send(DjinnEvent::TaskUpdated { task: task.clone(), from_sync: false });
        Ok(task)
    }
}
