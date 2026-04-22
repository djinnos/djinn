use super::*;

pub(super) async fn call_task_list(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: TaskListParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());

    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);
    let query = djinn_db::ListQuery {
        project_id: project_id.map(|s| s.to_string()),
        status: non_empty(p.status),
        issue_type: non_empty(p.issue_type),
        priority: p.priority.filter(|&v| v != 0),
        text: non_empty(p.text),
        label: non_empty(p.label),
        parent: non_empty(p.parent),
        sort: non_empty(p.sort).unwrap_or_else(|| "priority".to_string()),
        limit,
        offset,
    };

    let result = repo.list_filtered(query).await.map_err(|e| e.to_string())?;
    let has_more = offset + i64::try_from(result.tasks.len()).unwrap_or(0) < result.total_count;

    Ok(serde_json::json!({
        "tasks": result.tasks.iter().map(task_to_value).collect::<Vec<_>>(),
        "total": result.total_count,
        "limit": limit,
        "offset": offset,
        "has_more": has_more,
    }))
}

pub(super) async fn call_task_show(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskShowParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());
    let session_repo = SessionRepository::new(state.db.clone(), state.event_bus.clone());

    match repo.resolve(&p.id).await {
        Ok(Some(task)) => {
            let mut value = task_to_value(&task);
            if let Some(map) = value.as_object_mut() {
                let session_count = session_repo.count_for_task(&task.id).await.unwrap_or(0);
                let active_session = session_repo.active_for_task(&task.id).await.ok().flatten();
                map.insert(
                    "session_count".to_string(),
                    serde_json::json!(session_count),
                );
                map.insert(
                    "active_session".to_string(),
                    serde_json::json!(active_session),
                );

                // Include recent activity (comments, transitions) so agents
                // can see worker notes and review history.
                // Cap entries and payload sizes to prevent context-window blowup
                // on tasks with many sessions / verbose error logs.
                const MAX_ACTIVITY_ENTRIES: usize = 30;
                const MAX_PAYLOAD_CHARS: usize = 1500;
                let activity = repo.list_activity(&task.id).await.unwrap_or_default();
                let activity_json: Vec<serde_json::Value> = activity
                    .iter()
                    // Skip session_error events — they contain verbose diagnostics
                    // that are not useful for agent decision-making.
                    .filter(|e| e.event_type != "session_error")
                    .take(MAX_ACTIVITY_ENTRIES)
                    .map(|entry| {
                        let mut payload = serde_json::from_str::<serde_json::Value>(&entry.payload)
                            .unwrap_or(serde_json::json!({}));
                        // Truncate large payload string values (e.g. verification output).
                        if let Some(obj) = payload.as_object_mut() {
                            for value in obj.values_mut() {
                                if let Some(s) = value.as_str()
                                    && s.len() > MAX_PAYLOAD_CHARS
                                {
                                    *value = serde_json::json!(crate::truncate::smart_truncate(
                                        s,
                                        MAX_PAYLOAD_CHARS
                                    ));
                                }
                            }
                        }
                        serde_json::json!({
                            "id": entry.id,
                            "actor_role": entry.actor_role,
                            "event_type": entry.event_type,
                            "payload": payload,
                            "created_at": entry.created_at,
                        })
                    })
                    .collect();
                map.insert("activity".to_string(), serde_json::json!(activity_json));
            }
            Ok(value)
        }
        Ok(None) => Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) })),
        Err(e) => Err(e.to_string()),
    }
}

