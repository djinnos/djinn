//! Memory and agent tool handlers.
//!
//! These handlers delegate to the `djinn-control-plane` shared MCP server
//! using [`crate::ExtensionContext`] for state access.

use std::path::Path;

use djinn_control_plane::tools::agent_tools::{
    AgentCreateParams as SharedAgentCreateParams, AgentMetricsParams as SharedAgentMetricsParams,
    create_agent as shared_create_agent, metrics_for_agents as shared_metrics_for_agents,
};
use djinn_control_plane::tools::memory_tools::{
    BrokenLinksParams as SharedMemoryBrokenLinksParams,
    BuildContextParams as SharedMemoryBuildContextParams, EditParams as SharedMemoryEditParams,
    ExtractedAuditParams as SharedMemoryExtractedAuditParams,
    HealthParams as SharedMemoryHealthParams, ListParams as SharedMemoryListParams,
    OrphansParams as SharedMemoryOrphansParams, ReadParams as SharedMemoryReadParams,
    RecallTraceParams as SharedMemoryRecallTraceParams,
    RetrievalOutcomesReportParams as SharedMemoryRetrievalOutcomesReportParams,
    SearchParams as SharedMemorySearchParams, WriteParams as SharedMemoryWriteParams,
};
use djinn_db::AgentRepository;

use crate::context::ExtensionContext;
use crate::helpers::*;
use crate::types::*;

pub(crate) async fn call_memory_read(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: MemoryReadParams = parse_args(arguments)?;
    let project_path = project_path.to_owned();
    let server = djinn_control_plane::server::DjinnMcpServer::new(ctx.mcp_state());
    Ok(serde_json::to_value(
        djinn_control_plane::tools::memory_tools::ops::memory_read(
            &server,
            SharedMemoryReadParams {
                project: project_path,
                identifier: p.identifier,
            },
        )
        .await,
    )
    .unwrap_or_else(|_| serde_json::json!({ "error": "failed to serialize memory_read response" })))
}

pub(crate) async fn call_memory_search(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    session_task_id: Option<&str>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: MemorySearchParams = parse_args(arguments)?;
    let project_path = project_path.to_owned();
    let task_id = p.task_id.or_else(|| session_task_id.map(ToOwned::to_owned));
    let server = djinn_control_plane::server::DjinnMcpServer::new(ctx.mcp_state());
    Ok(serde_json::to_value(
        djinn_control_plane::tools::memory_tools::ops::memory_search(
            &server,
            SharedMemorySearchParams {
                project: project_path,
                query: p.query,
                folder: p.folder,
                note_type: p.note_type,
                limit: p.limit,
                entity_types: None,
                edge_kinds: None,
            },
            task_id.as_deref(),
        )
        .await,
    )
    .unwrap_or_else(
        |_| serde_json::json!({ "error": "failed to serialize memory_search response" }),
    ))
}

pub(crate) async fn call_memory_list(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: MemoryListParams = parse_args(arguments)?;
    let project_path = project_path.to_owned();
    let server = djinn_control_plane::server::DjinnMcpServer::new(ctx.mcp_state());
    Ok(serde_json::to_value(
        djinn_control_plane::tools::memory_tools::ops::memory_list(
            &server,
            SharedMemoryListParams {
                project: project_path,
                folder: p.folder,
                note_type: p.note_type,
                status: p.status,
                depth: p.depth,
            },
        )
        .await,
    )
    .unwrap_or_else(|_| serde_json::json!({ "error": "failed to serialize memory_list response" })))
}

/// Delegate retrieval-trace inspection to the control-plane operation while
/// forcing the resolved current project. The extension owns no trace-query
/// logic and intentionally ignores caller-supplied project selectors here.
pub(crate) async fn call_memory_recall_trace(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let mut raw = arguments.clone().unwrap_or_default();
    raw.insert("project".to_string(), serde_json::json!(project_path));
    raw.remove("project_id");
    let params: SharedMemoryRecallTraceParams =
        serde_json::from_value(serde_json::Value::Object(raw))
            .map_err(|error| format!("invalid arguments: {error}"))?;
    let server = djinn_control_plane::server::DjinnMcpServer::new(ctx.mcp_state());
    Ok(serde_json::to_value(
        djinn_control_plane::tools::memory_tools::ops::memory_recall_trace(&server, params).await,
    )
    .unwrap_or_else(
        |_| serde_json::json!({ "error": "failed to serialize memory_recall_trace response" }),
    ))
}

