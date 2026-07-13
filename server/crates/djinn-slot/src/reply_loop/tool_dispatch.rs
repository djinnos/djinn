//! Tool dispatch: delegates to host callbacks.
//!
//! Adapted from the agent implementation.  All host-only tool infrastructure
//! (stash tools, MCP tools, extension tools) is accessed through the
//! [`crate::host::SlotToolDispatcher`] trait.  No `djinn-agent` imports.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::host::{SlotContext, SlotToolDispatcher};
use djinn_provider::message::ContentBlock;
use djinn_provider::provider::telemetry;

use super::phase::SessionPhaseTracker;

/// Balances the session phase around an entire outer tool dispatch, including
/// retries and backoff. Nested and parallel calls share tracker depth.
struct ToolPhaseGuard<'a> {
    tracker: &'a Arc<Mutex<SessionPhaseTracker>>,
}

impl ToolPhaseGuard<'_> {
    fn enter(tracker: &Arc<Mutex<SessionPhaseTracker>>) -> ToolPhaseGuard<'_> {
        tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .enter_tool_execution();
        ToolPhaseGuard { tracker }
    }
}

impl Drop for ToolPhaseGuard<'_> {
    fn drop(&mut self) {
        self.tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .exit_tool_execution();
    }
}

/// Maximum number of concurrent-safe tools that can execute in parallel within
/// a single batch (ADR-048 §1A).
pub(super) const MAX_TOOL_CONCURRENCY: usize = 8;

/// Cadence at which an in-flight tool call emits a liveness heartbeat.
const TOOL_HEARTBEAT_SECS: u64 = 30;

fn tool_heartbeat_interval() -> std::time::Duration {
    std::time::Duration::from_secs(TOOL_HEARTBEAT_SECS)
}

/// Drive `fut` to completion while invoking `beat` every `interval`.
async fn run_with_heartbeat<F, T, MkBeat, BeatFut>(
    interval: std::time::Duration,
    mk_beat: MkBeat,
    fut: F,
) -> T
where
    F: std::future::Future<Output = T>,
    MkBeat: Fn() -> BeatFut,
    BeatFut: std::future::Future<Output = ()>,
{
    let mut fut = std::pin::pin!(fut);
    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await;
    loop {
        tokio::select! {
            out = &mut fut => return out,
            _ = ticker.tick() => mk_beat().await,
        }
    }
}

