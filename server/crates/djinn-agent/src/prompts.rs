// Embedded prompt templates for Djinn agent types.
//
// Templates are compiled into the binary via include_str!() and rendered with
// simple {{variable}} string substitution. A shared base template provides
// system identity, task context, workspace config, and common tools. Each role
// template appends role-specific mission, instructions, and rules.

use serde::Deserialize;

#[cfg(test)]
use super::AgentType;
use crate::roles::RoleConfig;
use djinn_core::models::Task;

/// Hard cap on rendered system prompt size (chars). Individual sections have
/// their own soft limits, but this catches cases where multiple sections
/// combine to blow past a reasonable size.
const MAX_SYSTEM_PROMPT_CHARS: usize = 30_000;

// ─── Embedded templates ────────────────────────────────────────────────────────

const BASE_TEMPLATE: &str = include_str!("prompts/base.md");
pub(crate) const DEV_TEMPLATE: &str = include_str!("prompts/dev.md");
pub(crate) const REVIEWER_TEMPLATE: &str = include_str!("prompts/task-reviewer.md");
pub(crate) const LEAD_TEMPLATE: &str = include_str!("prompts/lead.md");
pub(crate) const PLANNER_TEMPLATE: &str = include_str!("prompts/planner.md");
pub(crate) const ARCHITECT_TEMPLATE: &str = include_str!("prompts/architect.md");

// ─── Context ───────────────────────────────────────────────────────────────────

/// Runtime context injected alongside the task's stored fields at render time.
///
/// Worker agents need `project_path` and `workspace_path`. Reviewer agents
/// additionally use the workspace to inspect code. Workers with conflict
/// context receive merge details.
pub struct TaskContext {
    /// Absolute path to the project root (passed to Djinn tools as `project`).
    pub project_path: String,
    /// Absolute path to the active execution workspace (task worktree).
    pub workspace_path: String,

    // ── Task reviewer fields ──────────────────────────────────────────────────
    /// Formatted git diff for the task branch (start_commit..end_commit).
    pub diff: Option<String>,
    /// Formatted `git log --oneline` output for the task branch.
    pub commits: Option<String>,
    /// Merge-base of the task branch with the target branch (task reviewer).
    pub start_commit: Option<String>,
    /// HEAD of the task branch (task reviewer).
    pub end_commit: Option<String>,

    // -- Conflict resolver fields --------------------------------------------
    pub conflict_files: Option<String>,
    pub merge_base_branch: Option<String>,
    pub merge_target_branch: Option<String>,
    pub merge_failure_context: Option<String>,

    // ── Project command fields ────────────────────────────────────────────
    /// Newline-separated list of setup command names, or None if none configured.
    pub setup_commands: Option<String>,
    /// Newline-separated list of verification command names, or None if none configured.
    pub verification_commands: Option<String>,

    // ── Verification rules ────────────────────────────────────────────────
    /// Formatted verification rules section for scoped build/test guidance.
    /// None or empty means no rules are configured; section is omitted from prompt.
    pub verification_rules: Option<String>,

    // ── Activity log ─────────────────────────────────────────────────────
    /// Pre-formatted activity log (comments, transitions) for the task.
    pub activity: Option<String>,

    // ── Worker submission context (for reviewer) ─────────────────────────
    /// Summary from the last `work_submitted` activity entry.
    pub worker_summary: Option<String>,
    /// Remaining concerns from the last `work_submitted` activity entry.
    pub worker_concerns: Option<String>,
    /// Body of the last verification failure comment.
    pub verification_failure: Option<String>,

    // ── Epic context ─────────────────────────────────────────────────────
    /// Epic context section for lead agents (title, description, memory_refs, sibling tasks).
    pub epic_context: Option<String>,
}

// ─── Renderer ─────────────────────────────────────────────────────────────────

/// Render a system prompt for `agent_type` using data from `task` and `ctx`.
///
/// Test-only convenience wrapper — production code uses `render_prompt_for_role`.
#[cfg(test)]
pub fn render_prompt(agent_type: AgentType, task: &Task, ctx: &TaskContext) -> String {
    let config = agent_type.role_config();
    render_prompt_for_role(config, task, ctx)
}

