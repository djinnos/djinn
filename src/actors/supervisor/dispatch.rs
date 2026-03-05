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
        if agent_type == AgentType::ConflictResolver {
            if let Some(ref ctx) = conflict_ctx {
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

    pub(super) async fn dispatch_resume(
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

    pub(super) async fn handle_session_result(
        &self,
        task_id: &str,
        session: SessionClosure,
        result: Result<(), String>,
        output: ParsedAgentOutput,
    ) {
        let agent_type = session.agent_type;
        let repo =
            TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());

        if let Some(model_id) = session.model_id.as_deref() {
            match &result {
                Ok(()) => self.app_state.health_tracker().record_success(model_id),
                Err(_) => self.app_state.health_tracker().record_failure(model_id),
            }
            self.app_state.persist_model_health_state().await;
        }

        let (tokens_in, tokens_out) = self.tokens_for_session(&session.goose_session_id).await;

        // Worker Done: pause session record (keep worktree alive for resume after review).
        // All other cases: complete or fail the session record.
        let is_worker_done = result.is_ok()
            && matches!(agent_type, AgentType::Worker | AgentType::ConflictResolver)
            && matches!(output.worker_signal, Some(WorkerSignal::Done));

        if is_worker_done {
            self.update_session_record_paused(session.record_id.as_deref(), tokens_in, tokens_out)
                .await;
        } else {
            let session_status = if result.is_ok() {
                SessionStatus::Completed
            } else {
                SessionStatus::Failed
            };
            self.update_session_record(
                session.record_id.as_deref(),
                session_status,
                tokens_in,
                tokens_out,
            )
            .await;
        }

        if let Some(worktree_path) = session.worktree_path.as_ref() {
            // Post-DONE validation pipeline: setup → verification.
            // Any failure at either step bounces back to the worker with feedback.
            if is_worker_done {
                // Re-run setup commands to catch issues like stale lockfiles,
                // missing dependencies, etc. introduced by the worker's changes.
                if let Some(feedback) =
                    self.run_setup_commands_checked(task_id, worktree_path).await
                {
                    self.queue_resume_after_verification_failure(
                        task_id,
                        &session,
                        worktree_path,
                        &feedback,
                        tokens_in,
                    )
                    .await;
                    return;
                }

                // Run verification commands (tsc, cargo check, etc.).
                if let Some(feedback) = self.run_verification_commands(task_id, worktree_path).await
                {
                    self.queue_resume_after_verification_failure(
                        task_id,
                        &session,
                        worktree_path,
                        &feedback,
                        tokens_in,
                    )
                    .await;
                    return;
                }
            }

            if is_worker_done {
                // Commit final work and keep worktree alive for the review→resume cycle.
                if let Err(e) = self
                    .commit_final_work_if_needed(task_id, worktree_path)
                    .await
                {
                    tracing::warn!(
                        task_id = %task_id,
                        worktree_path = %worktree_path.display(),
                        error = %e,
                        "failed to commit work before pausing for review; preserving worktree"
                    );
                }
                // Worktree intentionally kept — cleaned up in cleanup_paused_worker_session
                // when the task is finally approved.
            } else {
                self.cleanup_worktree(task_id, worktree_path).await;
            }
        }

        if let Some(feedback) = output.reviewer_feedback.as_deref() {
            let payload = serde_json::json!({ "body": feedback }).to_string();
            if let Err(e) = repo
                .log_activity(
                    Some(task_id),
                    "agent-supervisor",
                    "task_reviewer",
                    "comment",
                    &payload,
                )
                .await
            {
                tracing::warn!(task_id = %task_id, error = %e, "failed to store reviewer feedback comment");
            }
        }

        if let Err(reason) = &result {
            let payload = serde_json::json!({
                "error": reason,
                "agent_type": agent_type.as_str(),
            })
            .to_string();
            if let Err(e) = repo
                .log_activity(
                    Some(task_id),
                    "agent-supervisor",
                    "system",
                    "session_error",
                    &payload,
                )
                .await
            {
                tracing::warn!(task_id = %task_id, error = %e, "failed to store session error activity");
            }
        }

        if result.is_ok()
            && let Some(reason) = output.runtime_error.as_deref()
        {
            let payload = serde_json::json!({
                "error": reason,
                "agent_type": agent_type.as_str(),
            })
            .to_string();
            if let Err(e) = repo
                .log_activity(
                    Some(task_id),
                    "agent-supervisor",
                    "system",
                    "session_error",
                    &payload,
                )
                .await
            {
                tracing::warn!(task_id = %task_id, error = %e, "failed to store session error activity");
            }
        }

        let epic_error = result.as_ref().err().cloned();
        let transition = match result {
            Ok(()) => self.success_transition(task_id, agent_type, &output).await,
            Err(reason) => match agent_type {
                AgentType::Worker | AgentType::ConflictResolver => {
                    Some((TransitionAction::Release, Some(reason)))
                }
                AgentType::TaskReviewer => {
                    Some((TransitionAction::ReleaseTaskReview, Some(reason)))
                }
                AgentType::EpicReviewer => None,
            },
        };

        if agent_type == AgentType::EpicReviewer {
            self.finalize_epic_batch(task_id, &output, epic_error.as_deref())
                .await;
        }

        if let Some((action, reason)) = transition {
            tracing::info!(
                task_id = %task_id,
                agent_type = %agent_type.as_str(),
                transition_action = ?action,
                transition_reason = reason.as_deref().unwrap_or("<none>"),
                tokens_in,
                tokens_out,
                "Supervisor: applying session transition"
            );
            let is_reviewer_rejection = matches!(
                action,
                TransitionAction::TaskReviewReject | TransitionAction::TaskReviewRejectConflict
            );
            if let Err(e) = repo
                .transition(
                    task_id,
                    action,
                    "agent-supervisor",
                    "system",
                    reason.as_deref(),
                    None,
                )
                .await
            {
                tracing::warn!(task_id = %task_id, error = %e, "failed to transition task after session");
            }

            // After a reviewer rejection, interrupt any paused worker session so the
            // next dispatch starts a fresh Goose session. Without this, the resumed
            // worker sees its own "I already completed this" conversation history and
            // outputs DONE immediately without doing real work → infinite reject loop.
            // The worktree is preserved so the fresh worker can inspect existing state.
            if is_reviewer_rejection {
                self.interrupt_paused_worker_session(task_id).await;
            }
        } else {
            tracing::info!(
                task_id = %task_id,
                agent_type = %agent_type.as_str(),
                tokens_in,
                tokens_out,
                "Supervisor: session completed with no task transition"
            );
        }

        // Capacity has just been released by this session completion. Trigger an
        // immediate dispatch pass for the same project so the next ready task
        // starts without waiting for the coordinator interval tick.
        if let Ok(task) = self.load_task(task_id).await
            && let Some(coordinator) = self.app_state.coordinator().await
        {
            let _ = coordinator
                .trigger_dispatch_for_project(&task.project_id)
                .await;
        }
    }

    /// Runs the project's setup commands in the task worktree.
    /// Called after conflict resolution to refresh the environment (e.g. reinstall
    /// dependencies that changed as a result of merging main into the task branch).
    /// Failures are logged as warnings but do not abort the session.
    /// Runs the project's setup commands in the task worktree.
    /// Returns `None` if all commands pass or there are no setup commands.
    /// Returns `Some(feedback)` if any command fails, with the failure details.
    pub(super) async fn run_setup_commands_checked(
        &self,
        task_id: &str,
        worktree_path: &Path,
    ) -> Option<String> {
        let task = self.load_task(task_id).await.ok()?;
        let project_repo =
            ProjectRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let project = project_repo.get(&task.project_id).await.ok()??;
        let specs: Vec<CommandSpec> =
            serde_json::from_str(&project.setup_commands).unwrap_or_default();
        if specs.is_empty() {
            return None;
        }
        tracing::info!(
            task_id = %task_id,
            command_count = specs.len(),
            "Supervisor: running setup commands"
        );
        match run_commands(&specs, worktree_path).await {
            Ok(results) => {
                let failed = results.iter().find(|r| r.exit_code != 0)?;
                tracing::info!(
                    task_id = %task_id,
                    command = %failed.name,
                    exit_code = failed.exit_code,
                    "Supervisor: setup command failed"
                );
                let trim_output = |s: &str| -> String {
                    let lines: Vec<&str> = s.trim().lines().collect();
                    if lines.len() > 50 {
                        format!(
                            "... ({} lines truncated) ...\n{}",
                            lines.len() - 50,
                            lines[lines.len() - 50..].join("\n")
                        )
                    } else {
                        lines.join("\n")
                    }
                };
                Some(format!(
                    "Setup command '{}' failed with exit code {}.\n\nYour changes likely broke a setup step (e.g. lockfile out of sync with package.json). Use your shell tools to fix the issue, then signal WORKER_RESULT: DONE.\n\nstdout:\n{}\nstderr:\n{}",
                    failed.name,
                    failed.exit_code,
                    trim_output(&failed.stdout),
                    trim_output(&failed.stderr),
                ))
            }
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "Supervisor: setup command system error");
                Some(format!(
                    "Setup commands could not run: {e}\n\nFix the issue and signal WORKER_RESULT: DONE when complete."
                ))
            }
        }
    }

    /// Runs the project's verification commands in the task worktree.
    /// Returns `None` if all commands pass or there are no verification commands.
    /// Returns `Some(feedback)` if any command fails, with the failure details.
    pub(super) async fn run_verification_commands(
        &self,
        task_id: &str,
        worktree_path: &Path,
    ) -> Option<String> {
        let task = self.load_task(task_id).await.ok()?;
        let project_repo =
            ProjectRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let project = project_repo.get(&task.project_id).await.ok()??;
        let specs: Vec<CommandSpec> =
            serde_json::from_str(&project.verification_commands).unwrap_or_default();
        if specs.is_empty() {
            return None;
        }
        tracing::info!(
            task_id = %task_id,
            command_count = specs.len(),
            "Supervisor: running verification commands"
        );
        match run_commands(&specs, worktree_path).await {
            Ok(results) => {
                let failed = results.iter().find(|r| r.exit_code != 0)?;
                tracing::info!(
                    task_id = %task_id,
                    command = %failed.name,
                    exit_code = failed.exit_code,
                    "Supervisor: verification command failed"
                );
                // Trim output to last 50 lines to avoid context overflow on noisy tools like tsc.
                // The agent has shell tools and can re-run the command or read files as needed.
                let trim_output = |s: &str| -> String {
                    let lines: Vec<&str> = s.trim().lines().collect();
                    if lines.len() > 50 {
                        format!(
                            "... ({} lines truncated) ...\n{}",
                            lines.len() - 50,
                            lines[lines.len() - 50..].join("\n")
                        )
                    } else {
                        lines.join("\n")
                    }
                };
                Some(format!(
                    "Verification command '{}' failed with exit code {}.\n\nUse your shell and editor tools to inspect and fix the issue, then signal WORKER_RESULT: DONE.\n\nstdout:\n{}\nstderr:\n{}",
                    failed.name,
                    failed.exit_code,
                    trim_output(&failed.stdout),
                    trim_output(&failed.stderr),
                ))
            }
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "Supervisor: verification command system error");
                Some(format!(
                    "Verification commands could not run: {e}\n\nFix the issue and signal WORKER_RESULT: DONE when complete."
                ))
            }
        }
    }

    /// Logs the verification failure as a task comment and queues a ResumeSession message.
    pub(super) async fn queue_resume_after_verification_failure(
        &self,
        task_id: &str,
        session: &SessionClosure,
        worktree_path: &Path,
        feedback: &str,
        tokens_in: i64,
    ) {
        let repo =
            TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let payload = serde_json::json!({ "body": feedback }).to_string();
        if let Err(e) = repo
            .log_activity(
                Some(task_id),
                "agent-supervisor",
                "verification",
                "comment",
                &payload,
            )
            .await
        {
            tracing::warn!(task_id = %task_id, error = %e, "failed to log verification failure comment");
        }

        let Some(model_id) = session.model_id.clone() else {
            tracing::warn!(
                task_id = %task_id,
                "no model_id in session closure; cannot resume after verification failure"
            );
            return;
        };

        let msg = SupervisorMessage::ResumeSession {
            task_id: task_id.to_owned(),
            model_id,
            goose_session_id: session.goose_session_id.clone(),
            worktree_path: worktree_path.to_owned(),
            resume_prompt: feedback.to_owned(),
            tokens_in,
            old_record_id: session.record_id.clone(),
        };
        if let Err(e) = self.sender.send(msg).await {
            tracing::warn!(task_id = %task_id, error = %e, "failed to queue resume session after verification failure");
        }
    }

    pub(super) async fn success_transition(
        &self,
        task_id: &str,
        agent_type: AgentType,
        output: &ParsedAgentOutput,
    ) -> Option<(TransitionAction, Option<String>)> {
        match agent_type {
            AgentType::Worker | AgentType::ConflictResolver => match output.worker_signal {
                Some(WorkerSignal::Done) => Some((TransitionAction::SubmitTaskReview, None)),
                None => {
                    let reason = output.runtime_error.clone().unwrap_or_else(|| {
                        "worker session completed without DONE marker".to_string()
                    });
                    tracing::warn!(reason = %reason, "worker session completed without structured result marker");
                    Some((TransitionAction::Release, Some(reason)))
                }
            },
            AgentType::TaskReviewer => match output.reviewer_verdict {
                Some(ReviewerVerdict::Verified) => self.merge_after_task_review(task_id).await,
                Some(ReviewerVerdict::Reopen) => Some((
                    TransitionAction::TaskReviewReject,
                    Some(
                        output
                            .reviewer_feedback
                            .clone()
                            .unwrap_or_else(|| "reviewer requested REOPEN".to_string()),
                    ),
                )),
                None => {
                    tracing::warn!("task reviewer session completed without REVIEW_RESULT marker");
                    Some((
                        TransitionAction::ReleaseTaskReview,
                        Some("reviewer session completed without REVIEW_RESULT marker".to_string()),
                    ))
                }
            },
            AgentType::EpicReviewer => match output.epic_verdict {
                Some(EpicReviewVerdict::Clean) => None,
                Some(EpicReviewVerdict::IssuesFound) => None,
                None => {
                    tracing::warn!(
                        "epic reviewer session completed without EPIC_REVIEW_RESULT marker"
                    );
                    None
                }
            },
        }
    }
}
