use super::*;

impl CoordinatorActor {
    /// Find all ready tasks (open, no unresolved blockers, non-epic) and dispatch
    /// those that don't already have an active session.
    pub(super) async fn dispatch_ready_tasks(&mut self, project_filter: Option<&str>) {
        let mut role_models: HashMap<&'static str, Vec<String>> = HashMap::new();
        for role in ["worker", "task_reviewer", "pm"] {
            let model_ids = self.resolve_dispatch_models_for_role(role).await;
            if !model_ids.is_empty() {
                role_models.insert(role, model_ids);
            }
        }
        if role_models.is_empty() {
            tracing::warn!("CoordinatorActor: no configured model found, skipping dispatch");
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

        for status in ["needs_task_review", "needs_pm_intervention"] {
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

        // Expire stale cooldowns and old dispatch timestamps.
        self.dispatch_cooldowns
            .retain(|_, instant| instant.elapsed() < DISPATCH_COOLDOWN);
        self.last_dispatched
            .retain(|_, instant| instant.elapsed() < RAPID_FAILURE_THRESHOLD);

        for task in ready {
            if let Some(project_id) = project_filter
                && task.project_id != project_id
            {
                continue;
            }
            if !self.is_project_dispatch_enabled(&task.project_id) {
                continue;
            }
            // Detect rapid failure: if this task was dispatched very recently and
            // is already back as ready, it failed lifecycle early → add to cooldown.
            if let Some(last) = self.last_dispatched.get(&task.id)
                && last.elapsed() < RAPID_FAILURE_THRESHOLD
            {
                tracing::warn!(
                    task_id = %task.short_id,
                    elapsed_ms = last.elapsed().as_millis(),
                    cooldown_secs = DISPATCH_COOLDOWN.as_secs(),
                    "CoordinatorActor: rapid failure detected, adding dispatch cooldown"
                );
                self.dispatch_cooldowns
                    .insert(task.id.clone(), StdInstant::now());
            }
            // Skip tasks in cooldown (recently failed lifecycle setup).
            if self.dispatch_cooldowns.contains_key(&task.id) {
                tracing::debug!(
                    task_id = %task.short_id,
                    "CoordinatorActor: task in dispatch cooldown, skipping"
                );
                continue;
            }

            let role = Self::role_for_task_status(&task.status);
            if exhausted_roles.contains(role) {
                continue;
            }
            let Some(model_ids) = role_models.get(role) else {
                tracing::warn!(task_id = %task.short_id, role, "CoordinatorActor: no model configured for task role");
                continue;
            };

            match self.pool.has_session(&task.id).await {
                Ok(true) => continue, // session already active
                Ok(false) => {}
                Err(PoolError::ActorDead) => {
                    tracing::error!("CoordinatorActor: slot pool actor dead, aborting dispatch");
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

                match self.pool.dispatch(&task.id, &project_path, model_id).await {
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
                        self.last_dispatched
                            .insert(task.id.clone(), StdInstant::now());
                        self.dispatched += 1;
                        dispatched = true;
                        break;
                    }
                    Err(PoolError::AtCapacity { .. }) => {
                        role_at_capacity = true;
                        tracing::debug!(
                            task_id = %task.short_id,
                            model_id = %model_id,
                            "CoordinatorActor: model at capacity, trying next model"
                        );
                    }
                    Err(PoolError::ActorDead) => {
                        tracing::error!(
                            "CoordinatorActor: slot pool actor dead, aborting dispatch"
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
        self.publish_status();
    }

    /// On each tick: find tasks in active execution states with no active session
    /// and release them back to a dispatch-ready state (AGENT-08).
    ///
    /// For slot-based statuses (in_progress, in_task_review, in_pm_intervention),
    /// we check `has_session` in the slot pool.
    ///
    /// For "verifying", we check the shared `VerificationTracker` — if no
    /// background verification pipeline is registered for the task, it was
    /// orphaned (e.g. server restart) and gets released back to open.
    pub(super) async fn detect_and_recover_stuck_filtered(&mut self, project_filter: Option<&str>) {
        let repo = self.task_repo();

        let mut any_recovered = false;

        // ── Slot-based statuses: check has_session ──
        for (status, agent_type) in [
            ("in_progress", AgentType::Worker),
            ("in_task_review", AgentType::TaskReviewer),
            ("in_pm_intervention", AgentType::PM),
        ] {
            let action = agent_type.release_action();
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

                match self.pool.has_session(&task.id).await {
                    Ok(true) => continue, // healthy — session is active
                    Ok(false) => {}
                    Err(PoolError::ActorDead) => {
                        tracing::error!(
                            "CoordinatorActor: slot pool actor dead during stuck check"
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

        // ── Verifying: check verification tracker ──
        // Verification runs as a background tokio task (not in a slot).
        // After server restart the background task is gone but the task stays
        // in "verifying".  We release orphaned verifying tasks back to open.
        if let Ok(verifying_tasks) = repo.list_by_status("verifying").await {
            // Collect orphaned task info under the lock, then release it before async work.
            let orphans: Vec<_> = {
                let tracker = self.verification_tracker.lock().expect("poisoned");
                verifying_tasks
                    .into_iter()
                    .filter(|task| {
                        if let Some(project_id) = project_filter
                            && task.project_id != project_id
                        {
                            return false;
                        }
                        !tracker.contains(&task.id)
                    })
                    .collect()
            };

            for task in orphans {
                tracing::warn!(
                    task_id = %task.short_id,
                    task_uuid = %task.id,
                    project_id = %task.project_id,
                    "CoordinatorActor: orphaned verifying task (no pipeline) — releasing to open"
                );
                match repo
                    .transition(
                        &task.id,
                        crate::models::task::TransitionAction::ReleaseVerification,
                        "coordinator",
                        "system",
                        Some("orphaned verifying task — verification pipeline lost (server restart)"),
                        None,
                    )
                    .await
                {
                    Ok(_) => {
                        tracing::info!(
                            task_id = %task.short_id,
                            task_uuid = %task.id,
                            "CoordinatorActor: released orphaned verifying task"
                        );
                        self.recovered += 1;
                        any_recovered = true;
                    }
                    Err(e) => {
                        tracing::debug!(
                            task_id = %task.short_id,
                            error = %e,
                            "CoordinatorActor: verifying recovery transition failed"
                        );
                    }
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


impl CoordinatorActor {
    pub(super) fn mark_backlog_event(&mut self, project_id: &str) {
        self.backlog_debounce.insert(
            project_id.to_string(),
            Instant::now() + Duration::from_secs(2),
        );
    }

    pub(super) async fn backlog_count(&self, project_id: &str) -> usize {
        let repo = self.task_repo();
        match repo.list_by_status("backlog").await {
            Ok(tasks) => tasks.into_iter().filter(|t| t.project_id == project_id).count(),
            Err(e) => {
                tracing::warn!(project_id = %project_id, error = %e, "CoordinatorActor: failed to count backlog tasks");
                0
            }
        }
    }

    pub(super) async fn dispatch_groomer_for_project(&mut self, project_id: &str) -> bool {
        if self.active_groomer_sessions.contains(project_id) {
            tracing::debug!(project_id = %project_id, "Groomer dispatch: skipped — already active");
            return false;
        }
        if !self.is_project_dispatch_enabled(project_id) {
            tracing::debug!(project_id = %project_id, "Groomer dispatch: skipped — project dispatch disabled");
            return false;
        }
        let count = self.backlog_count(project_id).await;
        if count == 0 {
            tracing::debug!(project_id = %project_id, "Groomer dispatch: skipped — backlog empty");
            return false;
        }
        let role = AgentType::Groomer.dispatch_role();
        let model_ids = self.resolve_dispatch_models_for_role(role).await;
        if model_ids.is_empty() {
            tracing::warn!(project_id = %project_id, role, "Groomer dispatch: no models resolved for role");
            return false;
        }
        let Some(project_path) = self.project_path_for_id(project_id).await else {
            tracing::warn!(project_id = %project_id, "Groomer dispatch: project path not found");
            return false;
        };
        for model_id in model_ids {
            if !self.health.is_available(&model_id) {
                continue;
            }
            match self
                .pool
                .dispatch_project(project_id, &project_path, "groomer", &model_id)
                .await
            {
                Ok(()) => {
                    self.active_groomer_sessions.insert(project_id.to_string());
                    self.dispatched += 1;
                    return true;
                }
                Err(PoolError::AtCapacity { .. }) => continue,
                Err(_) => break,
            }
        }
        false
    }

    pub(super) async fn ensure_groomer_dispatch(&mut self, project_filter: Option<&str>) {
        let now = Instant::now();
        let due: Vec<String> = self
            .backlog_debounce
            .iter()
            .filter_map(|(project_id, when)| {
                if when <= &now && project_filter.is_none_or(|p| p == project_id) {
                    Some(project_id.clone())
                } else {
                    None
                }
            })
            .collect();
        for project_id in due {
            self.backlog_debounce.remove(&project_id);
            let _ = self.dispatch_groomer_for_project(&project_id).await;
        }

        let repo = self.task_repo();
        if let Ok(tasks) = repo.list_by_status("backlog").await {
            let mut projects = HashSet::new();
            for task in tasks {
                if project_filter.is_none_or(|p| p == task.project_id) {
                    projects.insert(task.project_id);
                }
            }
            for project_id in projects {
                if !self.active_groomer_sessions.contains(&project_id) {
                    let _ = self.dispatch_groomer_for_project(&project_id).await;
                }
            }
        }
    }
}
