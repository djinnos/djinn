use super::*;

pub(super) async fn call_memory_read(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: MemoryReadParams = parse_args(arguments)?;
    let project_path = project_path.to_owned();
    let server = djinn_control_plane::server::DjinnMcpServer::new(state.to_mcp_state());
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

pub(super) async fn call_memory_search(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    session_task_id: Option<&str>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: MemorySearchParams = parse_args(arguments)?;
    let project_path = project_path.to_owned();
    let task_id = p.task_id.or_else(|| session_task_id.map(ToOwned::to_owned));
    let server = djinn_control_plane::server::DjinnMcpServer::new(state.to_mcp_state());
    Ok(serde_json::to_value(
        djinn_control_plane::tools::memory_tools::ops::memory_search(
            &server,
            SharedMemorySearchParams {
                project: project_path,
                query: p.query,
                folder: p.folder,
                note_type: p.note_type,
                limit: p.limit,
            },
            task_id.as_deref(),
        )
        .await,
    )
    .unwrap_or_else(
        |_| serde_json::json!({ "error": "failed to serialize memory_search response" }),
    ))
}

pub(super) async fn call_memory_list(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: MemoryListParams = parse_args(arguments)?;
    let project_path = project_path.to_owned();
    let server = djinn_control_plane::server::DjinnMcpServer::new(state.to_mcp_state());
    Ok(serde_json::to_value(
        djinn_control_plane::tools::memory_tools::ops::memory_list(
            &server,
            SharedMemoryListParams {
                project: project_path,
                folder: p.folder,
                note_type: p.note_type,
                depth: p.depth,
            },
        )
        .await,
    )
    .unwrap_or_else(|_| serde_json::json!({ "error": "failed to serialize memory_list response" })))
}

pub(super) async fn call_memory_build_context(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    session_task_id: Option<&str>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: MemoryBuildContextParams = parse_args(arguments)?;
    let project_path = project_path.to_owned();
    let task_id = p.task_id.or_else(|| session_task_id.map(ToOwned::to_owned));
    let url = p.url.unwrap_or_else(|| "/*".to_string());
    let max_related = p.max_related.or(p.limit);
    let server = djinn_control_plane::server::DjinnMcpServer::new(state.to_mcp_state());
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
            },
            task_id.as_deref(),
        )
        .await,
    )
    .unwrap_or_else(
        |_| serde_json::json!({ "error": "failed to serialize memory_build_context response" }),
    ))
}

pub(super) async fn call_memory_health(
    state: &AgentContext,
    _arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let server = djinn_control_plane::server::DjinnMcpServer::new(state.to_mcp_state());
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

pub(super) async fn call_memory_extracted_audit(
    state: &AgentContext,
    _arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let server = djinn_control_plane::server::DjinnMcpServer::new(state.to_mcp_state());
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

pub(super) async fn call_memory_write(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
    worktree_root: &Path,
) -> Result<serde_json::Value, String> {
    let p: MemoryWriteParams = parse_args(arguments)?;
    let project_path = project_path.to_owned();
    let server = djinn_control_plane::server::DjinnMcpServer::new(state.to_mcp_state());
    let worktree_root = Some(worktree_root.to_path_buf());
    let result = server
        .memory_write_with_worktree(
            rmcp::handler::server::wrapper::Parameters(SharedMemoryWriteParams {
                project: project_path,
                title: p.title,
                content: p.content,
                note_type: p.note_type,
                status: p.status,
                tags: p.tags,
                scope_paths: p.scope_paths,
            }),
            worktree_root,
        )
        .await;
    Ok(serde_json::to_value(result.0).unwrap_or_else(
        |_| serde_json::json!({ "error": "failed to serialize memory_write response" }),
    ))
}

pub(super) async fn call_memory_edit(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
    worktree_root: &Path,
) -> Result<serde_json::Value, String> {
    let p: MemoryEditParams = parse_args(arguments)?;
    let project_path = project_path.to_owned();
    let server = djinn_control_plane::server::DjinnMcpServer::new(state.to_mcp_state());
    let worktree_root = Some(worktree_root.to_path_buf());
    let result = server
        .memory_edit_with_worktree(
            rmcp::handler::server::wrapper::Parameters(SharedMemoryEditParams {
                project: project_path,
                identifier: p.identifier,
                operation: p.operation,
                content: p.content,
                find_text: p.find_text,
                section: p.section,
                note_type: p.note_type,
            }),
            worktree_root,
        )
        .await;
    Ok(serde_json::to_value(result.0).unwrap_or_else(
        |_| serde_json::json!({ "error": "failed to serialize memory_edit response" }),
    ))
}

pub(super) async fn call_memory_move(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: MemoryMoveParams = parse_args(arguments)?;
    let server = djinn_control_plane::server::DjinnMcpServer::new(state.to_mcp_state());
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

pub(super) async fn call_memory_broken_links(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: MemoryBrokenLinksLocalParams = parse_args(arguments)?;
    let server = djinn_control_plane::server::DjinnMcpServer::new(state.to_mcp_state());
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

pub(super) async fn call_memory_orphans(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: MemoryOrphansLocalParams = parse_args(arguments)?;
    let server = djinn_control_plane::server::DjinnMcpServer::new(state.to_mcp_state());
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

pub(super) async fn call_agent_metrics(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let project_id = project_id_for_path(state, project_path).await?;

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
        &AgentRepository::new(state.db.clone(), state.event_bus.clone()),
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
                "learned_prompt": entry.learned_prompt,
                "success_rate": entry.success_rate,
                "avg_reopens": entry.avg_reopens,
                "verification_pass_rate": entry.verification_pass_rate,
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

pub(super) async fn call_agent_amend_prompt(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: AgentAmendPromptParams = parse_args(arguments)?;
    let project_id = project_id_for_path(state, project_path).await?;

    let repo = AgentRepository::new(state.db.clone(), state.event_bus.clone());

    // Resolve role by UUID or name.
    let role = {
        let by_id = repo.get(&p.agent_id).await.map_err(|e| e.to_string())?;
        match by_id {
            Some(r) if r.project_id == project_id => Some(r),
            _ => repo
                .get_by_name_for_project(&project_id, &p.agent_id)
                .await
                .map_err(|e| e.to_string())?,
        }
    };

    let role = role.ok_or_else(|| format!("agent not found: {}", p.agent_id))?;

    // Only allow amending specialist roles. Prevents patching high-level orchestration roles.
    if matches!(role.base_role.as_str(), "architect" | "lead" | "planner") {
        return Err(format!(
            "cannot amend learned_prompt for base_role '{}'; only specialist roles (worker, reviewer) are eligible",
            role.base_role
        ));
    }

    let updated = repo
        .append_learned_prompt(&role.id, &p.amendment, p.metrics_snapshot.as_deref())
        .await
        .map_err(|e| e.to_string())?;

    Ok(serde_json::json!({
        "agent_id": updated.id,
        "agent_name": updated.name,
        "learned_prompt": updated.learned_prompt,
        "updated_at": updated.updated_at,
        "amendment_appended": true,
    }))
}

pub(super) async fn call_agent_create(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let project_id = project_id_for_path(state, project_path).await?;

    let mut raw = arguments.clone().unwrap_or_default();
    // Inject project so the shared params struct deserialises.
    raw.entry("project")
        .or_insert_with(|| serde_json::json!(project_path));
    let params: SharedAgentCreateParams = serde_json::from_value(serde_json::Value::Object(raw))
        .map_err(|e| format!("invalid arguments: {e}"))?;

    let response = shared_create_agent(
        &AgentRepository::new(state.db.clone(), state.event_bus.clone()),
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
