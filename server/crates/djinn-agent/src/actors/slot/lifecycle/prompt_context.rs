//! Role-specific prompt-context assembly: conflict, activity, epic, knowledge,
//! code-graph, and CI directives → rendered system prompt with extensions + skills.

use std::path::Path;

use djinn_core::models::Task;
use djinn_core::models::task_attempt::TaskAttemptPromptSummary;

use crate::actors::slot::MergeConflictMetadata;
use crate::actors::slot::helpers::{
    COMBINED_BRIEF_TOTAL_CHARS, build_reviewer_diff_context, build_role_code_graph_context,
    derive_task_scope_paths, extract_worker_context, format_attempt_history,
    format_knowledge_notes, recent_feedback,
};
use crate::actors::slot::lifecycle::attempt_context;
use crate::context::AgentContext;
use crate::prompts::{TaskContext, apply_role_extensions, apply_skills};
use crate::roles::AgentRole;
use crate::skills::ResolvedSkill;
use djinn_db::{NoteRepository, ProposalRepository, TaskRepository};

/// Fully-assembled prompt context for a single role session.
#[allow(dead_code)]
pub(crate) struct PromptContext {
    /// Merge-conflict file list. `None` when no active conflict.
    pub conflict_files: Option<String>,
    /// Activity-log digest. `None` when no activity on the task.
    pub activity_text: Option<String>,
    /// Last `work_submitted` summary (reviewer context).
    pub worker_summary: Option<String>,
    /// Last `work_submitted` remaining concerns (reviewer context).
    pub worker_concerns: Option<String>,
    /// Epic context block.
    pub epic_context: Option<String>,
    /// Knowledge-notes block scoped to the task's paths.
    pub knowledge_context: Option<String>,
    /// Auto-injected `code_graph context` summary.
    pub code_graph_context: Option<String>,
    /// Auto-injected `code_graph detect_changes` summary for reviewers.
    pub reviewer_diff_context: Option<String>,
    /// sa4x: promoted BLOCKING directive for red required CI.
    pub ci_blocking_directive: Option<String>,
    /// 4x3v / jteh: durable prior attempt history for the current task.
    pub prior_attempts: Option<Vec<TaskAttemptPromptSummary>>,
    /// 4x3v / jteh: completed dependency-parent summaries from blocker parents.
    pub completed_dependency_parents: Option<Vec<djinn_db::CompletedParentSummary>>,
    /// y8pv / 48ru: one-line resume note for worker dispatch.
    pub worker_resume_note: Option<String>,
    /// Base system prompt rendered from the role template + `TaskContext`.
    pub base_system_prompt: String,
    /// Base prompt with role-level extensions + `learned_prompt` appended.
    pub system_prompt_with_extensions: String,
    /// Final prompt: extensions + skills. Pushed as the system message.
    pub system_prompt: String,
    /// Setup-command description for session log provenance.
    pub prompt_setup_commands: Option<String>,
}

/// Sibling project flagged as relevant (read-only multi-repo, no eager checkout).
#[derive(Debug, Clone)]
pub(crate) struct ReadSourceInfo {
    pub slug: String,
    pub name: String,
}

/// Append read-only sibling repo section to prompt. No-op when no read sources.
fn append_read_sources_prompt(prompt: &str, read_sources: &[ReadSourceInfo]) -> String {
    if read_sources.is_empty() {
        return prompt.to_string();
    }
    let mut s = String::from(prompt);
    s.push_str("\n\n## Related repositories (read-only)\n");
    s.push_str(
        "These sibling repos are flagged as relevant to this task. Read any file with \
         `read(project=\"<slug>\", file_path=...)`, search them with \
         `code_search(query=..., project=\"<slug>\")` (omit `project` to search ALL \
         registered repos at once), and for full shell/build use \
         `shell(project=\"<slug>\", ...)`. You can reach ANY registered repo this way — \
         these are just the ones called out as relevant. You MUST NOT write to, commit \
         to, or open a PR against them — every write goes to THIS task's own project.\n\n",
    );
    for rs in read_sources {
        s.push_str(&format!("- **{}** ({})\n", rs.slug, rs.name));
    }
    s
}

