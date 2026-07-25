//! Prompt templates, rendering, and context types for Djinn agent roles.
//!
//! Templates are compiled into the binary via `include_str!()` and rendered
//! with simple `{{variable}}` string substitution.  A shared base template
//! provides system identity, task context, workspace config, and common tools.
//! Each role template appends role-specific mission, instructions, and rules.

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::RoleConfig;
use djinn_core::models::Task;

/// Hard cap on rendered system prompt size (chars). Individual sections have
/// their own soft limits, but this catches cases where multiple sections
/// combine to blow past a reasonable size.
///
/// Sized to fit the largest legitimate prompt — the Planner decomposition mode
/// (`base.md` + `planner.md` + `planner/decomposition.md` + tool schemas + task
/// fields), which is ~35K after the planner prompt-contract guidance landed. The
/// old 31K cap silently truncated the tail of `decomposition.md` (the
/// "prune unverifiable AC instead of escalating" contract), so the planner never
/// saw guidance it was explicitly given. All in-use models have >=128K context,
/// so a ~48K system prompt is comfortably within budget.
pub const MAX_SYSTEM_PROMPT_CHARS: usize = 48_000;

// ─── Embedded templates ────────────────────────────────────────────────────────

pub const BASE_TEMPLATE: &str = include_str!("base.md");
pub const DEV_TEMPLATE: &str = include_str!("dev.md");
pub const REVIEWER_TEMPLATE: &str = include_str!("task-reviewer.md");
pub const LEAD_TEMPLATE: &str = include_str!("lead.md");
pub const PLANNER_TEMPLATE: &str = include_str!("planner.md");
pub const ARCHITECT_TEMPLATE: &str = include_str!("architect.md");
/// PR F4: cluster-doc synthesis template.
pub const CLUSTER_DOC_TEMPLATE: &str = include_str!("cluster-doc.md");
/// Proposal refinement tribunal roles (k9zw).
pub const ADVOCATE_TEMPLATE: &str = include_str!("advocate.md");
pub const ADVERSARY_TEMPLATE: &str = include_str!("adversary.md");
pub const JUDGE_TEMPLATE: &str = include_str!("judge.md");

// ─── Mode section constants ────────────────────────────────────────────────────

pub(crate) const WORKER_RESEARCH: &str = include_str!("worker/research.md");
pub(crate) const WORKER_CONFLICT: &str = include_str!("worker/conflict.md");

pub(crate) const PLANNER_DECOMPOSITION: &str = include_str!("planner/decomposition.md");
pub(crate) const PLANNER_INTERVENTION: &str = include_str!("planner/intervention.md");
pub(crate) const PLANNER_PROPOSAL: &str = include_str!("planner/proposal.md");
pub(crate) const PLANNER_PROPOSAL_REVIEW: &str = include_str!("planner/proposal_review.md");

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
    /// via `smart_truncate`.
    pub code_graph_context: Option<String>,

    // ── Reviewer diff context (PR E3) ────────────────────────────────────
    /// Auto-injected `code_graph detect_changes` summary for reviewer roles.
    /// One bullet per touched symbol with `impact`-derived risk + direct
    /// caller / module counts; sorted CRITICAL → HIGH → MEDIUM → LOW. `None`
    /// when the reviewer role is not in the `DJINN_AUTO_CODE_CONTEXT_ROLES`
    /// allowlist, when no base/head SHAs could be resolved, or when the
    /// detected change set is empty. Capped at 2000 chars via `smart_truncate`.
    pub reviewer_diff_context: Option<String>,

    // ── Red-CI blocking directive (sa4x) ──────────────────────────────────────
    /// Promoted BLOCKING directive for red required CI. Contains concrete
    /// PR number, failing head SHA, blocking check/job names, and failure
    /// fingerprint. Deliberately separate from ordinary activity-log prose.
    /// `None` when CI is not failing, when no remediation baseline exists,
    /// or when failures are advisory-only.
    pub ci_blocking_directive: Option<String>,

    // ── Worker resume note (y8pv / 48ru) ──────────────────────────────────────
    /// One-line resume note injected into worker prompt context after a
    /// recoverable termination / resume-source selection. Contains prior
    /// session ID, checkpoint SHA or submit/review ID, previous model,
    /// termination reason, last durable-progress summary, and a suggested
    /// verification command when available. `None` when this is not a
    /// resume dispatch or the role does not receive worker-resume
    /// instructions.
    pub worker_resume_note: Option<String>,

    // ── Arbiter directive (zkk9 / monitored reopen) ───────────────────────────
    /// Arbiter directive injected verbatim at the top of exactly one next
    /// worker prompt after a monitored-reopen decision. Populated only for
    /// worker-role prompts when the latest unconsumed arbitration row has a
    /// monitored reopen in progress (`monitored_reopen_count >= 1`).  Never
    /// injected into planner/reviewer/lead/architect prompts.  `None` when
    /// there is no active monitored reopen or the role does not receive it.
    pub arbiter_directive: Option<String>,

    // ── Final-verification plan (epic 1bnj) ────────────────────────────────────
    /// `true` when the resolved project `lifecycle.final_verification` plan is
    /// non-empty. Drives the conditional agent surface: Worker/Reviewer prompts
    /// render `run_verification` guidance and the tool is kept in the rendered
    /// tools section only when this is `true`. Empty-plan projects render legacy
    /// prompt language and make no claim that a gate runs at submit.
    pub final_verification_configured: bool,
}