/// One liveness heartbeat: best-effort touch_activity callback.
async fn beat_activity(ctx: &SlotContext, task_id: &str) {
    if let Err(e) = ctx.callbacks.touch_activity_rpc(task_id.to_string()).await {
        tracing::debug!(
            task_id = %task_id,
            error = %e,
            "ReplyLoop: tool-heartbeat touch_activity failed"
        );
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct ToolRuntimeMetadata {
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
    pub open_world: bool,
    pub concurrent_safe: bool,
}

impl ToolRuntimeMetadata {
    pub(super) fn auto_approval_safe(self) -> bool {
        self.read_only && self.idempotent && !self.destructive && self.concurrent_safe
    }
    pub(super) fn retry_safe(self) -> bool {
        !self.destructive && (self.read_only || self.idempotent)
    }
}

pub(super) type ToolRuntimeMetadataMap = HashMap<String, ToolRuntimeMetadata>;

fn schema_bool(tool: &serde_json::Value, key: &str) -> bool {
    tool.get(key)
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

pub(super) fn tool_runtime_metadata(tools: &[serde_json::Value]) -> ToolRuntimeMetadataMap {
    tools
        .iter()
        .filter_map(|tool| {
            let name = tool
                .get("name")
                .and_then(|value| value.as_str())
                .or_else(|| {
                    tool.get("function")
                        .and_then(|value| value.get("name"))
                        .and_then(|value| value.as_str())
                })?;
            Some((
                name.to_string(),
                ToolRuntimeMetadata {
                    read_only: schema_bool(tool, "readOnly"),
                    destructive: schema_bool(tool, "destructive"),
                    idempotent: schema_bool(tool, "idempotent"),
                    open_world: schema_bool(tool, "openWorld"),
                    concurrent_safe: schema_bool(tool, "concurrent_safe"),
                },
            ))
        })
        .collect()
}

pub(super) fn is_tool_concurrent_safe(tool_metadata: &ToolRuntimeMetadataMap, name: &str) -> bool {
    tool_metadata
        .get(name)
        .copied()
        .map(|metadata| metadata.concurrent_safe)
        .unwrap_or(false)
}

pub(super) fn is_tool_retry_safe(tool_metadata: &ToolRuntimeMetadataMap, name: &str) -> bool {
    tool_metadata
        .get(name)
        .copied()
        .map(ToolRuntimeMetadata::retry_safe)
        .unwrap_or(false)
}

/// ADR-048 side queries are ordinary tool calls whose schema marks them `concurrent_safe=true`.
pub(super) fn is_side_query_tool(tool_metadata: &ToolRuntimeMetadataMap, name: &str) -> bool {
    tool_metadata
        .get(name)
        .copied()
        .map(ToolRuntimeMetadata::auto_approval_safe)
        .unwrap_or(false)
}

pub(super) struct ToolDispatchContext<'a> {
    pub ctx: &'a SlotContext,
    pub task_id: &'a str,
    pub worktree_path: &'a std::path::Path,
    pub role_name: &'a str,
    pub tool_metadata: &'a ToolRuntimeMetadataMap,
    pub tool_dispatcher: &'a dyn SlotToolDispatcher,
    pub otel_session: Option<&'a telemetry::SessionSpan>,
    pub phase_tracker: Option<&'a Arc<Mutex<SessionPhaseTracker>>>,
}

/// Per-call fields passed into [`dispatch_single_tool`].
pub(super) struct ToolDispatchRequest {
    pub(super) idx: usize,
    pub id: String,
    pub name: String,
    pub args: Option<serde_json::Map<String, serde_json::Value>>,
    pub tool_span: Option<djinn_provider::provider::telemetry::ToolSpan>,
    pub retry_safe: bool,
}

pub(super) enum ToolBatch {
    Parallel(Vec<usize>),
    Serial(usize),
}

pub(super) fn build_tool_batches<'a>(
    turn_tool_calls: &'a [ContentBlock],
    streaming_dispatched: &HashSet<usize>,
    tool_metadata: &ToolRuntimeMetadataMap,
) -> (Vec<(usize, &'a ContentBlock)>, Vec<ToolBatch>) {
    let indexed_tool_calls: Vec<(usize, &ContentBlock)> = turn_tool_calls
        .iter()
        .enumerate()
        .filter(|(idx, b)| {
            matches!(b, ContentBlock::ToolUse { .. }) && !streaming_dispatched.contains(idx)
        })
        .collect();
    let mut batches: Vec<ToolBatch> = Vec::new();
    let mut current_parallel: Vec<usize> = Vec::new();
    for &(idx, block) in &indexed_tool_calls {
        let name = match block {
            ContentBlock::ToolUse { name, .. } => name.as_str(),
            _ => unreachable!(),
        };
        if is_tool_concurrent_safe(tool_metadata, name) {
            current_parallel.push(idx);
        } else {
            if !current_parallel.is_empty() {
                batches.push(ToolBatch::Parallel(std::mem::take(&mut current_parallel)));
            }
            batches.push(ToolBatch::Serial(idx));
        }
    }
    if !current_parallel.is_empty() {
        batches.push(ToolBatch::Parallel(current_parallel));
    }
    (indexed_tool_calls, batches)
}

pub(super) async fn dispatch_single_tool<'a>(
    req: ToolDispatchRequest,
    ctx: &'a ToolDispatchContext<'a>,
) -> (usize, ContentBlock) {
    // Enclose selection, dispatch, retry/backoff, result rendering, and every
    // return. Drop also balances accounting if cancellation drops this future.
    let _phase_guard = ctx.phase_tracker.map(ToolPhaseGuard::enter);
    let ToolDispatchRequest {
        idx,
        id,
        name,
        args,
        tool_span,
        retry_safe,
    } = req;
    if ctx.tool_dispatcher.is_stash_tool(&name) {
        let result = ctx.tool_dispatcher.handle_stash_call(&name, args.as_ref());
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
                (
                    vec![ContentBlock::Text {
                        text: format!("error: {err}"),
                    }],
                    true,
                )
            }
        };
        if let Some(ts) = tool_span {
            if is_error {
                ts.end_error("tool returned error");
            } else {
                ts.end_ok();
            }
        }
        return (
            idx,
            ContentBlock::ToolResult {
                tool_use_id: id,
                content,
                is_error,
            },
        );
    }
    if ctx.tool_dispatcher.is_mcp_tool(&name) {
        tracing::debug!(task_id = %ctx.task_id, tool = %name, "ReplyLoop: dispatching to MCP server");
        let mcp_result = run_with_heartbeat(
            tool_heartbeat_interval(),
            || beat_activity(ctx.ctx, ctx.task_id),
            ctx.tool_dispatcher.dispatch_mcp_tool(&name, args.clone()),
        )
        .await;
        let (content, is_error) = match mcp_result {
            Ok(value) => {
                let text = ctx.tool_dispatcher.render_result(&id, &name, &value);
                if let Some(ts) = &tool_span {
                    ts.record_output(&text, false);
                }
                (vec![ContentBlock::Text { text }], false)
            }
            Err(err) => {
                if let Some(ts) = &tool_span {
                    ts.record_output(&err, true);
                }
                (
                    vec![ContentBlock::Text {
                        text: format!("error: {err}"),
                    }],
                    true,
                )
            }
        };
        if let Some(ts) = tool_span {
            if is_error {
                ts.end_error("MCP tool returned error");
            } else {
                ts.end_ok();
            }
        }
        return (
            idx,
            ContentBlock::ToolResult {
                tool_use_id: id,
                content,
                is_error,
            },
        );
    }
    // Native MCP resource tools (`list_mcp_resources`, `read_mcp_resource`).
    // Dispatched through the host's resource callback; results arrive as text.
    if ctx.tool_dispatcher.is_resource_tool(&name) {
        tracing::debug!(task_id = %ctx.task_id, tool = %name, "ReplyLoop: dispatching native MCP resource tool");
        let resource_result = run_with_heartbeat(
            tool_heartbeat_interval(),
            || beat_activity(ctx.ctx, ctx.task_id),
            ctx.tool_dispatcher
                .dispatch_resource_tool(&name, args.clone()),
        )
        .await;
        let (content, is_error) = match resource_result {
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
                (
                    vec![ContentBlock::Text {
                        text: format!("error: {err}"),
                    }],
                    true,
                )
            }
        };
        if let Some(ts) = tool_span {
            if is_error {
                ts.end_error("MCP resource tool returned error");
            } else {
                ts.end_ok();
            }
        }
        return (
            idx,
            ContentBlock::ToolResult {
                tool_use_id: id,
                content,
                is_error,
            },
        );
    }
    let mut result = run_with_heartbeat(
        tool_heartbeat_interval(),
        || beat_activity(ctx.ctx, ctx.task_id),
        ctx.tool_dispatcher.dispatch_extension_tool(
            &name,
            args.clone(),
            ctx.worktree_path,
            ctx.task_id,
            ctx.role_name,
        ),
    )
    .await;
    {
        let mut retries = 0u32;
        while retries < 5 {
            match &result {
                Err(e) if e.contains("database is locked") && retry_safe => {
                    retries += 1;
                    let backoff = std::time::Duration::from_millis(100 * (1 << retries.min(4)));
                    tracing::warn!(
                        task_id = %ctx.task_id,
                        tool = %name,
                        retry = retries,
                        backoff_ms = backoff.as_millis() as u64,
                        "ReplyLoop: database locked, retrying"
                    );
                    tokio::time::sleep(backoff).await;
                    result = run_with_heartbeat(
                        tool_heartbeat_interval(),
                        || beat_activity(ctx.ctx, ctx.task_id),
                        ctx.tool_dispatcher.dispatch_extension_tool(
                            &name,
                            args.clone(),
                            ctx.worktree_path,
                            ctx.task_id,
                            ctx.role_name,
                        ),
                    )
                    .await;
                }
                _ => break,
            }
        }
    }
    let (content, is_error) = match result {
        Ok(value) => {
            let text = ctx.tool_dispatcher.render_result(&id, &name, &value);
            if let Some(ts) = &tool_span {
                ts.record_output(&text, false);
            }
            (vec![ContentBlock::Text { text }], false)
        }
        Err(err) => {
            tracing::warn!(task_id = %ctx.task_id, tool = %name, error = %err, "ReplyLoop: tool call returned error");
            let err_text = format!("error: {err}");
            if let Some(ts) = &tool_span {
                ts.record_output(&err_text, true);
            }
            (vec![ContentBlock::Text { text: err_text }], true)
        }
    };
    if let Some(ts) = tool_span {
        if is_error {
            ts.end_error("tool returned error");
        } else {
            ts.end_ok();
        }
    }
    (
        idx,
        ContentBlock::ToolResult {
            tool_use_id: id,
            content,
            is_error,
        },
    )
}

