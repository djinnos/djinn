use std::path::{Path, PathBuf};
use std::sync::Arc;

use goose::agents::{
    Agent as GooseAgent, AgentConfig as GooseAgentConfig, GoosePlatform,
    SessionConfig as GooseSessionConfig,
};
use goose::config::{GooseMode, PermissionManager};
use goose::conversation::message::{Message as GooseMessage, MessageContent};
use goose::model::ModelConfig;
use goose::providers;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::agent::output_parser::ParsedAgentOutput;
use crate::agent::{AgentType, SessionManager, SessionType};
use crate::agent::prompts::{TaskContext, render_prompt};
use crate::commands::{CommandSpec, run_commands};
use crate::db::repositories::epic_review_batch::EpicReviewBatchRepository;
use crate::db::repositories::project::ProjectRepository;
use crate::db::repositories::session::SessionRepository;
use crate::db::repositories::task::TaskRepository;
use crate::events::DjinnEvent;
use crate::models::session::SessionStatus;
use crate::models::task::TransitionAction;
use crate::agent::output_parser::WorkerSignal;
use crate::agent::extension;
use crate::server::AppState;

use super::*;

// ─── Reply loop sub-function ──────────────────────────────────────────────────

/// Compaction signal returned by the reply loop when the 80% threshold is hit.
struct CompactionSignal {
    session_id: String,
    tokens_in: i64,
    context_window: i64,
}