pub(crate) async fn call_memory_retrieval_outcomes_report(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: MemoryRetrievalOutcomesReportParams =
        parse_args_stripping(arguments, &["project", "project_id"])?;
    let server = djinn_control_plane::server::DjinnMcpServer::new(ctx.mcp_state());
    Ok(serde_json::to_value(
        djinn_control_plane::tools::memory_tools::ops::memory_retrieval_outcomes_report(
            &server,
            SharedMemoryRetrievalOutcomesReportParams {
                project: Some(project_path.to_owned()),
                project_id: None,
                start: p.start,
                end: p.end,
                timezone: p.timezone,
            },
        )
        .await,
    )
    .unwrap())
}

pub(crate) async fn call_memory_build_context(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    session_task_id: Option<&str>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: MemoryBuildContextParams = parse_args(arguments)?;
    let project_path = project_path.to_owned();
    let task_id = p.task_id.or_else(|| session_task_id.map(ToOwned::to_owned));
    let url = p.url.unwrap_or_else(|| "/*".to_string());
    let max_related = p.max_related.or(p.limit);
    let server = djinn_control_plane::server::DjinnMcpServer::new(ctx.mcp_state());
    Ok(serde_json::to_value(
        djinn_control_plane::tools::memory_tools::ops::memory_build_context(
            &server,
            SharedMemoryBuildContextParams {
                project: project_path,
                url,
                depth: None,
                max_related,
                budget: p.budget,
                task_id: task_id.clone(),
                min_confidence: p.min_confidence,
                edge_kinds: None,
            },
            task_id.as_deref(),
        )
        .await,
    )
    .unwrap_or_else(
        |_| serde_json::json!({ "error": "failed to serialize memory_build_context response" }),
    ))
}

pub(crate) async fn call_memory_health(
    ctx: &dyn ExtensionContext,
    _arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let server = djinn_control_plane::server::DjinnMcpServer::new(ctx.mcp_state());
    Ok(serde_json::to_value(
        djinn_control_plane::tools::memory_tools::ops::memory_health(
            &server,
            SharedMemoryHealthParams {
                project: Some(project_path.to_owned()),
            },
        )
        .await,
    )
    .unwrap_or_else(
        |_| serde_json::json!({ "error": "failed to serialize memory_health response" }),
    ))
}

pub(crate) async fn call_memory_extracted_audit(
    ctx: &dyn ExtensionContext,
    _arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let server = djinn_control_plane::server::DjinnMcpServer::new(ctx.mcp_state());
    Ok(serde_json::to_value(
        djinn_control_plane::tools::memory_tools::ops::memory_extracted_audit(
            &server,
            SharedMemoryExtractedAuditParams {
                project: project_path.to_owned(),
            },
        )
        .await,
    )
    .unwrap_or_else(
        |_| serde_json::json!({ "error": "failed to serialize memory_extracted_audit response" }),
    ))
}

pub(crate) async fn call_memory_write(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
    _worktree_root: &Path,
) -> Result<serde_json::Value, String> {
    let p: MemoryWriteParams = parse_args_stripping(arguments, &["project"])?;
    let project_path = project_path.to_owned();
    let server = djinn_control_plane::server::DjinnMcpServer::new(ctx.mcp_state());
    let result = server
        .memory_write(rmcp::handler::server::wrapper::Parameters(
            SharedMemoryWriteParams {
                reason: p.reason,
                project: project_path,
                title: p.title,
                content: p.content,
                note_type: p.note_type,
                status: p.status,
                tags: p.tags,
                scope_paths: p.scope_paths,
                retrieval_anchor: p.retrieval_anchor,
            },
        ))
        .await;
    Ok(serde_json::to_value(result.0).unwrap_or_else(
        |_| serde_json::json!({ "error": "failed to serialize memory_write response" }),
    ))
}

pub(crate) async fn call_memory_edit(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
    _worktree_root: &Path,
) -> Result<serde_json::Value, String> {
    let p: MemoryEditParams = parse_args_stripping(arguments, &["project"])?;
    let project_path = project_path.to_owned();
    let server = djinn_control_plane::server::DjinnMcpServer::new(ctx.mcp_state());
    let result = server
        .memory_edit(rmcp::handler::server::wrapper::Parameters(
            SharedMemoryEditParams {
                reason: p.reason,
                project: project_path,
                identifier: p.identifier,
                operation: p.operation,
                content: p.content,
                find_text: p.find_text,
                section: p.section,
                note_type: p.note_type,
                retrieval_anchor: p.retrieval_anchor,
            },
        ))
        .await;
    Ok(serde_json::to_value(result.0).unwrap_or_else(
        |_| serde_json::json!({ "error": "failed to serialize memory_edit response" }),
    ))
}