pub(super) async fn call_task_activity_list(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    use djinn_db::ActivityQuery;

    let p: TaskActivityListParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());

    // Resolve short_id to full UUID
    let task_id = match repo.resolve(&p.id).await {
        Ok(Some(task)) => task.id,
        Ok(None) => return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) })),
        Err(e) => return Err(e.to_string()),
    };

    let limit = p.limit.unwrap_or(30).min(50);
    let entries = repo
        .query_activity(ActivityQuery {
            task_id: Some(task_id),
            event_type: p.event_type,
            actor_role: p.actor_role,
            limit,
            ..Default::default()
        })
        .await
        .map_err(|e| e.to_string())?;

    const MAX_PAYLOAD_CHARS: usize = 1500;
    let activity_json: Vec<serde_json::Value> = entries
        .iter()
        .map(|entry| {
            let mut payload = serde_json::from_str::<serde_json::Value>(&entry.payload)
                .unwrap_or(serde_json::json!({}));
            if let Some(obj) = payload.as_object_mut() {
                for value in obj.values_mut() {
                    if let Some(s) = value.as_str()
                        && s.len() > MAX_PAYLOAD_CHARS
                    {
                        *value = serde_json::json!(crate::truncate::smart_truncate(
                            s,
                            MAX_PAYLOAD_CHARS
                        ));
                    }
                }
            }
            serde_json::json!({
                "actor_role": entry.actor_role,
                "event_type": entry.event_type,
                "payload": payload,
                "created_at": entry.created_at,
            })
        })
        .collect();

    Ok(serde_json::json!({ "count": activity_json.len(), "entries": activity_json }))
}

pub(crate) async fn call_epic_show(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    resolved_project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: EpicShowParams = parse_args(arguments)?;
    let project_id = match resolved_project_id {
        Some(id) => id.to_string(),
        None => resolve_project_id_for_agent_tools(state, arguments).await?,
    };
    let repo = EpicRepository::new(state.db.clone(), state.event_bus.clone());
    let response = djinn_control_plane::tools::epic_ops::epic_show(
        &repo,
        &project_id,
        EpicShowRequest {
            project: String::new(),
            id: p.id,
        },
    )
    .await;
    serde_json::to_value(response).map_err(|e| e.to_string())
}

pub(crate) async fn call_epic_update(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    resolved_project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: EpicUpdateParams = parse_args(arguments)?;
    let project_id = match resolved_project_id {
        Some(id) => id.to_string(),
        None => resolve_project_id_for_agent_tools(state, arguments).await?,
    };
    let repo = EpicRepository::new(state.db.clone(), state.event_bus.clone());
    let response = djinn_control_plane::tools::epic_ops::epic_update_with_delta(
        &repo,
        &project_id,
        EpicUpdateDeltaRequest {
            project: String::new(),
            id: p.id,
            title: p.title,
            description: p.description,
            emoji: None,
            color: None,
            owner: None,
            memory_refs_add: p.memory_refs_add,
            memory_refs_remove: p.memory_refs_remove,
            status: p.status,
        },
    )
    .await;
    serde_json::to_value(response).map_err(|e| e.to_string())
}

pub(crate) async fn call_epic_tasks(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    resolved_project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: EpicTasksParams = parse_args(arguments)?;
    let project_id = match resolved_project_id {
        Some(id) => id.to_string(),
        None => resolve_project_id_for_agent_tools(state, arguments).await?,
    };
    let epic_repo = EpicRepository::new(state.db.clone(), state.event_bus.clone());
    let task_repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());
    let response = djinn_control_plane::tools::epic_ops::epic_tasks(
        &epic_repo,
        &task_repo,
        &project_id,
        EpicTasksRequest {
            project: String::new(),
            epic_id: p.id,
            status: None,
            issue_type: None,
            sort: None,
            limit: p.limit,
            offset: p.offset,
        },
    )
    .await;
    let mut value = serde_json::to_value(response).map_err(|e| e.to_string())?;
    if let Some(map) = value.as_object_mut()
        && let Some(total_count) = map.remove("total_count")
    {
        map.insert("total".to_string(), total_count);
    }
    Ok(value)
}

