use std::path::Path;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::agent::AgentType;
use crate::agent::extension;
use crate::agent::message::{ContentBlock, Conversation, Message, MessageMeta, Role};
use crate::agent::output_parser::ParsedAgentOutput;
use crate::agent::provider::telemetry;
use crate::agent::provider::{LlmProvider, StreamEvent};
use crate::events::DjinnEvent;
use crate::server::AppState;

use super::*;

const MAX_TURNS: u32 = 1000;
/// Maximum retries for empty assistant turns before treating as a hard failure.
const MAX_EMPTY_TURN_RETRIES: u32 = 2;

fn is_context_length_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string().to_lowercase();
    msg.contains("context_length")
        || msg.contains("too many tokens")
        || msg.contains("maximum context")
        || msg.contains("context window")
        || msg.contains("prompt is too long")
}

/// Detect "No tool call found for function call output" errors from the OpenAI
/// Responses API. These happen when a `tool` role message references a
/// `tool_call_id` that doesn't exist in any preceding assistant message —
/// typically after compaction removed the assistant message but left orphaned
/// tool results.
fn is_orphaned_tool_call_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string().to_lowercase();
    msg.contains("no tool call found for function call output")
        || msg.contains("no function call found")
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

/// Maximum reactive compaction attempts before giving up.
const MAX_COMPACTION_RETRIES: u32 = 2;

