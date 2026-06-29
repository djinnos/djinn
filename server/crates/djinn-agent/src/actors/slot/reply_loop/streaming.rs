use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;
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

pub(super) type StreamingFut<'a> =
    Pin<Box<dyn std::future::Future<Output = (usize, ContentBlock)> + Send + 'a>>;

pub(super) struct StreamTurnState {
    pub turn_text: String,
    pub turn_thinking: String,
    pub turn_provider_state: Vec<ContentBlock>,
    pub turn_tool_calls: Vec<ContentBlock>,
    pub turn_tokens_in: u32,
    pub turn_tokens_out: u32,
    /// Cache-read / cache-write / reasoning token counts for this turn's last
    /// usage report (prompt-cache accounting, ADR-043).
    pub turn_cache_read: u32,
    pub turn_cache_write: u32,
    pub turn_reasoning_out: u32,
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
        }
    }
}

pub(super) struct StreamLoopContext<'a> {
    pub provider: &'a dyn LlmProvider,
    pub stream: Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>>,
    pub tool_metadata: &'a ToolRuntimeMetadataMap,
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
    /// Last unix-second the worker fired a `services.touch_activity`
    /// RPC to the host's ActivityTracker. Throttled to once per
    /// `TOUCH_ACTIVITY_RPC_INTERVAL_SECS` so the per-StreamEvent loop
    /// doesn't flood the host with one round-trip per token. The
    /// host's stall threshold is 5 minutes (workers); a 30s
    /// inter-touch budget leaves ~10x headroom for transient
    /// transport flakes without false-stalling. The local
    /// `activity_ts.store(..)` still fires on every event for
    /// worker-side diagnostics.
    pub last_rpc_touch: &'a Arc<AtomicU64>,
    /// Last unix-second the cumulative token counters were flushed to the
    /// session row via `services.flush_session_tokens`. Throttled to once
    /// per `TOKEN_FLUSH_INTERVAL_SECS`; best-effort (see field above for
    /// the host/worker routing of `services`).
    pub last_token_flush: &'a Arc<AtomicU64>,
    /// Host-side / worker-side `SupervisorServices` handle. On the
    /// host this resolves to `DirectServices` and the touch is a
    /// local atomic write; on the worker it resolves to
    /// `WorkerSupervisorServices` and routes over RPC. Fire-and-
    /// forget — transient errors are swallowed with a warn log
    /// because the local tracker still serves worker diagnostics.
    pub services: &'a dyn djinn_supervisor::SupervisorServices,
    pub compaction_attempts: u32,
    pub current_context_tokens: &'a mut u32,
    pub total_tokens_in: &'a mut u32,
    pub total_tokens_out: &'a mut u32,
    /// Cumulative cache-read / cache-write / reasoning token aggregates
    /// (prompt-cache accounting, ADR-043) summed across all turns.
    pub total_cache_read: &'a mut u32,
    pub total_cache_write: &'a mut u32,
    pub total_reasoning_out: &'a mut u32,
}

/// Throttle interval for the worker→host activity-touch RPC in the
/// per-StreamEvent loop. 30 seconds is well under the host's 5-minute
/// stall threshold so a single skipped touch (due to throttling) can
/// never alone trip the stall poller; the next stream event ticks the
/// clock and fires the RPC fresh.
const TOUCH_ACTIVITY_RPC_INTERVAL_SECS: u64 = 30;

/// Throttle interval for the mid-flight session-row token flush fired on
/// `StreamEvent::Usage` frames (one per generation). Initialised to 0 in
/// `run_reply_loop` so the first generation's usage lands in the DB
/// immediately; after that one write per 30s is plenty for observability —
/// the teardown `update_session_status` remains the authoritative total.
const TOKEN_FLUSH_INTERVAL_SECS: u64 = 30;

