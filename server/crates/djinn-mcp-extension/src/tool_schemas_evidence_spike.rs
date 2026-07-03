//! Evidence-spike read-only schema profile.
//!
//! This module defines the tool surface for evidence spikes: Architect-routed,
//! read-only investigation tasks that may read code, search, inspect tasks/epics
//! /proposals/memory, and finalize via the existing `submit_work` terminal path.
//!
//! Mutation tools (`write`, `edit`, `apply_patch`, `shell`, task/epic mutation,
//! memory write/edit/move, etc.) are intentionally excluded.

use crate::finalize_tools;
use crate::shared_schemas::{self, ToolSafetyAnnotations};
use crate::tool_defs::{
    tool_ci_job_log, tool_code_graph, tool_code_search, tool_github_search, tool_lsp,
    tool_output_grep, tool_output_view, tool_read, tool_skill_read,
};
use rmcp::model::Tool as RmcpTool;

fn serialize_tool(tool: RmcpTool, annotations: ToolSafetyAnnotations) -> serde_json::Value {
    shared_schemas::serialize_tool_schema(tool, annotations)
}

/// Read-only evidence-spike tool schema profile.
///
/// Use this to provide the tool surface for a linked evidence-spike task. It is
/// built additively from existing tool schema constructors; it does not narrow or
/// replace the normal Architect schema.
pub fn tool_schemas_evidence_spike() -> Vec<serde_json::Value> {
    let mut tool_values = Vec::new();

    // Representative read/search/analysis tools.
    tool_values.push(serialize_tool(tool_read(), ToolSafetyAnnotations::read_only()));
    tool_values.push(serialize_tool(
        tool_code_search(),
        ToolSafetyAnnotations::open_world_read_only(),
    ));
    tool_values.push(serialize_tool(
        tool_code_graph(),
        ToolSafetyAnnotations::open_world_read_only(),
    ));
    tool_values.push(serialize_tool(
        tool_github_search(),
        ToolSafetyAnnotations::open_world_read_only(),
    ));
    tool_values.push(serialize_tool(
        tool_output_view(),
        ToolSafetyAnnotations::read_only(),
    ));
    tool_values.push(serialize_tool(
        tool_output_grep(),
        ToolSafetyAnnotations::read_only(),
    ));
    tool_values.push(serialize_tool(tool_lsp(), ToolSafetyAnnotations::read_only()));
    tool_values.push(serialize_tool(tool_skill_read(), ToolSafetyAnnotations::read_only()));
    tool_values.push(serialize_tool(tool_ci_job_log(), ToolSafetyAnnotations::read_only()));

    // Read-only task/epic inspection tools.
    tool_values.push(serialize_tool(
        shared_schemas::tool_task_show(),
        ToolSafetyAnnotations::read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_task_list(),
        ToolSafetyAnnotations::read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_task_activity_list(),
        ToolSafetyAnnotations::read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_epic_show(),
        ToolSafetyAnnotations::read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_epic_tasks(),
        ToolSafetyAnnotations::read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_epic_blockers_list(),
        ToolSafetyAnnotations::read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_epic_blocked_list(),
        ToolSafetyAnnotations::read_only(),
    ));

    // Read-only proposal/debate inspection tools.
    tool_values.push(serialize_tool(
        shared_schemas::tool_proposal_show(),
        ToolSafetyAnnotations::read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_proposal_debate_list(),
        ToolSafetyAnnotations::read_only(),
    ));

    // Read-only memory/health inspection tools.
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_read(),
        ToolSafetyAnnotations::read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_search(),
        ToolSafetyAnnotations::read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_list(),
        ToolSafetyAnnotations::read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_build_context(),
        ToolSafetyAnnotations::read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_health(),
        ToolSafetyAnnotations::read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_orphans(),
        ToolSafetyAnnotations::read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_broken_links(),
        ToolSafetyAnnotations::read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_extracted_audit(),
        ToolSafetyAnnotations::read_only(),
    ));

    // Terminal completion via the existing Architect finalize tool.
    tool_values.push(serialize_tool(
        finalize_tools::tool_submit_work(),
        ToolSafetyAnnotations::mutation(),
    ));

    tool_values
}
