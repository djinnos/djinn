use super::*;
use crate::roles::DispatchContext;
use crate::task_merge::{self, MergeActions};
use djinn_core::models::task::{IssueType, PRIORITY_CRITICAL};
use djinn_core::models::{TaskStatus, TransitionAction};
#[cfg(not(test))]
use djinn_db::AgentRepository;

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
    fn session_taxonomy_has_durable_artifacts(taxonomy: &serde_json::Value) -> bool {
        taxonomy
            .get("files_changed")
            .and_then(|v| v.as_u64())
            .is_some_and(|count| count > 0)
            || taxonomy
                .get("notes_written")
                .and_then(|v| v.as_u64())
                .is_some_and(|count| count > 0)
    }

    fn activity_entry_mentions_djinn_path(entry: &djinn_core::models::ActivityEntry) -> bool {
        if !entry.payload.contains(".djinn/") {
            return false;
        }

        serde_json::from_str::<serde_json::Value>(&entry.payload)
            .ok()
            .and_then(|payload| {
                payload
                    .get("body")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| entry.payload.clone())
            .contains(".djinn/")
    }

    /// Probe a session worktree on disk for uncommitted changes (modified,
    /// staged, or untracked files).  This is the ground-truth signal: it
    /// catches files written via `call_shell`, file moves, and anything the
    /// session-extraction taxonomy (which only counts `write|edit|apply_patch`
    /// tool calls) cannot see.
    ///
    /// Returns `true` if the path exists, opens as a git repo, and reports any
    /// non-clean entry.  Errors and missing paths conservatively return
    /// `false` so we never *promote* a task to the PR flow on a bogus signal —
    /// the in-DB signals (taxonomy, comments) are still consulted as a
    /// fallback.
    pub(super) fn worktree_has_uncommitted_changes(worktree_path: &std::path::Path) -> bool {
        if !worktree_path.exists() {
            return false;
        }
        let repo = match git2::Repository::open(worktree_path) {
            Ok(repo) => repo,
            Err(e) => {
                tracing::debug!(
                    path = %worktree_path.display(),
                    error = %e,
                    "worktree_has_uncommitted_changes: not a git repo"
                );
                return false;
            }
        };
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(true)
            .include_ignored(false)
            .recurse_untracked_dirs(true);
        match repo.statuses(Some(&mut opts)) {
            Ok(statuses) => statuses.iter().any(|entry| {
                let s = entry.status();
                s.intersects(
                    git2::Status::INDEX_NEW
                        | git2::Status::INDEX_MODIFIED
                        | git2::Status::INDEX_DELETED
                        | git2::Status::INDEX_RENAMED
                        | git2::Status::INDEX_TYPECHANGE
                        | git2::Status::WT_NEW
                        | git2::Status::WT_MODIFIED
                        | git2::Status::WT_DELETED
                        | git2::Status::WT_TYPECHANGE
                        | git2::Status::WT_RENAMED,
                )
            }),
            Err(e) => {
                tracing::debug!(
                    path = %worktree_path.display(),
                    error = %e,
                    "worktree_has_uncommitted_changes: status() failed"
                );
                false
            }
        }
    }

    pub(super) async fn simple_lifecycle_task_has_durable_artifacts(&self, task_id: &str) -> bool {
        let session_repo = SessionRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let task_run_repo =
            djinn_db::repositories::task_run::TaskRunRepository::new(self.db.clone());

        // Signal 1: real workspace git status (catches shell-driven changes
        // that the tool-call-based extraction in session_extraction.rs misses).
        // Post-refactor we read the workspace path from task_runs rather than
        // sessions (migration 5); task_run_id is NULL for stubbed supervisor
        // runs today, so this silently degrades to the taxonomy signal below.
        if let Ok(Some(workspace)) = task_run_repo.latest_workspace_path_for_task(task_id).await {
            let path = std::path::PathBuf::from(&workspace);
            if Self::worktree_has_uncommitted_changes(&path) {
                tracing::info!(
                    task_id = %task_id,
                    workspace = %workspace,
                    "simple-lifecycle artifact detected: workspace has uncommitted changes"
                );
                return true;
            }
        }

        // Signal 2: session event taxonomy (files_changed / notes_written).
        if let Ok(Some(taxonomy)) = session_repo.latest_event_taxonomy_for_task(task_id).await
            && Self::session_taxonomy_has_durable_artifacts(&taxonomy)
        {
            tracing::info!(
                task_id = %task_id,
                "simple-lifecycle artifact detected: non-zero files_changed/notes_written in taxonomy"
            );
            return true;
        }

        // Signal 3: task comments referencing .djinn/ paths.
        let repo = self.task_repo();
        if let Ok(entries) = repo.list_activity(task_id).await
            && entries
                .iter()
                .filter(|entry| entry.event_type == "comment")
                .any(Self::activity_entry_mentions_djinn_path)
        {
            tracing::info!(
                task_id = %task_id,
                "simple-lifecycle artifact detected: task comment references .djinn/ path"
            );
            return true;
        }

        false
    }

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

    /// Check whether the GitHub App is configured on this server.
    ///
    /// A configured App is the gate for dispatch because the merge path mints
    /// installation tokens via `djinn_provider::github_app`. Without it every
    /// completed task's PR creation would fail. In test builds we short-circuit
    /// to `true` so dispatch tests don't need to plumb env vars.
    async fn has_github_credentials(&self) -> bool {
        #[cfg(test)]
        {
            true
        }
        #[cfg(not(test))]
        {
            djinn_provider::github_app::app_id().is_ok()
        }
    }

    /// Find all ready tasks (open, no unresolved blockers, non-epic) and dispatch
    /// those that don't already have an active session.
    pub(super) async fn dispatch_ready_tasks(&mut self, project_filter: Option<&str>) {
        // Gate: do not dispatch if the GitHub App isn't configured (ADR-039).
        // PR creation depends on minting installation tokens, which requires
        // GITHUB_APP_ID + private key; without them every dispatch would
        // fail-retry at merge time.
        if !self.has_github_credentials().await {
            tracing::warn!(
                "CoordinatorActor: GitHub App not configured (GITHUB_APP_ID unset or \
                 private key missing), skipping dispatch. Configure the App env vars \
                 before starting execution."
            );
            return;
        }

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

        for status in ["needs_task_review", "needs_lead_intervention"] {
            match repo.list_by_status_filtered(status, true).await {
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

        // ADR-048 §3A: cancel any in-flight idle consolidation sweep when
        // tasks are ready for dispatch.
        if !ready.is_empty() {
            self.cancel_idle_consolidation();
        }

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
            let model_preference_ids = self
                .resolve_role_model_preference(&task.project_id, role)
                .await;
            let combined_models: Vec<String>;
            let model_ids: &[String] = if model_preference_ids.is_empty() {
                base_model_ids
            } else {
                // Prepend model_preference; deduplicate while preserving order.
                let mut seen = std::collections::HashSet::new();
                let mut merged =
                    Vec::with_capacity(model_preference_ids.len() + base_model_ids.len());
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

            // Phase 3 PR 8: architect-only pre-dispatch `await_fresh` gate.
            // Blocks up to 45s for a warm canonical graph; on timeout the
            // warmer returns Ok and the architect proceeds best-effort.
            // Other roles proceed immediately (per ADR: workers tolerate
            // a stale skeleton).
            if role == "architect"
                && let Some(warmer) = self.graph_warmer.clone()
            {
                let pid = task.project_id.clone();
                let _ = warmer
                    .await_fresh(
                        &pid,
                        std::time::Duration::from_secs(300),
                        std::time::Duration::from_secs(45),
                    )
                    .await;
            }

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

        /// Default stall timeout: 5 minutes for all agent types.
        const STALL_TIMEOUT_SECS: u64 = 5 * 60;
        /// Architect sessions get a longer timeout (10 minutes) because patrol
        /// reviews involve reading many files and epics sequentially.
        const ARCHITECT_STALL_TIMEOUT_SECS: u64 = 10 * 60;

        // Collect active task IDs so we can prune stall_killed entries for
        // sessions that have finished cleaning up.
        let active_task_ids: HashSet<String> =
            active.iter().filter_map(|s| s.task_id.clone()).collect();
        self.stall_killed.retain(|id| active_task_ids.contains(id));

        for session in active {
            let Some(task_id) = session.task_id.as_deref() else {
                continue;
            };

            // Skip sessions we've already killed — the DB record stays
            // `running` until the async lifecycle cleanup finishes.
            if self.stall_killed.contains(task_id) {
                continue;
            }

            // Use role-specific stall timeout: Architect gets 10 minutes.
            let stall_threshold = if session.agent_type == "architect" {
                ARCHITECT_STALL_TIMEOUT_SECS
            } else {
                STALL_TIMEOUT_SECS
            };

            // Query the activity tracker for idle time.  If the task has no
            // activity entry (e.g. session predates this feature, or reply loop
            // never started) fall back to wall-clock elapsed from started_at.
            let idle = match self.pool.session_for_task(task_id).await {
                Ok(Some(info)) => info.idle_seconds,
                _ => {
                    // Fallback: parse ISO-8601 started_at from the DB and compute
                    // elapsed seconds.  The column stores datetime strings like
                    // "2026-03-27 13:52:47" or "2026-03-27T13:52:47.231Z".
                    let Some(elapsed) = parse_iso_elapsed(&session.started_at) else {
                        tracing::warn!(
                            task_id = %task_id,
                            started_at = %session.started_at,
                            "CoordinatorActor: cannot parse started_at for stall check, skipping"
                        );
                        continue;
                    };
                    elapsed
                }
            };

            if idle <= stall_threshold {
                continue;
            }

            if let Err(e) = self.pool.kill_session(task_id).await {
                tracing::warn!(task_id = %task_id, session_id = %session.id, error = %e, "CoordinatorActor: failed to kill stalled session");
                continue;
            }

            // Mark as killed so we don't re-kill and re-log on subsequent ticks.
            self.stall_killed.insert(task_id.to_owned());

            let task_repo = self.task_repo();
            let payload = serde_json::json!({
                "message": format!(
                    "Coordinator stall timeout: {} session idle for {}s (threshold {}s). Session was cancelled for redispatch.",
                    session.agent_type, idle, stall_threshold
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
    /// For slot-based statuses (in_progress, in_task_review, in_lead_intervention),
    /// we check `has_session` in the slot pool.
    ///
    /// For "verifying", we check the shared `VerificationTracker` — if no
    /// background verification pipeline is registered for the task, it was
    /// orphaned (e.g. server restart) and gets released back to open.
    pub(super) async fn detect_and_recover_stuck_filtered(&mut self, project_filter: Option<&str>) {
        let repo = self.task_repo();
        let mut affected = 0u64;

        for status in ["in_progress", "in_task_review", "in_lead_intervention"] {
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
                    "in_lead_intervention" => (
                        TransitionAction::LeadInterventionRelease,
                        "needs_lead_intervention",
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
            #[allow(clippy::needless_return)]
            return Vec::new();
        }

        #[cfg(not(test))]
        {
            let role_repo = AgentRepository::new(
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

    /// Dispatch a Planner escalation: create a review task, add a comment linking it
    /// to the source task, then dispatch the Planner to it.
    ///
    /// Called when Lead calls `request_planner` or when auto-escalation fires on the
    /// 2nd `request_lead` for the same task.  Per ADR-051 §8 the Planner is now the
    /// escalation ceiling above Lead — it owns the board and decides whether to
    /// reshape, dedupe, or (if the issue requires deeper code-structural reasoning)
    /// dispatch an Architect spike.
    pub(super) async fn dispatch_planner_escalation(
        &mut self,
        source_task_id: &str,
        reason: &str,
        project_id: &str,
    ) {
        let model_ids = self.resolve_dispatch_models_for_role("planner").await;
        if model_ids.is_empty() {
            tracing::warn!(
                source_task_id = %source_task_id,
                "CoordinatorActor: planner escalation — no model configured for planner role"
            );
            return;
        }

        let Some(project_path) = self.project_path_for_id(project_id).await else {
            tracing::warn!(
                project_id = %project_id,
                source_task_id = %source_task_id,
                "CoordinatorActor: planner escalation — project path not found"
            );
            return;
        };

        let task_repo = TaskRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let title = format!("Planner escalation: {}", &reason[..reason.len().min(80)]);
        let description = format!(
            "Escalated from task {source_task_id}. Lead could not resolve — Planner review required.\n\nReason: {reason}"
        );
        let review_task = match task_repo
            .create_in_project(
                project_id,
                None,
                &title,
                &description,
                "Review the escalated task and either resolve it, reshape the work, or leave a 'Requires human review' comment.",
                "review",
                0,
                "system",
                Some("open"),
                None,
            )
            .await
        {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    project_id = %project_id,
                    source_task_id = %source_task_id,
                    "CoordinatorActor: planner escalation — failed to create review task"
                );
                return;
            }
        };

        // Log a comment on the source task linking to the planner review task.
        let comment_payload = serde_json::json!({
            "body": format!(
                "[PLANNER_ESCALATION] Escalated to Planner review task {}. Reason: {}",
                review_task.short_id, reason
            )
        })
        .to_string();
        let _ = task_repo
            .log_activity(
                Some(source_task_id),
                "coordinator",
                "system",
                "comment",
                &comment_payload,
            )
            .await;

        let task_id = review_task.id.clone();
        let project_path_owned = project_path.clone();
        let outcome = self
            .try_dispatch_to_pool(&review_task.short_id, &model_ids, |pool, model_id| {
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
                    review_task_id = %review_task.short_id,
                    review_task_uuid = %review_task.id,
                    source_task_id = %source_task_id,
                    project_id = %project_id,
                    "CoordinatorActor: Planner escalation dispatched"
                );
                self.last_dispatched
                    .insert(review_task.id.clone(), StdInstant::now());
                self.dispatched += 1;
                self.publish_status();
            }
            DispatchOutcome::AtCapacity => {
                tracing::debug!(
                    "CoordinatorActor: planner escalation — Planner model at capacity, will retry next cycle"
                );
            }
            DispatchOutcome::PoolDead => {
                tracing::error!("CoordinatorActor: planner escalation — slot pool actor dead");
            }
            DispatchOutcome::Failed => {
                tracing::debug!(
                    "CoordinatorActor: planner escalation — no model could accept Planner dispatch"
                );
            }
        }
    }

    /// Dispatch a Planner patrol session at a dynamic interval when:
    ///   - No Planner session is currently running.
    ///   - At least one project has dispatch enabled (not paused/unhealthy).
    ///   - The board has at least one open or in_progress task (skip empty boards).
    ///   - No open patrol review task already exists for that project.
    ///
    /// Per ADR-051 §1 the Planner owns the board patrol (previously Architect).
    /// The patrol interval is self-scheduled by the planner via the
    /// `next_patrol_minutes` field in `submit_grooming`. When no schedule exists,
    /// the default interval (DEFAULT_PLANNER_PATROL_INTERVAL) is used.
    ///
    /// Creates a "review" task for visibility, then dispatches the Planner.
    pub(super) async fn maybe_dispatch_planner_patrol(&mut self) {
        // Step 0: Check for the most recent patrol_schedule activity to update
        // the dynamic patrol interval.
        {
            let task_repo = self.task_repo();
            if let Some(minutes) = task_repo
                .query_activity(ActivityQuery {
                    event_type: Some("patrol_schedule".to_string()),
                    limit: 1,
                    ..Default::default()
                })
                .await
                .ok()
                .and_then(|a| a.into_iter().next())
                .and_then(|e| serde_json::from_str::<serde_json::Value>(&e.payload).ok())
                .and_then(|p| p.get("next_patrol_minutes").and_then(|v| v.as_u64()))
            {
                let minutes = (minutes as u32).clamp(
                    rules::MIN_PLANNER_PATROL_MINUTES,
                    rules::MAX_PLANNER_PATROL_MINUTES,
                );
                let new_interval = Duration::from_secs(u64::from(minutes) * 60);
                if new_interval != self.next_patrol_interval {
                    tracing::info!(
                        old_secs = self.next_patrol_interval.as_secs(),
                        new_secs = new_interval.as_secs(),
                        minutes,
                        "CoordinatorActor: patrol interval updated by planner"
                    );
                    self.next_patrol_interval = new_interval;
                }
            }
        }

        // Check if any Planner session is already running. Per ADR-051 §1 the
        // Planner owns patrol; a single active Planner (patrol, decomposition,
        // or intervention) is enough to suppress a new patrol dispatch.
        let session_repo = SessionRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let active_sessions = match session_repo.list_active().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: patrol — failed to list active sessions");
                return;
            }
        };
        let planner_running = active_sessions.iter().any(|s| s.agent_type == "planner");
        if planner_running {
            tracing::debug!("CoordinatorActor: patrol — Planner already running, skipping");
            return;
        }
        tracing::debug!(
            sessions = active_sessions.len(),
            "CoordinatorActor: patrol — no planner session running"
        );
        #[cfg(test)]
        eprintln!(
            "[patrol] step 1 passed: no planner session. Active sessions: {}",
            active_sessions.len()
        );

        // Find a dispatch-enabled project.  The patrol reviews the whole board,
        // so we only need at least one project that is actively running.
        let project_repo = ProjectRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let projects = match project_repo.list().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: patrol — failed to list projects");
                return;
            }
        };
        let Some(active_project) = projects.first() else {
            tracing::debug!("CoordinatorActor: patrol — no projects, skipping");
            return;
        };
        let project_id = active_project.id.clone();
        tracing::debug!(project_id = %project_id, "CoordinatorActor: patrol — using project");
        #[cfg(test)]
        eprintln!("[patrol] step 2: project dispatch enabled, project_id={project_id}");

        // Precondition: skip patrol if there are no non-closed tasks on the
        // board.  No point patrolling an empty board.
        let task_repo = TaskRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        {
            let has_active_work = {
                let mut found = false;
                // Check every non-closed status so the patrol fires whenever
                // there is any active work — not just open/in_progress.
                for status in [
                    "open",
                    "in_progress",
                    "verifying",
                    "needs_task_review",
                    "in_task_review",
                    "approved",
                    "pr_draft",
                    "pr_review",
                    "needs_lead_intervention",
                    "in_lead_intervention",
                ] {
                    let tasks = task_repo.list_by_status(status).await.unwrap_or_default();
                    // Exclude review-type tasks (patrol tasks themselves) from the count
                    // to avoid the patrol perpetually triggering because its own task exists.
                    if tasks.iter().any(|t| t.issue_type != "review") {
                        found = true;
                        break;
                    }
                }
                found
            };
            if !has_active_work {
                tracing::debug!("CoordinatorActor: patrol — no active tasks on board, skipping");
                #[cfg(test)]
                eprintln!("[patrol] skipping: empty board");
                return;
            }
        }

        // Guard: never create a patrol if one already exists in any non-terminal
        // state.  Query all review tasks for this project and check for any that
        // are not yet closed.  This prevents duplicates regardless of status
        // (open, in_progress, setting_up, verifying, etc.).
        {
            let all_reviews = task_repo
                .list_filtered(djinn_db::ListQuery {
                    project_id: Some(project_id.clone()),
                    status: None, // all statuses
                    issue_type: Some("review".to_string()),
                    priority: None,
                    label: None,
                    text: None,
                    parent: None,
                    sort: "created_desc".to_string(),
                    limit: 50,
                    offset: 0,
                })
                .await;
            if let Ok(result) = &all_reviews {
                let active_patrol = result
                    .tasks
                    .iter()
                    .find(|t| t.status != "closed" && t.title.contains("patrol"));
                if let Some(existing) = active_patrol {
                    tracing::debug!(
                        project_id = %project_id,
                        existing_task = %existing.short_id,
                        status = %existing.status,
                        "CoordinatorActor: patrol — non-closed patrol task exists, skipping"
                    );
                    #[cfg(test)]
                    eprintln!(
                        "[patrol] step 3: non-closed patrol task exists (status={}), skipping",
                        existing.status
                    );
                    return;
                }
            }
        }
        #[cfg(test)]
        eprintln!("[patrol] step 3: no existing non-closed patrol task");

        // Resolve models for the "planner" role.
        let model_ids = self.resolve_dispatch_models_for_role("planner").await;
        tracing::debug!(model_ids = ?model_ids, "CoordinatorActor: patrol — resolved models");
        #[cfg(test)]
        eprintln!("[patrol] step 4: resolved models: {:?}", model_ids);
        if model_ids.is_empty() {
            tracing::debug!("CoordinatorActor: patrol — no model configured for planner role");
            return;
        }

        // Create a review task for the patrol session.
        let review_task = match task_repo
            .create_in_project(
                &project_id,
                None,
                "Planner patrol: board health review",
                "Automated patrol session to review board health, epic progress, and approach viability.",
                "Review open epics and tasks for stuck work, missing blockers, and strategic issues.",
                "review",
                PRIORITY_CRITICAL,
                "system",
                Some("open"),
                None,
            )
            .await
        {
            Ok(t) => {
                #[cfg(test)]
                eprintln!("[patrol] step 5: review task created: {}", t.id);
                t
            }
            Err(e) => {
                #[cfg(test)]
                eprintln!("[patrol] step 5: FAILED to create review task: {e}");
                tracing::warn!(
                    error = %e,
                    project_id = %project_id,
                    "CoordinatorActor: patrol — failed to create review task"
                );
                return;
            }
        };

        let Some(project_path) = self.project_path_for_id(&project_id).await else {
            #[cfg(test)]
            eprintln!("[patrol] step 8: FAILED to get project path");
            tracing::warn!(
                project_id = %project_id,
                "CoordinatorActor: patrol — project path not found"
            );
            return;
        };
        #[cfg(test)]
        eprintln!("[patrol] step 8: project_path={project_path}");

        let task_id = review_task.id.clone();
        let project_path_owned = project_path.clone();
        let outcome = self
            .try_dispatch_to_pool(&review_task.short_id, &model_ids, |pool, model_id| {
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
                    task_id = %review_task.short_id,
                    task_uuid = %review_task.id,
                    project_id = %project_id,
                    "CoordinatorActor: Planner patrol dispatched"
                );
                self.last_dispatched
                    .insert(review_task.id.clone(), StdInstant::now());
                self.dispatched += 1;
                self.publish_status();
            }
            DispatchOutcome::AtCapacity => {
                tracing::debug!(
                    "CoordinatorActor: patrol — Planner model at capacity, will retry next cycle"
                );
            }
            DispatchOutcome::PoolDead => {
                tracing::error!("CoordinatorActor: patrol — slot pool actor dead");
            }
            DispatchOutcome::Failed => {
                tracing::debug!(
                    "CoordinatorActor: patrol — no model could accept Planner dispatch"
                );
            }
        }
    }

    /// Process tasks in `approved` status: create a GitHub PR (or fall back to
    /// direct squash-merge when no GitHub App credential is configured).
    ///
    /// Runs on each coordinator tick. This is a lightweight API-call path — no
    /// agent session is created.
    pub(super) async fn process_approved_tasks(&mut self) {
        let repo = self.task_repo();
        let tasks = match repo.list_by_status("approved").await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: list_by_status(approved) failed");
                return;
            }
        };

        if tasks.is_empty() {
            return;
        }

        // Build an AgentContext for the merge helpers (they need DB + event bus +
        // git actors).  This is the same construction used by the stale-sweep path
        // in the tick loop.
        let app_state = crate::context::AgentContext {
            db: self.db.clone(),
            event_bus: crate::events::event_bus_for(&self.events_tx),
            git_actors: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            verifying_tasks: self.verification_tracker.clone(),
            role_registry: self.role_registry.clone(),
            health_tracker: self.health.clone(),
            file_time: Arc::new(crate::file_time::FileTime::new()),
            lsp: self.lsp.clone(),
            catalog: self.catalog.clone(),
            coordinator: Arc::new(tokio::sync::Mutex::new(None)),
            active_tasks: crate::context::ActivityTracker::default(),
            task_ops_project_path_override: None,
            working_root: None,
            graph_warmer: None,
            repo_graph_ops: None,
            mirror: self.mirror.clone(),
            rpc_registry: None,
        };

        // Use Reopen as a sentinel for "leave in approved / retry next tick".
        // The approved → reopen transition is not valid in the state machine, so
        // we intercept it before calling `repo.transition` and simply skip.
        const SKIP_SENTINEL: TransitionAction = TransitionAction::Reopen;

        /// Transition actions for the coordinator-driven approved → PR path.
        const APPROVED_MERGE_ACTIONS: MergeActions = MergeActions {
            // Direct-merge success (no GitHub App): close the task.
            approve: TransitionAction::Close,
            // Merge conflict: reopen so the worker can rebase.
            conflict: TransitionAction::PrConflict,
            // Transient / infra failure: leave in approved (retry next tick).
            release: SKIP_SENTINEL,
            // No verification gate on this path.
            verification_fail: None,
            // PR creation auth/infra failure: leave in approved (retry next tick).
            pr_creation_fail: Some(SKIP_SENTINEL),
            // PR created successfully: transition approved → pr_draft.
            pr_created: Some(TransitionAction::PrCreated),
        };

        for task in tasks {

            // Simple-lifecycle tasks normally close directly, but sessions that
            // produced durable artifacts (file changes, memory writes, or task
            // comments pointing at .djinn paths) must survive as branch/PR
            // artifacts instead of being short-circuited here.
            let simple = IssueType::parse(&task.issue_type)
                .map(|it| it.uses_simple_lifecycle())
                .unwrap_or(false);
            if simple
                && !self
                    .simple_lifecycle_task_has_durable_artifacts(&task.id)
                    .await
            {
                tracing::info!(
                    task_id = %task.short_id,
                    issue_type = %task.issue_type,
                    "CoordinatorActor: simple-lifecycle task approved — closing directly"
                );
                if let Err(e) = repo
                    .transition(
                        &task.id,
                        TransitionAction::Close,
                        "coordinator",
                        "system",
                        Some("simple-lifecycle task — no PR needed"),
                        None,
                    )
                    .await
                {
                    tracing::warn!(
                        task_id = %task.short_id,
                        error = %e,
                        "CoordinatorActor: failed to close simple-lifecycle approved task"
                    );
                }
                continue;
            }

            if simple {
                tracing::info!(
                    task_id = %task.short_id,
                    issue_type = %task.issue_type,
                    "CoordinatorActor: simple-lifecycle task approved with durable artifacts — routing through PR flow"
                );
            }

            tracing::info!(
                task_id = %task.short_id,
                task_uuid = %task.id,
                project_id = %task.project_id,
                "CoordinatorActor: processing approved task for PR creation"
            );

            let result = task_merge::merge_and_transition(
                &task.id,
                &app_state,
                &APPROVED_MERGE_ACTIONS,
                None, // no verification gate
            )
            .await;

            match result {
                Some((action, reason)) if action == SKIP_SENTINEL => {
                    // Transient failure — leave in approved, retry next tick.
                    tracing::debug!(
                        task_id = %task.short_id,
                        "CoordinatorActor: approved task PR/merge deferred (will retry)"
                    );
                    // Surface PR creation errors so board_health can display them.
                    if let Some(ref err) = reason {
                        self.pr_errors.insert(task.project_id.clone(), err.clone());
                        self.publish_status();
                    }
                }
                Some((action, reason)) => {
                    // PR created successfully — clear any stored error.
                    self.pr_errors.remove(&task.project_id);
                    if let Err(e) = repo
                        .transition(
                            &task.id,
                            action.clone(),
                            "coordinator",
                            "system",
                            reason.as_deref(),
                            None,
                        )
                        .await
                    {
                        tracing::warn!(
                            task_id = %task.short_id,
                            action = ?action,
                            error = %e,
                            "CoordinatorActor: failed to transition approved task"
                        );
                    } else {
                        tracing::info!(
                            task_id = %task.short_id,
                            action = ?action,
                            "CoordinatorActor: approved task transitioned"
                        );
                    }
                }
                None => {
                    // merge_and_transition returned None — unexpected, log and skip.
                    tracing::warn!(
                        task_id = %task.short_id,
                        "CoordinatorActor: merge_and_transition returned None for approved task"
                    );
                }
            }
        }
    }
}

/// Parse an ISO-8601 datetime string from the DB (e.g. "2026-03-27T13:52:47.231Z"
/// or "2026-03-27 13:52:47") and return seconds elapsed since that time.
fn parse_iso_elapsed(started_at: &str) -> Option<u64> {
    use ::time::OffsetDateTime;
    use ::time::format_description::well_known::Iso8601;

    // Try ISO-8601 with offset first, then fall back to space-separated SQLite format.
    let parsed = OffsetDateTime::parse(started_at, &Iso8601::DEFAULT)
        .ok()
        .or_else(|| {
            // SQLite often stores "YYYY-MM-DD HH:MM:SS" without offset — assume UTC.
            let fmt =
                ::time::format_description::parse("[year]-[month]-[day] [hour]:[minute]:[second]")
                    .ok()?;
            let primitive = ::time::PrimitiveDateTime::parse(started_at, &fmt).ok()?;
            Some(primitive.assume_utc())
        })?;

    let now = OffsetDateTime::now_utc();
    let elapsed = (now - parsed).whole_seconds();
    Some(if elapsed < 0 { 0 } else { elapsed as u64 })
}
