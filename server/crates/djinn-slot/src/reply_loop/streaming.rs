//! Reply loop streaming: consumes the provider stream for a single turn.
//!
//! Adapted from the agent implementation.  Uses [`SlotContext`] for event
//! emission, activity tracking, and host-callback RPCs (touch_activity,
//! flush_session_tokens).  No `djinn-agent` imports.

use std::collections::HashSet;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use futures::stream::FuturesUnordered;

use djinn_core::events::DjinnEventEnvelope;
use djinn_provider::message::{ContentBlock, Conversation};
use djinn_provider::provider::client::backoff_delay_ms;
use djinn_provider::provider::{LlmProvider, ProviderError, StreamEvent, ToolChoice};

use super::budget::record_provider_usage;
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

/// One unresolved reasoning fragment in provider event arrival order.
#[derive(Clone)]
pub enum UnresolvedThinkingFragment {
    Attributed { id: u64, text: String },
    Unattributed(String),
}

/// Apply the persistence-relevant portion of one provider event to a turn.
///
/// The live stream consumer owns event-bus emission, tool dispatch, and usage
/// accounting, while this function owns the single source of truth for turn
/// content that is later finalized or flushed.
pub fn apply_persistence_event(state: &mut StreamTurnState, event: StreamEvent) {
    match event {
        StreamEvent::Delta(ContentBlock::Text { text }) => state.turn_text.push_str(&text),
        StreamEvent::Delta(tool_use @ ContentBlock::ToolUse { .. }) => {
            state.turn_tool_calls.push(tool_use);
        }
        StreamEvent::Delta(reasoning @ ContentBlock::OpenAIReasoning { .. })
        | StreamEvent::Delta(reasoning @ ContentBlock::Thinking { .. })
        | StreamEvent::Delta(reasoning @ ContentBlock::RedactedThinking { .. })
        | StreamEvent::Delta(reasoning @ ContentBlock::Unknown { .. }) => {
            state.turn_provider_state.push(reasoning);
        }
        StreamEvent::Thinking(thinking) => {
            state.turn_thinking.push_str(&thinking);
            state
                .turn_unresolved_thinking
                .push(UnresolvedThinkingFragment::Unattributed(thinking));
        }
        StreamEvent::ThinkingDelta { id, text } => {
            state.turn_thinking.push_str(&text);
            state
                .turn_unresolved_thinking
                .push(UnresolvedThinkingFragment::Attributed { id, text });
        }
        StreamEvent::ThinkingBlockComplete {
            id,
            thinking,
            signature,
        } => {
            state.turn_provider_state.push(ContentBlock::Thinking {
                thinking,
                signature,
            });
            state.turn_completed_thinking_ids.insert(id);
        }
        StreamEvent::Delta(ContentBlock::ToolResult { .. })
        | StreamEvent::Delta(ContentBlock::Image { .. })
        | StreamEvent::Delta(ContentBlock::Document { .. })
        | StreamEvent::Usage(_)
        | StreamEvent::Done => {}
    }
}

/// Drive explicit provider events through the live persistence state consumer.
///
/// Host event emission, tool execution, and token accounting are deliberately
/// outside this test seam; persistence-relevant event handling is the exact
/// function invoked by [`consume_provider_stream`].
#[cfg(any(test, feature = "test-support"))]
pub fn consume_events_for_persistence(
    events: impl IntoIterator<Item = StreamEvent>,
) -> StreamTurnState {
    let mut state = StreamTurnState::new();
    for event in events {
        let done = matches!(event, StreamEvent::Done);
        apply_persistence_event(&mut state, event);
        if done {
            break;
        }
    }
    state
}

