//! Reply-loop facade: adapts agent host services into djinn-slot SlotContext.

use std::sync::{Arc, Mutex};

use djinn_provider::message::Conversation;

use crate::context::AgentContext;
use crate::output_parser::ParsedAgentOutput;
use crate::output_stash::{
    DurableOutputDetails, OutputStash, externalize_rendered_tool_result, handle_stash_tool,
    is_stash_tool, render_tool_result,
};

pub(crate) mod error_handling {
    pub(crate) use djinn_slot::reply_loop::error_handling::*;
}

#[cfg(test)]
mod resource_tests;

pub(crate) mod loop_guard {
    pub(crate) use djinn_slot::reply_loop::loop_guard::*;
}

pub(crate) struct ReplyLoopContext<'a> {
    pub provider: &'a dyn djinn_provider::provider::LlmProvider,
    pub tools: &'a [serde_json::Value],
    pub task_id: &'a str,
    pub task_short_id: &'a str,
    pub session_id: &'a str,
    pub project_path: &'a str,
    pub worktree_path: &'a std::path::Path,
    pub role_name: &'a str,
    pub finalize_tool_names: &'a [&'a str],
    pub context_window: i64,
    pub model_id: &'a str,
    pub cancel: &'a tokio_util::sync::CancellationToken,
    pub global_cancel: &'a tokio_util::sync::CancellationToken,
    pub app_state: &'a AgentContext,
    pub services: &'a dyn djinn_supervisor::SupervisorServices,
    pub mcp_registry: Option<&'a crate::mcp_client::McpToolRegistry>,
    pub active_skill_names: &'a [String],
    pub active_mcp_server_names: &'a [String],
    pub max_turns_override: Option<u32>,
    /// When `true`, the session runs under the evidence-spike read-only
    /// profile.  The dispatcher enforces allowed_schemas at dispatch time
    /// as defense-in-depth beyond the stage-time schema restriction.
    pub is_evidence_spike: bool,
}

/// Format MCP resource contents into deterministic inline text for tool results.
///
/// Text resources render with URI, MIME type, and text content. Binary/blob
/// resources produce a descriptive omission message. Text resources exceeding
/// [`crate::mcp_client::MAX_MCP_RESOURCE_TEXT_BYTES`] are omitted with size
/// context.
pub(crate) fn format_resource_contents(contents: &[rmcp::model::ResourceContents]) -> String {
    use crate::mcp_client::MAX_MCP_RESOURCE_TEXT_BYTES;

    let mut out = String::new();
    for content in contents {
        match content {
            rmcp::model::ResourceContents::TextResourceContents {
                uri,
                mime_type,
                text,
                ..
            } => {
                let rendered_len = text.len();
                if rendered_len > MAX_MCP_RESOURCE_TEXT_BYTES {
                    out.push_str(&format!(
                        "Resource: {uri}\nMIME: {}\n[resource omitted: {rendered_len} bytes exceeds {} MiB limit]",
                        mime_type.as_deref().unwrap_or("text/plain"),
                        MAX_MCP_RESOURCE_TEXT_BYTES / (1024 * 1024)
                    ));
                } else {
                    out.push_str(&format!("Resource: {uri}\n"));
                    if let Some(mime) = mime_type {
                        out.push_str(&format!("MIME: {mime}\n"));
                    }
                    out.push_str(text);
                }
            }
            rmcp::model::ResourceContents::BlobResourceContents {
                uri,
                mime_type,
                blob,
                ..
            } => {
                let size = blob.len();
                out.push_str(&format!(
                    "Resource: {uri}\nMIME: {}\n[binary resource omitted: {size} bytes]",
                    mime_type.as_deref().unwrap_or("application/octet-stream")
                ));
            }
        }
    }
    out
}

struct AgentToolDispatcher {
    app_state: AgentContext,
    services: &'static dyn djinn_supervisor::SupervisorServices,
    mcp_registry: Option<&'static crate::mcp_client::McpToolRegistry>,
    output_stash: Mutex<OutputStash>,
    /// When set, only tools whose names appear in these schemas may be
    /// dispatched.  Used by the evidence-spike runtime profile to enforce
    /// read-only/fail-closed access at dispatch time (defense-in-depth
    /// beyond the stage-time schema restriction).
    allowed_schemas: Option<Vec<serde_json::Value>>,
    /// Agent-private cancellation pair (session + global) retained on the
    /// concrete dispatcher so cancellation can flow to the shell runner
    /// without changing the shared `SlotToolDispatcher` trait.
    cancel: crate::extension::ToolCancellation,
}

