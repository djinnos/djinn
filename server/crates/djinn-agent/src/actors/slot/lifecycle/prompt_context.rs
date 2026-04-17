//! Role-specific prompt-context assembly for the task lifecycle.
//!
//! This is a pure code-motion extraction from `run_task_lifecycle` (task #17).
//! It gathers the data the base prompt template needs — conflict metadata,
//! activity-log digest, extracted worker submission context, epic context,
//! knowledge notes, planner patrol context — builds the full
//! [`TaskContext`], renders the role's system prompt, and layers the DB-level
//! prompt extensions + skills on top.
//!
//! The extracted block is unconditional: every field of [`TaskContext`] is
//! populated regardless of role, and the downstream prompt template picks
//! what to use based on the role (the per-role gating already lives inside
//! [`AgentRole::needs_epic_context`], `render_prompt`, and the template
//! strings themselves). This mirrors the byte-for-byte behaviour of the
//! former inline block between lines ~671 and ~844 of `lifecycle.rs`.
//!
//! Worker-resume context is intentionally **not** handled here — that block
//! hangs off a paused session record + saved conversation that lives later
//! in the lifecycle, and the supervisor path doesn't have a resume analogue
//! yet (see the `TODO(phase-2): wire worker resume` in `execute_stage`).

use std::path::Path;

use djinn_core::models::Task;

use crate::actors::slot::MergeConflictMetadata;
use crate::actors::slot::helpers::{
    build_planner_patrol_context, derive_task_scope_paths, extract_worker_context,
    format_knowledge_notes, recent_feedback,
};
use crate::context::AgentContext;
use crate::prompts::{TaskContext, apply_role_extensions, apply_skills};
use crate::roles::AgentRole;
use crate::skills::ResolvedSkill;
use djinn_db::TaskRepository;

/// Fully-assembled prompt context for a single role session.
///
/// Holds both the intermediate fields (so the call site can still observe
/// them for tracing / test assertions) and the final rendered system
/// prompts. The lifecycle call site consumes `system_prompt` for the session
/// conversation; the intermediate fields are kept so they can be referenced
/// by future extraction steps (and to make the helper testable without
/// re-deriving data downstream).
#[allow(dead_code)]
pub(crate) struct PromptContext {
    /// `- <path>` markdown list built from the merge-conflict metadata. `None`
    /// when there's no active conflict context.
    pub conflict_files: Option<String>,
    /// Pre-formatted activity-log digest (last-3 high-signal comments + per-
    /// role totals). `None` when there is no activity on the task.
    pub activity_text: Option<String>,
    /// Last `work_submitted` summary (reviewer context).
    pub worker_summary: Option<String>,
    /// Last `work_submitted` remaining concerns (reviewer context).
    pub worker_concerns: Option<String>,
    /// Body of the last verification-failure comment (worker-on-retry
    /// context).
    pub verification_failure: Option<String>,
    /// Epic context block (lead / roles that call `needs_epic_context`).
    pub epic_context: Option<String>,
    /// Knowledge-notes block scoped to the task's paths.
    pub knowledge_context: Option<String>,
    /// Planner-patrol-only code-graph diff summary.
    pub planner_patrol_context: Option<String>,
    /// Base system prompt rendered from the role template + `TaskContext`.
    pub base_system_prompt: String,
    /// Base prompt with role-level `system_prompt_extensions` + `learned_prompt`
    /// appended.
    pub system_prompt_with_extensions: String,
    /// Final prompt: extensions + resolved skills section.  This is what gets
    /// pushed into the conversation as the system message.
    pub system_prompt: String,
    /// Cloned-forward setup-command description (session log provenance +
    /// downstream mcp/verification plumbing).
    pub prompt_setup_commands: Option<String>,
    /// Cloned-forward verification-command description.
    pub prompt_verification_commands: Option<String>,
    /// Cloned-forward verification-rules markdown.
    pub prompt_verification_rules: Option<String>,
}

/// Inputs for [`build_prompt_context`].
///
/// The supervisor path passes `conflict_ctx = None`, `merge_validation_ctx =
/// None`, `system_prompt_extensions = ""`, `learned_prompt = None` because
/// the specialist-override + conflict-retry code is not yet wired through
/// the supervisor (see the TODOs in `supervisor::stage::execute_stage`).
#[allow(clippy::too_many_arguments)]
pub(crate) struct PromptContextInputs<'a> {
    pub task: &'a Task,
    /// Role whose template is rendered (`runtime_role` in the lifecycle —
    /// may be a specialist override).
    pub runtime_role: &'a dyn AgentRole,
    /// Role consulted for `needs_epic_context`. In the lifecycle this is the
    /// *original injected role*, not the specialist runtime role, because
    /// specialists only override config (prompt extensions, skills, model)
    /// — the "does this role see epic context" question is about the
    /// base-role contract.
    pub role_for_epic_check: &'a dyn AgentRole,
    pub project_path: &'a str,
    pub worktree_path: &'a Path,
    pub conflict_ctx: Option<&'a MergeConflictMetadata>,
    pub merge_validation_ctx: Option<String>,
    pub prompt_setup_commands: Option<String>,
    pub prompt_verification_commands: Option<String>,
    pub prompt_verification_rules: Option<String>,
    pub system_prompt_extensions: &'a str,
    pub learned_prompt: Option<&'a str>,
    pub resolved_skills: &'a [ResolvedSkill],
    pub app_state: &'a AgentContext,
}

