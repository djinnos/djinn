use super::*;

/// Async compaction worker — runs in a spawned task, sends result back to supervisor via `sender`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn perform_compaction(
    task_id: String,
    agent_type: AgentType,
    project_id: String,
    old_goose_session_id: String,
    old_record_id: Option<String>,
    model_id: String,
    worktree_path: Option<PathBuf>,
    context_window: i64,
    tokens_in: i64,
    session_manager: Arc<SessionManager>,
    app_state: AppState,
    sender: mpsc::Sender<SupervisorMessage>,
    // Resume context appended to the summary (for resume-time compaction).
    // When present, the kickoff becomes "{summary}\n\n---\n\n{resume_context}".
    resume_context: Option<String>,
) {
    let abort = |task_id: String, model_id: String, worktree_path: Option<PathBuf>| {
        let s = sender.clone();
        async move {
            let _ = s
                .send(SupervisorMessage::CompactionAborted {
                    task_id,
                    model_id,
                    agent_type,
                    worktree_path,
                })
                .await;
        }
    };

    // 1. Read conversation history + final token counts from old Goose session.
    let (final_tokens_in, final_tokens_out, messages) = match session_manager
        .get_session(&old_goose_session_id, true)
        .await
    {
        Ok(s) => {
            let tin = s.accumulated_input_tokens.or(s.input_tokens).unwrap_or(0) as i64;
            let tout = s.accumulated_output_tokens.or(s.output_tokens).unwrap_or(0) as i64;
            let msgs = s
                .conversation
                .map(|c| c.messages().clone())
                .unwrap_or_default();
            (tin.max(tokens_in), tout, msgs)
        }
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                error = %e,
                "compaction: failed to read Goose session"
            );
            (tokens_in, 0, vec![])
        }
    };

    // 2. Finalize old Djinn session record as Compacted.
    if let Some(record_id) = old_record_id.as_deref() {
        let repo = SessionRepository::new(app_state.db().clone(), app_state.events().clone());
        if let Err(e) = repo
            .update(
                record_id,
                SessionStatus::Compacted,
                final_tokens_in,
                final_tokens_out,
            )
            .await
        {
            tracing::warn!(record_id = %record_id, error = %e, "compaction: failed to finalize old session record");
        }
    }

    // 3. Parse model ID and resolve Goose provider.
    let (catalog_provider_id, model_name) = {
        let Some((c, m)) = model_id.split_once('/') else {
            tracing::warn!(task_id = %task_id, model_id = %model_id, "compaction: invalid model_id");
            abort(task_id, model_id, worktree_path).await;
            return;
        };
        (c.to_owned(), m.to_owned())
    };

    let entries = providers::providers().await;
    let canonical = |id: &str| -> String {
        id.chars()
            .filter(char::is_ascii_alphanumeric)
            .flat_map(char::to_lowercase)
            .collect()
    };
    let goose_provider_id = entries
        .iter()
        .find(|(meta, _)| meta.name == catalog_provider_id)
        .or_else(|| {
            let c = canonical(&catalog_provider_id);
            entries.iter().find(|(meta, _)| canonical(&meta.name) == c)
        })
        .map(|(meta, _)| meta.name.clone())
        .unwrap_or_else(|| catalog_provider_id.clone());

    let supports_oauth = entries
        .iter()
        .find(|(meta, _)| meta.name == goose_provider_id)
        .map(|(meta, _)| meta.config_keys.iter().any(|k| k.oauth_flow))
        .unwrap_or(false);

    if !supports_oauth {
        let key_name = app_state
            .catalog()
            .list_providers()
            .into_iter()
            .find(|p| p.id == catalog_provider_id)
            .and_then(|p| p.env_vars.into_iter().next())
            .unwrap_or_else(|| format!("{}_API_KEY", catalog_provider_id.to_ascii_uppercase()));
        let cred_repo =
            CredentialRepository::new(app_state.db().clone(), app_state.events().clone());
        match cred_repo.get_decrypted(&key_name).await {
            Ok(Some(api_key)) => {
                if let Err(e) = GooseConfig::global().set_secret(&key_name, &api_key) {
                    tracing::warn!(error = %e, "compaction: failed to set API key");
                }
            }
            Ok(None) => {
                tracing::warn!(
                    task_id = %task_id,
                    key_name = %key_name,
                    "compaction: no API key found"
                );
            }
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "compaction: credential lookup failed");
            }
        }
    }

    let goose_model = match ModelConfig::new(&model_name) {
        Ok(m) => m.with_canonical_limits(&goose_provider_id),
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "compaction: failed to build ModelConfig");
            abort(task_id, model_id, worktree_path).await;
            return;
        }
    };

    // 4. Generate summary via provider.complete() — no tools, no extensions.
    let summary = if messages.is_empty() {
        tracing::warn!(task_id = %task_id, "compaction: empty conversation history; using fallback summary");
        "Context window was compacted. Please review the current state of the worktree and continue the task.".to_string()
    } else {
        let compaction_system = crate::agent::prompts::render_compaction_prompt();
        let summary_provider =
            match providers::create(&goose_provider_id, goose_model.clone(), vec![]).await {
                Ok(p) => p,
                Err(e) => {
                    app_state.health_tracker().record_failure(&model_id);
                    tracing::warn!(
                        task_id = %task_id,
                        error = %e,
                        "compaction: summary provider creation failed; aborting compaction"
                    );
                    abort(task_id, model_id, worktree_path).await;
                    return;
                }
            };
        let model_config = summary_provider.get_model_config();
        match summary_provider
            .complete(
                &model_config,
                &old_goose_session_id,
                compaction_system,
                &messages,
                &[],
            )
            .await
        {
            Ok((msg, _)) => {
                tracing::info!(
                    task_id = %task_id,
                    "compaction: summary generated successfully"
                );
                msg.as_concat_text()
            }
            Err(e) => {
                tracing::warn!(
                    task_id = %task_id,
                    error = %e,
                    "compaction: summary generation failed; aborting compaction"
                );
                abort(task_id, model_id, worktree_path).await;
                return;
            }
        }
    };

    // Worktree is required for the new session.
    let Some(worktree_path) = worktree_path else {
        tracing::warn!(task_id = %task_id, "compaction: no worktree path; aborting");
        abort(task_id, model_id, None).await;
        return;
    };

    // 5. Create new Goose session.
    let task_name = {
        let task_repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
        match task_repo.get(&task_id).await {
            Ok(Some(t)) => format!("{} {} (compacted)", t.short_id, t.title),
            _ => format!("{task_id} (compacted)"),
        }
    };
    let new_goose_session = match session_manager
        .create_session(worktree_path.clone(), task_name, SessionType::SubAgent)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "compaction: failed to create new Goose session");
            abort(task_id, model_id, Some(worktree_path)).await;
            return;
        }
    };

    // 6. Create new Djinn session record with continuation_of pointing to the old record.
    let session_repo = SessionRepository::new(app_state.db().clone(), app_state.events().clone());
    let new_record = match session_repo
        .create(
            &project_id,
            &task_id,
            &model_id,
            agent_type.as_str(),
            worktree_path.to_str(),
            Some(&new_goose_session.id),
            old_record_id.as_deref(),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "compaction: failed to create new session record");
            abort(task_id, model_id, Some(worktree_path)).await;
            return;
        }
    };

    // Log compaction activity entry so the desktop can render the session timeline.
    {
        let task_repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
        let usage_pct = if context_window > 0 {
            final_tokens_in as f64 / context_window as f64
        } else {
            0.0
        };
        let payload = serde_json::json!({
            "old_session_id": old_record_id.as_deref().unwrap_or(""),
            "new_session_id": new_record.id,
            "tokens_in_at_compaction": final_tokens_in,
            "context_window": context_window,
            "usage_pct": usage_pct,
            "summary_token_count": summary.chars().count(),
        })
        .to_string();
        if let Err(e) = task_repo
            .log_activity(Some(&task_id), "system", "system", "compaction", &payload)
            .await
        {
            tracing::warn!(task_id = %task_id, error = %e, "compaction: failed to log activity");
        }
    }

    // 7. Set up new agent with provider and extensions.
    let extensions = vec![extension::config(agent_type)];
    let provider = match providers::create(&goose_provider_id, goose_model, extensions.clone())
        .await
    {
        Ok(p) => p,
        Err(e) => {
            app_state.health_tracker().record_failure(&model_id);
            tracing::warn!(task_id = %task_id, error = %e, "compaction: failed to create new agent provider");
            abort(task_id, model_id, Some(worktree_path)).await;
            return;
        }
    };

    let agent = Arc::new(GooseAgent::with_config(GooseAgentConfig::new(
        session_manager.clone(),
        PermissionManager::instance(),
        None,
        GooseMode::Auto,
        true,
        GoosePlatform::GooseCli,
    )));

    if let Err(e) = agent.update_provider(provider, &new_goose_session.id).await {
        app_state.health_tracker().record_failure(&model_id);
        tracing::warn!(task_id = %task_id, error = %e, "compaction: failed to set provider on new agent");
        abort(task_id, model_id, Some(worktree_path)).await;
        return;
    }

    for ext in extensions {
        if let Err(e) = agent.add_extension(ext, &new_goose_session.id).await {
            tracing::warn!(task_id = %task_id, error = %e, "compaction: failed to add extension to new agent");
        }
    }

    // 8. Append resume context to summary if this is a resume-time compaction.
    let kickoff_summary = match resume_context {
        Some(ctx) => format!("{summary}\n\n---\n\n{ctx}"),
        None => summary,
    };

    // 9. Send CompactionComplete — supervisor will spawn the new reply task.
    let _ = sender
        .send(SupervisorMessage::CompactionComplete {
            task_id,
            model_id,
            agent_type,
            project_id,
            new_goose_session_id: new_goose_session.id,
            new_record_id: new_record.id,
            agent,
            worktree_path,
            summary: kickoff_summary,
            context_window,
        })
        .await;
}