#[allow(clippy::disallowed_methods)] // scoped: direct wall-clock read; migration tracked by lint-ratchet task 70y0 (Clock abstraction already lands in 8bcj/m5g4)
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
                        // Preserve `e` as the anyhow *source* (via `.context`) rather than
                        // formatting it into a fresh string. The original error carries a
                        // typed `ProviderError` (e.g. `Authentication` for a 401
                        // token_revoked) that the host's `classify_provider_failure` must be
                        // able to `downcast_ref`; rebuilding with `anyhow!("...{e}...")` erased
                        // that type, so every streaming provider failure surfaced as
                        // `provider_failure: None` and the per-(scope,model) health breaker
                        // was never fed — dispatch then re-selected a dead model forever
                        // instead of failing over. We keep the same `display=/debug=` body in
                        // the context message so the existing string-based detectors
                        // (`is_orphaned_tool_call_error`, context-length substring fallback)
                        // still match on the top-level Display.
                        let detail = format!(
                            "provider stream event failed: display={e} debug={e:?}; {diag}; {env_diag}"
                        );
                        return Err(e.context(detail));
                    }
                };

                state.saw_round_event = true;

                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                ctx.activity_ts.store(now, Ordering::Relaxed);

                // Bridge to the host's ActivityTracker so the
                // coordinator's stall poller sees the worker stay
                // active. Throttled to avoid one RPC per token.
                let last = ctx.last_rpc_touch.load(Ordering::Relaxed);
                if now.saturating_sub(last) >= TOUCH_ACTIVITY_RPC_INTERVAL_SECS {
                    ctx.last_rpc_touch.store(now, Ordering::Relaxed);
                    if let Err(e) = ctx.services.touch_activity(ctx.task_id.to_string()).await {
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
                    | StreamEvent::Delta(ContentBlock::Image { .. })
                    | StreamEvent::Delta(ContentBlock::Document { .. }) => {}
                    StreamEvent::Delta(reasoning @ ContentBlock::OpenAIReasoning { .. })
                    | StreamEvent::Delta(reasoning @ ContentBlock::Thinking { .. }) => {
                        state.turn_provider_state.push(reasoning);
                    }
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
                        state.turn_cache_read = usage.cache_read;
                        state.turn_cache_write = usage.cache_write;
                        state.turn_reasoning_out = usage.reasoning_output;
                        // Cache-aware context gauge: `usage.input` excludes
                        // cached reads/writes for Anthropic-format providers, so
                        // it massively undercounts the real prompt context on a
                        // cache hit. `context_total()` normalizes this across
                        // formats (Anthropic: input+cache_read+cache_write;
                        // OpenAI/Google: input, already cache-inclusive) so the
                        // gauge, compaction trigger, and UI usage_pct all see the
                        // true context size. `tokens_in` keeps accumulating raw
                        // `input` (its per-provider meaning is documented and the
                        // persisted cache columns make it interpretable).
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
                        ctx.app_state.event_bus.send(DjinnEventEnvelope::session_token_update(
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

                        // Persist the cumulative counters to the session row
                        // so long-running sessions don't read `tokens_in = 0`
                        // everywhere until teardown. Usage frames arrive once
                        // per generation; the throttle keeps fast turn
                        // cadences from turning into a DB write per turn.
                        // Best-effort: the final `update_session_status` at
                        // stage teardown is still the authoritative write.
                        let last = ctx.last_token_flush.load(Ordering::Relaxed);
                        if now.saturating_sub(last) >= TOKEN_FLUSH_INTERVAL_SECS {
                            ctx.last_token_flush.store(now, Ordering::Relaxed);
                            if let Err(e) = ctx
                                .services
                                .flush_session_tokens(
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

fn runtime_fs_diagnostics(project_path: &str, worktree_path: &std::path::Path) -> String {
    super::super::runtime_fs_diagnostics(project_path, worktree_path)
}

fn runtime_env_diagnostics(
    session_id: &str,
    project_path: &str,
    worktree_path: &std::path::Path,
) -> String {
    super::super::runtime_env_diagnostics(session_id, project_path, worktree_path)
}
