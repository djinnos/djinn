use super::*;

impl AgentSupervisor {
    pub(super) async fn kill_session(&mut self, task_id: String) -> Result<(), SupervisorError> {
        let Some(mut handle) = self.sessions.remove(&task_id) else {
            return Ok(());
        };

        self.interrupted_sessions.insert(task_id.clone());
        let model_id = self.session_models.remove(&task_id);
        let agent_type = self
            .session_agent_types
            .remove(&task_id)
            .unwrap_or(AgentType::Worker);
        self.session_projects.remove(&task_id);
        let session_record_id = self.task_session_records.remove(&task_id);
        let goose_session_id = handle.session_id.clone();

        handle.cancel.cancel();

        match tokio::time::timeout(Duration::from_secs(30), &mut handle.join).await {
            Ok(_) => {}
            Err(_) => {
                tracing::warn!(task_id = %task_id, "session join timed out during kill; aborting");
                handle.join.abort();
                let _ = handle.join.await;
            }
        }

        self.decrement_capacity_for_model(model_id.as_deref());

        if let Some(worktree_path) = handle.worktree_path.as_ref() {
            self.commit_wip_if_needed(&task_id, worktree_path).await;
            self.cleanup_worktree(&task_id, worktree_path).await;
        }
        let (tokens_in, tokens_out) = self.tokens_for_session(&goose_session_id).await;
        self.update_session_record(
            session_record_id.as_deref(),
            SessionStatus::Interrupted,
            tokens_in,
            tokens_out,
        )
        .await;
        self.transition_interrupted(
            &task_id,
            agent_type,
            "session interrupted by supervisor kill",
        )
        .await;

        Ok(())
    }

    pub(super) async fn pause_session(&mut self, task_id: String) -> Result<(), SupervisorError> {
        let Some(mut handle) = self.sessions.remove(&task_id) else {
            return Ok(());
        };

        self.interrupted_sessions.insert(task_id.clone());
        let model_id = self.session_models.remove(&task_id);
        let _agent_type = self
            .session_agent_types
            .remove(&task_id)
            .unwrap_or(AgentType::Worker);
        self.session_projects.remove(&task_id);
        let session_record_id = self.task_session_records.remove(&task_id);
        let goose_session_id = handle.session_id.clone();
        let worktree_path = handle.worktree_path.take();

        handle.cancel.cancel();

        match tokio::time::timeout(Duration::from_secs(30), &mut handle.join).await {
            Ok(_) => {}
            Err(_) => {
                tracing::warn!(task_id = %task_id, "session join timed out during pause; aborting");
                handle.join.abort();
                let _ = handle.join.await;
            }
        }

        self.decrement_capacity_for_model(model_id.as_deref());

        // Commit WIP but keep the worktree alive for resume.
        if let Some(worktree_path) = worktree_path.as_ref() {
            self.commit_wip_if_needed(&task_id, worktree_path).await;
        }

        let (tokens_in, tokens_out) = self.tokens_for_session(&goose_session_id).await;
        self.update_session_record_paused(session_record_id.as_deref(), tokens_in, tokens_out)
            .await;

        tracing::info!(
            task_id = %task_id,
            worktree = ?worktree_path.as_ref().map(|p: &PathBuf| p.display().to_string()),
            "Supervisor: session paused, worktree preserved"
        );

        Ok(())
    }

