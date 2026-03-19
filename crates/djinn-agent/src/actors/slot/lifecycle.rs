use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::commands::run_commands;
use crate::context::AgentContext;
use crate::message::{Conversation, Message};
use crate::prompts::TaskContext;
use crate::provider::create_provider;
use crate::roles::AgentRole;
use crate::verification::settings::load_commands;
use djinn_core::models::SessionStatus;
use djinn_core::models::TransitionAction;
use djinn_db::SessionRepository;
use djinn_db::TaskRepository;
use djinn_db::repositories::session::CreateSessionParams;

use super::reply_loop::{ReplyLoopContext, run_reply_loop};
use super::*;
use crate::task_merge::interrupt_paused_worker_session;

/// Standalone async function that runs the full per-task lifecycle:
/// load -> worktree -> session -> reply loop -> post-session work -> cleanup.
/// Verification runs as a separate background task after the slot is freed.
///
/// Compaction is handled as an inline loop (no supervisor messages). The reply
/// loop returns its result directly instead of sending SessionCompleted back to
/// an actor.
///
/// Sends `SlotEvent::Free` on normal completion and `SlotEvent::Killed` when
/// cancelled via `cancel`.
pub(crate) struct TaskLifecycleParams {
    pub task_id: String,
    pub project_path: String,
    pub model_id: String,
    pub role: Arc<dyn AgentRole>,
    pub app_state: AgentContext,
    pub cancel: CancellationToken,
    pub pause: CancellationToken,
    pub event_tx: mpsc::Sender<SlotEvent>,
}