/// Role-based variant of `render_prompt` — does not require `AgentType`.
pub(crate) fn render_prompt_for_role(
    config: &RoleConfig,
    task: &Task,
    ctx: &TaskContext,
) -> String {
    let (role_name, role_template) = (config.display_name, config.initial_message);

    let ac = format_acceptance_criteria(&task.acceptance_criteria);
    let labels = format_labels(&task.labels);

    // Compose: base template + role-specific template
    let mut out = format!("{BASE_TEMPLATE}\n{role_template}");
    out = out.replace("{{role_name}}", role_name);

    // Task fields
    out = out.replace("{{task_id}}", &task.id);
    out = out.replace("{{task_title}}", &task.title);
    out = out.replace("{{issue_type}}", &task.issue_type);
    out = out.replace("{{priority}}", &task.priority.to_string());
    out = out.replace("{{labels}}", &labels);
    out = out.replace("{{description}}", &task.description);
    out = out.replace("{{design}}", &task.design);
    out = out.replace("{{acceptance_criteria}}", &ac);

    // Context fields
    out = out.replace("{{project_path}}", &ctx.project_path);
    out = out.replace("{{workspace_path}}", &ctx.workspace_path);
    out = out.replace("{{diff}}", ctx.diff.as_deref().unwrap_or(""));
    out = out.replace("{{commits}}", ctx.commits.as_deref().unwrap_or(""));
    out = out.replace(
        "{{start_commit}}",
        ctx.start_commit.as_deref().unwrap_or(""),
    );
    out = out.replace("{{end_commit}}", ctx.end_commit.as_deref().unwrap_or(""));
    out = out.replace(
        "{{conflict_files}}",
        ctx.conflict_files.as_deref().unwrap_or(""),
    );
    out = out.replace(
        "{{merge_base_branch}}",
        ctx.merge_base_branch.as_deref().unwrap_or(""),
    );
    out = out.replace(
        "{{merge_target_branch}}",
        ctx.merge_target_branch.as_deref().unwrap_or(""),
    );
    out = out.replace(
        "{{merge_failure_context}}",
        ctx.merge_failure_context.as_deref().unwrap_or(""),
    );

    // Project command sections — rendered as full markdown blocks or empty string
    // so the section headings are absent when no commands are configured.
    let setup_section = match &ctx.setup_commands {
        Some(cmds) if !cmds.trim().is_empty() => format!(
            "## Automated Commands\n\nThese commands run automatically before your session starts. **Do not run them yourself.**\n\n{cmds}\n"
        ),
        _ => String::new(),
    };
    let verification_section = match &ctx.verification_commands {
        Some(cmds) if !cmds.trim().is_empty() => format!(
            "## Automated Verification\n\nBuild/test verification commands passed before review. Focus on acceptance criteria and code quality.\n\n{cmds}\n"
        ),
        _ => String::new(),
    };
    out = out.replace("{{setup_commands_section}}", &setup_section);
    out = out.replace("{{verification_section}}", &verification_section);

    let epic_context_section = match &ctx.epic_context {
        Some(text) if !text.trim().is_empty() => format!("## Epic Context\n\n{text}\n"),
        _ => String::new(),
    };
    out = out.replace("{{epic_context_section}}", &epic_context_section);

    let activity_section = match &ctx.activity {
        Some(log) if !log.trim().is_empty() => format!(
            "### Activity Log\n\nKey feedback and recent history from previous sessions. Use `task_activity_list` with filters for full details.\n\n{log}\n"
        ),
        _ => String::new(),
    };
    out = out.replace("{{activity_section}}", &activity_section);

    // Worker submission context (reviewer-facing)
    let worker_context_section = {
        let mut parts = Vec::new();
        if let Some(summary) = &ctx.worker_summary
            && !summary.trim().is_empty()
        {
            parts.push(format!("### Worker's submission notes\n\n{summary}"));
        }
        if let Some(concerns) = &ctx.worker_concerns
            && !concerns.trim().is_empty()
        {
            parts.push(format!("### Worker's remaining concerns\n\n{concerns}"));
        }
        if let Some(failure) = &ctx.verification_failure
            && !failure.trim().is_empty()
        {
            parts.push(format!(
                "### Previous verification failure\n\n{failure}\n\n\
                 **Important:** Fix this verification failure ONLY by changing files \
                 within this task's scope (as defined by the task description and design). \
                 If the failure is caused by code outside your task's scope (e.g. a \
                 pre-existing compile error in an unrelated module), do NOT modify those \
                 files. Instead, call `request_lead` to escalate — the lead can either \
                 expand your task's scope or create a separate blocking task to fix the \
                 out-of-scope issue first."
            ));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("## Worker Context\n\n{}\n", parts.join("\n\n"))
        }
    };
    out = out.replace("{{worker_context_section}}", &worker_context_section);

    let verification_rules_section = match &ctx.verification_rules {
        Some(rules) if !rules.trim().is_empty() => format!(
            "## Verification Rules\n\n\
             When running build/test commands between edits, match your changed files against \
             these rules and run the corresponding commands — not full-workspace equivalents.\n\n\
             {rules}\n"
        ),
        _ => String::new(),
    };
    out = out.replace(
        "{{verification_rules_section}}",
        &verification_rules_section,
    );

    // Hard cap: truncate the rendered system prompt to prevent context window
    // blowout when individual sections escape their soft limits.
    if out.len() > MAX_SYSTEM_PROMPT_CHARS {
        let original_len = out.len();
        out = crate::truncate::smart_truncate(&out, MAX_SYSTEM_PROMPT_CHARS);
        tracing::warn!(
            agent_type = %config.name,
            original_len,
            truncated_to = out.len(),
            "system prompt exceeded hard cap and was truncated"
        );
    }

    out
}