    pub(super) async fn resume_paused_session(
        &mut self,
        task_id: String,
        project_path: String,
        _requested_model_id: String,
        paused: SessionRecord,
        context_message: String,
    ) -> Result<(), SupervisorError> {
        let goose_session_id = paused.goose_session_id.clone().ok_or_else(|| {
            SupervisorError::Goose(format!(
                "paused session {} has no goose_session_id",
                paused.id
            ))
        })?;
        let worktree_path = paused
            .worktree_path
            .as_deref()
            .map(PathBuf::from)
            .ok_or_else(|| {
                SupervisorError::Goose(format!("paused session {} has no worktree_path", paused.id))
            })?;

        // Use the model from the paused record (continuity — same model that wrote the code).
        let model_id = paused.model_id.clone();

        // Verify worktree still exists.
        if !worktree_path.exists() || !worktree_path.is_dir() {
            // Finalize the stale paused session so it won't be picked up again.
            let session_repo = SessionRepository::new(
                self.app_state.db().clone(),
                self.app_state.events().clone(),
            );
            let _ = session_repo
                .update(
                    &paused.id,
                    SessionStatus::Interrupted,
                    paused.tokens_in,
                    paused.tokens_out,
                )
                .await;
            tracing::warn!(
                task_id = %task_id,
                session_id = %paused.id,
                worktree = %worktree_path.display(),
                "Supervisor: paused session worktree missing; finalized session as interrupted"
            );
            return Err(SupervisorError::PausedSessionStale {
                task_id: task_id.to_string(),
            });
        }

        let max_for_model = self.max_for_model(&model_id);
        let (active, max) = {
            let entry = self
                .capacity
                .entry(model_id.clone())
                .or_insert(ModelCapacity {
                    active: 0,
                    max: max_for_model,
                });
            (entry.active, entry.max)
        };
        if active >= max {
            return Err(SupervisorError::ModelAtCapacity {
                model_id,
                active,
                max,
            });
        }

        let task = self.load_task(&task_id).await?;
        let agent_type = self.agent_type_for_task(&task, false);

        // Don't resume a worker session as a reviewer (or vice versa).
        // The paused session has the wrong system prompt and conversation history.
        if paused.agent_type != agent_type.as_str() {
            tracing::info!(
                task_id = %task_id,
                paused_agent_type = %paused.agent_type,
                needed_agent_type = %agent_type.as_str(),
                "Supervisor: paused session agent type mismatch; skipping resume"
            );
            return Err(SupervisorError::PausedSessionStale { task_id });
        }

        tracing::info!(
            task_id = %task.short_id,
            task_uuid = %task.id,
            goose_session_id = %goose_session_id,
            model_id = %model_id,
            agent_type = %agent_type.as_str(),
            worktree = %worktree_path.display(),
            "Supervisor: resuming paused session"
        );

        self.transition_start(&task, agent_type).await?;

        // Check if the paused session's context is over 80% of the context window.
        // If so, compact before resuming to avoid running out of context mid-task.
        let context_window = self
            .app_state
            .catalog()
            .find_model(&model_id)
            .map(|m| m.context_window)
            .unwrap_or(0);
        if context_window > 0 && paused.tokens_in as f64 >= 0.8 * context_window as f64 {
            tracing::info!(
                task_id = %task_id,
                tokens_in = paused.tokens_in,
                context_window,
                "Supervisor: resume-time compaction triggered (paused session over 80% context threshold)"
            );
            let max_sessions = self.max_for_model(&model_id);
            self.capacity
                .entry(model_id.clone())
                .or_insert(ModelCapacity {
                    active: 0,
                    max: max_sessions,
                })
                .active += 1;
            self.compacting_tasks.insert(task_id.clone());
            let sender = self.sender.clone();
            let session_manager = self.session_manager.clone();
            let app_state = self.app_state.clone();
            tokio::spawn(perform_compaction(
                task_id,
                agent_type,
                task.project_id,
                goose_session_id,
                Some(paused.id),
                model_id,
                Some(worktree_path),
                context_window,
                paused.tokens_in,
                session_manager,
                app_state,
                sender,
                Some(context_message),
            ));
            return Ok(());
        }

        let (catalog_provider_id, model_name) = Self::parse_model_id(&model_id)?;
        let goose_provider_id = self.resolve_goose_provider_id(&catalog_provider_id).await;

        if !self
            .provider_supports_oauth(&goose_provider_id)
            .await
            .unwrap_or(false)
        {
            let (key_name, api_key) = self.load_provider_api_key(&catalog_provider_id).await?;
            GooseConfig::global()
                .set_secret(&key_name, &api_key)
                .map_err(|e| SupervisorError::Goose(e.to_string()))?;
        }

        let goose_model = ModelConfig::new(&model_name)
            .map_err(|e| SupervisorError::Goose(e.to_string()))?
            .with_canonical_limits(&goose_provider_id);

        let extensions = self.extensions_for(agent_type);

        let provider = providers::create(&goose_provider_id, goose_model, extensions.clone())
            .await
            .map_err(|e| {
                self.app_state.health_tracker().record_failure(&model_id);
                SupervisorError::Goose(e.to_string())
            })?;

        let agent = Arc::new(GooseAgent::with_config(GooseAgentConfig::new(
            self.session_manager.clone(),
            PermissionManager::instance(),
            None,
            GooseMode::Auto,
            true,
            GoosePlatform::GooseCli,
        )));

        agent
            .update_provider(provider, &goose_session_id)
            .await
            .map_err(|e| {
                self.app_state.health_tracker().record_failure(&model_id);
                SupervisorError::Goose(e.to_string())
            })?;

        for ext in extensions {
            agent
                .add_extension(ext, &goose_session_id)
                .await
                .map_err(|e| SupervisorError::Goose(e.to_string()))?;
        }

        let prompt = render_prompt(
            agent_type,
            &task,
            &TaskContext {
                project_path: project_path.clone(),
                workspace_path: worktree_path.display().to_string(),
                diff: None,
                commits: None,
                start_commit: None,
                end_commit: None,
                batch_num: None,
                task_count: None,
                tasks_summary: None,
                common_labels: None,
                conflict_files: None,
                merge_base_branch: None,
                merge_target_branch: None,
                merge_failure_context: None,
                setup_commands: None,
                verification_commands: None,
            },
        );
        agent.override_system_prompt(prompt).await;

        // Mark session record as running again.
        let session_repo =
            SessionRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        if let Err(e) = session_repo.set_running(&paused.id).await {
            tracing::warn!(
                record_id = %paused.id,
                error = %e,
                "failed to mark resumed session as running"
            );
        }

        if let Some(entry) = self.capacity.get_mut(&model_id) {
            entry.active += 1;
        }
        let session_cancel = CancellationToken::new();
        let kickoff = GooseMessage::user().with_text(&context_message);
        let join = spawn_reply_task(
            agent,
            goose_session_id.clone(),
            task_id.clone(),
            project_path.clone(),
            worktree_path.clone(),
            agent_type,
            kickoff,
            session_cancel.clone(),
            self.cancel.clone(),
            self.sender.clone(),
            self.app_state.clone(),
            context_window,
            self.session_manager.clone(),
        );

        self.sessions.insert(
            task_id.clone(),
            GooseSessionHandle {
                join,
                cancel: session_cancel,
                session_id: goose_session_id,
                task_id: task_id.clone(),
                worktree_path: Some(worktree_path),
                started_at: Instant::now(),
            },
        );
        self.session_models.insert(task_id.clone(), model_id);
        self.session_projects
            .insert(task_id.clone(), task.project_id.clone());
        self.task_session_records.insert(task_id.clone(), paused.id);
        self.session_agent_types.insert(task_id, agent_type);

        tracing::info!(
            task_id = %task.short_id,
            agent_type = %agent_type.as_str(),
            "Supervisor: resumed session registered"
        );

        Ok(())
    }

