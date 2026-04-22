use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::StreamExt;
use futures::stream::FuturesUnordered;

use djinn_provider::message::ContentBlock;
use djinn_provider::provider::{LlmProvider, StreamEvent};
use djinn_core::events::DjinnEventEnvelope;

use super::error_handling::{
    MAX_COMPACTION_RETRIES, is_context_length_error, is_orphaned_tool_call_error,
};
use super::tool_dispatch::{
    MAX_TOOL_CONCURRENCY, ToolDispatchContext, is_side_query_tool, make_tool_future,
};

pub(super) type StreamingFut<'a> =
    Pin<Box<dyn std::future::Future<Output = (usize, ContentBlock)> + Send + 'a>>;

pub(super) struct StreamTurnState {
    pub turn_text: String,
    pub turn_thinking: String,
    pub turn_tool_calls: Vec<ContentBlock>,
    pub turn_tokens_in: u32,
    pub turn_tokens_out: u32,
    pub interrupted: Option<&'static str>,
    pub saw_round_event: bool,
    pub needs_reactive_compaction: bool,
    pub streaming_results: Vec<(usize, ContentBlock)>,
    pub streaming_dispatched: HashSet<usize>,
}

impl StreamTurnState {
    pub(super) fn new() -> Self {
        Self {
            turn_text: String::new(),
            turn_thinking: String::new(),
            turn_tool_calls: Vec::new(),
            turn_tokens_in: 0,
            turn_tokens_out: 0,
            interrupted: None,
            saw_round_event: false,
            needs_reactive_compaction: false,
            streaming_results: Vec::new(),
            streaming_dispatched: HashSet::new(),
        }
    }
}

pub(super) struct StreamLoopContext<'a> {
    pub provider: &'a dyn LlmProvider,
    pub stream: Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>>,
    pub tool_metadata: &'a std::collections::HashMap<String, bool>,
    pub dispatch: &'a ToolDispatchContext<'a>,
    pub task_id: &'a str,
    pub session_id: &'a str,
    pub role_name: &'a str,
    pub project_path: &'a str,
    pub worktree_path: &'a std::path::Path,
    pub context_window: i64,
    pub app_state: &'a crate::context::AgentContext,
    pub cancel: &'a tokio_util::sync::CancellationToken,
    pub global_cancel: &'a tokio_util::sync::CancellationToken,
    pub activity_ts: &'a Arc<AtomicU64>,
    pub compaction_attempts: u32,
    pub current_context_tokens: &'a mut u32,
    pub total_tokens_in: &'a mut u32,
    pub total_tokens_out: &'a mut u32,
}