pub(crate) async fn run_task_lifecycle(params: TaskLifecycleParams) -> anyhow::Result<()> {
    let TaskLifecycleParams {
        task_id,
        project_path,
        model_id,
        role,
        app_state,
        cancel,
        pause,
        event_tx,
    } = params;
    let emit_step = |task_id: &str, step: &str, detail: serde_json::Value| {
        app_state
            .event_bus
            .send(djinn_core::events::DjinnEventEnvelope::task_lifecycle_step(
                task_id, step, &detail,
            ));
    };

    // Helper macros for early-exit slot events. These send to a dummy channel
    // (slot_id 0 is never a real slot). The authoritative SlotEvent::Free /
    // SlotEvent::Killed is emitted by SlotActor::emit_completion_event after
    // the lifecycle future resolves.
    macro_rules! return_free {
        () => {{
            let _ = event_tx
                .send(SlotEvent::Free {
                    slot_id: 0,
                    model_id: model_id.clone(),
                    task_id: task_id.clone(),
                })
                .await;
            return Ok(());
        }};
    }
    macro_rules! return_killed {
        () => {{
            let _ = event_tx
                .send(SlotEvent::Killed {
                    slot_id: 0,
                    model_id: model_id.clone(),
                    task_id: task_id.clone(),
                })
                .await;
            return Ok(());
        }};
    }

    if cancel.is_cancelled() {
        return_killed!();
    }
    if pause.is_cancelled() {
        return_free!();
    }

    // ── Load task ──────────────────────────────────────────────────────────────
    let task = match load_task(&task_id, &app_state).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to load task");
            return_free!();
        }
    };
    let conflict_ctx = conflict_context_for_dispatch(&task.id, &app_state).await;
    let merge_validation_ctx = merge_validation_context_for_dispatch(&task.id, &app_state).await;

    tracing::info!(
        task_id = %task.short_id,
        task_uuid = %task.id,
        project_id = %task.project_id,
        model_id = %model_id,
        role = %role.config().name,
        task_status = %task.status,
        has_conflict_context = conflict_ctx.is_some(),
        has_merge_validation_context = merge_validation_ctx.is_some(),
        "Lifecycle: dispatch accepted; preparing session"
    );

    // ── Transition task to in-progress ────────────────────────────────────────
    if let Err(e) = transition_start(&task, role.config().start_action, &app_state).await {
        tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: transition_start failed");
        return_free!();
    }

    // Notify the frontend immediately so it can show the agent avatar while
    // worktree/setup is still running.
    app_state
        .event_bus
        .send(djinn_core::events::DjinnEventEnvelope::session_dispatched(
            &task.project_id,
            &task.id,
            &model_id,
            role.config().name,
        ));
    tracing::info!(
        task_id = %task_id,
        "Lifecycle: emitted session.dispatched SSE event"
    );

    // ── Parse model ID and load credentials ───────────────────────────────────
    let (catalog_provider_id, model_name) = match parse_model_id(&model_id) {
        Ok((provider_id, name)) => {
            // Settings may store display names (e.g. "GPT-5.3 Codex") or
            // bare suffixes (e.g. "GLM-4.7" for internal "hf:zai-org/GLM-4.7").
            // Resolve to the actual model ID for the provider API.
            let resolved = app_state
                .catalog
                .list_models(&provider_id)
                .iter()
                .find(|m| {
                    let bare = m.id.rsplit('/').next().unwrap_or(&m.id);
                    m.id == name || m.name == name || bare == name
                })
                .map(|m| m.id.clone())
                .unwrap_or(name);
            (provider_id, resolved)
        }
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: invalid model ID");
            transition_interrupted(
                &task_id,
                role.config().release_action,
                &e.to_string(),
                &app_state,
            )
            .await;
            return_free!();
        }
    };
    emit_step(
        &task.id,
        "credential_loading",
        serde_json::json!({"provider_id": catalog_provider_id}),
    );
    let provider_credential = match load_provider_credential(&catalog_provider_id, &app_state).await
    {
        Ok(cred) => cred,
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: missing credential");
            transition_interrupted(
                &task_id,
                role.config().release_action,
                &e.to_string(),
                &app_state,
            )
            .await;
            return_free!();
        }
    };

    // ── Prepare worktree / paused-session resume context ──────────────────────
    let project_dir = PathBuf::from(&project_path);

    let paused = find_paused_session_record(&task_id, role.config().name, &app_state).await;

    // `resume_record_id` is set when we can resume a paused worker session
    // (same model, same agent type, worktree intact, conversation file present).
    let mut resume_record_id: Option<String> = None;

    emit_step(&task.id, "worktree_creating", serde_json::json!({}));
    emit_step(&task.id, "branch_creating", serde_json::json!({}));
    let mut worktree_conflict_files: Option<Vec<String>> = None;
    let worktree_path = if let Some(paused) = paused {
        if let Some(paused_worktree_path) = paused.worktree_path.as_deref().map(PathBuf::from) {
            if paused.model_id != model_id {
                tracing::info!(
                    task_id = %task_id,
                    paused_model_id = %paused.model_id,
                    requested_model_id = %model_id,
                    "Lifecycle: paused session model mismatch; starting fresh session"
                );
                match prepare_worktree(&project_dir, &task, &app_state).await {
                    Ok((p, cf)) => { worktree_conflict_files = cf; p },
                    Err(e) => {
                        tracing::error!(task_id = %task_id, error = %e, "Lifecycle: prepare_worktree failed; leaving task in_progress for stuck-detector recovery");
                        return_free!();
                    }
                }
            } else if paused.agent_type != role.config().name {
                tracing::info!(
                    task_id = %task_id,
                    paused_agent_type = %paused.agent_type,
                    needed_agent_type = %role.config().name,
                    "Lifecycle: paused session agent type mismatch; starting fresh session"
                );
                match prepare_worktree(&project_dir, &task, &app_state).await {
                    Ok((p, cf)) => { worktree_conflict_files = cf; p },
                    Err(e) => {
                        tracing::error!(task_id = %task_id, error = %e, "Lifecycle: prepare_worktree failed; leaving task in_progress for stuck-detector recovery");
                        return_free!();
                    }
                }
            } else if !paused_worktree_path.exists() || !paused_worktree_path.is_dir() {
                let session_repo =
                    SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
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
                    session_record_id = %paused.id,
                    worktree = %paused_worktree_path.display(),
                    "Lifecycle: paused session worktree missing; finalized as interrupted"
                );
                match prepare_worktree(&project_dir, &task, &app_state).await {
                    Ok((p, cf)) => { worktree_conflict_files = cf; p },
                    Err(e) => {
                        tracing::error!(task_id = %task_id, error = %e, "Lifecycle: prepare_worktree failed; leaving task in_progress for stuck-detector recovery");
                        return_free!();
                    }
                }
            } else {
                // Model match, worktree intact — resume the paused session
                // instead of starting fresh (agent_type already filtered by query).
                tracing::info!(
                    task_id = %task_id,
                    session_record_id = %paused.id,
                    "Lifecycle: resuming paused session; reusing worktree"
                );
                resume_record_id = Some(paused.id);
                paused_worktree_path
            }
        } else {
            tracing::warn!(task_id = %task_id, session_record_id = %paused.id, "Lifecycle: paused session missing worktree; starting fresh session");
            match prepare_worktree(&project_dir, &task, &app_state).await {
                Ok((p, cf)) => { worktree_conflict_files = cf; p },
                Err(e) => {
                    tracing::error!(task_id = %task_id, error = %e, "Lifecycle: prepare_worktree failed; leaving task in_progress for stuck-detector recovery");
                    return_free!();
                }
            }
        }
    } else {
        match prepare_worktree(&project_dir, &task, &app_state).await {
            Ok((p, cf)) => { worktree_conflict_files = cf; p },
            Err(e) => {
                // Do NOT call transition_interrupted here — that would release
                // the task back to "open" immediately, and return_free!() would
                // trigger redispatch, creating a tight infinite loop when
                // prepare_worktree keeps failing (e.g. concurrent git ops racing).
                // Instead, leave the task in "in_progress" so the coordinator's
                // 30-second stuck-task detector releases it with natural backoff.
                tracing::error!(task_id = %task_id, error = %e, "Lifecycle: prepare_worktree failed; leaving task in_progress for stuck-detector recovery");
                return_free!();
            }
        }
    };

    // ── Persist worktree rebase conflict metadata if detected ────────────────
    if let Some(ref conflict_files) = worktree_conflict_files {
        let target_branch = default_target_branch(&task.project_id, &app_state).await;
        let meta = serde_json::json!({
            "conflicting_files": conflict_files,
            "base_branch": format!("task/{}", task.short_id),
            "merge_target": target_branch,
        });
        let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
        if let Err(e) = task_repo
            .set_merge_conflict_metadata(&task.id, Some(&meta.to_string()))
            .await
        {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to persist merge conflict metadata");
        }
    }

    // ── Role-specific worktree preparation (e.g. conflict resolver merge) ────
    emit_step(
        &task.id,
        "worktree_created",
        serde_json::json!({"path": worktree_path.display().to_string()}),
    );
    emit_step(&task.id, "branch_created", serde_json::json!({}));

    let _ = role
        .prepare_worktree(&worktree_path, &task, &app_state)
        .await;

    emit_step(&task.id, "preflight_checking", serde_json::json!({}));
    if !worktree_path.exists() || !worktree_path.is_dir() {
        let diag = runtime_fs_diagnostics(&project_path, &worktree_path);
        tracing::warn!(task_id = %task_id, diag = %diag, "Lifecycle: worktree preflight failed");
        transition_interrupted(
            &task_id,
            role.config().release_action,
            "worktree preflight failed",
            &app_state,
        )
        .await;
        return_free!();
    }
    emit_step(&task.id, "preflight_passed", serde_json::json!({}));

    // ── Run setup commands before session ─────────────────────────────────────
    let (prompt_setup_commands, prompt_verification_commands) = {
        let (setup_specs, verification_specs) = load_commands(&worktree_path).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "failed to load project commands, using empty");
            (Vec::new(), Vec::new())
        });
        let prompt_setup_commands = format_command_details(&setup_specs);
        let prompt_verification_commands = format_command_details(&verification_specs);
        if !setup_specs.is_empty() {
            let setup_start = std::time::Instant::now();
            tracing::info!(
                task_id = %task.short_id,
                command_count = setup_specs.len(),
                "Lifecycle: running setup commands"
            );
            let mut setup_results = Vec::new();
            let mut setup_error: Option<anyhow::Error> = None;
            for spec in &setup_specs {
                emit_step(
                    &task.id,
                    "setup_command_started",
                    serde_json::json!({"name": spec.name, "command": spec.command}),
                );
                match run_commands(std::slice::from_ref(spec), &worktree_path).await {
                    Ok(mut results) => {
                        if let Some(result) = results.pop() {
                            let status = if result.exit_code == 0 { "ok" } else { "error" };
                            emit_step(
                                &task.id,
                                "setup_command_finished",
                                serde_json::json!({"name": result.name, "status": status, "exit_code": result.exit_code}),
                            );
                            setup_results.push(result);
                            if status == "error" {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        emit_step(
                            &task.id,
                            "setup_command_finished",
                            serde_json::json!({"name": spec.name, "status": "error", "error": e.to_string()}),
                        );
                        setup_error = Some(e);
                        break;
                    }
                }
            }

            match setup_error {
                Some(e) => {
                    let reason = format!("Setup commands error: {e}");
                    tracing::warn!(task_id = %task.short_id, error = %e, "Lifecycle: setup command error");
                    let task_repo =
                        TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
                    let _ = task_repo
                        .transition(
                            &task.id,
                            (role.config().release_action)(),
                            "agent-supervisor",
                            "system",
                            Some(&reason),
                            None,
                        )
                        .await;
                    teardown_worktree(
                        &task.short_id,
                        &worktree_path,
                        &project_dir,
                        &app_state,
                        false,
                    )
                    .await;
                    return_free!();
                }
                None => {
                    crate::actors::slot::commands::log_commands_run_event(
                        &task.id,
                        "setup",
                        &setup_specs,
                        &setup_results,
                        &app_state,
                    )
                    .await;
                    let failed = setup_results.iter().find(|r| r.exit_code != 0);
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
                            "Lifecycle: setup command failed; releasing task"
                        );
                        let task_repo =
                            TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
                        let _ = task_repo
                            .transition(
                                &task.id,
                                (role.config().release_action)(),
                                "agent-supervisor",
                                "system",
                                Some(&reason),
                                None,
                            )
                            .await;
                        teardown_worktree(
                            &task.short_id,
                            &worktree_path,
                            &project_dir,
                            &app_state,
                            false,
                        )
                        .await;
                        return_free!();
                    }
                    tracing::info!(
                        task_id = %task.short_id,
                        duration_ms = setup_start.elapsed().as_millis(),
                        "Lifecycle: setup commands completed"
                    );
                }
            }
        }
        (prompt_setup_commands, prompt_verification_commands)
    };

    let conflict_files = conflict_ctx.as_ref().map(|m| {
        m.conflicting_files
            .iter()
            .map(|f| format!("- {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    });

    // Fetch activity log for the prompt: last 3 high-signal comments plus a
    // summary of total counts by role so the agent knows what to look up.
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let activity_text = match task_repo.list_activity(&task.id).await {
        Ok(entries) if !entries.is_empty() => {
            // Last 3 high-signal comments (PM, reviewer, verification)
            let feedback = recent_feedback(&entries, 3);

            // Count comments by role for the summary line
            let mut counts: std::collections::BTreeMap<&str, usize> =
                std::collections::BTreeMap::new();
            for e in &entries {
                if e.event_type == "comment" {
                    *counts.entry(e.actor_role.as_str()).or_default() += 1;
                }
            }
            let count_summary: String = counts
                .iter()
                .map(|(role, n)| format!("{n} {role}"))
                .collect::<Vec<_>>()
                .join(", ");

            let mut parts = Vec::new();
            if !feedback.is_empty() {
                parts.push(format!(
                    "**Recent feedback (newest last):**\n{}",
                    feedback.join("\n\n---\n")
                ));
            }
            if !count_summary.is_empty() {
                parts.push(format!(
                    "**Activity totals:** {count_summary} comments. Use `task_activity_list` with `actor_role` filter for full history."
                ));
            }

            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n\n"))
            }
        }
        _ => None,
    };

    // ── Build epic context for roles that need it (e.g. PM) ─────────────────
    let epic_context = if role.needs_epic_context() {
        if let Some(ref epic_id) = task.epic_id {
            let epic_repo =
                djinn_db::EpicRepository::new(app_state.db.clone(), app_state.event_bus.clone());
            let task_repo_ctx =
                TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
            match epic_repo.get(epic_id).await {
                Ok(Some(epic)) => {
                    let mut ctx_lines = vec![
                        format!("**Epic:** {} ({})", epic.title, epic.short_id),
                        format!("**Description:** {}", epic.description),
                        format!("**Memory refs:** {}", epic.memory_refs),
                    ];
                    // Load sibling tasks
                    if let Ok(result) = task_repo_ctx
                        .list_filtered(djinn_db::ListQuery {
                            parent: Some(epic_id.clone()),
                            limit: 50,
                            ..Default::default()
                        })
                        .await
                    {
                        let open = result.tasks.iter().filter(|t| t.status != "closed").count();
                        let closed = result.tasks.iter().filter(|t| t.status == "closed").count();
                        ctx_lines.push(format!(
                            "\n### Sibling Tasks ({open} open, {closed} closed)"
                        ));
                        for t in &result.tasks {
                            let status_marker = if t.status == "closed" {
                                "closed"
                            } else {
                                &t.status
                            };
                            ctx_lines
                                .push(format!("- [{}] {}: {}", status_marker, t.short_id, t.title));
                        }
                    }
                    Some(ctx_lines.join("\n"))
                }
                _ => None,
            }
        } else {
            None
        }
    } else {
        None
    };

    let system_prompt = role.render_prompt(
        &task,
        &TaskContext {
            project_path: project_path.clone(),
            workspace_path: worktree_path.display().to_string(),
            diff: None,
            commits: None,
            start_commit: None,
            end_commit: None,
            conflict_files,
            merge_base_branch: conflict_ctx.as_ref().map(|m| m.base_branch.clone()),
            merge_target_branch: conflict_ctx.as_ref().map(|m| m.merge_target.clone()),
            merge_failure_context: merge_validation_ctx,
            setup_commands: prompt_setup_commands.clone(),
            verification_commands: prompt_verification_commands.clone(),
            activity: activity_text,
            epic_context,
        },
    );

    let context_window = app_state
        .catalog
        .find_model(&model_id)
        .map(|m| m.context_window)
        .unwrap_or(0);

    let session_repo = SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());

    // Use the resume session ID or a pre-generated UUID as the provider
    // affinity key.  The actual DB session record is created later (once we
    // know whether we're resuming or starting fresh) to avoid orphaning a
    // ghost record when the second creation shadows this one.
    let affinity_key = resume_record_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());

    // ── Build Djinn-native provider ───────────────────────────────────────────
    let telemetry_meta = build_telemetry_meta(role.config().name, &task_id);

    let provider_config = match provider_credential {
        ProviderCredential::OAuthConfig(mut cfg) => {
            // OAuth config carries defaults; override model_id and context_window
            // from the dispatch request.
            cfg.model_id = model_name.clone();
            cfg.context_window = context_window.max(0) as u32;
            cfg.telemetry = Some(telemetry_meta);
            cfg.session_affinity_key = Some(affinity_key.clone());
            *cfg
        }
        ProviderCredential::ApiKey(_key_name, api_key) => {
            let format_family = format_family_for_provider(&catalog_provider_id, &model_name);
            // Prefer the catalog's base_url (handles custom providers like
            // "synthetic"); fall back to the hardcoded defaults for well-known
            // providers that may not carry a base_url in the catalog.
            let base_url = app_state
                .catalog
                .list_providers()
                .iter()
                .find(|p| p.id == catalog_provider_id)
                .map(|p| p.base_url.clone())
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| default_base_url(&catalog_provider_id));
            crate::provider::ProviderConfig {
                base_url,
                auth: auth_method_for_provider(&catalog_provider_id, &api_key),
                format_family,
                model_id: model_name.clone(),
                context_window: context_window.max(0) as u32,
                telemetry: Some(telemetry_meta),
                session_affinity_key: Some(affinity_key.clone()),
                provider_headers: Default::default(),
                capabilities: capabilities_for_provider(&catalog_provider_id),
            }
        }
    };
    let provider = create_provider(provider_config);

    // ── Create or resume session record + build conversation ─────────────────
    let tools = (role.config().tool_schemas)();

    // Workers include recent feedback in the initial message; other roles use
    // a generic kickoff (they read activity via tools themselves).
    let fresh_user_message = role.initial_user_message(&task_id, &app_state).await;

    // Try to resume from a paused session's saved conversation.
    emit_step(
        &task.id,
        "session_creating",
        serde_json::json!({"resume": resume_record_id.is_some()}),
    );
    let (current_record_id, mut conversation) = if let Some(ref resume_id) = resume_record_id {
        match super::conversation_store::load(resume_id).await {
            Ok(Some(mut saved_conv)) => {
                // Replace the system prompt with a fresh one (reflects updated AC).
                if !saved_conv.messages.is_empty()
                    && saved_conv.messages[0].role == crate::message::Role::System
                {
                    saved_conv.messages[0] = Message::system(system_prompt.clone());
                }

                // Compact the prior conversation before appending feedback.
                // This strips the model's "I'm done" messages and frees context
                // window for actual work, while preserving research/context.
                let pre_compact_len = saved_conv.messages.len();
                let compacted = crate::compaction::compact_conversation(
                    provider.as_ref(),
                    &mut saved_conv,
                    resume_id,
                    &task_id,
                    &app_state,
                    crate::compaction::CompactionContext::PreResume(role.config().name.to_string()),
                    context_window,
                )
                .await;
                tracing::info!(
                    task_id = %task_id,
                    session_record_id = %resume_id,
                    pre_compact_len,
                    post_compact_len = saved_conv.messages.len(),
                    compacted,
                    "Lifecycle: compacted conversation before resume"
                );

                // Append reviewer feedback as the fresh user message.
                let feedback = resume_context_for_task(&task_id, &app_state).await;
                saved_conv.push(Message::user(feedback));

                // Reuse the paused session record.
                session_repo.set_running(resume_id).await.ok();
                tracing::info!(
                    task_id = %task_id,
                    session_record_id = %resume_id,
                    conversation_len = saved_conv.messages.len(),
                    "Lifecycle: resumed paused session with reviewer feedback"
                );
                (Some(resume_id.clone()), saved_conv)
            }
            Ok(None) | Err(_) => {
                // Conversation file missing/corrupt — fall back to fresh session.
                tracing::warn!(
                    task_id = %task_id,
                    session_record_id = %resume_id,
                    "Lifecycle: conversation file missing; falling back to fresh session"
                );
                // Mark the stale paused session as interrupted.
                let _ = session_repo
                    .update(resume_id, SessionStatus::Interrupted, 0, 0)
                    .await;
                let record_id = match session_repo
                    .create(CreateSessionParams {
                        project_id: &task.project_id,
                        task_id: Some(&task.id),
                        model: &model_id,
                        agent_type: role.config().name,
                        worktree_path: worktree_path.to_str(),
                        metadata_json: None,
                    })
                    .await
                {
                    Ok(r) => Some(r.id),
                    Err(e) => {
                        tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to create session record");
                        transition_interrupted(
                            &task_id,
                            role.config().release_action,
                            &e.to_string(),
                            &app_state,
                        )
                        .await;
                        teardown_worktree(
                            &task.short_id,
                            &worktree_path,
                            &project_dir,
                            &app_state,
                            false,
                        )
                        .await;
                        return_free!();
                    }
                };
                let mut conv = Conversation::new();
                conv.push(Message::system(system_prompt.clone()));
                conv.push(Message::user(fresh_user_message.clone()));
                (record_id, conv)
            }
        }
    } else {
        // Fresh session — no paused session to resume.
        let record_id = match session_repo
            .create(CreateSessionParams {
                project_id: &task.project_id,
                task_id: Some(&task.id),
                model: &model_id,
                agent_type: role.config().name,
                worktree_path: worktree_path.to_str(),
                metadata_json: None,
            })
            .await
        {
            Ok(r) => Some(r.id),
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to create session record");
                transition_interrupted(
                    &task_id,
                    role.config().release_action,
                    &e.to_string(),
                    &app_state,
                )
                .await;
                teardown_worktree(
                    &task.short_id,
                    &worktree_path,
                    &project_dir,
                    &app_state,
                    false,
                )
                .await;
                return_free!();
            }
        };
        let mut conv = Conversation::new();
        conv.push(Message::system(system_prompt.clone()));
        conv.push(Message::user(fresh_user_message));
        (record_id, conv)
    };

    // Use the DB record ID as the session ID so OTel/Langfuse traces, error
    // diagnostics, and session_messages all reference the same identifier.
    let current_session_id = current_record_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());

    // ── Run reply loop ────────────────────────────────────────────────────────
    let (reply_result, final_output, tokens_in_loop, tokens_out_loop) = run_reply_loop(
        ReplyLoopContext {
            provider: provider.as_ref(),
            tools: &tools,
            task_id: &task.id,
            task_short_id: &task.short_id,
            session_id: &current_session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: role.config().name,
            finalize_tool_names: role.config().finalize_tool_names,
            context_window,
            model_id: &model_id,
            cancel: &cancel,
            global_cancel: &pause,
            app_state: &app_state,
        },
        &mut conversation,
        resume_record_id.is_some(),
    )
    .await;

    // Persist conversation messages to session_messages table for timeline display.
    // Compaction already saves pre-compaction messages; this saves whatever remains
    // (post-compaction turns, or the full conversation if no compaction occurred).
    if let Some(ref record_id) = current_record_id {
        let msg_repo = djinn_db::SessionMessageRepository::new(
            app_state.db.clone(),
            app_state.event_bus.clone(),
        );
        if let Err(e) = msg_repo
            .insert_messages_batch(record_id, &task.id, &conversation.messages)
            .await
        {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to persist conversation messages to DB");
        }
    }

    // Always commit whatever the agent wrote before verification or cleanup.
    commit_wip_if_needed(&task_id, &worktree_path, &app_state).await;

    // ── Handle pause/kill cancellation ────────────────────────────────────────
    if pause.is_cancelled() {
        tracing::info!(task_id = %task_id, "Lifecycle: paused; preserving worktree");
        update_session_record_paused(
            current_record_id.as_deref(),
            tokens_in_loop,
            tokens_out_loop,
            &app_state,
        )
        .await;
        // Worktree is preserved for resume, but LSP clients are demand-spawned
        // so they'll be re-created when the session resumes and touches files.
        app_state.lsp.shutdown_for_worktree(&worktree_path).await;
        return_free!();
    }
    if cancel.is_cancelled() {
        tracing::info!(task_id = %task_id, "Lifecycle: cancelled; cleaning up");
        update_session_record(
            current_record_id.as_deref(),
            SessionStatus::Interrupted,
            tokens_in_loop,
            tokens_out_loop,
            &app_state,
        )
        .await;
        teardown_worktree(
            &task.short_id,
            &worktree_path,
            &project_dir,
            &app_state,
            false,
        )
        .await;
        transition_interrupted(
            &task_id,
            role.config().release_action,
            "session cancelled",
            &app_state,
        )
        .await;
        return_killed!();
    }

    let final_result = reply_result;
    let tokens_in = tokens_in_loop;
    let tokens_out = tokens_out_loop;

    // ── Post-loop: health + transitions + cleanup ─────────────────────────────

    // Health tracking.
    match &final_result {
        Ok(()) => app_state.health_tracker.record_success(&model_id),
        Err(_) => app_state.health_tracker.record_failure(&model_id),
    }
    app_state.persist_model_health_state().await;

    let is_worker_done = final_result.is_ok() && role.config().preserves_session;

    // Worktree: commit final work.  For workers, preserve the worktree and
    // save the conversation so the session can be resumed after review.
    // Non-workers (reviewers, PM) still clean up immediately.
    if is_worker_done
        && let Err(e) = commit_final_work_if_needed(&task_id, &worktree_path, &app_state).await
    {
        tracing::warn!(
            task_id = %task_id,
            error = %e,
            "Lifecycle: failed to commit final work"
        );
    }
    if is_worker_done {
        // Save conversation for potential resume after review cycle.
        if let Some(ref record_id) = current_record_id
            && let Err(e) = super::conversation_store::save(record_id, &conversation).await
        {
            tracing::warn!(
                task_id = %task_id,
                record_id = %record_id,
                error = %e,
                "Lifecycle: failed to save conversation for resume"
            );
        }
        // Mark session as Paused (not Completed) — worker may resume.
        update_session_record_paused(
            current_record_id.as_deref(),
            tokens_in,
            tokens_out,
            &app_state,
        )
        .await;
        // Don't clean up worktree — will be reused on resume.
        // LSP clients are demand-spawned; shut them down now to free resources.
        // They'll be re-created when the session resumes and touches files.
        app_state.lsp.shutdown_for_worktree(&worktree_path).await;
    } else {
        // Non-worker or failed: close session and clean up.
        let session_status = if final_result.is_ok() {
            SessionStatus::Completed
        } else {
            SessionStatus::Failed
        };
        update_session_record(
            current_record_id.as_deref(),
            session_status,
            tokens_in,
            tokens_out,
            &app_state,
        )
        .await;
        teardown_worktree(
            &task.short_id,
            &worktree_path,
            &project_dir,
            &app_state,
            false,
        )
        .await;

        // For non-worker roles, free the slot immediately and run
        // post-session work (finalize payload, on_complete, transition) in a
        // background task.  This prevents slow operations like merge
        // verification from blocking a slot while no LLM session is active.
        let final_error = final_result.as_ref().err().map(|e| e.to_string());
        let final_result_ok = final_result.is_ok();
        spawn_post_session_work(PostSessionParams {
            task_id: task_id.clone(),
            project_path: project_path.clone(),
            role: role.clone(),
            app_state: app_state.clone(),
            final_output,
            final_result_ok,
            final_error,
            tokens_in,
            tokens_out,
        });
        return_free!();
    }

    // ── Worker path: inline post-session (workers don't do merges) ────────────

    // Log reviewer feedback from text markers — only when no finalize payload is
    // present. With ADR-036, reviewer feedback comes via submit_review.feedback
    // and is logged by process_finalize_payload below.
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    if final_output.finalize_payload.is_none()
        && let Some(feedback) = final_output.reviewer_feedback.as_deref()
    {
        let payload = serde_json::json!({ "body": feedback }).to_string();
        if let Err(e) = task_repo
            .log_activity(
                Some(&task_id),
                "agent-supervisor",
                "reviewer",
                "comment",
                &payload,
            )
            .await
        {
            tracing::warn!(task_id = %task_id, error = %e, "failed to store reviewer feedback comment");
        }
    }

    // Process finalize tool payload (ADR-036): log structured activity and apply
    // side effects (e.g. AC updates for submit_review) before on_complete runs.
    if final_result.is_ok() {
        super::finalize_handlers::process_finalize_payload(
            &final_output.finalize_payload,
            final_output.finalize_tool_name.as_deref().unwrap_or(""),
            &task_id,
            &app_state,
        )
        .await;
    }

    // Log session errors.
    if let Err(reason) = &final_result {
        let payload = serde_json::json!({
            "error": reason.to_string(),
            "agent_type": role.config().name,
        })
        .to_string();
        let _ = task_repo
            .log_activity(
                Some(&task_id),
                "agent-supervisor",
                "system",
                "session_error",
                &payload,
            )
            .await;
    }
    if final_result.is_ok()
        && let Some(reason) = final_output.runtime_error.as_deref()
    {
        let payload = serde_json::json!({
            "error": reason,
            "agent_type": role.config().name,
        })
        .to_string();
        let _ = task_repo
            .log_activity(
                Some(&task_id),
                "agent-supervisor",
                "system",
                "session_error",
                &payload,
            )
            .await;
    }

    // Determine transition.
    let transition = match final_result {
        Ok(()) => role.on_complete(&task_id, &final_output, &app_state).await,
        Err(reason) => Some(((role.config().release_action)(), Some(reason.to_string()))),
    };

    apply_transition_and_dispatch(
        transition,
        &task_id,
        &project_path,
        &role,
        &app_state,
        tokens_in,
        tokens_out,
    )
    .await;

    return_free!();
}

