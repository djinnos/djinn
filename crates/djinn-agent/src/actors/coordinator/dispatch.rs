use super::*;
use crate::roles::DispatchContext;
use djinn_core::models::{TaskStatus, TransitionAction};
use djinn_db::AgentRoleRepository;

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
    /// both regular task dispatch and planner dispatch.
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
        let mut ready: Vec<djinn_core::models::Task> = match repo
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

            let ctx = DispatchContext;
            let Some(role) = self.role_registry.dispatch_role_for_task(&task, &ctx) else {
                continue;
            };
            if exhausted_roles.contains(role) {
                continue;
            }
            let Some(base_model_ids) = role_models.get(role) else {
                tracing::warn!(task_id = %task.short_id, role, "CoordinatorActor: no model configured for task role");
                continue;
            };

            // Look up the default DB role for this task's project + base_role.
            // If model_preference is set and resolves to a known model, prepend it
            // so the coordinator prefers it over the globally configured priorities.
            let model_preference_ids =
                self.resolve_role_model_preference(&task.project_id, role).await;
            let combined_models: Vec<String>;
            let model_ids: &[String] = if model_preference_ids.is_empty() {
                base_model_ids
            } else {
                // Prepend model_preference; deduplicate while preserving order.
                let mut seen = std::collections::HashSet::new();
                let mut merged = Vec::with_capacity(
                    model_preference_ids.len() + base_model_ids.len(),
                );
                for id in model_preference_ids.iter().chain(base_model_ids.iter()) {
                    if seen.insert(id.clone()) {
                        merged.push(id.clone());
                    }
                }
                combined_models = merged;
                &combined_models
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

    /// Kill any session that has been idle (no stream events or tool activity)
    /// for more than 5 minutes.  Unlike the old wall-clock timeout this applies
    /// to **all** agent types including workers — a session that stops producing
    /// tokens is stalled regardless of role.
    pub(super) async fn enforce_session_stall_timeout(&mut self) {
        let repo = djinn_db::SessionRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let active = match repo.list_active().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: failed to list active sessions for stall timeout");
                return;
            }
        };

        const STALL_TIMEOUT_SECS: u64 = 5 * 60;

        for session in active {
            let Some(task_id) = session.task_id.as_deref() else {
                continue;
            };

            // Query the activity tracker for idle time.  If the task has no
            // activity entry (e.g. session predates this feature) fall back to
            // wall-clock elapsed from started_at.
            let idle = match self.pool.session_for_task(task_id).await {
                Ok(Some(info)) => info.idle_seconds,
                _ => {
                    // Fallback: compute from started_at.
                    let Ok(started_secs) = session.started_at.parse::<u64>() else {
                        continue;
                    };
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    now.saturating_sub(started_secs)
                }
            };

            if idle <= STALL_TIMEOUT_SECS {
                continue;
            }

            if let Err(e) = self.pool.kill_session(task_id).await {
                tracing::warn!(task_id = %task_id, session_id = %session.id, error = %e, "CoordinatorActor: failed to kill stalled session");
                continue;
            }

            let task_repo = self.task_repo();
            let payload = serde_json::json!({
                "message": format!(
                    "Coordinator stall timeout: {} session idle for {}s (threshold {}s). Session was cancelled for redispatch.",
                    session.agent_type, idle, STALL_TIMEOUT_SECS
                )
            })
            .to_string();
            let _ = task_repo
                .log_activity(Some(task_id), "coordinator", "system", "comment", &payload)
                .await;

            tracing::warn!(
                task_id = %task_id,
                session_id = %session.id,
                agent_type = %session.agent_type,
                idle_seconds = idle,
                "CoordinatorActor: killed stalled session"
            );
        }
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

                // Non-worker roles free the slot immediately and run post-session
                // work (merge, transition) in a background task. The verification
                // tracker covers both verification pipelines AND post-session work.
                let has_background_work = {
                    let guard = self
                        .verification_tracker
                        .lock()
                        .expect("verification tracker mutex poisoned");
                    guard.contains(&task.id)
                };
                if has_background_work {
                    continue;
                }

                let (release_action, release_to) = match task.status.as_str() {
                    "in_task_review" => (TransitionAction::ReleaseTaskReview, "needs_task_review"),
                    "in_pm_intervention" => (
                        TransitionAction::PmInterventionRelease,
                        "needs_pm_intervention",
                    ),
                    _ => (TransitionAction::Release, "open"),
                };

                match repo
                    .transition(
                        &task.id,
                        release_action,
                        "coordinator",
                        "system",
                        Some(&format!(
                            "Recovered by coordinator: no active slot session for {}",
                            task.status
                        )),
                        None,
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
                        // Finalize any orphaned "running" session records for this
                        // task so they don't accumulate as ghost rows.
                        let session_repo = djinn_db::SessionRepository::new(
                            self.db.clone(),
                            crate::events::event_bus_for(&self.events_tx),
                        );
                        if let Err(e) = session_repo.interrupt_running_for_task(&task.id).await {
                            tracing::warn!(
                                task_id = %task.short_id,
                                error = %e,
                                "CoordinatorActor: failed to finalize orphaned sessions"
                            );
                        }
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

    /// Resolve a `provider/model` list for a DB role's `model_preference`.
    ///
    /// Looks up the default AgentRole for `(project_id, base_role)`.  If the
    /// role has a `model_preference` string, resolves it against connected
    /// providers (same logic as `resolve_dispatch_models_for_role`) and returns
    /// the matched model IDs.  Returns an empty Vec when:
    ///   - No default role is configured.
    ///   - No `model_preference` is set.
    ///   - The preference cannot be resolved to a connected model.
    ///   - In test builds (always returns empty to keep tests simple).
    async fn resolve_role_model_preference(
        &self,
        project_id: &str,
        base_role: &str,
    ) -> Vec<String> {
        #[cfg(test)]
        {
            let _ = (project_id, base_role);
            return Vec::new();
        }

        #[cfg(not(test))]
        {
            let role_repo = AgentRoleRepository::new(
                self.db.clone(),
                crate::events::event_bus_for(&self.events_tx),
            );
            let db_role = match role_repo
                .get_default_for_base_role(project_id, base_role)
                .await
            {
                Ok(Some(r)) => r,
                Ok(None) => return Vec::new(),
                Err(e) => {
                    tracing::warn!(
                        project_id,
                        base_role,
                        error = %e,
                        "CoordinatorActor: failed to load default role for model_preference"
                    );
                    return Vec::new();
                }
            };

            let preference = match db_role.model_preference.as_deref() {
                Some(p) if !p.trim().is_empty() => p.trim().to_string(),
                _ => return Vec::new(),
            };

            // Resolve `preference` (which may be a bare model name like
            // "claude-opus-4-6" or a full "provider/model" ID) against
            // connected credentials — same resolution path as model priorities.
            let cred_repo = djinn_provider::repos::CredentialRepository::new(
                self.db.clone(),
                crate::events::event_bus_for(&self.events_tx),
            );
            let credentials = match cred_repo.list().await {
                Ok(c) => c,
                Err(_) => return Vec::new(),
            };
            let credential_provider_ids = self.catalog.connected_provider_ids(&credentials);
            if credential_provider_ids.is_empty() {
                return Vec::new();
            }

            // Try to match the preference against every connected provider's model list.
            let mut resolved = Vec::new();
            for provider_id in &credential_provider_ids {
                for model in self.catalog.list_models(provider_id) {
                    let bare = model.id.rsplit('/').next().unwrap_or(&model.id);
                    let full_id = format!("{provider_id}/{}", model.id);
                    if model.id == preference
                        || model.name == preference
                        || bare == preference
                        || full_id == preference
                    {
                        resolved.push(full_id);
                        break;
                    }
                }
                if !resolved.is_empty() {
                    break;
                }
            }

            if !resolved.is_empty() {
                tracing::debug!(
                    project_id,
                    base_role,
                    preference,
                    resolved_model = %resolved[0],
                    "CoordinatorActor: resolved role model_preference"
                );
            }

            resolved
        }
    }

}
