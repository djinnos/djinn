use super::*;
use crate::knowledge_promotion::{
    KnowledgeCleanupReason, KnowledgePromotionDecision, apply_task_knowledge_decision,
};

pub(super) async fn call_task_transition(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    use djinn_core::models::{TaskStatus, TransitionAction};
    let p: TaskTransitionParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());
    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };
    let action = TransitionAction::parse(&p.action).map_err(|e| e.to_string())?;

    // Lead approve: transition to approved; coordinator handles PR creation separately.
    if action == TransitionAction::LeadApprove {
        if task.status != TaskStatus::InLeadIntervention.as_str() {
            return Ok(
                serde_json::json!({ "error": "lead_approve is only valid from in_lead_intervention" }),
            );
        }

        let updated = repo
            .transition(
                &task.id,
                TransitionAction::LeadApprove,
                "lead-agent",
                "lead",
                None,
                None,
            )
            .await
            .map_err(|e| e.to_string())?;
        return Ok(task_to_value(&updated));
    }

    // Guard: force_close requires either replacement_task_ids (for decomposition)
    // or a reason (for redundant/already-landed tasks). This prevents the Lead from
    // silently closing tasks without explanation while still allowing closure of
    // tasks whose work already landed on main.
    if action == TransitionAction::ForceClose {
        let has_replacements = p
            .replacement_task_ids
            .as_ref()
            .is_some_and(|ids| !ids.is_empty());
        let has_reason = p.reason.as_ref().is_some_and(|r: &String| !r.is_empty());

        if !has_replacements && !has_reason {
            return Ok(serde_json::json!({
                "error": "force_close requires either replacement_task_ids (array of subtask IDs created as replacements) or a reason string explaining why the task is being closed (e.g. work already landed on main, task is redundant)."
            }));
        }

        // Validate replacement task IDs if provided (skip empty arrays)
        if let Some(ref ids) = p.replacement_task_ids
            && !ids.is_empty()
        {
            let mut missing = Vec::new();
            for id in ids {
                match repo.resolve(id).await {
                    Ok(Some(t))
                        if t.status == TaskStatus::Open.as_str()
                            || t.status == TaskStatus::Closed.as_str() => {}
                    Ok(Some(t)) => {
                        missing.push(format!("{} (status: {})", id, t.status));
                    }
                    _ => {
                        missing.push(format!("{} (not found)", id));
                    }
                }
            }
            if !missing.is_empty() {
                return Ok(serde_json::json!({
                    "error": format!(
                        "force_close replacement tasks must exist and be open or closed. Problems: {}",
                        missing.join(", ")
                    )
                }));
            }

            // Auto-transfer downstream blocker edges: any task that was blocked by
            // the closing task should now be blocked by the last replacement task.
            // This prevents premature dispatch when force_close auto-resolves blockers
            // on the transition that follows.
            let last_replacement_id = ids.last().unwrap();
            if let Ok(Some(last_task)) = repo.resolve(last_replacement_id).await {
                let downstream = repo.list_blocked_by(&task.id).await.unwrap_or_default();
                for blocked_ref in &downstream {
                    let _ = repo.add_blocker(&blocked_ref.task_id, &last_task.id).await;
                }
            }
        }
    }

    let target = p
        .target_status
        .as_deref()
        .map(TaskStatus::parse)
        .transpose()
        .map_err(|e| e.to_string())?;
    let project_id = project_id_for_path(state, project_path).await?;
    let server = djinn_mcp::server::DjinnMcpServer::new(state.to_mcp_state());
    let Json(response) = shared_transition_task(
        &server,
        &project_id,
        SharedTransitionTaskRequest {
            id: task.id,
            action,
            actor_id: "lead-agent".to_string(),
            actor_role: "lead".to_string(),
            reason: p.reason,
            target_override: target,
        },
    )
    .await;
    error_or_to_value(response, task_response_to_value)
}

pub(super) async fn call_task_delete_branch(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskDeleteBranchParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());
    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };

    // Interrupt the paused worker session record.
    crate::task_merge::interrupt_paused_worker_session(&task.id, state).await;

    // Resolve project dir — needed for teardown_worktree.
    let project_dir =
        match crate::task_merge::resolve_project_path_for_id(&task.project_id, state).await {
            Some(p) => std::path::PathBuf::from(p),
            None => return Ok(serde_json::json!({ "error": "project not found" })),
        };

    // Tear down: LSP shutdown → worktree removal → branch deletion (correct order).
    let worktree_path = project_dir
        .join(".djinn")
        .join("worktrees")
        .join(&task.short_id);
    let base_branch = format!("task/{}", task.short_id);
    crate::actors::slot::teardown_worktree(
        &task.short_id,
        &worktree_path,
        &project_dir,
        state,
        true,
    )
    .await;
    let _ = apply_task_knowledge_decision(
        &task.id,
        KnowledgePromotionDecision::Discard,
        KnowledgeCleanupReason::BranchReset,
        state,
    )
    .await;

    Ok(serde_json::json!({
        "ok": true,
        "task_id": task.short_id,
        "branch_deleted": base_branch,
    }))
}

pub(super) async fn call_task_archive_activity(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskArchiveActivityParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());
    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };
    let count = repo
        .archive_activity_for_task(&task.id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "ok": true, "task_id": task.short_id, "archived_count": count }))
}

pub(super) async fn call_task_reset_counters(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskResetCountersParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());
    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };
    sqlx::query(
        "UPDATE tasks SET reopen_count = 0, continuation_count = 0, intervention_count = intervention_count + 1, last_intervention_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ'), updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ') WHERE id = ?"
    )
    .bind(&task.id)
    .execute(state.db.pool())
    .await
    .map_err(|e| e.to_string())?;
    let updated = repo
        .get(&task.id)
        .await
        .map_err(|e| e.to_string())?
        .unwrap_or(task.clone());
    state
        .event_bus
        .send(djinn_core::events::DjinnEventEnvelope::task_updated(
            &updated, false,
        ));
    Ok(
        serde_json::json!({ "ok": true, "task_id": task.short_id, "reopen_count": 0, "continuation_count": 0 }),
    )
}

pub(super) async fn call_task_kill_session(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskKillSessionParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());
    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };

    // Interrupt the paused session record; clean up the worktree without deleting the branch
    // so the task can resume on the same branch next dispatch.
    crate::task_merge::interrupt_paused_worker_session(&task.id, state).await;
    if let Some(project_path_str) =
        crate::task_merge::resolve_project_path_for_id(&task.project_id, state).await
    {
        let project_dir = std::path::PathBuf::from(&project_path_str);
        let worktree_path = project_dir
            .join(".djinn")
            .join("worktrees")
            .join(&task.short_id);
        crate::actors::slot::teardown_worktree(
            &task.short_id,
            &worktree_path,
            &project_dir,
            state,
            false,
        )
        .await;
    }

    Ok(serde_json::json!({
        "ok": true,
        "task_id": task.short_id,
        "message": "Paused session interrupted and worktree cleaned up. Next dispatch will start a fresh session."
    }))
}

pub(super) async fn call_task_blocked_list(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskShowParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());
    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };
    let blocked = repo
        .list_blocked_by(&task.id)
        .await
        .map_err(|e| e.to_string())?;
    let tasks: Vec<serde_json::Value> = blocked
        .iter()
        .map(|b| {
            serde_json::json!({
                "task_id": b.task_id,
                "short_id": b.short_id,
                "title": b.title,
                "status": b.status,
            })
        })
        .collect();
    Ok(serde_json::json!({ "tasks": tasks }))
}
