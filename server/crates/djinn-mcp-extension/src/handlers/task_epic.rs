//! Task, epic, and proposal tool handlers.
//!
//! All handlers operate through [`crate::ExtensionContext`] rather than
//! the concrete `AgentContext`.

use std::collections::HashSet;

use djinn_control_plane::tools::epic_ops::{
    EpicShowRequest, EpicTasksRequest, EpicUpdateDeltaRequest,
};
use djinn_control_plane::tools::task_tools::{
    CommentTaskRequest as SharedCommentTaskRequest, CreateTaskRequest as SharedCreateTaskRequest,
    UpdateTaskRequest as SharedUpdateTaskRequest, add_task_comment as shared_add_task_comment,
    create_task as shared_create_task, update_task as shared_update_task,
};
use djinn_db::repositories::proposal::ProposalAcceptanceCriteriaAmendment;
use djinn_db::{
    EpicRepository, ProjectRepository, ProposalDebateTrailCreateInput, ProposalRepository,
    SessionRepository, TaskRepository,
};
use rmcp::Json;

use crate::context::ExtensionContext;
use crate::helpers::*;
use crate::truncate::smart_truncate;
use crate::types::*;

// ── Task query / show ───────────────────────────────────────────────────────

pub(crate) async fn call_task_list(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: TaskListParams = parse_args(arguments)?;
    let repo = TaskRepository::new(ctx.db(), ctx.event_bus());

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

pub(crate) async fn call_task_show(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskShowParams = parse_args(arguments)?;
    let repo = TaskRepository::new(ctx.db(), ctx.event_bus());
    let session_repo = SessionRepository::new(ctx.db(), ctx.event_bus());

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

                const MAX_ACTIVITY_ENTRIES: usize = 30;
                const MAX_PAYLOAD_CHARS: usize = 1500;
                let activity = repo.list_activity(&task.id).await.unwrap_or_default();
                let activity_json: Vec<serde_json::Value> = activity
                    .iter()
                    .filter(|e| e.event_type != "session_error")
                    .take(MAX_ACTIVITY_ENTRIES)
                    .map(|entry| {
                        let mut payload = serde_json::from_str::<serde_json::Value>(&entry.payload)
                            .unwrap_or(serde_json::json!({}));
                        if let Some(obj) = payload.as_object_mut() {
                            for value in obj.values_mut() {
                                if let Some(s) = value.as_str()
                                    && s.len() > MAX_PAYLOAD_CHARS
                                {
                                    *value =
                                        serde_json::json!(smart_truncate(s, MAX_PAYLOAD_CHARS));
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

pub(crate) async fn call_task_activity_list(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    use djinn_db::ActivityQuery;

    let p: TaskActivityListParams = parse_args(arguments)?;
    let repo = TaskRepository::new(ctx.db(), ctx.event_bus());

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
                        *value = serde_json::json!(smart_truncate(s, MAX_PAYLOAD_CHARS));
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

// ── Epic tools ──────────────────────────────────────────────────────────────

pub(crate) async fn call_epic_show(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    resolved_project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: EpicShowParams = parse_args(arguments)?;
    let project_id = match resolved_project_id {
        Some(id) => id.to_string(),
        None => resolve_project_id_for_agent_tools(ctx, arguments).await?,
    };
    let repo = EpicRepository::new(ctx.db(), ctx.event_bus());
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
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    resolved_project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: EpicUpdateParams = parse_args(arguments)?;
    let project_id = match resolved_project_id {
        Some(id) => id.to_string(),
        None => resolve_project_id_for_agent_tools(ctx, arguments).await?,
    };
    let repo = EpicRepository::new(ctx.db(), ctx.event_bus());

    if p.blocked_by_add.is_some() || p.blocked_by_remove.is_some() {
        if let Some(target) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        {
            let mut add_ids = Vec::new();
            for r in p.blocked_by_add.clone().unwrap_or_default() {
                if let Ok(Some(e)) = repo.resolve(&r).await {
                    add_ids.push(e.id);
                } else {
                    return Err(format!("blocker epic not found: {r}"));
                }
            }
            let mut remove_ids = Vec::new();
            for r in p.blocked_by_remove.clone().unwrap_or_default() {
                match repo.resolve(&r).await {
                    Ok(Some(e)) => remove_ids.push(e.id),
                    _ => remove_ids.push(r),
                }
            }
            repo.update_blockers_atomic(&target.id, &add_ids, &remove_ids)
                .await
                .map_err(|e| e.to_string())?;
        } else {
            return Err(format!("epic not found: {}", p.id));
        }
    }

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

pub(crate) async fn call_epic_create(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    resolved_project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: EpicCreateParams = parse_args(arguments)?;
    if p.title.trim().is_empty() {
        return Err("epic title is required".to_string());
    }
    let project_repo = ProjectRepository::new(ctx.db(), ctx.event_bus());
    let project_id = if let Some(proj) = p.project.as_deref().filter(|s| !s.is_empty()) {
        match project_repo.resolve(proj).await {
            Ok(Some(id)) => id,
            _ => return Err(format!("project not found: {proj}")),
        }
    } else {
        match resolved_project_id {
            Some(id) => id.to_string(),
            None => resolve_project_id_for_agent_tools(ctx, arguments).await?,
        }
    };

    let epic_repo = EpicRepository::new(ctx.db(), ctx.event_bus());
    let memory_refs_json = p
        .memory_refs
        .as_ref()
        .map(|r| serde_json::to_string(r).unwrap_or_else(|_| "[]".to_string()));
    let epic = epic_repo
        .create_for_project(
            &project_id,
            djinn_db::EpicCreateInput {
                title: &p.title,
                description: p.description.as_deref().unwrap_or(""),
                emoji: "",
                color: "",
                owner: "",
                memory_refs: memory_refs_json.as_deref(),
                status: Some("open"),
                auto_breakdown: p.auto_breakdown,
                originating_adr_id: None,
                blocked_by: None,
            },
        )
        .await
        .map_err(|e| e.to_string())?;

    // Seed read sources (cross-repo read context).
    if let Some(sources) = &p.read_sources {
        for src in sources {
            if let Ok(Some(src_id)) = project_repo.resolve(src).await
                && src_id != epic.project_id
            {
                let _ = epic_repo.add_read_source(&epic.id, &src_id).await;
            }
        }
    }

    // Wire epic dependencies.
    if let Some(blockers) = &p.blocked_by {
        for b in blockers {
            match epic_repo.resolve(b).await {
                Ok(Some(be)) => {
                    epic_repo
                        .add_blocker(&epic.id, &be.id)
                        .await
                        .map_err(|e| e.to_string())?;
                }
                _ => return Err(format!("blocker epic not found: {b}")),
            }
        }
    }

    // Record the proposal → epic link (Planner Mode D).
    if let Some(pref) = &p.proposal_id {
        let proposal_repo = ProposalRepository::new(ctx.db(), ctx.event_bus());
        if let Ok(Some(prop)) = proposal_repo.resolve(pref).await {
            let _ = proposal_repo
                .link_epic(&prop.id, &epic.id, &epic.project_id)
                .await;
        }
    }

    serde_json::to_value(djinn_control_plane::tools::epic_ops::EpicModel::from(&epic))
        .map_err(|e| e.to_string())
}

pub(crate) async fn call_epic_blockers_list(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    resolved_project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: EpicBlockersParams = parse_args(arguments)?;
    let project_id = match resolved_project_id {
        Some(id) => id.to_string(),
        None => resolve_project_id_for_agent_tools(ctx, arguments).await?,
    };
    let repo = EpicRepository::new(ctx.db(), ctx.event_bus());
    let Some(epic) = repo
        .resolve_in_project(&project_id, &p.id)
        .await
        .ok()
        .flatten()
    else {
        return Err(format!("epic not found: {}", p.id));
    };
    let refs = repo
        .list_blockers(&epic.id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "blockers": refs.iter().map(|b| serde_json::json!({
            "epic_id": b.epic_id, "short_id": b.short_id, "title": b.title, "status": b.status,
        })).collect::<Vec<_>>()
    }))
}

pub(crate) async fn call_epic_blocked_list(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    resolved_project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: EpicBlockersParams = parse_args(arguments)?;
    let project_id = match resolved_project_id {
        Some(id) => id.to_string(),
        None => resolve_project_id_for_agent_tools(ctx, arguments).await?,
    };
    let repo = EpicRepository::new(ctx.db(), ctx.event_bus());
    let Some(epic) = repo
        .resolve_in_project(&project_id, &p.id)
        .await
        .ok()
        .flatten()
    else {
        return Err(format!("epic not found: {}", p.id));
    };
    let refs = repo
        .list_blocked_by(&epic.id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "blockers": refs.iter().map(|b| serde_json::json!({
            "epic_id": b.epic_id, "short_id": b.short_id, "title": b.title, "status": b.status,
        })).collect::<Vec<_>>()
    }))
}

pub(crate) async fn call_epic_tasks(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    resolved_project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: EpicTasksParams = parse_args(arguments)?;
    let project_id = match resolved_project_id {
        Some(id) => id.to_string(),
        None => resolve_project_id_for_agent_tools(ctx, arguments).await?,
    };
    let epic_repo = EpicRepository::new(ctx.db(), ctx.event_bus());
    let task_repo = TaskRepository::new(ctx.db(), ctx.event_bus());
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

pub(crate) async fn call_epic_close(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    resolved_project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: EpicShowParams = parse_args(arguments)?;
    let project_id = match resolved_project_id {
        Some(id) => id.to_string(),
        None => resolve_project_id_for_agent_tools(ctx, arguments).await?,
    };
    let repo = EpicRepository::new(ctx.db(), ctx.event_bus());
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

// ── Proposal tools ──────────────────────────────────────────────────────────

pub(crate) async fn call_proposal_show(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: ProposalShowParams = parse_args(arguments)?;
    let proposal_repo = ProposalRepository::new(ctx.db(), ctx.event_bus());
    let Some(proposal) = proposal_repo.resolve(&p.id).await.ok().flatten() else {
        return Err(format!("proposal not found: {}", p.id));
    };
    let targets = proposal_repo
        .targets(&proposal.id)
        .await
        .map_err(|e| e.to_string())?;
    let project_repo = ProjectRepository::new(ctx.db(), ctx.event_bus());
    let mut target_json = Vec::with_capacity(targets.len());
    for t in &targets {
        let slug = match project_repo.get(&t.project_id).await {
            Ok(Some(proj)) => format!("{}/{}", proj.github_owner, proj.github_repo),
            _ => t.project_id.clone(),
        };
        target_json.push(serde_json::json!({
            "project_id": t.project_id,
            "project": slug,
            "role": t.role,
        }));
    }
    let acceptance: serde_json::Value =
        serde_json::from_str(&proposal.acceptance_criteria).unwrap_or(serde_json::json!([]));
    Ok(serde_json::json!({
        "id": proposal.id,
        "short_id": proposal.short_id,
        "title": proposal.title,
        "body": proposal.body,
        "status": proposal.status,
        "acceptance_criteria": acceptance,
        "targets": target_json,
    }))
}

pub(crate) async fn call_proposal_debate_append(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: ProposalDebateAppendParams = parse_args(arguments)?;
    let proposal_repo = ProposalRepository::new(ctx.db(), ctx.event_bus());
    let Some(proposal) = proposal_repo.resolve(&p.proposal_id).await.ok().flatten() else {
        return Err(format!("proposal not found: {}", p.proposal_id));
    };
    let kind = p.kind.trim();
    if !matches!(kind, "objection" | "rebuttal" | "verdict") {
        return Err(format!(
            "invalid kind: {kind:?} (expected objection, rebuttal, or verdict)"
        ));
    }
    if p.body.trim().is_empty() {
        return Err("body must not be empty".to_string());
    }
    if p.agent_role.trim().is_empty() {
        return Err("agent_role must not be empty".to_string());
    }
    if p.round < 1 {
        return Err(format!("round must be >= 1 (got {})", p.round));
    }
    let entry = proposal_repo
        .add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &proposal.id,
            kind,
            body: &p.body,
            blocking: p.blocking,
            agent_role: p.agent_role.trim(),
            author_kind: "agent",
            author_model: None,
            source_task_id: None,
            against_revision_seq: p.against_revision_seq,
            round: p.round,
        })
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "ok": true,
        "id": entry.id,
        "kind": entry.kind,
        "blocking": entry.blocking,
        "round": entry.round,
    }))
}

pub(crate) async fn call_proposal_debate_list(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: ProposalDebateListParams = parse_args(arguments)?;
    let proposal_repo = ProposalRepository::new(ctx.db(), ctx.event_bus());
    let Some(proposal) = proposal_repo.resolve(&p.proposal_id).await.ok().flatten() else {
        return Err(format!("proposal not found: {}", p.proposal_id));
    };
    let entries = proposal_repo
        .debate_trail(&proposal.id)
        .await
        .map_err(|e| e.to_string())?;
    let rows: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "id": e.id,
                "round": e.round,
                "agent_role": e.agent_role,
                "kind": e.kind,
                "blocking": e.blocking,
                "resolved": e.resolved_at.is_some() && e.reopened_at.is_none(),
                "against_revision_seq": e.against_revision_seq,
                "body": e.body,
                "created_at": e.created_at,
            })
        })
        .collect();
    Ok(serde_json::json!({
        "proposal_id": proposal.id,
        "entries": rows,
    }))
}

