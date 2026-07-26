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
                        // Truncate large payload string values.
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

    // Apply epic-dependency edits first (resolve blockers globally so they can
    // live in another repo). The target epic is resolved within the session
    // project; cross-project dependencies are set at creation via epic_create.
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
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    resolved_project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: EpicCreateParams = parse_args(arguments)?;
    if p.title.trim().is_empty() {
        return Err("epic title is required".to_string());
    }
    let project_repo = ProjectRepository::new(state.db.clone(), state.event_bus.clone());
    // Mode D may target a sibling repo via `project`; otherwise use the
    // session's resolved project.
    let project_id = if let Some(proj) = p.project.as_deref().filter(|s| !s.is_empty()) {
        match project_repo.resolve(proj).await {
            Ok(Some(id)) => id,
            _ => return Err(format!("project not found: {proj}")),
        }
    } else {
        match resolved_project_id {
            Some(id) => id.to_string(),
            None => resolve_project_id_for_agent_tools(state, arguments).await?,
        }
    };

    let epic_repo = EpicRepository::new(state.db.clone(), state.event_bus.clone());
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

    // Wire epic dependencies (resolved globally for cross-repo ordering).
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
        let proposal_repo = ProposalRepository::new(state.db.clone(), state.event_bus.clone());
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
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    resolved_project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: EpicBlockersParams = parse_args(arguments)?;
    let project_id = match resolved_project_id {
        Some(id) => id.to_string(),
        None => resolve_project_id_for_agent_tools(state, arguments).await?,
    };
    let repo = EpicRepository::new(state.db.clone(), state.event_bus.clone());
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
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    resolved_project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: EpicBlockersParams = parse_args(arguments)?;
    let project_id = match resolved_project_id {
        Some(id) => id.to_string(),
        None => resolve_project_id_for_agent_tools(state, arguments).await?,
    };
    let repo = EpicRepository::new(state.db.clone(), state.event_bus.clone());
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

pub(crate) async fn call_proposal_show(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: ProposalShowParams = parse_args(arguments)?;

    // Validate `fields` if provided.
    if let Some(ref fields) = p.fields {
        djinn_control_plane::tools::proposal_ops::validate_show_fields(fields)?;
    }
    // Validate `revision_bodies` if provided.
    if let Some(ref rb) = p.revision_bodies {
        djinn_control_plane::tools::proposal_ops::validate_revision_bodies_value(rb)?;
    }

    let field_selected = |name: &str| {
        p.fields
            .as_ref()
            .is_none_or(|f| f.iter().any(|s| s == name))
    };

    let proposal_repo = ProposalRepository::new(state.db.clone(), state.event_bus.clone());
    let Some(proposal) = proposal_repo.resolve(&p.id).await.ok().flatten() else {
        return Err(format!("proposal not found: {}", p.id));
    };

    let revisions_for_head = proposal_repo
        .revisions(&proposal.id)
        .await
        .map_err(|e| e.to_string())?;
    let head_revision = revisions_for_head
        .iter()
        .rev()
        .find(|revision| {
            revision.seq == proposal.latest_revision_seq
                && revision.body == proposal.body
                && revision.body_format == proposal.body_format
        })
        .ok_or_else(|| {
            format!(
                "committed revision not found for proposal {}/{}",
                proposal.id, proposal.latest_revision_seq
            )
        })?;
    let latest_lint = proposal_repo
        .lint_for_revision(head_revision)
        .await
        .map_err(|e| e.to_string())?;
    let mut result = serde_json::json!({ "latest_lint": latest_lint });

    if field_selected("proposal") {
        let acceptance: serde_json::Value =
            serde_json::from_str(&proposal.acceptance_criteria).unwrap_or(serde_json::json!([]));
        result["id"] = serde_json::json!(proposal.id);
        result["short_id"] = serde_json::json!(proposal.short_id);
        result["title"] = serde_json::json!(proposal.title);
        result["body"] = serde_json::json!(proposal.body);
        result["status"] = serde_json::json!(proposal.status);
        result["acceptance_criteria"] = acceptance;
    }

    if field_selected("revisions") {
        let mut revisions = Vec::with_capacity(revisions_for_head.len());
        for revision in &revisions_for_head {
            let lint = proposal_repo
                .lint_for_revision(revision)
                .await
                .map_err(|e| e.to_string())?;
            let mut model =
                djinn_control_plane::tools::proposal_ops::ProposalRevisionModel::from(revision);
            model.lint = Some(lint);
            revisions.push(model);
        }
        let mode = p.revision_bodies.as_deref().unwrap_or("excerpt");
        djinn_control_plane::tools::proposal_ops::apply_revision_body_mode(&mut revisions, mode);
        result["revisions"] = serde_json::to_value(revisions).map_err(|e| e.to_string())?;
    }

    if field_selected("targets") {
        let targets = proposal_repo
            .targets(&proposal.id)
            .await
            .map_err(|e| e.to_string())?;
        let project_repo = ProjectRepository::new(state.db.clone(), state.event_bus.clone());
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
        result["targets"] = serde_json::json!(target_json);
    }

    Ok(result)
}