pub(crate) async fn call_memory_move(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: MemoryMoveParams = parse_args(arguments)?;
    let server = djinn_control_plane::server::DjinnMcpServer::new(ctx.mcp_state());
    let result = server
        .memory_move(rmcp::handler::server::wrapper::Parameters(
            djinn_control_plane::tools::memory_tools::MoveParams {
                project: project_path.to_owned(),
                identifier: p.identifier,
                note_type: p.note_type,
                title: p.title,
            },
        ))
        .await;
    Ok(serde_json::to_value(result.0).unwrap_or_else(
        |_| serde_json::json!({ "error": "failed to serialize memory_move response" }),
    ))
}

pub(crate) async fn call_memory_broken_links(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: MemoryBrokenLinksLocalParams = parse_args(arguments)?;
    let server = djinn_control_plane::server::DjinnMcpServer::new(ctx.mcp_state());
    Ok(serde_json::to_value(
        djinn_control_plane::tools::memory_tools::ops::memory_broken_links(
            &server,
            SharedMemoryBrokenLinksParams {
                project: project_path.to_owned(),
                folder: non_empty(p.folder),
            },
        )
        .await,
    )
    .unwrap_or_else(
        |_| serde_json::json!({ "error": "failed to serialize memory_broken_links response" }),
    ))
}

pub(crate) async fn call_memory_orphans(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: MemoryOrphansLocalParams = parse_args(arguments)?;
    let server = djinn_control_plane::server::DjinnMcpServer::new(ctx.mcp_state());
    Ok(serde_json::to_value(
        djinn_control_plane::tools::memory_tools::ops::memory_orphans(
            &server,
            SharedMemoryOrphansParams {
                project: project_path.to_owned(),
                folder: non_empty(p.folder),
            },
        )
        .await,
    )
    .unwrap_or_else(
        |_| serde_json::json!({ "error": "failed to serialize memory_orphans response" }),
    ))
}

pub(crate) async fn call_agent_metrics(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let project_id = project_id_for_path(ctx, project_path).await?;

    let raw = arguments.clone().unwrap_or_default();
    let params = SharedAgentMetricsParams {
        project: project_path.to_owned(),
        agent_id: raw
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned),
        window_days: raw.get("window_days").and_then(|v| v.as_i64()),
    };

    let response = shared_metrics_for_agents(
        &AgentRepository::new(ctx.db(), ctx.event_bus()),
        &project_id,
        params,
    )
    .await;

    let roles = response
        .agents
        .unwrap_or_default()
        .into_iter()
        .map(|entry| {
            serde_json::json!({
                "agent_id": entry.agent_id,
                "agent_name": entry.agent_name,
                "base_role": entry.base_role,
                "success_rate": entry.success_rate,
                "avg_reopens": entry.avg_reopens,
                "completed_task_count": entry.completed_task_count,
                "avg_tokens": entry.avg_tokens,
                "avg_time_seconds": entry.avg_time_seconds,
                "extraction_quality": entry.extraction_quality,
            })
        })
        .collect::<Vec<_>>();

    Ok(serde_json::json!({
        "roles": roles,
        "window_days": response.window_days,
    }))
}

pub(crate) async fn call_agent_create(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let project_id = project_id_for_path(ctx, project_path).await?;

    let mut raw = arguments.clone().unwrap_or_default();
    raw.entry("project")
        .or_insert_with(|| serde_json::json!(project_path));
    let params: SharedAgentCreateParams = serde_json::from_value(serde_json::Value::Object(raw))
        .map_err(|e| format!("invalid arguments: {e}"))?;

    let response = shared_create_agent(
        &AgentRepository::new(ctx.db(), ctx.event_bus()),
        &project_id,
        params,
    )
    .await;

    match response.agent {
        Some(agent) => Ok(serde_json::json!({
            "agent_id": agent.id,
            "agent_name": agent.name,
            "base_role": agent.base_role,
            "created": true,
        })),
        None => Err(response
            .error
            .unwrap_or_else(|| "failed to create agent".to_string())),
    }
}