// ─── Conditional final-verification surface (epic 1bnj) ─────────────────────────

/// Tool name gated on a non-empty resolved `lifecycle.final_verification` plan.
///
/// The canonical Worker/Reviewer schema sets advertise this tool
/// unconditionally (so the reviewed baseline and snapshots capture the full
/// surface); the live per-project surface and the rendered prompt strip it when
/// the resolved plan is empty via [`retain_conditional_verification_tools`].
pub const CONDITIONAL_VERIFICATION_TOOL: &str = "run_verification";

/// Remove the on-demand `run_verification` tool from a role's rendered tool
/// surface when the resolved project has no `lifecycle.final_verification`
/// plan configured.
///
/// Single source of truth shared by the live MCP tool surface (supervisor
/// stage) and the rendered prompt tools section so the two cannot drift.
/// `prepare_build_cache` is intentionally never touched here — it renders for
/// the Worker unconditionally.
pub fn retain_conditional_verification_tools(
    schemas: &mut Vec<serde_json::Value>,
    final_verification_configured: bool,
) {
    if !final_verification_configured {
        schemas.retain(|schema| {
            schema.get("name").and_then(|name| name.as_str()) != Some(CONDITIONAL_VERIFICATION_TOOL)
        });
    }
}

/// Render the role-specific `run_verification` guidance block.
///
/// Non-empty only for the Worker and Reviewer roles when the resolved plan is
/// configured. Empty-plan projects (and every other role) render nothing, so
/// the legacy prompt language stands with no claim that a gate runs at submit.
fn verification_guidance_section(role_name: &str, final_verification_configured: bool) -> String {
    if !final_verification_configured {
        return String::new();
    }
    match role_name {
        "worker" => "## Final Verification\n\n\
             This project has a configured final-verification gate. Iterate against it: call \
             `run_verification` to run the canonical gate on the current worktree on demand and \
             confirm your tree passes before you `submit_work`. It consults a passing result for \
             an unchanged tree first, so an unchanged tree is never recompiled and calling it is \
             cheap. Running `cargo`/build/test shell commands yourself for debugging is permitted, \
             but such manual runs are not credited — only `run_verification` records gate evidence \
             for the required checks.\n"
            .to_string(),
        "reviewer" => "## Independent Verification\n\n\
             This project has a configured final-verification gate. You may independently re-run it \
             with `run_verification` to confirm the submitted tree passes on demand — it reuses a \
             passing result for an unchanged authored tree and executes the checks only when the \
             tree has changed. Use it to corroborate the worker's submission rather than trusting \
             it blind.\n"
            .to_string(),
        _ => String::new(),
    }
}

// ─── Renderer ─────────────────────────────────────────────────────────────────