pub(crate) async fn call_proposal_debate_resolve(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: ProposalDebateResolveParams = parse_args(arguments)?;
    let proposal_repo = ProposalRepository::new(ctx.db(), ctx.event_bus());
    let entry = proposal_repo
        .resolve_debate_trail_entry(&p.id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "ok": true,
        "id": entry.id,
        "kind": entry.kind,
        "resolved": entry.resolved_at.is_some(),
    }))
}

pub(crate) async fn call_proposal_complete(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: ProposalCompleteParams = parse_args(arguments)?;
    let proposal_repo = ProposalRepository::new(ctx.db(), ctx.event_bus());
    let Some(proposal) = proposal_repo.resolve(&p.id).await.ok().flatten() else {
        return Err(format!("proposal not found: {}", p.id));
    };
    if proposal.status != "building" {
        return Err(format!(
            "proposal {} is `{}`, not `building` — only a building proposal can be completed",
            proposal.short_id, proposal.status
        ));
    }
    let existing: Vec<serde_json::Value> =
        serde_json::from_str(&proposal.acceptance_criteria).unwrap_or_default();
    if !existing.is_empty() {
        let all_met: Vec<serde_json::Value> = existing
            .iter()
            .map(|_| serde_json::json!({ "met": true }))
            .collect();
        let ac_json = merge_acceptance_criteria(&proposal.acceptance_criteria, &all_met);
        let _ = proposal_repo
            .set_acceptance_criteria(&proposal.id, &ac_json)
            .await;
    }
    let updated = proposal_repo
        .set_done(&proposal.id)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(summary) = p.summary.as_deref().filter(|s| !s.trim().is_empty()) {
        tracing::info!(
            proposal_id = %updated.id,
            proposal_short_id = %updated.short_id,
            summary,
            "proposal_complete: marked proposal done"
        );
    }
    Ok(serde_json::json!({
        "ok": true,
        "id": updated.id,
        "short_id": updated.short_id,
        "status": updated.status,
    }))
}

