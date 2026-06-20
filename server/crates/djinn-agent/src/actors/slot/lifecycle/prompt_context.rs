//! Role-specific prompt-context assembly for the task lifecycle.
//!
//! This is a pure code-motion extraction from `run_task_lifecycle` (task #17).
//! It gathers the data the base prompt template needs — conflict metadata,
//! activity-log digest, extracted worker submission context, epic context,
//! knowledge notes — builds the full
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
//! Worker-resume context is intentionally **not** handled here: the
//! supervisor flow has no paused-session record to resume from. See the
//! `## Deferred: worker-resume` block in
//! [`crate::supervisor_impl::stage`] for the list of deleted helpers + the
//! cross-crate plumbing a reintroduction would need.

use std::path::Path;

use djinn_core::models::Task;

use crate::actors::slot::MergeConflictMetadata;
use crate::actors::slot::helpers::{
    build_reviewer_diff_context, build_role_code_graph_context, derive_task_scope_paths,
    extract_worker_context, format_knowledge_notes, recent_feedback,
};
use crate::context::AgentContext;
use crate::prompts::{TaskContext, apply_role_extensions, apply_skills};
use crate::roles::AgentRole;
use crate::skills::ResolvedSkill;
use djinn_db::{ProposalRepository, TaskRepository};

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
    /// PR E2: auto-injected `code_graph context` summary for the dispatch
    /// role. `None` when the role is not in the
    /// `DJINN_AUTO_CODE_CONTEXT_ROLES` allowlist or no scope-path symbols
    /// resolved.
    pub code_graph_context: Option<String>,
    /// PR E3: auto-injected `code_graph detect_changes` summary for
    /// reviewer roles. `None` when the role is not in the
    /// `DJINN_AUTO_CODE_CONTEXT_ROLES` allowlist, when no base/head SHAs
    /// could be resolved from the worktree, or when the detected change
    /// set is empty.
    pub reviewer_diff_context: Option<String>,
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

/// A sibling project flagged as relevant to this task (read-only multi-repo).
/// Reached on demand via `read(project=…)` / `code_search` / `shell(project=…)`
/// — no eager checkout.
#[derive(Debug, Clone)]
pub(crate) struct ReadSourceInfo {
    pub slug: String,
    pub name: String,
}

/// Append a "related repositories" section to the assembled system prompt.
/// Tells the agent which OTHER registered projects are relevant and how to read
/// them, while keeping all writes pinned to the task's own project. No-op when
/// the task has no flagged read sources.
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

/// Inputs for [`build_prompt_context`].
///
/// The supervisor path fills `conflict_ctx`,
/// `system_prompt_extensions`, and `learned_prompt` from
/// `conflict_context_for_dispatch` +
/// [`lifecycle::role_overrides::resolve_role_overrides`].  `merge_validation_ctx`
/// stays `None` — the legacy merge-validation prompt helper was deleted as
/// dead code in commit 6bf5d5931.
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
    /// Read-only multi-repo: other registered projects the task's epic
    /// allows it to read. Materialized + resolved by the caller.
    pub read_sources: &'a [ReadSourceInfo],
}