// ─── Background post-session work (non-worker roles) ─────────────────────────

/// Parameters for the background post-session task that runs after the slot is
/// freed.  Handles finalize payload processing, on_complete (which may do slow
/// merge + verification), transition, and dispatch triggering.
struct PostSessionParams {
    task_id: String,
    project_path: String,
    role: Arc<dyn AgentRole>,
    app_state: AgentContext,
    final_output: crate::output_parser::ParsedAgentOutput,
    final_result_ok: bool,
    final_error: Option<String>,
    tokens_in: i64,
    tokens_out: i64,
}

/// Spawn the post-session work as a background tokio task so the slot is freed
/// immediately after the LLM session ends.
fn spawn_post_session_work(params: PostSessionParams) {
    // Register in the verification tracker so the coordinator's stuck-task
    // recovery doesn't reset the task while post-session work (merge,
    // transition) is still in flight.
    params.app_state.register_verification(&params.task_id);
    tokio::spawn(async move {
        let PostSessionParams {
            task_id,
            project_path,
            role,
            app_state,
            final_output,
            final_result_ok,
            final_error,
            tokens_in,
            tokens_out,
        } = params;

        // Log reviewer feedback from text markers.
        let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
        if final_output.finalize_payload.is_none()
            && let Some(feedback) = final_output.reviewer_feedback.as_deref()
        {
            let payload = serde_json::json!({ "body": feedback }).to_string();
            if let Err(e) = task_repo
                .log_activity(
                    Some(&task_id),
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

        // Process finalize tool payload (ADR-036).
        if final_result_ok {
            super::finalize_handlers::process_finalize_payload(
                &final_output.finalize_payload,
                final_output.finalize_tool_name.as_deref().unwrap_or(""),
                &task_id,
                &app_state,
            )
            .await;
        }

        // Log session errors.
        if let Some(reason) = &final_error {
            let payload = serde_json::json!({
                "error": reason,
                "agent_type": role.config().name,
            })
            .to_string();
            let _ = task_repo
                .log_activity(
                    Some(&task_id),
                    "agent-supervisor",
                    "system",
                    "session_error",
                    &payload,
                )
                .await;
        }
        if final_result_ok && let Some(reason) = final_output.runtime_error.as_deref() {
            let payload = serde_json::json!({
                "error": reason,
                "agent_type": role.config().name,
            })
            .to_string();
            let _ = task_repo
                .log_activity(
                    Some(&task_id),
                    "agent-supervisor",
                    "system",
                    "session_error",
                    &payload,
                )
                .await;
        }

        // Determine transition.
        let transition = if final_result_ok {
            role.on_complete(&task_id, &final_output, &app_state).await
        } else if let Some(reason) = final_error {
            Some(((role.config().release_action)(), Some(reason)))
        } else {
            Some(((role.config().release_action)(), None))
        };

        apply_transition_and_dispatch(
            transition,
            &task_id,
            &project_path,
            &role,
            &app_state,
            tokens_in,
            tokens_out,
        )
        .await;

        // Deregister from the verification tracker now that all post-session
        // work (finalize payload, on_complete, transition, merge) is done.
        app_state.deregister_verification(&task_id);
    });
}

/// Apply the transition from on_complete and trigger dispatch for the project.
/// Shared by both the inline worker path and the background non-worker path.
async fn apply_transition_and_dispatch(
    transition: Option<(TransitionAction, Option<String>)>,
    task_id: &str,
    project_path: &str,
    role: &Arc<dyn AgentRole>,
    app_state: &AgentContext,
    tokens_in: i64,
    tokens_out: i64,
) {
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());

    if let Some((action, reason)) = transition {
        tracing::info!(
            task_id = %task_id,
            role = %role.config().name,
            transition_action = ?action,
            transition_reason = reason.as_deref().unwrap_or("<none>"),
            tokens_in,
            tokens_out,
            "Lifecycle: applying session transition"
        );
        let is_conflict_rejection = action == TransitionAction::TaskReviewRejectConflict;
        let is_submit_verification = action == TransitionAction::SubmitVerification;
        if let Err(e) = task_repo
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
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to transition task after session");
        }
        if is_conflict_rejection {
            interrupt_paused_worker_session(task_id, app_state).await;
        }
        if is_submit_verification {
            super::verification::spawn_verification(
                task_id.to_string(),
                project_path.to_string(),
                app_state.clone(),
            );
        }
    } else {
        tracing::info!(
            task_id = %task_id,
            role = %role.config().name,
            tokens_in,
            tokens_out,
            "Lifecycle: session completed with no task transition"
        );
    }

    // Trigger dispatcher for the project so the next ready task starts promptly.
    if let Ok(task) = load_task(task_id, app_state).await
        && let Some(coordinator) = app_state.coordinator().await
    {
        let _ = coordinator
            .trigger_dispatch_for_project(&task.project_id)
            .await;
    }
}

