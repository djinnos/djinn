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

    // -- Merge conflict context (handled by Worker) ----------------------------
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

    // ── Knowledge context ────────────────────────────────────────────────
    /// Path-scoped knowledge notes relevant to this task's code areas.
    pub knowledge_context: Option<String>,

    // ── Planner patrol context ───────────────────────────────────────────
    /// Planner-patrol-only summary of code-graph diffs and undocumented hotspots.
    pub planner_patrol_context: Option<String>,
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

    // Dynamic tools section from role schemas
    let tools_md = format_tools_section(&(config.tool_schemas)());
    out = out.replace("{{tools_section}}", &tools_md);

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

    let knowledge_context_section = match &ctx.knowledge_context {
        Some(text) if !text.trim().is_empty() => format!(
            "## Relevant Knowledge\n\n\
             The following patterns, pitfalls, and cases were learned from previous work \
             in the code areas this task touches.\n\n{text}\n"
        ),
        _ => String::new(),
    };
    out = out.replace("{{knowledge_context_section}}", &knowledge_context_section);

    let planner_patrol_context_section = match &ctx.planner_patrol_context {
        Some(text) if !text.trim().is_empty() => format!("## Planner Patrol Context\n\n{text}\n"),
        _ => String::new(),
    };
    out = out.replace(
        "{{planner_patrol_context_section}}",
        &planner_patrol_context_section,
    );

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
pub fn apply_skills(prompt: &str, skills: &[crate::skills::ResolvedSkill]) -> String {
    let section = crate::skills::format_skills_section(skills);
    if section.is_empty() {
        return prompt.to_string();
    }
    let mut out = prompt.to_string();
    out.push_str("\n\n");
    out.push_str(&section);
    out
}

// ─── Tool section generator ───────────────────────────────────────────────