// ─── Role extensions ──────────────────────────────────────────────────────────

/// Append per-role prompt extensions to a fully-rendered system prompt.
///
/// Order: base rendered prompt → system_prompt_extensions → learned_prompt.
/// Empty or whitespace-only values are skipped.
/// Called by the execution layer after resolving the applicable DB agent_role.
pub fn apply_role_extensions(
    base: &str,
    system_prompt_extensions: &str,
    learned_prompt: Option<&str>,
) -> String {
    let mut out = base.to_string();
    if !system_prompt_extensions.trim().is_empty() {
        out.push_str("\n\n");
        out.push_str(system_prompt_extensions.trim());
    }
    if let Some(lp) = learned_prompt.filter(|s| !s.trim().is_empty()) {
        out.push_str("\n\n");
        out.push_str(lp.trim());
    }
    out
}

/// Append the skills section to a system prompt.
///
/// Called after `apply_role_extensions`. Appends the formatted "## Available Skills"
/// section when any skills were resolved. Returns the prompt unchanged when empty.
pub(crate) fn apply_skills(prompt: &str, skills: &[crate::skills::ResolvedSkill]) -> String {
    let section = crate::skills::format_skills_section(skills);
    if section.is_empty() {
        return prompt.to_string();
    }
    let mut out = prompt.to_string();
    out.push_str("\n\n");
    out.push_str(&section);
    out
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Format a JSON acceptance-criteria array as a markdown checklist.
///
/// Input: `[{"criterion": "...", "met": false}, ...]`
/// Output: `- [ ] ...\n- [x] ...\n`
fn format_acceptance_criteria(json: &str) -> String {
    #[derive(Deserialize)]
    struct Criterion {
        criterion: String,
        #[serde(default)]
        met: bool,
    }

    let Ok(criteria) = serde_json::from_str::<Vec<Criterion>>(json) else {
        return json.to_string();
    };

    criteria
        .into_iter()
        .map(|c| {
            let box_char = if c.met { "x" } else { " " };
            format!("- [{box_char}] {}", c.criterion)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Format a JSON label array as a comma-separated string.
///
/// Input: `["wave:1", "tech-debt"]`
/// Output: `wave:1, tech-debt`
fn format_labels(json: &str) -> String {
    djinn_core::models::parse_json_array(json).join(", ")
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_task() -> Task {
        Task {
            id: "task-123".into(),
            project_id: "project-1".into(),
            short_id: "t123".into(),
            epic_id: Some("epic-1".into()),
            title: "Add widget".into(),
            description: "Implement the widget feature.".into(),
            design: "Use the widget pattern.".into(),
            issue_type: "task".into(),
            status: "open".into(),
            priority: 1,
            owner: "dev@example.com".into(),
            labels: r#"["wave:1"]"#.into(),
            acceptance_criteria: r#"[{"criterion":"Widget exists","met":false},{"criterion":"Tests pass","met":true}]"#.into(),
            reopen_count: 0,
            continuation_count: 0,
            verification_failure_count: 0,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            closed_at: None,
            close_reason: None,
            merge_commit_sha: None,
            pr_url: None,
            merge_conflict_metadata: None,
            memory_refs: "[]".into(),
            agent_type: None,
            unresolved_blocker_count: 0,
            total_reopen_count: 0,
            total_verification_failure_count: 0,
            intervention_count: 0,
            last_intervention_at: None,
        }
    }

    fn make_ctx() -> TaskContext {
        TaskContext {
            project_path: "/home/user/project".into(),
            workspace_path: "/home/user/project/.djinn/worktrees/t123".into(),
            diff: None,
            commits: None,
            start_commit: None,
            end_commit: None,
            conflict_files: None,
            merge_base_branch: None,
            merge_target_branch: None,
            merge_failure_context: None,
            setup_commands: None,
            verification_commands: None,
            verification_rules: None,
            activity: None,
            worker_summary: None,
            worker_concerns: None,
            verification_failure: None,
            epic_context: None,
        }
    }

    #[test]
    fn worker_prompt_contains_task_fields() {
        let task = make_task();
        let ctx = make_ctx();
        let prompt = render_prompt(AgentType::Worker, &task, &ctx);

        assert!(prompt.contains("task-123"));
        assert!(prompt.contains("Add widget"));
        assert!(prompt.contains("Implement the widget feature."));
        assert!(prompt.contains("Use the widget pattern."));
        assert!(prompt.contains("wave:1"));
        assert!(prompt.contains("- [ ] Widget exists"));
        assert!(prompt.contains("- [x] Tests pass"));
        assert!(prompt.contains("/home/user/project"));
        assert!(prompt.contains("/home/user/project/.djinn/worktrees/t123"));
        // No un-substituted placeholders
        assert!(!prompt.contains("{{"));
    }

    #[test]
    fn task_reviewer_prompt_contains_task_fields() {
        let task = make_task();
        let ctx = make_ctx();
        let prompt = render_prompt(AgentType::Reviewer, &task, &ctx);

        // Task ID and title are substituted.
        assert!(prompt.contains(&task.id));
        // The reviewer is instructed to run git diff itself, not receive it injected.
        assert!(prompt.contains("git diff"));
        // Reviewer uses AC state for verdict.
        assert!(prompt.contains("task_update"));
        assert!(!prompt.contains("{{"));
    }

    #[test]
    fn format_acceptance_criteria_invalid_json_passthrough() {
        let result = format_acceptance_criteria("not json");
        assert_eq!(result, "not json");
    }

    #[test]
    fn format_labels_empty_array() {
        assert_eq!(format_labels("[]"), "");
    }

    #[test]
    fn worker_prompt_includes_setup_commands_when_present() {
        let task = make_task();
        let ctx = TaskContext {
            setup_commands: Some("- `npm install`\n- `npm run build`".into()),
            verification_commands: Some("- `npm test`".into()),
            ..make_ctx()
        };
        let prompt = render_prompt(AgentType::Worker, &task, &ctx);

        assert!(prompt.contains("Automated Commands"));
        assert!(prompt.contains("Do not run them yourself"));
        assert!(prompt.contains("npm install"));
        assert!(!prompt.contains("{{setup_commands_section}}"));
        assert!(!prompt.contains("{{verification_section}}"));
    }

    #[test]
    fn worker_prompt_omits_setup_section_when_no_commands() {
        let task = make_task();
        let ctx = make_ctx(); // setup_commands: None
        let prompt = render_prompt(AgentType::Worker, &task, &ctx);

        assert!(!prompt.contains("Automated Commands"));
        assert!(!prompt.contains("{{setup_commands_section}}"));
    }

    #[test]
    fn reviewer_prompt_includes_verification_section_when_present() {
        let task = make_task();
        let ctx = TaskContext {
            diff: Some("+ fn foo() {}".into()),
            commits: Some("abc1234 Add widget".into()),
            start_commit: Some("abc0000".into()),
            end_commit: Some("abc1234".into()),
            verification_commands: Some("- `cargo test`".into()),
            ..make_ctx()
        };
        let prompt = render_prompt(AgentType::Reviewer, &task, &ctx);

        assert!(prompt.contains("Automated Verification"));
        assert!(prompt.contains("Focus on acceptance criteria"));
        assert!(prompt.contains("cargo test"));
        assert!(!prompt.contains("{{verification_section}}"));
    }

    #[test]
    fn reviewer_prompt_omits_verification_section_when_no_commands() {
        let task = make_task();
        let ctx = TaskContext {
            diff: Some("+ fn foo() {}".into()),
            commits: Some("abc1234 Add widget".into()),
            start_commit: Some("abc0000".into()),
            end_commit: Some("abc1234".into()),
            ..make_ctx()
        };
        let prompt = render_prompt(AgentType::Reviewer, &task, &ctx);

        assert!(!prompt.contains("Automated Verification"));
        assert!(!prompt.contains("{{verification_section}}"));
    }

    #[test]
    fn system_prompt_truncated_when_exceeding_hard_cap() {
        let task = make_task();
        // Inject a massive activity log that blows past the 30k char cap.
        let huge_activity = "x".repeat(40_000);
        let ctx = TaskContext {
            activity: Some(huge_activity),
            ..make_ctx()
        };
        let prompt = render_prompt(AgentType::Worker, &task, &ctx);

        assert!(
            prompt.len() <= super::MAX_SYSTEM_PROMPT_CHARS + 200, // +200 for the truncation notice
            "prompt should be truncated to ~30k chars, got {}",
            prompt.len()
        );
        // smart_truncate uses "bytes omitted" or "truncated" markers
        assert!(prompt.contains("omitted") || prompt.contains("truncated"));
    }

    #[test]
    fn system_prompt_not_truncated_when_under_cap() {
        let task = make_task();
        let ctx = make_ctx();
        let prompt = render_prompt(AgentType::Worker, &task, &ctx);

        assert!(!prompt.contains("omitted"));
    }

    #[test]
    fn worker_prompt_includes_merge_failure_context() {
        let task = make_task();
        let ctx = TaskContext {
            merge_failure_context: Some(
                "**Merge Conflict Detected**\n\nFile `src/main.rs` has conflicts.".into(),
            ),
            ..make_ctx()
        };
        let prompt = render_prompt(AgentType::Worker, &task, &ctx);

        assert!(
            prompt.contains("Merge Conflict Detected"),
            "worker prompt should include merge failure context"
        );
        assert!(
            !prompt.contains("{{merge_failure_context}}"),
            "template placeholder should be replaced"
        );
    }

    #[test]
    fn worker_prompt_includes_conflict_files_for_conflict_context() {
        let task = make_task();
        let ctx = TaskContext {
            conflict_files: Some("- src/main.rs\n- src/lib.rs".into()),
            merge_base_branch: Some("task/abc123".into()),
            merge_target_branch: Some("main".into()),
            ..make_ctx()
        };
        let prompt = render_prompt(AgentType::Worker, &task, &ctx);

        assert!(
            prompt.contains("src/main.rs"),
            "worker prompt should include conflict files"
        );
        assert!(
            prompt.contains("task/abc123"),
            "worker prompt should include merge base branch"
        );
        assert!(
            prompt.contains("main"),
            "worker prompt should include merge target branch"
        );
    }

    #[test]
    fn worker_prompt_includes_verification_rules_when_present() {
        let task = make_task();
        let ctx = TaskContext {
            verification_rules: Some(
                "- `crates/djinn-mcp/**`: `cargo test -p djinn-mcp`, `cargo clippy -p djinn-mcp -- -D warnings`".into(),
            ),
            ..make_ctx()
        };
        let prompt = render_prompt(AgentType::Worker, &task, &ctx);

        assert!(
            prompt.contains("Verification Rules"),
            "worker prompt should include verification rules section heading"
        );
        assert!(
            prompt.contains("crates/djinn-mcp/**"),
            "worker prompt should include rule pattern"
        );
        assert!(
            prompt.contains("cargo test -p djinn-mcp"),
            "worker prompt should include rule commands"
        );
        assert!(!prompt.contains("{{verification_rules_section}}"));
    }

    #[test]
    fn worker_prompt_omits_verification_rules_when_empty() {
        let task = make_task();
        let ctx = make_ctx(); // verification_rules: None
        let prompt = render_prompt(AgentType::Worker, &task, &ctx);

        assert!(
            !prompt.contains("Verification Rules"),
            "worker prompt should not include verification rules section when empty"
        );
        assert!(!prompt.contains("{{verification_rules_section}}"));
    }

    #[test]
    fn worker_prompt_contains_scoped_build_guidance() {
        let task = make_task();
        let ctx = make_ctx();
        let prompt = render_prompt(AgentType::Worker, &task, &ctx);

        assert!(
            prompt.contains("scoped"),
            "worker prompt should mention scoped build/check commands"
        );
    }

    #[test]
    fn architect_prompt_contains_memory_health_review() {
        let task = make_task();
        let ctx = make_ctx();
        let prompt = render_prompt(AgentType::Architect, &task, &ctx);

        assert!(
            prompt.contains("Memory Health Review"),
            "architect prompt should include memory health review section"
        );
        assert!(
            prompt.contains("memory_health()"),
            "architect prompt should reference memory_health tool"
        );
        assert!(
            prompt.contains("memory_broken_links()"),
            "architect prompt should reference memory_broken_links tool"
        );
        assert!(
            prompt.contains("memory_orphans()"),
            "architect prompt should reference memory_orphans tool"
        );
    }

    #[test]
    fn architect_prompt_contains_contradiction_review() {
        let task = make_task();
        let ctx = make_ctx();
        let prompt = render_prompt(AgentType::Architect, &task, &ctx);

        assert!(
            prompt.contains("Contradiction and Low-Confidence Review"),
            "architect prompt should include contradiction review section"
        );
        assert!(
            prompt.contains("contradicts supersedes stale"),
            "architect prompt should instruct searching for contradictions"
        );
        assert!(
            prompt.contains("canonical"),
            "architect prompt should mention canonicalization of conflicting notes"
        );
    }
}