pub(crate) async fn call_proposal_complete(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: ProposalCompleteParams = parse_args(arguments)?;
    let proposal_repo = ProposalRepository::new(state.db.clone(), state.event_bus.clone());
    let Some(proposal) = proposal_repo.resolve(&p.id).await.ok().flatten() else {
        return Err(format!("proposal not found: {}", p.id));
    };
    // Only a building proposal can be completed via this path. (A done/archived
    // proposal is already terminal; refuse rather than silently re-stamp.)
    if proposal.status != "building" {
        return Err(format!(
            "proposal {} is `{}`, not `building` — only a building proposal can be completed",
            proposal.short_id, proposal.status
        ));
    }
    // Completing a proposal asserts every acceptance criterion is satisfied —
    // flip them all to met=true so the proposal reads N/N (the by-index merge
    // preserves each criterion's text).
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

/// Reconcile a proposal's acceptance-criteria `met` flags (Planner Workflow E).
/// Lightweight status annotation — does NOT bump a spec revision or clear
/// sign-offs. Mirrors `task_update_ac`: the incoming list is merged by index
/// against the current criteria, preserving each criterion's text.
pub(crate) async fn call_proposal_ac_set(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: ProposalAcSetParams = parse_args(arguments)?;
    let proposal_repo = ProposalRepository::new(state.db.clone(), state.event_bus.clone());
    let Some(proposal) = proposal_repo.resolve(&p.id).await.ok().flatten() else {
        return Err(format!("proposal not found: {}", p.id));
    };
    let ac_json = merge_acceptance_criteria(&proposal.acceptance_criteria, &p.acceptance_criteria);
    let updated = proposal_repo
        .set_acceptance_criteria(&proposal.id, &ac_json)
        .await
        .map_err(|e| e.to_string())?;
    // `proposal_ac_set` is the Planner Workflow E reconciliation tool: when it
    // successfully records the delivered AC state, also advance proposal-level
    // and per-graduated-epic reconciliation metadata to the revision just
    // reconciled so proposal.show badges reflect the actual closeout path.
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

/// Retire one obsolete graduated epic during proposal reconciliation.
pub(crate) async fn call_proposal_reconcile_obsolete_epic(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: ProposalReconcileObsoleteEpicParams = parse_args(arguments)?;
    let proposal_repo = ProposalRepository::new(state.db.clone(), state.event_bus.clone());
    let epic_repo = EpicRepository::new(state.db.clone(), state.event_bus.clone());
    let task_repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());

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

/// Apply real acceptance-criteria spec amendments (rewrite/drop/waive) with a
/// required audit reason. Unlike `proposal_ac_set`, this delegates to the DB
/// repository's revision-bumping amendment path rather than met-flag merge.
pub(crate) async fn call_proposal_ac_amend(
    state: &AgentContext,
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

    let proposal_repo = ProposalRepository::new(state.db.clone(), state.event_bus.clone());
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

/// Deprecated compatibility route for stale `request_lead` calls from
/// worker/reviewer sessions that were dispatched before the drain cutover
/// (epic 10qg).  Logs a typed `deprecated_request_lead` activity and routes
/// through `dispatch_planner_escalation` WITHOUT transitioning the task to
/// `needs_lead_intervention`.
pub(crate) async fn call_request_lead(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    #[derive(serde::Deserialize)]
    struct RequestLeadParams {
        id: String,
        reason: String,
        suggested_breakdown: Option<String>,
    }

    let p: RequestLeadParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());

    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };

    // Emit a typed deprecated-request-lead activity so the drain window is
    // observable.  Preserves the caller's reason and suggested_breakdown.
    let mut body = format!(
        "DEPRECATED: request_lead is deprecated for worker/reviewer; routing to Planner. {}",
        p.reason
    );
    if let Some(ref breakdown) = p.suggested_breakdown {
        body.push_str(&format!("\n\nSuggested breakdown:\n{breakdown}"));
    }
    let payload = serde_json::json!({ "body": body }).to_string();
    repo.log_activity(
        Some(&task.id),
        "system",
        "system",
        "deprecated_request_lead",
        &payload,
    )
    .await
    .map_err(|e| e.to_string())?;

    // Dispatch to Planner via the shared escalation path — no
    // needs_lead_intervention transition, no durable escalation count.
    let Some(coordinator) = state.coordinator().await else {
        return Ok(serde_json::json!({
            "error": "coordinator not available — cannot dispatch Planner via deprecated request_lead"
        }));
    };

    // Fold suggested_breakdown into the reason so the Planner remediation
    // task/comment receives the stale caller's full context — not just the
    // bare reason.
    let planner_reason = match p.suggested_breakdown {
        Some(ref breakdown) => format!("{}\n\nSuggested breakdown:\n{breakdown}", p.reason),
        None => p.reason.clone(),
    };
    let _ = coordinator
        .dispatch_planner_escalation(&task.id, &planner_reason, &task.project_id)
        .await;

    Ok(serde_json::json!({
        "status": "planner_dispatched",
        "deprecated": "request_lead",
        "task_id": task.id,
        "message": "request_lead is deprecated for worker/reviewer; the task has been routed to Planner. Your session should end now."
    }))
}

/// Route a planner escalation request from any role (worker, reviewer, or lead).
/// Logs a role-neutral Planner-request activity that preserves the caller's
/// reason, then dispatches `dispatch_planner_escalation`.
pub(crate) async fn call_request_planner(
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

    // Role-neutral planner-request activity — preserves the caller's reason.
    let body = format!("[PLANNER_REQUEST] {}", p.reason);
    let payload = serde_json::json!({ "body": body }).to_string();
    repo.log_activity(Some(&task.id), "system", "system", "comment", &payload)
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