/// Djinn-native reply loop. Drives an `LlmProvider` stream, dispatches tool
/// calls via the extension layer, and continues until the assistant produces a
/// text-only response or a termination condition is reached.
///
/// Context-length-exceeded errors trigger reactive compaction and retry
/// (up to `MAX_COMPACTION_RETRIES` times) before failing the session.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_reply_loop(
    provider: &dyn LlmProvider,
    conversation: &mut Conversation,
    tools: &[serde_json::Value],
    task_id: &str,
    task_short_id: &str,
    session_id: &str,
    project_path: &str,
    worktree_path: &Path,
    agent_type: AgentType,
    cancel: &CancellationToken,
    global_cancel: &CancellationToken,
    app_state: &AppState,
    context_window: i64,
    model_id: &str,
    is_resumed_session: bool,
) -> (anyhow::Result<()>, ParsedAgentOutput, i64, i64) {
    let mut output = ParsedAgentOutput::new(agent_type);

    // Token counts and last assistant text are declared outside the async block
    // so they survive the borrow and can be used for telemetry/return values.
    let mut total_tokens_in: u32 = 0;
    let mut total_tokens_out: u32 = 0;
    let mut final_assistant_text = String::new();

    // Resumed sessions may respond text-only if the model determines the
    // reviewer's concerns are already addressed in the existing code.

    // ── Create session-level OTel span (root trace) ──────────────────────────
    let otel_session = if telemetry::is_active() {
        let session = telemetry::SessionSpan::start(&telemetry::SessionSpanAttributes {
            provider: provider.name(),
            model: model_id,
            task_short_id,
            task_id,
            agent_type: agent_type.as_str(),
            session_id,
        });
        // Record system prompt from the first message.
        if let Some(sys_msg) = conversation.messages.first()
            && sys_msg.role == Role::System
        {
            let sys_text: String = sys_msg
                .content
                .iter()
                .filter_map(|b| b.as_text())
                .collect::<Vec<_>>()
                .join("\n");
            if !sys_text.is_empty() {
                session.record_system_prompt(&sys_text);
            }
        }
        // Record the user message as the trace-level input
        // (shows in the Langfuse trace list Input column).
        // For resumed sessions use the *last* user message (reviewer feedback);
        // for fresh sessions use the first.
        let input_msg = if is_resumed_session {
            conversation.messages.iter().rfind(|m| m.role == Role::User)
        } else {
            conversation.messages.iter().find(|m| m.role == Role::User)
        };
        if let Some(user_msg) = input_msg {
            let input_text: String = user_msg
                .content
                .iter()
                .filter_map(|b| b.as_text())
                .collect::<Vec<_>>()
                .join("\n");
            if !input_text.is_empty() {
                session.record_trace_input(&input_text);
            }
        }
        Some(session)
    } else {
        None
    };

    let run_result: anyhow::Result<()> = async {
        let mut saw_any_event = false;
        let mut saw_any_tool_use = false;
        let mut assistant_message_count: usize = 0;
        let mut assistant_fragments: Vec<String> = Vec::new();
        let mut compaction_attempts: u32 = 0;
        let mut empty_turn_retries: u32 = 0;

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

            // ── Start OTel generation span for this turn ─────────────────────
            let otel_llm = otel_session.as_ref().map(|session| {
                let llm = telemetry::LlmSpan::start(
                    session.context(),
                    provider.name(),
                    model_id,
                    turns,
                );
                // Build a JSON messages array for the generation input.
                // On the first turn include the system prompt; on subsequent
                // turns only the last user/tool-result message.
                {
                    let mut msgs = Vec::new();
                    if turns == 1
                        && let Some(sys) = conversation
                            .messages
                            .iter()
                            .find(|m| m.role == Role::System)
                    {
                        let text: String = sys
                            .content
                            .iter()
                            .filter_map(|b| b.as_text())
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !text.is_empty() {
                            msgs.push(serde_json::json!({"role": "system", "content": text}));
                        }
                    }
                    if let Some(last_user) = conversation
                        .messages
                        .iter()
                        .rev()
                        .find(|m| m.role == Role::User)
                    {
                        let text: String = last_user
                            .content
                            .iter()
                            .filter_map(|b| b.as_text())
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !text.is_empty() {
                            msgs.push(serde_json::json!({"role": "user", "content": text}));
                        }
                    }
                    if !msgs.is_empty() {
                        llm.record_input(&serde_json::to_string(&msgs).unwrap());
                    }
                }
                llm
            });

            // ── Start streaming from the provider ────────────────────────────
            let stream_result = provider.stream(conversation, tools).await;
            let mut stream = match stream_result {
                Ok(s) => s,
                Err(e) if (is_context_length_error(&e) || is_orphaned_tool_call_error(&e)) && compaction_attempts < MAX_COMPACTION_RETRIES => {
                    // Reactive compaction: context exceeded or orphaned tool
                    // call references on stream init.
                    tracing::warn!(
                        task_id = %task_id,
                        compaction_attempts,
                        error = %e,
                        "ReplyLoop: recoverable provider error on stream init; compacting reactively"
                    );
                    if let Some(llm) = otel_llm {
                        llm.end_error("context_length_exceeded");
                    }
                    let compacted = crate::agent::compaction::compact_conversation(
                        provider, conversation, session_id, task_id, app_state,
                        crate::agent::compaction::CompactionContext::MidSession(agent_type),
                        context_window,
                    ).await;
                    if compacted {
                        total_tokens_in = 0;
                        total_tokens_out = 0;
                        compaction_attempts += 1;
                        conversation.push(crate::agent::message::Message::user(
                            "Continue with the task.",
                        ));
                        continue;
                    }
                    return Err(anyhow::anyhow!(
                        "context_length_exceeded and reactive compaction failed"
                    ));
                }
                Err(e) => {
                    if let Some(llm) = otel_llm {
                        llm.end_error(&e.to_string());
                    }
                    let diag = runtime_fs_diagnostics(project_path, worktree_path);
                    let env_diag = runtime_env_diagnostics(session_id, project_path, worktree_path);
                    return Err(anyhow::anyhow!(
                        "provider stream init failed: display={} debug={:?}; {}; {}",
                        e, e, diag, env_diag
                    ));
                }
            };

            // Accumulate the assistant turn from stream events.
            let mut turn_text = String::new();
            let mut turn_tool_calls: Vec<ContentBlock> = Vec::new();
            let mut turn_tokens_in: u32 = 0;
            let mut turn_tokens_out: u32 = 0;
            let mut interrupted: Option<&'static str> = None;
            let mut saw_round_event = false;
            let mut needs_reactive_compaction = false;

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
                        let evt = match evt {
                            Ok(e) => e,
                            Err(e) if (is_context_length_error(&e) || is_orphaned_tool_call_error(&e)) && compaction_attempts < MAX_COMPACTION_RETRIES => {
                                needs_reactive_compaction = true;
                                break;
                            }
                            Err(e) => {
                                let diag = runtime_fs_diagnostics(project_path, worktree_path);
                                let env_diag = runtime_env_diagnostics(session_id, project_path, worktree_path);
                                return Err(anyhow::anyhow!(
                                    "provider stream event failed: display={} debug={:?}; {}; {}",
                                    e, e, diag, env_diag
                                ));
                            }
                        };

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

            // ── End OTel generation span for this turn ───────────────────────
            if let Some(llm) = otel_llm {
                if interrupted.is_some() {
                    llm.end_error("interrupted");
                } else {
                    llm.record_usage(turn_tokens_in, turn_tokens_out);
                    // Record assistant output text (current turn, not stale).
                    if !turn_text.is_empty() {
                        llm.record_output(&turn_text);
                    }
                    // Record tool call names on the generation span.
                    let tool_names: Vec<String> = turn_tool_calls
                        .iter()
                        .filter_map(|tc| {
                            if let ContentBlock::ToolUse { name, .. } = tc {
                                Some(name.clone())
                            } else {
                                None
                            }
                        })
                        .collect();
                    llm.record_tool_calls(&tool_names);
                    llm.end_ok();
                }
            }

            if let Some(reason) = interrupted {
                return Err(anyhow::anyhow!(reason));
            }

            // ── Reactive compaction: mid-stream context overflow ─────────────
            if needs_reactive_compaction {
                tracing::warn!(
                    task_id = %task_id,
                    compaction_attempts,
                    "ReplyLoop: context_length_exceeded mid-stream; compacting reactively"
                );
                let compacted = crate::agent::compaction::compact_conversation(
                    provider, conversation, session_id, task_id, app_state,
                    crate::agent::compaction::CompactionContext::MidSession(agent_type),
                    context_window,
                ).await;
                if compacted {
                    total_tokens_in = 0;
                    total_tokens_out = 0;
                    compaction_attempts += 1;
                    conversation.push(crate::agent::message::Message::user(
                        "Continue with the task.",
                    ));
                    continue;
                }
                return Err(anyhow::anyhow!(
                    "context_length_exceeded and reactive compaction failed"
                ));
            }

            if !saw_round_event {
                if empty_turn_retries < MAX_EMPTY_TURN_RETRIES {
                    empty_turn_retries += 1;
                    tracing::warn!(
                        task_id = %task_id,
                        retry = empty_turn_retries,
                        "ReplyLoop: provider stream ended without events; retrying"
                    );
                    continue;
                }
                let diag = runtime_fs_diagnostics(project_path, worktree_path);
                return Err(anyhow::anyhow!(
                    "provider stream ended without any events (after {} retries); {}",
                    empty_turn_retries, diag
                ));
            }

            // ── Build the assistant message from this turn ───────────────────
            let mut assistant_content: Vec<ContentBlock> = Vec::new();
            if !turn_text.is_empty() {
                push_fragment(&mut assistant_fragments, format!("text:{}", turn_text));
                last_assistant_text = turn_text.clone();
                final_assistant_text = turn_text.clone();
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
                if empty_turn_retries < MAX_EMPTY_TURN_RETRIES {
                    empty_turn_retries += 1;
                    tracing::warn!(
                        task_id = %task_id,
                        retry = empty_turn_retries,
                        "ReplyLoop: provider returned empty assistant turn; retrying"
                    );
                    continue;
                }
                let diag = runtime_fs_diagnostics(project_path, worktree_path);
                return Err(anyhow::anyhow!(
                    "provider returned empty assistant turn (after {} retries); {}",
                    empty_turn_retries, diag
                ));
            }
            // Reset retry counter on successful content.
            empty_turn_retries = 0;

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

            // ── Compaction threshold check ────────────────────────────────────
            if crate::agent::compaction::needs_compaction(total_tokens_in, context_window) {
                tracing::info!(
                    task_id = %task_id,
                    usage_pct = total_tokens_in as f64 / context_window as f64,
                    "ReplyLoop: compaction threshold reached, compacting"
                );
                let compacted = crate::agent::compaction::compact_conversation(
                    provider,
                    conversation,
                    session_id,
                    task_id,
                    app_state,
                    crate::agent::compaction::CompactionContext::MidSession(agent_type),
                    context_window,
                )
                .await;
                if compacted {
                    // Reset token counters — the compacted conversation is much
                    // smaller.  The next turn's usage report will set accurate
                    // values for the new context size.
                    total_tokens_in = 0;
                    total_tokens_out = 0;

                    // After compaction the conversation was replaced — the
                    // assistant message containing this turn's ToolUse blocks
                    // is gone. If we dispatched tool calls and appended
                    // ToolResults, those would reference call_ids that no
                    // longer exist, causing "No tool call found for function
                    // call output" from the OpenAI API.
                    //
                    // Skip tool dispatch and let the LLM produce a fresh
                    // response against the compacted conversation.
                    if !turn_tool_calls.is_empty() {
                        compaction_attempts += 1;
                        conversation.push(Message::user(
                            "Continue with the task.",
                        ));
                        continue;
                    }
                }
                // Text-only after compaction = worker thinks it's done.
                // Let it fall through to the normal text-only exit below —
                // the reviewer will validate and resume if needed.
            }

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

            // Maximum characters per tool result to prevent context overflow.
            // ~30k chars ≈ 7.5k tokens — enough for diagnosis, safe with multiple calls.
            const MAX_TOOL_RESULT_CHARS: usize = 30_000;

            // Dispatch tool calls concurrently and collect results.
            // Each tool call gets its own OTel span as a child of the session.
            let tool_futures: Vec<_> = turn_tool_calls
                .iter()
                .filter_map(|tool_call| {
                    let ContentBlock::ToolUse { id, name, input } = tool_call else {
                        return None;
                    };
                    tracing::debug!(
                        task_id = %task_id,
                        tool = %name,
                        tool_use_id = %id,
                        "ReplyLoop: dispatching tool call"
                    );
                    let id = id.clone();
                    let name = name.clone();
                    let input_json = input.clone();
                    let args = match input {
                        serde_json::Value::Object(map) => Some(map.clone()),
                        _ => None,
                    };

                    // Start tool span before the async block so it parents correctly.
                    let tool_span = otel_session.as_ref().map(|session| {
                        let ts = telemetry::ToolSpan::start(session.context(), &name, &id);
                        ts.record_input(&input_json.to_string());
                        ts
                    });

                    Some(async move {
                        let (content, is_error) =
                            match extension::call_tool(app_state, &name, args, worktree_path).await {
                                Ok(value) => {
                                    let mut text = if value.is_string() {
                                        value.as_str().unwrap_or("").to_string()
                                    } else {
                                        value.to_string()
                                    };
                                    if text.len() > MAX_TOOL_RESULT_CHARS {
                                        let truncated_len = text.len();
                                        // Find a clean UTF-8 boundary at or before the limit,
                                        // then truncate. (truncate panics on non-boundaries.)
                                        let mut end = MAX_TOOL_RESULT_CHARS;
                                        while end > 0 && !text.is_char_boundary(end) {
                                            end -= 1;
                                        }
                                        text.truncate(end);
                                        text.push_str(&format!(
                                            "\n\n[OUTPUT TRUNCATED — showing {MAX_TOOL_RESULT_CHARS} of {truncated_len} chars. Narrow your query for full results.]"
                                        ));
                                    }
                                    if let Some(ts) = &tool_span {
                                        ts.record_output(&text, false);
                                    }
                                    (vec![ContentBlock::Text { text }], false)
                                }
                                Err(err) => {
                                    tracing::warn!(
                                        task_id = %task_id,
                                        tool = %name,
                                        error = %err,
                                        "ReplyLoop: tool call returned error"
                                    );
                                    let err_text = format!("error: {err}");
                                    if let Some(ts) = &tool_span {
                                        ts.record_output(&err_text, true);
                                    }
                                    (
                                        vec![ContentBlock::Text {
                                            text: err_text,
                                        }],
                                        true,
                                    )
                                }
                            };
                        // End tool span.
                        if let Some(ts) = tool_span {
                            if is_error {
                                ts.end_error("tool returned error");
                            } else {
                                ts.end_ok();
                            }
                        }
                        ContentBlock::ToolResult {
                            tool_use_id: id,
                            content,
                            is_error,
                        }
                    })
                })
                .collect();
            let tool_result_blocks = futures::future::join_all(tool_futures).await;

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
            if is_resumed_session {
                // A resumed worker that responds text-only with zero tool calls
                // is NOT making progress — it saw its own prior "done" message
                // and short-circuited.  Treat this as an error so the lifecycle
                // does NOT send it to verification (which would create a review
                // doom loop: verify→review→reject→resume→text-only→verify…).
                tracing::warn!(
                    task_id = %task_id,
                    agent_type = %agent_type.as_str(),
                    "ReplyLoop: resumed session ended text-only (no tool calls = no progress)"
                );
                return Err(anyhow::anyhow!(
                    "resumed worker ended without any tool use (no progress made on reviewer feedback)"
                ));
            } else {
                let reason = match agent_type {
                    AgentType::Worker | AgentType::ConflictResolver => {
                        "worker ended without any tool use (provider error?)"
                    }
                    AgentType::TaskReviewer => {
                        "task reviewer ended without any tool use (provider error?)"
                    }
                    AgentType::PM => "PM agent ended without any tool use (provider error?)",
                    AgentType::Groomer => {
                        "groomer agent ended without any tool use (provider error?)"
                    }
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

    // ── End session-level OTel span ──────────────────────────────────────────
    if let Some(session) = otel_session {
        session.record_usage(total_tokens_in, total_tokens_out);
        // Record the last assistant text as the trace-level output
        // (shows in the Langfuse trace list Output column).
        if !final_assistant_text.is_empty() {
            session.record_trace_output(&final_assistant_text);
        }
        match &run_result {
            Ok(()) => session.end_ok(),
            Err(e) => session.end_error(&e.to_string()),
        }
    }

    (
        run_result,
        output,
        total_tokens_in as i64,
        total_tokens_out as i64,
    )
}
