use super::*;

impl AgentSupervisor {
    #[allow(clippy::too_many_arguments)]
    pub(in super::super) async fn dispatch_resume(
        &mut self,
        task_id: String,
        model_id: String,
        goose_session_id: String,
        worktree_path: PathBuf,
        resume_prompt: String,
        tokens_in: i64,
        old_record_id: Option<String>,
    ) -> Result<(), SupervisorError> {
        if self.sessions.contains_key(&task_id) {
            return Err(SupervisorError::SessionAlreadyActive { task_id });
        }
        // Re-mark in-flight: this is called after a SessionCompleted removed it
        // (verification-failure resume path). Caller removes on error.
        self.in_flight.insert(task_id.clone());

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

        // Check if the paused session's context is over 80% of the context window.
        // If so, compact before resuming to avoid running out of context mid-task.
        let context_window = self
            .app_state
            .catalog()
            .find_model(&model_id)
            .map(|m| m.context_window)
            .unwrap_or(0);
        if context_window > 0 && tokens_in as f64 >= 0.8 * context_window as f64 {
            tracing::info!(
                task_id = %task_id,
                tokens_in,
                context_window,
                "Supervisor: dispatch_resume compaction triggered (over 80% context threshold)"
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
                AgentType::Worker,
                task.project_id,
                goose_session_id,
                old_record_id,
                model_id,
                Some(worktree_path),
                context_window,
                tokens_in,
                session_manager,
                app_state,
                sender,
                Some(resume_prompt),
            ));
            return Ok(());
        }
        let project_path = self
            .project_path_for_id(&task.project_id)
            .await
            .unwrap_or_else(|| task.project_id.clone());

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

        let session_repo =
            SessionRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let session_record = session_repo
            .create(
                &task.project_id,
                &task.id,
                &model_id,
                AgentType::Worker.as_str(),
                worktree_path.to_str(),
                Some(&goose_session_id),
                None,
            )
            .await
            .map_err(|e| SupervisorError::Goose(e.to_string()))?;

        let goose_model = ModelConfig::new(&model_name)
            .map_err(|e| SupervisorError::Goose(e.to_string()))?
            .with_canonical_limits(&goose_provider_id);

        let extensions = self.extensions_for(AgentType::Worker);

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

        if let Some(entry) = self.capacity.get_mut(&model_id) {
            entry.active += 1;
        }

        let context_window = self
            .app_state
            .catalog()
            .find_model(&model_id)
            .map(|m| m.context_window)
            .unwrap_or(0);
        let session_cancel = CancellationToken::new();
        let kickoff = GooseMessage::user().with_text(&resume_prompt);

        let join = spawn_reply_task(
            agent,
            goose_session_id.clone(),
            task_id.clone(),
            project_path.clone(),
            worktree_path.clone(),
            AgentType::Worker,
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
                session_id: goose_session_id.clone(),
                task_id: task_id.clone(),
                worktree_path: Some(worktree_path),
                started_at: Instant::now(),
            },
        );
        self.session_models.insert(task_id.clone(), model_id);
        self.session_projects
            .insert(task_id.clone(), task.project_id.clone());
        self.task_session_records
            .insert(task_id.clone(), session_record.id);
        self.session_agent_types
            .insert(task_id.clone(), AgentType::Worker);

        tracing::info!(
            task_id = %task.short_id,
            task_uuid = %task.id,
            "Supervisor: resume session dispatched after verification failure"
        );

        Ok(())
    }
}