/// Build the full prompt context (all `TaskContext` fields, base +
/// extensions + skills prompts) for one role session.
///
/// Reads activity log, epic row (when the role needs it), knowledge notes
/// scoped to the task's paths, and planner patrol signals. Non-fatal: every
/// DB query falls back to `None` on error, mirroring the original inline
/// block.
pub(crate) async fn build_prompt_context(inputs: PromptContextInputs<'_>) -> PromptContext {
    let PromptContextInputs {
        task,
        runtime_role,
        role_for_epic_check,
        project_path,
        worktree_path,
        conflict_ctx,
        merge_validation_ctx,
        prompt_setup_commands,
        prompt_verification_commands,
        prompt_verification_rules,
        system_prompt_extensions,
        learned_prompt,
        resolved_skills,
        app_state,
    } = inputs;

    let conflict_files = conflict_ctx.map(|m| {
        m.conflicting_files
            .iter()
            .map(|f| format!("- {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    });

    // Fetch activity log for the prompt: last 3 high-signal comments plus a
    // summary of total counts by role so the agent knows what to look up.
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let activity_entries = task_repo.list_activity(&task.id).await.ok();
    let activity_text = match &activity_entries {
        Some(entries) if !entries.is_empty() => {
            // Last 3 high-signal comments (lead, reviewer, verification)
            let feedback = recent_feedback(entries, 3);

            // Count comments by role for the summary line
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
    };

    // Extract worker submission summary/concerns and last verification failure
    // from the activity log so the reviewer can see why certain changes were made.
    let (worker_summary, worker_concerns, verification_failure) =
        extract_worker_context(&activity_entries);

    // ── Build epic context for roles that need it (e.g. lead) ─────────────────
    let epic_context = if role_for_epic_check.needs_epic_context() {
        if let Some(ref epic_id) = task.epic_id {
            let epic_repo =
                djinn_db::EpicRepository::new(app_state.db.clone(), app_state.event_bus.clone());
            let task_repo_ctx =
                TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
            match epic_repo.get(epic_id).await {
                Ok(Some(epic)) => {
                    let mut ctx_lines = vec![
                        format!("**Epic:** {} ({})", epic.title, epic.short_id),
                        format!("**Description:** {}", epic.description),
                        format!("**Memory refs:** {}", epic.memory_refs),
                    ];
                    // Load sibling tasks
                    if let Ok(result) = task_repo_ctx
                        .list_filtered(djinn_db::ListQuery {
                            parent: Some(epic_id.clone()),
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
                            ctx_lines
                                .push(format!("- [{}] {}: {}", status_marker, t.short_id, t.title));
                        }
                    }
                    Some(ctx_lines.join("\n"))
                }
                _ => None,
            }
        } else {
            None
        }
    } else {
        None
    };

    // ── Build knowledge context from scope-matched notes ─────────────
    let knowledge_context = {
        let note_repo =
            djinn_db::NoteRepository::new(app_state.db.clone(), app_state.event_bus.clone());

        let task_paths = derive_task_scope_paths(task, epic_context.as_deref());

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
    };

    let planner_patrol_context =
        build_planner_patrol_context(task, app_state, project_path).await;

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
            verification_commands: prompt_verification_commands.clone(),
            verification_rules: prompt_verification_rules.clone(),
            activity: activity_text.clone(),
            worker_summary: worker_summary.clone(),
            worker_concerns: worker_concerns.clone(),
            verification_failure: verification_failure.clone(),
            epic_context: epic_context.clone(),
            knowledge_context: knowledge_context.clone(),
            planner_patrol_context: planner_patrol_context.clone(),
        },
    );
    // Apply role-level prompt extensions from DB (system_prompt_extensions + learned_prompt).
    let system_prompt_with_extensions =
        apply_role_extensions(&base_system_prompt, system_prompt_extensions, learned_prompt);
    // Append skills section after all other extensions.
    let system_prompt = apply_skills(&system_prompt_with_extensions, resolved_skills);

    PromptContext {
        conflict_files,
        activity_text,
        worker_summary,
        worker_concerns,
        verification_failure,
        epic_context,
        knowledge_context,
        planner_patrol_context,
        base_system_prompt,
        system_prompt_with_extensions,
        system_prompt,
        prompt_setup_commands,
        prompt_verification_commands,
        prompt_verification_rules,
    }
}
