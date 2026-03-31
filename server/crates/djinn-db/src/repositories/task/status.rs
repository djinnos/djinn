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
        self.transition_with_conflict_metadata(
            id,
            action,
            actor_id,
            actor_role,
            reason,
            target_override,
            None,
        )
        .await
    }

    /// Transition with optional conflict metadata.
    ///
    /// When `conflict_metadata` is provided and the transition sets the
    /// `set_merge_conflict_metadata` flag, the JSON is persisted on the task.
    #[allow(clippy::too_many_arguments)]
    pub async fn transition_with_conflict_metadata(
        &self,
        id: &str,
        action: TransitionAction,
        actor_id: &str,
        actor_role: &str,
        reason: Option<&str>,
        target_override: Option<TaskStatus>,
        conflict_metadata: Option<&str>,
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
        let apply = compute_transition_for_issue_type(
            &action,
            &from,
            target_override.as_ref(),
            &current.issue_type,
        )?;

        // For pre-completion closes: reject if this task still blocks other non-closed
        // tasks. The lead/architect must reassign blockers to replacement tasks before
        // closing incomplete work. Closures from post-approval states (Approved, PrDraft,
        // PrReview) and PrMerge are exempt because the work actually landed.
        // Simple-lifecycle tasks (spikes, research, planning, review) are also exempt
        // when closed via Close — closing IS their completion, they never go through
        // approval/merge states.
        let simple_lifecycle_close = IssueType::parse(&current.issue_type)
            .map(|it| it.uses_simple_lifecycle())
            .unwrap_or(false)
            && action == TransitionAction::Close;
        let work_landed = matches!(
            from,
            TaskStatus::Approved | TaskStatus::PrDraft | TaskStatus::PrReview
        ) || action == TransitionAction::PrMerge
            || simple_lifecycle_close
            || action == TransitionAction::ForceClose;
        if apply.set_closed_at && !work_landed {
            let downstream = sqlx::query_as::<_, BlockerRef>(
                "SELECT t.id AS task_id, t.short_id, t.title, t.status
                 FROM blockers b
                 JOIN tasks t ON t.id = b.task_id
                 WHERE b.blocking_task_id = ?1
                   AND t.status != 'closed'",
            )
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
            if !downstream.is_empty() {
                let names: Vec<String> = downstream
                    .iter()
                    .map(|b| format!("{} ({})", b.short_id, b.title))
                    .collect();
                return Err(Error::InvalidTransition(format!(
                    "task blocks {} other task(s): {}. Remove or reassign these blockers before closing.",
                    downstream.len(),
                    names.join(", ")
                )));
            }
        }

        // For Start: block if any unresolved blockers exist.
        // A blocker is unresolved if its blocking task has not reached post-merge states.
        if action == TransitionAction::Start {
            let uses_simple = IssueType::parse(&current.issue_type)
                .map(|it| it.uses_simple_lifecycle())
                .unwrap_or(false);
            if !uses_simple {
                let ac = current.acceptance_criteria.trim();
                if ac.is_empty() || ac == "[]" {
                    return Err(Error::InvalidTransition(
                        "task has no acceptance criteria".into(),
                    ));
                }
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

        // Auto-extract conflict metadata from reason when the transition sets the flag
        // but no explicit metadata was provided. The reason has the format:
        // "merge_conflict:{JSON}" — extract the JSON portion.
        let effective_conflict_metadata: Option<String> = if apply.set_merge_conflict_metadata {
            conflict_metadata.map(|s| s.to_owned()).or_else(|| {
                reason_str
                    .strip_prefix("merge_conflict:")
                    .map(|s| s.to_owned())
            })
        } else {
            None
        };
        let conflict_meta_ref = effective_conflict_metadata.as_deref();

        // Apply all side effects atomically.
        sqlx::query(
            "UPDATE tasks SET
                status = ?2,
                reopen_count = reopen_count + ?3,
                total_reopen_count = total_reopen_count + ?3,
                continuation_count = CASE WHEN ?4 THEN 0 WHEN ?5 THEN continuation_count + 1 ELSE continuation_count END,
                verification_failure_count = CASE WHEN ?10 THEN 0 WHEN ?11 THEN verification_failure_count + 1 ELSE verification_failure_count END,
                total_verification_failure_count = total_verification_failure_count + CASE WHEN ?11 THEN 1 ELSE 0 END,
                intervention_count = CASE WHEN ?15 THEN intervention_count + 1 ELSE intervention_count END,
                last_intervention_at = CASE WHEN ?15 THEN strftime('%Y-%m-%dT%H:%M:%fZ', 'now') ELSE last_intervention_at END,
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
                merge_conflict_metadata = CASE
                    WHEN ?12 THEN NULL
                    WHEN ?13 THEN ?14
                    ELSE merge_conflict_metadata
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
        .bind(apply.clear_merge_conflict_metadata)
        .bind(apply.set_merge_conflict_metadata)
        .bind(conflict_meta_ref)
        .bind(apply.record_intervention)
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
            let ac_value: serde_json::Value =
                serde_json::from_str(ac).unwrap_or(serde_json::json!([]));
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

        self.events
            .send(DjinnEventEnvelope::task_updated(&task, false));

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
        let from_task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(self.db.pool())
            .await?;
        let closed_at_sql = if status == "closed" {
            "closed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),"
        } else {
            ""
        };
        // Only increment reopen_count on actual reopen transitions (not open->open)
        let reopen_inc: i64 = if status == "open" && from_task.status != "open" {
            1
        } else {
            0
        };
        sqlx::query(&format!(
            "UPDATE tasks SET status = ?2, {closed_at_sql}
                reopen_count = reopen_count + ?3,
                total_reopen_count = total_reopen_count + ?3,
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

        let status_payload = serde_json::json!({
            "from_status": from_task.status,
            "to_status": status,
            "reopen_count": task.reopen_count,
        })
        .to_string();
        let _ = self
            .log_activity(
                Some(id),
                "coordinator",
                "system",
                "status_changed",
                &status_payload,
            )
            .await;

        self.events
            .send(DjinnEventEnvelope::task_updated(&task, false));
        Ok(task)
    }

    /// Raw status update with explicit close reason.
    ///
    /// This helper is intentionally narrow so tests can represent custom
    /// terminal outcomes such as `close_reason = "failed"` without broad
    /// state-machine refactors.
    pub async fn set_status_with_reason(
        &self,
        id: &str,
        status: &str,
        close_reason: Option<&str>,
    ) -> Result<Task> {
        self.db.ensure_initialized().await?;

        let from_task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(self.db.pool())
            .await?;

        // Only increment reopen_count on actual reopen transitions (not open->open)
        let reopen_inc: i64 = if status == "open" && from_task.status != "open" {
            1
        } else {
            0
        };
        sqlx::query(
            "UPDATE tasks SET
                status = ?2,
                closed_at = CASE
                    WHEN ?2 = 'closed' THEN strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                    WHEN status = 'closed' THEN NULL
                    ELSE closed_at
                END,
                reopen_count = reopen_count + ?3,
                total_reopen_count = total_reopen_count + ?3,
                close_reason = CASE
                    WHEN ?2 = 'closed' THEN ?4
                    ELSE NULL
                END,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(id)
        .bind(status)
        .bind(reopen_inc)
        .bind(close_reason)
        .execute(self.db.pool())
        .await?;

        let task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(self.db.pool())
            .await?;

        let status_payload = serde_json::json!({
            "from_status": from_task.status,
            "to_status": status,
            "reason": close_reason,
            "reopen_count": task.reopen_count,
        })
        .to_string();
        let _ = self
            .log_activity(
                Some(id),
                "coordinator",
                "system",
                "status_changed",
                &status_payload,
            )
            .await;

        self.events
            .send(DjinnEventEnvelope::task_updated(&task, false));
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
            maybe_reopen_epic(&self.db, &self.events, epic_id).await?;
        }

        self.events
            .send(DjinnEventEnvelope::task_updated(&task, false));
        Ok(task)
    }
}