impl AgentToolDispatcher {
    fn new(
        app_state: &AgentContext,
        services: &dyn djinn_supervisor::SupervisorServices,
        mcp_registry: Option<&crate::mcp_client::McpToolRegistry>,
        session_id: &str,
        allowed_schemas: Option<Vec<serde_json::Value>>,
        cancel: tokio_util::sync::CancellationToken,
        global_cancel: tokio_util::sync::CancellationToken,
    ) -> Self {
        // SAFETY: the dispatcher is created immediately before calling the
        // canonical reply loop and dropped when that call returns. The canonical
        // loop awaits every tool-dispatch future before returning, so these
        // erased references cannot outlive their source borrows.
        let services_static = unsafe {
            std::mem::transmute::<
                &dyn djinn_supervisor::SupervisorServices,
                &'static dyn djinn_supervisor::SupervisorServices,
            >(services)
        };
        let registry_static = mcp_registry.map(|registry| unsafe {
            std::mem::transmute::<
                &crate::mcp_client::McpToolRegistry,
                &'static crate::mcp_client::McpToolRegistry,
            >(registry)
        });
        Self {
            app_state: app_state.clone(),
            services: services_static,
            mcp_registry: registry_static,
            output_stash: Mutex::new(OutputStash::with_session_id(session_id)),
            allowed_schemas,
            cancel: crate::extension::ToolCancellation::new(cancel, global_cancel),
        }
    }
}

/// Persist compaction candidates under the dispatcher's trusted stash session.
/// Factoring this out lets focused tests cover write-before-pointer ordering.
fn persist_tool_results_before_compaction(
    stash: &mut OutputStash,
    results: &[djinn_slot::host::PreCompactionToolResult],
) -> Result<Vec<djinn_compaction::ToolOutputPointer>, String> {
    let existing = stash.list_durable_outputs()?;
    for result in results {
        if existing.iter().any(|metadata| metadata.tool_use_id == result.tool_use_id) {
            continue;
        }
        let chars = result.content.chars().count();
        stash.insert_with_metadata(
            result.tool_use_id.clone(), result.tool_name.clone(), result.content.clone(),
            DurableOutputDetails { turn: result.turn, result_kind: "tool_result".into(), original_chars: chars, stored_chars: chars, completeness: "complete".into() },
        )?;
    }
    let durable = stash.list_durable_outputs()?;
    results.iter().map(|result| {
        let metadata = durable.iter().find(|metadata| metadata.tool_use_id == result.tool_use_id)
            .ok_or_else(|| format!("durable output missing after write: {}", result.tool_use_id))?;
        Ok(djinn_compaction::ToolOutputPointer { tool_use_id: metadata.tool_use_id.clone(), turn: metadata.turn, original_chars: metadata.original_chars, result_kind: metadata.result_kind.clone() })
    }).collect()
}