/// Runs the Goose reply loop for one session turn. Returns the result, the
/// accumulated output, and an optional compaction signal (if the 80% context
/// window threshold was reached mid-stream). The caller should compact and
/// restart the loop if a compaction signal is returned.
///
/// When `cancel` is triggered, the loop exits and returns `Err("cancelled")`.
#[allow(clippy::too_many_arguments)]
async fn run_reply_loop(
    agent: &GooseAgent,
    session_id: &str,
    task_id: &str,
    project_path: &str,
    worktree_path: &Path,
    agent_type: AgentType,
    kickoff: GooseMessage,
    cancel: &CancellationToken,
    global_cancel: &CancellationToken,
    app_state: &AppState,
    context_window: i64,
    session_manager: &Arc<SessionManager>,
) -> (anyhow::Result<()>, ParsedAgentOutput, Option<CompactionSignal>) {
    let mut output = ParsedAgentOutput::new(agent_type);
    let mut compaction_signal: Option<CompactionSignal> = None;

    let run_result: anyhow::Result<()> = async {
        let mut pending_message = Some(kickoff);
        let mut saw_any_event = false;
        let mut saw_any_tool_use = false;
        let assistant_role = GooseMessage::assistant().role;
        let mut assistant_message_count: usize = 0;
        let mut assistant_fragments: Vec<String> = Vec::new();
        let mut compaction_signaled = false;

        let push_fragment = |fragments: &mut Vec<String>, value: String| {
            const MAX_FRAGMENTS: usize = 12;
            let normalized = value.replace('\n', "\\n").trim().to_string();
            if normalized.is_empty() {
                return;
            }
            let snippet: String = normalized.chars().take(160).collect();
            if fragments.len() >= MAX_FRAGMENTS {
                fragments.remove(0);
            }
            fragments.push(snippet);
        };

        'outer: while let Some(next_message) = pending_message.take() {
            let env_diag = runtime_env_diagnostics(session_id, project_path, worktree_path);
            tracing::info!(
                task_id = %task_id,
                session_id = %session_id,
                worktree = %worktree_path.display(),
                "Lifecycle: starting Goose reply; {}",
                env_diag
            );

            let mut stream = agent
                .reply(
                    next_message,
                    GooseSessionConfig {
                        id: session_id.to_owned(),
                        schedule_id: None,
                        max_turns: Some(300),
                        retry_config: None,
                    },
                    Some(cancel.clone()),
                )
                .await
                .map_err(|e| {
                    let diag = runtime_fs_diagnostics(project_path, worktree_path);
                    let env_diag = runtime_env_diagnostics(session_id, project_path, worktree_path);
                    anyhow::anyhow!(
                        "agent reply init failed: display={} debug={:?}; {}; {}",
                        e, e, diag, env_diag
                    )
                })?;

            let mut interrupted: Option<&'static str> = None;
            let mut saw_round_event = false;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        interrupted = Some("session cancelled");
                        break;
                    }
                    _ = global_cancel.cancelled() => {
                        interrupted = Some("supervisor shutting down");
                        break;
                    }
                    evt = stream.next() => {
                        let Some(evt) = evt else { break; };
                        let evt = evt.map_err(|e| {
                            let diag = runtime_fs_diagnostics(project_path, worktree_path);
                            let env_diag = runtime_env_diagnostics(session_id, project_path, worktree_path);
                            anyhow::anyhow!(
                                "agent stream event failed: display={} debug={:?}; {}; {}",
                                e, e, diag, env_diag
                            )
                        })?;
                        saw_any_event = true;
                        saw_round_event = true;
                        if let goose::agents::AgentEvent::Message(msg) = &evt
                            && msg.role == assistant_role
                        {
                            assistant_message_count += 1;
                            for content in &msg.content {
                                match content {
                                    MessageContent::Text(text) => {
                                        output.ingest_text(&text.text);
                                        push_fragment(&mut assistant_fragments, format!("text:{}", text.text));
                                    }
                                    MessageContent::ToolRequest(req) => {
                                        push_fragment(&mut assistant_fragments, format!("tool_request:{}", req.id));
                                        saw_any_tool_use = true;
                                        output.ingest_text(&content.to_string());
                                    }
                                    MessageContent::FrontendToolRequest(req) => {
                                        push_fragment(&mut assistant_fragments, format!("frontend_tool_request:{}", req.id));
                                        saw_any_tool_use = true;
                                        output.ingest_text(&content.to_string());
                                    }
                                    _ => {
                                        push_fragment(&mut assistant_fragments, format!("{}", content));
                                        output.ingest_text(&content.to_string());
                                    }
                                }
                            }

                            // Token tracking + compaction threshold check.
                            {
                                let goose_session = session_manager.get_session(session_id, false).await;
                                let (tokens_in, tokens_out) = if let Ok(s) = goose_session {
                                    let ti = s.accumulated_input_tokens
                                        .or(s.input_tokens)
                                        .unwrap_or(0) as i64;
                                    let to = s.accumulated_output_tokens
                                        .or(s.output_tokens)
                                        .unwrap_or(0) as i64;
                                    (ti, to)
                                } else {
                                    tokens_from_goose_sqlite(session_id).await.unwrap_or((0, 0))
                                };
                                let usage_pct = if context_window > 0 {
                                    tokens_in as f64 / context_window as f64
                                } else {
                                    0.0
                                };
                                let _ = app_state.events().send(DjinnEvent::SessionTokenUpdate {
                                    session_id: session_id.to_owned(),
                                    task_id: task_id.to_owned(),
                                    tokens_in,
                                    tokens_out,
                                    context_window,
                                    usage_pct,
                                });
                                #[allow(unused_assignments)]
                                if !compaction_signaled && context_window > 0 && usage_pct >= 0.8 {
                                    compaction_signaled = true;
                                    tracing::info!(
                                        task_id = %task_id,
                                        session_id = %session_id,
                                        tokens_in,
                                        context_window,
                                        threshold_pct = 80,
                                        "Lifecycle: compaction threshold reached; breaking reply loop"
                                    );
                                    compaction_signal = Some(CompactionSignal {
                                        session_id: session_id.to_owned(),
                                        tokens_in,
                                        context_window,
                                    });
                                    // Break out of both loops — compaction will restart with a fresh session.
                                    break 'outer;
                                }
                            }
                        }
                        extension::handle_event(app_state, agent, &evt, worktree_path).await;
                    }
                }
            }

            if let Some(reason) = interrupted {
                return Err(anyhow::anyhow!(reason));
            }

            if !saw_round_event {
                let diag = runtime_fs_diagnostics(project_path, worktree_path);
                return Err(anyhow::anyhow!(
                    "agent stream ended without any events; {}",
                    diag
                ));
            }
        }

        // If we broke out for compaction, skip the nudge / marker checks.
        if compaction_signal.is_some() {
            return Ok(());
        }

        if !saw_any_event {
            let diag = runtime_fs_diagnostics(project_path, worktree_path);
            return Err(anyhow::anyhow!("agent session produced no events; {}", diag));
        }

        // Send a nudge if the required marker is missing.
        if saw_any_tool_use && missing_required_marker(agent_type, &output)
            && let Some(nudge) = missing_marker_nudge(agent_type, &output) {
                tracing::info!(
                    task_id = %task_id,
                    agent_type = %agent_type.as_str(),
                    "Lifecycle: session ended without required marker; sending post-session nudge"
                );
                let nudge_msg = GooseMessage::user().with_text(nudge);
                let mut stream = agent
                    .reply(
                        nudge_msg,
                        GooseSessionConfig {
                            id: session_id.to_owned(),
                            schedule_id: None,
                            max_turns: Some(3),
                            retry_config: None,
                        },
                        Some(cancel.clone()),
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("nudge reply init failed: {e}"))?;

                let assistant_role = GooseMessage::assistant().role;
                while let Some(evt) = stream.next().await {
                    let evt = evt.map_err(|e| anyhow::anyhow!("nudge stream error: {e}"))?;
                    if let goose::agents::AgentEvent::Message(msg) = &evt
                        && msg.role == assistant_role
                    {
                        for content in &msg.content {
                            match content {
                                MessageContent::Text(text) => {
                                    output.ingest_text(&text.text);
                                }
                                _ => {
                                    output.ingest_text(&content.to_string());
                                }
                            }
                        }
                    }
                    extension::handle_event(app_state, agent, &evt, worktree_path).await;
                }
            }

        if let Some(last_assistant_text) =
            last_assistant_text_from_goose_sqlite(session_id).await
        {
            output.ingest_text(&last_assistant_text);
            tracing::info!(
                task_id = %task_id,
                agent_type = %agent_type.as_str(),
                marker_present_after_persisted_check = !missing_required_marker(agent_type, &output),
                "Lifecycle: parsed persisted last assistant message before marker decision"
            );
        }

        if missing_required_marker(agent_type, &output) {
            tracing::warn!(
                task_id = %task_id,
                agent_type = %agent_type.as_str(),
                saw_any_event,
                saw_any_tool_use,
                assistant_message_count,
                worker_signal = ?output.worker_signal,
                reviewer_verdict = ?output.reviewer_verdict,
                epic_verdict = ?output.epic_verdict,
                runtime_error = ?output.runtime_error,
                reviewer_feedback = ?output.reviewer_feedback,
                assistant_fragments = ?assistant_fragments,
                "Lifecycle: required marker missing at session end"
            );
            let reason = if !saw_any_tool_use {
                match agent_type {
                    AgentType::Worker | AgentType::ConflictResolver => "worker ended without any tool use (provider error?)",
                    AgentType::TaskReviewer => "task reviewer ended without any tool use (provider error?)",
                    AgentType::EpicReviewer => "epic reviewer ended without any tool use (provider error?)",
                }
            } else {
                match agent_type {
                    AgentType::Worker | AgentType::ConflictResolver => "worker ended without WORKER_RESULT marker",
                    AgentType::TaskReviewer => "task reviewer ended without REVIEW_RESULT marker",
                    AgentType::EpicReviewer => "epic reviewer ended without EPIC_REVIEW_RESULT marker",
                }
            };
            return Err(anyhow::anyhow!(reason));
        }

        Ok(())
    }
    .await;

    (run_result, output, compaction_signal)
}