    pub(super) async fn cleanup_paused_worker_session(&self, task_id: &str) {
        let repo =
            SessionRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let Ok(Some(paused)) = repo.paused_for_task(task_id).await else {
            return;
        };

        let (tokens_in, tokens_out) = if let Some(ref gsid) = paused.goose_session_id {
            self.tokens_for_session(gsid).await
        } else {
            (paused.tokens_in, paused.tokens_out)
        };

        if let Err(e) = repo
            .update(&paused.id, SessionStatus::Completed, tokens_in, tokens_out)
            .await
        {
            tracing::warn!(
                record_id = %paused.id,
                error = %e,
                "failed to finalize paused session record on task approval"
            );
        }

        if let Some(worktree_path) = paused.worktree_path.as_deref().map(PathBuf::from) {
            self.cleanup_worktree(task_id, &worktree_path).await;
        }
    }

    /// Interrupt the paused worker session after a reviewer rejection, without
    /// cleaning up the worktree. The next dispatch will create a fresh Goose
    /// session that reads the existing worktree state cold — no poisoned history.
    pub(super) async fn interrupt_paused_worker_session(&self, task_id: &str) {
        let repo =
            SessionRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let Ok(Some(paused)) = repo.paused_for_task(task_id).await else {
            return;
        };
        if let Err(e) = repo
            .update(
                &paused.id,
                SessionStatus::Interrupted,
                paused.tokens_in,
                paused.tokens_out,
            )
            .await
        {
            tracing::warn!(
                task_id = %task_id,
                record_id = %paused.id,
                error = %e,
                "failed to interrupt paused worker session after reviewer rejection"
            );
        } else {
            tracing::info!(
                task_id = %task_id,
                record_id = %paused.id,
                goose_session_id = paused.goose_session_id.as_deref().unwrap_or("<none>"),
                "Supervisor: interrupted paused worker session after reviewer rejection — next dispatch will be fresh"
            );
        }
    }

    pub(super) async fn update_session_record_paused(
        &self,
        record_id: Option<&str>,
        tokens_in: i64,
        tokens_out: i64,
    ) {
        let Some(record_id) = record_id else {
            return;
        };

        let repo =
            SessionRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        if let Err(e) = repo.pause(record_id, tokens_in, tokens_out).await {
            tracing::warn!(
                record_id = %record_id,
                error = %e,
                "failed to pause session record"
            );
        }
    }

