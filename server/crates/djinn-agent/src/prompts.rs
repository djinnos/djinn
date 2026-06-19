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
/// PR F4: cluster-doc synthesis template. Currently shipped but not yet
/// wired to an LLM call site — `djinn-graph::cluster_doc` writes a
/// deterministic placeholder body until the agent runtime exposes a
/// generic "render this prompt with these slots" entry point. Slots:
/// `{{MODULE_NAME}}`, `{{INTRA_CALLS}}`, `{{OUTGOING_CALLS}}`,
/// `{{TOP_PROCESSES}}`, `{{CHILDREN_DOCS}}`, `{{PROJECT_INFO}}`.
pub const CLUSTER_DOC_TEMPLATE: &str = include_str!("prompts/cluster-doc.md");

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

    // ── Activity log ─────────────────────────────────────────────────────
    /// Pre-formatted activity log (comments, transitions) for the task.
    pub activity: Option<String>,

    // ── Worker submission context (for reviewer) ─────────────────────────
    /// Summary from the last `work_submitted` activity entry.
    pub worker_summary: Option<String>,
    /// Remaining concerns from the last `work_submitted` activity entry.
    pub worker_concerns: Option<String>,

    // ── Epic context ─────────────────────────────────────────────────────
    /// Epic context section for lead agents (title, description, memory_refs, sibling tasks).
    pub epic_context: Option<String>,

    // ── Knowledge context ────────────────────────────────────────────────
    /// Path-scoped knowledge notes relevant to this task's code areas.
    pub knowledge_context: Option<String>,

    // ── Code graph context (PR E2) ───────────────────────────────────────
    /// Auto-injected `code_graph context` summary for worker / reviewer roles
    /// whose tasks touch known files in the canonical graph. `None` when the
    /// role is not in the `DJINN_AUTO_CODE_CONTEXT_ROLES` allowlist or when
    /// the task has no resolvable scope-path symbols. Capped at 2000 chars
    /// via `truncate::smart_truncate`.
    pub code_graph_context: Option<String>,

    // ── Reviewer diff context (PR E3) ────────────────────────────────────
    /// Auto-injected `code_graph detect_changes` summary for reviewer roles.
    /// One bullet per touched symbol with `impact`-derived risk + direct
    /// caller / module counts; sorted CRITICAL → HIGH → MEDIUM → LOW. `None`
    /// when the reviewer role is not in the `DJINN_AUTO_CODE_CONTEXT_ROLES`
    /// allowlist, when no base/head SHAs could be resolved, or when the
    /// detected change set is empty. Capped at 2000 chars via
    /// `truncate::smart_truncate`.
    pub reviewer_diff_context: Option<String>,
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

    // Mode-specific section: the dispatcher (code, not the LLM) selects which
    // workflow this stage runs and we inject ONLY that section. Single-mode
    // roles leave `mode_section` None and have no `{{role_mode_section}}`
    // placeholder, so this is a harmless no-op for them.
    let mode_section = config
        .mode_section
        .map(|select| select(task, ctx))
        .unwrap_or("");
    out = out.replace("{{role_mode_section}}", mode_section);

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
    out = out.replace("{{setup_commands_section}}", &setup_section);

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

    // PR E2: auto-injected `code_graph context` summary for worker/reviewer
    // roles. Emits an empty string when `None` (per inter-PR contract).
    let code_graph_context_section = match &ctx.code_graph_context {
        Some(text) if !text.trim().is_empty() => format!("## Code Graph Context\n\n{text}\n"),
        _ => String::new(),
    };
    out = out.replace(
        "{{code_graph_context_section}}",
        &code_graph_context_section,
    );

    // PR E3: auto-injected `code_graph detect_changes` summary for reviewer
    // roles. Emits an empty string when `None` so reviewer prompts that
    // don't have base/head SHAs (or aren't in the allowlist) don't show a
    // dangling section header.
    let reviewer_diff_context_section = match &ctx.reviewer_diff_context {
        Some(text) if !text.trim().is_empty() => format!("## Changed Symbols\n\n{text}\n"),
        _ => String::new(),
    };
    out = out.replace(
        "{{reviewer_diff_context_section}}",
        &reviewer_diff_context_section,
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
        if parts.is_empty() {
            String::new()
        } else {
            format!("## Worker Context\n\n{}\n", parts.join("\n\n"))
        }
    };
    out = out.replace("{{worker_context_section}}", &worker_context_section);

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
mod tests;
