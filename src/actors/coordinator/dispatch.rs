use super::*;
use crate::agent::roles::DispatchContext;
use crate::models::{TaskStatus, TransitionAction};

/// Result of a single `try_dispatch_to_pool` attempt.
enum DispatchOutcome {
    /// Successfully dispatched to a slot.
    Dispatched,
    /// All candidate models are at capacity.
    AtCapacity,
    /// No healthy model could accept the dispatch (non-capacity errors).
    Failed,
    /// The slot pool actor is dead — caller should abort.
    PoolDead,
}

impl CoordinatorActor {
    /// Shared model-resolution → health-check → pool-dispatch loop used by
    /// both regular task dispatch and groomer dispatch.
    ///
    /// `dispatch_fn` receives `(&SlotPoolHandle, &str)` — the pool handle and
    /// model_id — and returns the pool dispatch future's result.
    async fn try_dispatch_to_pool<F, Fut>(
        &self,
        label: &str,
        model_ids: &[String],
        dispatch_fn: F,
    ) -> DispatchOutcome
    where
        F: Fn(&SlotPoolHandle, &str) -> Fut,
        Fut: std::future::Future<Output = Result<(), PoolError>>,
    {
        let mut any_at_capacity = false;

        for model_id in model_ids {
            if !self.health.is_available(model_id) {
                tracing::debug!(
                    model_id = %model_id,
                    label,
                    "CoordinatorActor: model unavailable by health tracker"
                );
                continue;
            }

            match dispatch_fn(&self.pool, model_id).await {
                Ok(()) => return DispatchOutcome::Dispatched,
                Err(PoolError::AtCapacity { .. }) => {
                    any_at_capacity = true;
                    tracing::debug!(
                        model_id = %model_id,
                        label,
                        "CoordinatorActor: model at capacity, trying next model"
                    );
                }
                Err(PoolError::ActorDead) => {
                    tracing::error!("CoordinatorActor: slot pool actor dead, aborting dispatch");
                    return DispatchOutcome::PoolDead;
                }
                Err(e) => {
                    tracing::warn!(
                        model_id = %model_id,
                        label,
                        error = %e,
                        "CoordinatorActor: dispatch failed"
                    );
                    return DispatchOutcome::Failed;
                }
            }
        }

        if any_at_capacity {
            DispatchOutcome::AtCapacity
        } else {
            DispatchOutcome::Failed
        }
    }

