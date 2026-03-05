use super::*;

impl CoordinatorActor {
    /// Find all ready tasks (open, no unresolved blockers, non-epic) and dispatch
    /// those that don't already have an active session.
    pub(super) async fn dispatch_ready_tasks(&mut self, project_filter: Option<&str>) {
        let mut role_models: HashMap<&'static str, Vec<String>> = HashMap::new();
        for role in ["worker", "task_reviewer", "epic_reviewer"] {
            let model_ids = self.resolve_dispatch_models_for_role(role).await;
            if !model_ids.is_empty() {
                role_models.insert(role, model_ids);
            }
        }
        if role_models.is_empty() {
            tracing::debug!("CoordinatorActor: no configured model found, skipping dispatch");
            return;
        }

        let repo = self.task_repo();
        let mut ready: Vec<crate::models::task::Task> = match repo
            .list_ready(ReadyQuery {
                issue_type: None,
                limit: self.dispatch_limit as i64,
                ..Default::default()
            })
            .await
        {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: list_ready failed");
                return;
            }
        };

        for status in ["needs_task_review"] {
            match repo.list_by_status(status).await {
                Ok(mut tasks) => ready.append(&mut tasks),
                Err(e) => {
                    tracing::warn!(error = %e, status, "CoordinatorActor: list_by_status failed");
                }
            }
        }

