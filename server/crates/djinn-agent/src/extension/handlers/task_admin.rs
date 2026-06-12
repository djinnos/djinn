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

    // Block agents from using `close` directly. The Close transition records
    // `close_reason="completed"`, which is the "work actually landed on main"
    // signal — only the supervisor's open_pr / pr_merge path is allowed to
    // fire it (after a real merge_commit_sha is produced). Letting the
    // lead/planner call it lets them fake a "merged" status for tasks whose
    // PR was never opened, which silently mass-closes the wave. Force the
    // agent to use `force_close` with an explicit reason instead.
    if action == TransitionAction::Close {
        return Ok(serde_json::json!({
            "error": "close is not callable by agents — it marks the task as merged-to-main \
                      and is reserved for the supervisor's PR-merge path. If the task is \
                      done because work already landed on a different branch/PR, use \
                      force_close with a reason explaining what landed; if you're closing a \
                      decomposed or redundant task, use force_close with replacement_task_ids \
                      and/or a reason."
        }));
    }

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

        // Worker tasks (issue_type=task/bug/feature) must produce a real merged PR
        // before they can be closed. The planner/lead patrol previously used a
        // bare "reason" to mass-close tasks whose reviewer just approved them but
        // whose PR never actually opened — making it look like the wave landed
        // when nothing reached the remote. Block that path explicitly: if the
        // task is a full-lifecycle issue_type AND merge_commit_sha is null AND
        // no replacement_task_ids are provided, refuse the force_close.
        let task_uses_full_lifecycle = matches!(
            task.issue_type.as_str(),
            "task" | "bug" | "feature" | "decomposition"
        );
        let has_merge_sha = task
            .merge_commit_sha
            .as_ref()
            .is_some_and(|s| !s.is_empty());
        if task_uses_full_lifecycle && !has_merge_sha && !has_replacements {
            return Ok(serde_json::json!({
                "error": format!(
                    "task '{}' (issue_type={}) has no merge_commit_sha and no replacement_task_ids — \
                     force_close with a bare reason is not allowed for worker tasks. Either provide \
                     replacement_task_ids (decomposition) or wait for the PR to open + merge. If the \
                     work genuinely landed on a different branch/PR, set merge_commit_sha first via \
                     the supervisor PR-merge flow.",
                    task.short_id, task.issue_type
                )
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
    let server = djinn_control_plane::server::DjinnMcpServer::new(state.to_mcp_state());
    let task_id_for_cleanup = task.id.clone();
    let cleanup_action = action.clone();
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

    // Branch hygiene: when a lead / admin / planner caller force-closes
    // a task, delete the task branch on both the local mirror and the GitHub
    // remote so it doesn't sit on the mirror as a dead ref and an open PR.
    // Best-effort: errors are logged inside `cleanup_task_branches_post_close`
    // and never block the response.
    if cleanup_action == TransitionAction::ForceClose
        && matches!(
            response,
            djinn_control_plane::tools::task_tools::ErrorOr::Ok(_)
        )
    {
        crate::task_merge::cleanup_task_branches_post_close(
            &task_id_for_cleanup,
            &state.db,
            &state.event_bus,
            state.mirror.as_deref(),
        )
        .await;
    }

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

    // Resolve project dir so we can delete the branch from the local clone.
    let project_dir =
        match crate::task_merge::resolve_project_path_for_id(&task.project_id, state).await {
            Some(p) => std::path::PathBuf::from(p),
            None => return Ok(serde_json::json!({ "error": "project not found" })),
        };

    // Task #8: the supervisor-driven dispatch path does not create user-
    // visible `.djinn/worktrees/<short_id>` directories, so there's nothing
    // to tear down.  Just delete the local task branch; the remote branch
    // (if any) is cleaned up by the PR pipeline / GitHub settings.
    let base_branch = format!("task/{}", task.short_id);
    if let Ok(git) = state.git_actor(&project_dir).await
        && let Err(e) = git.delete_branch(&base_branch).await
    {
        tracing::warn!(
            task_id = %task.short_id,
            branch = %base_branch,
            error = %e,
            "task_delete_branch: failed to delete local task branch"
        );
    }
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
    let updated = repo
        .reset_intervention_counters(&task.id)
        .await
        .map_err(|e| e.to_string())?;
    // `reset_intervention_counters` already broadcasts `task_updated`, but
    // the legacy caller also emitted one via `state.event_bus`. Keep a
    // single canonical emit via the repo.
    let _ = &updated;
    Ok(
        serde_json::json!({ "ok": true, "task_id": task.short_id, "reopen_count": 0, "continuation_count": 0 }),
    )
}

pub(crate) async fn call_task_kill_session(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskKillSessionParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());
    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };

    // Interrupt the paused session record.  Task #8: no worktree cleanup
    // is needed under the supervisor-driven dispatch path — the paused
    // session doesn't have a persistent task worktree associated with it.
    crate::task_merge::interrupt_paused_worker_session(&task.id, state).await;

    Ok(serde_json::json!({
        "ok": true,
        "task_id": task.short_id,
        "message": "Paused session interrupted. Next dispatch will start a fresh session."
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