    /// Find all ready tasks (open, no unresolved blockers, non-epic) and dispatch
    /// those that don't already have an active session.
    pub(super) async fn dispatch_ready_tasks(&mut self, project_filter: Option<&str>) {
        let mut role_models: HashMap<&'static str, Vec<String>> = HashMap::new();
        for role in self.role_registry.model_pool_roles() {
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
        let mut ready: Vec<crate::models::Task> = match repo
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

            let ctx = DispatchContext::default();
            let Some(role) = self.role_registry.dispatch_role_for_task(&task, &ctx) else {
                continue;
            };
            if exhausted_roles.contains(role) {
                continue;
            }
            let Some(model_ids) = role_models.get(role) else {
                tracing::warn!(task_id = %task.short_id, role, "CoordinatorActor: no model configured for task role");
                continue;
            };

            match self.pool.has_session(&task.id).await {
                Ok(true) => continue,
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

            let task_id = task.id.clone();
            let project_path_owned = project_path.clone();
            let outcome = self
                .try_dispatch_to_pool(&task.short_id, model_ids, |pool, model_id| {
                    let pool = pool.clone();
                    let tid = task_id.clone();
                    let pp = project_path_owned.clone();
                    let mid = model_id.to_owned();
                    async move { pool.dispatch(&tid, &pp, &mid).await }
                })
                .await;

            match outcome {
                DispatchOutcome::Dispatched => {
                    tracing::info!(
                        task_id = %task.short_id,
                        task_uuid = %task.id,
                        project_id = %task.project_id,
                        status = %task.status,
                        priority = task.priority,
                        role,
                        project_path,
                        "CoordinatorActor: task dispatched"
                    );
                    self.last_dispatched
                        .insert(task.id.clone(), StdInstant::now());
                    self.dispatched += 1;
                }
                DispatchOutcome::AtCapacity => {
                    tracing::debug!(
                        task_id = %task.short_id,
                        task_uuid = %task.id,
                        project_id = %task.project_id,
                        role,
                        status = %task.status,
                        candidate_models = model_ids.len(),
                        "CoordinatorActor: all models at capacity for role"
                    );
                    exhausted_roles.insert(role);
                }
                DispatchOutcome::PoolDead => return,
                DispatchOutcome::Failed => {
                    tracing::debug!(
                        task_id = %task.short_id,
                        task_uuid = %task.id,
                        project_id = %task.project_id,
                        role,
                        status = %task.status,
                        candidate_models = model_ids.len(),
                        "CoordinatorActor: no model could accept dispatch"
                    );
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
        let mut affected = 0u64;

        for status in ["in_progress", "in_task_review", "in_pm_intervention"] {
            let tasks = match repo.list_by_status(status).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, status, "CoordinatorActor: list_by_status failed");
                    continue;
                }
            };

            for task in tasks {
                if let Some(project_id) = project_filter
                    && task.project_id != project_id
                {
                    continue;
                }
                if !self.is_project_dispatch_enabled(&task.project_id) {
                    continue;
                }

                let has_session = match self.pool.has_session(&task.id).await {
                    Ok(b) => b,
                    Err(PoolError::ActorDead) => {
                        tracing::error!(
                            "CoordinatorActor: slot pool actor dead, aborting stuck scan"
                        );
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(task_id = %task.short_id, error = %e, "CoordinatorActor: has_session query failed");
                        continue;
                    }
                };

                if has_session {
                    continue;
                }

                let release_to = match task.status.as_str() {
                    "in_task_review" => "needs_task_review",
                    "in_pm_intervention" => "needs_pm_intervention",
                    _ => "open",
                };

                match repo
                    .transition(
                        &task.id,
                        TransitionAction::Release,
                        "coordinator",
                        "system",
                        Some(&format!(
                            "Recovered by coordinator: no active slot session for {}",
                            task.status
                        )),
                        Some(TaskStatus::parse(release_to).expect("valid task status")),
                    )
                    .await
                {
                    Ok(_) => {
                        tracing::warn!(
                            task_id = %task.short_id,
                            from = %task.status,
                            to = release_to,
                            "CoordinatorActor: recovered stuck task"
                        );
                        affected += 1;
                    }
                    Err(e) => {
                        tracing::warn!(task_id = %task.short_id, error = %e, "CoordinatorActor: failed to recover stuck task")
                    }
                }
            }
        }

        let verifying = match repo.list_by_status("verifying").await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: list_by_status(verifying) failed");
                Vec::new()
            }
        };

