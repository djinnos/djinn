use super::*;
use crate::knowledge_promotion::{
    KnowledgeCleanupReason, KnowledgePromotionDecision, apply_task_knowledge_decision,
};

/// True when an issue_type goes through the full PR/merge lifecycle (and so a
/// "done" close should correspond to a real merge to the base branch).
fn issue_type_uses_full_lifecycle(issue_type: &str) -> bool {
    matches!(issue_type, "task" | "bug" | "feature" | "decomposition")
}

/// Decide whether an agent-driven `force_close` must be refused because the task
/// holds committed-but-unmerged work. Returns `Some(error_message)` to refuse,
/// `None` to allow. Pure so it can be unit-tested without a DB.
///
/// Two cases, both for full-lifecycle worker tasks with no `merge_commit_sha`:
///
/// 1. **No replacements at all** — a bare-reason force-close that would mark the
///    task done without it ever merging or being decomposed. (Original guard.)
/// 2. **Open PR but unmerged** (task b29n / PR #718) — even *with*
///    replacement_task_ids, a task whose PR is open but hasn't merged must not be
///    force-closed. The `replacement_task_ids` exemption exists for genuine
///    decomposition (where the replacement carries the work forward), but the
///    planner abused it to declare an approved-but-unmerged PR "superseded": the
///    required `Quality Gate` was red, the code never reached `main`, and the
///    "replacement" (CI hygiene) did not carry the original code. Downstream
///    tasks then broke against missing symbols. An open, unmerged PR means the
///    work is real and committed but blocked from landing — it must be driven to
///    merge (or its PR explicitly closed), not silently abandoned.
///
/// Tasks whose PR never opened (`pr_url` is `None`/empty) can still be
/// force-closed via replacement_task_ids — that is legitimate decomposition. The
/// coordinator's pr_poller loop-breaking escalation is unaffected because it
/// transitions through the repository directly, not this tool handler.
fn force_close_unmerged_block(
    issue_type: &str,
    merge_commit_sha: Option<&str>,
    pr_url: Option<&str>,
    has_replacements: bool,
    short_id: &str,
) -> Option<String> {
    if !issue_type_uses_full_lifecycle(issue_type) {
        return None;
    }
    let has_merge_sha = merge_commit_sha.is_some_and(|s| !s.is_empty());
    if has_merge_sha {
        return None;
    }
    let has_open_pr = pr_url.is_some_and(|u| !u.is_empty());

    if has_open_pr {
        return Some(format!(
            "task '{short_id}' (issue_type={issue_type}) has an open PR ({pr}) but no \
             merge_commit_sha — force_close is not allowed, even with replacement_task_ids. The \
             work is committed to a PR branch but has NOT merged to the base branch (often because \
             a required check is red), so closing it would abandon the code and break downstream \
             tasks that depend on it. Drive the PR to merge (fix the failing required checks), or \
             close the PR on GitHub first if the work is genuinely being abandoned. Do not declare \
             an unmerged PR 'done'/'superseded'.",
            pr = pr_url.unwrap_or(""),
        ));
    }

    if !has_replacements {
        return Some(format!(
            "task '{short_id}' (issue_type={issue_type}) has no merge_commit_sha and no \
             replacement_task_ids — force_close with a bare reason is not allowed for worker \
             tasks. Either provide replacement_task_ids (decomposition) or wait for the PR to open \
             + merge. If the work genuinely landed on a different branch/PR, set merge_commit_sha \
             first via the supervisor PR-merge flow."
        ));
    }

    None
}

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

        // Worker tasks must produce a real merged PR before they can be closed
        // as done. See `force_close_unmerged_block` for the two guarded cases
        // (no-merge-sha-no-replacements, and open-PR-but-unmerged). Both stem
        // from the planner faking a "landed/superseded" status for work that
        // never reached the base branch.
        if let Some(err) = force_close_unmerged_block(
            &task.issue_type,
            task.merge_commit_sha.as_deref(),
            task.pr_url.as_deref(),
            has_replacements,
            &task.short_id,
        ) {
            return Ok(serde_json::json!({ "error": err }));
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
    // visible `.task-runtime/worktrees/<short_id>` directories, so there's nothing
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

#[cfg(test)]
mod force_close_guard_tests {
    use super::force_close_unmerged_block;

    #[test]
    fn merged_task_is_allowed() {
        // A real merge SHA → work landed → closing is fine regardless of PR/reps.
        assert!(
            force_close_unmerged_block(
                "task",
                Some("3249dce5"),
                Some("https://github.com/o/r/pull/718"),
                false,
                "b29n",
            )
            .is_none()
        );
    }

    #[test]
    fn open_pr_unmerged_blocked_even_with_replacements() {
        // The exact b29n / PR #718 abuse: approved but unmerged PR, planner
        // force-closes as "superseded" with a CI-hygiene replacement task. Must
        // be refused even though replacements are provided.
        let err = force_close_unmerged_block(
            "task",
            None,
            Some("https://github.com/djinnos/djinn/pull/718"),
            true, // has replacement zgaq
            "b29n",
        )
        .expect("open unmerged PR must block force_close");
        assert!(err.contains("open PR"));
        assert!(err.contains("not merged") || err.contains("NOT merged"));
        assert!(err.contains("even with replacement_task_ids"));
    }

    #[test]
    fn open_pr_unmerged_blocked_without_replacements() {
        let err = force_close_unmerged_block(
            "feature",
            None,
            Some("https://github.com/o/r/pull/9"),
            false,
            "abcd",
        )
        .expect("open unmerged PR must block");
        assert!(err.contains("open PR"));
    }

    #[test]
    fn no_pr_no_replacements_blocked() {
        // Original guard: bare-reason close of a never-PR'd worker task.
        let err = force_close_unmerged_block("bug", None, None, false, "abcd")
            .expect("bare-reason close must block");
        assert!(err.contains("no merge_commit_sha and no replacement_task_ids"));
    }

    #[test]
    fn no_pr_with_replacements_allowed() {
        // Legitimate decomposition: PR never opened, replacement carries work.
        assert!(
            force_close_unmerged_block("task", None, None, true, "abcd").is_none(),
            "decomposition of a never-PR'd task is allowed via replacements"
        );
    }

    #[test]
    fn empty_pr_url_treated_as_no_pr() {
        // An empty pr_url string must not count as an open PR.
        assert!(
            force_close_unmerged_block("task", None, Some(""), true, "abcd").is_none(),
            "empty pr_url is not an open PR; decomposition stays allowed"
        );
        let err = force_close_unmerged_block("task", None, Some(""), false, "abcd")
            .expect("empty pr_url + no replacements still hits the bare-reason guard");
        assert!(err.contains("no merge_commit_sha and no replacement_task_ids"));
    }

    #[test]
    fn non_lifecycle_issue_type_is_exempt() {
        // Spikes / chores etc. don't go through the PR/merge lifecycle.
        assert!(
            force_close_unmerged_block(
                "spike",
                None,
                Some("https://github.com/o/r/pull/1"),
                false,
                "abcd",
            )
            .is_none()
        );
    }
}
