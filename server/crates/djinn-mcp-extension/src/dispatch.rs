//! Central tool dispatch with tool-group helpers.
//!
//! The [`dispatch_tool_call`] function is the primary entry point for
//! tool dispatch through the `djinn-mcp-extension` crate.  It resolves
//! the project context, checks allowed schemas, and routes to the
//! appropriate tool-group helper.
//!
//! Tools that require djinn-agent internals (workspace, task_merge,
//! coordinator, skills_manifest) return [`DispatchResult::Unhandled`]
//! so the calling façade can handle them with its concrete context.

use std::path::Path;

use serde::Serialize;

use crate::compatibility::{
    CompatibilityTrap, NormalizationResult, PRODUCTION_REGISTRY, PreparedToolCall,
    ServerReleaseVersion, normalize_call,
};
use crate::context::ExtensionContext;
use crate::handlers::{code_intel, memory_agent, task_admin, task_epic};
use crate::helpers::*;
use crate::types::*;

/// Result of attempting to dispatch a tool call through the extension.
pub enum DispatchResult {
    /// The extension handled the tool call.
    Handled(djinn_core::tool_call::ToolCallOutcome),
    /// The extension does not handle this tool. The caller (djinn-agent façade)
    /// should handle it with its concrete context.
    Unhandled(PreparedToolCall),
}

fn handled(
    result: Result<serde_json::Value, String>,
    warnings: Vec<djinn_core::tool_call::CompatibilityMetadata>,
) -> DispatchResult {
    match djinn_core::tool_call::ToolCallOutcome::from_result(result) {
        djinn_core::tool_call::ToolCallOutcome::Success { value, .. } => {
            DispatchResult::Handled(djinn_core::tool_call::ToolCallOutcome::Success {
                value,
                warnings,
            })
        }
        failure => DispatchResult::Handled(failure),
    }
}

/// Main dispatch entry point.
///
/// Parses the incoming tool call, resolves the project context, and routes
/// to the appropriate tool-group handler. Returns `Unhandled` for tools
/// that require djinn-agent internals.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_tool_call<T>(
    ctx: &dyn ExtensionContext,
    services: &dyn djinn_supervisor::SupervisorServices,
    tool_call: &T,
    worktree_path: &Path,
    allowed_schemas: Option<&[serde_json::Value]>,
    session_task_id: Option<&str>,
    session_role: Option<&str>,
) -> DispatchResult
where
    T: Serialize,
{
    let current_release = ServerReleaseVersion {
        major: 0,
        minor: 1,
        patch: 0,
    };
    dispatch_tool_call_with_compatibility(
        ctx,
        services,
        tool_call,
        worktree_path,
        allowed_schemas,
        session_task_id,
        session_role,
        PRODUCTION_REGISTRY,
        &current_release,
    )
    .await
}