/// Generate a markdown tools reference from serialized tool schemas.
///
/// Each schema is expected to have `name`, `description`, and `inputSchema`
/// (with `properties` and optionally `required`). Output is a bullet list:
///
/// ```text
/// - `tool_name(required_param, optional_param?)` — Description text.
/// ```
fn format_tools_section(schemas: &[serde_json::Value]) -> String {
    let mut lines = Vec::with_capacity(schemas.len());
    for schema in schemas {
        let name = schema["name"].as_str().unwrap_or("unknown");
        let desc = schema["description"].as_str().unwrap_or("");
        let input = &schema["inputSchema"];
        let required: Vec<&str> = input["required"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        // Collect parameter names in a stable order: required first, then optional.
        let mut req_params = Vec::new();
        let mut opt_params = Vec::new();
        if let Some(props) = input["properties"].as_object() {
            // Sort keys for deterministic output.
            let mut keys: Vec<&String> = props.keys().collect();
            keys.sort();
            for key in keys {
                if required.contains(&key.as_str()) {
                    req_params.push(key.as_str());
                } else {
                    opt_params.push(format!("{key}?"));
                }
            }
        }

        let mut params: Vec<String> = req_params.iter().map(|s| s.to_string()).collect();
        params.extend(opt_params);
        let sig = params.join(", ");

        lines.push(format!("- `{name}({sig})` — {desc}"));
    }
    lines.join("\n")
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
            knowledge_context: None,
            planner_patrol_context: None,
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
        assert!(prompt.contains("issue_type` is `research`"));
        assert!(prompt.contains(".djinn/memory/"));
        assert!(prompt.contains("write`/`edit`/`apply_patch`"));
        assert!(prompt.contains("Originated from task task-123"));
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

    /// Per ADR-051 §1 the memory-health review moved from Architect to Planner
    /// (patrol mode). This test now asserts the content lives on the Planner
    /// prompt side.
    #[test]
    fn planner_patrol_prompt_contains_memory_health_review() {
        let task = make_task();
        let ctx = make_ctx();
        let prompt = render_prompt(AgentType::Planner, &task, &ctx);

        assert!(
            prompt.contains("Memory Health Review"),
            "planner prompt should include memory health review section (patrol mode)"
        );
        assert!(
            prompt.contains("memory_health()"),
            "planner prompt should reference memory_health tool"
        );
        assert!(
            prompt.contains("memory_broken_links()"),
            "planner prompt should reference memory_broken_links tool"
        );
        assert!(
            prompt.contains("memory_orphans()"),
            "planner prompt should reference memory_orphans tool"
        );
        assert!(
            prompt.contains("planning task"),
            "planner prompt should direct memory-health follow-ups through planning tasks"
        );
        assert!(
            prompt.contains("Knowledge Task Guard Rails"),
            "planner prompt should explicitly mention patrol knowledge-task guard rails"
        );
        assert!(
            prompt.contains("suppress the duplicate instead of creating another one"),
            "planner prompt should tell patrol to suppress similar open knowledge tasks"
        );
        assert!(
            prompt.contains("stop once the patrol budget is exhausted"),
            "planner prompt should enforce the knowledge-task budget"
        );
    }

    /// Per ADR-051 §1 the contradiction review moved from Architect to Planner
    /// (patrol mode). The architect prompt still asks for spike-note task
    /// traceability, which this test also asserts below.
    #[test]
    fn planner_patrol_prompt_contains_contradiction_review() {
        let task = make_task();
        let ctx = make_ctx();
        let prompt = render_prompt(AgentType::Planner, &task, &ctx);

        assert!(
            prompt.contains("Contradiction and Low-Confidence Review"),
            "planner prompt should include contradiction review section"
        );
        assert!(
            prompt.contains("contradicts supersedes stale"),
            "planner prompt should instruct searching for contradictions"
        );
        assert!(
            prompt.contains("canonical"),
            "planner prompt should mention canonicalization of conflicting notes"
        );
        assert!(
            prompt.contains("planning task to deprecate the outdated note"),
            "planner prompt should prescribe planning-task routing for contradiction resolution"
        );
    }

    #[test]
    fn planner_prompt_includes_patrol_context_section_when_present() {
        let task = make_task();
        let ctx = TaskContext {
            planner_patrol_context: Some(
                "### Memory Health Signals\n- Notes: 2 total, 1 low-confidence\n\n### Code Graph Diff Summary\n\nNew modules: `server/src/new_area.rs`\n\n### Knowledge Coverage Gaps\n- Stale scoped-note areas affected by changed code: `server/src/new_area.rs`".into(),
            ),
            ..make_ctx()
        };
        let prompt = render_prompt(AgentType::Planner, &task, &ctx);

        assert!(prompt.contains("Planner Patrol Context"));
        assert!(prompt.contains("Code Graph Diff Summary"));
        assert!(prompt.contains("New modules:"));
    }

    /// Architect spike notes must still carry task traceability (per ADR-051
    /// Contract 2 / §9 "Spike and Research Findings — Memory Writes").
    #[test]
    fn architect_prompt_requires_spike_note_traceability() {
        let task = make_task();
        let ctx = make_ctx();
        let prompt = render_prompt(AgentType::Architect, &task, &ctx);

        assert!(
            prompt.contains("Originated from task task-123"),
            "architect prompt should require task traceability in persisted spike notes"
        );
        assert!(
            prompt.contains("task objective"),
            "architect prompt should ask for enough context to explain why a memory note exists"
        );
    }

    #[test]
    fn architect_prompt_requires_read_back_verification_before_file_comments() {
        let task = make_task();
        let ctx = make_ctx();
        let prompt = render_prompt(AgentType::Architect, &task, &ctx);

        assert!(
            prompt.contains("Never use it to claim a file exists, was copied, or was moved until you have read that exact path back successfully in the current session"),
            "architect prompt should forbid file-existence comments before read-back verification"
        );
        assert!(
            prompt.contains("Never add a task comment claiming a file exists, was copied, or was moved unless you have just verified that exact path by reading it back successfully"),
            "architect prompt should require read-back verification immediately before file-placement comments"
        );
    }

    // ── Tools section snapshot tests ─────────────────────────────────────────

    #[test]
    fn worker_tools_section_snapshot() {
        let schemas = (AgentType::Worker.role_config().tool_schemas)();
        let section = format_tools_section(&schemas);
        insta::assert_snapshot!(section);
    }

    #[test]
    fn reviewer_tools_section_snapshot() {
        let schemas = (AgentType::Reviewer.role_config().tool_schemas)();
        let section = format_tools_section(&schemas);
        insta::assert_snapshot!(section);
    }

    #[test]
    fn lead_tools_section_snapshot() {
        let schemas = (AgentType::Lead.role_config().tool_schemas)();
        let section = format_tools_section(&schemas);
        insta::assert_snapshot!(section);
    }

    #[test]
    fn planner_tools_section_snapshot() {
        let schemas = (AgentType::Planner.role_config().tool_schemas)();
        let section = format_tools_section(&schemas);
        insta::assert_snapshot!(section);
    }

    #[test]
    fn architect_tools_section_snapshot() {
        let schemas = (AgentType::Architect.role_config().tool_schemas)();
        let section = format_tools_section(&schemas);
        insta::assert_snapshot!(section);
    }

    #[test]
    fn tools_section_injected_into_rendered_prompt() {
        let task = make_task();
        let ctx = make_ctx();

        // Verify each role's prompt contains its tools and NOT other roles' tools.
        let worker_prompt = render_prompt(AgentType::Worker, &task, &ctx);
        assert!(
            worker_prompt.contains("`submit_work("),
            "worker prompt should contain submit_work"
        );
        assert!(
            worker_prompt.contains("`github_search("),
            "worker prompt should contain github_search"
        );
        assert!(
            !worker_prompt.contains("`submit_review("),
            "worker prompt should NOT contain submit_review"
        );
        assert!(
            !worker_prompt.contains("{{tools_section}}"),
            "tools_section placeholder should be replaced"
        );

        let reviewer_prompt = render_prompt(AgentType::Reviewer, &task, &ctx);
        assert!(
            reviewer_prompt.contains("`submit_review("),
            "reviewer prompt should contain submit_review"
        );
        assert!(
            reviewer_prompt.contains("`github_search("),
            "reviewer prompt should contain github_search"
        );
        assert!(
            !reviewer_prompt.contains("`submit_work("),
            "reviewer prompt should NOT contain submit_work"
        );

        let lead_prompt = render_prompt(AgentType::Lead, &task, &ctx);
        assert!(
            lead_prompt.contains("`submit_decision("),
            "lead prompt should contain submit_decision"
        );
        assert!(
            lead_prompt.contains("`task_create("),
            "lead prompt should contain task_create"
        );
        assert!(
            !lead_prompt.contains("`submit_work("),
            "lead prompt should NOT contain submit_work"
        );

        let planner_prompt = render_prompt(AgentType::Planner, &task, &ctx);
        assert!(
            planner_prompt.contains("`submit_grooming("),
            "planner prompt should contain submit_grooming"
        );
        assert!(
            planner_prompt.contains("`epic_tasks("),
            "planner prompt should contain epic_tasks"
        );

        let architect_prompt = render_prompt(AgentType::Architect, &task, &ctx);
        assert!(
            architect_prompt.contains("`submit_work("),
            "architect prompt should contain submit_work"
        );
        assert!(
            architect_prompt.contains("`memory_health("),
            "architect prompt should contain memory_health"
        );
        // Per ADR-051 §1 `role_amend_prompt` moved from Architect to Planner
        // (agent-effectiveness review is a patrol action, not a consultant
        // action). Architect keeps `role_metrics` (read) and `role_create`
        // (structural proposal) but cannot mutate existing learned_prompts.
        assert!(
            !architect_prompt.contains("`agent_amend_prompt("),
            "architect prompt should NOT contain agent_amend_prompt — it moved to Planner per ADR-051"
        );
        assert!(
            planner_prompt.contains("`agent_amend_prompt("),
            "planner prompt should contain agent_amend_prompt — it moved here per ADR-051"
        );
    }
}
