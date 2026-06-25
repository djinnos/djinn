use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::extension;
use crate::output_stash::{OutputStash, handle_stash_tool, is_stash_tool, render_tool_result};
use djinn_provider::message::ContentBlock;
use djinn_provider::provider::telemetry;

/// Maximum number of concurrent-safe tools that can execute in parallel within
/// a single batch (ADR-048 §1A).
pub(super) const MAX_TOOL_CONCURRENCY: usize = 8;

/// Cadence at which an in-flight tool call emits a liveness heartbeat
/// (`touch_activity`).  The worker otherwise only touches activity on LLM
/// stream events (see `reply_loop/streaming.rs`), so a multi-minute *blocking*
/// tool — most acutely a cold `cargo clippy` on a large workspace — emits
/// nothing for its whole duration.  The coordinator's 10-minute zombie hard cap
/// (`coordinator/dispatch.rs::reap_zombie_sessions`) then cannot tell a live,
/// busy worker from a dead pod (both read `idle_seconds > 600`) and false-reaps
/// the session mid-build.  30s keeps `idle_seconds` well under every reap
/// threshold (180s zero-token breaker, 300s first-call, 600s zombie cap).
const TOOL_HEARTBEAT_SECS: u64 = 30;

fn tool_heartbeat_interval() -> std::time::Duration {
    std::time::Duration::from_secs(TOOL_HEARTBEAT_SECS)
}

/// Drive `fut` to completion while invoking `beat` every `interval`.
///
/// The heartbeat runs in the SAME task via `select!`, so the beat future may
/// borrow freely (no `'static`/`spawn` requirement — `services`/`task_id` are
/// borrows). `tokio::time::interval`'s first tick is immediate and is consumed
/// up front, so a tool that completes before the first cadence never beats.
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
    // Consume the immediate first tick so we only beat once a tool has actually
    // been running for `interval`.
    ticker.tick().await;
    loop {
        tokio::select! {
            out = &mut fut => return out,
            _ = ticker.tick() => mk_beat().await,
        }
    }
}

