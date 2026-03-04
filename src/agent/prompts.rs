// Embedded prompt templates for Goose agent types.
//
// Templates are compiled into the binary via include_str!() and rendered with
// simple {{variable}} string substitution. The output is passed directly to
// Goose's set_system_prompt_override().

use serde::Deserialize;

use super::AgentType;
use crate::models::task::Task;

// ─── Embedded templates ────────────────────────────────────────────────────────

const DEV_TEMPLATE: &str = include_str!("prompts/dev.md");
const CONFLICT_RESOLVER_TEMPLATE: &str = include_str!("prompts/conflict-resolver.md");
const TASK_REVIEWER_TEMPLATE: &str = include_str!("prompts/task-reviewer.md");
const EPIC_REVIEWER_TEMPLATE: &str = include_str!("prompts/epic-reviewer-batch.md");

// ─── Context ───────────────────────────────────────────────────────────────────

/// Runtime context injected alongside the task's stored fields at render time.
///
/// Worker agents only need `project_path`. Reviewer agents additionally supply
/// commit range and diff. Epic reviewer agents supply batch metadata.
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

    // ── Epic reviewer batch fields ────────────────────────────────────────────
    pub batch_num: Option<u32>,
    pub task_count: Option<u32>,
    /// Each line: "<merge_sha> <short_id>: <title>" — agent runs `git show <sha>` per entry.
    /// SHAs are per-task squash-merge commits; no contiguous range exists since other
    /// epics' commits may be interleaved on the same branch.
    pub tasks_summary: Option<String>,
    pub common_labels: Option<String>,

    // -- Conflict resolver fields --------------------------------------------
    pub conflict_files: Option<String>,
    pub merge_base_branch: Option<String>,
    pub merge_target_branch: Option<String>,
}

// ─── Renderer ─────────────────────────────────────────────────────────────────

/// Render a system prompt for `agent_type` using data from `task` and `ctx`.
///
/// Returns a plain `String` ready for `agent.set_system_prompt_override()`.
pub fn render_prompt(agent_type: AgentType, task: &Task, ctx: &TaskContext) -> String {
    let template = match agent_type {
        AgentType::Worker => DEV_TEMPLATE,
        AgentType::ConflictResolver => CONFLICT_RESOLVER_TEMPLATE,
        AgentType::TaskReviewer => TASK_REVIEWER_TEMPLATE,
        AgentType::EpicReviewer => EPIC_REVIEWER_TEMPLATE,
    };

    let ac = format_acceptance_criteria(&task.acceptance_criteria);
    let labels = format_labels(&task.labels);

    let mut out = template.to_string();

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
        "{{batch_num}}",
        &ctx.batch_num.map(|n| n.to_string()).unwrap_or_default(),
    );
    out = out.replace(
        "{{task_count}}",
        &ctx.task_count.map(|n| n.to_string()).unwrap_or_default(),
    );
    out = out.replace(
        "{{tasks_summary}}",
        ctx.tasks_summary.as_deref().unwrap_or(""),
    );
    out = out.replace(
        "{{common_labels}}",
        ctx.common_labels.as_deref().unwrap_or(""),
    );
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
    let Ok(labels) = serde_json::from_str::<Vec<String>>(json) else {
        return String::new();
    };
    labels.join(", ")
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
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            closed_at: None,
            blocked_from_status: None,
            close_reason: None,
            merge_commit_sha: None,
            memory_refs: "[]".into(),
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
            batch_num: None,
            task_count: None,
            tasks_summary: None,
            common_labels: None,
            conflict_files: None,
            merge_base_branch: None,
            merge_target_branch: None,
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
    fn task_reviewer_prompt_contains_diff() {
        let task = make_task();
        let ctx = TaskContext {
            diff: Some("+ fn foo() {}".into()),
            commits: Some("abc1234 Add widget".into()),
            start_commit: Some("abc0000".into()),
            end_commit: Some("abc1234".into()),
            ..make_ctx()
        };
        let prompt = render_prompt(AgentType::TaskReviewer, &task, &ctx);

        assert!(prompt.contains("+ fn foo() {}"));
        assert!(prompt.contains("abc0000..abc1234"));
        assert!(!prompt.contains("{{"));
    }

    #[test]
    fn epic_reviewer_prompt_contains_batch_meta() {
        let task = make_task();
        let ctx = TaskContext {
            batch_num: Some(2),
            task_count: Some(5),
            common_labels: Some("wave:1".into()),
            tasks_summary: Some("abc1234 lnfo: Add widget\ndef5678 rvmf: Fix regression".into()),
            ..make_ctx()
        };
        let prompt = render_prompt(AgentType::EpicReviewer, &task, &ctx);

        assert!(prompt.contains("Batch: 2") || prompt.contains("**Batch:** 2"));
        assert!(prompt.contains("abc1234 lnfo: Add widget"));
        assert!(prompt.contains("git show <sha>"));
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
}
