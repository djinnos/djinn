use std::path::Path;
use std::sync::Arc;

use goose::agents::{Agent as GooseAgent, SessionConfig as GooseSessionConfig};
use goose::conversation::message::{Message as GooseMessage, MessageContent};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::agent::extension;
use crate::agent::output_parser::ParsedAgentOutput;
use crate::agent::{AgentType, SessionManager};
use crate::events::DjinnEvent;
use crate::server::AppState;

use super::*;

fn serialize_goose_message(msg: &GooseMessage) -> serde_json::Value {
    serde_json::to_value(msg).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "failed to serialize Goose message for SessionMessage event");
        serde_json::json!({
            "role": msg.role,
            "content": msg.content.iter().map(ToString::to_string).collect::<Vec<_>>(),
        })
    })
}

/// Compaction signal returned by the reply loop when the 80% threshold is hit.
pub(super) struct CompactionSignal {
    pub(super) session_id: String,
    pub(super) tokens_in: i64,
    pub(super) context_window: i64,
}

/// Runs the Goose reply loop for one session turn. Returns the result, the
/// accumulated output, and an optional compaction signal (if the 80% context
/// window threshold was reached mid-stream). The caller should compact and
/// restart the loop if a compaction signal is returned.
///
/// When `cancel` is triggered, the loop exits and returns `Err("cancelled")`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_reply_loop(
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
) -> (
    anyhow::Result<()>,
    ParsedAgentOutput,
    Option<CompactionSignal>,
) {
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
                        if let goose::agents::AgentEvent::Message(msg) = &evt {
                            if msg.role == assistant_role {
                                assistant_message_count += 1;
                                for content in &msg.content {
                                    match content {
                                        MessageContent::Text(text) => {
                                            push_fragment(&mut assistant_fragments, format!("text:{}", text.text));
                                            // Detect context exhaustion from streaming text
                                            // (Goose may not persist the error message to SQLite).
                                            // Trigger compaction signal immediately — no point
                                            // nudging or checking markers when context is blown.
                                            if !compaction_signaled && !output.context_exhausted {
                                                let lower = text.text.to_lowercase();
                                                if lower.contains("context_length_exceeded")
                                                    || lower.contains("context length exceeded")
                                                {
                                                    output.context_exhausted = true;
                                                    #[allow(unused_assignments)]
                                                    { compaction_signaled = true; }
                                                    let (ti, _to) = if let Ok(s) = session_manager.get_session(session_id, false).await {
                                                        let ti = s.accumulated_input_tokens.or(s.input_tokens).unwrap_or(0) as i64;
                                                        let to = s.accumulated_output_tokens.or(s.output_tokens).unwrap_or(0) as i64;
                                                        (ti, to)
                                                    } else {
                                                        tokens_from_goose_sqlite(session_id).await.unwrap_or((0, 0))
                                                    };
                                                    let cw = if context_window > 0 { context_window } else { 200_000 };
                                                    tracing::info!(
                                                        task_id = %task_id,
                                                        session_id = %session_id,
                                                        tokens_in = ti,
                                                        context_window = cw,
                                                        "Lifecycle: context_length_exceeded detected in stream; triggering compaction"
                                                    );
                                                    compaction_signal = Some(CompactionSignal {
                                                        session_id: session_id.to_owned(),
                                                        tokens_in: ti,
                                                        context_window: cw,
                                                    });
                                                    break 'outer;
                                                }
                                            }
                                        }
                                        MessageContent::ToolRequest(req) => {
                                            push_fragment(&mut assistant_fragments, format!("tool_request:{}", req.id));
                                            saw_any_tool_use = true;
                                        }
                                        MessageContent::FrontendToolRequest(req) => {
                                            push_fragment(&mut assistant_fragments, format!("frontend_tool_request:{}", req.id));
                                            saw_any_tool_use = true;
                                        }
                                        _ => {
                                            push_fragment(&mut assistant_fragments, format!("{}", content));
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

                                let _ = app_state.events().send(DjinnEvent::SessionMessage {
                                    session_id: session_id.to_owned(),
                                    task_id: task_id.to_owned(),
                                    agent_type: agent_type.as_str().to_owned(),
                                    message: serialize_goose_message(msg),
                                });
                            }
                            extension::handle_event(app_state, agent, &evt, worktree_path).await;
                        }
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

        // Parse markers from the persisted final assistant message (not from streaming chunks).
        if let Some(last_text) = last_assistant_text_from_goose_sqlite(session_id).await {
            output.ingest_text(&last_text);
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
                        && msg.role == assistant_role {
                            let _ = app_state.events().send(DjinnEvent::SessionMessage {
                                session_id: session_id.to_owned(),
                                task_id: task_id.to_owned(),
                                agent_type: agent_type.as_str().to_owned(),
                                message: serialize_goose_message(msg),
                            });
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