/// Dispatch a call using an explicitly supplied compatibility registry.
///
/// Production callers use [`dispatch_tool_call`], which supplies the
/// server-owned production registry. This seam lets integration tests verify
/// normalization through agent-local fallback without advertising stale
/// surfaces in the production tool inventory.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_tool_call_with_compatibility<T>(
    ctx: &dyn ExtensionContext,
    services: &dyn djinn_supervisor::SupervisorServices,
    tool_call: &T,
    worktree_path: &Path,
    allowed_schemas: Option<&[serde_json::Value]>,
    session_task_id: Option<&str>,
    session_role: Option<&str>,
    registry: &[CompatibilityTrap],
    current_release: &ServerReleaseVersion,
) -> DispatchResult
where
    T: Serialize,
{
    let call: IncomingToolCall = match serde_json::to_value(tool_call)
        .map_err(|e| e.to_string())
        .and_then(|v| from_value(v).map_err(|e| format!("invalid frontend tool payload: {e}")))
    {
        Ok(c) => c,
        Err(e) => return handled(Err(e), Vec::new()),
    };

    let prepared = match normalize_call(registry, current_release, &call.name, call.arguments) {
        NormalizationResult::Prepared(prepared) => prepared,
        NormalizationResult::Failure(failure) => {
            return DispatchResult::Handled(djinn_core::tool_call::ToolCallOutcome::Failure(
                failure,
            ));
        }
    };

    // Allowed schemas gate.
    if let Some(schemas) = allowed_schemas
        && !is_tool_allowed_for_schemas(schemas, &prepared.name)
    {
        return handled(
            Err(format!(
                "tool `{}` is not in the allowed schema list",
                prepared.name
            )),
            prepared.compatibility_warnings,
        );
    }

    // Resolve project context.
    let normalized_call = IncomingToolCall {
        name: prepared.name.clone(),
        arguments: prepared.arguments.clone(),
    };
    let project = resolve_project(ctx, worktree_path, &normalized_call).await;
    let project_id = project.as_ref().map(|p| p.id.clone());
    let project_ref = project
        .as_ref()
        .map(|p| p.slug())
        .unwrap_or_else(|| worktree_path.display().to_string());
    let worktree_project_path = worktree_path.display().to_string();

    // Route to tool-group helpers.
    let name = prepared.name.as_str();
    let args = &prepared.arguments;

    // ── Task query/mutation tools ──────────────────────────────────────
    if let Some(result) = dispatch_task_query_tools(
        ctx,
        name,
        args,
        project_id.as_deref(),
        &worktree_project_path,
        session_role,
    )
    .await
    {
        return handled(result, prepared.compatibility_warnings);
    }

    // ── Epic tools ─────────────────────────────────────────────────────
    if let Some(result) = dispatch_epic_tools(ctx, name, args, project_id.as_deref()).await {
        return handled(result, prepared.compatibility_warnings);
    }

    // ── Proposal tools ─────────────────────────────────────────────────
    if let Some(result) = dispatch_proposal_tools(ctx, name, args).await {
        return handled(result, prepared.compatibility_warnings);
    }

    // ── Memory tools ───────────────────────────────────────────────────
    if let Some(result) = dispatch_memory_tools(
        ctx,
        name,
        args,
        session_task_id,
        &project_ref,
        worktree_path,
    )
    .await
    {
        return handled(result, prepared.compatibility_warnings);
    }

    // ── Agent tools ────────────────────────────────────────────────────
    if let Some(result) =
        dispatch_agent_tools(ctx, name, args, &project_ref, &worktree_project_path).await
    {
        return handled(result, prepared.compatibility_warnings);
    }

    // ── Task admin tools (that don't need djinn-agent internals) ───────
    if let Some(result) = dispatch_task_admin_tools(ctx, name, args).await {
        return handled(result, prepared.compatibility_warnings);
    }

    // ── Code intelligence tools ────────────────────────────────────────
    if let Some(result) =
        dispatch_code_intel_tools(ctx, name, args, worktree_path, project_id.as_deref()).await
    {
        return handled(result, prepared.compatibility_warnings);
    }

    // ── Supervisor-routed tools ────────────────────────────────────────
    if let Some(result) =
        dispatch_supervisor_tools(services, name, args, session_task_id, project_id.as_deref())
            .await
    {
        return handled(result, prepared.compatibility_warnings);
    }

    // Not handled by extension — caller should handle.
    DispatchResult::Unhandled(prepared)
}

// ── Project resolution ──────────────────────────────────────────────────────

async fn resolve_project(
    ctx: &dyn ExtensionContext,
    worktree_path: &Path,
    call: &IncomingToolCall,
) -> Option<djinn_core::models::Project> {
    let repo = djinn_db::ProjectRepository::new(ctx.db(), ctx.event_bus());
    let mut candidates: Vec<String> = vec![worktree_path.to_string_lossy().into_owned()];
    if let Some(args) = call.arguments.as_ref()
        && let Some(proj) = args.get("project").and_then(|v| v.as_str())
    {
        candidates.push(proj.to_string());
    }
    let mut resolved: Option<djinn_core::models::Project> = None;
    for cand in &candidates {
        // Try direct resolve (UUID or owner/repo slug).
        if let Ok(Some(id)) = repo.resolve(cand).await
            && let Ok(Some(proj)) = repo.get(&id).await
        {
            resolved = Some(proj);
            break;
        }
        // Fall back to reverse-parsing `{root}/{owner}/{repo}` filesystem
        // paths — the same logic AgentContext::require_project_id_for_task_ops
        // uses.
        let segments: Vec<String> = std::path::Path::new(cand)
            .components()
            .rev()
            .take(2)
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        if segments.len() >= 2 {
            let repo_name = &segments[0];
            let owner_name = &segments[1];
            if let Ok(Some(proj)) = repo.get_by_github(owner_name, repo_name).await {
                resolved = Some(proj);
                break;
            }
        }
    }
    if resolved.is_none()
        && let Some(default_id) = ctx.default_project_id()
        && !default_id.is_empty()
        && let Ok(Some(proj)) = repo.get(default_id).await
    {
        resolved = Some(proj);
    }
    resolved
}

