//! Tool dispatch: delegates to host callbacks.
//!
//! Adapted from the agent implementation.  All host-only tool infrastructure
//! (stash tools, MCP tools, extension tools) is accessed through the
//! [`crate::host::SlotToolDispatcher`] trait.  No `djinn-agent` imports.

use std::collections::{HashMap, HashSet};

use crate::host::{SlotContext, SlotToolDispatcher};
use djinn_provider::message::ContentBlock;
use djinn_provider::provider::telemetry;

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
mod tests {
    use super::super::turn_budget::{
        DEFAULT_TURN_INLINE_CHAR_BUDGET, DEFAULT_TURN_INLINE_PREVIEW_FLOOR, TurnInlineBudgetConfig,
        apply_turn_inline_budget_pass_with_config, read_positive_env_usize,
    };
    use super::*;
    fn test_tool_schema(
        name: &str,
        read_only: Option<bool>,
        destructive: Option<bool>,
        idempotent: Option<bool>,
        open_world: Option<bool>,
        concurrent_safe: Option<bool>,
    ) -> serde_json::Value {
        let mut schema = serde_json::json!({
            "type": "function",
            "function": {
                "name": name,
                "description": "test",
                "parameters": {"type": "object"}
            }
        });
        let obj = schema.as_object_mut().expect("object schema");
        if let Some(value) = read_only {
            obj.insert("readOnly".to_string(), serde_json::Value::Bool(value));
        }
        if let Some(value) = destructive {
            obj.insert("destructive".to_string(), serde_json::Value::Bool(value));
        }
        if let Some(value) = idempotent {
            obj.insert("idempotent".to_string(), serde_json::Value::Bool(value));
        }
        if let Some(value) = open_world {
            obj.insert("openWorld".to_string(), serde_json::Value::Bool(value));
        }
        if let Some(value) = concurrent_safe {
            obj.insert(
                "concurrent_safe".to_string(),
                serde_json::Value::Bool(value),
            );
        }
        schema
    }
    #[test]
    fn runtime_metadata_parses_safety_annotations_and_gates_retry() {
        let schemas = vec![
            test_tool_schema(
                "safe_read",
                Some(true),
                Some(false),
                Some(true),
                Some(false),
                Some(true),
            ),
            test_tool_schema(
                "open_read",
                Some(true),
                Some(false),
                Some(true),
                Some(true),
                Some(true),
            ),
            test_tool_schema(
                "idempotent_write",
                Some(false),
                Some(false),
                Some(true),
                Some(false),
                Some(false),
            ),
            test_tool_schema(
                "non_idempotent_write",
                Some(false),
                Some(false),
                Some(false),
                Some(false),
                Some(false),
            ),
            test_tool_schema(
                "destructive",
                Some(false),
                Some(true),
                Some(true),
                Some(false),
                Some(false),
            ),
            test_tool_schema("missing_metadata", None, None, None, None, None),
        ];
        let metadata = tool_runtime_metadata(&schemas);
        assert_eq!(
            metadata["open_read"],
            ToolRuntimeMetadata {
                read_only: true,
                destructive: false,
                idempotent: true,
                open_world: true,
                concurrent_safe: true,
            }
        );
        assert!(is_side_query_tool(&metadata, "safe_read"));
        assert!(is_side_query_tool(&metadata, "open_read"));
        assert!(is_tool_retry_safe(&metadata, "safe_read"));
        assert!(is_tool_retry_safe(&metadata, "open_read"));
        assert!(is_tool_retry_safe(&metadata, "idempotent_write"));
        assert!(!is_side_query_tool(&metadata, "idempotent_write"));
        assert!(!is_side_query_tool(&metadata, "non_idempotent_write"));
        assert!(!is_tool_retry_safe(&metadata, "non_idempotent_write"));
        assert!(!is_side_query_tool(&metadata, "destructive"));
        assert!(!is_tool_retry_safe(&metadata, "destructive"));
        assert!(!is_side_query_tool(&metadata, "missing_metadata"));
        assert!(!is_tool_retry_safe(&metadata, "missing_metadata"));
        assert!(!is_side_query_tool(&metadata, "unknown"));
        assert!(!is_tool_retry_safe(&metadata, "unknown"));
    }
    #[tokio::test(start_paused = true)]
    async fn heartbeat_fires_while_a_long_tool_runs() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let beats = AtomicU32::new(0);
        let interval = std::time::Duration::from_secs(30);
        let out = run_with_heartbeat(
            interval,
            || async {
                beats.fetch_add(1, Ordering::SeqCst);
            },
            async {
                tokio::time::sleep(std::time::Duration::from_secs(95)).await;
                42u32
            },
        )
        .await;
        assert_eq!(out, 42);
        assert_eq!(
            beats.load(Ordering::SeqCst),
            3,
            "a 95s tool at a 30s cadence should beat at 30/60/90s"
        );
    }
    #[tokio::test(start_paused = true)]
    async fn heartbeat_does_not_fire_for_a_fast_tool() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let beats = AtomicU32::new(0);
        let out = run_with_heartbeat(
            std::time::Duration::from_secs(30),
            || async {
                beats.fetch_add(1, Ordering::SeqCst);
            },
            async { 7u32 },
        )
        .await;
        assert_eq!(out, 7);
        assert_eq!(beats.load(Ordering::SeqCst), 0);
    }

    fn test_dispatch_context<'a>(
        ctx: &'a SlotContext,
        tool_metadata: &'a ToolRuntimeMetadataMap,
        worktree_path: &'a std::path::Path,
    ) -> ToolDispatchContext<'a> {
        ToolDispatchContext {
            ctx,
            task_id: "test-task",
            worktree_path,
            role_name: "test-role",
            tool_metadata,
            tool_dispatcher: ctx.tool_dispatcher.as_ref().unwrap().as_ref(),
            otel_session: None,
        }
    }

    #[tokio::test]
    async fn collect_tool_results_preserves_names_and_ordering_across_serial_parallel_streaming() {
        use crate::test_helpers::{agent_context_from_db, create_test_db};
        use std::collections::HashSet;
        use tokio_util::sync::CancellationToken;

        let db = create_test_db();
        let ctx = agent_context_from_db(db, CancellationToken::new());
        let worktree_path = std::path::Path::new("/tmp");

        let schemas = vec![
            // serial: read-only but not concurrent-safe
            test_tool_schema(
                "shell",
                Some(true),
                Some(false),
                Some(true),
                Some(false),
                Some(false),
            ),
            // parallel
            test_tool_schema(
                "read",
                Some(true),
                Some(false),
                Some(true),
                Some(false),
                Some(true),
            ),
            test_tool_schema(
                "code_search",
                Some(true),
                Some(false),
                Some(true),
                Some(false),
                Some(true),
            ),
        ];
        let tool_metadata = tool_runtime_metadata(&schemas);

        let turn_tool_calls = vec![
            ContentBlock::ToolUse {
                id: "call-0".into(),
                name: "shell".into(),
                input: serde_json::json!({}),
            },
            ContentBlock::ToolUse {
                id: "call-1".into(),
                name: "read".into(),
                input: serde_json::json!({}),
            },
            ContentBlock::ToolUse {
                id: "call-2".into(),
                name: "code_search".into(),
                input: serde_json::json!({}),
            },
            ContentBlock::ToolUse {
                id: "call-3".into(),
                name: "write".into(),
                input: serde_json::json!({}),
            },
        ];

        let streaming_results = vec![(
            3,
            ContentBlock::ToolResult {
                tool_use_id: "call-3".into(),
                content: vec![ContentBlock::text("streamed write ok")],
                is_error: false,
            },
        )];
        let streaming_dispatched = HashSet::from([3]);

        let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);

        let collected = collect_tool_results_internal(
            &turn_tool_calls,
            streaming_results,
            &streaming_dispatched,
            &tool_metadata,
            &dispatch_ctx,
        )
        .await;

        assert_eq!(collected.len(), 4);
        assert_eq!(collected[0].idx, 0);
        assert_eq!(collected[1].idx, 1);
        assert_eq!(collected[2].idx, 2);
        assert_eq!(collected[3].idx, 3);

        assert_eq!(collected[0].tool_name, "shell");
        assert_eq!(collected[1].tool_name, "read");
        assert_eq!(collected[2].tool_name, "code_search");
        assert_eq!(collected[3].tool_name, "write");

        assert!(!collected.iter().any(|r| r.name_missing));

        let blocks: Vec<ContentBlock> = collected
            .into_iter()
            .map(CollectedToolResult::into_content_block)
            .collect();
        let ids: Vec<String> = blocks
            .iter()
            .map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => tool_use_id.clone(),
                _ => panic!("expected ToolResult"),
            })
            .collect();
        assert_eq!(ids, vec!["call-0", "call-1", "call-2", "call-3"]);

        let rendered_results: Vec<(String, String, bool)> = blocks
            .iter()
            .map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let [ContentBlock::Text { text }] = content.as_slice() else {
                        panic!("expected a ToolResult containing one text block");
                    };
                    (tool_use_id.clone(), text.clone(), *is_error)
                }
                _ => panic!("expected a ToolResult containing one text block"),
            })
            .collect();
        assert_eq!(
            rendered_results,
            vec![
                (
                    "call-0".to_string(),
                    "{\n  \"ok\": true,\n  \"exit_code\": 0,\n  \"stdout\": \"mock shell output\\n\",\n  \"stderr\": \"\",\n  \"workdir\": \"/tmp\"\n}"
                        .to_string(),
                    false,
                ),
                ("call-1".to_string(), "{\n  \"ok\": true\n}".to_string(), false),
                ("call-2".to_string(), "{\n  \"ok\": true\n}".to_string(), false),
                ("call-3".to_string(), "streamed write ok".to_string(), false),
            ]
        );
    }

    #[tokio::test]
    async fn collect_tool_results_uses_unknown_tool_for_nameless_input() {
        use crate::test_helpers::{agent_context_from_db, create_test_db};
        use std::collections::HashSet;
        use tokio_util::sync::CancellationToken;

        let db = create_test_db();
        let ctx = agent_context_from_db(db, CancellationToken::new());
        let worktree_path = std::path::Path::new("/tmp");
        let tool_metadata = ToolRuntimeMetadataMap::new();

        let turn_tool_calls = vec![ContentBlock::ToolUse {
            id: "call-0".into(),
            name: "shell".into(),
            input: serde_json::json!({}),
        }];

        let streaming_results = vec![(
            5,
            ContentBlock::ToolResult {
                tool_use_id: "call-5".into(),
                content: vec![ContentBlock::text("orphan result")],
                is_error: true,
            },
        )];
        let streaming_dispatched = HashSet::from([5]);

        let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);

        let collected = collect_tool_results_internal(
            &turn_tool_calls,
            streaming_results,
            &streaming_dispatched,
            &tool_metadata,
            &dispatch_ctx,
        )
        .await;

        assert_eq!(collected.len(), 2);
        assert_eq!(collected[0].idx, 0);
        assert_eq!(collected[0].tool_name, "shell");
        assert!(!collected[0].name_missing);

        assert_eq!(collected[1].idx, 5);
        assert_eq!(collected[1].tool_name, UNKNOWN_TOOL_NAME);
        assert!(collected[1].name_missing);
    }

    #[tokio::test]
    async fn collect_tool_results_preserves_mcp_and_extension_names() {
        use crate::test_helpers::{
            ConfigurableToolDispatcher, ToolHandlerFn, agent_context_from_db_with_dispatcher,
            create_test_db,
        };
        use std::collections::HashMap;
        use std::sync::Arc;
        use tokio_util::sync::CancellationToken;

        let db = create_test_db();
        let mut handlers: HashMap<String, ToolHandlerFn> = HashMap::new();
        handlers.insert(
            "mcp_fetch".to_string(),
            (|_| Ok(serde_json::json!({"ok": true}))) as ToolHandlerFn,
        );
        handlers.insert(
            "extension_compute".to_string(),
            (|_| Ok(serde_json::json!({"result": 42}))) as ToolHandlerFn,
        );
        let dispatcher = Arc::new(ConfigurableToolDispatcher::new(
            vec!["mcp_fetch".to_string()],
            handlers,
        ));
        let ctx =
            agent_context_from_db_with_dispatcher(db, CancellationToken::new(), Some(dispatcher));
        let worktree_path = std::path::Path::new("/tmp");

        let schemas = vec![
            test_tool_schema(
                "mcp_fetch",
                Some(true),
                Some(false),
                Some(true),
                Some(false),
                Some(true),
            ),
            test_tool_schema(
                "extension_compute",
                Some(true),
                Some(false),
                Some(true),
                Some(false),
                Some(true),
            ),
        ];
        let tool_metadata = tool_runtime_metadata(&schemas);

        let turn_tool_calls = vec![
            ContentBlock::ToolUse {
                id: "mcp-1".into(),
                name: "mcp_fetch".into(),
                input: serde_json::json!({}),
            },
            ContentBlock::ToolUse {
                id: "ext-1".into(),
                name: "extension_compute".into(),
                input: serde_json::json!({}),
            },
        ];

        let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);

        let collected = collect_tool_results_internal(
            &turn_tool_calls,
            Vec::new(),
            &HashSet::new(),
            &tool_metadata,
            &dispatch_ctx,
        )
        .await;

        assert_eq!(collected.len(), 2);
        assert_eq!(collected[0].tool_name, "mcp_fetch");
        assert_eq!(collected[1].tool_name, "extension_compute");
        assert!(!collected.iter().any(|r| r.name_missing));
    }

    #[tokio::test]
    async fn collect_tool_results_preserves_stash_tool_name() {
        use crate::test_helpers::{agent_context_from_db, create_test_db};
        use tokio_util::sync::CancellationToken;

        let db = create_test_db();
        let ctx = agent_context_from_db(db, CancellationToken::new());
        let worktree_path = std::path::Path::new("/tmp");
        let tool_metadata = ToolRuntimeMetadataMap::new();

        let turn_tool_calls = vec![ContentBlock::ToolUse {
            id: "stash-1".into(),
            name: "output_view".into(),
            input: serde_json::json!({"tool_use_id": "prior"}),
        }];

        let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);

        let collected = collect_tool_results_internal(
            &turn_tool_calls,
            Vec::new(),
            &HashSet::new(),
            &tool_metadata,
            &dispatch_ctx,
        )
        .await;

        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].tool_name, "output_view");
        assert!(!collected[0].name_missing);
    }

    // ─── Per-turn inline-character budget post-pass tests (v9ie) ────────────

    /// Build a `CollectedToolResult` for a single text-block tool result.
    fn collected_text(
        idx: usize,
        tool_use_id: &str,
        tool_name: &str,
        text: &str,
    ) -> CollectedToolResult {
        CollectedToolResult {
            idx,
            tool_use_id: tool_use_id.to_string(),
            tool_name: tool_name.to_string(),
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            is_error: false,
            name_missing: false,
        }
    }

    #[test]
    fn config_defaults_match_specification() {
        // The compiled-in constants must match the specification.
        assert_eq!(DEFAULT_TURN_INLINE_CHAR_BUDGET, 100_000);
        assert_eq!(DEFAULT_TURN_INLINE_PREVIEW_FLOOR, 10_000);
    }

    #[test]
    fn config_reads_validated_env_overrides() {
        // Direct parsing tests for the env-read helper; these don't touch the
        // post-pass so they are safe under parallel execution.
        assert_eq!(
            read_positive_env_usize("DJINN_TEST_BUDGET_OVERRIDE_NONEXISTENT", 42),
            42,
            "unset var falls back to default"
        );
        // The from_env constructor must produce the defaults when the env vars
        // are unset (validated independently of the constants test above).
        let config = TurnInlineBudgetConfig {
            budget: 100_000,
            preview_floor: 10_000,
        };
        assert_eq!(config.budget, DEFAULT_TURN_INLINE_CHAR_BUDGET);
        assert_eq!(config.preview_floor, DEFAULT_TURN_INLINE_PREVIEW_FLOOR);
    }

    #[tokio::test]
    async fn under_budget_turn_is_unchanged_byte_for_byte() {
        use crate::test_helpers::{agent_context_from_db, create_test_db};
        use tokio_util::sync::CancellationToken;

        let db = create_test_db();
        let ctx = agent_context_from_db(db, CancellationToken::new());
        let worktree_path = std::path::Path::new("/tmp");
        let tool_metadata = ToolRuntimeMetadataMap::new();
        let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);

        let body = "x".repeat(1_000);
        let mut results = vec![collected_text(0, "call-0", "read", &body)];
        let snapshot_before: Vec<String> = results
            .iter()
            .map(|r| match &r.content[0] {
                ContentBlock::Text { text } => text.clone(),
                _ => panic!("expected text"),
            })
            .collect();
        // Very large budget so the turn is guaranteed under budget.
        let config = TurnInlineBudgetConfig {
            budget: 100_000_000,
            preview_floor: 10_000,
        };
        apply_turn_inline_budget_pass_with_config(&mut results, &dispatch_ctx, config);
        let snapshot_after: Vec<String> = results
            .iter()
            .map(|r| match &r.content[0] {
                ContentBlock::Text { text } => text.clone(),
                _ => panic!("expected text"),
            })
            .collect();
        assert_eq!(
            snapshot_before, snapshot_after,
            "under-budget turn must be byte-for-byte unchanged"
        );
    }

    #[tokio::test]
    async fn largest_first_selection_externalizes_the_biggest_candidate() {
        use crate::test_helpers::{agent_context_from_db, create_test_db};
        use tokio_util::sync::CancellationToken;

        let db = create_test_db();
        let ctx = agent_context_from_db(db, CancellationToken::new());
        let worktree_path = std::path::Path::new("/tmp");
        let tool_metadata = ToolRuntimeMetadataMap::new();
        let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);

        // Small budget + small floor so externalization triggers and the stub
        // is genuinely smaller than the original large body.
        let config = TurnInlineBudgetConfig {
            budget: 200,
            preview_floor: 10,
        };
        let big = "B".repeat(5_000);
        let small = "S".repeat(500);
        let mut results = vec![
            collected_text(0, "call-big", "shell", &big),
            collected_text(1, "call-small", "read", &small),
        ];
        apply_turn_inline_budget_pass_with_config(&mut results, &dispatch_ctx, config);

        // The biggest candidate must be externalized (stub header present).
        let big_text = match &results[0].content[0] {
            ContentBlock::Text { text } => text.as_str(),
            _ => panic!("expected text"),
        };
        assert!(
            big_text.starts_with("[djinn-output-stash"),
            "largest candidate should be externalized, got: {}",
            &big_text[..big_text.len().min(80)]
        );
        assert!(big_text.contains("reason=\"turn_budget\""));
        assert!(big_text.contains("tool_name=\"shell\""));
    }

    #[tokio::test]
    async fn non_shrinking_stub_is_skipped() {
        use crate::test_helpers::{agent_context_from_db, create_test_db};
        use tokio_util::sync::CancellationToken;

        let db = create_test_db();
        let ctx = agent_context_from_db(db, CancellationToken::new());
        let worktree_path = std::path::Path::new("/tmp");
        let tool_metadata = ToolRuntimeMetadataMap::new();
        let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);

        // A candidate just above the floor whose externalized stub (header +
        // preview) would not be smaller than the original is skipped.
        // 41 chars: above the 40-char floor, but the stub header alone exceeds
        // 41 chars so externalization cannot shrink it → skip, allow overflow.
        let config = TurnInlineBudgetConfig {
            budget: 50,
            preview_floor: 40,
        };
        let body = "x".repeat(41);
        let original = body.clone();
        let mut results = vec![collected_text(0, "call-0", "read", &body)];
        apply_turn_inline_budget_pass_with_config(&mut results, &dispatch_ctx, config);

        let text = match &results[0].content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert_eq!(
            text, original,
            "non-shrinking stub must be skipped, leaving the original unchanged"
        );
    }

    #[tokio::test]
    async fn preview_floor_prevents_fitting_allows_overflow() {
        use crate::test_helpers::{agent_context_from_db, create_test_db};
        use tokio_util::sync::CancellationToken;

        let db = create_test_db();
        let ctx = agent_context_from_db(db, CancellationToken::new());
        let worktree_path = std::path::Path::new("/tmp");
        let tool_metadata = ToolRuntimeMetadataMap::new();
        let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);

        // Every candidate is at or below the preview floor, so none can shrink
        // and the overflow must be permitted rather than shrinking previews.
        // Two 500-char results: total 1000 > 100 budget, but both are below the
        // 10000-char floor so neither is eligible → overflow permitted.
        let config = TurnInlineBudgetConfig {
            budget: 100,
            preview_floor: 10_000,
        };
        let body_a = "A".repeat(500);
        let body_b = "B".repeat(500);
        let original_a = body_a.clone();
        let original_b = body_b.clone();
        let mut results = vec![
            collected_text(0, "call-0", "read", &body_a),
            collected_text(1, "call-1", "read", &body_b),
        ];
        apply_turn_inline_budget_pass_with_config(&mut results, &dispatch_ctx, config);

        let text_a = match &results[0].content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        let text_b = match &results[1].content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert_eq!(
            text_a, original_a,
            "floor-limited candidate must remain unchanged"
        );
        assert_eq!(
            text_b, original_b,
            "floor-limited candidate must remain unchanged"
        );
    }

    #[tokio::test]
    async fn externalization_preserves_tool_use_id_and_name_in_stub() {
        use crate::test_helpers::{agent_context_from_db, create_test_db};
        use tokio_util::sync::CancellationToken;

        let db = create_test_db();
        let ctx = agent_context_from_db(db, CancellationToken::new());
        let worktree_path = std::path::Path::new("/tmp");
        let tool_metadata = ToolRuntimeMetadataMap::new();
        let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);

        let config = TurnInlineBudgetConfig {
            budget: 200,
            preview_floor: 10,
        };
        let big = "Z".repeat(5_000);
        let mut results = vec![collected_text(7, "call-preserve-id", "code_search", &big)];
        apply_turn_inline_budget_pass_with_config(&mut results, &dispatch_ctx, config);

        let text = match &results[0].content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("tool_use_id=\"call-preserve-id\""));
        assert!(text.contains("tool_name=\"code_search\""));
        assert!(text.contains("reason=\"turn_budget\""));
    }

    // ─── Telemetry regression: group-level tool_name_missing ────────────────

    /// A `MakeWriter` that captures tracing output into a buffer so tests can
    /// assert on structured log content.
    #[derive(Clone, Default)]
    struct CapturedLogs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl CapturedLogs {
        fn output(&self) -> String {
            let buf = self.0.lock().expect("captured logs mutex poisoned");
            String::from_utf8(buf.clone()).expect("captured log bytes were not valid utf-8")
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
        type Writer = CapturedLogsWriter;
        fn make_writer(&'a self) -> Self::Writer {
            CapturedLogsWriter {
                inner: std::sync::Arc::clone(&self.0),
            }
        }
    }
    struct CapturedLogsWriter {
        inner: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }
    impl std::io::Write for CapturedLogsWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.inner
                .lock()
                .expect("captured logs mutex poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Regression for the group-level `tool_name_missing` telemetry flag.
    ///
    /// When a nameless result exists in the batch but is NOT selected for
    /// externalization (because a larger named result trips the budget and is
    /// selected first), the telemetry must still report `tool_name_missing=true`.
    /// This mirrors the caller-reachable scenario where an orphan streaming
    /// result (constructed without an originating `ToolUse` name) coexists with
    /// a larger named result.
    #[tokio::test]
    async fn budget_trip_reports_tool_name_missing_for_unselected_nameless_result() {
        use crate::test_helpers::{agent_context_from_db, create_test_db};
        use tokio_util::sync::CancellationToken;

        let db = create_test_db();
        let ctx = agent_context_from_db(db, CancellationToken::new());
        let worktree_path = std::path::Path::new("/tmp");
        let tool_metadata = ToolRuntimeMetadataMap::new();
        let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);

        // Small budget + small floor so externalization triggers.
        let config = TurnInlineBudgetConfig {
            budget: 200,
            preview_floor: 10,
        };

        // A large named result that will be externalized first.
        let big = "B".repeat(5_000);
        let big_result = collected_text(0, "call-big", "shell", &big);

        // A small nameless result that will NOT be selected for externalization
        // (it is smaller than the named result and below the budget after the
        // big result is externalized). This mirrors the orphan streaming result.
        let small_nameless = CollectedToolResult {
            idx: 5,
            tool_use_id: "call-5".to_string(),
            tool_name: UNKNOWN_TOOL_NAME.to_string(),
            content: vec![ContentBlock::Text {
                text: "orphan result".to_string(),
            }],
            is_error: true,
            name_missing: true,
        };

        let mut results = vec![big_result, small_nameless];

        // Capture the tracing output to verify the telemetry flag.
        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(logs.clone())
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NONE)
            .with_target(true)
            .with_ansi(false)
            .with_level(true)
            .finish();
        let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
        let _guard = tracing::dispatcher::set_default(&dispatch);

        apply_turn_inline_budget_pass_with_config(&mut results, &dispatch_ctx, config);

        let output = logs.output();
        assert!(
            output.contains("tool_name_missing=true"),
            "telemetry must report tool_name_missing=true when a nameless result \
             exists in the batch, even if it was not selected for externalization. \
             Got: {output}"
        );
    }
}