/// Inputs for [`assemble_prompt_context`].
#[allow(clippy::too_many_arguments)]
pub(crate) struct PromptContextInputs<'a> {
    pub task: &'a Task,
    /// Role whose template is rendered (may be a specialist override).
    pub runtime_role: &'a dyn AgentRole,
    /// Role consulted for `needs_epic_context` (original injected role).
    pub role_for_epic_check: &'a dyn AgentRole,
    pub project_path: &'a str,
    pub worktree_path: &'a Path,
    pub conflict_ctx: Option<&'a MergeConflictMetadata>,
    pub merge_validation_ctx: Option<String>,
    pub prompt_setup_commands: Option<String>,
    pub system_prompt_extensions: &'a str,
    pub learned_prompt: Option<&'a str>,
    pub resolved_skills: &'a [ResolvedSkill],
    pub app_state: &'a AgentContext,
    /// Read-only multi-repo sources for the task.
    pub read_sources: &'a [ReadSourceInfo],
    /// Worker resume note (y8pv/48ru). `None` for non-worker roles.
    pub worker_resume_note: Option<&'a str>,
    /// Per-server MCP instructions from connected servers, in deterministic
    /// server-name order. Failed or no-instruction servers are omitted.
    pub mcp_server_instructions: &'a std::collections::BTreeMap<String, String>,
}

