use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::extension;
use crate::output_stash::{OutputStash, handle_stash_tool, is_stash_tool, render_tool_result};
use djinn_provider::message::ContentBlock;
use djinn_provider::provider::telemetry;

/// Maximum number of concurrent-safe tools that can execute in parallel within
/// a single batch (ADR-048 §1A).
pub(super) const MAX_TOOL_CONCURRENCY: usize = 8;

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
    tool_span: Option<djinn_provider::provider::telemetry::ToolSpan>,
    stash: Arc<Mutex<OutputStash>>,
    app_state: &'a crate::context::AgentContext,
    services: &'a dyn djinn_supervisor::SupervisorServices,
    task_id: &'a str,
    worktree_path: &'a std::path::Path,
    role_name: &'a str,
    mcp_registry: Option<&'a crate::mcp_client::McpToolRegistry>,
) -> (usize, ContentBlock) {
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

    if let Some(registry) = mcp_registry
        && registry.has_tool(&name)
    {
        tracing::debug!(task_id = %task_id, tool = %name, "ReplyLoop: dispatching to MCP server");
        let mcp_result = registry.call_tool(&name, args.clone()).await;
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

    let mut result = extension::call_tool(
        app_state,
        services,
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
                        services,
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
            let text = render_tool_result(&stash, &id, &name, &value);
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
    dispatch_single_tool(
        idx,
        id,
        name,
        input_json,
        args,
        tool_span,
        stash,
        ctx.app_state,
        ctx.services,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_stash::{MAX_TOOL_RESULT_CHARS, handle_stash_tool};

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