impl djinn_slot::host::SlotToolDispatcher for AgentToolDispatcher {
    fn is_stash_tool(&self, tool_name: &str) -> bool {
        is_stash_tool(tool_name)
    }
    fn handle_stash_call(
        &self,
        tool_name: &str,
        arguments: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<String, String> {
        handle_stash_tool(&self.output_stash, tool_name, arguments)
    }
    fn render_result(
        &self,
        tool_use_id: &str,
        tool_name: &str,
        value: &serde_json::Value,
    ) -> String {
        render_tool_result(&self.output_stash, tool_use_id, tool_name, value)
    }
    fn externalize_rendered_result(
        &self,
        tool_use_id: &str,
        tool_name: &str,
        rendered: &str,
        preview_chars: usize,
    ) -> String {
        externalize_rendered_tool_result(
            &self.output_stash,
            tool_use_id,
            tool_name,
            rendered,
            preview_chars,
        )
    }
    fn persist_tool_results_before_compaction(
        &self,
        results: &[djinn_slot::host::PreCompactionToolResult],
    ) -> Result<Vec<djinn_compaction::ToolOutputPointer>, String> {
        let mut stash = self
            .output_stash
            .lock()
            .map_err(|_| "output stash mutex poisoned")?;
        persist_tool_results_before_compaction(&mut stash, results)
    }
    fn dispatch_extension_tool<'a>(
        &'a self,
        tool_name: &'a str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
        worktree_path: &'a std::path::Path,
        task_id: &'a str,
        role_name: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = djinn_core::tool_call::ToolCallOutcome> + Send + 'a>,
    > {
        Box::pin(crate::extension::call_tool(
            &self.app_state,
            self.services,
            tool_name,
            arguments,
            worktree_path,
            Some(task_id),
            Some(role_name),
            self.mcp_registry,
            self.allowed_schemas.as_deref(),
            &self.cancel,
        ))
    }
    fn is_mcp_tool(&self, tool_name: &str) -> bool {
        self.mcp_registry
            .map(|registry| registry.has_tool(tool_name))
            .unwrap_or(false)
    }
    fn dispatch_mcp_tool<'a>(
        &'a self,
        tool_name: &'a str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send + 'a>,
    > {
        match self.mcp_registry {
            Some(registry) => Box::pin(registry.call_tool(tool_name, arguments)),
            None => Box::pin(async move { Err(format!("MCP tool `{tool_name}` not found")) }),
        }
    }
    fn mcp_server_for_tool(&self, tool_name: &str) -> Option<String> {
        self.mcp_registry
            .and_then(|registry| registry.server_for_tool(tool_name))
    }
    fn is_resource_tool(&self, tool_name: &str) -> bool {
        tool_name == "list_mcp_resources" || tool_name == "read_mcp_resource"
    }
    fn dispatch_resource_tool<'a>(
        &'a self,
        tool_name: &'a str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
    {
        let registry = self.mcp_registry;
        let tool = tool_name.to_string();
        Box::pin(async move {
            let Some(registry) = registry else {
                return Err("MCP registry not available".to_string());
            };
            let args = arguments.unwrap_or_default();
            match tool.as_str() {
                "list_mcp_resources" => {
                    let server = args
                        .get("server")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let resources = registry.list_resources(server.as_deref()).await?;
                    if resources.is_empty() {
                        return Ok("No resources found.".to_string());
                    }
                    let mut out = String::new();
                    for (server_name, resource) in &resources {
                        out.push_str(&format!(
                            "- server: {server_name}\n  uri: {}\n  name: {}\n",
                            resource.uri, resource.name
                        ));
                        if let Some(desc) = &resource.description {
                            out.push_str(&format!("  description: {desc}\n"));
                        }
                        if let Some(mime) = &resource.mime_type {
                            out.push_str(&format!("  mime_type: {mime}\n"));
                        }
                    }
                    Ok(out)
                }
                "read_mcp_resource" => {
                    let server = args
                        .get("server")
                        .and_then(|v| v.as_str())
                        .ok_or("missing required argument `server` (string)")?;
                    let uri = args
                        .get("uri")
                        .and_then(|v| v.as_str())
                        .ok_or("missing required argument `uri` (string)")?;
                    let contents = registry.read_resource(server, uri).await?;
                    Ok(format_resource_contents(&contents))
                }
                _ => Err(format!("unknown resource tool: {tool}")),
            }
        })
    }
    fn clear_stash(&self) {
        self.output_stash
            .lock()
            .expect("output stash mutex")
            .clear();
    }
}

pub(crate) async fn run_reply_loop(
    ctx: ReplyLoopContext<'_>,
    conversation: &mut Conversation,
    is_resumed_session: bool,
) -> (anyhow::Result<()>, ParsedAgentOutput, i64, i64, i64, i64) {
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
        services,
        mcp_registry,
        active_skill_names,
        active_mcp_server_names,
        max_turns_override,
        is_evidence_spike,
    } = ctx;
    let mut slot_ctx = super::host_callbacks::agent_to_reply_loop_slot_context(app_state, services);
    // Use the compaction critical section already threaded into SlotContext by
    // the slot actor; do not create a separate local instance so the reply loop
    // and actor observe the same active/release transitions.
    let compaction_cs = &slot_ctx.compaction_cs;
    // Evidence-spike sessions get the restricted tool set as allowed_schemas
    // for defense-in-depth dispatch-time enforcement.  For normal sessions
    // this is None (no dispatch-time gate — the full role schema applies).
    let allowed_for_dispatch = if is_evidence_spike {
        Some(tools.to_vec())
    } else {
        None
    };
    slot_ctx.tool_dispatcher = Some(Arc::new(AgentToolDispatcher::new(
        app_state,
        services,
        mcp_registry,
        session_id,
        allowed_for_dispatch,
        cancel.clone(),
        global_cancel.clone(),
    )));
    let (result, output, tokens_in, tokens_out, cache_read, cache_write) =
        djinn_slot::reply_loop::run_reply_loop(
            djinn_slot::reply_loop::ReplyLoopContext {
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
                ctx: &slot_ctx,
                active_skill_names,
                active_mcp_server_names,
                max_turns_override,
                compaction_cs,
            },
            conversation,
            is_resumed_session,
        )
        .await;
    let mut agent_output = ParsedAgentOutput::new(false);
    agent_output.runtime_error = output.runtime_error;
    agent_output.reviewer_feedback = output.reviewer_feedback;
    agent_output.finalize_payload = output.finalize_payload;
    agent_output.finalize_tool_name = output.finalize_tool_name;
    agent_output.completion_intent = output.completion_intent;
    agent_output.budget_wind_down_summary = output.budget_wind_down_summary;
    agent_output.budget_wind_down_details = output.budget_wind_down_details;
    (
        result,
        agent_output,
        tokens_in,
        tokens_out,
        cache_read,
        cache_write,
    )
}