// ─── Inline compaction ────────────────────────────────────────────────────────

struct CompactResult {
    new_session_id: String,
    new_record_id: String,
    agent: Arc<GooseAgent>,
    kickoff_summary: String,
}

/// Performs context compaction inline (without actor messaging). Creates a new
/// Goose session with a summary of the old one, and returns the new session info.
#[allow(clippy::too_many_arguments)]
async fn compact_inline(
    task_id: &str,
    agent_type: AgentType,
    project_id: &str,
    old_session_id: &str,
    old_record_id: Option<&str>,
    model_id: &str,
    goose_provider_id: &str,
    model_name: &str,
    worktree_path: &Path,
    context_window: i64,
    tokens_in: i64,
    session_manager: &Arc<SessionManager>,
    app_state: &AppState,
    resume_context: Option<&str>,
) -> Result<CompactResult, String> {
    // 1. Read conversation history + final token counts.
    let (final_tokens_in, final_tokens_out, messages) =
        match session_manager.get_session(old_session_id, true).await {
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
                tracing::warn!(task_id = %task_id, error = %e, "compaction: failed to read Goose session");
                (tokens_in, 0, vec![])
            }
        };

    // 2. Finalize old Djinn session record.
    if let Some(record_id) = old_record_id {
        let repo = SessionRepository::new(app_state.db().clone(), app_state.events().clone());
        if let Err(e) = repo
            .update(record_id, SessionStatus::Compacted, final_tokens_in, final_tokens_out)
            .await
        {
            tracing::warn!(record_id = %record_id, error = %e, "compaction: failed to finalize old session record");
        }
    }

    let goose_model = ModelConfig::new(model_name)
        .map_err(|e| format!("compaction: failed to build ModelConfig: {e}"))?
        .with_canonical_limits(goose_provider_id);

    // 3. Generate summary.
    let summary = if messages.is_empty() {
        tracing::warn!(task_id = %task_id, "compaction: empty conversation history; using fallback summary");
        "Context window was compacted. Please review the current state of the worktree and continue the task.".to_string()
    } else {
        let compaction_system = crate::agent::prompts::render_compaction_prompt();
        let summary_provider =
            providers::create(goose_provider_id, goose_model.clone(), vec![])
                .await
                .map_err(|e| {
                    app_state.health_tracker().record_failure(model_id);
                    format!("compaction: summary provider creation failed: {e}")
                })?;
        let model_config = summary_provider.get_model_config();
        summary_provider
            .complete(
                &model_config,
                old_session_id,
                compaction_system,
                &messages,
                &[],
            )
            .await
            .map(|(msg, _)| {
                tracing::info!(task_id = %task_id, "compaction: summary generated successfully");
                msg.as_concat_text()
            })
            .map_err(|e| {
                format!("compaction: summary generation failed: {e}")
            })?
    };

    // 4. Create new Goose session.
    let task_name = {
        let task_repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
        match task_repo.get(task_id).await {
            Ok(Some(t)) => format!("{} {} (compacted)", t.short_id, t.title),
            _ => format!("{task_id} (compacted)"),
        }
    };
    let new_goose_session = session_manager
        .create_session(worktree_path.to_owned(), task_name, SessionType::SubAgent)
        .await
        .map_err(|e| format!("compaction: failed to create new Goose session: {e}"))?;

    // 5. Create new Djinn session record.
    let session_repo = SessionRepository::new(app_state.db().clone(), app_state.events().clone());
    let new_record = session_repo
        .create(
            project_id,
            task_id,
            model_id,
            agent_type.as_str(),
            worktree_path.to_str(),
            Some(&new_goose_session.id),
            old_record_id,
        )
        .await
        .map_err(|e| format!("compaction: failed to create new session record: {e}"))?;

    // Log compaction activity.
    {
        let task_repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
        let usage_pct = if context_window > 0 {
            final_tokens_in as f64 / context_window as f64
        } else {
            0.0
        };
        let payload = serde_json::json!({
            "old_session_id": old_record_id.unwrap_or(""),
            "new_session_id": new_record.id,
            "tokens_in_at_compaction": final_tokens_in,
            "context_window": context_window,
            "usage_pct": usage_pct,
            "summary_token_count": summary.chars().count(),
        })
        .to_string();
        if let Err(e) = task_repo
            .log_activity(Some(task_id), "system", "system", "compaction", &payload)
            .await
        {
            tracing::warn!(task_id = %task_id, error = %e, "compaction: failed to log activity");
        }
    }

    // 6. Set up new agent.
    let extensions = extensions_for(agent_type);
    let provider = providers::create(goose_provider_id, goose_model, extensions.clone())
        .await
        .map_err(|e| {
            app_state.health_tracker().record_failure(model_id);
            format!("compaction: failed to create new agent provider: {e}")
        })?;

    let agent = Arc::new(GooseAgent::with_config(GooseAgentConfig::new(
        session_manager.clone(),
        PermissionManager::instance(),
        None,
        GooseMode::Auto,
        true,
        GoosePlatform::GooseCli,
    )));

    agent
        .update_provider(provider, &new_goose_session.id)
        .await
        .map_err(|e| {
            app_state.health_tracker().record_failure(model_id);
            format!("compaction: failed to set provider on new agent: {e}")
        })?;

    for ext in extensions {
        if let Err(e) = agent.add_extension(ext, &new_goose_session.id).await {
            tracing::warn!(task_id = %task_id, error = %e, "compaction: failed to add extension");
        }
    }

    let kickoff_summary = match resume_context {
        Some(ctx) => format!("{summary}\n\n---\n\n{ctx}"),
        None => summary,
    };

    Ok(CompactResult {
        new_session_id: new_goose_session.id,
        new_record_id: new_record.id,
        agent,
        kickoff_summary,
    })
}

