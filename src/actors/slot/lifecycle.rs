use std::path::PathBuf;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Truncate a `String` to at most `max_bytes` bytes, rounding down to the
/// nearest UTF-8 char boundary so we never panic.
fn truncate_utf8(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

use crate::agent::extension;
use crate::agent::message::{Conversation, Message};
use crate::agent::prompts::{TaskContext, render_prompt};
use crate::agent::provider::create_provider;
use crate::agent::AgentType;
use crate::commands::{CommandSpec, run_commands};
use crate::db::ProjectRepository;
use crate::db::SessionRepository;
use crate::db::TaskRepository;
use crate::models::SessionStatus;
use crate::models::TransitionAction;
use crate::server::AppState;

use super::*;
use super::reply_loop::run_reply_loop;

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
#[allow(clippy::too_many_arguments)]
pub async fn run_task_lifecycle(
    task_id: String,
    project_path: String,
    model_id: String,
    app_state: AppState,
    cancel: CancellationToken,
    pause: CancellationToken,
    event_tx: mpsc::Sender<SlotEvent>,
) -> anyhow::Result<()> {
    // Helper macros for emitting slot events on exit.
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

    // ── Determine agent type and context ──────────────────────────────────────
    let conflict_ctx = conflict_context_for_dispatch(&task.id, &app_state).await;
    let merge_validation_ctx = merge_validation_context_for_dispatch(&task.id, &app_state).await;
    let agent_type = agent_type_for_task(&task, conflict_ctx.is_some());

    tracing::info!(
        task_id = %task.short_id,
        task_uuid = %task.id,
        project_id = %task.project_id,
        model_id = %model_id,
        agent_type = %agent_type.as_str(),
        task_status = %task.status,
        has_conflict_context = conflict_ctx.is_some(),
        has_merge_validation_context = merge_validation_ctx.is_some(),
        "Lifecycle: dispatch accepted; preparing session"
    );

    // ── Transition task to in-progress ────────────────────────────────────────
    if let Err(e) = transition_start(&task, agent_type, &app_state).await {
        tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: transition_start failed");
        return_free!();
    }

    // Notify the frontend immediately so it can show the agent avatar while
    // worktree/setup is still running.
    let _ = app_state.events().send(crate::events::DjinnEvent::SessionDispatched {
        project_id: task.project_id.clone(),
        task_id: task.id.clone(),
        model_id: model_id.clone(),
        agent_type: agent_type.as_str().to_string(),
    });

    // ── Parse model ID and load credentials ───────────────────────────────────
    let (catalog_provider_id, model_name) = match parse_model_id(&model_id) {
        Ok((provider_id, name)) => {
            // Settings may store display names (e.g. "GPT-5.3 Codex") or
            // bare suffixes (e.g. "GLM-4.7" for internal "hf:zai-org/GLM-4.7").
            // Resolve to the actual model ID for the provider API.
            let resolved = app_state
                .catalog()
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
            transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
            return_free!();
        }
    };
    let provider_credential =
        match load_provider_credential(&catalog_provider_id, &app_state).await {
            Ok(cred) => cred,
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: missing credential");
                transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
                return_free!();
            }
        };

    // ── Prepare worktree / paused-session resume context ──────────────────────
    let project_dir = PathBuf::from(&project_path);

    let paused = find_paused_session_record(&task_id, agent_type, &app_state).await;

    // `resume_record_id` is set when we can resume a paused worker session
    // (same model, same agent type, worktree intact, conversation file present).
    let mut resume_record_id: Option<String> = None;

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
                    Ok(p) => p,
                    Err(e) => {
                        tracing::error!(task_id = %task_id, error = %e, "Lifecycle: prepare_worktree failed; leaving task in_progress for stuck-detector recovery");
                        return_free!();
                    }
                }
            } else if paused.agent_type != agent_type.as_str() {
                tracing::info!(
                    task_id = %task_id,
                    paused_agent_type = %paused.agent_type,
                    needed_agent_type = %agent_type.as_str(),
                    "Lifecycle: paused session agent type mismatch; starting fresh session"
                );
                match prepare_worktree(&project_dir, &task, &app_state).await {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::error!(task_id = %task_id, error = %e, "Lifecycle: prepare_worktree failed; leaving task in_progress for stuck-detector recovery");
                        return_free!();
                    }
                }
            } else if !paused_worktree_path.exists() || !paused_worktree_path.is_dir() {
                let session_repo =
                    SessionRepository::new(app_state.db().clone(), app_state.events().clone());
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
                    Ok(p) => p,
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
                Ok(p) => p,
                Err(e) => {
                    tracing::error!(task_id = %task_id, error = %e, "Lifecycle: prepare_worktree failed; leaving task in_progress for stuck-detector recovery");
                    return_free!();
                }
            }
        }
    } else {
        match prepare_worktree(&project_dir, &task, &app_state).await {
            Ok(p) => p,
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

    // ── Conflict resolver: start merge for conflict markers ───────────────────
    if agent_type == AgentType::ConflictResolver
        && let Some(ref ctx) = conflict_ctx
    {
        let target_ref = format!("origin/{}", ctx.merge_target);
        if let Ok(wt_git) = app_state.git_actor(&worktree_path).await {
            let _ = wt_git
                .run_command(vec![
                    "fetch".into(),
                    "origin".into(),
                    ctx.merge_target.clone(),
                ])
                .await;
            let merge_result = wt_git
                .run_command(vec![
                    "merge".into(),
                    target_ref.clone(),
                    "--no-commit".into(),
                ])
                .await;
            if merge_result.is_ok() {
                let _ = wt_git
                    .run_command(vec!["merge".into(), "--abort".into()])
                    .await;
            } else {
                tracing::info!(
                    task_id = %task.short_id,
                    target_ref = %target_ref,
                    "Lifecycle: started merge in worktree for conflict resolver"
                );
            }
        }
    }

    if !worktree_path.exists() || !worktree_path.is_dir() {
        let diag = runtime_fs_diagnostics(&project_path, &worktree_path);
        tracing::warn!(task_id = %task_id, diag = %diag, "Lifecycle: worktree preflight failed");
        transition_interrupted(
            &task_id,
            agent_type,
            "worktree preflight failed",
            &app_state,
        )
        .await;
        return_free!();
    }

    // ── Project commands ──────────────────────────────────────────────────────
    let project_repo = ProjectRepository::new(app_state.db().clone(), app_state.events().clone());
    let (prompt_setup_commands, prompt_verification_commands) = {
        if let Ok(Some(ref p)) = project_repo.get(&task.project_id).await {
            let setup_names = format_command_names(&p.setup_commands);
            let verify_names = format_command_names(&p.verification_commands);
            (setup_names, verify_names)
        } else {
            (None, None)
        }
    };

    // ── Run setup commands before session ─────────────────────────────────────
    if let Ok(Some(project)) = project_repo.get(&task.project_id).await
    {
        let setup_specs: Vec<CommandSpec> =
            serde_json::from_str(&project.setup_commands).unwrap_or_default();
        if !setup_specs.is_empty() {
            let setup_start = std::time::Instant::now();
            tracing::info!(
                task_id = %task.short_id,
                command_count = setup_specs.len(),
                "Lifecycle: running setup commands"
            );
            let setup_result = run_commands(&setup_specs, &worktree_path).await;
            match setup_result {
                Ok(results) => {
                    crate::actors::slot::commands::log_commands_run_event(
                        &task.id,
                        "setup",
                        &setup_specs,
                        &results,
                        &app_state,
                    )
                    .await;
                    let failed = results.iter().find(|r| r.exit_code != 0);
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
                            TaskRepository::new(app_state.db().clone(), app_state.events().clone());
                        let _ = task_repo
                            .transition(
                                &task.id,
                                agent_type.release_action(),
                                "agent-supervisor",
                                "system",
                                Some(&reason),
                                None,
                            )
                            .await;
                        cleanup_worktree(&task.id, &worktree_path, &app_state).await;
                        return_free!();
                    }
                    tracing::info!(
                        task_id = %task.short_id,
                        duration_ms = setup_start.elapsed().as_millis(),
                        "Lifecycle: setup commands completed"
                    );
                }
                Err(e) => {
                    let reason = format!("Setup commands error: {e}");
                    tracing::warn!(task_id = %task.short_id, error = %e, "Lifecycle: setup command error");
                    let task_repo =
                        TaskRepository::new(app_state.db().clone(), app_state.events().clone());
                    let _ = task_repo
                        .transition(
                            &task.id,
                            agent_type.release_action(),
                            "agent-supervisor",
                            "system",
                            Some(&reason),
                            None,
                        )
                        .await;
                    cleanup_worktree(&task.id, &worktree_path, &app_state).await;
                    return_free!();
                }
            }
        }
    }

    let conflict_files = conflict_ctx.as_ref().map(|m| {
        m.conflicting_files
            .iter()
            .map(|f| format!("- {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    });

    // Fetch activity log for the prompt. Split into two tiers:
    // 1. Feedback (PM + reviewer comments) — shown prominently and in full
    // 2. History (worker notes, transitions, verification) — summarized
    // Agents can use `task_activity_list` with filters for deeper queries.
    const MAX_FEEDBACK_CHARS: usize = 2000;
    const MAX_HISTORY_ENTRY_CHARS: usize = 400;
    const MAX_HISTORY_ENTRIES: usize = 10;
    const MAX_TOTAL_CHARS: usize = 6000;
    let task_repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
    let activity_text = match task_repo.list_activity(&task.id).await {
        Ok(entries) if !entries.is_empty() => {
            // Tier 1: PM and reviewer feedback — high-signal, shown in full
            let feedback_lines: Vec<String> = entries
                .iter()
                .filter(|e| {
                    e.event_type == "comment"
                        && (e.actor_role == "pm" || e.actor_role == "task_reviewer")
                })
                .map(|e| {
                    let body = serde_json::from_str::<serde_json::Value>(&e.payload)
                        .ok()
                        .and_then(|v| v.get("body").and_then(|s| s.as_str().map(String::from)))
                        .unwrap_or_default();
                    let label = if e.actor_role == "pm" {
                        "PM guidance"
                    } else {
                        "Reviewer feedback"
                    };
                    let mut line = format!("- **{label}**: {body}");
                    if line.len() > MAX_FEEDBACK_CHARS {
                        truncate_utf8(&mut line, MAX_FEEDBACK_CHARS);
                        line.push_str("… [truncated]");
                    }
                    line
                })
                .collect();

            // Tier 2: Everything else — compact summary
            let history_lines: Vec<String> = entries
                .iter()
                .filter(|e| {
                    // Skip feedback (already shown above) and noisy events
                    if e.event_type == "comment"
                        && (e.actor_role == "pm" || e.actor_role == "task_reviewer")
                    {
                        return false;
                    }
                    e.event_type == "comment"
                        || e.event_type == "status_changed"
                        || e.event_type == "merge_conflict"
                })
                .take(MAX_HISTORY_ENTRIES)
                .map(|e| {
                    let preview = serde_json::from_str::<serde_json::Value>(&e.payload)
                        .ok()
                        .and_then(|v| {
                            v.get("body")
                                .or_else(|| v.get("to_status"))
                                .and_then(|s| s.as_str().map(String::from))
                        })
                        .unwrap_or_default();
                    let mut line =
                        format!("- **{}** ({}): {}", e.event_type, e.actor_role, preview);
                    if line.len() > MAX_HISTORY_ENTRY_CHARS {
                        truncate_utf8(&mut line, MAX_HISTORY_ENTRY_CHARS);
                        line.push('…');
                    }
                    line
                })
                .collect();

            let mut sections = Vec::new();
            if !feedback_lines.is_empty() {
                sections.push(format!(
                    "**Feedback (action required):**\n{}",
                    feedback_lines.join("\n")
                ));
            }
            if !history_lines.is_empty() {
                sections.push(format!("**History:**\n{}", history_lines.join("\n")));
            }
            if sections.is_empty() {
                None
            } else {
                let mut joined = sections.join("\n\n");
                if joined.len() > MAX_TOTAL_CHARS {
                    truncate_utf8(&mut joined, MAX_TOTAL_CHARS);
                    joined.push_str(
                        "\n… [truncated — use `task_activity_list` with filters for full history]",
                    );
                }
                Some(joined)
            }
        }
        _ => None,
    };

    let system_prompt = render_prompt(
        agent_type,
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
            setup_commands: prompt_setup_commands,
            verification_commands: prompt_verification_commands,
            activity: activity_text,
        },
    );

    let context_window = app_state
        .catalog()
        .find_model(&model_id)
        .map(|m| m.context_window)
        .unwrap_or(0);

    let session_repo = SessionRepository::new(app_state.db().clone(), app_state.events().clone());

    // ── Build Djinn-native provider ───────────────────────────────────────────
    let telemetry_meta = build_telemetry_meta(agent_type, &task_id);

    let provider_config = match provider_credential {
        ProviderCredential::OAuthConfig(mut cfg) => {
            // OAuth config carries defaults; override model_id and context_window
            // from the dispatch request.
            cfg.model_id = model_name.clone();
            cfg.context_window = context_window.max(0) as u32;
            cfg.telemetry = Some(telemetry_meta);
            cfg
        }
        ProviderCredential::ApiKey(_key_name, api_key) => {
            let format_family =
                format_family_for_provider(&catalog_provider_id, &model_name);
            // Prefer the catalog's base_url (handles custom providers like
            // "synthetic"); fall back to the hardcoded defaults for well-known
            // providers that may not carry a base_url in the catalog.
            let base_url = app_state
                .catalog()
                .list_providers()
                .iter()
                .find(|p| p.id == catalog_provider_id)
                .map(|p| p.base_url.clone())
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| default_base_url(&catalog_provider_id));
            crate::agent::provider::ProviderConfig {
                base_url,
                auth: auth_method_for_provider(&catalog_provider_id, &api_key),
                format_family,
                model_id: model_name.clone(),
                context_window: context_window.max(0) as u32,
                telemetry: Some(telemetry_meta),
                provider_headers: Default::default(),
                capabilities: capabilities_for_provider(&catalog_provider_id),
            }
        }
    };
    let provider = create_provider(provider_config);

    // ── Create or resume session record + build conversation ─────────────────
    let tools = extension::tool_schemas(agent_type);

    // Try to resume from a paused session's saved conversation.
    let (current_record_id, mut conversation) = if let Some(ref resume_id) = resume_record_id {
        match super::conversation_store::load(resume_id).await {
            Ok(Some(mut saved_conv)) => {
                // Replace the system prompt with a fresh one (reflects updated AC).
                if !saved_conv.messages.is_empty()
                    && saved_conv.messages[0].role == crate::agent::message::Role::System
                {
                    saved_conv.messages[0] = Message::system(system_prompt.clone());
                }

                // Compact the prior conversation before appending feedback.
                // This strips the model's "I'm done" messages and frees context
                // window for actual work, while preserving research/context.
                let pre_compact_len = saved_conv.messages.len();
                let compacted = crate::agent::compaction::compact_conversation(
                    provider.as_ref(),
                    &mut saved_conv,
                    resume_id,
                    &task_id,
                    &app_state,
                    crate::agent::compaction::CompactionContext::PreResume(agent_type),
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
                    .create(
                        &task.project_id,
                        Some(&task.id),
                        &model_id,
                        agent_type.as_str(),
                        worktree_path.to_str(),
                        None,
                    )
                    .await
                {
                    Ok(r) => Some(r.id),
                    Err(e) => {
                        tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to create session record");
                        transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state)
                            .await;
                        cleanup_worktree(&task_id, &worktree_path, &app_state).await;
                        return_free!();
                    }
                };
                let mut conv = Conversation::new();
                conv.push(Message::system(system_prompt.clone()));
                conv.push(Message::user(
                    "Start by understanding the task context and execute it fully before stopping.",
                ));
                (record_id, conv)
            }
        }
    } else {
        // Fresh session — no paused session to resume.
        let record_id = match session_repo
            .create(
                &task.project_id,
                Some(&task.id),
                &model_id,
                agent_type.as_str(),
                worktree_path.to_str(),
                None,
            )
            .await
        {
            Ok(r) => Some(r.id),
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to create session record");
                transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
                cleanup_worktree(&task_id, &worktree_path, &app_state).await;
                return_free!();
            }
        };
        let mut conv = Conversation::new();
        conv.push(Message::system(system_prompt.clone()));
        conv.push(Message::user(
            "Start by understanding the task context and execute it fully before stopping.",
        ));
        (record_id, conv)
    };

    // Use the DB record ID as the session ID so OTel/Langfuse traces, error
    // diagnostics, and session_messages all reference the same identifier.
    let current_session_id = current_record_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());

    // ── Run reply loop ────────────────────────────────────────────────────────
    let (reply_result, final_output, tokens_in_loop, tokens_out_loop) = run_reply_loop(
        provider.as_ref(),
        &mut conversation,
        &tools,
        &task.id,
        &task.short_id,
        &current_session_id,
        &project_path,
        &worktree_path,
        agent_type,
        &cancel,
        &pause,
        &app_state,
        context_window,
        &model_id,
        resume_record_id.is_some(),
    )
    .await;

    // Persist conversation messages to session_messages table for timeline display.
    // Compaction already saves pre-compaction messages; this saves whatever remains
    // (post-compaction turns, or the full conversation if no compaction occurred).
    if let Some(ref record_id) = current_record_id {
        let msg_repo = crate::db::SessionMessageRepository::new(
            app_state.db().clone(),
            app_state.events().clone(),
        );
        if let Err(e) = msg_repo
            .insert_messages_batch(record_id, &task.id, &conversation.messages)
            .await
        {
            tracing::warn!(
                task_id = %task_id,
                session_id = %record_id,
                error = %e,
                "Lifecycle: failed to persist conversation messages to DB"
            );
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
        cleanup_worktree(&task_id, &worktree_path, &app_state).await;
        transition_interrupted(&task_id, agent_type, "session cancelled", &app_state).await;
        return_killed!();
    }

    let final_result = reply_result;
    let tokens_in = tokens_in_loop;
    let tokens_out = tokens_out_loop;

    // ── Post-loop: health + transitions + cleanup ─────────────────────────────

    // Health tracking.
    match &final_result {
        Ok(()) => app_state.health_tracker().record_success(&model_id),
        Err(_) => app_state.health_tracker().record_failure(&model_id),
    }
    app_state.persist_model_health_state().await;

    let is_worker_done = final_result.is_ok()
        && matches!(agent_type, AgentType::Worker | AgentType::ConflictResolver);

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
        cleanup_worktree(&task_id, &worktree_path, &app_state).await;
    }

    // Log reviewer feedback.
    let task_repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
    if let Some(feedback) = final_output.reviewer_feedback.as_deref() {
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

    // Log session errors.
    if let Err(reason) = &final_result {
        let payload = serde_json::json!({
            "error": reason.to_string(),
            "agent_type": agent_type.as_str(),
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
            "agent_type": agent_type.as_str(),
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
        Ok(()) => success_transition(&task_id, agent_type, &final_output, &app_state).await,
        Err(reason) => Some((agent_type.release_action(), Some(reason.to_string()))),
    };

    if let Some((action, reason)) = transition {
        tracing::info!(
            task_id = %task_id,
            agent_type = %agent_type.as_str(),
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
                &task_id,
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
        // Only interrupt the paused worker session on conflict rejection
        // (agent type changes to ConflictResolver, so the saved conversation
        // is not useful).  For regular reject/reject_stale, the paused session
        // stays so the worker can resume with reviewer feedback.
        if is_conflict_rejection {
            interrupt_paused_worker_session(&task_id, &app_state).await;
        }
        // Spawn background verification — runs outside the slot so the slot is
        // freed immediately.  The verification pipeline creates its own worktree,
        // runs setup + verification commands, and transitions the task to
        // needs_task_review (pass) or open (fail).
        if is_submit_verification {
            super::verification::spawn_verification(
                task_id.clone(),
                project_path.clone(),
                app_state.clone(),
            );
        }
    } else {
        tracing::info!(
            task_id = %task_id,
            agent_type = %agent_type.as_str(),
            tokens_in,
            tokens_out,
            "Lifecycle: session completed with no task transition"
        );
    }

    // Trigger dispatcher for the project so the next ready task starts promptly.
    if let Ok(task) = load_task(&task_id, &app_state).await
        && let Some(coordinator) = app_state.coordinator().await
    {
        let _ = coordinator
            .trigger_dispatch_for_project(&task.project_id)
            .await;
    }

    return_free!();
}

pub struct ProjectLifecycleParams {
    pub project_id: String,
    pub project_path: String,
    pub agent_type: String,
    pub model_id: String,
    pub app_state: AppState,
    pub cancel: CancellationToken,
    pub pause: CancellationToken,
    pub event_tx: mpsc::Sender<SlotEvent>,
}

pub async fn run_project_lifecycle(
    params: ProjectLifecycleParams,
) -> anyhow::Result<()> {
    let task_id = format!("project:{}:{}", params.project_id, params.agent_type);
    let model_id = params.model_id;
    let app_state = params.app_state;
    let cancel = params.cancel;
    let pause = params.pause;
    let event_tx = params.event_tx;
    let project_path = params.project_path;
    let project_id = params.project_id;

    let agent_type: AgentType = params
        .agent_type
        .parse()
        .unwrap_or(AgentType::Groomer);

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

    tracing::info!(
        task_id = %task_id,
        project_id = %project_id,
        model_id = %model_id,
        agent_type = %agent_type.as_str(),
        "Lifecycle: project-scoped dispatch accepted"
    );

    // ── Parse model ID and load credentials ───────────────────────────────
    let (catalog_provider_id, model_name) = match parse_model_id(&model_id) {
        Ok((provider_id, name)) => {
            let resolved = app_state
                .catalog()
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
    let provider_credential =
        match load_provider_credential(&catalog_provider_id, &app_state).await {
            Ok(cred) => cred,
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: missing credential");
                return_free!();
            }
        };

    // ── Build prompt ──────────────────────────────────────────────────────
    let system_prompt =
        crate::agent::prompts::render_project_prompt(agent_type, &project_path);

    let context_window = app_state
        .catalog()
        .find_model(&model_id)
        .map(|m| m.context_window)
        .unwrap_or(0);

    let session_repo =
        SessionRepository::new(app_state.db().clone(), app_state.events().clone());

    // ── Build provider ────────────────────────────────────────────────────
    let telemetry_meta = build_telemetry_meta(agent_type, &task_id);
    let provider_config = match provider_credential {
        ProviderCredential::OAuthConfig(mut cfg) => {
            cfg.model_id = model_name.clone();
            cfg.context_window = context_window.max(0) as u32;
            cfg.telemetry = Some(telemetry_meta);
            cfg
        }
        ProviderCredential::ApiKey(_key_name, api_key) => {
            let format_family =
                format_family_for_provider(&catalog_provider_id, &model_name);
            let base_url = app_state
                .catalog()
                .list_providers()
                .iter()
                .find(|p| p.id == catalog_provider_id)
                .map(|p| p.base_url.clone())
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| default_base_url(&catalog_provider_id));
            crate::agent::provider::ProviderConfig {
                base_url,
                auth: auth_method_for_provider(&catalog_provider_id, &api_key),
                format_family,
                model_id: model_name.clone(),
                context_window: context_window.max(0) as u32,
                telemetry: Some(telemetry_meta),
                provider_headers: Default::default(),
                capabilities: capabilities_for_provider(&catalog_provider_id),
            }
        }
    };
    let provider = create_provider(provider_config);

    // ── Create session record (no task_id for project-scoped agents) ─────
    let current_record_id = match session_repo
        .create(
            &project_id,
            None,
            &model_id,
            agent_type.as_str(),
            Some(&project_path),
            None,
        )
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

    // ── Build conversation ────────────────────────────────────────────────
    let tools = extension::tool_schemas(agent_type);
    let mut conversation = Conversation::new();
    conversation.push(Message::system(system_prompt));
    conversation.push(Message::user(
        "Begin grooming the backlog for this project.",
    ));

    let project_dir = PathBuf::from(&project_path);

    // ── Run reply loop ────────────────────────────────────────────────────
    let (reply_result, _final_output, tokens_in, tokens_out) = run_reply_loop(
        provider.as_ref(),
        &mut conversation,
        &tools,
        &task_id,
        &task_id, // short_id — use task_id for project-scoped
        &current_session_id,
        &project_path,
        &project_dir, // worktree = project dir (no worktree for groomer)
        agent_type,
        &cancel,
        &pause,
        &app_state,
        context_window,
        &model_id,
        false, // not a resumed session
    )
    .await;

    // ── Persist messages ──────────────────────────────────────────────────
    if let Some(ref record_id) = current_record_id {
        let msg_repo = crate::db::SessionMessageRepository::new(
            app_state.db().clone(),
            app_state.events().clone(),
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
        Ok(()) => app_state.health_tracker().record_success(&model_id),
        Err(_) => app_state.health_tracker().record_failure(&model_id),
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