pub(super) fn make_tool_future<'a>(
    idx: usize,
    tool_call: ContentBlock,
    ctx: &'a ToolDispatchContext<'a>,
) -> impl std::future::Future<Output = (usize, ContentBlock)> + Send + 'a {
    let ContentBlock::ToolUse { id, name, input } = tool_call else {
        unreachable!("filtered above");
    };
    tracing::debug!(
        task_id = %ctx.task_id,
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
    let mcp_server_name = ctx.tool_dispatcher.mcp_server_for_tool(&name);
    let tool_span = ctx.otel_session.map(|session| {
        let ts = telemetry::ToolSpan::start_with_server(
            session.context(),
            &name,
            &id,
            mcp_server_name.as_deref(),
        );
        ts.record_input(&input_json.to_string());
        ts
    });
    let retry_safe = is_tool_retry_safe(ctx.tool_metadata, &name);
    let req = ToolDispatchRequest {
        idx,
        id,
        name,
        args,
        tool_span,
        retry_safe,
    };
    dispatch_single_tool(req, ctx)
}

pub(super) async fn collect_tool_results(
    turn_tool_calls: &[ContentBlock],
    streaming_results: Vec<(usize, ContentBlock)>,
    streaming_dispatched: &HashSet<usize>,
    tool_metadata: &ToolRuntimeMetadataMap,
    ctx: &ToolDispatchContext<'_>,
) -> Vec<ContentBlock> {
    let mut collected = collect_tool_results_internal(
        turn_tool_calls,
        streaming_results,
        streaming_dispatched,
        tool_metadata,
        ctx,
    )
    .await;
    // Per-turn inline-character budget post-pass (v9ie). Runs immediately after
    // the serial/parallel/streaming results are merged and sorted, before they
    // are converted to transcript ContentBlocks. Greedily externalizes the
    // largest shrinking tool-result candidates until the projected inline-char
    // total fits the configured budget or no candidate can shrink below the
    // configured preview floor.
    super::turn_budget::apply_turn_inline_budget_pass(&mut collected, ctx);
    collected
        .into_iter()
        .map(CollectedToolResult::into_content_block)
        .collect()
}