/// Format conflicting files as a `- <path>` markdown list.
fn format_conflict_files(conflict_ctx: Option<&MergeConflictMetadata>) -> Option<String> {
    conflict_ctx.map(|m| {
        m.conflicting_files
            .iter()
            .map(|f| format!("- {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

/// Build activity-log digest with recent feedback and per-role counts. Returns None when empty.
fn format_activity_text(
    activity_entries: &Option<Vec<djinn_core::models::ActivityEntry>>,
    max_feedback: usize,
) -> Option<String> {
    match activity_entries {
        Some(entries) if !entries.is_empty() => {
            let feedback = recent_feedback(entries, max_feedback);
            let mut counts: std::collections::BTreeMap<&str, usize> =
                std::collections::BTreeMap::new();
            for e in entries {
                if e.event_type == "comment" {
                    *counts.entry(e.actor_role.as_str()).or_default() += 1;
                }
            }
            let count_summary: String = counts
                .iter()
                .map(|(role, n)| format!("{n} {role}"))
                .collect::<Vec<_>>()
                .join(", ");
            let mut parts = Vec::new();
            if !feedback.is_empty() {
                parts.push(format!(
                    "**Recent feedback (newest last):**\n{}",
                    feedback.join("\n\n---\n")
                ));
            }
            if !count_summary.is_empty() {
                parts.push(format!(
                    "**Activity totals:** {count_summary} comments. Use `task_activity_list` with `actor_role` filter for full history."
                ));
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n\n"))
            }
        }
        _ => None,
    }
}

/// Format MCP server instructions as a deterministic `mcp_instructions` prompt
/// section. Returns `None` when the map is empty (no server provided instructions).
///
/// Each server gets its own subsection, sorted by server name. The section is
/// only included when at least one connected server returned non-empty
/// instructions.
fn format_mcp_instructions(
    mcp_server_instructions: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    if mcp_server_instructions.is_empty() {
        return None;
    }
    let mut sections = Vec::new();
    for (server_name, instruction) in mcp_server_instructions {
        sections.push(format!("### {server_name}\n{instruction}"));
    }
    Some(format!(
        "## MCP Server Instructions\n{}",
        sections.join("\n\n")
    ))
}

/// Apply extensions, skills, read sources, and MCP instructions to base prompt
/// in canonical order.
fn apply_prompt_sections(
    base_system_prompt: &str,
    system_prompt_extensions: &str,
    learned_prompt: Option<&str>,
    resolved_skills: &[ResolvedSkill],
    read_sources: &[ReadSourceInfo],
    mcp_server_instructions: &std::collections::BTreeMap<String, String>,
) -> String {
    let with_extensions =
        apply_role_extensions(base_system_prompt, system_prompt_extensions, learned_prompt);
    let with_skills = apply_skills(&with_extensions, resolved_skills);
    let with_read_sources = append_read_sources_prompt(&with_skills, read_sources);
    match format_mcp_instructions(mcp_server_instructions) {
        Some(section) => format!("{with_read_sources}\n\n{section}"),
        None => with_read_sources,
    }
}

/// Append sibling task summary lines to `ctx_lines` for the given epic.
async fn load_sibling_tasks(
    epic_id: &str,
    task_repo: &TaskRepository,
    ctx_lines: &mut Vec<String>,
) {
    if let Ok(result) = task_repo
        .list_filtered(djinn_db::ListQuery {
            parent: Some(epic_id.to_string()),
            limit: 50,
            ..Default::default()
        })
        .await
    {
        let open = result.tasks.iter().filter(|t| t.status != "closed").count();
        let closed = result.tasks.iter().filter(|t| t.status == "closed").count();
        ctx_lines.push(format!(
            "\n### Sibling Tasks ({open} open, {closed} closed)"
        ));
        for t in &result.tasks {
            let status_marker = if t.status == "closed" {
                "closed"
            } else {
                &t.status
            };
            ctx_lines.push(format!("- [{}] {}: {}", status_marker, t.short_id, t.title));
        }
    }
}

/// Append delivered-task sub-bullets for a blocker's closed tasks.
async fn append_blocker_deliveries(
    epic_id: &str,
    blocker: &djinn_db::EpicBlockerRef,
    task_repo: &TaskRepository,
    ctx_lines: &mut Vec<String>,
) {
    let closed_tasks = match task_repo
        .list_filtered(djinn_db::ListQuery {
            parent: Some(blocker.epic_id.clone()),
            status: Some("closed".to_string()),
            limit: 20,
            ..Default::default()
        })
        .await
    {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::debug!(
                epic_id = %epic_id,
                blocking_epic_id = %blocker.epic_id,
                error = %e,
                "Lifecycle: failed to list closed tasks for blocking epic"
            );
            return;
        }
    };
    for t in &closed_tasks.tasks {
        ctx_lines.push(format!("  - Delivered: {}", t.title));
    }
}

/// Append blocking-epic lines and delivered-task sub-bullets.
async fn load_blocking_epics(
    epic_id: &str,
    epic_repo: &djinn_db::EpicRepository,
    task_repo: &TaskRepository,
    ctx_lines: &mut Vec<String>,
) {
    let blockers = match epic_repo.list_blockers(epic_id).await {
        Ok(b) if !b.is_empty() => b,
        _ => return,
    };
    ctx_lines.push("\n### Blocking Epics".to_string());
    for blocker in &blockers {
        ctx_lines.push(format!(
            "- **{}** ({}) — {}",
            blocker.title, blocker.short_id, blocker.status
        ));
        append_blocker_deliveries(epic_id, blocker, task_repo, ctx_lines).await;
    }
}

/// Append proposal-sibling-epic lines if epic belongs to a multi-epic proposal.
async fn load_proposal_sibling_epics(
    epic_id: &str,
    epic_repo: &djinn_db::EpicRepository,
    proposal_repo: &ProposalRepository,
    ctx_lines: &mut Vec<String>,
) {
    let proposal = match proposal_repo.proposal_for_epic(epic_id).await {
        Ok(Some(p)) => p,
        _ => return,
    };
    let siblings = match proposal_repo.graduated_epics(&proposal.id).await {
        Ok(s) => s,
        _ => return,
    };
    let sibling_ids: Vec<String> = siblings
        .into_iter()
        .filter(|(sid, _)| sid != epic_id)
        .map(|(sid, _)| sid)
        .collect();
    if sibling_ids.is_empty() {
        return;
    }
    ctx_lines.push(format!("\n### Proposal Sibling Epics ({})", proposal.title));
    for sid in &sibling_ids {
        append_proposal_sibling_epic(epic_id, sid, epic_repo, ctx_lines).await;
    }
}

/// Append a single proposal-sibling-epic bullet; skip on error.
async fn append_proposal_sibling_epic(
    epic_id: &str,
    sibling_id: &str,
    epic_repo: &djinn_db::EpicRepository,
    ctx_lines: &mut Vec<String>,
) {
    match epic_repo.get(sibling_id).await {
        Ok(Some(sibling_epic)) => {
            ctx_lines.push(format!(
                "- **{}** ({}) — {}",
                sibling_epic.title, sibling_epic.short_id, sibling_epic.status
            ));
        }
        Ok(None) => {}
        Err(e) => {
            tracing::debug!(
                epic_id = %epic_id,
                sibling_epic_id = %sibling_id,
                error = %e,
                "Lifecycle: failed to load proposal sibling epic for prompt context"
            );
        }
    }
}

/// Load epic context block; returns None when not needed or on error.
async fn load_epic_context(
    task: &Task,
    needs_epic_context: bool,
    app_state: &AgentContext,
) -> Option<String> {
    if !needs_epic_context {
        return None;
    }
    let epic_id = task.epic_id.as_deref()?;
    let epic_repo =
        djinn_db::EpicRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let epic = epic_repo.get(epic_id).await.ok()??;
    let mut ctx_lines = vec![
        format!("**Epic:** {} ({})", epic.title, epic.short_id),
        format!("**Description:** {}", epic.description),
        format!(
            "**Memory refs:** call `epic_show({})` then `memory_read(identifier=<ref>)` for each — memory notes live in Dolt, not on disk.",
            epic.short_id
        ),
    ];
    load_sibling_tasks(epic_id, &task_repo, &mut ctx_lines).await;
    load_blocking_epics(epic_id, &epic_repo, &task_repo, &mut ctx_lines).await;
    let proposal_repo = ProposalRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    load_proposal_sibling_epics(epic_id, &epic_repo, &proposal_repo, &mut ctx_lines).await;
    Some(ctx_lines.join("\n"))
}

/// Load knowledge context from scope-matched notes. Returns None on error/empty.
async fn load_knowledge_context(
    task: &Task,
    epic_context: Option<&str>,
    app_state: &AgentContext,
) -> Option<String> {
    let note_repo = NoteRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let task_paths = derive_task_scope_paths(task, epic_context);
    match note_repo
        .query_by_scope_overlap(
            &task.project_id,
            &task_paths,
            &["pattern", "pitfall", "case"],
            0.3,
            10,
        )
        .await
    {
        Ok(notes) if !notes.is_empty() => Some(format_knowledge_notes(&notes, 2000)),
        Ok(_) => None,
        Err(e) => {
            tracing::debug!(
                task_id = %task.short_id,
                error = %e,
                "Lifecycle: failed to query knowledge context"
            );
            None
        }
    }
}

/// Build full prompt context for one role session. Non-fatal: DB queries fall back to None.
pub(crate) async fn assemble_prompt_context(inputs: PromptContextInputs<'_>) -> PromptContext {
    let PromptContextInputs {
        task,
        runtime_role,
        role_for_epic_check,
        project_path,
        worktree_path,
        conflict_ctx,
        merge_validation_ctx,
        prompt_setup_commands,
        system_prompt_extensions,
        learned_prompt,
        resolved_skills,
        app_state,
        read_sources,
        worker_resume_note,
        mcp_server_instructions,
    } = inputs;
    let conflict_files = format_conflict_files(conflict_ctx);
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let activity_entries = task_repo.list_activity(&task.id).await.ok();
    let activity_text = format_activity_text(&activity_entries, 3);
    let (worker_summary, worker_concerns) = extract_worker_context(&activity_entries);
    let epic_context =
        load_epic_context(task, role_for_epic_check.needs_epic_context(), app_state).await;
    let knowledge_context = load_knowledge_context(task, epic_context.as_deref(), app_state).await;
    let ci_blocking_directive = build_ci_blocking_directive(task);
    let task_attempt_repo = djinn_db::TaskAttemptRepository::new(app_state.db.clone());
    let prior_attempts = attempt_context::load_prior_attempts(task, &task_attempt_repo).await;
    let completed_dependency_parents =
        attempt_context::load_completed_dependency_parents(task, &task_attempt_repo).await;
    // Append attempt history to the existing activity text so it renders inside
    // the Activity Log section, not as a new competing top-level prompt section.
    // The attempt entries share the COMBINED_BRIEF_TOTAL_CHARS budget with existing
    // feedback.  Deduplication prevents rendering the same rejection text twice;
    // over-budget output drops oldest attempt entries first, then oldest
    // dependency-parent entries, with a deterministic truncation note.
    let activity_text = {
        let existing_len = activity_text.as_deref().map_or(0, |s| s.len());
        let remaining_budget = COMBINED_BRIEF_TOTAL_CHARS.saturating_sub(existing_len);
        let attempt_history_text = format_attempt_history(
            prior_attempts.as_deref().unwrap_or(&[]),
            completed_dependency_parents.as_deref().unwrap_or(&[]),
            activity_text.as_deref().unwrap_or(""),
            remaining_budget,
        );
        match (activity_text, attempt_history_text) {
            (Some(activity), Some(history)) => Some(format!("{activity}\n\n---\n\n{history}")),
            (activity @ Some(_), None) => activity,
            (None, history @ Some(_)) => history,
            (None, None) => None,
        }
    };
    let task_paths_for_code_graph = derive_task_scope_paths(task, epic_context.as_deref());
    let code_graph_context = build_role_code_graph_context(
        runtime_role.config().name,
        task,
        app_state,
        project_path,
        &task_paths_for_code_graph,
    )
    .await;
    let reviewer_diff_context = {
        let role_name = runtime_role.config().name;
        if crate::actors::slot::helpers::is_role_auto_code_context_enabled(role_name) {
            let (from_sha, to_sha) =
                resolve_reviewer_diff_shas(worktree_path, &task.project_id, app_state).await;
            if from_sha.is_some() || to_sha.is_some() {
                build_reviewer_diff_context(
                    role_name,
                    task,
                    app_state,
                    project_path,
                    from_sha.as_deref(),
                    to_sha.as_deref(),
                )
                .await
            } else {
                None
            }
        } else {
            None
        }
    };
    let base_system_prompt = runtime_role.render_prompt(
        task,
        &TaskContext {
            project_path: project_path.to_string(),
            workspace_path: worktree_path.display().to_string(),
            diff: None,
            commits: None,
            start_commit: None,
            end_commit: None,
            conflict_files: conflict_files.clone(),
            merge_base_branch: conflict_ctx.map(|m| m.base_branch.clone()),
            merge_target_branch: conflict_ctx.map(|m| m.merge_target.clone()),
            merge_failure_context: merge_validation_ctx.clone(),
            setup_commands: prompt_setup_commands.clone(),
            activity: activity_text.clone(),
            worker_summary: worker_summary.clone(),
            worker_concerns: worker_concerns.clone(),
            epic_context: epic_context.clone(),
            knowledge_context: knowledge_context.clone(),
            code_graph_context: code_graph_context.clone(),
            reviewer_diff_context: reviewer_diff_context.clone(),
            ci_blocking_directive: ci_blocking_directive.clone(),
            worker_resume_note: worker_resume_note.map(str::to_string),
        },
    );
    let system_prompt_with_extensions = apply_role_extensions(
        &base_system_prompt,
        system_prompt_extensions,
        learned_prompt,
    );
    let system_prompt = apply_prompt_sections(
        &base_system_prompt,
        system_prompt_extensions,
        learned_prompt,
        resolved_skills,
        read_sources,
        mcp_server_instructions,
    );
    PromptContext {
        conflict_files,
        activity_text,
        worker_summary,
        worker_concerns,
        epic_context,
        knowledge_context,
        code_graph_context,
        reviewer_diff_context,
        ci_blocking_directive,
        worker_resume_note: worker_resume_note.map(str::to_string),
        prior_attempts,
        completed_dependency_parents,
        base_system_prompt,
        system_prompt_with_extensions,
        system_prompt,
        prompt_setup_commands,
    }
}

/// Build BLOCKING directive for red required CI. Returns None for passing/advisory states.
fn build_ci_blocking_directive(task: &Task) -> Option<String> {
    if task.ci_status != "failing" {
        return None;
    }
    let base_sha = task.ci_last_remediation_base_sha.as_deref()?;
    let head_sha = task.ci_head_sha.as_deref().unwrap_or("unknown");
    let pr_number = task.ci_pr_number.unwrap_or(0);
    let check_names: Vec<String> =
        serde_json::from_str(&task.ci_blocking_required_check_names).unwrap_or_default();
    let checks_display = if check_names.is_empty() {
        "unknown".to_string()
    } else {
        check_names.join(", ")
    };
    let fingerprint_line = match &task.ci_failure_fingerprint {
        Some(fp) => format!("**Failure fingerprint:** `{fp}`\n"),
        None => String::new(),
    };
    Some(format!(
        "**PR:** #{pr_number}\\\n\
         **Failing head SHA:** `{head_sha}`\\\n\
         **Blocking checks:** {checks_display}\\\n\
         {fingerprint_line}\
         **Remediation baseline SHA:** `{base_sha}`\n\n\
         > REQUIRED CI is failing on the current PR head. You MUST fix the \
         failing required checks listed above before this task can proceed. \
         The task will remain in remediation until all blocking checks pass \
         on a new commit pushed to the PR branch."
    ))
}

/// Resolve (from_sha, to_sha) for reviewer diff context via git. Best-effort.
async fn resolve_reviewer_diff_shas(
    worktree_path: &Path,
    project_id: &str,
    app_state: &AgentContext,
) -> (Option<String>, Option<String>) {
    let target_branch = {
        let repo =
            djinn_db::ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());
        match repo.get_config(project_id).await {
            Ok(Some(config)) => config.target_branch,
            _ => "main".to_string(),
        }
    };
    let head_sha = djinn_git::rev_parse(worktree_path, "HEAD").await.ok();
    let base_sha = djinn_git::merge_base(worktree_path, &target_branch, "HEAD")
        .await
        .ok();
    (base_sha, head_sha)
}

/// Only the worker role receives resume context.
pub(crate) fn role_receives_worker_resume(role_name: &str) -> bool {
    role_name == "worker"
}

/// Build one-line worker resume note. Returns None when not applicable.
pub(crate) fn build_worker_resume_note(
    role_name: &str,
    metadata: Option<&djinn_runtime::ResumeLifecycleMetadata>,
) -> Option<String> {
    if !role_receives_worker_resume(role_name) {
        return None;
    }
    let metadata = metadata?;
    if !metadata.considered {
        return None;
    }
    let has_checkpoint = metadata.commit_sha.is_some();
    let has_submit_or_review = metadata
        .submit_or_review_id
        .as_ref()
        .is_some_and(|id| !id.trim().is_empty());
    let has_prior_session = metadata
        .prior_session_lineage
        .as_ref()
        .is_some_and(|s| !s.trim().is_empty());
    if !has_checkpoint && !has_submit_or_review && !has_prior_session {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(session) = &metadata.prior_session_lineage
        && !session.trim().is_empty()
    {
        parts.push(format!("prior session `{session}`"));
    }
    if let Some(sha) = &metadata.commit_sha
        && !sha.trim().is_empty()
    {
        parts.push(format!("checkpoint `{sha}`"));
    } else if let Some(id) = &metadata.submit_or_review_id
        && !id.trim().is_empty()
    {
        parts.push(format!("submit/review `{id}`"));
    }
    if let Some(reason) = metadata.selection_reason {
        parts.push(format!("terminated: {}", termination_label(reason)));
    }
    if let Some(prev_model) = &metadata.previous_model
        && !prev_model.trim().is_empty()
    {
        parts.push(format!("prev model `{prev_model}`"));
    }
    if let Some(summary) = &metadata.last_durable_progress_summary
        && !summary.trim().is_empty()
    {
        let truncated = if summary.len() > 120 {
            format!("{}…", &summary[..117])
        } else {
            summary.clone()
        };
        parts.push(format!("last progress: {truncated}"));
    }
    if let Some(cmd) = &metadata.verification_command
        && !cmd.trim().is_empty()
    {
        parts.push(format!("verify: `{cmd}`"));
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!(
        "**Resuming from prior session.** {}",
        parts.join("; ")
    ))
}

/// Map a [`ResumeSelectionReason`] to a human-readable termination label.
fn termination_label(reason: djinn_runtime::ResumeSelectionReason) -> &'static str {
    use djinn_runtime::ResumeSelectionReason as R;
    match reason {
        R::AutoSubmitAccepted => "auto-submit accepted",
        R::LatestSafeCheckpoint => "no-progress checkpoint",
        R::AlternateCheckpointRef => "alternate checkpoint ref",
        R::CleanTaskBranchFallback => "clean fallback",
        R::NewerTaskBranch => "newer task branch",
        R::CheckpointMissing => "checkpoint missing",
        R::CheckpointUnsafe => "checkpoint unsafe",
        R::MergeConflict => "merge conflict",
        R::Disabled => "resume disabled",
    }
}

#[cfg(test)]
#[path = "test_support.rs"]
mod test_support;

#[cfg(test)]
#[path = "prompt_context_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "ci_directive_tests.rs"]
mod ci_directive_tests;