// ── Tool-group dispatch helpers ─────────────────────────────────────────────

/// Task query and basic mutation tools.
async fn dispatch_task_query_tools(
    ctx: &dyn ExtensionContext,
    name: &str,
    args: &Option<serde_json::Map<String, serde_json::Value>>,
    project_id: Option<&str>,
    worktree_project_path: &str,
    session_role: Option<&str>,
) -> Option<Result<serde_json::Value, String>> {
    match name {
        "task_list" => Some(task_epic::call_task_list(ctx, args, project_id).await),
        "task_show" => Some(task_epic::call_task_show(ctx, args).await),
        "task_create" => Some(task_epic::call_task_create(ctx, args, worktree_project_path).await),
        "task_update" => Some(task_epic::call_task_update(ctx, args, worktree_project_path).await),
        "task_update_ac" => Some(task_epic::call_task_update_ac(ctx, args).await),
        "task_comment_add" => Some(
            task_epic::call_task_comment_add(ctx, args, session_role, worktree_project_path).await,
        ),
        "task_activity_list" => Some(task_epic::call_task_activity_list(ctx, args).await),
        _ => None,
    }
}

/// Epic tools.
async fn dispatch_epic_tools(
    ctx: &dyn ExtensionContext,
    name: &str,
    args: &Option<serde_json::Map<String, serde_json::Value>>,
    project_id: Option<&str>,
) -> Option<Result<serde_json::Value, String>> {
    match name {
        "epic_show" => Some(task_epic::call_epic_show(ctx, args, project_id).await),
        "epic_create" => Some(task_epic::call_epic_create(ctx, args, project_id).await),
        "epic_update" => Some(task_epic::call_epic_update(ctx, args, project_id).await),
        "epic_tasks" => Some(task_epic::call_epic_tasks(ctx, args, project_id).await),
        "epic_close" => Some(task_epic::call_epic_close(ctx, args, project_id).await),
        "epic_blockers_list" => {
            Some(task_epic::call_epic_blockers_list(ctx, args, project_id).await)
        }
        "epic_blocked_list" => Some(task_epic::call_epic_blocked_list(ctx, args, project_id).await),
        _ => None,
    }
}

