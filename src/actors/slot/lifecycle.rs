use std::path::PathBuf;
use std::sync::Arc;

use goose::agents::{
    Agent as GooseAgent, AgentConfig as GooseAgentConfig, GoosePlatform,
};
use goose::config::{GooseMode, PermissionManager};
use goose::conversation::message::Message as GooseMessage;
use goose::model::ModelConfig;
use goose::providers;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::output_parser::WorkerSignal;
use crate::agent::prompts::{TaskContext, render_prompt};
use crate::agent::{AgentType, SessionManager, SessionType};
use crate::commands::{CommandSpec, run_commands};
use crate::db::repositories::epic_review_batch::EpicReviewBatchRepository;
use crate::db::repositories::project::ProjectRepository;
use crate::db::repositories::session::SessionRepository;
use crate::db::repositories::task::TaskRepository;
use crate::models::session::SessionStatus;
use crate::models::task::TransitionAction;
use crate::server::AppState;

use super::*;
use super::reply_loop::run_reply_loop;

/// Standalone async function that runs the full per-task lifecycle:
/// load -> worktree -> session -> reply loop -> verification -> post-session work -> cleanup.
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
    session_manager: Arc<SessionManager>,
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
    let active_batch = active_epic_batch_for_task(&task.id, &app_state).await;
    let conflict_ctx = conflict_context_for_dispatch(&task.id, &app_state).await;
    let merge_validation_ctx = merge_validation_context_for_dispatch(&task.id, &app_state).await;
    let agent_type = if active_batch.is_some() {
        AgentType::EpicReviewer
    } else {
        agent_type_for_task(&task, conflict_ctx.is_some())
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
        "Lifecycle: dispatch accepted; preparing session"
    );

    // ── Transition task to in-progress ────────────────────────────────────────
    if let Err(e) = transition_start(&task, agent_type, &app_state).await {
        tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: transition_start failed");
        return_free!();
    }

    // ── Parse model ID and load credentials ───────────────────────────────────
    let (catalog_provider_id, model_name) = match parse_model_id(&model_id) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: invalid model ID");
            transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
            return_free!();
        }
    };
    let goose_provider_id = resolve_goose_provider_id(&catalog_provider_id).await;

    if !provider_supports_oauth(&catalog_provider_id, &goose_provider_id).await {
        match load_provider_api_key(&catalog_provider_id, &app_state).await {
            Ok((key_name, api_key)) => {
                if let Err(e) = goose::config::Config::global().set_secret(&key_name, &api_key) {
                    tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to set API key");
                    transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
                    return_free!();
                }
            }
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: missing credential");
                transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
                return_free!();
            }
        }
    }

    // ── Prepare worktree / paused-session resume context ──────────────────────
    let session_name = format!("{} {}", task.short_id, task.title);
    let project_dir = PathBuf::from(&project_path);
    // Session resume is intentionally disabled — fresh sessions force the
    // agent to re-read the worktree and reviewer feedback.  These are kept
    // as None sentinels so the downstream session-creation branch still works.
    let resumed_session_id: Option<String> = None;
    let resumed_record_id: Option<String> = None;
    let resumed_kickoff: Option<GooseMessage> = None;

    let paused = if agent_type == AgentType::EpicReviewer {
        None
    } else {
        find_paused_session_record(&task_id, &app_state).await
    };

    let worktree_path = if let Some(paused) = paused {
        if let (Some(paused_session_id), Some(paused_worktree_path)) = (
            paused.goose_session_id.clone(),
            paused.worktree_path.as_deref().map(PathBuf::from),
        ) {
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
                        tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: prepare_worktree failed");
                        transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state)
                            .await;
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
                        tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: prepare_worktree failed");
                        transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state)
                            .await;
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
                        tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: prepare_worktree failed");
                        transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state)
                            .await;
                        return_free!();
                    }
                }
            } else {
                // Never resume paused Goose sessions — a fresh session forces
                // the agent to re-read the worktree and reviewer feedback instead
                // of just repeating "DONE".  Reuse the existing worktree (the
                // branch has all committed work).
                tracing::info!(
                    task_id = %task_id,
                    paused_session = %paused_session_id,
                    "Lifecycle: starting fresh session (no resume); reusing worktree"
                );
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
                paused_worktree_path
            }
        } else {
            tracing::warn!(task_id = %task_id, session_record_id = %paused.id, "Lifecycle: paused session missing resume metadata; starting fresh session");
            match prepare_worktree(&project_dir, &task, &app_state).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: prepare_worktree failed");
                    transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
                    return_free!();
                }
            }
        }
    } else if agent_type == AgentType::EpicReviewer {
        let batch_id = active_batch.as_deref().unwrap_or_default();
        match prepare_epic_reviewer_worktree(&project_dir, batch_id, &app_state).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: prepare_epic_reviewer_worktree failed");
                transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
                return_free!();
            }
        }
    } else {
        match prepare_worktree(&project_dir, &task, &app_state).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: prepare_worktree failed");
                transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
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

    // ── Goose logs dir ────────────────────────────────────────────────────────
    let goose_logs_dir = goose::config::paths::Paths::in_state_dir("logs");
    if let Err(e) = std::fs::create_dir_all(&goose_logs_dir) {
        tracing::warn!(task_id = %task.short_id, path = %goose_logs_dir.display(), error = %e, "failed to ensure Goose logs directory");
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
    if resumed_session_id.is_none()
        && let Ok(Some(project)) = project_repo.get(&task.project_id).await
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
                            "Lifecycle: setup command failed; releasing task"
                        );
                        let task_repo =
                            TaskRepository::new(app_state.db().clone(), app_state.events().clone());
                        let _ = task_repo
                            .transition(
                                &task.id,
                                TransitionAction::Release,
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
                            TransitionAction::Release,
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

    // ── Create or resume Goose session ─────────────────────────────────────────
    let session_repo = SessionRepository::new(app_state.db().clone(), app_state.events().clone());
    let (current_session_id, current_record_id, mut kickoff) = if let (
        Some(session_id),
        Some(record_id),
        Some(kickoff),
    ) = (
        resumed_session_id.clone(),
        resumed_record_id.clone(),
        resumed_kickoff,
    ) {
        if let Err(e) = session_repo.set_running(&record_id).await {
            tracing::warn!(record_id = %record_id, error = %e, "Lifecycle: failed to mark resumed session running");
        }
        tracing::info!(
            task_id = %task.short_id,
            task_uuid = %task.id,
            session_id = %session_id,
            "Lifecycle: resuming paused session"
        );
        (session_id, Some(record_id), kickoff)
    } else {
        let session = match session_manager
            .create_session(worktree_path.clone(), session_name, SessionType::SubAgent)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to create Goose session");
                transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
                cleanup_worktree(&task_id, &worktree_path, &app_state).await;
                return_free!();
            }
        };

        let session_record = match session_repo
            .create(
                &task.project_id,
                &task.id,
                &model_id,
                agent_type.as_str(),
                worktree_path.to_str(),
                Some(session.id.as_str()),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to create session record");
                transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
                cleanup_worktree(&task_id, &worktree_path, &app_state).await;
                return_free!();
            }
        };

        if agent_type == AgentType::EpicReviewer
            && let Some(batch_id) = active_batch.as_deref()
        {
            let batch_repo =
                EpicReviewBatchRepository::new(app_state.db().clone(), app_state.events().clone());
            if let Err(e) = batch_repo.mark_in_review(batch_id, &session.id).await {
                tracing::warn!(task_id = %task.short_id, batch_id = %batch_id, error = %e, "failed to mark epic review batch in_review");
            }
        }

        (
            session.id,
            Some(session_record.id),
            GooseMessage::user().with_text(
                "Start by understanding the task context and execute it fully before stopping.",
            ),
        )
    };

    // ── Create agent ───────────────────────────────────────────────────────────
    //
    // Look up the context window from models.dev (our catalog) and inject it
    // into Goose's ModelConfig so that Goose's built-in auto-compaction uses the
    // correct limit — especially for models not in Goose's canonical registry
    // (e.g. codex-5.3 injected via models.dev).
    let catalog_context_window = app_state
        .catalog()
        .find_model(&model_id)
        .map(|m| m.context_window as usize)
        .filter(|&w| w > 0);

    let goose_model = match ModelConfig::new(&model_name) {
        Ok(m) => {
            let mut cfg = m.with_canonical_limits(&goose_provider_id);
            // Only apply catalog value if no env var or canonical limit was found.
            if cfg.context_limit.is_none() {
                cfg = cfg.with_context_limit(catalog_context_window);
            }
            cfg
        }
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to build ModelConfig");
            transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
            cleanup_worktree(&task_id, &worktree_path, &app_state).await;
            return_free!();
        }
    };

    let exts = extensions_for(agent_type);
    let provider = match providers::create(&goose_provider_id, goose_model.clone(), exts.clone())
        .await
    {
        Ok(p) => p,
        Err(e) => {
            app_state.health_tracker().record_failure(&model_id);
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to create provider");
            transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
            cleanup_worktree(&task_id, &worktree_path, &app_state).await;
            return_free!();
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

    if let Err(e) = agent.update_provider(provider, &current_session_id).await {
        app_state.health_tracker().record_failure(&model_id);
        tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to set provider");
        transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
        cleanup_worktree(&task_id, &worktree_path, &app_state).await;
        return_free!();
    }

    for ext in exts {
        if let Err(e) = agent.add_extension(ext, &current_session_id).await {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to add extension");
        }
    }

    // ── Build and set system prompt ───────────────────────────────────────────
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
    agent
        .extend_system_prompt("djinn_task".to_string(), prompt)
        .await;

    // Context window for SSE token-usage events (desktop UI).  Goose now owns
    // compaction using the context_limit we injected into ModelConfig above, so
    // this is purely informational.
    let context_window = goose_model.context_limit() as i64;

    // ── Main lifecycle loop (verification retry) ─────────────────────────────
    let current_agent = agent;

    let (final_result, final_output) = loop {
        let (reply_result, output) = run_reply_loop(
            &current_agent,
            &current_session_id,
            &task_id,
            &project_path,
            &worktree_path,
            agent_type,
            kickoff.clone(),
            &cancel,
            &pause,
            &app_state,
            context_window,
            &session_manager,
        )
        .await;

        // ── Handle pause/kill cancellation ─────────────────────────────────
        if pause.is_cancelled() {
            tracing::info!(task_id = %task_id, "Lifecycle: paused; committing WIP and preserving worktree");
            let (ti, to) = tokens_for_session(&current_session_id, &session_manager).await;
            update_session_record_paused(current_record_id.as_deref(), ti, to, &app_state).await;
            commit_wip_if_needed(&task_id, &worktree_path, &app_state).await;
            return_free!();
        }
        if cancel.is_cancelled() {
            tracing::info!(task_id = %task_id, "Lifecycle: cancelled; committing WIP and cleaning up");
            let (ti, to) = tokens_for_session(&current_session_id, &session_manager).await;
            update_session_record(
                current_record_id.as_deref(),
                SessionStatus::Interrupted,
                ti,
                to,
                &app_state,
            )
            .await;
            commit_wip_if_needed(&task_id, &worktree_path, &app_state).await;
            cleanup_worktree(&task_id, &worktree_path, &app_state).await;
            transition_interrupted(&task_id, agent_type, "session cancelled", &app_state).await;
            return_killed!();
        }

        // ── Verification pipeline for worker DONE ───────────────────────────
        let is_worker_done = reply_result.is_ok()
            && matches!(agent_type, AgentType::Worker | AgentType::ConflictResolver)
            && matches!(output.worker_signal, Some(WorkerSignal::Done));

        if is_worker_done {
            if let Some(feedback) =
                run_setup_commands_checked(&task_id, &worktree_path, &app_state).await
            {
                tracing::info!(task_id = %task_id, "Lifecycle: setup verification failed; resuming with feedback");
                // Log the feedback as a comment.
                let repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
                let payload = serde_json::json!({ "body": feedback }).to_string();
                let _ = repo
                    .log_activity(
                        Some(&task_id),
                        "agent-supervisor",
                        "verification",
                        "comment",
                        &payload,
                    )
                    .await;
                kickoff = GooseMessage::user().with_text(&feedback);
                continue;
            }
            if let Some(feedback) =
                run_verification_commands(&task_id, &worktree_path, &app_state).await
            {
                tracing::info!(task_id = %task_id, "Lifecycle: verification failed; resuming with feedback");
                let repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
                let payload = serde_json::json!({ "body": feedback }).to_string();
                let _ = repo
                    .log_activity(
                        Some(&task_id),
                        "agent-supervisor",
                        "verification",
                        "comment",
                        &payload,
                    )
                    .await;
                kickoff = GooseMessage::user().with_text(&feedback);
                continue;
            }
        }

        // ── Done ────────────────────────────────────────────────────────────
        break (reply_result, output);
    };

    // ── Post-loop: session record + health + transitions + cleanup ────────────
    let (tokens_in, tokens_out) = tokens_for_session(&current_session_id, &session_manager).await;

    // Health tracking.
    match &final_result {
        Ok(()) => app_state.health_tracker().record_success(&model_id),
        Err(_) => app_state.health_tracker().record_failure(&model_id),
    }
    app_state.persist_model_health_state().await;

    let is_worker_done = final_result.is_ok()
        && matches!(agent_type, AgentType::Worker | AgentType::ConflictResolver)
        && matches!(final_output.worker_signal, Some(WorkerSignal::Done));

    // Update session record.
    if is_worker_done {
        update_session_record_paused(
            current_record_id.as_deref(),
            tokens_in,
            tokens_out,
            &app_state,
        )
        .await;
    } else {
        let status = if final_result.is_ok() {
            SessionStatus::Completed
        } else {
            SessionStatus::Failed
        };
        update_session_record(
            current_record_id.as_deref(),
            status,
            tokens_in,
            tokens_out,
            &app_state,
        )
        .await;
    }

    // Worktree: commit and keep for worker done; cleanup otherwise.
    if let Some(worktree_ref) = Some(&worktree_path) {
        if is_worker_done {
            // Commit final work but keep worktree alive for review -> resume cycle.
            if let Err(e) = commit_final_work_if_needed(&task_id, worktree_ref, &app_state).await {
                tracing::warn!(
                    task_id = %task_id,
                    error = %e,
                    "Lifecycle: failed to commit final work before pausing for review"
                );
            }
        } else {
            cleanup_worktree(&task_id, worktree_ref, &app_state).await;
        }

        // Post-DONE setup re-check is already handled in the main loop above.
        // (run_setup_commands_checked / run_verification_commands are called in the loop)
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
    let epic_error = final_result.as_ref().err().map(|e| e.to_string());
    let transition = match final_result {
        Ok(()) => success_transition(&task_id, agent_type, &final_output, &app_state).await,
        Err(reason) => match agent_type {
            AgentType::Worker | AgentType::ConflictResolver => {
                Some((TransitionAction::Release, Some(reason.to_string())))
            }
            AgentType::TaskReviewer => Some((
                TransitionAction::ReleaseTaskReview,
                Some(reason.to_string()),
            )),
            AgentType::EpicReviewer => None,
        },
    };

    if agent_type == AgentType::EpicReviewer {
        finalize_epic_batch(&task_id, &final_output, epic_error.as_deref(), &app_state).await;
    }

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
        let is_reviewer_rejection = matches!(
            action,
            TransitionAction::TaskReviewReject | TransitionAction::TaskReviewRejectConflict
        );
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
        if is_reviewer_rejection {
            interrupt_paused_worker_session(&task_id, &app_state).await;
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