impl AgentSupervisor {
    /// Detect context exhaustion at session end and trigger a fresh continuation
    /// instead of treating it as a normal failure/release. Returns `true` if
    /// compaction was initiated (caller should skip normal result handling).
    pub(super) async fn maybe_compact_on_context_exhaustion(
        &mut self,
        task_id: &str,
        result: &Result<(), String>,
        output: &ParsedAgentOutput,
    ) -> bool {
        let is_context_error = match result {
            Err(reason) => {
                let lower = reason.to_lowercase();
                lower.contains("context length exceeded")
                    || lower.contains("context_length_exceeded")
                    || lower.contains("context limit exceeded")
            }
            Ok(()) => {
                // Goose handled it internally but gave up after compaction attempts.
                output.runtime_error.as_deref().map_or(false, |e| {
                    let lower = e.to_lowercase();
                    lower.contains("context length exceeded")
                        || lower.contains("context limit exceeded")
                })
            }
        } || output.context_exhausted; // Goose catches the error internally and reports it as text

        if !is_context_error {
            return false;
        }

        let agent_type = self
            .session_agent_types
            .get(task_id)
            .copied()
            .unwrap_or(AgentType::Worker);

        // For reviewers: compaction won't help (the prompt itself exceeds the
        // context window). Release gracefully and block the task so it doesn't
        // loop endlessly with the same model.
        if matches!(agent_type, AgentType::TaskReviewer | AgentType::EpicReviewer) {
            tracing::warn!(
                task_id = %task_id,
                agent_type = %agent_type.as_str(),
                "Supervisor: context_length_exceeded on reviewer — prompt too large for model; blocking task"
            );
            let session = self.remove_session(task_id);
            // Mark session as failed.
            let (tokens_in, tokens_out) = {
                let sm = self.session_manager.clone();
                let sid = &session.goose_session_id;
                if let Ok(s) = sm.get_session(sid, false).await {
                    let ti = s.accumulated_input_tokens.or(s.input_tokens).unwrap_or(0) as i64;
                    let to = s.accumulated_output_tokens.or(s.output_tokens).unwrap_or(0) as i64;
                    (ti, to)
                } else {
                    (0, 0)
                }
            };
            self.update_session_record(
                session.record_id.as_deref(),
                SessionStatus::Failed,
                tokens_in,
                tokens_out,
            )
            .await;
            if let Some(wp) = session.worktree_path.as_ref() {
                self.cleanup_worktree(task_id, wp).await;
            }
            // Record model failure so health tracker deprioritizes this model.
            if let Some(model_id) = session.model_id.as_deref() {
                self.app_state.health_tracker().record_failure(model_id);
                self.app_state.persist_model_health_state().await;
            }
            let repo = TaskRepository::new(
                self.app_state.db().clone(),
                self.app_state.events().clone(),
            );
            let reason = "context_length_exceeded: review prompt too large for current model";
            let _ = repo
                .transition(
                    task_id,
                    TransitionAction::ReleaseTaskReview,
                    "agent-supervisor",
                    "system",
                    Some(reason),
                    None,
                )
                .await;
            self.in_flight.remove(task_id);
            return true;
        }

        let Some(handle) = self.sessions.remove(task_id) else {
            return false;
        };

        let goose_session_id = handle.session_id.clone();
        let worktree_path = handle.worktree_path;
        let model_id = self.session_models.remove(task_id).unwrap_or_default();
        let agent_type = self
            .session_agent_types
            .remove(task_id)
            .unwrap_or(AgentType::Worker);
        let project_id = self.session_projects.remove(task_id).unwrap_or_default();
        let old_record_id = self.task_session_records.remove(task_id);

        // Don't decrement capacity — the compaction slot replaces the old one.
        self.compacting_tasks.insert(task_id.to_owned());
        // Suppress the interrupted_sessions guard (session already completed).

        let context_window = self
            .app_state
            .catalog()
            .find_model(&model_id)
            .map(|m| m.context_window)
            .unwrap_or(200_000);

        tracing::info!(
            task_id = %task_id,
            goose_session_id = %goose_session_id,
            agent_type = %agent_type.as_str(),
            "Supervisor: context exhaustion detected at session end; triggering fresh continuation"
        );

        let sender = self.sender.clone();
        let session_manager = self.session_manager.clone();
        let app_state = self.app_state.clone();

        // Session is already done — no join handle to wait on. Spawn compaction directly.
        let task_id_owned = task_id.to_owned();
        tokio::spawn(async move {
            perform_compaction(
                task_id_owned,
                agent_type,
                project_id,
                goose_session_id,
                old_record_id,
                model_id,
                worktree_path,
                context_window,
                // tokens_in: use context_window as a signal that we're at the limit
                context_window,
                session_manager,
                app_state,
                sender,
                None,
            )
            .await;
        });

        true
    }