pub(crate) struct ProjectLifecycleParams {
    pub(crate) project_id: String,
    pub(crate) project_path: String,
    pub(crate) role: Arc<dyn AgentRole>,
    pub model_id: String,
    pub app_state: AgentContext,
    pub cancel: CancellationToken,
    pub pause: CancellationToken,
    pub event_tx: mpsc::Sender<SlotEvent>,
}

pub async fn run_project_lifecycle(params: ProjectLifecycleParams) -> anyhow::Result<()> {
    let task_id = format!(
        "project:{}:{}",
        params.project_id,
        params.role.config().name
    );
    let model_id = params.model_id;
    let app_state = params.app_state;
    let cancel = params.cancel;
    let pause = params.pause;
    let event_tx = params.event_tx;
    let project_path = params.project_path;
    let project_id = params.project_id;

    let role = params.role;

    // These macros send to a dummy channel (slot_id 0 is intentional).
    // The real SlotEvent is emitted by SlotActor::emit_completion_event.
    macro_rules! return_free {
        () => {{
            let _ = event_tx
                .send(SlotEvent::Free {
                    slot_id: 0,
                    model_id: model_id.clone(),
                    task_id: task_id.clone(),
                })
                .await;
            return Ok(());
        }};
    }
    macro_rules! return_killed {
        () => {{
            let _ = event_tx
                .send(SlotEvent::Killed {
                    slot_id: 0,
                    model_id: model_id.clone(),
                    task_id: task_id.clone(),
                })
                .await;
            return Ok(());
        }};
    }

    if cancel.is_cancelled() {
        return_killed!();
    }
    if pause.is_cancelled() {
        return_free!();
    }

    app_state
        .event_bus
        .send(djinn_core::events::DjinnEventEnvelope::session_dispatched(
            &project_id,
            &task_id,
            &model_id,
            role.config().name,
        ));
    tracing::info!(
        task_id = %task_id,
        "Lifecycle(continuation): emitted session.dispatched SSE event"
    );

    tracing::info!(
        task_id = %task_id,
        project_id = %project_id,
        model_id = %model_id,
        agent_type = %role.config().name,
        "Lifecycle: project-scoped dispatch accepted"
    );

    // ── Parse model ID and load credentials ───────────────────────────────
    let (catalog_provider_id, model_name) = match parse_model_id(&model_id) {
        Ok((provider_id, name)) => {
            let resolved = app_state
                .catalog
                .list_models(&provider_id)
                .iter()
                .find(|m| {
                    let bare = m.id.rsplit('/').next().unwrap_or(&m.id);
                    m.id == name || m.name == name || bare == name
                })
                .map(|m| m.id.clone())
                .unwrap_or(name);
            (provider_id, resolved)
        }
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: invalid model ID");
            return_free!();
        }
    };
    let provider_credential = match load_provider_credential(&catalog_provider_id, &app_state).await
    {
        Ok(cred) => cred,
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: missing credential");
            return_free!();
        }
    };
    let verification_commands = {
        let (_, verification_specs) = load_commands(std::path::Path::new(&project_path))
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "failed to load project commands for planner prompt");
                (Vec::new(), Vec::new())
            });
        helpers::format_command_details(&verification_specs)
    };

    // ── Build prompt ──────────────────────────────────────────────────────
    let system_prompt = crate::prompts::render_project_prompt_for_role(
        role.config(),
        &project_path,
        verification_commands.as_deref(),
    );

    let context_window = app_state
        .catalog
        .find_model(&model_id)
        .map(|m| m.context_window)
        .unwrap_or(0);

    let session_repo = SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let current_record_id = match session_repo
        .create(CreateSessionParams {
            project_id: &project_id,
            task_id: None,
            model: &model_id,
            agent_type: role.config().name,
            worktree_path: Some(&project_path),
            metadata_json: None,
        })
        .await
    {
        Ok(r) => Some(r.id),
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to create session record");
            return_free!();
        }
    };
    let current_session_id = current_record_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());

    // ── Build provider ────────────────────────────────────────────────────
    let telemetry_meta = build_telemetry_meta(role.config().name, &task_id);
    let provider_config = match provider_credential {
        ProviderCredential::OAuthConfig(mut cfg) => {
            cfg.model_id = model_name.clone();
            cfg.context_window = context_window.max(0) as u32;
            cfg.telemetry = Some(telemetry_meta);
            cfg.session_affinity_key = Some(current_session_id.clone());
            *cfg
        }
        ProviderCredential::ApiKey(_key_name, api_key) => {
            let format_family = format_family_for_provider(&catalog_provider_id, &model_name);
            let base_url = app_state
                .catalog
                .list_providers()
                .iter()
                .find(|p| p.id == catalog_provider_id)
                .map(|p| p.base_url.clone())
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| default_base_url(&catalog_provider_id));
            crate::provider::ProviderConfig {
                base_url,
                auth: auth_method_for_provider(&catalog_provider_id, &api_key),
                format_family,
                model_id: model_name.clone(),
                context_window: context_window.max(0) as u32,
                telemetry: Some(telemetry_meta),
                session_affinity_key: Some(current_session_id.clone()),
                provider_headers: Default::default(),
                capabilities: capabilities_for_provider(&catalog_provider_id),
            }
        }
    };
    let provider = create_provider(provider_config);

    // ── Create session record (no task_id for project-scoped agents) ─────
    let tools = (role.config().tool_schemas)();
    let mut conversation = Conversation::new();
    conversation.push(Message::system(system_prompt));
    conversation.push(Message::user(
        "Begin planning the backlog for this project.",
    ));

    let project_dir = PathBuf::from(&project_path);

    // ── Run reply loop ────────────────────────────────────────────────────
    let (reply_result, _final_output, tokens_in, tokens_out) = run_reply_loop(
        ReplyLoopContext {
            provider: provider.as_ref(),
            tools: &tools,
            task_id: &task_id,
            task_short_id: &task_id, // short_id — use task_id for project-scoped
            session_id: &current_session_id,
            project_path: &project_path,
            worktree_path: &project_dir, // worktree = project dir (no worktree for planner)
            role_name: role.config().name,
            finalize_tool_names: role.config().finalize_tool_names,
            context_window,
            model_id: &model_id,
            cancel: &cancel,
            global_cancel: &pause,
            app_state: &app_state,
        },
        &mut conversation,
        false, // not a resumed session
    )
    .await;

    // ── Persist messages ──────────────────────────────────────────────────
    if let Some(ref record_id) = current_record_id {
        let msg_repo = djinn_db::SessionMessageRepository::new(
            app_state.db.clone(),
            app_state.event_bus.clone(),
        );
        let _ = msg_repo
            .insert_messages_batch(record_id, &task_id, &conversation.messages)
            .await;
    }

    // ── Handle pause/kill ─────────────────────────────────────────────────
    if pause.is_cancelled() {
        update_session_record_paused(
            current_record_id.as_deref(),
            tokens_in,
            tokens_out,
            &app_state,
        )
        .await;
        return_free!();
    }
    if cancel.is_cancelled() {
        update_session_record(
            current_record_id.as_deref(),
            SessionStatus::Interrupted,
            tokens_in,
            tokens_out,
            &app_state,
        )
        .await;
        return_killed!();
    }

    // ── Health tracking ───────────────────────────────────────────────────
    match &reply_result {
        Ok(()) => app_state.health_tracker.record_success(&model_id),
        Err(_) => app_state.health_tracker.record_failure(&model_id),
    }
    app_state.persist_model_health_state().await;

    let session_status = if reply_result.is_ok() {
        SessionStatus::Completed
    } else {
        SessionStatus::Failed
    };
    update_session_record(
        current_record_id.as_deref(),
        session_status,
        tokens_in,
        tokens_out,
        &app_state,
    )
    .await;

    return_free!();
}
