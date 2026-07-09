use super::task_select_where_id;
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
        Self::validate_reason_requirement(&action, reason)?;

        self.db.ensure_initialized().await?;
        let reason_str = reason.unwrap_or("").to_owned();
        let id_owned = id.to_owned();
        let actor_id_owned = actor_id.to_owned();
        let actor_role_owned = actor_role.to_owned();
        let conflict_metadata_owned = conflict_metadata.map(|s| s.to_owned());

        // Dolt's commit-time MVCC check rejects overlapping writers on
        // `tasks` / `activity_log` / `blockers` with error 1213; the entire
        // transition is idempotent given the fresh `current` snapshot, so a
        // retry on a fresh transaction succeeds once the contender commits.
        let task: Task = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id_owned.clone();
                let actor_id = actor_id_owned.clone();
                let actor_role = actor_role_owned.clone();
                let reason_str = reason_str.clone();
                let conflict_metadata = conflict_metadata_owned.clone();
                let action = action.clone();
                let target_override = target_override.clone();
                async move {
                    let mut tx = self.db.pool().begin().await?;

                    let current: Task =
                        task_select_where_id!(&id).fetch_one(&mut *tx).await?;
                    let from = TaskStatus::parse(&current.status)?;

                    // Snapshot AC when reviewer starts so we can detect stale cycles later.
                    let ac_snapshot = if action == TransitionAction::TaskReviewStart {
                        Some(current.acceptance_criteria.clone())
                    } else {
                        None
                    };

                    let apply = compute_transition_for_issue_type(
                        &action,
                        &from,
                        target_override.as_ref(),
                        &current.issue_type,
                    )?;

                    Self::validate_close_blockers_guard(
                        &current, &apply, &action, &from, &id, &mut tx,
                    )
                    .await?;

                    Self::validate_start_guard(&current, &action, &id, &mut tx).await?;

                    let to_status = apply
                        .to_status
                        .as_ref()
                        .expect("all transitions must have a target status");
                    let to_str = to_status.as_str();
                    let from_str = from.as_str();

                    let effective_conflict_metadata = Self::resolve_conflict_metadata(
                        &apply, &conflict_metadata, &reason_str,
                    );
                    let conflict_meta_ref = effective_conflict_metadata.as_deref();

                    // Apply all side effects atomically.
                    let reopen_inc_val: i64 = if apply.increment_reopen { 1 } else { 0 };
                    sqlx::query!(
                        r#"UPDATE tasks SET
                            status = $1,
                            reopen_count = reopen_count + $2,
                            total_reopen_count = total_reopen_count + $3,
                            continuation_count = CASE WHEN $4 THEN 0 WHEN $5 THEN continuation_count + 1 ELSE continuation_count END,
                            intervention_count = CASE WHEN $6 THEN intervention_count + 1 ELSE intervention_count END,
                            last_intervention_at = CASE WHEN $7 THEN to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') ELSE last_intervention_at END,
                            closed_at = CASE
                                WHEN $8 THEN to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                                WHEN $9 THEN NULL
                                ELSE closed_at
                            END,
                            close_reason = CASE
                                WHEN $10::text IS NOT NULL THEN $11
                                WHEN $12 THEN NULL
                                ELSE close_reason
                            END,
                            merge_conflict_metadata = CASE
                                WHEN $13 THEN NULL
                                WHEN $14 THEN $15
                                ELSE merge_conflict_metadata
                            END,
                            updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                         WHERE id = $16"#,
                        to_str,
                        reopen_inc_val,
                        reopen_inc_val,
                        apply.reset_continuation,
                        apply.increment_continuation,
                        apply.record_intervention,
                        apply.record_intervention,
                        apply.set_closed_at,
                        apply.clear_closed_at,
                        apply.close_reason,
                        apply.close_reason,
                        apply.clear_close_reason,
                        apply.clear_merge_conflict_metadata,
                        apply.set_merge_conflict_metadata,
                        conflict_meta_ref,
                        id
                    )
                    .execute(&mut *tx)
                    .await?;

                    let payload =
                        Self::build_activity_payload(from_str, to_str, &reason_str, &ac_snapshot, apply.reopen_class);

                    let activity_id = uuid::Uuid::now_v7().to_string();
                    sqlx::query!(
                        "INSERT INTO activity_log
                            (id, task_id, actor_id, actor_role, event_type, payload)
                         VALUES ($1, $2, $3, $4, $5, $6)",
                        activity_id,
                        id,
                        actor_id,
                        actor_role,
                        apply.activity_type,
                        payload
                    )
                    .execute(&mut *tx)
                    .await?;

                    let task: Task = task_select_where_id!(&id).fetch_one(&mut *tx).await?;
                    tx.commit().await?;
                    Ok::<_, crate::Error>(task)
                }
            },
        )
        .await?;

        self.events
            .send(DjinnEventEnvelope::task_updated(&task, false));

        // Blocker resolution: when a task reaches post-merge/closed states, notify any tasks
        // it was blocking that are now fully unblocked, so coordinators can dispatch them.
        if task.status == "closed" {
            // uv3p Part C: closing a human-review hold stamps a consumed-hold
            // marker on the source task(s) it was holding BEFORE they are
            // revived, so the park evaluator that runs on the very next
            // dispatch tick discounts the pre-release strike ledger and cannot
            // spawn a duplicate hold on unchanged counters (ygj0).
            //
            // The same stamp applies when an autonomous `planner-park-escalation`
            // closes: the Planner resolving an arbiter-park escalation must
            // release the source exactly like a human closing a hold did, so
            // the post-release strike floor is set identically. Kept as literal
            // labels (djinn-db has no dependency on djinn-coordinator's
            // `releases_source_on_close`).
            if (task.labels.contains("human-review-hold")
                || task.labels.contains("planner-park-escalation"))
                && let Err(e) = self.mark_human_review_resolved(&task.id).await
            {
                tracing::warn!(
                    hold_task_id = %task.id,
                    error = %e,
                    "failed to stamp human_review_resolved_at on held source(s)"
                );
            }
            self.emit_unblocked_tasks(&task.id).await?;
        }

        Ok(task)
    }

    /// Raw status update, bypassing state machine validation.
    ///
    /// Only used for tests and admin tooling. Production code should use `transition`.
    pub async fn set_status(&self, id: &str, status: &str) -> Result<Task> {
        self.db.ensure_initialized().await?;
        let id_owned = id.to_owned();
        let status_owned = status.to_owned();

        let (from_task, task): (Task, Task) = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id_owned.clone();
                let status = status_owned.clone();
                async move {
                    let from_task: Task =
                        task_select_where_id!(id).fetch_one(self.db.pool()).await?;
                    let closed_at_sql = if status == "closed" {
                        r#"closed_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),"#
                    } else {
                        ""
                    };
                    // Only increment reopen_count on actual reopen transitions (not open->open)
                    let reopen_inc: i64 = if status == "open" && from_task.status != "open" {
                        1
                    } else {
                        0
                    };
                    // NOTE: dynamic SQL (closed_at fragment) — compile-time check not possible
                    sqlx::query(&format!(
                        r#"UPDATE tasks SET status = $1, {closed_at_sql}
                            reopen_count = reopen_count + $2,
                            total_reopen_count = total_reopen_count + $3,
                            updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                         WHERE id = $4"#
                    ))
                    .bind(&status)
                    .bind(reopen_inc)
                    .bind(reopen_inc)
                    .bind(&id)
                    .execute(self.db.pool())
                    .await?;
                    let task: Task =
                        task_select_where_id!(id).fetch_one(self.db.pool()).await?;
                    Ok::<_, crate::Error>((from_task, task))
                }
            },
        )
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
        let id_owned = id.to_owned();
        let status_owned = status.to_owned();
        let close_reason_owned = close_reason.map(|s| s.to_owned());

        let (from_task, task): (Task, Task) = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id_owned.clone();
                let status = status_owned.clone();
                let close_reason = close_reason_owned.clone();
                async move {
                    let from_task: Task =
                        task_select_where_id!(id).fetch_one(self.db.pool()).await?;

                    // Only increment reopen_count on actual reopen transitions (not open->open)
                    let reopen_inc: i64 = if status == "open" && from_task.status != "open" {
                        1
                    } else {
                        0
                    };
                    sqlx::query!(
                        r#"UPDATE tasks SET
                            status = $1,
                            closed_at = CASE
                                WHEN $2 = 'closed' THEN to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                                WHEN status = 'closed' THEN NULL
                                ELSE closed_at
                            END,
                            reopen_count = reopen_count + $3,
                            total_reopen_count = total_reopen_count + $4,
                            close_reason = CASE
                                WHEN $5 = 'closed' THEN $6
                                ELSE NULL
                            END,
                            updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                         WHERE id = $7"#,
                        status,
                        status,
                        reopen_inc,
                        reopen_inc,
                        status,
                        close_reason,
                        id
                    )
                    .execute(self.db.pool())
                    .await?;

                    let task: Task =
                        task_select_where_id!(id).fetch_one(self.db.pool()).await?;
                    Ok::<_, crate::Error>((from_task, task))
                }
            },
        )
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
        let id_owned = id.to_owned();
        let new_epic_id_owned = new_epic_id.map(|s| s.to_owned());

        let task: Task = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id_owned.clone();
                let new_epic_id = new_epic_id_owned.clone();
                async move {
                    sqlx::query!(
                        r#"UPDATE tasks SET epic_id = $1,
                            updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                         WHERE id = $2"#,
                        new_epic_id,
                        id
                    )
                    .execute(self.db.pool())
                    .await?;
                    let task: Task =
                        task_select_where_id!(id).fetch_one(self.db.pool()).await?;
                    Ok::<_, crate::Error>(task)
                }
            },
        )
        .await?;

        if let Some(epic_id) = new_epic_id {
            maybe_reopen_epic(&self.db, &self.events, epic_id).await?;
        }

        self.events
            .send(DjinnEventEnvelope::task_updated(&task, false));
        Ok(task)
    }

    // ── Private helpers for transition_with_conflict_metadata ─────────────────

    /// Pre-DB validation: reject transitions whose action requires a reason when none is provided.
    fn validate_reason_requirement(action: &TransitionAction, reason: Option<&str>) -> Result<()> {
        if action.requires_reason() && reason.map(str::is_empty).unwrap_or(true) {
            return Err(Error::InvalidTransition(format!(
                "{action:?} requires a non-empty reason"
            )));
        }
        Ok(())
    }

    /// Guard for pre-completion closes: reject if this task still blocks other
    /// non-closed tasks. Exemptions:
    /// - Closures from post-approval states (Approved, PrDraft, PrReview)
    /// - PrMerge and ForceClose actions
    /// - Simple-lifecycle tasks closed via Close (closing IS their completion)
    async fn validate_close_blockers_guard(
        current: &Task,
        apply: &djinn_core::models::TransitionApply,
        action: &TransitionAction,
        from: &TaskStatus,
        id: &str,
        tx: &mut sqlx::PgConnection,
    ) -> Result<()> {
        if !apply.set_closed_at {
            return Ok(());
        }

        let simple_lifecycle_close = IssueType::parse(&current.issue_type)
            .map(|it| it.uses_simple_lifecycle())
            .unwrap_or(false)
            && *action == TransitionAction::Close;
        let work_landed = matches!(
            from,
            TaskStatus::Approved | TaskStatus::PrDraft | TaskStatus::PrReview
        ) || *action == TransitionAction::PrMerge
            || simple_lifecycle_close
            || *action == TransitionAction::ForceClose
            // Arbiter supersede is a force-close of a decomposed task: its
            // downstream blockers are transferred to the replacement subtask by
            // the supersede transaction before this close, so the blocks-others
            // guard must not reject it (same exemption as ForceClose).
            || *action == TransitionAction::ArbiterSupersede;
        if work_landed {
            return Ok(());
        }

        let downstream = sqlx::query_as::<_, BlockerRef>(
            r#"SELECT t.id AS task_id, t.short_id, t.title, t.status
             FROM blockers b
             JOIN tasks t ON t.id = b.task_id
             WHERE b.blocking_task_id = $1
               AND t.status != 'closed'"#,
        )
        .bind(id)
        .fetch_all(tx)
        .await?;
        if downstream.is_empty() {
            return Ok(());
        }

        let names: Vec<String> = downstream
            .iter()
            .map(|b| format!("{} ({})", b.short_id, b.title))
            .collect();
        Err(Error::InvalidTransition(format!(
            "task blocks {} other task(s): {}. Remove or reassign these blockers before closing.",
            downstream.len(),
            names.join(", ")
        )))
    }

    /// Guard for Start: require acceptance criteria (for full-lifecycle types)
    /// and reject if any unresolved blockers exist.
    async fn validate_start_guard(
        current: &Task,
        action: &TransitionAction,
        id: &str,
        tx: &mut sqlx::PgConnection,
    ) -> Result<()> {
        if *action != TransitionAction::Start {
            return Ok(());
        }

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
            r#"SELECT COUNT(*) FROM blockers b
             JOIN tasks bt ON b.blocking_task_id = bt.id
             WHERE b.task_id = $1
               AND bt.status != 'closed'"#,
        )
        .bind(id)
        .fetch_one(tx)
        .await?;
        if count > 0 {
            return Err(Error::InvalidTransition(
                "task has unresolved blockers".into(),
            ));
        }

        Ok(())
    }

    /// Derive the effective conflict metadata JSON for the transition.
    ///
    /// Auto-extracts from reason (`"merge_conflict:{JSON}"`) when the transition
    /// sets the merge-conflict-metadata flag but no explicit metadata was provided.
    fn resolve_conflict_metadata(
        apply: &djinn_core::models::TransitionApply,
        conflict_metadata: &Option<String>,
        reason_str: &str,
    ) -> Option<String> {
        if !apply.set_merge_conflict_metadata {
            return None;
        }
        conflict_metadata.clone().or_else(|| {
            reason_str
                .strip_prefix("merge_conflict:")
                .map(|s| s.to_owned())
        })
    }

    /// Build the activity log payload JSON from transition state.
    fn build_activity_payload(
        from_str: &str,
        to_str: &str,
        reason_str: &str,
        ac_snapshot: &Option<String>,
        reopen_class: Option<djinn_core::models::ReopenClass>,
    ) -> serde_json::Value {
        let mut payload_obj = serde_json::json!({
            "from_status": from_str,
            "to_status": to_str,
            "reason": if reason_str.is_empty() { None } else { Some(reason_str) },
        });
        if let Some(ac) = ac_snapshot {
            let ac_value: serde_json::Value =
                serde_json::from_str(ac).unwrap_or(serde_json::json!([]));
            payload_obj["ac_snapshot"] = ac_value;
        }
        if let Some(class) = reopen_class {
            payload_obj["reopen_class"] = serde_json::json!(class.as_str());
        }
        payload_obj
    }
}
