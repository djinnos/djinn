//! Shared helper functions for extension handlers.
//!
//! These utilities replace the `helpers` module in `djinn-agent::extension`
//! with versions that operate through [`crate::ExtensionContext`] rather than
//! the concrete `AgentContext`.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};

use djinn_db::ProjectRepository;

use crate::context::ExtensionContext;

/// Resolve a project identifier from a path/slug string using the extension
/// context's database and event bus.
///
/// This replaces `project_id_for_path` from the djinn-agent helpers.
/// Handles UUIDs, `owner/repo` slugs, *and* filesystem paths that end in
/// `{projects_root}/{owner}/{repo}` — the same fallback chain that
/// `AgentContext::require_project_id_for_task_ops` provides.
pub(crate) async fn project_id_for_path(
    ctx: &dyn ExtensionContext,
    project_path: &str,
) -> Result<String, String> {
    let repo = ProjectRepository::new(ctx.db(), ctx.event_bus());

    // 1. Direct resolve (UUID or owner/repo slug).
    if let Some(id) = repo
        .resolve(project_path)
        .await
        .map_err(|e| e.to_string())?
    {
        return Ok(id);
    }

    // 2. Try with trailing path separators stripped.
    let trimmed = project_path
        .trim_end_matches(std::path::MAIN_SEPARATOR)
        .trim_end_matches('/');
    if trimmed != project_path
        && let Some(id) = repo.resolve(trimmed).await.map_err(|e| e.to_string())?
    {
        return Ok(id);
    }

    // 3. Fall back to reverse-parsing the `{root}/{owner}/{repo}` clone
    //    path shape.  The project identity is (github_owner, github_repo);
    //    any raw filesystem path we get here is expected to end in that
    //    two-segment tail.
    let segments: Vec<String> = std::path::Path::new(project_path)
        .components()
        .rev()
        .take(2)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if segments.len() >= 2 {
        // rev().take(2) yields [repo, owner]; flip them.
        let repo_name = &segments[0];
        let owner_name = &segments[1];
        if let Some(project) = repo
            .get_by_github(owner_name, repo_name)
            .await
            .map_err(|e| e.to_string())?
        {
            return Ok(project.id);
        }
    }

    // 4. Fall back to the context's default project id (K8s single-project
    //    workers).
    if let Some(default_id) = ctx.default_project_id().filter(|s| !s.is_empty()) {
        return Ok(default_id.to_string());
    }

    Err(format!("project not found: {project_path}"))
}