pub(super) async fn call_epic_close(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    resolved_project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: EpicShowParams = parse_args(arguments)?;
    let project_id = match resolved_project_id {
        Some(id) => id.to_string(),
        None => resolve_project_id_for_agent_tools(state, arguments).await?,
    };
    let repo = EpicRepository::new(state.db.clone(), state.event_bus.clone());
    let epic = repo
        .resolve_in_project(&project_id, &p.id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("epic not found: {}", p.id))?;
    if epic.status == "closed" {
        return Err("epic is already closed".to_string());
    }
    let closed = repo.close(&epic.id).await.map_err(|e| e.to_string())?;
    serde_json::to_value(serde_json::json!({
        "epic": {
            "id": closed.id,
            "short_id": closed.short_id,
            "title": closed.title,
            "status": closed.status,
        }
    }))
    .map_err(|e| e.to_string())
}

pub(super) async fn call_task_create(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: TaskCreateParams = parse_args(arguments)?;
    let status = match p.status.as_deref() {
        None => None,
        Some("open") => Some("open"),
        Some(other) => {
            return Err(format!("invalid status: {other:?} (expected open)"));
        }
    };
    let project_id = project_id_for_path(state, project_path).await?;
    let server = djinn_control_plane::server::DjinnMcpServer::new(state.to_mcp_state());
    let Json(response) = shared_create_task(
        &server,
        &project_id,
        SharedCreateTaskRequest {
            title: p.title,
            description: p.description.unwrap_or_default(),
            design: p.design.unwrap_or_default(),
            issue_type: p.issue_type.unwrap_or_else(|| "task".to_string()),
            priority: p.priority.unwrap_or(0),
            owner: p.owner.unwrap_or_default(),
            status: status.map(str::to_string),
            acceptance_criteria: p.acceptance_criteria.map(|criteria| {
                criteria
                    .into_iter()
                    .map(|item| acceptance_criterion_to_string(&item))
                    .collect()
            }),
            labels: Vec::new(),
            memory_refs: p.memory_refs.unwrap_or_default(),
            blocked_by_refs: p.blocked_by.unwrap_or_default(),
            agent_type: p.agent_type,
            epic_ref: Some(p.epic_id),
        },
    )
    .await;

    error_or_to_value(response, task_response_to_value)
}

pub(super) async fn call_task_update(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: TaskUpdateParams = parse_args(arguments)?;
    let project_id = project_id_for_path(state, project_path).await?;
    let server = djinn_control_plane::server::DjinnMcpServer::new(state.to_mcp_state());
    let Json(response) = shared_update_task(
        &server,
        &project_id,
        SharedUpdateTaskRequest {
            id: p.id,
            title: p.title,
            description: p.description,
            design: p.design,
            priority: p.priority,
            owner: p.owner,
            acceptance_criteria: p.acceptance_criteria.map(|criteria| {
                criteria
                    .into_iter()
                    .map(|item| acceptance_criterion_to_string(&item))
                    .collect()
            }),
            labels_add: p.labels_add.unwrap_or_default(),
            labels_remove: p
                .labels_remove
                .unwrap_or_default()
                .into_iter()
                .collect::<HashSet<_>>(),
            memory_refs_add: p.memory_refs_add.unwrap_or_default(),
            memory_refs_remove: p
                .memory_refs_remove
                .unwrap_or_default()
                .into_iter()
                .collect::<HashSet<_>>(),
            blocked_by_add_refs: p.blocked_by_add,
            blocked_by_remove_refs: p.blocked_by_remove,
            agent_type: None,
            epic_ref: None,
        },
    )
    .await;

    error_or_to_value(response, task_response_to_value)
}

pub(super) async fn call_task_update_ac(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskUpdateAcParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());

    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };

    // Merge incoming AC with existing criteria so the `criterion` text is
    // preserved even when the reviewer only sends `{met: bool}` objects.
    let ac_json = merge_acceptance_criteria(&task.acceptance_criteria, &p.acceptance_criteria);

    let updated = repo
        .update(
            &task.id,
            &task.title,
            &task.description,
            &task.design,
            task.priority,
            &task.owner,
            &task.labels,
            &ac_json,
        )
        .await
        .map_err(|e| e.to_string())?;

    Ok(task_to_value(&updated))
}

