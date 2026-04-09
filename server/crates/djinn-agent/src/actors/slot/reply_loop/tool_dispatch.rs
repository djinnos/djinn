use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::extension;
use crate::message::ContentBlock;
use crate::output_stash::OutputStash;
use crate::provider::telemetry;

/// Maximum number of concurrent-safe tools that can execute in parallel within
/// a single batch (ADR-048 §1A).
pub(super) const MAX_TOOL_CONCURRENCY: usize = 8;

/// Maximum characters per tool result to prevent context overflow.
/// ~30k chars = 7.5k tokens — enough for diagnosis, safe with multiple calls.
const MAX_TOOL_RESULT_CHARS: usize = 30_000;

pub(super) fn tool_concurrency_safety(tools: &[serde_json::Value]) -> HashMap<String, bool> {
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
            let concurrent_safe = tool
                .get("concurrent_safe")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            Some((name.to_string(), concurrent_safe))
        })
        .collect()
}

pub(super) fn is_tool_concurrent_safe(tool_metadata: &HashMap<String, bool>, name: &str) -> bool {
    tool_metadata.get(name).copied().unwrap_or(false)
}

/// ADR-048 side queries are not a separate protocol primitive.
///
/// In the reply loop architecture they are modeled as ordinary tool calls whose
/// schema marks them `concurrent_safe=true`, meaning the lookup is read-only and
/// can be started opportunistically during streaming without blocking other turn
/// assembly. Their results still flow back through the normal `tool_result`
/// message on the next user turn so provider tool-call pairing remains valid.
pub(super) fn is_side_query_tool(tool_metadata: &HashMap<String, bool>, name: &str) -> bool {
    is_tool_concurrent_safe(tool_metadata, name)
}

/// Extract browsable content for the output stash.
///
/// For shell results, the LLM wants to browse raw stdout/stderr — not the
/// `{"ok":true,"stdout":"..."}` JSON envelope.  For other tools the
/// pretty-printed JSON is already useful, so we return `None` to let the
/// caller fall back to the default.
pub(super) fn extract_stash_content(tool_name: &str, value: &serde_json::Value) -> Option<String> {
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

pub(super) struct ToolDispatchContext<'a> {
    pub app_state: &'a crate::context::AgentContext,
    pub task_id: &'a str,
    pub worktree_path: &'a std::path::Path,
    pub role_name: &'a str,
    pub mcp_registry: Option<&'a crate::mcp_client::McpToolRegistry>,
    pub output_stash: Arc<Mutex<OutputStash>>,
    pub otel_session: Option<&'a telemetry::SessionSpan>,
}

pub(super) enum ToolBatch {
    Parallel(Vec<usize>),
    Serial(usize),
}

pub(super) fn build_tool_batches<'a>(
    turn_tool_calls: &'a [ContentBlock],
    streaming_dispatched: &HashSet<usize>,
    tool_metadata: &HashMap<String, bool>,
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

#[allow(clippy::too_many_arguments)]
pub(super) async fn dispatch_single_tool<'a>(
    idx: usize,
    id: String,
    name: String,
    _input_json: serde_json::Value,
    args: Option<serde_json::Map<String, serde_json::Value>>,
    tool_span: Option<crate::provider::telemetry::ToolSpan>,
    stash: Arc<Mutex<OutputStash>>,
    app_state: &'a crate::context::AgentContext,
    task_id: &'a str,
    worktree_path: &'a std::path::Path,
    role_name: &'a str,
    mcp_registry: Option<&'a crate::mcp_client::McpToolRegistry>,
) -> (usize, ContentBlock) {
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

    if let Some(registry) = mcp_registry
        && registry.has_tool(&name)
    {
        tracing::debug!(task_id = %task_id, tool = %name, "ReplyLoop: dispatching to MCP server");
        let mcp_result = registry.call_tool(&name, args.clone()).await;
        let (content, is_error) = match mcp_result {
            Ok(value) => {
                let text = if value.is_string() {
                    value.as_str().unwrap_or("").to_string()
                } else {
                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
                };
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

    let mut result = extension::call_tool(
        app_state,
        &name,
        args.clone(),
        worktree_path,
        Some(task_id),
        Some(role_name),
        mcp_registry,
    )
    .await;
    {
        let mut retries = 0u32;
        while retries < 5 {
            match &result {
                Err(e) if e.contains("database is locked") => {
                    retries += 1;
                    let backoff = std::time::Duration::from_millis(100 * (1 << retries.min(4)));
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
                        Some(task_id),
                        Some(role_name),
                        mcp_registry,
                    )
                    .await;
                }
                _ => break,
            }
        }
    }
    let (content, is_error) = match result {
        Ok(value) => {
            let mut text = if value.is_string() {
                value.as_str().unwrap_or("").to_string()
            } else {
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
            };
            if text.len() > MAX_TOOL_RESULT_CHARS {
                let stash_text =
                    extract_stash_content(&name, &value).unwrap_or_else(|| text.clone());
                stash
                    .lock()
                    .unwrap()
                    .insert(id.clone(), name.clone(), stash_text);
                let full_bytes = text.len();
                text = crate::truncate::smart_truncate(&text, MAX_TOOL_RESULT_CHARS);
                text.push_str(&format!(
                    "\n\n[Full output stashed ({full_bytes} bytes). Use output_view(tool_use_id=\"{id}\") to paginate or output_grep(tool_use_id=\"{id}\", pattern=\"...\") to search.]"
                ));
            }
            if let Some(ts) = &tool_span {
                ts.record_output(&text, false);
            }
            (vec![ContentBlock::Text { text }], false)
        }
        Err(err) => {
            tracing::warn!(task_id = %task_id, tool = %name, error = %err, "ReplyLoop: tool call returned error");
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
    let tool_span = ctx.otel_session.map(|session| {
        let ts = telemetry::ToolSpan::start(session.context(), &name, &id);
        ts.record_input(&input_json.to_string());
        ts
    });
    let stash = Arc::clone(&ctx.output_stash);
    dispatch_single_tool(
        idx,
        id,
        name,
        input_json,
        args,
        tool_span,
        stash,
        ctx.app_state,
        ctx.task_id,
        ctx.worktree_path,
        ctx.role_name,
        ctx.mcp_registry,
    )
}

pub(super) async fn collect_tool_results(
    turn_tool_calls: &[ContentBlock],
    streaming_results: Vec<(usize, ContentBlock)>,
    streaming_dispatched: &HashSet<usize>,
    tool_metadata: &HashMap<String, bool>,
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
