mod resume;
mod result;
mod setup;

use super::*;

impl AgentSupervisor {
    pub(super) async fn dispatch(
        &mut self,
        task_id: String,
        project_path: String,
        model_id: String,
    ) -> Result<(), SupervisorError> {
        if self.sessions.contains_key(&task_id) {
            return Err(SupervisorError::SessionAlreadyActive { task_id });
        }
        // Mark in-flight immediately so stuck detection skips this task for the
        // entire dispatch → session → post-session lifecycle. The caller (Dispatch
        // message handler) removes it on error; SessionCompleted handler removes
        // it after all post-session work is done.
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

        // Check for a paused session — resume it instead of starting fresh.
        if let Some(paused) = self.find_paused_session_record(&task_id).await {
            // Don't resume a worker session when the task needs conflict resolution.
            // The worker doesn't have the conflict resolver prompt or merge setup.
            let has_conflict = self.conflict_context_for_dispatch(&task_id).await.is_some();
            if has_conflict && paused.agent_type == AgentType::Worker.as_str() {
                tracing::info!(
                    task_id = %task_id,
                    paused_session_id = %paused.id,
                    "Supervisor: paused worker session skipped — task needs conflict resolution"
                );
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
                // Fall through to fresh dispatch as conflict resolver below.
            } else {
                let context = self.resume_context_for_task(&task_id).await;
                match self
                    .resume_paused_session(
                        task_id.clone(),
                        project_path.clone(),
                        model_id.clone(),
                        paused,
                        context,
                    )
                    .await
                {
                    Err(SupervisorError::PausedSessionStale { .. }) => {
                        // Stale paused session was finalized; fall through to fresh dispatch below.
                    }
                    other => return other,
                }
            }
        }

        if model_id == "test/mock" {
            if let Some(entry) = self.capacity.get_mut(&model_id) {
                entry.active += 1;
            }
            self.spawn_mock_session(task_id, model_id);
            return Ok(());
        }

        let task = self.load_task(&task_id).await?;
        let active_batch = self.active_epic_batch_for_task(&task.id).await;
        let conflict_ctx = self.conflict_context_for_dispatch(&task.id).await;
        let merge_validation_ctx = self.merge_validation_context_for_dispatch(&task.id).await;
        let agent_type = if active_batch.is_some() {
            AgentType::EpicReviewer
        } else {
            self.agent_type_for_task(&task, conflict_ctx.is_some())
        };

        tracing::info!(
            task_id = %task.short_id,
            task_uuid = %task.id,
            project_id = %task.project_id,
            model_id = %model_id,
            agent_type = %agent_type.as_str(),
            task_status = %task.status,
            has_conflict_context = conflict_ctx.is_some(),
            has_merge_validation_context = merge_validation_ctx.is_some(),
            "Supervisor: dispatch accepted; preparing session"
        );

        self.transition_start(&task, agent_type).await?;

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

        let session_name = format!("{} {}", task.short_id, task.title);
        let project_dir = PathBuf::from(&project_path);
        let worktree_path = if agent_type == AgentType::EpicReviewer {
            let batch_id = active_batch.as_deref().unwrap_or_default();
            self.prepare_epic_reviewer_worktree(&project_dir, batch_id)
                .await?
        } else {
            self.prepare_worktree(&project_dir, &task).await?
        };

        // For conflict resolver: start a merge of the target branch into the task
        // worktree so conflict markers are present for the agent to resolve.
        // Without this, the resolver edits files but the task branch never
        // incorporates main's changes, causing the same conflict on re-merge.
        if agent_type == AgentType::ConflictResolver
            && let Some(ref ctx) = conflict_ctx {
                let target_ref = format!("origin/{}", ctx.merge_target);
                let wt_git = self.app_state.git_actor(&worktree_path).await;
                if let Ok(wt_git) = wt_git {
                    // Fetch latest target first.
                    let _ = wt_git
                        .run_command(vec![
                            "fetch".into(),
                            "origin".into(),
                            ctx.merge_target.clone(),
                        ])
                        .await;
                    // Start the merge — this will leave conflict markers in the worktree.
                    let merge_result = wt_git
                        .run_command(vec!["merge".into(), target_ref.clone(), "--no-commit".into()])
                        .await;
                    if merge_result.is_ok() {
                        // No conflict after all (main changed since last attempt).
                        // Abort the merge and let the normal review flow handle it.
                        let _ = wt_git
                            .run_command(vec!["merge".into(), "--abort".into()])
                            .await;
                    } else {
                        tracing::info!(
                            task_id = %task.short_id,
                            target_ref = %target_ref,
                            "Supervisor: started merge in worktree for conflict resolver; markers present"
                        );
                    }
                }
            }

        let goose_logs_dir = goose::config::paths::Paths::in_state_dir("logs");
        if let Err(e) = std::fs::create_dir_all(&goose_logs_dir) {
            tracing::warn!(
                task_id = %task.short_id,
                path = %goose_logs_dir.display(),
                error = %e,
                "failed to ensure Goose state logs directory"
            );
        }
        if !worktree_path.exists() || !worktree_path.is_dir() {
            let diag = runtime_fs_diagnostics(&project_path, &worktree_path);
            return Err(SupervisorError::Goose(format!(
                "worktree preflight failed before session creation: {diag}"
            )));
        }

        // Load project commands once — used for both setup execution and prompt injection.
        let project_repo =
            ProjectRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let (prompt_setup_commands, prompt_verification_commands) = {
            if let Ok(Some(ref p)) = project_repo.get(&task.project_id).await {
                let setup_names = format_command_names(&p.setup_commands);
                let verify_names = format_command_names(&p.verification_commands);
                (setup_names, verify_names)
            } else {
                (None, None)
            }
        };

        // Run setup commands in the worktree before starting the agent session.
        {
            if let Ok(Some(project)) = project_repo.get(&task.project_id).await {
                let setup_specs: Vec<CommandSpec> =
                    serde_json::from_str(&project.setup_commands).unwrap_or_default();
                if !setup_specs.is_empty() {
                    let setup_start = std::time::Instant::now();
                    tracing::info!(
                        task_id = %task.short_id,
                        command_count = setup_specs.len(),
                        "Supervisor: running setup commands"
                    );
                    let setup_result = run_commands(&setup_specs, &worktree_path).await;
                    match setup_result {
                        Ok(results) => {
                            let failed = results.last().filter(|r| r.exit_code != 0);
                            if let Some(failure) = failed {
                                let reason = format!(
                                    "Setup command '{}' failed (exit {})\nstdout: {}\nstderr: {}",
                                    failure.name,
                                    failure.exit_code,
                                    failure.stdout.trim(),
                                    failure.stderr.trim(),
                                );
                                tracing::warn!(
                                    task_id = %task.short_id,
                                    command = %failure.name,
                                    exit_code = failure.exit_code,
                                    "Supervisor: setup command failed; releasing task"
                                );
                                let task_repo = TaskRepository::new(
                                    self.app_state.db().clone(),
                                    self.app_state.events().clone(),
                                );
                                if let Err(e) = task_repo
                                    .transition(
                                        &task.id,
                                        TransitionAction::Release,
                                        "agent-supervisor",
                                        "system",
                                        Some(&reason),
                                        None,
                                    )
                                    .await
                                {
                                    tracing::warn!(
                                        task_id = %task.short_id,
                                        error = %e,
                                        "failed to block task after setup failure"
                                    );
                                }
                                self.cleanup_worktree(&task.id, &worktree_path).await;
                                return Err(SupervisorError::Goose(format!(
                                    "setup commands failed for task {}: {}",
                                    task.short_id, reason
                                )));
                            }
                            tracing::info!(
                                task_id = %task.short_id,
                                duration_ms = setup_start.elapsed().as_millis(),
                                "Supervisor: setup commands completed"
                            );
                        }
                        Err(e) => {
                            let reason = format!("Setup commands error: {e}");
                            tracing::warn!(
                                task_id = %task.short_id,
                                error = %e,
                                "Supervisor: setup command error; releasing task"
                            );
                            let task_repo = TaskRepository::new(
                                self.app_state.db().clone(),
                                self.app_state.events().clone(),
                            );
                            if let Err(e2) = task_repo
                                .transition(
                                    &task.id,
                                    TransitionAction::Release,
                                    "agent-supervisor",
                                    "system",
                                    Some(&reason),
                                    None,
                                )
                                .await
                            {
                                tracing::warn!(
                                    task_id = %task.short_id,
                                    error = %e2,
                                    "failed to block task after setup error"
                                );
                            }
                            self.cleanup_worktree(&task.id, &worktree_path).await;
                            return Err(SupervisorError::Goose(reason));
                        }
                    }
                }
            }
        }

        let session = self
            .session_manager
            .create_session(worktree_path.clone(), session_name, SessionType::SubAgent)
            .await
            .map_err(|e| SupervisorError::Goose(e.to_string()))?;

        let session_repo =
            SessionRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let session_record = session_repo
            .create(
                &task.project_id,
                &task.id,
                &model_id,
                agent_type.as_str(),
                worktree_path.to_str(),
                Some(session.id.as_str()),
                None,
            )
            .await
            .map_err(|e| SupervisorError::Goose(e.to_string()))?;

        if agent_type == AgentType::EpicReviewer
            && let Some(batch_id) = active_batch
        {
            let batch_repo = EpicReviewBatchRepository::new(
                self.app_state.db().clone(),
                self.app_state.events().clone(),
            );
            if let Err(e) = batch_repo.mark_in_review(&batch_id, &session.id).await {
                tracing::warn!(
                    task_id = %task.short_id,
                    batch_id = %batch_id,
                    error = %e,
                    "failed to mark epic review batch in_review"
                );
            }
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
            .update_provider(provider, &session.id)
            .await
            .map_err(|e| {
                self.app_state.health_tracker().record_failure(&model_id);
                SupervisorError::Goose(e.to_string())
            })?;

        // NOTE: do NOT record_success here — provider creation is just configuration,
        // not an actual API call. Success is recorded in handle_session_result when
        // the session completes without error.

        for ext in extensions {
            agent
                .add_extension(ext, &session.id)
                .await
                .map_err(|e| SupervisorError::Goose(e.to_string()))?;
        }

        let conflict_files = conflict_ctx.as_ref().map(|m| {
            m.conflicting_files
                .iter()
                .map(|f| format!("- {f}"))
                .collect::<Vec<_>>()
                .join("\n")
        });
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
                conflict_files,
                merge_base_branch: conflict_ctx.as_ref().map(|m| m.base_branch.clone()),
                merge_target_branch: conflict_ctx.as_ref().map(|m| m.merge_target.clone()),
                merge_failure_context: merge_validation_ctx,
                setup_commands: prompt_setup_commands,
                verification_commands: prompt_verification_commands,
            },
        );
        agent.override_system_prompt(prompt).await;

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
        let kickoff = GooseMessage::user().with_text(
            "Start by understanding the task context and execute it fully before stopping.",
        );
        let join = spawn_reply_task(
            agent,
            session.id.clone(),
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
                session_id: session.id,
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
        self.session_agent_types.insert(task_id, agent_type);

        if let Some(handle) = self.sessions.get(&task.id) {
            tracing::info!(
                task_id = %task.short_id,
                task_uuid = %task.id,
                project_id = %task.project_id,
                session_id = %handle.session_id,
                agent_type = %agent_type.as_str(),
                worktree = ?handle.worktree_path.as_ref().map(|p| p.display().to_string()),
                "Supervisor: session registered"
            );
        }

        Ok(())
    }
}