const UNKNOWN_TOOL_NAME: &str = "unknown_tool";

/// Internal collected result that preserves the originating tool name and the
/// original index through merge and sort so a later per-turn policy pass can
/// correlate results with their `ToolUse` without re-scanning the transcript.
#[derive(Debug, Clone)]
pub(super) struct CollectedToolResult {
    pub(super) idx: usize,
    pub(super) tool_use_id: String,
    pub(super) tool_name: String,
    pub(super) content: Vec<ContentBlock>,
    pub(super) is_error: bool,
    /// True when the originating `ToolUse` did not have a tool name at the
    /// original index; the explicit `unknown_tool` sentinel is used.
    pub(super) name_missing: bool,
}

impl CollectedToolResult {
    fn into_content_block(self) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: self.tool_use_id,
            content: self.content,
            is_error: self.is_error,
        }
    }
}

fn resolve_tool_name(turn_tool_calls: &[ContentBlock], idx: usize) -> (String, bool) {
    match turn_tool_calls.get(idx) {
        Some(ContentBlock::ToolUse { name, .. }) => (name.clone(), false),
        _ => (UNKNOWN_TOOL_NAME.to_string(), true),
    }
}

fn collect_tool_result_from_block(
    idx: usize,
    block: ContentBlock,
    turn_tool_calls: &[ContentBlock],
) -> CollectedToolResult {
    match block {
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let (tool_name, name_missing) = resolve_tool_name(turn_tool_calls, idx);
            CollectedToolResult {
                idx,
                tool_use_id,
                tool_name,
                content,
                is_error,
                name_missing,
            }
        }
        _ => {
            let (tool_name, name_missing) = resolve_tool_name(turn_tool_calls, idx);
            CollectedToolResult {
                idx,
                tool_use_id: String::new(),
                tool_name,
                content: vec![block],
                is_error: false,
                name_missing,
            }
        }
    }
}

