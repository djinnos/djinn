// The new Djinn-native reply loop is not yet wired into lifecycle.rs (pending
// task g7qy). Suppress dead-code lint until that wiring is done.
#![allow(dead_code)]

use std::path::Path;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::agent::extension;
use crate::agent::message::{ContentBlock, Conversation, Message, MessageMeta, Role};
use crate::agent::output_parser::ParsedAgentOutput;
use crate::agent::provider::{LlmProvider, StreamEvent};
use crate::agent::AgentType;
use crate::events::DjinnEvent;
use crate::server::AppState;

use super::*;

const MAX_TURNS: u32 = 1000;

fn is_context_length_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string().to_lowercase();
    msg.contains("context_length")
        || msg.contains("too many tokens")
        || msg.contains("maximum context")
        || msg.contains("context window")
        || msg.contains("prompt is too long")
}

fn serialize_message(msg: &Message) -> serde_json::Value {
    serde_json::to_value(msg).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "failed to serialize Message for SessionMessage event");
        serde_json::json!({
            "role": format!("{:?}", msg.role).to_lowercase(),
            "content": msg.content.iter().filter_map(|b| b.as_text()).collect::<Vec<_>>(),
        })
    })
}

fn push_fragment(fragments: &mut Vec<String>, value: String) {
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
}