    pub(super) async fn handle_compaction_needed(
        &mut self,
        task_id: String,
        old_goose_session_id: String,
        tokens_in: i64,
        context_window: i64,
    ) {
        let Some(mut handle) = self.sessions.remove(&task_id) else {
            tracing::warn!(task_id = %task_id, "compaction: task not in sessions map");
            return;
        };

        let model_id = self.session_models.remove(&task_id).unwrap_or_default();
        let agent_type = self
            .session_agent_types
            .remove(&task_id)
            .unwrap_or(AgentType::Worker);
        let project_id = self.session_projects.remove(&task_id).unwrap_or_default();
        let old_record_id = self.task_session_records.remove(&task_id);
        let worktree_path = handle.worktree_path.take();

        // Track this task as compacting so has_session() returns true and coordinator
        // does not release the task as stuck during the async compaction window.
        self.compacting_tasks.insert(task_id.clone());
        // Suppress the SessionCompleted that will arrive after cancellation.
        self.interrupted_sessions.insert(task_id.clone());

        handle.cancel.cancel();

        tracing::info!(
            task_id = %task_id,
            old_goose_session_id = %old_goose_session_id,
            tokens_in,
            context_window,
            threshold_pct = 80,
            "Supervisor: compaction triggered; cancelling current turn"
        );

        let sender = self.sender.clone();
        let session_manager = self.session_manager.clone();
        let app_state = self.app_state.clone();
        let abort_handle = handle.join.abort_handle();

        tokio::spawn(async move {
            match tokio::time::timeout(Duration::from_secs(30), handle.join).await {
                Ok(_) => {}
                Err(_) => {
                    tracing::warn!(task_id = %task_id, "compaction: timeout waiting for old session to exit; aborting");
                    abort_handle.abort();
                }
            }

            perform_compaction(
                task_id,
                agent_type,
                project_id,
                old_goose_session_id,
                old_record_id,
                model_id,
                worktree_path,
                context_window,
                tokens_in,
                session_manager,
                app_state,
                sender,
                None,
            )
            .await;
        });
    }