/// Render a system prompt using the given `RoleConfig` and tool schemas.
///
/// The `tool_schemas` function provides the dynamic tools section.  It is
/// passed separately (rather than stored in `RoleConfig`) so that the
/// concrete schema providers can live in `djinn-agent` without creating
/// a `djinn-roles → djinn-agent` dependency.
pub fn render_prompt_for_role(
    config: &RoleConfig,
    tool_schemas: fn() -> Vec<serde_json::Value>,
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

    // Dynamic tools section from role schemas. The canonical schema set
    // advertises `run_verification` unconditionally; strip it here when the
    // resolved project has no final-verification plan so the rendered surface
    // matches the live per-project MCP surface.
    let mut schemas = (tool_schemas)();
    retain_conditional_verification_tools(&mut schemas, ctx.final_verification_configured);
    let tools_md = format_tools_section(&schemas);
    out = out.replace("{{tools_section}}", &tools_md);

    // Role-specific `run_verification` guidance, rendered only for configured
    // projects (Worker/Reviewer). The placeholder vanishes cleanly for
    // empty-plan projects and every other role.
    let verification_guidance =
        verification_guidance_section(config.name, ctx.final_verification_configured);
    out = out.replace("{{verification_guidance_section}}", &verification_guidance);

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

    // Project command sections
    let setup_section = match &ctx.setup_commands {
        Some(cmds) if !cmds.trim().is_empty() => format!(
            "## Automated Commands\n\nThese commands run automatically before your session starts. **Do not run them yourself.**\n\n{cmds}\n"
        ),
        _ => String::new(),
    };
    out = out.replace("{{setup_commands_section}}", &setup_section);

    // sa4x: promoted BLOCKING directive for red required CI.
    // Deliberately separate from ordinary activity-log prose and deduped
    // by construction (derived from durable CI gate snapshot state).
    let ci_blocking_directive_section = match &ctx.ci_blocking_directive {
        Some(text) if !text.trim().is_empty() => {
            format!("## ⛔ BLOCKING: Required CI Failing\n\n{text}\n")
        }
        _ => String::new(),
    };
    out = out.replace(
        "{{ci_blocking_directive_section}}",
        &ci_blocking_directive_section,
    );

    // y8pv / 48ru: worker resume note injected after a recoverable
    // termination / resume-source selection. Only populated for worker
    // dispatch where resume metadata is present; other roles see an empty
    // section so the template placeholder vanishes cleanly.
    let worker_resume_section = match &ctx.worker_resume_note {
        Some(text) if !text.trim().is_empty() => {
            format!("## Resume Context\n\n{text}\n")
        }
        _ => String::new(),
    };
    out = out.replace("{{worker_resume_section}}", &worker_resume_section);

    // zkk9: arbiter directive for monitored reopen. Injected verbatim at the
    // top of exactly one next worker prompt. The prompt-context layer gates
    // this to worker-role only and only when monitored_reopen_count >= 1, so
    // non-worker roles always see an empty section here.
    let arbiter_directive_section = match &ctx.arbiter_directive {
        Some(text) if !text.trim().is_empty() => {
            format!("## \u{2691} Arbiter Directive (monitored reopen)\n\n{text}\n")
        }
        _ => String::new(),
    };
    out = out.replace("{{arbiter_directive_section}}", &arbiter_directive_section);

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

    // PR E2: auto-injected `code_graph context` summary for worker/reviewer roles.
    let code_graph_context_section = match &ctx.code_graph_context {
        Some(text) if !text.trim().is_empty() => format!("## Code Graph Context\n\n{text}\n"),
        _ => String::new(),
    };
    out = out.replace(
        "{{code_graph_context_section}}",
        &code_graph_context_section,
    );

    // PR E3: auto-injected `code_graph detect_changes` summary for reviewer roles.
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
        out = truncate::smart_truncate(&out, MAX_SYSTEM_PROMPT_CHARS);
        tracing::warn!(
            agent_type = %config.name,
            original_len,
            truncated_to = out.len(),
            "system prompt exceeded hard cap and was truncated"
        );
    }

    out
}

/// Deterministic hash of the final rendered system prompt.
///
/// Hashes the exact UTF-8 bytes of `prompt` (the string returned after
/// `render_prompt_for_role` has already applied `MAX_SYSTEM_PROMPT_CHARS`
/// truncation) and returns a stable identifier in the form
/// `sha256:<16 lowercase hex characters>`.
pub fn rendered_system_prompt_hash(prompt: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prompt.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{digest:x}");
    format!("sha256:{}", &hex[..16])
}

// ─── Tool section generator ───────────────────────────────────────────────