pub(crate) fn acceptance_criterion_to_string(value: &serde_json::Value) -> String {
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

pub(crate) fn task_response_to_value(
    response: djinn_control_plane::tools::task_tools::TaskResponse,
) -> serde_json::Value {
    serde_json::to_value(response)
        .unwrap_or_else(|_| serde_json::json!({ "error": "failed to serialize task response" }))
}

pub(crate) fn activity_entry_to_value(
    response: djinn_control_plane::tools::task_tools::ActivityEntryResponse,
) -> serde_json::Value {
    serde_json::to_value(response)
        .unwrap_or_else(|_| serde_json::json!({ "error": "failed to serialize activity response" }))
}

pub(crate) fn error_or_to_value<T>(
    response: djinn_control_plane::tools::task_tools::ErrorOr<T>,
    ok: impl FnOnce(T) -> serde_json::Value,
) -> Result<serde_json::Value, String> {
    Ok(match response {
        djinn_control_plane::tools::task_tools::ErrorOr::Ok(value) => ok(value),
        djinn_control_plane::tools::task_tools::ErrorOr::Error(error) => {
            serde_json::json!({ "error": error.error })
        }
    })
}

/// Normalize `Some("")` → `None`.
pub(crate) fn non_empty(opt: Option<String>) -> Option<String> {
    opt.filter(|s| !s.is_empty())
}

/// Resolve project id for agent tools, using the extension context.
///
/// Replaces the djinn-agent helper with the same name. Tries the explicit
/// `project` argument first, then falls back to `default_project_id`, then
/// to a single-project auto-detect.
pub(crate) async fn resolve_project_id_for_agent_tools(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<String, String> {
    let explicit_project = arguments
        .as_ref()
        .and_then(|map| map.get("project"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty());

    let default_id = ctx.default_project_id().filter(|s| !s.is_empty());

    if let Some(project) = explicit_project {
        let repo = ProjectRepository::new(ctx.db(), ctx.event_bus());
        match repo.resolve(project).await.map_err(|e| e.to_string())? {
            Some(id) => return Ok(id),
            None => {
                // Try filesystem-path fallback (e.g. `{root}/{owner}/{repo}`).
                let segments: Vec<String> = std::path::Path::new(project)
                    .components()
                    .rev()
                    .take(2)
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect();
                if segments.len() >= 2 {
                    let repo_name = &segments[0];
                    let owner_name = &segments[1];
                    if let Some(found) = repo
                        .get_by_github(owner_name, repo_name)
                        .await
                        .map_err(|e| e.to_string())?
                    {
                        return Ok(found.id);
                    }
                }
                if let Some(default_id) = default_id {
                    return Ok(default_id.to_string());
                }
                return Err(format!("project not found: {project}"));
            }
        }
    }

    if let Some(default_id) = default_id {
        return Ok(default_id.to_string());
    }

    let repo = ProjectRepository::new(ctx.db(), ctx.event_bus());
    let projects = repo.list().await.map_err(|e| e.to_string())?;
    match projects.as_slice() {
        [project] => Ok(project.id.clone()),
        [] => Err("no project configured for agent tool call".to_string()),
        _ => Err("project is required when multiple projects are configured".to_string()),
    }
}

pub(crate) fn resolve_path(raw: &str, base: &std::path::Path) -> PathBuf {
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

pub(crate) fn is_tool_allowed_for_schemas(schemas: &[serde_json::Value], name: &str) -> bool {
    schemas
        .iter()
        .any(|schema| schema.get("name").and_then(|n| n.as_str()) == Some(name))
}

pub(crate) fn ensure_path_within_worktree(path: &Path, worktree_path: &Path) -> Result<(), String> {
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

pub(crate) fn parse_args<T>(
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
/// Stored entries can be either `{criterion, met}` objects or bare strings
/// (refinement revisions write bare strings), so both forms are valid
/// fallback sources when an incoming object carries only `met`; incoming
/// bare strings become the criterion text itself.
pub(crate) fn merge_acceptance_criteria(
    existing_json: &str,
    incoming: &[serde_json::Value],
) -> String {
    let existing: Vec<serde_json::Value> = serde_json::from_str(existing_json).unwrap_or_default();

    let merged: Vec<serde_json::Value> = incoming
        .iter()
        .enumerate()
        .map(|(i, inc)| {
            let mut obj = match inc {
                serde_json::Value::String(s) => {
                    let mut m = serde_json::Map::new();
                    m.insert(
                        "criterion".to_string(),
                        serde_json::Value::String(s.clone()),
                    );
                    m
                }
                _ => inc.as_object().cloned().unwrap_or_default(),
            };
            if !obj.contains_key("criterion")
                && let Some(existing_criterion) = existing.get(i).and_then(|e| {
                    e.as_str()
                        .or_else(|| e.get("criterion").and_then(|v| v.as_str()))
                })
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

#[cfg(test)]
mod merge_ac_tests {
    use super::merge_acceptance_criteria;
    use serde_json::json;

    #[test]
    fn met_only_payload_preserves_object_criterion() {
        let merged = merge_acceptance_criteria(
            r#"[{"criterion": "does the thing", "met": false}]"#,
            &[json!({"met": true})],
        );
        assert_eq!(merged, r#"[{"met":true,"criterion":"does the thing"}]"#);
    }

    #[test]
    fn met_only_payload_preserves_bare_string_criterion() {
        let merged = merge_acceptance_criteria(r#"["does the thing"]"#, &[json!({"met": true})]);
        assert_eq!(merged, r#"[{"met":true,"criterion":"does the thing"}]"#);
    }

    #[test]
    fn incoming_bare_string_becomes_criterion_text() {
        let merged = merge_acceptance_criteria(r#"["old text"]"#, &[json!("new text")]);
        assert_eq!(merged, r#"[{"criterion":"new text"}]"#);
    }

    #[test]
    fn incoming_criterion_wins_over_existing() {
        let merged = merge_acceptance_criteria(
            r#"["old text"]"#,
            &[json!({"criterion": "new text", "met": true})],
        );
        assert_eq!(merged, r#"[{"criterion":"new text","met":true}]"#);
    }
}

pub(crate) fn task_to_value(t: &djinn_core::models::Task) -> serde_json::Value {
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
        "total_reopen_count": t.total_reopen_count,
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

pub(crate) fn from_value<T>(value: serde_json::Value) -> Result<T, serde_json::Error>
where
    T: DeserializeOwned,
{
    serde_json::from_value(value)
}

pub(crate) fn validate_symbol_only_params(
    operation: &str,
    params: &crate::types::LspParams,
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