    pub(super) async fn handle_compaction_complete(
        &mut self,
        task_id: String,
        model_id: String,
        agent_type: AgentType,
        project_id: String,
        new_goose_session_id: String,
        new_record_id: String,
        agent: Arc<GooseAgent>,
        worktree_path: PathBuf,
        summary: String,
        context_window: i64,
    ) {
        self.compacting_tasks.remove(&task_id);

        let project_path = self
            .project_path_for_id(&project_id)
            .await
            .unwrap_or_else(|| project_id.clone());

        let task = match self.load_task(&task_id).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "compaction_complete: failed to load task");
                return;
            }
        };

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

        // Capacity: the old session slot was never released (compacting_tasks kept it occupied),
        // so just ensure the model entry exists — no increment needed.
        let max_sessions = self.max_for_model(&model_id);
        self.capacity
            .entry(model_id.clone())
            .or_insert_with(|| ModelCapacity {
                active: 1,
                max: max_sessions,
            });

        let session_cancel = CancellationToken::new();
        let kickoff = GooseMessage::user().with_text(&summary);
        let join = spawn_reply_task(
            agent,
            new_goose_session_id.clone(),
            task_id.clone(),
            project_path,
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
                session_id: new_goose_session_id,
                task_id: task_id.clone(),
                worktree_path: Some(worktree_path),
                started_at: Instant::now(),
            },
        );
        self.session_models.insert(task_id.clone(), model_id);
        self.session_projects.insert(task_id.clone(), project_id);
        self.task_session_records
            .insert(task_id.clone(), new_record_id);
        self.session_agent_types.insert(task_id.clone(), agent_type);

        tracing::info!(task_id = %task_id, "Supervisor: compaction complete, new session registered");
    }

    pub(super) async fn handle_compaction_aborted(
        &mut self,
        task_id: String,
        model_id: String,
        agent_type: AgentType,
        worktree_path: Option<PathBuf>,
    ) {
        self.compacting_tasks.remove(&task_id);
        self.decrement_capacity_for_model(Some(&model_id));

        if let Some(ref wp) = worktree_path {
            self.commit_wip_if_needed(&task_id, wp).await;
        }

        tracing::warn!(
            task_id = %task_id,
            "Supervisor: compaction aborted; releasing task back to open"
        );
        self.transition_interrupted(&task_id, agent_type, "compaction aborted")
            .await;
    }
}