/// Proposal tools.
async fn dispatch_proposal_tools(
    ctx: &dyn ExtensionContext,
    name: &str,
    args: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Option<Result<serde_json::Value, String>> {
    match name {
        "proposal_show" => Some(task_epic::call_proposal_show(ctx, args).await),
        "proposal_update" => Some(task_epic::call_proposal_update(ctx, args).await),
        "proposal_block_patch" => Some(task_epic::call_proposal_block_patch(ctx, args).await),
        "get_block_catalog" => Some(task_epic::call_get_block_catalog(ctx, args).await),
        "proposal_blocks" => Some(task_epic::call_proposal_blocks(ctx, args).await),
        "proposal_debate_append" => Some(task_epic::call_proposal_debate_append(ctx, args).await),
        "proposal_debate_list" => Some(task_epic::call_proposal_debate_list(ctx, args).await),
        "proposal_debate_resolve" => Some(task_epic::call_proposal_debate_resolve(ctx, args).await),
        "proposal_complete" => Some(task_epic::call_proposal_complete(ctx, args).await),
        "proposal_ac_set" => Some(task_epic::call_proposal_ac_set(ctx, args).await),
        "proposal_ac_amend" => Some(task_epic::call_proposal_ac_amend(ctx, args).await),
        "proposal_reconcile_obsolete_epic" => {
            Some(task_epic::call_proposal_reconcile_obsolete_epic(ctx, args).await)
        }
        _ => None,
    }
}

/// Memory tools.
async fn dispatch_memory_tools(
    ctx: &dyn ExtensionContext,
    name: &str,
    args: &Option<serde_json::Map<String, serde_json::Value>>,
    session_task_id: Option<&str>,
    project_ref: &str,
    worktree_path: &Path,
) -> Option<Result<serde_json::Value, String>> {
    match name {
        "memory_read" => Some(memory_agent::call_memory_read(ctx, args, project_ref).await),
        "memory_search" => {
            Some(memory_agent::call_memory_search(ctx, args, session_task_id, project_ref).await)
        }
        "memory_list" => Some(memory_agent::call_memory_list(ctx, args, project_ref).await),
        "memory_recall_trace" => {
            Some(memory_agent::call_memory_recall_trace(ctx, args, project_ref).await)
        }
        "memory_retrieval_outcomes_report" => {
            Some(memory_agent::call_memory_retrieval_outcomes_report(ctx, args, project_ref).await)
        }
        "memory_build_context" => Some(
            memory_agent::call_memory_build_context(ctx, args, session_task_id, project_ref).await,
        ),
        "memory_write" => {
            Some(memory_agent::call_memory_write(ctx, args, project_ref, worktree_path).await)
        }
        "memory_edit" => {
            Some(memory_agent::call_memory_edit(ctx, args, project_ref, worktree_path).await)
        }
        "memory_move" => Some(memory_agent::call_memory_move(ctx, args, project_ref).await),
        "memory_health" => Some(memory_agent::call_memory_health(ctx, args, project_ref).await),
        "memory_extracted_audit" => {
            Some(memory_agent::call_memory_extracted_audit(ctx, args, project_ref).await)
        }
        "memory_broken_links" => {
            Some(memory_agent::call_memory_broken_links(ctx, args, project_ref).await)
        }
        "memory_orphans" => Some(memory_agent::call_memory_orphans(ctx, args, project_ref).await),
        _ => None,
    }
}

/// Agent tools.
async fn dispatch_agent_tools(
    ctx: &dyn ExtensionContext,
    name: &str,
    args: &Option<serde_json::Map<String, serde_json::Value>>,
    project_ref: &str,
    worktree_project_path: &str,
) -> Option<Result<serde_json::Value, String>> {
    match name {
        "agent_metrics" => Some(memory_agent::call_agent_metrics(ctx, args, project_ref).await),
        "agent_create" => {
            Some(memory_agent::call_agent_create(ctx, args, worktree_project_path).await)
        }
        _ => None,
    }
}

/// Task admin tools that don't need djinn-agent internals.
async fn dispatch_task_admin_tools(
    ctx: &dyn ExtensionContext,
    name: &str,
    args: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Option<Result<serde_json::Value, String>> {
    match name {
        "task_archive_activity" => Some(task_admin::call_task_archive_activity(ctx, args).await),
        "task_reset_counters" => Some(task_admin::call_task_reset_counters(ctx, args).await),
        "task_blocked_list" => Some(task_admin::call_task_blocked_list(ctx, args).await),
        _ => None,
    }
}

/// Code intelligence tools (code_graph, lsp).
async fn dispatch_code_intel_tools(
    ctx: &dyn ExtensionContext,
    name: &str,
    args: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
    _project_id: Option<&str>,
) -> Option<Result<serde_json::Value, String>> {
    match name {
        "lsp" => {
            let root = ctx.working_root_for(worktree_path);
            Some(code_intel::call_lsp(ctx, args, &root).await)
        }
        // code_graph requires complex bridge operations and remains as
        // Unhandled for the djinn-agent façade to handle with its concrete
        // AgentContext.
        _ => None,
    }
}

/// Tools routed through SupervisorServices.
async fn dispatch_supervisor_tools(
    services: &dyn djinn_supervisor::SupervisorServices,
    name: &str,
    args: &Option<serde_json::Map<String, serde_json::Value>>,
    session_task_id: Option<&str>,
    project_id: Option<&str>,
) -> Option<Result<serde_json::Value, String>> {
    match name {
        "ci_job_log" => Some(
            services
                .tool_ci_job_log(
                    session_task_id.map(str::to_string),
                    args.clone().unwrap_or_default(),
                )
                .await,
        ),
        "github_search" => Some(
            services
                .tool_github_search(
                    project_id.map(str::to_string),
                    args.clone().unwrap_or_default(),
                )
                .await,
        ),
        _ => None,
    }
}