    pub(super) async fn commit_wip_if_needed(&self, task_id: &str, worktree_path: &PathBuf) {
        let git = match self.app_state.git_actor(worktree_path).await {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "failed to open git actor for worktree");
                return;
            }
        };

        let status = match git
            .run_command(vec!["status".into(), "--porcelain".into()])
            .await
        {
            Ok(out) => out,
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "failed to read worktree status");
                return;
            }
        };

        if status.stdout.trim().is_empty() {
            return;
        }

        if let Err(e) = git.run_command(vec!["add".into(), "-A".into()]).await {
            tracing::warn!(task_id = %task_id, error = %e, "failed to stage interrupted session changes");
            return;
        }

        let message = format!("WIP: interrupted session {task_id}");
        if let Err(e) = git
            .run_command(vec![
                "commit".into(),
                "--no-verify".into(),
                "-m".into(),
                message,
            ])
            .await
        {
            tracing::warn!(task_id = %task_id, error = %e, "failed to commit interrupted session changes");
        }
    }

    pub(super) async fn commit_final_work_if_needed(
        &self,
        task_id: &str,
        worktree_path: &Path,
    ) -> Result<(), String> {
        let git = self
            .app_state
            .git_actor(worktree_path)
            .await
            .map_err(|e| format!("failed to open git actor for worktree: {e}"))?;

        let status = git
            .run_command(vec!["status".into(), "--porcelain".into()])
            .await
            .map_err(|e| format!("failed to read worktree status: {e}"))?;

        if status.stdout.trim().is_empty() {
            return Ok(());
        }

        git.run_command(vec!["add".into(), "-A".into()])
            .await
            .map_err(|e| format!("failed to stage completed session changes: {e}"))?;

        let message = format!("WIP: auto-save completed session {task_id}");
        git.run_command(vec![
            "commit".into(),
            "--no-verify".into(),
            "-m".into(),
            message,
        ])
        .await
        .map_err(|e| format!("failed to commit completed session changes: {e}"))?;

        Ok(())
    }

    pub(super) async fn cleanup_worktree(&self, task_id: &str, worktree_path: &Path) {
        // Guard: don't destroy a worktree if a paused session still references it.
        // The paused session will be resumed later with this worktree.
        let session_repo =
            SessionRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        if let Ok(Some(paused)) = session_repo.paused_for_task(task_id).await {
            if paused.worktree_path.as_deref() == Some(worktree_path.to_str().unwrap_or("")) {
                tracing::info!(
                    task_id = %task_id,
                    worktree = %worktree_path.display(),
                    "Supervisor: skipping worktree cleanup — paused session still references it"
                );
                return;
            }
        }

        let task = match self.load_task(task_id).await {
            Ok(task) => task,
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "failed to load task for worktree cleanup");
                return;
            }
        };

        let Some(project_path) = self.project_path_for_id(&task.project_id).await else {
            tracing::warn!(task_id = %task_id, "project path not found for worktree cleanup");
            return;
        };

        let git = match self.app_state.git_actor(Path::new(&project_path)).await {
            Ok(git) => git,
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "failed to open git actor for worktree cleanup");
                return;
            }
        };

        if let Err(e) = git.remove_worktree(worktree_path).await {
            tracing::warn!(task_id = %task_id, error = %e, "failed to remove worktree; attempting filesystem cleanup");
            if worktree_path.exists()
                && let Err(remove_err) = std::fs::remove_dir_all(worktree_path)
            {
                tracing::warn!(task_id = %task_id, error = %remove_err, "failed to remove worktree directory");
            }
        }
    }

    pub(super) async fn transition_interrupted(&self, task_id: &str, agent_type: AgentType, reason: &str) {
        let action = match agent_type {
            AgentType::Worker | AgentType::ConflictResolver => Some(TransitionAction::Release),
            AgentType::TaskReviewer => Some(TransitionAction::ReleaseTaskReview),
            AgentType::EpicReviewer => None,
        };

        let Some(action) = action else {
            return;
        };

        let repo =
            TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        if let Err(e) = repo
            .transition(
                task_id,
                action,
                "agent-supervisor",
                "system",
                Some(reason),
                None,
            )
            .await
        {
            tracing::warn!(task_id = %task_id, error = %e, "failed to transition interrupted task");
        }
    }
}