async fn collect_tool_results_internal(
    turn_tool_calls: &[ContentBlock],
    streaming_results: Vec<(usize, ContentBlock)>,
    streaming_dispatched: &HashSet<usize>,
    tool_metadata: &ToolRuntimeMetadataMap,
    ctx: &ToolDispatchContext<'_>,
) -> Vec<CollectedToolResult> {
    // rdx6 introduced the host seam for externalizing an already-rendered
    // result; v9ie applies that seam as a per-turn inline-character budget
    // post-pass in collect_tool_results immediately after this function returns
    // the sorted results. This function only collects, merges, and sorts.
    let (indexed_tool_calls, batches) =
        build_tool_batches(turn_tool_calls, streaming_dispatched, tool_metadata);
    let total_tools = turn_tool_calls
        .iter()
        .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
        .count();
    if total_tools > 0 {
        let safe_remaining: usize = batches
            .iter()
            .map(|b| match b {
                ToolBatch::Parallel(v) => v.len(),
                ToolBatch::Serial(_) => 0,
            })
            .sum();
        let serial_remaining = indexed_tool_calls.len().saturating_sub(safe_remaining);
        tracing::debug!(
            task_id = %ctx.task_id,
            total = total_tools,
            streamed = streaming_dispatched.len(),
            remaining_safe = safe_remaining,
            remaining_serial = serial_remaining,
            batch_count = batches.len(),
            "ReplyLoop: tool call dispatch (ADR-048 §1A+§1B)"
        );
    }
    let mut indexed_results: Vec<CollectedToolResult> =
        Vec::with_capacity(indexed_tool_calls.len() + streaming_results.len());
    indexed_results.extend(
        streaming_results
            .into_iter()
            .map(|(idx, block)| collect_tool_result_from_block(idx, block, turn_tool_calls)),
    );
    for batch in &batches {
        match batch {
            ToolBatch::Parallel(indices) => {
                for chunk in indices.chunks(MAX_TOOL_CONCURRENCY) {
                    let futures: Vec<_> = chunk
                        .iter()
                        .map(|&idx| make_tool_future(idx, turn_tool_calls[idx].clone(), ctx))
                        .collect();
                    let results = futures::future::join_all(futures).await;
                    indexed_results.extend(results.into_iter().map(|(idx, block)| {
                        collect_tool_result_from_block(idx, block, turn_tool_calls)
                    }));
                }
            }
            ToolBatch::Serial(idx) => {
                let result = make_tool_future(*idx, turn_tool_calls[*idx].clone(), ctx).await;
                indexed_results.push(collect_tool_result_from_block(
                    result.0,
                    result.1,
                    turn_tool_calls,
                ));
            }
        }
    }
    indexed_results.sort_by_key(|r| r.idx);
    indexed_results
}

#[cfg(test)]
#[path = "tool_dispatch_tests.rs"]
mod tool_dispatch_tests;
