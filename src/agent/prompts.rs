// Embedded prompt templates for Djinn agent types.
//
// Templates are compiled into the binary via include_str!() and rendered with
// simple {{variable}} string substitution. A shared base template provides
// system identity, task context, workspace config, and common tools. Each role
// template appends role-specific mission, instructions, and rules.

use serde::Deserialize;

use super::AgentType;
use crate::models::task::Task;

// ─── Embedded templates ────────────────────────────────────────────────────────

const BASE_TEMPLATE: &str = include_str!("prompts/base.md");
const DEV_TEMPLATE: &str = include_str!("prompts/dev.md");
const CONFLICT_RESOLVER_TEMPLATE: &str = include_str!("prompts/conflict-resolver.md");
const TASK_REVIEWER_TEMPLATE: &str = include_str!("prompts/task-reviewer.md");
const PM_TEMPLATE: &str = include_str!("prompts/pm.md");

// ─── Context ───────────────────────────────────────────────────────────────────

/// Runtime context injected alongside the task's stored fields at render time.
///
/// Worker agents need `project_path` and `workspace_path`. Reviewer agents
/// additionally use the workspace to inspect code. Conflict resolvers supply
/// merge context.
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

    // ── Activity log ─────────────────────────────────────────────────────
    /// Pre-formatted activity log (comments, transitions) for the task.
    pub activity: Option<String>,
}

// ─── Renderer ─────────────────────────────────────────────────────────────────

/// Render a system prompt for `agent_type` using data from `task` and `ctx`.
///
/// Returns a plain `String` ready for `agent.set_system_prompt_override()`.
pub fn render_prompt(agent_type: AgentType, task: &Task, ctx: &TaskContext) -> String {
    let (role_name, role_template) = match agent_type {
        AgentType::Worker => ("Developer", DEV_TEMPLATE),
        AgentType::ConflictResolver => ("Conflict Resolver", CONFLICT_RESOLVER_TEMPLATE),
        AgentType::TaskReviewer => ("Task Reviewer", TASK_REVIEWER_TEMPLATE),
        AgentType::PM => ("PM Intervention", PM_TEMPLATE),
    };

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

    let activity_section = match &ctx.activity {
        Some(log) if !log.trim().is_empty() => format!(
            "### Activity Log\n\nComments and events from previous agent sessions on this task. You do not need to call `task_show` — this context is already complete.\n\n{log}\n"
        ),
        _ => String::new(),
    };
    out = out.replace("{{activity_section}}", &activity_section);

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
    crate::models::parse_json_array(json).join(", ")
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
            memory_refs: "[]".into(),
            unresolved_blocker_count: 0,
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
            activity: None,
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
        let prompt = render_prompt(AgentType::TaskReviewer, &task, &ctx);

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
        let prompt = render_prompt(AgentType::TaskReviewer, &task, &ctx);

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
        let prompt = render_prompt(AgentType::TaskReviewer, &task, &ctx);

        assert!(!prompt.contains("Automated Verification"));
        assert!(!prompt.contains("{{verification_section}}"));
    }
}
