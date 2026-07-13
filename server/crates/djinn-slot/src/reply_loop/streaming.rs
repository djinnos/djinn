//! Reply loop streaming: consumes the provider stream for a single turn.
//!
//! Adapted from the agent implementation.  Uses [`SlotContext`] for event
//! emission, activity tracking, and host-callback RPCs (touch_activity,
//! flush_session_tokens).  No `djinn-agent` imports.

use std::collections::HashSet;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

use futures::StreamExt;
use futures::stream::FuturesUnordered;

use djinn_core::events::DjinnEventEnvelope;
use djinn_provider::message::ContentBlock;
use djinn_provider::provider::{LlmProvider, StreamEvent};

use super::error_handling::{
    MAX_COMPACTION_RETRIES, is_context_length_error, is_orphaned_tool_call_error,
};
use super::tool_dispatch::{
    MAX_TOOL_CONCURRENCY, ToolDispatchContext, ToolRuntimeMetadataMap, is_side_query_tool,
    make_tool_future,
};
use crate::helpers::{runtime_env_diagnostics, runtime_fs_diagnostics};

pub(super) type StreamingFut<'a> =
    Pin<Box<dyn std::future::Future<Output = (usize, ContentBlock)> + Send + 'a>>;

pub(super) struct StreamTurnState {
    pub turn_text: String,
    pub turn_thinking: String,
    pub turn_provider_state: Vec<ContentBlock>,
    pub turn_tool_calls: Vec<ContentBlock>,
    pub turn_tokens_in: u32,
    pub turn_tokens_out: u32,
    pub turn_cache_read: u32,
    pub turn_cache_write: u32,
    pub turn_reasoning_out: u32,
    pub interrupted: Option<&'static str>,
    pub saw_round_event: bool,
    pub needs_reactive_compaction: bool,
    pub streaming_results: Vec<(usize, ContentBlock)>,
    pub streaming_dispatched: HashSet<usize>,
    /// True when the provider stream ended without a `StreamEvent::Done`
    /// (i.e., the stream returned `None` early).  This is distinct from
    /// cancellation/interruption and from normal completion; it tells the
    /// reply loop to flush any observed in-flight content before returning.
    pub early_stream_end: bool,
    /// Idempotency guard: `true` once this turn's observed assistant/tool
    /// rows have been persisted (either through the normal finalize path or
    /// via [`persistence::flush_in_flight_turn`]).  Repeated flush calls
    /// within the same turn are no-ops.
    pub turn_flushed: bool,
}

impl StreamTurnState {
    pub(super) fn new() -> Self {
        Self {
            turn_text: String::new(),
            turn_thinking: String::new(),
            turn_provider_state: Vec::new(),
            turn_tool_calls: Vec::new(),
            turn_tokens_in: 0,
            turn_tokens_out: 0,
            turn_cache_read: 0,
            turn_cache_write: 0,
            turn_reasoning_out: 0,
            interrupted: None,
            saw_round_event: false,
            needs_reactive_compaction: false,
            streaming_results: Vec::new(),
            streaming_dispatched: HashSet::new(),
            early_stream_end: false,
            turn_flushed: false,
        }
    }
}

pub(super) struct StreamLoopContext<'a> {
    pub provider: &'a dyn LlmProvider,
    pub stream: Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>>,
    pub tool_metadata: &'a ToolRuntimeMetadataMap,
    pub dispatch: &'a ToolDispatchContext<'a>,
    pub phase_tracker: &'a Arc<Mutex<super::phase::SessionPhaseTracker>>,
    pub task_id: &'a str,
    pub session_id: &'a str,
    pub role_name: &'a str,
    pub project_path: &'a str,
    pub worktree_path: &'a std::path::Path,
    pub context_window: i64,
    pub ctx: &'a crate::host::SlotContext,
    pub cancel: &'a tokio_util::sync::CancellationToken,
    pub global_cancel: &'a tokio_util::sync::CancellationToken,
    pub activity_ts: &'a Arc<AtomicU64>,
    pub last_rpc_touch: &'a Arc<AtomicU64>,
    pub last_token_flush: &'a Arc<AtomicU64>,
    pub compaction_attempts: u32,
    pub current_context_tokens: &'a mut u32,
    pub total_tokens_in: &'a mut u32,
    pub total_tokens_out: &'a mut u32,
    pub total_cache_read: &'a mut u32,
    pub total_cache_write: &'a mut u32,
    pub total_reasoning_out: &'a mut u32,
}

