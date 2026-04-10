use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};

use crate::context::AgentContext;
use djinn_core::models::Task;
use djinn_db::ProjectRepository;

/// Supported djinn-agent → djinn-mcp integration seam for shared task mutation ops.
///
/// External callers should bridge their existing runtime context through
/// [`AgentContext::to_mcp_state`] and resolve the project id with
/// [`AgentContext::require_project_id_for_task_ops`] using the session/worktree root
/// rather than a crate-local source path. This preserves MCP-side project resolution
/// semantics and lets shared mutation helpers return the same public response shapes
/// and JSON error envelopes that agent dispatch tests assert.
pub(super) async fn project_id_for_path(
    state: &AgentContext,
    project_path: &str,
) -> Result<String, String> {
    state
        .require_project_id_for_task_ops(project_path)
        .await
        .map_err(|error| error.error)
}

pub(super) fn acceptance_criterion_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => map
            .get("criterion")
            .and_then(|criterion| criterion.as_str())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| value.to_string()),
        serde_json::Value::String(text) => text.clone(),
        _ => value.to_string(),
    }
}

pub(super) fn task_response_to_value(
    response: djinn_mcp::tools::task_tools::TaskResponse,
) -> serde_json::Value {
    serde_json::to_value(response)
        .unwrap_or_else(|_| serde_json::json!({ "error": "failed to serialize task response" }))
}

pub(super) fn activity_entry_to_value(
    response: djinn_mcp::tools::task_tools::ActivityEntryResponse,
) -> serde_json::Value {
    serde_json::to_value(response)
        .unwrap_or_else(|_| serde_json::json!({ "error": "failed to serialize activity response" }))
}

pub(super) fn error_or_to_value<T>(
    response: djinn_mcp::tools::task_tools::ErrorOr<T>,
    ok: impl FnOnce(T) -> serde_json::Value,
) -> Result<serde_json::Value, String> {
    Ok(match response {
        djinn_mcp::tools::task_tools::ErrorOr::Ok(value) => ok(value),
        djinn_mcp::tools::task_tools::ErrorOr::Error(error) => {
            serde_json::json!({ "error": error.error })
        }
    })
}

/// Find the largest byte index <= `idx` that is a valid UTF-8 char boundary.
#[cfg(test)]
pub(super) fn floor_char_boundary(s: &str, idx: usize) -> usize {
    crate::truncate::floor_char_boundary(s, idx)
}

/// Normalize `Some("")` → `None`. OpenAI models often send empty strings
/// for optional parameters instead of omitting them, which breaks SQL filters.
pub(super) fn non_empty(opt: Option<String>) -> Option<String> {
    opt.filter(|s| !s.is_empty())
}