/// Djinn-native reply loop. Drives an `LlmProvider` stream, dispatches tool
/// calls via the extension layer, and continues until the assistant produces a
/// text-only response or a termination condition is reached.
///
/// Returns `(result, parsed_output)`.  A context-length-exceeded condition is
/// returned as `Err(anyhow!("context_length_exceeded"))` so the lifecycle can
/// trigger compaction rather than treating it as a fatal error.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_reply_loop(
    provider: &dyn LlmProvider,
    conversation: &mut Conversation,
    tools: &[serde_json::Value],
    task_id: &str,
    session_id: &str,
    project_path: &str,
    worktree_path: &Path,
    agent_type: AgentType,
    cancel: &CancellationToken,
    global_cancel: &CancellationToken,
    app_state: &AppState,
    context_window: i64,
) -> (anyhow::Result<()>, ParsedAgentOutput) {
    let mut output = ParsedAgentOutput::new(agent_type);

    let run_result: anyhow::Result<()> = async {
        let mut saw_any_event = false;
        let mut saw_any_tool_use = false;
        let mut assistant_message_count: usize = 0;
        let mut assistant_fragments: Vec<String> = Vec::new();

        // Accumulated token counts across all turns.
        let mut total_tokens_in: u32 = 0;
        let mut total_tokens_out: u32 = 0;

        // Track the last assistant text for output parsing.
        let mut last_assistant_text = String::new();

        let mut turns: u32 = 0;

        loop {
            if turns >= MAX_TURNS {
                return Err(anyhow::anyhow!(
                    "max turns ({}) exceeded without text-only response",
                    MAX_TURNS
                ));
            }
            turns += 1;

            let env_diag = runtime_env_diagnostics(session_id, project_path, worktree_path);
            tracing::info!(
                task_id = %task_id,
                session_id = %session_id,
                turn = turns,
                worktree = %worktree_path.display(),
                "ReplyLoop: starting provider stream; {}",
                env_diag
            );

            // ── Start streaming from the provider ────────────────────────────
            let mut stream = provider.stream(conversation, tools).await.map_err(|e| {
                if is_context_length_error(&e) {
                    return anyhow::anyhow!("context_length_exceeded");
                }
                let diag = runtime_fs_diagnostics(project_path, worktree_path);
                let env_diag = runtime_env_diagnostics(session_id, project_path, worktree_path);
                anyhow::anyhow!(
                    "provider stream init failed: display={} debug={:?}; {}; {}",
                    e,
                    e,
                    diag,
                    env_diag
                )
            })?;

            // Accumulate the assistant turn from stream events.
            let mut turn_text = String::new();
            let mut turn_tool_calls: Vec<ContentBlock> = Vec::new();
            let mut turn_tokens_in: u32 = 0;
            let mut turn_tokens_out: u32 = 0;
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
                            if is_context_length_error(&e) {
                                return anyhow::anyhow!("context_length_exceeded");
                            }
                            let diag = runtime_fs_diagnostics(project_path, worktree_path);
                            let env_diag = runtime_env_diagnostics(session_id, project_path, worktree_path);
                            anyhow::anyhow!(
                                "provider stream event failed: display={} debug={:?}; {}; {}",
                                e, e, diag, env_diag
                            )
                        })?;

                        saw_any_event = true;
                        saw_round_event = true;

                        match evt {
                            StreamEvent::Delta(ContentBlock::Text { text }) => {
                                // Emit streaming delta SSE event.
                                let _ = app_state.events().send(DjinnEvent::SessionMessage {
                                    session_id: session_id.to_owned(),
                                    task_id: task_id.to_owned(),
                                    agent_type: agent_type.as_str().to_owned(),
                                    message: serde_json::json!({
                                        "type": "delta",
                                        "role": "assistant",
                                        "text": text,
                                    }),
                                });
                                turn_text.push_str(&text);
                            }
                            StreamEvent::Delta(tool_use @ ContentBlock::ToolUse { .. }) => {
                                turn_tool_calls.push(tool_use);
                            }
                            StreamEvent::Delta(ContentBlock::ToolResult { .. }) => {
                                // Provider should not be streaming tool results; ignore.
                            }
                            StreamEvent::Usage(usage) => {
                                turn_tokens_in = usage.input;
                                turn_tokens_out = usage.output;
                                total_tokens_in = total_tokens_in.saturating_add(usage.input);
                                total_tokens_out = total_tokens_out.saturating_add(usage.output);

                                let usage_pct = if context_window > 0 {
                                    total_tokens_in as f64 / context_window as f64
                                } else {
                                    0.0
                                };
                                let _ = app_state.events().send(DjinnEvent::SessionTokenUpdate {
                                    session_id: session_id.to_owned(),
                                    task_id: task_id.to_owned(),
                                    tokens_in: total_tokens_in as i64,
                                    tokens_out: total_tokens_out as i64,
                                    context_window,
                                    usage_pct,
                                });
                            }
                            StreamEvent::Done => {
                                break;
                            }
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
                    "provider stream ended without any events; {}",
                    diag
                ));
            }

            // ── Build the assistant message from this turn ───────────────────
            let mut assistant_content: Vec<ContentBlock> = Vec::new();
            if !turn_text.is_empty() {
                push_fragment(&mut assistant_fragments, format!("text:{}", turn_text));
                last_assistant_text = turn_text.clone();
                assistant_content.push(ContentBlock::Text { text: turn_text.clone() });
            }
            for tool_call in &turn_tool_calls {
                if let ContentBlock::ToolUse { id, .. } = tool_call {
                    push_fragment(&mut assistant_fragments, format!("tool_use:{}", id));
                }
                saw_any_tool_use = true;
                assistant_content.push(tool_call.clone());
            }

            if assistant_content.is_empty() {
                // No content at all — treat as a provider error.
                let diag = runtime_fs_diagnostics(project_path, worktree_path);
                return Err(anyhow::anyhow!(
                    "provider returned empty assistant turn; {}",
                    diag
                ));
            }

            let assistant_msg = Message {
                role: Role::Assistant,
                content: assistant_content,
                metadata: Some(MessageMeta {
                    input_tokens: Some(turn_tokens_in),
                    output_tokens: Some(turn_tokens_out),
                    timestamp: Some(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0),
                    ),
                }),
            };

            assistant_message_count += 1;

            // Emit the complete assistant message as an SSE event.
            let _ = app_state.events().send(DjinnEvent::SessionMessage {
                session_id: session_id.to_owned(),
                task_id: task_id.to_owned(),
                agent_type: agent_type.as_str().to_owned(),
                message: serialize_message(&assistant_msg),
            });

            conversation.push(assistant_msg);

            // ── Dispatch tool calls, if any ──────────────────────────────────
            if turn_tool_calls.is_empty() {
                // No tool calls → text-only response → session complete.
                tracing::info!(
                    task_id = %task_id,
                    agent_type = %agent_type.as_str(),
                    turns,
                    assistant_message_count,
                    "ReplyLoop: text-only turn — session complete"
                );
                break;
            }

            // Dispatch each tool call and collect results.
            let mut tool_result_blocks: Vec<ContentBlock> = Vec::new();
            for tool_call in &turn_tool_calls {
                let ContentBlock::ToolUse { id, name, input } = tool_call else {
                    continue;
                };

                tracing::debug!(
                    task_id = %task_id,
                    tool = %name,
                    tool_use_id = %id,
                    "ReplyLoop: dispatching tool call"
                );

                let args = match input {
                    serde_json::Value::Object(map) => Some(map.clone()),
                    _ => None,
                };

                let (content, is_error) =
                    match extension::call_tool(app_state, name, args, worktree_path).await {
                        Ok(value) => {
                            let text = if value.is_string() {
                                value.as_str().unwrap_or("").to_string()
                            } else {
                                value.to_string()
                            };
                            (vec![ContentBlock::Text { text }], false)
                        }
                        Err(err) => {
                            tracing::warn!(
                                task_id = %task_id,
                                tool = %name,
                                error = %err,
                                "ReplyLoop: tool call returned error"
                            );
                            (
                                vec![ContentBlock::Text {
                                    text: format!("error: {err}"),
                                }],
                                true,
                            )
                        }
                    };

                tool_result_blocks.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content,
                    is_error,
                });
            }

            // Push a user message containing all tool results.
            let tool_result_msg = Message {
                role: Role::User,
                content: tool_result_blocks,
                metadata: None,
            };
            conversation.push(tool_result_msg);

            // Continue to next turn.
        }

        if !saw_any_event {
            let diag = runtime_fs_diagnostics(project_path, worktree_path);
            return Err(anyhow::anyhow!(
                "provider session produced no events; {}",
                diag
            ));
        }

        // Parse the final assistant text for runtime errors and reviewer feedback.
        if !last_assistant_text.is_empty() {
            output.ingest_text(&last_assistant_text);
        }

        if !saw_any_tool_use {
            let reason = match agent_type {
                AgentType::Worker | AgentType::ConflictResolver => {
                    "worker ended without any tool use (provider error?)"
                }
                AgentType::TaskReviewer => {
                    "task reviewer ended without any tool use (provider error?)"
                }
                AgentType::PM => "PM agent ended without any tool use (provider error?)",
            };
            tracing::warn!(
                task_id = %task_id,
                agent_type = %agent_type.as_str(),
                saw_any_event,
                assistant_message_count,
                runtime_error = ?output.runtime_error,
                assistant_fragments = ?assistant_fragments,
                "ReplyLoop: session ended without any tool use"
            );
            return Err(anyhow::anyhow!(reason));
        }

        tracing::info!(
            task_id = %task_id,
            agent_type = %agent_type.as_str(),
            saw_any_event,
            assistant_message_count,
            turns,
            "ReplyLoop: session completed normally"
        );

        Ok(())
    }
    .await;

    (run_result, output)
}
