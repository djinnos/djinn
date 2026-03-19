use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::context::AgentContext;
use crate::extension;
use crate::message::{ContentBlock, Conversation, Message, MessageMeta, Role};
use crate::output_parser::ParsedAgentOutput;
use crate::output_stash::OutputStash;
use crate::provider::telemetry;
use crate::provider::{LlmProvider, StreamEvent, ToolChoice};
use djinn_core::events::DjinnEventEnvelope;

use super::*;

const MAX_TURNS: u32 = 1000;
/// Maximum retries for empty assistant turns before treating as a hard failure.
const MAX_EMPTY_TURN_RETRIES: u32 = 2;
/// Maximum consecutive text-only turns before treating as a session failure.
/// Each text-only turn without a finalize tool call triggers a nudge message.
const MAX_NUDGE_ATTEMPTS: u32 = 3;

fn is_context_length_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string().to_lowercase();
    msg.contains("context_length")
        || msg.contains("context limit")
        || msg.contains("too many tokens")
        || msg.contains("maximum context")
        || msg.contains("context window")
        || msg.contains("prompt is too long")
        || msg.contains("max_tokens")
        || msg.contains("token limit")
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

fn serialize_llm_input(
    conversation: &Conversation,
    tools: &[serde_json::Value],
) -> serde_json::Value {
    serde_json::json!({
        "messages": conversation.to_openai_messages(),
        "tools": tools,
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

/// Extract browsable content for the output stash.
///
/// For shell results, the LLM wants to browse raw stdout/stderr — not the
/// `{"ok":true,"stdout":"..."}` JSON envelope.  For other tools the
/// pretty-printed JSON is already useful, so we return `None` to let the
/// caller fall back to the default.
fn extract_stash_content(tool_name: &str, value: &serde_json::Value) -> Option<String> {
    if tool_name != "shell" {
        return None;
    }
    let obj = value.as_object()?;
    let stdout = obj.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
    let stderr = obj.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
    let exit_code = obj.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(-1);

    let mut out = String::with_capacity(stdout.len() + stderr.len() + 64);
    if !stdout.is_empty() {
        out.push_str(stdout);
    }
    if !stderr.is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("--- stderr ---\n");
        out.push_str(stderr);
    }
    if exit_code != 0 {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("[exit code: {exit_code}]"));
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
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
    /// Tool names that signal session completion (ADR-036).
    /// The first entry is the primary finalize tool; additional entries are
    /// alternate exit paths (e.g. `request_pm`).
    pub finalize_tool_names: &'a [&'a str],
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
    let ReplyLoopContext {
        provider,
        tools,
        task_id,
        task_short_id,
        session_id,
        project_path,
        worktree_path,
        role_name,
        finalize_tool_names,
        context_window,
        model_id,
        cancel,
        global_cancel,
        app_state,
    } = ctx;

    // Register activity tracker — stall detection uses this to kill idle sessions.
    let activity_ts = app_state.register_activity(task_id);

    // Session-scoped stash for full tool outputs that exceed truncation limits.
    // The agent can navigate stashed outputs via `output_view` and `output_grep`.
    let output_stash = Arc::new(Mutex::new(OutputStash::new()));

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
        let mut assistant_message_count: usize = 0;
        let mut assistant_fragments: Vec<String> = Vec::new();
        let mut compaction_attempts: u32 = 0;
        let mut empty_turn_retries: u32 = 0;
        // Consecutive text-only turns without a finalize or tool call (for nudge loop).
        let mut consecutive_nudge_count: u32 = 0;

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
                let input = serialize_llm_input(conversation, tools);
                llm.record_input(&serde_json::to_string(&input).unwrap());
                llm
            });

            // ── Start streaming from the provider ────────────────────────────
            let tool_choice = if tools.is_empty() {
                None
            } else {
                Some(ToolChoice::Required)
            };
            let stream_result = provider.stream(conversation, tools, tool_choice).await;
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
                        output_stash.lock().unwrap().clear();
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

                        // Touch activity on every stream event — proves the session is alive.
                        {
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            activity_ts.store(now, Ordering::Relaxed);
                        }

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
                            StreamEvent::Thinking(_) => {
                                // Reasoning tokens (Kimi K2.5, DeepSeek-R1, GLM, etc.)
                                // Activity timestamp already updated above — nothing else
                                // to do; thinking tokens aren't stored in conversation.
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
                    output_stash.lock().unwrap().clear();
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
                    output_stash.lock().unwrap().clear();

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

            // ── Finalize-tool detection (ADR-036) ────────────────────────────
            // The primary finalize tool (first in the list, e.g. submit_work) is
            // a virtual tool — its payload is captured here and processed after
            // the loop by finalize_handlers. We break immediately (no dispatch).
            //
            // Alternate finalize tools (e.g. request_pm) are real extension tools
            // that must be dispatched to execute their side effects. We mark them
            // but fall through to the dispatch section; a post-dispatch check
            // breaks the loop after the tool results are collected.
            let primary_finalize = finalize_tool_names.first().copied().unwrap_or("");
            if let Some(finalize_call) = turn_tool_calls
                .iter()
                .find(|tc| matches!(tc, ContentBlock::ToolUse { name, .. } if name == primary_finalize))
            {
                let payload = if let ContentBlock::ToolUse { input, .. } = finalize_call {
                    input.clone()
                } else {
                    serde_json::Value::Null
                };
                tracing::info!(
                    task_id = %task_id,
                    agent_type = %role_name,
                    finalize_tool = %primary_finalize,
                    turns,
                    assistant_message_count,
                    "ReplyLoop: primary finalize tool called — session complete"
                );
                output.finalize_payload = Some(payload);
                output.finalize_tool_name = Some(primary_finalize.to_string());
                break;
            }
            // Check for alternate finalize tools — mark but don't break yet.
            let alternate_finalize = turn_tool_calls
                .iter()
                .find(|tc| matches!(tc, ContentBlock::ToolUse { name, .. } if finalize_tool_names[1..].contains(&name.as_str())))
                .and_then(|tc| if let ContentBlock::ToolUse { name, input, .. } = tc {
                    Some((name.clone(), input.clone()))
                } else {
                    None
                });

            // ── Nudge loop: text-only without finalize ────────────────────────
            if turn_tool_calls.is_empty() {
                if tools.is_empty() {
                    // No tools registered at all → text-only is a valid session end.
                    tracing::info!(
                        task_id = %task_id,
                        agent_type = %role_name,
                        turns,
                        assistant_message_count,
                        "ReplyLoop: text-only turn (no tools) — session complete"
                    );
                    break;
                }
                let finalize_list = finalize_tool_names.join("` or `");
                consecutive_nudge_count += 1;
                if consecutive_nudge_count >= MAX_NUDGE_ATTEMPTS {
                    return Err(anyhow::anyhow!(
                        "session failed: {} consecutive text-only responses without calling {}",
                        consecutive_nudge_count,
                        finalize_list
                    ));
                }
                tracing::warn!(
                    task_id = %task_id,
                    agent_type = %role_name,
                    nudge = consecutive_nudge_count,
                    finalize_tools = %finalize_list,
                    "ReplyLoop: text-only turn without finalize — injecting nudge"
                );
                conversation.push(Message::user(format!(
                    "You have not completed your session. You MUST call `{finalize_list}` \
                     when you are done. If you still have work to do, use the appropriate tools \
                     to continue. If you are done, call one of these tools now."
                )));
                continue;
            }

            // Non-finalize tool calls: reset nudge counter and dispatch normally.
            consecutive_nudge_count = 0;

            // ── Dispatch tool calls ──────────────────────────────────────────

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

                    let stash = Arc::clone(&output_stash);
                    Some(async move {
                        // ── Intercept stash-navigation tools (no extension dispatch needed) ──
                        if name == "output_view" || name == "output_grep" {
                            let result = {
                                let guard = stash.lock().unwrap();
                                let args_map = args.as_ref();
                                if name == "output_view" {
                                    let tid = args_map
                                        .and_then(|m| m.get("tool_use_id"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    let offset = args_map
                                        .and_then(|m| m.get("offset"))
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0) as usize;
                                    let limit = args_map
                                        .and_then(|m| m.get("limit"))
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(200) as usize;
                                    guard.view(tid, offset, limit)
                                } else {
                                    let tid = args_map
                                        .and_then(|m| m.get("tool_use_id"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    let pattern = args_map
                                        .and_then(|m| m.get("pattern"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    let ctx_lines = args_map
                                        .and_then(|m| m.get("context_lines"))
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(3) as usize;
                                    guard.grep(tid, pattern, ctx_lines)
                                }
                            };
                            let (content, is_error) = match result {
                                Ok(text) => {
                                    if let Some(ts) = &tool_span {
                                        ts.record_output(&text, false);
                                    }
                                    (vec![ContentBlock::Text { text }], false)
                                }
                                Err(err) => {
                                    if let Some(ts) = &tool_span {
                                        ts.record_output(&err, true);
                                    }
                                    (vec![ContentBlock::Text { text: format!("error: {err}") }], true)
                                }
                            };
                            if let Some(ts) = tool_span {
                                if is_error { ts.end_error("tool returned error"); } else { ts.end_ok(); }
                            }
                            return ContentBlock::ToolResult {
                                tool_use_id: id,
                                content,
                                is_error,
                            };
                        }

                        // ── Normal tool dispatch ────────────────────────────────
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
                                        // Pretty-print JSON so truncation and stash
                                        // navigation have real line structure (not a
                                        // single compact JSON line).
                                        serde_json::to_string_pretty(&value)
                                            .unwrap_or_else(|_| value.to_string())
                                    };
                                    if text.len() > MAX_TOOL_RESULT_CHARS {
                                        // Stash the browsable content before truncating.
                                        // For shell results, extract raw stdout+stderr
                                        // (the LLM wants to browse command output, not
                                        // the JSON envelope). For other tools, stash
                                        // the pretty-printed JSON as-is.
                                        let stash_text = extract_stash_content(&name, &value)
                                            .unwrap_or_else(|| text.clone());
                                        stash.lock().unwrap().insert(
                                            id.clone(),
                                            name.clone(),
                                            stash_text,
                                        );
                                        let full_bytes = text.len();
                                        text = crate::truncate::smart_truncate(
                                            &text,
                                            MAX_TOOL_RESULT_CHARS,
                                        );
                                        // Append navigation hint so the agent knows it can browse.
                                        text.push_str(&format!(
                                            "\n\n[Full output stashed ({full_bytes} bytes). \
                                             Use output_view(tool_use_id=\"{id}\") to paginate \
                                             or output_grep(tool_use_id=\"{id}\", pattern=\"...\") to search.]"
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

            // Touch activity after tool execution — tool calls are legitimate
            // work and can take a while (e.g. cargo build).
            {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                activity_ts.store(now, Ordering::Relaxed);
            }

            // Push a user message containing all tool results.
            let tool_result_msg = Message {
                role: Role::User,
                content: tool_result_blocks,
                metadata: None,
            };
            conversation.push(tool_result_msg);

            // ── Post-dispatch finalize check for alternate finalize tools ─────
            // Alternate finalize tools (e.g. request_pm) were dispatched above
            // so their side effects ran. Now break the loop.
            if let Some((name, payload)) = alternate_finalize {
                tracing::info!(
                    task_id = %task_id,
                    agent_type = %role_name,
                    finalize_tool = %name,
                    turns,
                    assistant_message_count,
                    "ReplyLoop: alternate finalize tool dispatched — session complete"
                );
                output.finalize_payload = Some(payload);
                output.finalize_tool_name = Some(name);
                break;
            }

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

        tracing::info!(
            task_id = %task_id,
            agent_type = %role_name,
            saw_any_event,
            assistant_message_count,
            turns,
            finalize_called = output.finalize_payload.is_some(),
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

    // Deregister activity tracker — session is done.
    app_state.deregister_activity(task_id);

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
    use djinn_core::message::Role;
    use djinn_db::repositories::session::CreateSessionParams;
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
            _tool_choice: Option<ToolChoice>,
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
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: Some(&task.id),
                model: "test/mock-model",
                agent_type: "worker",
                worktree_path: None,
                metadata_json: None,
            })
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

    // ── extract_stash_content tests ────────────────────────────────────────────

    #[test]
    fn extract_stash_content_shell_extracts_stdout() {
        let value = serde_json::json!({
            "ok": true,
            "exit_code": 0,
            "stdout": "line 1\nline 2\nline 3\n",
            "stderr": "",
            "workdir": "/tmp"
        });
        let result = extract_stash_content("shell", &value).unwrap();
        assert!(result.contains("line 1"));
        assert!(result.contains("line 3"));
        assert!(!result.contains("workdir"));
        assert!(!result.contains("exit code"));
    }

    #[test]
    fn extract_stash_content_shell_includes_stderr_and_exit_code() {
        let value = serde_json::json!({
            "ok": false,
            "exit_code": 1,
            "stdout": "building...\n",
            "stderr": "error: failed\n",
            "workdir": "/tmp"
        });
        let result = extract_stash_content("shell", &value).unwrap();
        assert!(result.contains("building..."));
        assert!(result.contains("--- stderr ---"));
        assert!(result.contains("error: failed"));
        assert!(result.contains("[exit code: 1]"));
    }

    #[test]
    fn extract_stash_content_non_shell_returns_none() {
        let value = serde_json::json!({"tasks": []});
        assert!(extract_stash_content("task_list", &value).is_none());
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
                finalize_tool_names: &["submit_work", "request_pm"],
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
                finalize_tool_names: &["submit_work", "request_pm"],
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
                tool_choice: Option<ToolChoice>,
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
                    inner.stream(conversation, tools, tool_choice)
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
                finalize_tool_names: &["submit_work", "request_pm"],
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

    #[test]
    fn serialize_llm_input_preserves_system_tools_and_full_history_order() {
        let mut conversation = Conversation::new();
        conversation.push(Message::system("You are a worker."));
        conversation.push(Message::user("First request"));
        conversation.push(Message::assistant("First reply"));
        conversation.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tool_1".into(),
                name: "shell".into(),
                input: serde_json::json!({"command": "pwd"}),
            }],
            metadata: None,
        });
        conversation.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tool_1".into(),
                content: vec![ContentBlock::text("/tmp")],
                is_error: false,
            }],
            metadata: None,
        });
        conversation.push(Message::user("Second request"));

        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": "shell",
                "description": "Run shell commands",
                "parameters": {"type": "object"}
            }
        })];

        let input = serialize_llm_input(&conversation, &tools);

        assert_eq!(input["tools"], serde_json::json!(tools));
        let messages = input["messages"].as_array().expect("messages array");
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "You are a worker.");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "First request");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "First reply");
        assert_eq!(messages[3]["role"], "assistant");
        assert_eq!(messages[3]["tool_calls"][0]["id"], "tool_1");
        assert_eq!(messages[4]["role"], "tool");
        assert_eq!(messages[4]["tool_call_id"], "tool_1");
        assert_eq!(messages[5]["role"], "user");
        assert_eq!(messages[5]["content"], "Second request");
    }

    #[test]
    fn serialize_llm_input_preserves_parallel_tool_call_order() {
        let mut conversation = Conversation::new();
        conversation.push(Message::system("You are a worker."));
        conversation.push(Message::user("Do three things at once"));

        // Assistant returns 3 parallel tool calls in a single message.
        conversation.push(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::ToolUse {
                    id: "tc_a".into(),
                    name: "shell".into(),
                    input: serde_json::json!({"command": "echo A"}),
                },
                ContentBlock::ToolUse {
                    id: "tc_b".into(),
                    name: "memory_search".into(),
                    input: serde_json::json!({"query": "foo"}),
                },
                ContentBlock::ToolUse {
                    id: "tc_c".into(),
                    name: "task_list".into(),
                    input: serde_json::json!({}),
                },
            ],
            metadata: None,
        });

        // Tool results come back in a single user message (same order).
        conversation.push(Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "tc_a".into(),
                    content: vec![ContentBlock::text("A")],
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "tc_b".into(),
                    content: vec![ContentBlock::text("found: bar")],
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "tc_c".into(),
                    content: vec![ContentBlock::text("[]")],
                    is_error: false,
                },
            ],
            metadata: None,
        });

        conversation.push(Message::user("Now summarize"));

        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": "shell",
                "description": "Run shell commands",
                "parameters": {"type": "object"}
            }
        })];

        let input = serialize_llm_input(&conversation, &tools);
        let messages = input["messages"].as_array().expect("messages array");

        // system, user, assistant(3 tool_calls), tool(A), tool(B), tool(C), user
        assert_eq!(messages.len(), 7);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "Do three things at once");

        // Assistant message with 3 tool_calls in order.
        assert_eq!(messages[2]["role"], "assistant");
        let tool_calls = messages[2]["tool_calls"].as_array().expect("tool_calls");
        assert_eq!(tool_calls.len(), 3);
        assert_eq!(tool_calls[0]["id"], "tc_a");
        assert_eq!(tool_calls[1]["id"], "tc_b");
        assert_eq!(tool_calls[2]["id"], "tc_c");

        // Tool results in matching order.
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "tc_a");
        assert_eq!(messages[3]["content"], "A");

        assert_eq!(messages[4]["role"], "tool");
        assert_eq!(messages[4]["tool_call_id"], "tc_b");
        assert_eq!(messages[4]["content"], "found: bar");

        assert_eq!(messages[5]["role"], "tool");
        assert_eq!(messages[5]["tool_call_id"], "tc_c");
        assert_eq!(messages[5]["content"], "[]");

        assert_eq!(messages[6]["role"], "user");
        assert_eq!(messages[6]["content"], "Now summarize");
    }

    // ── Finalize tool + nudge loop tests ──────────────────────────────────────

    fn dummy_tool_schema(name: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": { "name": name, "description": "test", "parameters": {"type": "object"} }
        })
    }

    /// Session ends immediately when the finalize tool is called.
    /// The payload is captured on the output.
    #[tokio::test]
    async fn finalize_tool_call_ends_session_and_captures_payload() {
        let tools = vec![dummy_tool_schema("submit_work")];

        let provider = MockProvider::new(vec![MockResponse {
            text: None,
            tool_calls: vec![ContentBlock::ToolUse {
                id: "fin1".to_string(),
                name: "submit_work".to_string(),
                input: serde_json::json!({"task_id": "t1", "summary": "done"}),
            }],
            input_tokens: 100,
            output_tokens: 10,
        }]);

        let (app_state, project_path, task_id, session_id, cancel) = make_context().await;
        let worktree_path = std::path::PathBuf::from("/tmp");
        let mut conv = Conversation::new();
        conv.push(Message::system("You are a worker."));
        conv.push(Message::user("Do the task."));

        let (result, output, _tokens_in, _tokens_out) = run_reply_loop(
            ReplyLoopContext {
                provider: &provider,
                tools: &tools,
                task_id: &task_id,
                task_short_id: "t1",
                session_id: &session_id,
                project_path: &project_path,
                worktree_path: &worktree_path,
                role_name: "worker",
                finalize_tool_names: &["submit_work", "request_pm"],
                context_window: 10_000,
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
        assert_eq!(provider.remaining(), 0, "finalize response consumed");
        assert!(
            output.finalize_payload.is_some(),
            "finalize payload should be captured"
        );
        assert_eq!(
            output.finalize_payload.unwrap()["summary"],
            "done",
            "payload should contain summary"
        );
    }

    /// A text-only response without a finalize call injects a nudge and continues.
    /// After 3 consecutive nudges the session fails.
    #[tokio::test]
    async fn text_only_without_finalize_triggers_nudge_then_fails() {
        let tools = vec![dummy_tool_schema("submit_work")];

        // 3 text-only responses → MAX_NUDGE_ATTEMPTS exceeded → error.
        let provider = MockProvider::new(vec![
            MockResponse::text_only("I think I'm done.", 100),
            MockResponse::text_only("Still think I'm done.", 110),
            MockResponse::text_only("Yes, definitely done.", 120),
            // The 4th turn is never reached because we fail after 3 nudges.
        ]);

        let (app_state, project_path, task_id, session_id, cancel) = make_context().await;
        let worktree_path = std::path::PathBuf::from("/tmp");
        let mut conv = Conversation::new();
        conv.push(Message::system("You are a worker."));
        conv.push(Message::user("Do the task."));

        let (result, _output, _tokens_in, _tokens_out) = run_reply_loop(
            ReplyLoopContext {
                provider: &provider,
                tools: &tools,
                task_id: &task_id,
                task_short_id: "t1",
                session_id: &session_id,
                project_path: &project_path,
                worktree_path: &worktree_path,
                role_name: "worker",
                finalize_tool_names: &["submit_work", "request_pm"],
                context_window: 10_000,
                model_id: "test/mock-model",
                cancel: &cancel,
                global_cancel: &cancel,
                app_state: &app_state,
            },
            &mut conv,
            false,
        )
        .await;

        assert!(result.is_err(), "expected error after nudge exhaustion");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("consecutive text-only"),
            "error should mention consecutive text-only responses"
        );
    }

    /// A nudge resets after a successful tool call.
    /// Pattern: text-only (nudge 1) → tool call (resets) → text-only (nudge 1) → finalize (ok).
    #[tokio::test]
    async fn nudge_count_resets_after_tool_call() {
        let tools = vec![
            dummy_tool_schema("some_tool"),
            dummy_tool_schema("submit_work"),
        ];

        let provider = MockProvider::new(vec![
            // Turn 1: text-only → nudge 1
            MockResponse::text_only("hmm", 100),
            // Turn 2: real tool call → resets nudge count
            MockResponse::tool_call("tc1", "some_tool", 110),
            // Turn 3: text-only → nudge 1 again (not 2)
            MockResponse::text_only("ok", 120),
            // Turn 4: finalize → session complete
            MockResponse {
                text: None,
                tool_calls: vec![ContentBlock::ToolUse {
                    id: "fin1".to_string(),
                    name: "submit_work".to_string(),
                    input: serde_json::json!({"task_id": "t1", "summary": "all done"}),
                }],
                input_tokens: 130,
                output_tokens: 10,
            },
        ]);

        let (app_state, project_path, task_id, session_id, cancel) = make_context().await;
        let worktree_path = std::path::PathBuf::from("/tmp");
        let mut conv = Conversation::new();
        conv.push(Message::system("You are a worker."));
        conv.push(Message::user("Do the task."));

        let (result, output, _tokens_in, _tokens_out) = run_reply_loop(
            ReplyLoopContext {
                provider: &provider,
                tools: &tools,
                task_id: &task_id,
                task_short_id: "t1",
                session_id: &session_id,
                project_path: &project_path,
                worktree_path: &worktree_path,
                role_name: "worker",
                finalize_tool_names: &["submit_work", "request_pm"],
                context_window: 10_000,
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
        assert_eq!(provider.remaining(), 0, "all responses consumed");
        assert!(output.finalize_payload.is_some(), "finalize payload set");
    }

    /// ToolChoice::Required is passed on every turn that has tools.
    /// Verified by recording the tool_choice values received by the mock provider.
    #[tokio::test]
    async fn tool_choice_required_passed_on_every_turn_with_tools() {
        use std::sync::Mutex;

        let tools = vec![dummy_tool_schema("submit_work")];

        struct RecordingProvider {
            recorded_choices: Arc<Mutex<Vec<Option<ToolChoice>>>>,
            inner: MockProvider,
        }

        impl LlmProvider for RecordingProvider {
            fn name(&self) -> &str {
                "recording"
            }
            fn stream<'a>(
                &'a self,
                conversation: &'a Conversation,
                tools: &'a [serde_json::Value],
                tool_choice: Option<ToolChoice>,
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
                self.recorded_choices
                    .lock()
                    .unwrap()
                    .push(tool_choice.clone());
                self.inner.stream(conversation, tools, tool_choice)
            }
        }

        let inner = MockProvider::new(vec![
            MockResponse::tool_call("tc1", "nonexistent_tool", 100),
            MockResponse {
                text: None,
                tool_calls: vec![ContentBlock::ToolUse {
                    id: "fin1".to_string(),
                    name: "submit_work".to_string(),
                    input: serde_json::json!({"task_id": "t1", "summary": "done"}),
                }],
                input_tokens: 110,
                output_tokens: 10,
            },
        ]);
        let recorded = Arc::new(Mutex::new(Vec::<Option<ToolChoice>>::new()));
        let provider = RecordingProvider {
            recorded_choices: Arc::clone(&recorded),
            inner,
        };

        let (app_state, project_path, task_id, session_id, cancel) = make_context().await;
        let worktree_path = std::path::PathBuf::from("/tmp");
        let mut conv = Conversation::new();
        conv.push(Message::system("You are a worker."));
        conv.push(Message::user("Do the task."));

        let (result, _output, _, _) = run_reply_loop(
            ReplyLoopContext {
                provider: &provider,
                tools: &tools,
                task_id: &task_id,
                task_short_id: "t1",
                session_id: &session_id,
                project_path: &project_path,
                worktree_path: &worktree_path,
                role_name: "worker",
                finalize_tool_names: &["submit_work", "request_pm"],
                context_window: 10_000,
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

        let choices = recorded.lock().unwrap();
        assert_eq!(choices.len(), 2, "two turns recorded");
        for (i, choice) in choices.iter().enumerate() {
            assert!(
                matches!(choice, Some(ToolChoice::Required)),
                "turn {i}: expected ToolChoice::Required, got {:?}",
                choice
            );
        }
    }
}