/// Build the full prompt context (all `TaskContext` fields, base +
/// extensions + skills prompts) for one role session.
///
/// Reads activity log, epic row (when the role needs it), knowledge notes
/// scoped to the task's paths. Non-fatal: every
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
        read_sources,
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
                        format!(
                            "**Memory refs:** call `epic_show({})` then `memory_read(identifier=<ref>)` for each — memory notes live in Dolt, not on disk.",
                            epic.short_id
                        ),
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

                    match epic_repo.list_blockers(epic_id).await {
                        Ok(blockers) if !blockers.is_empty() => {
                            ctx_lines.push("\n### Blocking Epics".to_string());
                            for blocker in &blockers {
                                ctx_lines.push(format!(
                                    "- **{}** ({}) — {}",
                                    blocker.title, blocker.short_id, blocker.status
                                ));
                                match task_repo_ctx
                                    .list_filtered(djinn_db::ListQuery {
                                        parent: Some(blocker.epic_id.clone()),
                                        status: Some("closed".to_string()),
                                        limit: 20,
                                        ..Default::default()
                                    })
                                    .await
                                {
                                    Ok(closed_tasks) => {
                                        for t in &closed_tasks.tasks {
                                            ctx_lines.push(format!("  - Delivered: {}", t.title));
                                        }
                                    }
                                    Err(e) => {
                                        tracing::debug!(
                                            epic_id = %epic_id,
                                            blocking_epic_id = %blocker.epic_id,
                                            error = %e,
                                            "Lifecycle: failed to list closed tasks for blocking epic"
                                        );
                                    }
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::debug!(
                                epic_id = %epic_id,
                                error = %e,
                                "Lifecycle: failed to list blocking epics for prompt context"
                            );
                        }
                    }

                    let proposal_repo =
                        ProposalRepository::new(app_state.db.clone(), app_state.event_bus.clone());
                    match proposal_repo.proposal_for_epic(epic_id).await {
                        Ok(Some(proposal)) => {
                            match proposal_repo.graduated_epics(&proposal.id).await {
                                Ok(siblings) => {
                                    let sibling_ids: Vec<_> = siblings
                                        .into_iter()
                                        .filter(|(sid, _)| sid != epic_id)
                                        .collect();
                                    if !sibling_ids.is_empty() {
                                        ctx_lines.push(format!(
                                            "\n### Proposal Sibling Epics ({})",
                                            proposal.title
                                        ));
                                        for (sid, _) in &sibling_ids {
                                            match epic_repo.get(sid).await {
                                                Ok(Some(sibling_epic)) => {
                                                    ctx_lines.push(format!(
                                                        "- **{}** ({}) — {}",
                                                        sibling_epic.title,
                                                        sibling_epic.short_id,
                                                        sibling_epic.status
                                                    ));
                                                }
                                                Ok(None) => {}
                                                Err(e) => {
                                                    tracing::debug!(
                                                        epic_id = %epic_id,
                                                        sibling_epic_id = %sid,
                                                        error = %e,
                                                        "Lifecycle: failed to load proposal sibling epic for prompt context"
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::debug!(
                                        epic_id = %epic_id,
                                        proposal_id = %proposal.id,
                                        error = %e,
                                        "Lifecycle: failed to list proposal sibling epics for prompt context"
                                    );
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            tracing::debug!(
                                epic_id = %epic_id,
                                error = %e,
                                "Lifecycle: failed to find parent proposal for prompt context"
                            );
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

    // PR E2: auto-include `code_graph context` for worker / reviewer roles
    // when `DJINN_AUTO_CODE_CONTEXT_ROLES` enables this role. Reuses the
    // task-scope-path inference already used by the knowledge context block.
    let task_paths_for_code_graph = derive_task_scope_paths(task, epic_context.as_deref());
    let code_graph_context = build_role_code_graph_context(
        runtime_role.config().name,
        task,
        app_state,
        project_path,
        &task_paths_for_code_graph,
    )
    .await;

    // PR E3: auto-include `code_graph detect_changes` summary for the
    // reviewer role. Resolves base/head SHAs by running `git merge-base
    // <target> HEAD` and `git rev-parse HEAD` against the task worktree
    // — the reviewer's worktree is the post-image of the task branch, so
    // HEAD is the head SHA. The merge target is the project's configured
    // target branch (defaulting to "main"). Failures resolving the SHAs
    // are non-fatal: we just skip injection.
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
            verification_commands: prompt_verification_commands.clone(),
            verification_rules: prompt_verification_rules.clone(),
            activity: activity_text.clone(),
            worker_summary: worker_summary.clone(),
            worker_concerns: worker_concerns.clone(),
            verification_failure: verification_failure.clone(),
            epic_context: epic_context.clone(),
            knowledge_context: knowledge_context.clone(),
            code_graph_context: code_graph_context.clone(),
            reviewer_diff_context: reviewer_diff_context.clone(),
        },
    );
    // Apply role-level prompt extensions from DB (system_prompt_extensions + learned_prompt).
    let system_prompt_with_extensions = apply_role_extensions(
        &base_system_prompt,
        system_prompt_extensions,
        learned_prompt,
    );
    // Append skills section after all other extensions.
    let system_prompt = apply_skills(&system_prompt_with_extensions, resolved_skills);
    // Read-only multi-repo: advertise the epic's read-source projects last
    // so the section survives all other extension/skill appends.
    let system_prompt = append_read_sources_prompt(&system_prompt, read_sources);

    PromptContext {
        conflict_files,
        activity_text,
        worker_summary,
        worker_concerns,
        verification_failure,
        epic_context,
        knowledge_context,
        code_graph_context,
        reviewer_diff_context,
        base_system_prompt,
        system_prompt_with_extensions,
        system_prompt,
        prompt_setup_commands,
        prompt_verification_commands,
        prompt_verification_rules,
    }
}

/// Resolve `(from_sha, to_sha)` for PR E3's reviewer diff context by
/// shelling out to `git` against the task's worktree.
///
/// `to_sha` = `git rev-parse HEAD` in the worktree.
/// `from_sha` = `git merge-base <target> HEAD` where `<target>` is the
/// project's configured target branch (default `main`). The merge-base
/// is what the reviewer would actually see if they ran `git diff
/// <target>..HEAD` themselves, so it's the right anchor for "what
/// changed in this PR".
///
/// Both SHAs are returned best-effort. Either value may be `None` if
/// the underlying git command fails — the caller skips injection
/// silently when both are missing.
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

    let head_sha = git_rev_parse(worktree_path, "HEAD").ok();
    let base_sha = git_merge_base(worktree_path, &target_branch, "HEAD").ok();

    (base_sha, head_sha)
}

fn git_rev_parse(worktree_path: &Path, rev: &str) -> std::io::Result<String> {
    let output = std::process::Command::new("git")
        .arg("rev-parse")
        .arg(rev)
        .current_dir(worktree_path)
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "git rev-parse {rev} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_merge_base(worktree_path: &Path, a: &str, b: &str) -> std::io::Result<String> {
    let output = std::process::Command::new("git")
        .arg("merge-base")
        .arg(a)
        .arg(b)
        .current_dir(worktree_path)
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "git merge-base {a} {b} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    use djinn_core::events::EventBus;
    use djinn_core::models::Epic;
    use djinn_db::{
        Database, EpicCreateInput, EpicRepository, ProposalCreateInput, ProposalRepository,
        TaskRepository,
    };
    use tokio_util::sync::CancellationToken;

    use crate::roles::LeadRole;
    use crate::test_helpers::{agent_context_from_db, create_test_project, test_tempdir};

    async fn create_epic(
        db: &Database,
        events: &EventBus,
        project_id: &str,
        title: &str,
        description: &str,
        status: Option<&str>,
    ) -> Epic {
        EpicRepository::new(db.clone(), events.clone())
            .create_for_project(
                project_id,
                EpicCreateInput {
                    title,
                    description,
                    emoji: "🧪",
                    color: "blue",
                    owner: "test-owner",
                    memory_refs: None,
                    status,
                    auto_breakdown: None,
                    originating_adr_id: None,
                },
            )
            .await
            .expect("create test epic")
    }

    async fn prompt_context_for_task(db: Database, task: &djinn_core::models::Task) -> String {
        let app_state = agent_context_from_db(db, CancellationToken::new());
        let worktree = test_tempdir("prompt-context-worktree-");
        let role = LeadRole;
        build_prompt_context(PromptContextInputs {
            task,
            runtime_role: &role,
            role_for_epic_check: &role,
            project_path: "/workspace/test-project",
            worktree_path: worktree.path(),
            conflict_ctx: None,
            merge_validation_ctx: None,
            prompt_setup_commands: None,
            prompt_verification_commands: None,
            prompt_verification_rules: None,
            system_prompt_extensions: "",
            learned_prompt: None,
            resolved_skills: &[],
            app_state: &app_state,
            read_sources: &[],
        })
        .await
        .epic_context
        .expect("lead prompt context includes epic context")
    }

    #[tokio::test]
    async fn epic_context_includes_blocking_and_sibling_sections() {
        let db = Database::ephemeral().await.expect("create ephemeral db");
        let events = EventBus::noop();
        let project = create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), events.clone());
        let task_repo = TaskRepository::new(db.clone(), events.clone());
        let proposal_repo = ProposalRepository::new(db.clone(), events.clone());

        let subject_epic = create_epic(
            &db,
            &events,
            &project.id,
            "Subject decomposition epic",
            "Build on dependency foundations without duplicating them.",
            None,
        )
        .await;
        let blocking_epic = create_epic(
            &db,
            &events,
            &project.id,
            "Foundation blocking epic",
            "Owns the schema and migration foundation.",
            Some("closed"),
        )
        .await;

        task_repo
            .create(
                &blocking_epic.id,
                "Ship shared migration",
                "migration delivered",
                "migration design",
                "task",
                1,
                "test-owner",
                Some("closed"),
            )
            .await
            .expect("create first closed blocker task");
        task_repo
            .create(
                &blocking_epic.id,
                "Ship shared schema module",
                "schema module delivered",
                "schema module design",
                "task",
                1,
                "test-owner",
                Some("closed"),
            )
            .await
            .expect("create second closed blocker task");

        epic_repo
            .update_blockers_atomic(
                &subject_epic.id,
                std::slice::from_ref(&blocking_epic.id),
                &[],
            )
            .await
            .expect("wire epic blocker relationship");

        let sibling_epic = create_epic(
            &db,
            &events,
            &project.id,
            "Sibling proposal epic",
            "Owns a later proposal phase.",
            None,
        )
        .await;
        let proposal = proposal_repo
            .create(ProposalCreateInput {
                title: "Dependency-aware decomposition proposal",
                body: "Proposal body",
                acceptance_criteria: None,
                status: Some("building"),
                body_format: None,
            })
            .await
            .expect("create proposal");
        proposal_repo
            .link_epic(&proposal.id, &subject_epic.id, &project.id)
            .await
            .expect("link subject epic to proposal");
        proposal_repo
            .link_epic(&proposal.id, &sibling_epic.id, &project.id)
            .await
            .expect("link sibling epic to proposal");

        let task = task_repo
            .create(
                &subject_epic.id,
                "Decompose subject epic",
                "task description",
                "task design",
                "task",
                1,
                "test-owner",
                None,
            )
            .await
            .expect("create subject task");

        let epic_context = prompt_context_for_task(db, &task).await;

        assert!(epic_context.contains("### Blocking Epics"));
        assert!(epic_context.contains("Foundation blocking epic"));
        assert!(epic_context.contains("Ship shared migration"));
        assert!(epic_context.contains("Ship shared schema module"));
        assert!(epic_context.contains("### Proposal Sibling Epics"));
        assert!(epic_context.contains("Sibling proposal epic"));
    }

    #[tokio::test]
    async fn epic_context_omits_sections_when_no_blockers_or_proposal() {
        let db = Database::ephemeral().await.expect("create ephemeral db");
        let events = EventBus::noop();
        let project = create_test_project(&db).await;
        let task_repo = TaskRepository::new(db.clone(), events.clone());
        let standalone_epic = create_epic(
            &db,
            &events,
            &project.id,
            "Standalone epic",
            "No blockers and no proposal link.",
            None,
        )
        .await;
        let task = task_repo
            .create(
                &standalone_epic.id,
                "Standalone task",
                "task description",
                "task design",
                "task",
                1,
                "test-owner",
                None,
            )
            .await
            .expect("create standalone task");

        let epic_context = prompt_context_for_task(db, &task).await;

        assert!(!epic_context.contains("### Blocking Epics"));
        assert!(!epic_context.contains("### Proposal Sibling Epics"));
    }
}
