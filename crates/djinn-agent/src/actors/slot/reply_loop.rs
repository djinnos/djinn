use std::path::Path;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::context::AgentContext;
use crate::extension;
use crate::message::{ContentBlock, Conversation, Message, MessageMeta, Role};
use crate::output_parser::ParsedAgentOutput;
use crate::provider::telemetry;
use crate::provider::{LlmProvider, StreamEvent};
use djinn_core::events::DjinnEventEnvelope;

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

pub(crate) struct ReplyLoopContext<'a> {
    pub provider: &'a dyn LlmProvider,
    pub tools: &'a [serde_json::Value],
    pub task_id: &'a str,
    pub task_short_id: &'a str,
    pub session_id: &'a str,
    pub project_path: &'a str,
    pub worktree_path: &'a Path,
    pub role_name: &'a str,
    pub context_window: i64,
    pub model_id: &'a str,
    pub cancel: &'a CancellationToken,
    pub global_cancel: &'a CancellationToken,
    pub app_state: &'a AgentContext,
}

/// Djinn-native reply loop. Drives an `LlmProvider` stream, dispatches tool
/// calls via the extension layer, and continues until the assistant produces a
/// text-only response or a termination condition is reached.
///
/// Context-length-exceeded errors trigger reactive compaction and retry
/// (up to `MAX_COMPACTION_RETRIES` times) before failing the session.
pub(super) async fn run_reply_loop(
    ctx: ReplyLoopContext<'_>,
    conversation: &mut Conversation,
    is_resumed_session: bool,
) -> (anyhow::Result<()>, ParsedAgentOutput, i64, i64) {
    let ReplyLoopContext { provider, tools, task_id, task_short_id, session_id, project_path, worktree_path, role_name, context_window, model_id, cancel, global_cancel, app_state } = ctx;
    let mut output = ParsedAgentOutput::new(role_name == "task_reviewer");

    // Token counts and last assistant text are declared outside the async block
    // so they survive the borrow and can be used for telemetry/return values.
    let mut total_tokens_in: u32 = 0;
    let mut total_tokens_out: u32 = 0;
    // Tracks the actual context-window fill for the most recent generation.
    // Each generation sends the entire conversation, so `usage.input` IS the
    // current context size — it overwrites the previous value rather than
    // accumulating.  This is the correct metric for the compaction threshold
    // and for the usage_pct SSE event.  `total_tokens_in` is kept as a
    // billing / telemetry aggregate (sum across all turns).
    let mut current_context_tokens: u32 = 0;
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
            agent_type: role_name,
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
                    let compacted = crate::compaction::compact_conversation(
                        provider, conversation, session_id, task_id, app_state,
                        crate::compaction::CompactionContext::MidSession(role_name.to_string()),
                        context_window,
                    ).await;
                    if compacted {
                        total_tokens_in = 0;
                        total_tokens_out = 0;
                        current_context_tokens = 0;
                        compaction_attempts += 1;
                        conversation.push(crate::message::Message::user(
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
                                app_state.event_bus.send(DjinnEventEnvelope::session_message(
                                    session_id,
                                    task_id,
                                    role_name,
                                    &serde_json::json!({
                                        "type": "delta",
                                        "role": "assistant",
                                        "text": text,
                                    }),
                                ));
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
                                // Overwrite (don't sum): each generation's input
                                // tokens represent the full current context size.
                                current_context_tokens = usage.input;
                                total_tokens_in = total_tokens_in.saturating_add(usage.input);
                                total_tokens_out = total_tokens_out.saturating_add(usage.output);

                                let usage_pct = if context_window > 0 {
                                    current_context_tokens as f64 / context_window as f64
                                } else {
                                    0.0
                                };
                                app_state.event_bus.send(DjinnEventEnvelope::session_token_update(
                                    session_id,
                                    task_id,
                                    current_context_tokens as i64,
                                    total_tokens_out as i64,
                                    context_window,
                                    usage_pct,
                                ));
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
                let compacted = crate::compaction::compact_conversation(
                    provider, conversation, session_id, task_id, app_state,
                    crate::compaction::CompactionContext::MidSession(role_name.to_string()),
                    context_window,
                ).await;
                if compacted {
                    total_tokens_in = 0;
                    total_tokens_out = 0;
                    current_context_tokens = 0;
                    compaction_attempts += 1;
                    conversation.push(crate::message::Message::user(
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
            app_state.event_bus.send(DjinnEventEnvelope::session_message(
                session_id,
                task_id,
                role_name,
                &serialize_message(&assistant_msg),
            ));

            conversation.push(assistant_msg);

            // ── Compaction threshold check ────────────────────────────────────
            if crate::compaction::needs_compaction(current_context_tokens, context_window) {
                tracing::info!(
                    task_id = %task_id,
                    current_context_tokens,
                    context_window,
                    usage_pct = current_context_tokens as f64 / context_window as f64,
                    "ReplyLoop: compaction threshold reached, compacting"
                );
                let compacted = crate::compaction::compact_conversation(
                    provider,
                    conversation,
                    session_id,
                    task_id,
                    app_state,
                    crate::compaction::CompactionContext::MidSession(role_name.to_string()),
                    context_window,
                )
                .await;
                if compacted {
                    // Reset token counters — the compacted conversation is much
                    // smaller.  The next turn's usage report will set accurate
                    // values for the new context size.
                    total_tokens_in = 0;
                    total_tokens_out = 0;
                    current_context_tokens = 0;

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
                    agent_type = %role_name,
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
                        // Retry logic for SQLite BUSY errors (concurrent tool
                        // calls from the same generation can contend on the
                        // write lock).
                        let mut result =
                            extension::call_tool(app_state, &name, args.clone(), worktree_path)
                                .await;
                        {
                            let mut retries = 0u32;
                            while retries < 5 {
                                match &result {
                                    Err(e) if e.contains("database is locked") => {
                                        retries += 1;
                                        let backoff = std::time::Duration::from_millis(
                                            100 * (1 << retries.min(4)),
                                        );
                                        tracing::warn!(
                                            task_id = %task_id,
                                            tool = %name,
                                            retry = retries,
                                            backoff_ms = backoff.as_millis() as u64,
                                            "ReplyLoop: database locked, retrying"
                                        );
                                        tokio::time::sleep(backoff).await;
                                        result = extension::call_tool(
                                            app_state,
                                            &name,
                                            args.clone(),
                                            worktree_path,
                                        )
                                        .await;
                                    }
                                    _ => break,
                                }
                            }
                        }
                        let (content, is_error) =
                            match result {
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
                    agent_type = %role_name,
                    "ReplyLoop: resumed session ended text-only (no tool calls = no progress)"
                );
                return Err(anyhow::anyhow!(
                    "resumed worker ended without any tool use (no progress made on reviewer feedback)"
                ));
            } else {
                let reason = match role_name {
                    "worker" | "conflict_resolver" => {
                        "worker ended without any tool use (provider error?)"
                    }
                    "task_reviewer" => {
                        "task reviewer ended without any tool use (provider error?)"
                    }
                    "pm" => "PM agent ended without any tool use (provider error?)",
                    "groomer" => {
                        "groomer agent ended without any tool use (provider error?)"
                    }
                    _ => "agent ended without any tool use (provider error?)",
                };
                tracing::warn!(
                    task_id = %task_id,
                    agent_type = %role_name,
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
            agent_type = %role_name,
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

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ContentBlock, Conversation, Message};
    use crate::provider::{LlmProvider, StreamEvent, TokenUsage};
    use crate::test_helpers;
    use djinn_db::{SessionMessageRepository, SessionRepository};
    use futures::stream;
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    // ── MockLlmProvider ───────────────────────────────────────────────────────

    /// Pre-scripted response: text (optional) + tool calls + token counts.
    struct MockResponse {
        text: Option<String>,
        tool_calls: Vec<ContentBlock>,
        input_tokens: u32,
        output_tokens: u32,
    }

    impl MockResponse {
        fn text_only(text: &str, input_tokens: u32) -> Self {
            Self {
                text: Some(text.to_string()),
                tool_calls: vec![],
                input_tokens,
                output_tokens: 10,
            }
        }

        fn tool_call(id: &str, name: &str, input_tokens: u32) -> Self {
            Self {
                text: None,
                tool_calls: vec![ContentBlock::ToolUse {
                    id: id.to_string(),
                    name: name.to_string(),
                    input: serde_json::json!({}),
                }],
                input_tokens,
                output_tokens: 10,
            }
        }
    }

    /// An `LlmProvider` that pops from a fixed queue of `MockResponse`s.
    /// When the queue is empty it returns a text-only "fallback done" response
    /// so that the loop always terminates.
    struct MockProvider {
        responses: Arc<Mutex<VecDeque<MockResponse>>>,
    }

    impl MockProvider {
        fn new(responses: Vec<MockResponse>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into())),
            }
        }

        fn remaining(&self) -> usize {
            self.responses.lock().unwrap().len()
        }
    }

    impl LlmProvider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn stream<'a>(
            &'a self,
            _conversation: &'a Conversation,
            _tools: &'a [serde_json::Value],
        ) -> Pin<
            Box<
                dyn futures::Future<
                        Output = anyhow::Result<
                            Pin<
                                Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                            >,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            let responses = Arc::clone(&self.responses);
            Box::pin(async move {
                let resp = responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| MockResponse::text_only("fallback done", 50));

                let mut events: Vec<anyhow::Result<StreamEvent>> = vec![];
                if let Some(text) = resp.text {
                    events.push(Ok(StreamEvent::Delta(ContentBlock::Text { text })));
                }
                for tc in resp.tool_calls {
                    events.push(Ok(StreamEvent::Delta(tc)));
                }
                events.push(Ok(StreamEvent::Usage(TokenUsage {
                    input: resp.input_tokens,
                    output: resp.output_tokens,
                })));
                events.push(Ok(StreamEvent::Done));

                Ok(Box::pin(stream::iter(events))
                    as Pin<
                        Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                    >)
            })
        }
    }

    // ── Test helpers ──────────────────────────────────────────────────────────

    /// Returns (context, project_path, task_id, session_id, cancel).
    async fn make_context() -> (
        crate::context::AgentContext,
        String,
        String,
        String,
        CancellationToken,
    ) {
        let cancel = CancellationToken::new();
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
        // Create a real session row so session_messages FK constraint is satisfied.
        let session_repo = SessionRepository::new(db.clone(), ctx.event_bus.clone());
        let session = session_repo
            .create(
                &project.id,
                Some(&task.id),
                "test/mock-model",
                "worker",
                None,
                None,
            )
            .await
            .expect("create session");
        (ctx, project.path, task.id, session.id, cancel)
    }

    async fn count_persisted_messages(
        app_state: &crate::context::AgentContext,
        session_id: &str,
    ) -> usize {
        let repo = SessionMessageRepository::new(app_state.db.clone(), app_state.event_bus.clone());
        repo.load_conversation(session_id)
            .await
            .map(|c| c.messages.len())
            .unwrap_or(0)
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    /// A single ToolUse turn above the compaction threshold triggers compaction,
    /// persists messages to DB, and replaces the conversation. The session then
    /// continues with the compacted context and ends normally.
    #[tokio::test]
    async fn proactive_compaction_fires_when_current_context_exceeds_threshold() {
        // context_window = 10,000 → threshold = 8,000 tokens
        let context_window = 10_000_i64;

        // Turn 1: ToolUse + 8,500 input tokens → above threshold → compaction fires.
        //         Tool dispatch is skipped when compaction fires (conversation replaced).
        // Turn 2: compaction LLM call → summary text returned.
        // Turn 3: "Continue with the task." → text-only → ends session.
        let provider = MockProvider::new(vec![
            MockResponse::tool_call("t1", "nonexistent_tool", 8_500),
            MockResponse::text_only("Summary: worked on the task using shell tools.", 200),
            MockResponse::text_only("Completed the task.", 300),
        ]);

        let (app_state, project_path, task_id, session_id, cancel) = make_context().await;
        let worktree_path = std::path::PathBuf::from("/tmp");
        let mut conv = Conversation::new();
        conv.push(Message::system("You are a worker."));
        conv.push(Message::user("Do the task."));

        let (result, _output, _tokens_in, _tokens_out) = run_reply_loop(
            ReplyLoopContext {
                provider: &provider,
                tools: &[],
                task_id: &task_id,
                task_short_id: "t1",
                session_id: &session_id,
                project_path: &project_path,
                worktree_path: &worktree_path,
                role_name: "worker",
                context_window,
                model_id: "test/mock-model",
                cancel: &cancel,
                global_cancel: &cancel,
                app_state: &app_state,
            },
            &mut conv,
            false,
        )
        .await;

        // Session should end successfully (compacted + continued).
        assert!(result.is_ok(), "expected ok, got: {:?}", result);

        // All 3 mock responses were consumed.
        assert_eq!(
            provider.remaining(),
            0,
            "all mock responses should be consumed"
        );

        // Messages were persisted to DB before compaction.
        let persisted = count_persisted_messages(&app_state, &session_id).await;
        assert!(
            persisted > 0,
            "expected session messages persisted before compaction, got 0"
        );

        // Conversation was replaced by compaction then continued.
        // Expected: [system, summary_user, ack_assistant, last_user_task,
        //            continue_user, final_assistant] = 6 messages.
        // The key check is that it's much smaller than an uncompacted session
        // and that the system prompt is first.
        assert!(
            conv.messages.len() <= 7,
            "conversation should be compact after compaction, got {} messages",
            conv.messages.len()
        );
        assert_eq!(
            conv.messages[0].role,
            crate::message::Role::System,
            "first message should still be the system prompt"
        );
    }

    /// Compaction must NOT fire based on the cumulative sum of input tokens across
    /// turns.  Even if the running sum exceeds the threshold, only the current
    /// turn's input token count (the actual context window fill) matters.
    ///
    /// Pattern: each turn adds tokens at a rate that would push the SUM above the
    /// threshold quickly, but the actual context (latest generation input) stays
    /// well below 80%.
    #[tokio::test]
    async fn no_compaction_when_sum_large_but_current_context_small() {
        // context_window = 10,000 → threshold = 8,000 tokens
        let context_window = 10_000_i64;

        // Turn 1: ToolUse + 7,500 input  (sum=7_500, current=7_500 → below threshold)
        // Turn 2: ToolUse + 7,800 input  (sum=15_300, current=7_800 → below threshold)
        //   With the OLD sum-based check: sum 15,300 > 8,000 → compaction would wrongly fire.
        //   With the NEW current-context check: 7,800 < 8,000 → no compaction. ✓
        // Turn 3: text-only "done" + 100 input  (ends session normally)
        let provider = MockProvider::new(vec![
            MockResponse::tool_call("t1", "nonexistent_tool", 7_500),
            MockResponse::tool_call("t2", "nonexistent_tool", 7_800),
            MockResponse::text_only("Completed.", 100),
        ]);

        let (app_state, project_path, task_id, session_id, cancel) = make_context().await;
        let worktree_path = std::path::PathBuf::from("/tmp");
        let mut conv = Conversation::new();
        conv.push(Message::system("You are a worker."));
        conv.push(Message::user("Do the task."));

        let (result, _output, _tokens_in, _tokens_out) = run_reply_loop(
            ReplyLoopContext {
                provider: &provider,
                tools: &[],
                task_id: &task_id,
                task_short_id: "t1",
                session_id: &session_id,
                project_path: &project_path,
                worktree_path: &worktree_path,
                role_name: "worker",
                context_window,
                model_id: "test/mock-model",
                cancel: &cancel,
                global_cancel: &cancel,
                app_state: &app_state,
            },
            &mut conv,
            false,
        )
        .await;

        assert!(result.is_ok(), "expected ok, got: {:?}", result);
        assert_eq!(
            provider.remaining(),
            0,
            "all 3 mock responses should be consumed"
        );

        // No compaction should have fired: DB has NO persisted session messages.
        let persisted = count_persisted_messages(&app_state, &session_id).await;
        assert_eq!(
            persisted, 0,
            "compaction should not have fired (no messages persisted), but found {persisted}"
        );
    }

    /// Reactive compaction fires when the provider itself signals a
    /// context-length error.  The session compacts and retries successfully.
    #[tokio::test]
    async fn reactive_compaction_on_context_length_error() {
        let context_window = 10_000_i64;

        // Provider behaviour:
        //   • Turn 1: ToolUse + small tokens (below threshold).
        //   • Turn 2 attempt: context_length error mid-stream → reactive compaction triggered.
        //   • Compaction call: summary returned.
        //   • Turn 2 retry: text-only → session ends.
        //
        // We simulate the context-length error by injecting an error event
        // BEFORE the ToolUse delta, so the stream init itself fails.
        struct ErrorOnSecondCallProvider {
            call_count: Arc<Mutex<u32>>,
            inner: MockProvider,
        }

        impl LlmProvider for ErrorOnSecondCallProvider {
            fn name(&self) -> &str {
                "mock-error"
            }

            fn stream<'a>(
                &'a self,
                conversation: &'a Conversation,
                tools: &'a [serde_json::Value],
            ) -> Pin<
                Box<
                    dyn futures::Future<
                            Output = anyhow::Result<
                                Pin<
                                    Box<
                                        dyn futures::Stream<Item = anyhow::Result<StreamEvent>>
                                            + Send,
                                    >,
                                >,
                            >,
                        > + Send
                        + 'a,
                >,
            > {
                let count = Arc::clone(&self.call_count);
                let inner = &self.inner;
                let turn = {
                    let mut n = count.lock().unwrap();
                    *n += 1;
                    *n
                };
                if turn == 2 {
                    // Simulate a context-length-exceeded error on stream init.
                    Box::pin(async move { Err(anyhow::anyhow!("context_length exceeded")) })
                } else {
                    inner.stream(conversation, tools)
                }
            }
        }

        let inner = MockProvider::new(vec![
            // Call 1: normal ToolUse turn.
            MockResponse::tool_call("t1", "nonexistent_tool", 500),
            // Call 2 would error (handled above).
            // Call 3: compaction LLM summary.
            MockResponse::text_only("Summary: used nonexistent_tool.", 100),
            // Call 4: continuation after compaction.
            MockResponse::text_only("Done.", 120),
        ]);
        let provider = ErrorOnSecondCallProvider {
            call_count: Arc::new(Mutex::new(0)),
            inner,
        };

        let (app_state, project_path, task_id, session_id, cancel) = make_context().await;
        let worktree_path = std::path::PathBuf::from("/tmp");
        let mut conv = Conversation::new();
        conv.push(Message::system("You are a worker."));
        conv.push(Message::user("Do the task."));

        let (result, _output, _tokens_in, _tokens_out) = run_reply_loop(
            ReplyLoopContext {
                provider: &provider,
                tools: &[],
                task_id: &task_id,
                task_short_id: "t1",
                session_id: &session_id,
                project_path: &project_path,
                worktree_path: &worktree_path,
                role_name: "worker",
                context_window,
                model_id: "test/mock-model",
                cancel: &cancel,
                global_cancel: &cancel,
                app_state: &app_state,
            },
            &mut conv,
            false,
        )
        .await;

        assert!(
            result.is_ok(),
            "expected ok after reactive compaction, got: {:?}",
            result
        );

        // Compaction fired → messages persisted.
        let persisted = count_persisted_messages(&app_state, &session_id).await;
        assert!(
            persisted > 0,
            "expected session messages persisted by reactive compaction"
        );
    }
}