pub(super) async fn consume_provider_stream(
    mut ctx: StreamLoopContext<'_>,
) -> anyhow::Result<StreamTurnState> {
    let mut state = StreamTurnState::new();
    let mut streaming_inflight: FuturesUnordered<StreamingFut<'_>> = FuturesUnordered::new();

    loop {
        tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => {
                state.interrupted = Some("session cancelled");
                break;
            }
            _ = ctx.global_cancel.cancelled() => {
                state.interrupted = Some("supervisor shutting down");
                break;
            }
            Some(result) = streaming_inflight.next() => {
                state.streaming_results.push(result);
            }
            evt = ctx.stream.next() => {
                let Some(evt) = evt else { break; };
                let evt = match evt {
                    Ok(e) => e,
                    Err(e) if (is_context_length_error(&e) || is_orphaned_tool_call_error(&e))
                        && ctx.compaction_attempts < MAX_COMPACTION_RETRIES => {
                        state.needs_reactive_compaction = true;
                        break;
                    }
                    Err(e) => {
                        let diag = runtime_fs_diagnostics(ctx.project_path, ctx.worktree_path);
                        let env_diag = runtime_env_diagnostics(ctx.session_id, ctx.project_path, ctx.worktree_path);
                        return Err(anyhow::anyhow!(
                            "provider stream event failed: display={} debug={:?}; {}; {}",
                            e, e, diag, env_diag
                        ));
                    }
                };

                state.saw_round_event = true;

                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                ctx.activity_ts.store(now, Ordering::Relaxed);

                match evt {
                    StreamEvent::Delta(ContentBlock::Text { text }) => {
                        ctx.app_state.event_bus.send(DjinnEventEnvelope::session_message(
                            ctx.session_id,
                            ctx.task_id,
                            ctx.role_name,
                            &serde_json::json!({
                                "type": "delta",
                                "role": "assistant",
                                "text": text,
                            }),
                        ));
                        state.turn_text.push_str(&text);
                    }
                    StreamEvent::Delta(tool_use @ ContentBlock::ToolUse { .. }) => {
                        let idx = state.turn_tool_calls.len();
                        let should_dispatch_now = if let ContentBlock::ToolUse { name, .. } = &tool_use {
                            is_side_query_tool(ctx.tool_metadata, name)
                                && state.streaming_dispatched.len() < MAX_TOOL_CONCURRENCY
                        } else {
                            false
                        };
                        state.turn_tool_calls.push(tool_use);
                        if should_dispatch_now {
                            state.streaming_dispatched.insert(idx);
                            let tool_call = state.turn_tool_calls[idx].clone();
                            streaming_inflight.push(Box::pin(make_tool_future(
                                idx,
                                tool_call,
                                ctx.dispatch,
                            )));
                        }
                    }
                    StreamEvent::Delta(ContentBlock::ToolResult { .. })
                    | StreamEvent::Delta(ContentBlock::Thinking { .. })
                    | StreamEvent::Delta(ContentBlock::Image { .. })
                    | StreamEvent::Delta(ContentBlock::Document { .. }) => {}
                    StreamEvent::Thinking(thinking) => {
                        ctx.app_state.event_bus.send(DjinnEventEnvelope::session_message(
                            ctx.session_id,
                            ctx.task_id,
                            ctx.role_name,
                            &serde_json::json!({
                                "type": "thinking_delta",
                                "role": "assistant",
                                "text": thinking,
                            }),
                        ));
                        state.turn_thinking.push_str(&thinking);
                    }
                    StreamEvent::Usage(usage) => {
                        state.turn_tokens_in = usage.input;
                        state.turn_tokens_out = usage.output;
                        *ctx.current_context_tokens = usage.input;
                        *ctx.total_tokens_in = ctx.total_tokens_in.saturating_add(usage.input);
                        *ctx.total_tokens_out = ctx.total_tokens_out.saturating_add(usage.output);

                        let usage_pct = if ctx.context_window > 0 {
                            *ctx.current_context_tokens as f64 / ctx.context_window as f64
                        } else {
                            0.0
                        };
                        ctx.app_state.event_bus.send(DjinnEventEnvelope::session_token_update(
                            ctx.session_id,
                            ctx.task_id,
                            *ctx.current_context_tokens as i64,
                            *ctx.total_tokens_out as i64,
                            ctx.context_window,
                            usage_pct,
                        ));
                    }
                    StreamEvent::Done => break,
                }
            }
        }
    }

    while let Some(result) = streaming_inflight.next().await {
        state.streaming_results.push(result);
    }
    if !state.streaming_dispatched.is_empty() {
        tracing::debug!(
            task_id = %ctx.task_id,
            dispatched = state.streaming_dispatched.len(),
            completed = state.streaming_results.len(),
            "ReplyLoop: streaming dispatch complete (ADR-048 §1B)"
        );
    }

    let _ = ctx.provider.name();
    Ok(state)
}

fn runtime_fs_diagnostics(project_path: &str, worktree_path: &std::path::Path) -> String {
    super::runtime_fs_diagnostics(project_path, worktree_path)
}

fn runtime_env_diagnostics(
    session_id: &str,
    project_path: &str,
    worktree_path: &std::path::Path,
) -> String {
    super::runtime_env_diagnostics(session_id, project_path, worktree_path)
}