pub(crate) async fn call_proposal_ac_set(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: ProposalAcSetParams = parse_args(arguments)?;
    let proposal_repo = ProposalRepository::new(ctx.db(), ctx.event_bus());
    let Some(proposal) = proposal_repo.resolve(&p.id).await.ok().flatten() else {
        return Err(format!("proposal not found: {}", p.id));
    };
    let ac_json = merge_acceptance_criteria(&proposal.acceptance_criteria, &p.acceptance_criteria);
    let updated = proposal_repo
        .set_acceptance_criteria(&proposal.id, &ac_json)
        .await
        .map_err(|e| e.to_string())?;
    let updated = if updated.status == "building" {
        proposal_repo
            .mark_reconciled(&updated.id)
            .await
            .map_err(|e| e.to_string())?
    } else {
        updated
    };
    let parsed: Vec<serde_json::Value> =
        serde_json::from_str(&updated.acceptance_criteria).unwrap_or_default();
    let met = parsed
        .iter()
        .filter(|c| {
            c.get("met")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
        .count();
    Ok(serde_json::json!({
        "ok": true,
        "id": updated.id,
        "short_id": updated.short_id,
        "met": met,
        "total": parsed.len(),
    }))
}

pub(crate) async fn call_proposal_ac_amend(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: ProposalAcAmendParams = parse_args(arguments)?;
    let reason = p.reason.as_deref().map(str::trim).unwrap_or_default();
    if reason.is_empty() {
        return Err("proposal_ac_amend requires a non-empty reason".to_string());
    }
    if p.amendments.is_empty() {
        return Err("proposal_ac_amend requires at least one amendment".to_string());
    }

    let mut amendments = Vec::with_capacity(p.amendments.len());
    for (position, amendment) in p.amendments.iter().enumerate() {
        let operation = amendment.operation.trim();
        match operation {
            "rewrite" => {
                let criterion = amendment
                    .criterion
                    .as_deref()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .ok_or_else(|| {
                        format!(
                            "proposal_ac_amend amendments[{position}] operation=rewrite requires non-empty `criterion`"
                        )
                    })?;
                amendments.push(ProposalAcceptanceCriteriaAmendment::Rewrite {
                    index: amendment.index,
                    criterion,
                });
            }
            "drop" => amendments.push(ProposalAcceptanceCriteriaAmendment::Drop {
                index: amendment.index,
            }),
            "waive" => amendments.push(ProposalAcceptanceCriteriaAmendment::Waive {
                index: amendment.index,
            }),
            other => {
                return Err(format!(
                    "proposal_ac_amend amendments[{position}] has invalid operation `{other}`; expected rewrite, drop, or waive"
                ));
            }
        }
    }

    let proposal_repo = ProposalRepository::new(ctx.db(), ctx.event_bus());
    let Some(proposal) = proposal_repo.resolve(&p.id).await.ok().flatten() else {
        return Err(format!("proposal not found: {}", p.id));
    };
    let updated = proposal_repo
        .amend_acceptance_criteria(&proposal.id, &amendments, reason)
        .await
        .map_err(|e| e.to_string())?;
    let parsed: Vec<serde_json::Value> =
        serde_json::from_str(&updated.acceptance_criteria).unwrap_or_default();

    Ok(serde_json::json!({
        "ok": true,
        "id": updated.id,
        "short_id": updated.short_id,
        "latest_revision_seq": updated.latest_revision_seq,
        "acceptance_criteria_count": parsed.len(),
    }))
}

pub(crate) async fn call_proposal_reconcile_obsolete_epic(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: ProposalReconcileObsoleteEpicParams = parse_args(arguments)?;
    let proposal_repo = ProposalRepository::new(ctx.db(), ctx.event_bus());
    let epic_repo = EpicRepository::new(ctx.db(), ctx.event_bus());
    let task_repo = TaskRepository::new(ctx.db(), ctx.event_bus());

    let Some(proposal) = proposal_repo.resolve(&p.proposal_id).await.ok().flatten() else {
        return Err(format!("proposal not found: {}", p.proposal_id));
    };
    let Some(epic) = epic_repo.resolve(&p.epic_id).await.ok().flatten() else {
        return Err(format!("epic not found: {}", p.epic_id));
    };

    let linked_epics = proposal_repo
        .graduated_epics(&proposal.id)
        .await
        .map_err(|e| e.to_string())?;
    if !linked_epics
        .iter()
        .any(|(linked_epic_id, _)| linked_epic_id == &epic.id)
    {
        return Err(format!(
            "epic {} is not linked to proposal {}",
            epic.short_id, proposal.short_id
        ));
    }

    let tasks = task_repo
        .list_by_epic(&epic.id)
        .await
        .map_err(|e| e.to_string())?;
    let merged: Vec<_> = tasks
        .iter()
        .filter(|task| {
            task.merge_commit_sha
                .as_deref()
                .is_some_and(|sha| !sha.is_empty())
        })
        .collect();
    if !merged.is_empty() {
        let merged_summary = merged
            .iter()
            .map(|task| {
                format!(
                    "{} ({}, merge_commit_sha={})",
                    task.short_id,
                    task.title,
                    task.merge_commit_sha.as_deref().unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let reason = p
            .reason
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("obsolete graduated epic contains merged work");
        proposal_repo
            .add_feedback(djinn_db::repositories::proposal::ProposalFeedbackCreateInput {
                proposal_id: &proposal.id,
                parent_id: None,
                author_kind: "ai",
                author_model: Some("proposal_reconcile_obsolete_epic"),
                body: &format!(
                    "Reconcile blocked while retiring obsolete epic {} ({}): {reason}. Already-merged tasks: {merged_summary}. No epics were unlinked or closed; do not mark the proposal reconciled until this is resolved.",
                    epic.short_id, epic.title
                ),
            })
            .await
            .map_err(|e| e.to_string())?;
        return Ok(serde_json::json!({
            "ok": false,
            "blocked": true,
            "proposal_id": proposal.id,
            "proposal_short_id": proposal.short_id,
            "epic_id": epic.id,
            "epic_short_id": epic.short_id,
            "blocked_reason": "merged_work",
            "message": "AI proposal feedback recorded; preserve all state, leave unrelated epics untouched, stop this reconcile pass, and do not mark reconciled.",
            "merged_tasks": merged.iter().map(|task| serde_json::json!({
                "id": task.id,
                "short_id": task.short_id,
                "title": task.title,
                "merge_commit_sha": task.merge_commit_sha,
            })).collect::<Vec<_>>()
        }));
    }

    let close_reason = p
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("obsolete graduated epic retired by proposal reconciliation");
    let mut closed_task_ids = Vec::new();
    for task in tasks.iter().filter(|task| task.status != "closed") {
        task_repo
            .transition(
                &task.id,
                djinn_core::models::TransitionAction::ForceClose,
                "proposal_reconcile_obsolete_epic",
                "ai",
                Some(close_reason),
                None,
            )
            .await
            .map_err(|e| e.to_string())?;
        closed_task_ids.push(task.id.clone());
    }
    let closed_epic = epic_repo.close(&epic.id).await.map_err(|e| e.to_string())?;
    proposal_repo
        .unlink_epic(&proposal.id, &epic.id)
        .await
        .map_err(|e| e.to_string())?;

    Ok(serde_json::json!({
        "ok": true,
        "blocked": false,
        "proposal_id": proposal.id,
        "proposal_short_id": proposal.short_id,
        "epic_id": closed_epic.id,
        "epic_short_id": closed_epic.short_id,
        "epic_status": closed_epic.status,
        "closed_task_ids": closed_task_ids,
        "unrelated_epics_preserved": true,
    }))
}

// ── Task mutation ───────────────────────────────────────────────────────────

pub(crate) async fn call_task_create(
    ctx: &dyn ExtensionContext,
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
    let project_id = project_id_for_path(ctx, project_path).await?;
    let server = djinn_control_plane::server::DjinnMcpServer::new(ctx.mcp_state());
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

pub(crate) async fn call_task_update(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: TaskUpdateParams = parse_args(arguments)?;
    let project_id = project_id_for_path(ctx, project_path).await?;
    let server = djinn_control_plane::server::DjinnMcpServer::new(ctx.mcp_state());
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

pub(crate) async fn call_task_update_ac(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskUpdateAcParams = parse_args(arguments)?;
    let repo = TaskRepository::new(ctx.db(), ctx.event_bus());

    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };

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

pub(crate) async fn call_task_comment_add(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    session_role: Option<&str>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: TaskCommentAddParams = parse_args(arguments)?;
    let default_role = session_role.unwrap_or("system");
    let project_id = project_id_for_path(ctx, project_path).await?;
    let server = djinn_control_plane::server::DjinnMcpServer::new(ctx.mcp_state());
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