// ─── Main task lifecycle function ─────────────────────────────────────────────

/// Standalone async function that runs the full per-task lifecycle:
/// load → worktree → session → reply loop → verification → post-session work → cleanup.
///
/// Compaction is handled as an inline loop (no supervisor messages). The reply
/// loop returns its result directly instead of sending SessionCompleted back to
/// an actor.
///
/// Sends `SlotEvent::Free` on normal completion and `SlotEvent::Killed` when
/// cancelled via `cancel`.
pub async fn run_task_lifecycle(
    task_id: String,
    project_path: String,
    model_id: String,
    app_state: AppState,
    session_manager: Arc<SessionManager>,
    cancel: CancellationToken,
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

    // ── Prepare worktree ───────────────────────────────────────────────────────
    let session_name = format!("{} {}", task.short_id, task.title);
    let project_dir = PathBuf::from(&project_path);
    let worktree_path = if agent_type == AgentType::EpicReviewer {
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
        && let Some(ref ctx) = conflict_ctx {
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
                    .run_command(vec!["merge".into(), target_ref.clone(), "--no-commit".into()])
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
        transition_interrupted(&task_id, agent_type, "worktree preflight failed", &app_state).await;
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
    if let Ok(Some(project)) = project_repo.get(&task.project_id).await {
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

    // ── Create Goose session ───────────────────────────────────────────────────
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

    // ── Create Djinn session record ───────────────────────────────────────────
    let session_repo = SessionRepository::new(app_state.db().clone(), app_state.events().clone());
    let session_record = match session_repo
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
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to create session record");
            transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
            cleanup_worktree(&task_id, &worktree_path, &app_state).await;
            return_free!();
        }
    };

    // Mark epic review batch as in_review.
    if agent_type == AgentType::EpicReviewer
        && let Some(batch_id) = active_batch.as_deref()
    {
        let batch_repo =
            EpicReviewBatchRepository::new(app_state.db().clone(), app_state.events().clone());
        if let Err(e) = batch_repo.mark_in_review(batch_id, &session.id).await {
            tracing::warn!(task_id = %task.short_id, batch_id = %batch_id, error = %e, "failed to mark epic review batch in_review");
        }
    }

    // ── Create agent ───────────────────────────────────────────────────────────
    let goose_model = match ModelConfig::new(&model_name) {
        Ok(m) => m.with_canonical_limits(&goose_provider_id),
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

    if let Err(e) = agent.update_provider(provider, &session.id).await {
        app_state.health_tracker().record_failure(&model_id);
        tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to set provider");
        transition_interrupted(&task_id, agent_type, &e.to_string(), &app_state).await;
        cleanup_worktree(&task_id, &worktree_path, &app_state).await;
        return_free!();
    }

    for ext in exts {
        if let Err(e) = agent.add_extension(ext, &session.id).await {
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
    agent.override_system_prompt(prompt).await;

    let context_window = app_state
        .catalog()
        .find_model(&model_id)
        .map(|m| m.context_window)
        .unwrap_or(0);

    // ── Main lifecycle loop (compaction + verification retry) ─────────────────
    let mut current_session_id = session.id.clone();
    let mut current_record_id = Some(session_record.id.clone());
    let mut current_agent = agent;
    let mut kickoff = GooseMessage::user().with_text(
        "Start by understanding the task context and execute it fully before stopping.",
    );

    let (final_result, final_output) = loop {
        let (reply_result, output, compaction_signal) = run_reply_loop(
            &current_agent,
            &current_session_id,
            &task_id,
            &project_path,
            &worktree_path,
            agent_type,
            kickoff.clone(),
            &cancel,
            &cancel, // global_cancel reuses task cancel (supervisor shuts down via same token)
            &app_state,
            context_window,
            &session_manager,
        )
        .await;

        // ── Handle cancellation ─────────────────────────────────────────────
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

        // ── Handle compaction signal (80% threshold) ────────────────────────
        if let Some(sig) = compaction_signal {
            tracing::info!(
                task_id = %task_id,
                tokens_in = sig.tokens_in,
                context_window = sig.context_window,
                "Lifecycle: compaction threshold reached; running inline compaction"
            );
            match compact_inline(
                &task_id,
                agent_type,
                &task.project_id,
                &sig.session_id,
                current_record_id.as_deref(),
                &model_id,
                &goose_provider_id,
                &model_name,
                &worktree_path,
                sig.context_window,
                sig.tokens_in,
                &session_manager,
                &app_state,
                None,
            )
            .await
            {
                Ok(compact) => {
                    // Refresh system prompt on the new agent.
                    let new_prompt = render_prompt(
                        agent_type,
                        &task,
                        &TaskContext {
                            project_path: project_path.clone(),
                            workspace_path: worktree_path.display().to_string(),
                            diff: None, commits: None, start_commit: None,
                            end_commit: None, batch_num: None, task_count: None,
                            tasks_summary: None, common_labels: None,
                            conflict_files: None, merge_base_branch: None,
                            merge_target_branch: None, merge_failure_context: None,
                            setup_commands: None, verification_commands: None,
                        },
                    );
                    compact.agent.override_system_prompt(new_prompt).await;
                    current_session_id = compact.new_session_id;
                    current_record_id = Some(compact.new_record_id);
                    current_agent = compact.agent;
                    kickoff = GooseMessage::user().with_text(&compact.kickoff_summary);
                    continue;
                }
                Err(e) => {
                    tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: compaction failed; releasing task");
                    let (ti, to) = tokens_for_session(&current_session_id, &session_manager).await;
                    update_session_record(
                        current_record_id.as_deref(),
                        SessionStatus::Failed,
                        ti,
                        to,
                        &app_state,
                    )
                    .await;
                    cleanup_worktree(&task_id, &worktree_path, &app_state).await;
                    transition_interrupted(&task_id, agent_type, &e, &app_state).await;
                    return_free!();
                }
            }
        }

        // ── Handle context exhaustion (at session end) ──────────────────────
        let is_context_error = match &reply_result {
            Err(reason) => {
                let lower = reason.to_string().to_lowercase();
                lower.contains("context length exceeded")
                    || lower.contains("context_length_exceeded")
                    || lower.contains("context limit exceeded")
            }
            Ok(()) => output.runtime_error.as_deref().is_some_and(|e| {
                let lower = e.to_lowercase();
                lower.contains("context length exceeded")
                    || lower.contains("context limit exceeded")
            }),
        } || output.context_exhausted;

        if is_context_error {
            // Reviewers: compaction won't help (prompt too large). Block the task.
            if matches!(agent_type, AgentType::TaskReviewer | AgentType::EpicReviewer) {
                tracing::warn!(
                    task_id = %task_id,
                    agent_type = %agent_type.as_str(),
                    "Lifecycle: context_length_exceeded on reviewer — blocking task"
                );
                let (ti, to) = tokens_for_session(&current_session_id, &session_manager).await;
                update_session_record(
                    current_record_id.as_deref(),
                    SessionStatus::Failed,
                    ti,
                    to,
                    &app_state,
                )
                .await;
                cleanup_worktree(&task_id, &worktree_path, &app_state).await;
                app_state.health_tracker().record_failure(&model_id);
                app_state.persist_model_health_state().await;
                let repo =
                    TaskRepository::new(app_state.db().clone(), app_state.events().clone());
                let reason = "context_length_exceeded: review prompt too large for current model";
                let _ = repo
                    .transition(
                        &task_id,
                        TransitionAction::ReleaseTaskReview,
                        "agent-supervisor",
                        "system",
                        Some(reason),
                        None,
                    )
                    .await;
                return_free!();
            }

            // Worker: compact and retry.
            tracing::info!(
                task_id = %task_id,
                "Lifecycle: context exhaustion at session end; triggering fresh continuation"
            );
            let (_ti, _) = tokens_for_session(&current_session_id, &session_manager).await;
            let cw = if context_window > 0 { context_window } else { 200_000 };
            match compact_inline(
                &task_id,
                agent_type,
                &task.project_id,
                &current_session_id,
                current_record_id.as_deref(),
                &model_id,
                &goose_provider_id,
                &model_name,
                &worktree_path,
                cw,
                cw, // signal we're at the limit
                &session_manager,
                &app_state,
                None,
            )
            .await
            {
                Ok(compact) => {
                    let new_prompt = render_prompt(
                        agent_type,
                        &task,
                        &TaskContext {
                            project_path: project_path.clone(),
                            workspace_path: worktree_path.display().to_string(),
                            diff: None, commits: None, start_commit: None,
                            end_commit: None, batch_num: None, task_count: None,
                            tasks_summary: None, common_labels: None,
                            conflict_files: None, merge_base_branch: None,
                            merge_target_branch: None, merge_failure_context: None,
                            setup_commands: None, verification_commands: None,
                        },
                    );
                    compact.agent.override_system_prompt(new_prompt).await;
                    current_session_id = compact.new_session_id;
                    current_record_id = Some(compact.new_record_id);
                    current_agent = compact.agent;
                    kickoff = GooseMessage::user().with_text(&compact.kickoff_summary);
                    continue;
                }
                Err(e) => {
                    let err_str = format!("context exhaustion compaction failed: {e}");
                    break (Err(anyhow::anyhow!("{}", err_str)), output);
                }
            }
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
                    .log_activity(Some(&task_id), "agent-supervisor", "verification", "comment", &payload)
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
                    .log_activity(Some(&task_id), "agent-supervisor", "verification", "comment", &payload)
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
        update_session_record_paused(current_record_id.as_deref(), tokens_in, tokens_out, &app_state).await;
    } else {
        let status = if final_result.is_ok() { SessionStatus::Completed } else { SessionStatus::Failed };
        update_session_record(current_record_id.as_deref(), status, tokens_in, tokens_out, &app_state).await;
    }

    // Worktree: commit and keep for worker done; cleanup otherwise.
    if let Some(worktree_ref) = Some(&worktree_path) {
        if is_worker_done {
            // Commit final work but keep worktree alive for review → resume cycle.
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
            .log_activity(Some(&task_id), "agent-supervisor", "task_reviewer", "comment", &payload)
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
            .log_activity(Some(&task_id), "agent-supervisor", "system", "session_error", &payload)
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
            .log_activity(Some(&task_id), "agent-supervisor", "system", "session_error", &payload)
            .await;
    }

    // Determine transition.
    let epic_error = final_result.as_ref().err().map(|e| e.to_string());
    let transition = match final_result {
        Ok(()) => {
            success_transition(&task_id, agent_type, &final_output, &app_state).await
        }
        Err(reason) => match agent_type {
            AgentType::Worker | AgentType::ConflictResolver => {
                Some((TransitionAction::Release, Some(reason.to_string())))
            }
            AgentType::TaskReviewer => {
                Some((TransitionAction::ReleaseTaskReview, Some(reason.to_string())))
            }
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