pub(super) async fn resolve_project_id_for_agent_tools(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<String, String> {
    let project_id = arguments
        .as_ref()
        .and_then(|map| map.get("project"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(|project| async move {
            let repo = ProjectRepository::new(state.db.clone(), state.event_bus.clone());
            repo.resolve(project)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("project not found: {project}"))
        });

    if let Some(project_id) = project_id {
        return project_id.await;
    }

    let repo = ProjectRepository::new(state.db.clone(), state.event_bus.clone());
    let projects = repo.list().await.map_err(|e| e.to_string())?;
    match projects.as_slice() {
        [project] => Ok(project.id.clone()),
        [] => Err("no project configured for agent tool call".to_string()),
        _ => Err("project is required when multiple projects are configured".to_string()),
    }
}

pub(super) fn resolve_path(raw: &str, base: &std::path::Path) -> PathBuf {
    use std::path::Component;
    let p = Path::new(raw);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub(super) fn is_tool_allowed_for_schemas(schemas: &[serde_json::Value], name: &str) -> bool {
    schemas
        .iter()
        .any(|schema| schema.get("name").and_then(|n| n.as_str()) == Some(name))
}

#[cfg(test)]
pub(super) fn is_tool_allowed_for_agent(agent_type: crate::AgentType, name: &str) -> bool {
    let schemas = agent_type.tool_schemas();
    is_tool_allowed_for_schemas(&schemas, name)
}

pub(super) fn ensure_path_within_worktree(path: &Path, worktree_path: &Path) -> Result<(), String> {
    let canonical_base = std::fs::canonicalize(worktree_path)
        .map_err(|e| format!("failed to canonicalize worktree path: {e}"))?;

    let candidate = if path.exists() {
        std::fs::canonicalize(path).map_err(|e| format!("failed to canonicalize path: {e}"))?
    } else {
        let parent = path.parent().unwrap_or(path);
        let canonical_parent = std::fs::canonicalize(parent)
            .map_err(|e| format!("failed to canonicalize parent path: {e}"))?;
        canonical_parent.join(path.file_name().unwrap_or_default())
    };

    if !candidate.starts_with(&canonical_base) {
        return Err(format!(
            "path is outside worktree: {}. Use the shell tool to read files outside your worktree (e.g. cat {})",
            path.display(),
            path.display(),
        ));
    }

    Ok(())
}

pub(super) fn parse_args<T>(
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
{
    let args = arguments.clone().unwrap_or_default();
    serde_json::from_value(serde_json::Value::Object(args)).map_err(|e| e.to_string())
}

/// Merge incoming AC objects with existing stored criteria.
///
/// If an incoming object has a `criterion` field it is used as-is.  Otherwise
/// the `criterion` text is copied from the existing array at the same index so
/// that reviewer payloads like `[{"met": true}]` don't erase the text.
pub(super) fn merge_acceptance_criteria(
    existing_json: &str,
    incoming: &[serde_json::Value],
) -> String {
    let existing: Vec<serde_json::Value> = serde_json::from_str(existing_json).unwrap_or_default();

    let merged: Vec<serde_json::Value> = incoming
        .iter()
        .enumerate()
        .map(|(i, inc)| {
            let mut obj = inc.as_object().cloned().unwrap_or_default();
            // If the incoming object is missing `criterion`, copy from existing.
            if !obj.contains_key("criterion")
                && let Some(existing_criterion) = existing
                    .get(i)
                    .and_then(|e| e.get("criterion"))
                    .and_then(|v| v.as_str())
            {
                obj.insert(
                    "criterion".to_string(),
                    serde_json::Value::String(existing_criterion.to_string()),
                );
            }
            serde_json::Value::Object(obj)
        })
        .collect();

    serde_json::to_string(&merged).unwrap_or_else(|_| "[]".to_string())
}

pub(super) fn task_to_value(t: &Task) -> serde_json::Value {
    let labels = djinn_core::models::parse_json_array(&t.labels);
    let ac: serde_json::Value =
        serde_json::from_str(&t.acceptance_criteria).unwrap_or(serde_json::json!([]));
    let memory_refs: serde_json::Value =
        serde_json::from_str(&t.memory_refs).unwrap_or(serde_json::json!([]));

    serde_json::json!({
        "id": t.id,
        "short_id": t.short_id,
        "epic_id": t.epic_id,
        "title": t.title,
        "description": t.description,
        "design": t.design,
        "issue_type": t.issue_type,
        "status": t.status,
        "priority": t.priority,
        "owner": t.owner,
        "labels": labels,
        "memory_refs": memory_refs,
        "acceptance_criteria": ac,
        "reopen_count": t.reopen_count,
        "continuation_count": t.continuation_count,
        "verification_failure_count": t.verification_failure_count,
        "total_reopen_count": t.total_reopen_count,
        "total_verification_failure_count": t.total_verification_failure_count,
        "intervention_count": t.intervention_count,
        "last_intervention_at": t.last_intervention_at,
        "created_at": t.created_at,
        "updated_at": t.updated_at,
        "closed_at": t.closed_at,
        "close_reason": t.close_reason,
        "merge_commit_sha": t.merge_commit_sha,
        "agent_type": t.agent_type,
    })
}

pub(super) fn from_value<T>(value: serde_json::Value) -> Result<T, serde_json::Error>
where
    T: DeserializeOwned,
{
    serde_json::from_value(value)
}

pub(super) fn validate_symbol_only_params(
    operation: &str,
    params: &super::types::LspParams,
) -> Result<(), String> {
    if operation == "symbols" {
        return Ok(());
    }

    let mut unexpected = Vec::new();
    if params.depth.is_some() {
        unexpected.push("depth");
    }
    if params.kind.is_some() {
        unexpected.push("kind");
    }
    if params.name_filter.is_some() {
        unexpected.push("name_filter");
    }

    if unexpected.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} only supported for operation='symbols'",
            unexpected.join(", ")
        ))
    }
}