pub(super) async fn call_request_lead(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    #[derive(serde::Deserialize)]
    struct RequestPmParams {
        id: String,
        reason: String,
        suggested_breakdown: Option<String>,
    }

    let p: RequestPmParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());

    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };

    // Log the Lead request as a structured comment.
    let mut body = format!("[LEAD_REQUEST] {}", p.reason);
    if let Some(ref breakdown) = p.suggested_breakdown {
        body.push_str(&format!("\n\nSuggested breakdown:\n{breakdown}"));
    }
    let payload = serde_json::json!({ "body": body }).to_string();
    repo.log_activity(
        Some(&task.id),
        "worker-agent",
        "worker",
        "comment",
        &payload,
    )
    .await
    .map_err(|e| e.to_string())?;

    // Check escalation count via coordinator.
    // On the 2nd+ escalation for the same task, auto-route to Planner
    // (per ADR-051 §8 — Planner is now the escalation ceiling above Lead).
    let coordinator = state.coordinator().await;
    let escalation_count = if let Some(ref coord) = coordinator {
        coord
            .increment_escalation_count(&task.id)
            .await
            .unwrap_or(1)
    } else {
        1
    };

    if escalation_count >= 2 {
        let planner_reason = format!(
            "Auto-escalated to Planner after {} Lead escalations. Latest reason: {}",
            escalation_count, p.reason
        );
        if let Some(ref coord) = coordinator {
            let _ = coord
                .dispatch_planner_escalation(&task.id, &planner_reason, &task.project_id)
                .await;
        }
    }

    // Escalate the task to needs_lead_intervention in all cases.
    let updated = repo
        .transition(
            &task.id,
            djinn_core::models::TransitionAction::Escalate,
            "worker-agent",
            "worker",
            Some(&p.reason),
            None,
        )
        .await
        .map_err(|e| e.to_string())?;

    if escalation_count >= 2 {
        Ok(serde_json::json!({
            "status": "planner_escalated",
            "task_id": updated.id,
            "new_status": updated.status,
            "escalation_count": escalation_count,
            "message": "Task has been escalated multiple times. Routing to Planner for board-level review. Your session should end now."
        }))
    } else {
        Ok(serde_json::json!({
            "status": "escalated",
            "task_id": updated.id,
            "new_status": updated.status,
            "escalation_count": escalation_count,
            "message": "Task escalated to Lead. Your session should end now."
        }))
    }
}

pub(super) async fn call_request_planner(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    #[derive(serde::Deserialize)]
    struct RequestPlannerParams {
        id: String,
        reason: String,
    }

    let p: RequestPlannerParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());

    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };

    let body = format!("[PLANNER_REQUEST] Lead escalating to Planner. {}", p.reason);
    let payload = serde_json::json!({ "body": body }).to_string();
    repo.log_activity(Some(&task.id), "lead-agent", "lead", "comment", &payload)
        .await
        .map_err(|e| e.to_string())?;

    let Some(coordinator) = state.coordinator().await else {
        return Ok(serde_json::json!({
            "error": "coordinator not available — cannot dispatch Planner"
        }));
    };

    let _ = coordinator
        .dispatch_planner_escalation(&task.id, &p.reason, &task.project_id)
        .await;

    Ok(serde_json::json!({
        "status": "planner_dispatched",
        "task_id": task.id,
        "message": "Planner has been dispatched to review this task. Your session should end now."
    }))
}

pub(super) async fn call_task_comment_add(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    session_role: Option<&str>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: TaskCommentAddParams = parse_args(arguments)?;
    let default_role = session_role.unwrap_or("system");
    let project_id = project_id_for_path(state, project_path).await?;
    let server = djinn_control_plane::server::DjinnMcpServer::new(state.to_mcp_state());
    let Json(response) = shared_add_task_comment(
        &server,
        &project_id,
        SharedCommentTaskRequest {
            id: p.id,
            body: p.body,
            actor_id: p.actor_id.unwrap_or_else(|| default_role.to_string()),
            actor_role: p.actor_role.unwrap_or_else(|| default_role.to_string()),
        },
    )
    .await;

    error_or_to_value(response, activity_entry_to_value)
}