// Agent-side reply-loop tests duplicated canonical behavior and are intentionally
// retired with this facade cut-over. Canonical coverage lives in
// `server/crates/djinn-slot/src/reply_loop/tests.rs` and
// `server/crates/djinn-slot/src/reply_loop_tests.rs`; compatibility is compile-
// checked through this adapter and `supervisor_impl::stage`.

#[cfg(test)]
mod compaction_persistence_tests {
    use super::*;
    use crate::output_stash::DurableOutputDetails;
    use djinn_slot::host::PreCompactionToolResult;

    fn stash(name: &str) -> OutputStash {
        let root = std::path::PathBuf::from("/var/tmp/djinn-compaction-persistence-tests").join(name);
        let _ = std::fs::remove_dir_all(&root);
        OutputStash::with_session_id_and_durable_root("trusted-session", root)
    }

    fn result(id: &str, content: &str, turn: u64) -> PreCompactionToolResult {
        PreCompactionToolResult { tool_use_id: id.into(), tool_name: "shell".into(), content: content.into(), turn }
    }

    #[test]
    fn persistence_reuses_existing_metadata_and_is_idempotent() {
        let mut stash = stash("reuse");
        stash.insert_with_metadata("large".into(), "shell".into(), "prefix".into(), DurableOutputDetails {
            turn: 9, result_kind: "shell_stdout".into(), original_chars: 99, stored_chars: 6, completeness: "partial-spill".into(),
        }).unwrap();
        let results = vec![result("large", "inline replacement must not overwrite", 1), result("small", "small inline output", 2)];
        let pointers = persist_tool_results_before_compaction(&mut stash, &results).unwrap();
        assert_eq!(pointers[0].turn, 9);
        assert_eq!(pointers[0].original_chars, 99);
        assert_eq!(pointers[0].result_kind, "shell_stdout");
        assert_eq!(pointers[1].turn, 2);
        assert_eq!(stash.list_durable_outputs().unwrap().len(), 2);
        persist_tool_results_before_compaction(&mut stash, &results).unwrap();
        let listed = stash.list_durable_outputs().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed.iter().find(|item| item.tool_use_id == "large").unwrap().completeness, "partial-spill");
        stash.clear();
        assert!(stash.view("small", 0, 10).unwrap().contains("small inline output"));
    }

    #[test]
    fn injected_write_failure_emits_no_pointer_or_in_memory_entry() {
        let mut stash = stash("failure");
        stash.set_fail_durable_writes_for_test(true);
        let results = vec![result("missing", "must remain inline", 4)];
        let error = persist_tool_results_before_compaction(&mut stash, &results).unwrap_err();
        assert!(error.contains("injected durable output write failure"));
        assert!(stash.list_durable_outputs().unwrap().is_empty());
        assert!(stash.view("missing", 0, 10).is_err());
    }
}