/// Generate a markdown tools reference from serialized tool schemas.
///
/// Each schema is expected to have `name`, `description`, and `inputSchema`
/// (with `properties` and optionally `required`). Output is a bullet list of
/// tool name plus required/optional parameter signature only — per-tool
/// description bodies are intentionally omitted because detailed tool docs
/// are supplied through provider tool definitions, not repeated in the system
/// prompt.
///
/// ```text
/// - `tool_name(required_param, optional_param?)`
/// ```
pub fn format_tools_section(schemas: &[serde_json::Value]) -> String {
    let mut lines = Vec::with_capacity(schemas.len());
    for schema in schemas {
        let name = schema["name"].as_str().unwrap_or("unknown");
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

        lines.push(format!("- `{name}({sig})`"));
    }
    lines.join("\n")
}

/// Render a tool line with the old format that includes the per-tool description.
///
/// Used by tests to compute a pre-change baseline size for prompt-shrinkage
/// assertions. Single-line convenience wrapper for one schema entry.
/// Not used in production rendering.
pub fn format_tool_line_with_description(schema: &serde_json::Value) -> String {
    let name = schema["name"].as_str().unwrap_or("unknown");
    let desc = schema["description"].as_str().unwrap_or("");
    let input = &schema["inputSchema"];
    let required: Vec<&str> = input["required"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let mut req_params = Vec::new();
    let mut opt_params = Vec::new();
    if let Some(props) = input["properties"].as_object() {
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

    format!("- `{name}({sig})` — {desc}")
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Format a JSON acceptance-criteria array as a markdown checklist.
///
/// Input: `[{"criterion": "...", "met": false}, ...]`
/// Output: `- [ ] ...\n- [x] ...\n`
pub fn format_acceptance_criteria(json: &str) -> String {
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
pub fn format_labels(json: &str) -> String {
    djinn_core::models::parse_json_array(json).join(", ")
}

// ─── Truncation (moved from djinn-agent::truncate) ────────────────────────────

mod truncate {
    /// Find the largest byte index <= `idx` that is a valid UTF-8 char boundary.
    fn floor_char_boundary(s: &str, idx: usize) -> usize {
        if idx >= s.len() {
            return s.len();
        }
        let mut i = idx;
        while i > 0 && !s.is_char_boundary(i) {
            i -= 1;
        }
        i
    }

    /// Smart-truncate text to `max_bytes` using a 60% head + 40% tail split.
    ///
    /// Preserves both the beginning (context, setup) and end (errors, results)
    /// of output.  Line-aware: splits happen at line boundaries when possible.
    /// Returns the original string unchanged if it fits within the budget.
    pub fn smart_truncate(text: &str, max_bytes: usize) -> String {
        if text.len() <= max_bytes {
            return text.to_string();
        }

        let total_bytes = text.len();

        // Reserve some bytes for the separator line
        let separator_reserve = 80;
        let usable = max_bytes.saturating_sub(separator_reserve);
        if usable == 0 {
            return format!("[truncated — {total_bytes} bytes total]");
        }

        let head_budget = (usable * 60) / 100;
        let tail_budget = usable - head_budget;

        // Collect head lines within budget
        let mut head_end = 0;
        for line in text.split_inclusive('\n') {
            if head_end + line.len() > head_budget && head_end > 0 {
                break;
            }
            head_end += line.len();
        }
        head_end = floor_char_boundary(text, head_end);

        // Collect tail lines within budget (scan backwards).
        let mut tail_start = text.len();
        let mut tail_used = 0usize;
        let bytes = text.as_bytes();
        let mut pos = text.len();
        loop {
            let line_start = if pos == 0 {
                0
            } else {
                match bytes[..pos].iter().rposition(|&b| b == b'\n') {
                    Some(nl) => nl + 1,
                    None => 0,
                }
            };
            let line_len = pos - line_start;
            if tail_used + line_len > tail_budget && tail_used > 0 {
                break;
            }
            tail_used += line_len;
            tail_start = line_start;
            if line_start == 0 {
                break;
            }
            pos = line_start - 1;
        }
        while tail_start < text.len() && !text.is_char_boundary(tail_start) {
            tail_start += 1;
        }

        // Ensure no overlap
        if head_end >= tail_start {
            let end = floor_char_boundary(text, max_bytes.saturating_sub(separator_reserve));
            return format!(
                "{}\n\n[truncated — {total_bytes} bytes total]",
                &text[..end]
            );
        }

        let omitted = tail_start - head_end;
        format!(
            "{}\n\n... [{omitted} bytes omitted — {total_bytes} bytes total] ...\n\n{}",
            text[..head_end].trim_end(),
            text[tail_start..].trim_start()
        )
    }
}

#[cfg(test)]
mod tests;