/// One liveness heartbeat: best-effort `touch_activity` RPC. Failures are
/// logged at debug and swallowed — a missed beat must never abort a tool call.
async fn beat_activity(services: &dyn djinn_supervisor::SupervisorServices, task_id: &str) {
    if let Err(e) = services.touch_activity(task_id.to_string()).await {
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

/// ADR-048 side queries are not a separate protocol primitive.
///
/// In the reply loop architecture they are modeled as ordinary tool calls whose
/// schema marks them explicitly read-only, idempotent, non-destructive, and
/// `concurrent_safe=true`, meaning the lookup can be started opportunistically
/// during streaming without mutation risk. Their results still flow back through
/// the normal `tool_result` message on the next user turn so provider tool-call
/// pairing remains valid.
pub(super) fn is_side_query_tool(tool_metadata: &ToolRuntimeMetadataMap, name: &str) -> bool {
    tool_metadata
        .get(name)
        .copied()
        .map(ToolRuntimeMetadata::auto_approval_safe)
        .unwrap_or(false)
}

pub(super) struct ToolDispatchContext<'a> {
    pub app_state: &'a crate::context::AgentContext,
    /// `SupervisorServices` handle used by the host-only tool subset
    /// (`github_search`, `github_fetch_file`, `ci_job_log`) to route over
    /// RPC when the reply loop runs inside a worker Pod. See the parent
    /// `ReplyLoopContext::services` doc for context.
    pub services: &'a dyn djinn_supervisor::SupervisorServices,
    pub task_id: &'a str,
    pub worktree_path: &'a std::path::Path,
    pub role_name: &'a str,
    pub mcp_registry: Option<&'a crate::mcp_client::McpToolRegistry>,
    pub tool_metadata: &'a ToolRuntimeMetadataMap,
    pub output_stash: Arc<Mutex<OutputStash>>,
    pub otel_session: Option<&'a telemetry::SessionSpan>,
}

/// Per-call fields passed into [`dispatch_single_tool`].
///
/// Grouped separately from the borrowed [`ToolDispatchContext`] so the dispatch
/// entry-point does not need `#[allow(clippy::too_many_arguments)]`.
pub(super) struct ToolDispatchRequest {
    pub idx: usize,
    pub id: String,
    pub name: String,
    pub args: Option<serde_json::Map<String, serde_json::Value>>,
    pub tool_span: Option<djinn_provider::provider::telemetry::ToolSpan>,
    pub stash: Arc<Mutex<OutputStash>>,
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
        stash,
        retry_safe,
    } = req;
    if is_stash_tool(&name) {
        let result = handle_stash_tool(&stash, &name, args.as_ref());
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

    if let Some(registry) = ctx.mcp_registry
        && registry.has_tool(&name)
    {
        tracing::debug!(task_id = %ctx.task_id, tool = %name, "ReplyLoop: dispatching to MCP server");
        let mcp_result = run_with_heartbeat(
            tool_heartbeat_interval(),
            || beat_activity(ctx.services, ctx.task_id),
            registry.call_tool(&name, args.clone()),
        )
        .await;
        let (content, is_error) = match mcp_result {
            Ok(value) => {
                // Route MCP results through the same stash/truncate/pagination
                // chokepoint as native tools so an oversized MCP payload (e.g. a
                // 12MB result) is bounded + stashed instead of blowing the
                // context window / tripping the 400.
                let text = render_tool_result(&stash, &id, &name, &value);
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

    let mut result = run_with_heartbeat(
        tool_heartbeat_interval(),
        || beat_activity(ctx.services, ctx.task_id),
        extension::call_tool(
            ctx.app_state,
            ctx.services,
            &name,
            args.clone(),
            ctx.worktree_path,
            Some(ctx.task_id),
            Some(ctx.role_name),
            ctx.mcp_registry,
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
                        || beat_activity(ctx.services, ctx.task_id),
                        extension::call_tool(
                            ctx.app_state,
                            ctx.services,
                            &name,
                            args.clone(),
                            ctx.worktree_path,
                            Some(ctx.task_id),
                            Some(ctx.role_name),
                            ctx.mcp_registry,
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
            let text = render_tool_result(&stash, &id, &name, &value);
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
    let mcp_server_name = ctx
        .mcp_registry
        .as_ref()
        .and_then(|r| r.server_for_tool(&name).map(str::to_string));
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
    let stash = Arc::clone(&ctx.output_stash);
    let retry_safe = is_tool_retry_safe(ctx.tool_metadata, &name);
    let req = ToolDispatchRequest {
        idx,
        id,
        name,
        args,
        tool_span,
        stash,
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
        let serial_remaining = indexed_tool_calls.len() - safe_remaining;
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

    let mut indexed_results: Vec<(usize, ContentBlock)> =
        Vec::with_capacity(indexed_tool_calls.len() + streaming_results.len());
    indexed_results.extend(streaming_results);

    for batch in &batches {
        match batch {
            ToolBatch::Parallel(indices) => {
                for chunk in indices.chunks(MAX_TOOL_CONCURRENCY) {
                    let futures: Vec<_> = chunk
                        .iter()
                        .map(|&idx| make_tool_future(idx, turn_tool_calls[idx].clone(), ctx))
                        .collect();
                    let results = futures::future::join_all(futures).await;
                    indexed_results.extend(results);
                }
            }
            ToolBatch::Serial(idx) => {
                let result = make_tool_future(*idx, turn_tool_calls[*idx].clone(), ctx).await;
                indexed_results.push(result);
            }
        }
    }

    indexed_results.sort_by_key(|(idx, _)| *idx);
    indexed_results
        .into_iter()
        .map(|(_, block)| block)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_stash::{MAX_TOOL_RESULT_CHARS, handle_stash_tool};
    use std::sync::atomic::{AtomicU32, Ordering};

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

    /// A blocking tool that runs longer than several heartbeat intervals must
    /// keep emitting `touch_activity` so the coordinator's zombie reaper sees a
    /// live worker. With a 30s cadence, a 95s tool beats at t=30/60/90 → 3.
    #[tokio::test(start_paused = true)]
    async fn heartbeat_fires_while_a_long_tool_runs() {
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

    /// A tool that completes before the first cadence must never heartbeat
    /// (the immediate first interval tick is consumed up front).
    #[tokio::test(start_paused = true)]
    async fn heartbeat_does_not_fire_for_a_fast_tool() {
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

    /// The transformation the worker MCP success branch applies to a tool result
    /// before handing it back to the model. Mirrors the `Ok(value)` arm of the
    /// MCP branch in [`dispatch_single_tool`] so the test exercises the exact
    /// `render_tool_result` chokepoint the fix routes MCP results through.
    fn mcp_branch_render(
        stash: &Mutex<OutputStash>,
        id: &str,
        name: &str,
        value: &serde_json::Value,
    ) -> String {
        render_tool_result(stash, id, name, value)
    }

    #[test]
    fn oversized_mcp_result_is_stashed_and_truncated() {
        let stash = Mutex::new(OutputStash::new());
        // A 12MB-class MCP payload, well over the clamp — the exact case G6 guards.
        let big = "z".repeat(MAX_TOOL_RESULT_CHARS * 4);
        let value = serde_json::Value::String(big.clone());
        let id = "mcp__server__huge-1";

        let text = mcp_branch_render(&stash, id, "mcp__server__huge", &value);

        // Bounded inline content + the output_view/grep navigation hint, exactly
        // like native tools.
        assert!(text.len() < big.len(), "inline text must be truncated");
        assert!(text.len() <= MAX_TOOL_RESULT_CHARS + 512);
        assert!(text.contains("Full output stashed"));
        assert!(text.contains(&format!("output_view(tool_use_id=\"{id}\")")));
        assert!(text.contains(&format!("output_grep(tool_use_id=\"{id}\"")));

        // The full payload is recoverable from the stash.
        let viewed = handle_stash_tool(
            &stash,
            "output_view",
            Some(
                &serde_json::json!({ "tool_use_id": id })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .unwrap();
        assert!(viewed.contains("zzz"));
    }

    #[test]
    fn small_mcp_result_passes_through_unchanged() {
        let stash = Mutex::new(OutputStash::new());
        let value = serde_json::json!({ "ok": true, "answer": "hello" });
        let id = "mcp__server__small-1";

        let text = mcp_branch_render(&stash, id, "mcp__server__small", &value);

        // Small results are returned verbatim (pretty JSON) with no stash hint…
        assert!(text.contains("\"answer\""));
        assert!(text.contains("hello"));
        assert!(!text.contains("Full output stashed"));
        // …and nothing was stashed.
        assert!(stash.lock().unwrap().view(id, 0, 10).is_err());
    }
}
