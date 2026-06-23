//! Per-session MCP server + skills resolution for the task lifecycle.
//!
//! This is a pure code-motion extraction from `run_task_lifecycle` (task #14
//! preparatory work). It reads the MCP default + global-skill fields of
//! `environment_config` from Dolt, resolves the effective MCP
//! servers and skills (role-level list merged with project defaults),
//! connects to the resolved MCP servers (best-effort; unreachable servers are
//! logged and skipped), and loads skill markdown files from the worktree.
//!
//! Native skills (platform-owned, compiled-in) are resolved separately from the
//! native skill registry and prepended to the project-resolved list so they
//! appear first in the prompt's skills section.  A project/worktree skill whose
//! name collides with a native skill is filtered out — the native body is
//! immutable and must not be shadowed.
//!
//! No failure modes here propagate errors: missing environment config, unknown
//! server names, unreachable endpoints, and missing skill files are all
//! non-fatal and emit a `tracing::warn!` on the existing log path. There is
//! therefore no error type — the helper always returns the populated struct.

use std::path::Path;

use crate::context::AgentContext;
use crate::environment::environment_config_for_path;
use crate::mcp_client::McpToolRegistry;
use crate::mcp_settings::{effective_mcp_server_names, effective_skill_names};
use crate::native_skills;
use crate::roles::AgentRole;
use crate::skills::ResolvedSkill;

use super::task_classifier::NativeSkillTrigger;

/// Resolved MCP + skills bundle for the upcoming session.
///
/// `effective_mcp_servers` / `effective_skills` are the pre-resolve *name*
/// lists used for downstream telemetry (the reply-loop context records them
/// for session-log provenance); `mcp_registry` / `resolved_skills` are the
/// fully-hydrated forms used for tool dispatch / prompt building.
///
/// Setup fields previously came in on a
/// `DjinnSettings` handle returned here; they were moved to Dolt's
/// `projects.environment_config` as part of the P8 cut-over.
/// Downstream callers fetch that block directly via
/// [`crate::environment::environment_config_for_path`].
pub(crate) struct McpAndSkills {
    pub effective_mcp_servers: Vec<String>,
    pub effective_skills: Vec<String>,
    pub mcp_registry: Option<McpToolRegistry>,
    pub resolved_skills: Vec<ResolvedSkill>,
    /// Names of native skills that were prepended to `resolved_skills` for
    /// this role.  Kept separate from `effective_skills` so downstream
    /// telemetry can distinguish platform-owned skills from mutable
    /// project/role skills without making native names look like user-editable
    /// role skills.
    pub native_skill_names: Vec<String>,
}