        for task in verifying {
            if let Some(project_id) = project_filter
                && task.project_id != project_id
            {
                continue;
            }
            if !self.is_project_dispatch_enabled(&task.project_id) {
                continue;
            }

            let tracked = {
                let guard = self
                    .verification_tracker
                    .lock()
                    .expect("verification tracker mutex poisoned");
                guard.contains(&task.id)
            };
            if tracked {
                continue;
            }

            match repo
                .transition(
                    &task.id,
                    TransitionAction::ReleaseVerification,
                    "coordinator",
                    "system",
                    Some("Recovered by coordinator: no active verification pipeline"),
                    Some(TaskStatus::Open),
                )
                .await
            {
                Ok(_) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        from = "verifying",
                        to = "open",
                        "CoordinatorActor: recovered orphaned verifying task"
                    );
                    affected += 1;
                }
                Err(e) => {
                    tracing::warn!(task_id = %task.short_id, error = %e, "CoordinatorActor: failed to recover verifying task")
                }
            }
        }

        if affected > 0 {
            self.recovered += affected;
            self.publish_status();
            tracing::info!(
                affected,
                total_recovered = self.recovered,
                "CoordinatorActor: stuck-task recovery pass complete"
            );
        }
    }

    pub(crate) fn mark_backlog_event(&mut self, project_id: &str) {
        self.backlog_debounce.insert(
            project_id.to_owned(),
            Instant::now() + Duration::from_secs(2),
        );
    }

    pub(crate) async fn backlog_count(&self, project_id: &str) -> i64 {
        let repo = self.task_repo();
        match repo
            .list_ready(ReadyQuery {
                project_id: Some(project_id.to_owned()),
                issue_type: Some("task".to_string()),
                limit: 1,
                ..Default::default()
            })
            .await
        {
            Ok(tasks) => tasks.len() as i64,
            Err(e) => {
                tracing::warn!(project_id, error = %e, "CoordinatorActor: backlog_count list_ready failed");
                0
            }
        }
    }

    pub(crate) async fn dispatch_groomer_for_project(
        &mut self,
        project_id: &str,
    ) -> Result<(), ()> {
        if self.active_groomer_sessions.contains(project_id) {
            return Ok(());
        }
        if !self.is_project_dispatch_enabled(project_id) {
            return Ok(());
        }

        let Some(model_ids) = ({
            let ids = self.resolve_dispatch_models_for_role("pm").await;
            if ids.is_empty() { None } else { Some(ids) }
        }) else {
            return Err(());
        };

        let Some(project_path) = self.project_path_for_id(project_id).await else {
            return Err(());
        };

        let project_id_owned = project_id.to_owned();
        let project_path_owned = project_path.clone();
        let outcome = self
            .try_dispatch_to_pool("groomer", &model_ids, |pool, model_id| {
                let pool = pool.clone();
                let pid = project_id_owned.clone();
                let pp = project_path_owned.clone();
                let mid = model_id.to_owned();
                async move { pool.dispatch_project(&pid, &pp, "groomer", &mid).await }
            })
            .await;

        match outcome {
            DispatchOutcome::Dispatched => {
                self.active_groomer_sessions.insert(project_id.to_owned());
                Ok(())
            }
            DispatchOutcome::AtCapacity | DispatchOutcome::Failed | DispatchOutcome::PoolDead => {
                Err(())
            }
        }
    }

    pub(crate) async fn ensure_groomer_dispatch(&mut self, project_filter: Option<&str>) {
        let now = Instant::now();
        let due_projects: Vec<String> = self
            .backlog_debounce
            .iter()
            .filter_map(|(project_id, due)| {
                if *due <= now && project_filter.is_none_or(|f| f == project_id) {
                    Some(project_id.clone())
                } else {
                    None
                }
            })
            .collect();

        for project_id in due_projects {
            self.backlog_debounce.remove(&project_id);
            if self.backlog_count(&project_id).await > 0 {
                let _ = self.dispatch_groomer_for_project(&project_id).await;
            }
        }
    }

    pub(crate) async fn interrupt_groomers(&mut self, project_filter: Option<&str>) {
        let active: Vec<String> = self.active_groomer_sessions.iter().cloned().collect();
        for project_id in active {
            if let Some(filter) = project_filter
                && project_id != filter
            {
                continue;
            }

            match self
                .pool
                .interrupt_project(&project_id, "groomer interrupted by coordinator pause")
                .await
            {
                Ok(()) => {
                    self.active_groomer_sessions.remove(&project_id);
                }
                Err(PoolError::ActorDead) => {
                    tracing::error!(
                        "CoordinatorActor: slot pool actor dead while interrupting groomers"
                    );
                    return;
                }
                Err(e) => {
                    tracing::warn!(project_id = %project_id, error = %e, "CoordinatorActor: failed to interrupt groomer session");
                    self.active_groomer_sessions.remove(&project_id);
                }
            }
        }
    }
}