pub struct StreamTurnState {
    pub turn_text: String,
    /// Complete arrival-order thinking aggregate for display and telemetry.
    /// This includes unattributed events and attributed delta text, so it is
    /// deliberately kept separate from canonical persistence input.
    pub turn_thinking: String,
    pub turn_provider_state: Vec<ContentBlock>,
    pub turn_tool_calls: Vec<ContentBlock>,
    /// Attributed and unattributed fragments in one event-arrival sequence.
    /// Persistence suppresses only attributed entries whose exact ID completed.
    pub turn_unresolved_thinking: Vec<UnresolvedThinkingFragment>,
    /// Block IDs for which a `ThinkingBlockComplete` was received. The
    /// canonical assembler reconciles `turn_unresolved_thinking` against
    /// this set, suppressing only exact-ID matches.
    pub turn_completed_thinking_ids: HashSet<u64>,
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

impl Default for StreamTurnState {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamTurnState {
    pub fn new() -> Self {
        Self {
            turn_text: String::new(),
            turn_thinking: String::new(),
            turn_provider_state: Vec::new(),
            turn_tool_calls: Vec::new(),
            turn_unresolved_thinking: Vec::new(),
            turn_completed_thinking_ids: HashSet::new(),
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

pub(super) type ProviderStream =
    Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>>;

pub(super) struct StreamLoopContext<'a> {
    pub provider: &'a dyn LlmProvider,
    pub stream: ProviderStream,
    /// The exact request inputs that produced [`Self::stream`], so a round
    /// killed by a *transient* mid-stream provider error can be re-issued
    /// verbatim. See [`consume_provider_stream`]'s retry branch.
    pub request_conversation: &'a Conversation,
    pub request_tools: &'a [serde_json::Value],
    pub request_tool_choice: Option<ToolChoice>,
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

/// Additional attempts granted to a round killed by a **transient** mid-stream
/// provider error (e.g. an OpenAI `server_is_overloaded` SSE `error` event).
///
/// The HTTP-level retry in `djinn_provider::provider::client` only covers
/// request *establishment*: once the provider answers `200 OK` and the SSE
/// stream is open, its `'retry` loop has already been broken out of, so an
/// error arriving as a stream *event* had no retry path at all and terminated
/// the whole agent session on the first occurrence.
///
/// Deliberately smaller than the client's `MAX_RETRIES` (3): each attempt here
/// re-issues a full LLM request, and the surrounding reply loop still has its
/// own recovery paths above this one. This shallow budget applies to
/// retryable **fault** classes (transport, provider-internal 5xx, empty
/// completion); throttle-class failures get the deeper
/// [`MAX_THROTTLE_STREAM_EVENT_RETRIES`] instead.
pub(super) const MAX_STREAM_EVENT_RETRIES: u32 = 2;

/// Additional attempts granted when the mid-stream failure is a **throttle**
/// ([`ProviderError::is_throttle`]: rate limit / quota / overload).
///
/// The shallow budget above adds up to ~3 seconds of total patience (1s + 2s
/// backoff), which is the wrong order of magnitude for capacity shedding:
/// real overload episodes last minutes. The overnight `server_is_overloaded`
/// incident killed planner sessions ~11 seconds after start — all three
/// requests burned inside a single burst that the retry was nominally
/// "handling". Ten attempts on the shared `backoff_delay_ms` schedule
/// (1, 2, 4, 8, 16, then 30s at the cap ≈ 3 minutes of waiting in total)
/// actually reaches the client's 30-second backoff ceiling and spans a
/// realistic episode, while staying far below the coordinator's 30-minute
/// session-stall budget.
///
/// A deep wait is only safe because the waits stay cancellation-responsive
/// (every sleep sits in a `tokio::select!` with both cancel tokens) and
/// because each retry iteration refreshes the activity clocks (see
/// [`touch_stream_activity`]) so the coordinator's stall poller cannot
/// mistake a deliberate backoff for a hung first call.
pub(super) const MAX_THROTTLE_STREAM_EVENT_RETRIES: u32 = 10;

/// Whether the round consumed so far can be safely thrown away and re-issued.
///
/// A retry re-sends the identical request, so it is only safe while **nothing
/// observable has happened yet this round**: no text streamed to the transcript
/// event bus, no tool call recorded or dispatched (tool execution has real side
/// effects), no thinking emitted, and no usage folded into the session
/// counters. `saw_round_event` is the load-bearing flag — it is set on the very
/// first successfully-decoded event, before any of the per-event handling below
/// runs — and the remaining checks are explicit belt-and-braces so a future
/// change to that flag's meaning cannot silently make retries unsafe.
fn round_is_safely_restartable(state: &StreamTurnState, inflight_tool_futures: usize) -> bool {
    !state.saw_round_event
        && state.turn_text.is_empty()
        && state.turn_thinking.is_empty()
        && state.turn_tool_calls.is_empty()
        && state.turn_provider_state.is_empty()
        && state.turn_unresolved_thinking.is_empty()
        && state.streaming_dispatched.is_empty()
        && state.streaming_results.is_empty()
        && state.turn_tokens_in == 0
        && state.turn_tokens_out == 0
        && inflight_tool_futures == 0
}

/// Delay before re-issuing a round killed by `error` plus the class-dependent
/// attempt budget (surfaced for logging), or `None` when the round must fail
/// exactly as it does today.
///
/// Retryability is decided *solely* by [`ProviderError::retryable`] — the same
/// predicate the HTTP client and the supervisor's failure classifier use — so
/// `RateLimit` / `ProviderInternal{5xx}` / `Transport` / `ExhaustedTransport` /
/// `EmptyCompletion` are retried while `Authentication` / `InvalidRequest` /
/// `ContextOverflow` / `InvalidOutput` are not. An error with no typed
/// `ProviderError` source is never retried.
///
/// *Patience* is decided separately, by [`ProviderError::is_throttle`]:
/// throttle-class failures (capacity shedding that routinely lasts minutes)
/// get [`MAX_THROTTLE_STREAM_EVENT_RETRIES`] attempts so the schedule can
/// wait an episode out, while every other retryable class keeps the shallow
/// [`MAX_STREAM_EVENT_RETRIES`]. Both budgets are compared against the
/// round's single shared attempt counter, so an error that changes class
/// mid-episode can never enlarge the patience already spent: a transport
/// fault arriving after three throttle waits is already over the shallow
/// budget and fails immediately.
fn transient_round_restart_delay(
    error: &anyhow::Error,
    attempts_used: u32,
    state: &StreamTurnState,
    inflight_tool_futures: usize,
) -> Option<(std::time::Duration, u32)> {
    if !round_is_safely_restartable(state, inflight_tool_futures) {
        return None;
    }
    let provider_error = error.downcast_ref::<ProviderError>()?;
    if !provider_error.retryable() {
        return None;
    }
    let budget = if provider_error.is_throttle() {
        MAX_THROTTLE_STREAM_EVENT_RETRIES
    } else {
        MAX_STREAM_EVENT_RETRIES
    };
    if attempts_used >= budget {
        return None;
    }
    // Same exponential-with-jitter schedule as the client's request retry
    // (which the throttle budget is deep enough to ride up to its 30s cap),
    // floored by a server-supplied Retry-After when the provider sent one.
    let attempt = attempts_used + 1;
    let delay_ms = backoff_delay_ms(attempt).max(provider_error.retry_after_ms().unwrap_or(0));
    Some((std::time::Duration::from_millis(delay_ms), budget))
}

/// Refresh the liveness clocks: store the in-process activity timestamp and
/// bridge it to the host's `ActivityTracker` via the (throttled)
/// `touch_activity` RPC.
///
/// Called on every decoded stream event, and — deliberately — before each
/// mid-stream retry wait. The host-side tracker entry is *upserted* by the
/// first touch, and its presence doubles as the stall poller's "past the
/// first LLM call" signal (see `AgentContext::touch_activity`): without a
/// touch, a session whose FIRST round lands inside an overload burst would
/// sit under the poller's aggressive first-call stall cap (300s) while
/// waiting out the multi-minute throttle schedule, and the coordinator would
/// kill it mid-backoff — a retry budget the watchdog undercuts is worthless.
/// Each retry iteration waits at most 30s (the `backoff_delay_ms` cap), so
/// touching once per iteration keeps observed idle within roughly one
/// [`TOUCH_ACTIVITY_RPC_INTERVAL_SECS`] interval.
///
/// Returns the unix timestamp used, so the per-event path can reuse it for
/// its token-flush throttle.
async fn touch_stream_activity(ctx: &StreamLoopContext<'_>) -> u64 {
    let now = ctx
        .ctx
        .clock
        .now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    ctx.activity_ts.store(now, Ordering::Relaxed);
    // Bridge to the host's ActivityTracker.
    let last = ctx.last_rpc_touch.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= TOUCH_ACTIVITY_RPC_INTERVAL_SECS {
        ctx.last_rpc_touch.store(now, Ordering::Relaxed);
        if let Err(e) = ctx
            .ctx
            .callbacks
            .touch_activity_rpc(ctx.task_id.to_string())
            .await
        {
            tracing::warn!(
                task_id = %ctx.task_id,
                error = %e,
                "reply_loop::streaming: touch_activity RPC failed; \
                 host stall poller may see stale idle for this turn"
            );
        }
    }
    now
}

pub(super) async fn consume_provider_stream(
    mut ctx: StreamLoopContext<'_>,
) -> anyhow::Result<StreamTurnState> {
    let mut state = StreamTurnState::new();
    let mut streaming_inflight: FuturesUnordered<StreamingFut<'_>> = FuturesUnordered::new();
    // Attempts already spent restarting this round after a transient
    // mid-stream provider error (see `MAX_STREAM_EVENT_RETRIES`).
    let mut stream_event_retries: u32 = 0;
    loop {
        // A concurrent-safe side tool may have temporarily taken phase
        // ownership. Every select iteration waits for the provider again, so
        // reclaim provider wait before polling the stream. This closes an
        // active tool phase at the same instant; the outstanding guard later
        // only balances depth and cannot emit overlapping time.
        ctx.phase_tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .enter_provider_wait();
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
                        // A transient provider failure that arrived as a stream
                        // *event* (HTTP 200 was already returned, so the client's
                        // request-establishment retry cannot help) gets a bounded
                        // restart of the whole round — but only while the round is
                        // still safely restartable. Otherwise fall through to the
                        // unchanged terminal path.
                        if let Some((delay, max_attempts)) = transient_round_restart_delay(
                            &e,
                            stream_event_retries,
                            &state,
                            streaming_inflight.len(),
                        ) {
                            stream_event_retries += 1;
                            tracing::warn!(
                                task_id = %ctx.task_id,
                                session_id = %ctx.session_id,
                                attempt = stream_event_retries,
                                max_attempts,
                                delay_ms = delay.as_millis() as u64,
                                error = %e,
                                "provider stream event failed with a retryable error before any \
                                 output was emitted; retrying"
                            );
                            // A deliberate backoff is activity, not idleness:
                            // refresh the liveness clocks so the coordinator's
                            // stall poller keeps counting this session as live
                            // across a multi-minute throttle wait.
                            touch_stream_activity(&ctx).await;
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
                                _ = tokio::time::sleep(delay) => {}
                            }
                            // Re-issue the identical request. Copy the `&'a`
                            // inputs out first so `ctx.stream` can be reassigned.
                            let provider = ctx.provider;
                            let request_conversation = ctx.request_conversation;
                            let request_tools = ctx.request_tools;
                            let request_tool_choice = ctx.request_tool_choice;
                            match provider
                                .stream(request_conversation, request_tools, request_tool_choice)
                                .await
                            {
                                Ok(restarted) => {
                                    ctx.stream = restarted;
                                    // Nothing was emitted (asserted by
                                    // `round_is_safely_restartable`), so the
                                    // round starts from a clean slate.
                                    state = StreamTurnState::new();
                                    continue;
                                }
                                Err(restart_error) => {
                                    tracing::warn!(
                                        task_id = %ctx.task_id,
                                        session_id = %ctx.session_id,
                                        attempt = stream_event_retries,
                                        error = %restart_error,
                                        "retry of the provider stream failed to start; \
                                         surfacing the original mid-stream error"
                                    );
                                }
                            }
                        }
                        let diag = runtime_fs_diagnostics(ctx.project_path, ctx.worktree_path);
                        let env_diag = runtime_env_diagnostics(ctx.session_id, ctx.project_path, ctx.worktree_path);
                        let detail = format!(
                            "provider stream event failed: display={e} debug={e:?}; {diag}; {env_diag}"
                        );
                        return Err(e.context(detail));
                    }
                };
                state.saw_round_event = true;
                let now = touch_stream_activity(&ctx).await;
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
                        apply_persistence_event(
                            &mut state,
                            StreamEvent::Delta(ContentBlock::Text { text }),
                        );
                    }
                    StreamEvent::Delta(tool_use @ ContentBlock::ToolUse { .. }) => {
                        let idx = state.turn_tool_calls.len();
                        let should_dispatch_now = if let ContentBlock::ToolUse { name, .. } = &tool_use {
                            is_side_query_tool(ctx.tool_metadata, name)
                                && state.streaming_dispatched.len() < MAX_TOOL_CONCURRENCY
                        } else {
                            false
                        };
                        apply_persistence_event(&mut state, StreamEvent::Delta(tool_use));
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
                        apply_persistence_event(&mut state, StreamEvent::Delta(reasoning));
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
                        apply_persistence_event(&mut state, StreamEvent::Thinking(thinking));
                    }
                    StreamEvent::ThinkingDelta { id, text } => {
                        // Display/telemetry aggregate gets the attributed
                        // delta text appended once.
                        ctx.ctx.event_bus.send(DjinnEventEnvelope::session_message(
                            ctx.session_id,
                            ctx.task_id,
                            ctx.role_name,
                            &serde_json::json!({
                                "type": "thinking_delta",
                                "role": "assistant",
                                "text": text,
                            }),
                        ));
                        apply_persistence_event(
                            &mut state,
                            StreamEvent::ThinkingDelta { id, text },
                        );
                    }
                    StreamEvent::ThinkingBlockComplete {
                        id,
                        thinking,
                        signature,
                    } => {
                        // Materialize the load-bearing completion before
                        // marking its ID complete, making interruption safe.
                        apply_persistence_event(
                            &mut state,
                            StreamEvent::ThinkingBlockComplete {
                                id,
                                thinking,
                                signature,
                            },
                        );
                    }
                    StreamEvent::Usage(usage) => {
                        state.turn_tokens_in = usage.input;
                        state.turn_tokens_out = usage.output;
                        state.turn_cache_read = usage.cache_read;
                        state.turn_cache_write = usage.cache_write;
                        state.turn_reasoning_out = usage.reasoning_output;
                        record_provider_usage(
                            ctx.total_tokens_in, ctx.total_tokens_out, ctx.total_cache_read,
                            ctx.total_cache_write, ctx.total_reasoning_out,
                            ctx.current_context_tokens, &usage,
                        );
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