/// Throttle interval for the worker→host activity-touch RPC in the per-StreamEvent loop.
const TOUCH_ACTIVITY_RPC_INTERVAL_SECS: u64 = 30;

/// Throttle interval for the mid-flight session-row token flush.
const TOKEN_FLUSH_INTERVAL_SECS: u64 = 30;

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
                let Some(evt) = evt else {
                    state.early_stream_end = true;
                    break;
                };
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
                        let detail = format!(
                            "provider stream event failed: display={e} debug={e:?}; {diag}; {env_diag}"
                        );
                        return Err(e.context(detail));
                    }
                };
                state.saw_round_event = true;
                let now = ctx.ctx.clock.now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                ctx.activity_ts.store(now, Ordering::Relaxed);
                // Bridge to the host's ActivityTracker.
                let last = ctx.last_rpc_touch.load(Ordering::Relaxed);
                if now.saturating_sub(last) >= TOUCH_ACTIVITY_RPC_INTERVAL_SECS {
                    ctx.last_rpc_touch.store(now, Ordering::Relaxed);
                    if let Err(e) = ctx.ctx.callbacks.touch_activity_rpc(ctx.task_id.to_string()).await {
                        tracing::warn!(
                            task_id = %ctx.task_id,
                            error = %e,
                            "reply_loop::streaming: touch_activity RPC failed; \
                             host stall poller may see stale idle for this turn"
                        );
                    }
                }
                match evt {
                    StreamEvent::Delta(ContentBlock::Text { text }) => {
                        ctx.ctx.event_bus.send(DjinnEventEnvelope::session_message(
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
                    | StreamEvent::Delta(ContentBlock::Image { .. })
                    | StreamEvent::Delta(ContentBlock::Document { .. }) => {}
                    StreamEvent::Delta(reasoning @ ContentBlock::OpenAIReasoning { .. })
                    | StreamEvent::Delta(reasoning @ ContentBlock::Thinking { .. })
                    | StreamEvent::Delta(reasoning @ ContentBlock::RedactedThinking { .. })
                    | StreamEvent::Delta(reasoning @ ContentBlock::Unknown { .. }) => {
                        state.turn_provider_state.push(reasoning);
                    }
                    StreamEvent::Thinking(thinking) => {
                        ctx.ctx.event_bus.send(DjinnEventEnvelope::session_message(
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
                        state.turn_cache_read = usage.cache_read;
                        state.turn_cache_write = usage.cache_write;
                        state.turn_reasoning_out = usage.reasoning_output;
                        *ctx.current_context_tokens = usage.context_total();
                        *ctx.total_tokens_in = ctx.total_tokens_in.saturating_add(usage.input);
                        *ctx.total_tokens_out = ctx.total_tokens_out.saturating_add(usage.output);
                        *ctx.total_cache_read =
                            ctx.total_cache_read.saturating_add(usage.cache_read);
                        *ctx.total_cache_write =
                            ctx.total_cache_write.saturating_add(usage.cache_write);
                        *ctx.total_reasoning_out =
                            ctx.total_reasoning_out.saturating_add(usage.reasoning_output);
                        let usage_pct = if ctx.context_window > 0 {
                            *ctx.current_context_tokens as f64 / ctx.context_window as f64
                        } else {
                            0.0
                        };
                        ctx.ctx.event_bus.send(DjinnEventEnvelope::session_token_update(
                            ctx.session_id,
                            ctx.task_id,
                            *ctx.current_context_tokens as i64,
                            *ctx.total_tokens_out as i64,
                            ctx.context_window,
                            usage_pct,
                            usage.cache_read as i64,
                            usage.cache_write as i64,
                            usage.reasoning_output as i64,
                        ));
                        // Persist mid-flight token counters.
                        let last = ctx.last_token_flush.load(Ordering::Relaxed);
                        if now.saturating_sub(last) >= TOKEN_FLUSH_INTERVAL_SECS {
                            ctx.last_token_flush.store(now, Ordering::Relaxed);
                            if let Err(e) = ctx
                                .ctx
                                .callbacks
                                .flush_session_tokens_rpc(
                                    ctx.session_id.to_string(),
                                    *ctx.total_tokens_in as i64,
                                    *ctx.total_tokens_out as i64,
                                    *ctx.total_cache_read as i64,
                                    *ctx.total_cache_write as i64,
                                )
                                .await
                            {
                                tracing::warn!(
                                    session_id = %ctx.session_id,
                                    error = %e,
                                    "reply_loop::streaming: flush_session_tokens failed; \
                                     session row keeps stale token counters until next flush"
                                );
                            }
                        }
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