        let mut seen = HashSet::new();
        ready.retain(|t| seen.insert(t.id.clone()));
        ready.sort_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then_with(|| a.created_at.cmp(&b.created_at))
        });

        let mut exhausted_roles: HashSet<&'static str> = HashSet::new();

        for task in ready {
            if let Some(project_id) = project_filter
                && task.project_id != project_id
            {
                continue;
            }
            if !self.is_project_dispatch_enabled(&task.project_id) {
                continue;
            }

            let role = Self::role_for_task_status(&task.status);
            if exhausted_roles.contains(role) {
                continue;
            }
            let Some(model_ids) = role_models.get(role) else {
                tracing::debug!(task_id = %task.short_id, role, "CoordinatorActor: no model configured for task role");
                continue;
            };

            match self.supervisor.has_session(&task.id).await {
                Ok(true) => continue, // session already active
                Ok(false) => {}
                Err(SupervisorError::ActorDead) => {
                    tracing::error!("CoordinatorActor: supervisor actor dead, aborting dispatch");
                    return;
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        error = %e,
                        "CoordinatorActor: has_session query failed"
                    );
                    continue;
                }
            }

            let Some(project_path) = self.project_path_for_id(&task.project_id).await else {
                tracing::warn!(task_id = %task.short_id, project_id = %task.project_id, "CoordinatorActor: project path not found, skipping dispatch");
                continue;
            };

            let mut dispatched = false;
            let mut role_at_capacity = false;
            for model_id in model_ids {
                if !self.health.is_available(model_id) {
                    tracing::debug!(
                        model_id = %model_id,
                        task_id = %task.short_id,
                        "CoordinatorActor: model unavailable by health tracker"
                    );
                    continue;
                }

                match self
                    .supervisor
                    .dispatch(&task.id, &project_path, model_id)
                    .await
                {
                    Ok(()) => {
                        tracing::info!(
                            task_id = %task.short_id,
                            task_uuid = %task.id,
                            project_id = %task.project_id,
                            status = %task.status,
                            priority = task.priority,
                            role,
                            model_id = %model_id,
                            project_path,
                            "CoordinatorActor: task dispatched"
                        );
                        self.dispatched += 1;
                        dispatched = true;
                        break;
                    }
                    Err(SupervisorError::ModelAtCapacity { .. }) => {
                        role_at_capacity = true;
                        tracing::debug!(
                            task_id = %task.short_id,
                            model_id = %model_id,
                            "CoordinatorActor: model at capacity, trying next model"
                        );
                    }
                    Err(SupervisorError::ActorDead) => {
                        tracing::error!(
                            "CoordinatorActor: supervisor actor dead, aborting dispatch"
                        );
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(
                            task_id = %task.short_id,
                            model_id = %model_id,
                            error = %e,
                            "CoordinatorActor: dispatch failed"
                        );
                        break;
                    }
                }
            }

            if !dispatched {
                tracing::debug!(
                    task_id = %task.short_id,
                    task_uuid = %task.id,
                    project_id = %task.project_id,
                    role,
                    status = %task.status,
                    candidate_models = model_ids.len(),
                    role_at_capacity,
                    "CoordinatorActor: no model with available capacity for task"
                );
                if role_at_capacity {
                    exhausted_roles.insert(role);
                }
            }
        }

        let (batch_events_tx, _batch_events_rx) = broadcast::channel(1);
        let batch_repo = crate::db::repositories::epic_review_batch::EpicReviewBatchRepository::new(
            self.db.clone(),
            batch_events_tx,
        );
        let queued_anchors = match batch_repo
            .list_queued_anchors(project_filter, self.dispatch_limit as i64)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: list_queued_anchors failed");
                return;
            }
        };

        let Some(epic_models) = role_models.get("epic_reviewer") else {
            return;
        };

        for anchor in queued_anchors {
            if !self.is_project_dispatch_enabled(&anchor.project_id) {
                continue;
            }

            match self.supervisor.has_session(&anchor.task_id).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(SupervisorError::ActorDead) => {
                    tracing::error!("CoordinatorActor: supervisor actor dead, aborting dispatch");
                    return;
                }
                Err(e) => {
                    tracing::warn!(task_id = %anchor.task_id, error = %e, "CoordinatorActor: has_session failed for epic batch anchor");
                    continue;
                }
            }

            let Some(project_path) = self.project_path_for_id(&anchor.project_id).await else {
                continue;
            };

            for model_id in epic_models {
                if !self.health.is_available(model_id) {
                    continue;
                }

                match self
                    .supervisor
                    .dispatch(&anchor.task_id, &project_path, model_id)
                    .await
                {
                    Ok(()) => {
                        tracing::info!(batch_id = %anchor.batch_id, epic_id = %anchor.epic_id, task_id = %anchor.task_id, model_id = %model_id, "CoordinatorActor: epic review batch dispatched");
                        self.dispatched += 1;
                        break;
                    }
                    Err(SupervisorError::ModelAtCapacity { .. }) => continue,
                    Err(SupervisorError::ActorDead) => {
                        tracing::error!(
                            "CoordinatorActor: supervisor actor dead, aborting dispatch"
                        );
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(batch_id = %anchor.batch_id, task_id = %anchor.task_id, model_id = %model_id, error = %e, "CoordinatorActor: epic batch dispatch failed");
                        break;
                    }
                }
            }
        }
        self.publish_status();
    }

    /// On each tick: find tasks in active execution states with no active session
    /// and release them back to a dispatch-ready state (AGENT-08).
    pub(super) async fn detect_and_recover_stuck_filtered(&mut self, project_filter: Option<&str>) {
        let repo = self.task_repo();

        let mut any_recovered = false;
        for (status, action) in [
            ("in_progress", TransitionAction::Release),
            ("in_task_review", TransitionAction::ReleaseTaskReview),
        ] {
            let tasks = match repo.list_by_status(status).await {
                Ok(tasks) => tasks,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        status,
                        "CoordinatorActor: list_by_status failed during stuck check"
                    );
                    continue;
                }
            };

            for task in tasks {
                if let Some(project_id) = project_filter
                    && task.project_id != project_id
                {
                    continue;
                }

                match self.supervisor.has_session(&task.id).await {
                    Ok(true) => continue, // healthy — session is active
                    Ok(false) => {}
                    Err(SupervisorError::ActorDead) => {
                        tracing::error!(
                            "CoordinatorActor: supervisor actor dead during stuck check"
                        );
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(
                            task_id = %task.short_id,
                            status,
                            error = %e,
                            "CoordinatorActor: has_session failed during stuck check"
                        );
                        continue;
                    }
                }

                tracing::warn!(
                    task_id = %task.short_id,
                    task_uuid = %task.id,
                    project_id = %task.project_id,
                    status,
                    transition_action = ?action,
                    "CoordinatorActor: stuck task detected (no session) — releasing"
                );
                match repo
                    .transition(
                        &task.id,
                        action.clone(),
                        "coordinator",
                        "system",
                        Some("stuck task — no active session detected"),
                        None,
                    )
                    .await
                {
                    Ok(_) => {
                        tracing::info!(
                            task_id = %task.short_id,
                            task_uuid = %task.id,
                            project_id = %task.project_id,
                            status,
                            transition_action = ?action,
                            "CoordinatorActor: released stuck task"
                        );
                        self.recovered += 1;
                        any_recovered = true;
                    }
                    Err(e) => {
                        tracing::debug!(
                            task_id = %task.short_id,
                            status,
                            error = %e,
                            "CoordinatorActor: recovery transition failed (task may have already transitioned)"
                        );
                    }
                }
            }
        }

        let (batch_events_tx, _batch_events_rx) = broadcast::channel(1);
        let batch_repo = crate::db::repositories::epic_review_batch::EpicReviewBatchRepository::new(
            self.db.clone(),
            batch_events_tx,
        );
        let in_review = match batch_repo.list_in_review_anchors(project_filter).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: list_in_review_anchors failed during stuck check");
                Vec::new()
            }
        };

        for batch in in_review {
            if !self.is_project_dispatch_enabled(&batch.project_id) {
                continue;
            }
            match self.supervisor.has_session(&batch.task_id).await {
                Ok(true) => continue,
                Ok(false) => {
                    if let Err(e) = batch_repo
                        .requeue(
                            &batch.batch_id,
                            Some("stuck batch review — no active session detected"),
                        )
                        .await
                    {
                        tracing::warn!(batch_id = %batch.batch_id, error = %e, "CoordinatorActor: failed to requeue stuck epic review batch");
                    } else {
                        any_recovered = true;
                    }
                }
                Err(SupervisorError::ActorDead) => {
                    tracing::error!(
                        "CoordinatorActor: supervisor actor dead during batch stuck check"
                    );
                    return;
                }
                Err(e) => {
                    tracing::warn!(batch_id = %batch.batch_id, error = %e, "CoordinatorActor: has_session failed during batch stuck check");
                }
            }
        }

        // After releasing stuck tasks, immediately try to dispatch the now-open tasks.
        if any_recovered {
            self.dispatch_ready_tasks(None).await;
        }
        self.publish_status();
    }
}