/// Fetch project environment config, resolve the effective MCP server + skill
/// lists for the current role, connect to the resolved MCP servers, and load
/// the skill markdown files.
///
/// Behaviour:
///   - Fetching the `environment_config` from Dolt is best-effort; any
///     failure (missing row, parse error, pre-reseed column) is logged on the
///     call path and defaulted to an empty config.
///   - Empty `effective_mcp_servers` short-circuits both the registry load
///     and `connect_and_discover` so default-role sessions don't touch the
///     MCP machinery at all.
///   - The `mcp_registry_override` test seam bypasses `connect_and_discover`.
///   - The two `tracing::info!` "resolved role MCP servers" / "resolved role
///     skills" log lines are preserved.
pub(crate) async fn resolve_mcp_and_skills(
    worktree_path: &Path,
    runtime_role: &dyn AgentRole,
    task_short_id: &str,
    task_issue_type: &str,
    role_mcp_servers: Option<&[String]>,
    role_skills: &[String],
    #[cfg(test)] mcp_registry_override: Option<McpToolRegistry>,
    app_state: &AgentContext,
) -> McpAndSkills {
    let env_cfg = environment_config_for_path(&app_state.db, worktree_path).await;

    let effective_mcp_servers = effective_mcp_server_names(
        &env_cfg.agent_mcp_defaults,
        runtime_role.config().name,
        role_mcp_servers,
    );
    let effective_skills = effective_skill_names(&env_cfg.global_skills, role_skills);

    // ── Resolve role-level MCP servers ────────────────────────────────────────
    // Load the project MCP server registry from standard discovery files
    // (`mcp.json`, `.cursor/mcp.json`, `.opencode/mcp.json`). Unknown names
    // are logged as warnings and skipped — they never block the session from
    // starting.
    //
    // Default roles have empty mcp_servers, so this block is a no-op for them.
    let resolved_mcp_servers = if !effective_mcp_servers.is_empty() {
        let registry = crate::mcp_settings::load_mcp_server_registry(worktree_path);
        let resolved = crate::mcp_settings::resolve_mcp_servers(
            task_short_id,
            runtime_role.config().name,
            &effective_mcp_servers,
            &registry,
        );
        tracing::info!(
            task_id = %task_short_id,
            role = %runtime_role.config().name,
            requested_count = effective_mcp_servers.len(),
            resolved_count = resolved.len(),
            "Lifecycle: resolved role MCP servers"
        );
        resolved
            .into_iter()
            .map(|(name, cfg)| (name, cfg.clone()))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    // Connect to resolved MCP servers and discover their tool definitions.
    // Unreachable or misconfigured servers are logged and skipped (non-fatal).
    let mcp_registry = {
        #[cfg(test)]
        {
            if let Some(registry) = mcp_registry_override {
                Some(registry)
            } else if !resolved_mcp_servers.is_empty() {
                crate::mcp_client::connect_and_discover(
                    task_short_id,
                    runtime_role.config().name,
                    &resolved_mcp_servers,
                    app_state,
                )
                .await
            } else {
                None
            }
        }
        #[cfg(not(test))]
        {
            if !resolved_mcp_servers.is_empty() {
                crate::mcp_client::connect_and_discover(
                    task_short_id,
                    runtime_role.config().name,
                    &resolved_mcp_servers,
                    app_state,
                )
                .await
            } else {
                None
            }
        }
    };

    // ── Load and resolve skills from worktree .djinn/skills/ ─────────────────
    // Skills are markdown files with YAML frontmatter. Missing skills are logged
    // as warnings and skipped when no checked manifest exists. If
    // `.djinn/skills.json` exists, stale/tampered materialized skills fail
    // closed so prompt summaries and inlined bodies cannot drift from the
    // generated hashes.
    let project_skills = if !effective_skills.is_empty() {
        match crate::skills_manifest::load_verified_skills(worktree_path, &effective_skills) {
            Ok(loaded) => {
                tracing::info!(
                    task_id = %task_short_id,
                    role = %runtime_role.config().name,
                    requested_count = effective_skills.len(),
                    resolved_count = loaded.len(),
                    "Lifecycle: resolved role skills"
                );
                loaded
            }
            Err(error) => {
                tracing::error!(
                    task_id = %task_short_id,
                    role = %runtime_role.config().name,
                    requested_count = effective_skills.len(),
                    error = %error,
                    "Lifecycle: skills manifest verification failed"
                );
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    // ── Merge native skills for the role ──────────────────────────────────────
    // Native skills are platform-owned, compiled-in, and immutable.  They are
    // resolved from the native registry (not from worktree files) and prepended
    // before project skills so they appear first in the prompt's skills section.
    // A project/worktree skill whose name collides with a native skill is
    // filtered out — the native body must not be shadowed or mutated.
    //
    // Native skills are only merged when the authoring trigger fires for the
    // current task.  This gates `visual-spec` (and future native authoring
    // skills) to proposal-authoring/grooming/refinement planner sessions,
    // keeping ordinary wave-planning/dispatch sessions free of context cost.
    let role_name = runtime_role.config().name;
    let authoring_trigger = classify_authoring_trigger(role_name, task_issue_type);
    let (resolved_skills, native_skill_names) =
        merge_native_skills(role_name, project_skills, authoring_trigger);

    if !native_skill_names.is_empty() {
        tracing::info!(
            task_id = %task_short_id,
            role = %role_name,
            native_count = native_skill_names.len(),
            native_names = %native_skill_names.join(", "),
            ?authoring_trigger,
            "Lifecycle: merged native skills for role"
        );
    }

    McpAndSkills {
        effective_mcp_servers,
        effective_skills,
        mcp_registry,
        resolved_skills,
        native_skill_names,
    }
}

/// Classify the native-skill authoring trigger for a `(role_name, issue_type)` pair.
///
/// Returns `Some(ProposalAuthoring)` when the role is `"planner"` and the
/// issue type is `"epic_breakdown"` (proposal decomposition / AC
/// reconciliation).  Returns `None` for all other combinations.
///
/// This is the lazy-loading gate: native skills are only merged into the
/// prompt's resolved-skills set when the trigger fires, so ordinary
/// wave-planning/dispatch sessions pay no context cost for authoring skills.
fn classify_authoring_trigger(
    role_name: &str,
    task_issue_type: &str,
) -> Option<NativeSkillTrigger> {
    if role_name == "planner" && task_issue_type == "epic_breakdown" {
        Some(NativeSkillTrigger::ProposalAuthoring)
    } else {
        None
    }
}

/// Merge native skills for `role_name` with project-resolved skills.
///
/// When `authoring_trigger` is `Some`, native skills recommended for the role
/// are prepended to the project list.  When `None`, native skills are not
/// merged — only project skills are returned.
///
/// Any project skill whose name matches a native skill name is filtered out to
/// prevent shadowing of the immutable native body.
///
/// Returns `(merged_skills, native_skill_names)` — the second element lists
/// the native skill names for separate telemetry tracking.
pub(crate) fn merge_native_skills(
    role_name: &str,
    project_skills: Vec<ResolvedSkill>,
    authoring_trigger: Option<NativeSkillTrigger>,
) -> (Vec<ResolvedSkill>, Vec<String>) {
    // Only merge native skills when the authoring trigger fires.
    if authoring_trigger.is_none() {
        return (project_skills, Vec::new());
    }

    let native = native_skills::resolved_native_skills_for_role(role_name);
    let native_names: Vec<String> = native.iter().map(|s| s.name.clone()).collect();

    if native.is_empty() {
        return (project_skills, native_names);
    }

    // Filter out project skills that shadow a native skill name.
    let filtered: Vec<ResolvedSkill> = project_skills
        .into_iter()
        .filter(|s| !native_names.contains(&s.name))
        .collect();

    // Prepend native skills so they appear before project skills.
    let merged: Vec<ResolvedSkill> = native.into_iter().chain(filtered).collect();
    (merged, native_names)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to build a minimal `ResolvedSkill` for test purposes.
    fn project_skill(name: &str) -> ResolvedSkill {
        ResolvedSkill {
            name: name.to_string(),
            description: format!("{name} project skill"),
            content: format!("{name} content from worktree"),
            required: false,
            trust_level: "project".to_string(),
            recommended_for_roles: Vec::new(),
            tags: Vec::new(),
        }
    }

    fn authoring() -> Option<NativeSkillTrigger> {
        Some(NativeSkillTrigger::ProposalAuthoring)
    }

    #[test]
    fn planner_receives_visual_spec_native_skill() {
        let (merged, native_names) = merge_native_skills("planner", Vec::new(), authoring());
        assert!(
            native_names.contains(&"visual-spec".to_string()),
            "planner should have visual-spec as a native skill"
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "visual-spec");
        assert_eq!(merged[0].trust_level, "platform");
        assert!(merged[0].required);
    }

    #[test]
    fn worker_does_not_receive_visual_spec() {
        let (merged, native_names) = merge_native_skills("worker", Vec::new(), authoring());
        assert!(
            native_names.is_empty(),
            "worker should have no native skills"
        );
        assert!(merged.is_empty());
    }

    #[test]
    fn reviewer_does_not_receive_visual_spec() {
        let (merged, native_names) = merge_native_skills("reviewer", Vec::new(), authoring());
        assert!(native_names.is_empty());
        assert!(merged.is_empty());
    }

    #[test]
    fn lead_does_not_receive_visual_spec() {
        let (merged, native_names) = merge_native_skills("lead", Vec::new(), authoring());
        assert!(native_names.is_empty());
        assert!(merged.is_empty());
    }

    #[test]
    fn architect_does_not_receive_visual_spec() {
        let (merged, native_names) = merge_native_skills("architect", Vec::new(), authoring());
        assert!(native_names.is_empty());
        assert!(merged.is_empty());
    }

    #[test]
    fn native_skills_prepended_before_project_skills() {
        let project = vec![project_skill("git"), project_skill("testing")];
        let (merged, native_names) = merge_native_skills("planner", project, authoring());

        assert_eq!(native_names, vec!["visual-spec"]);
        assert_eq!(merged.len(), 3);
        // Native skill comes first.
        assert_eq!(merged[0].name, "visual-spec");
        assert_eq!(merged[0].trust_level, "platform");
        // Project skills follow in original order.
        assert_eq!(merged[1].name, "git");
        assert_eq!(merged[2].name, "testing");
    }

    #[test]
    fn project_skill_named_visual_spec_does_not_shadow_native() {
        // A project/worktree skill named "visual-spec" must not replace or
        // override the native planner default body.
        let project = vec![
            project_skill("git"),
            project_skill("visual-spec"),
            project_skill("testing"),
        ];
        let (merged, native_names) = merge_native_skills("planner", project, authoring());

        assert_eq!(native_names, vec!["visual-spec"]);
        // The project "visual-spec" is filtered out; only the native one remains.
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].name, "visual-spec");
        assert_eq!(merged[0].trust_level, "platform");
        // The native body is the compiled-in content, not the project content.
        assert!(
            !merged[0].content.contains("from worktree"),
            "native visual-spec body must not be replaced by project content"
        );
        // Remaining project skills are preserved.
        assert_eq!(merged[1].name, "git");
        assert_eq!(merged[2].name, "testing");
    }

    #[test]
    fn non_planner_roles_with_project_skills_are_unchanged() {
        let project = vec![project_skill("git"), project_skill("visual-spec")];
        let (merged, native_names) = merge_native_skills("worker", project, authoring());

        assert!(native_names.is_empty());
        // Both project skills pass through unmodified for non-planner roles.
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "git");
        assert_eq!(merged[1].name, "visual-spec");
        assert_eq!(merged[1].trust_level, "project");
    }

    #[test]
    fn empty_project_skills_with_planner() {
        let (merged, native_names) = merge_native_skills("planner", Vec::new(), authoring());
        assert_eq!(native_names, vec!["visual-spec"]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "visual-spec");
    }

    #[test]
    fn effective_skills_excludes_native_names() {
        // effective_skill_names computes project/global/role skill names only.
        // Native skills are not part of this list.
        let globals = vec!["git".to_string()];
        let role = vec!["visual-spec".to_string()];
        let effective = crate::mcp_settings::effective_skill_names(&globals, &role);
        assert_eq!(effective, vec!["git", "visual-spec"]);
        // But merge_native_skills filters out the project "visual-spec" and
        // replaces it with the native one — the effective list is unaffected.
        let project = vec![project_skill("git"), project_skill("visual-spec")];
        let (merged, native_names) = merge_native_skills("planner", project, authoring());
        assert_eq!(native_names, vec!["visual-spec"]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "visual-spec");
        assert_eq!(merged[0].trust_level, "platform");
        assert_eq!(merged[1].name, "git");
    }

    #[test]
    fn no_authoring_trigger_means_no_native_skills() {
        // Without the authoring trigger, native skills are not merged.
        let (merged, native_names) = merge_native_skills("planner", Vec::new(), None);
        assert!(native_names.is_empty(), "no native skills without trigger");
        assert!(merged.is_empty());
    }

    #[test]
    fn no_trigger_preserves_project_skills() {
        // Without the authoring trigger, project skills pass through unchanged.
        let project = vec![project_skill("git"), project_skill("testing")];
        let (merged, native_names) = merge_native_skills("planner", project, None);
        assert!(native_names.is_empty());
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "git");
        assert_eq!(merged[1].name, "testing");
    }

    #[test]
    fn classify_authoring_trigger_returns_proposal_authoring_for_epic_breakdown() {
        assert_eq!(
            classify_authoring_trigger("planner", "epic_breakdown"),
            Some(NativeSkillTrigger::ProposalAuthoring),
        );
    }

    #[test]
    fn classify_authoring_trigger_returns_none_for_planning() {
        assert_eq!(classify_authoring_trigger("planner", "planning"), None);
    }

    #[test]
    fn classify_authoring_trigger_returns_none_for_non_planner() {
        assert_eq!(classify_authoring_trigger("worker", "epic_breakdown"), None,);
    }
}
