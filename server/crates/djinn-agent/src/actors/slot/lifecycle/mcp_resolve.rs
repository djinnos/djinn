//! Per-session MCP server + skills resolution for the task lifecycle.
//!
//! Resolves effective MCP servers and skills from `environment_config`, connects
//! to servers, loads skill markdown files, and merges native skills for authoring
//! planner sessions. All failures are non-fatal and logged.

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
#[allow(clippy::too_many_arguments)]
pub(crate) async fn resolve_mcp_and_skills(
    worktree_path: &Path,
    runtime_role: &dyn AgentRole,
    task_short_id: &str,
    role_mcp_servers: Option<&[String]>,
    role_skills: &[String],
    authoring_trigger: Option<NativeSkillTrigger>,
    #[cfg(test)] mcp_registry_override: Option<McpToolRegistry>,
    app_state: &AgentContext,
) -> McpAndSkills {
    let role_name = runtime_role.config().name;
    let env_cfg = environment_config_for_path(&app_state.db, worktree_path).await;
    let effective_mcp_servers =
        effective_mcp_server_names(&env_cfg.agent_mcp_defaults, role_name, role_mcp_servers);
    let effective_skills = effective_skill_names(&env_cfg.global_skills, role_skills);

    let resolved_mcp_servers = resolve_mcp_server_entries(
        worktree_path,
        task_short_id,
        role_name,
        &effective_mcp_servers,
    );
    let mcp_registry = connect_mcp_registry(
        task_short_id,
        role_name,
        &resolved_mcp_servers,
        #[cfg(test)]
        mcp_registry_override,
        app_state,
    )
    .await;
    let project_skills =
        load_project_skills(worktree_path, task_short_id, role_name, &effective_skills).await;
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

/// Resolve role-level MCP server entries from the project registry.
fn resolve_mcp_server_entries(
    worktree_path: &Path,
    task_short_id: &str,
    role_name: &str,
    effective_mcp_servers: &[String],
) -> Vec<(String, crate::mcp_settings::McpServerConfig)> {
    if effective_mcp_servers.is_empty() {
        return Vec::new();
    }
    let registry = crate::mcp_settings::load_mcp_server_registry(worktree_path);
    let resolved = crate::mcp_settings::resolve_mcp_servers(
        task_short_id,
        role_name,
        effective_mcp_servers,
        &registry,
    );
    tracing::info!(
        task_id = %task_short_id,
        role = %role_name,
        requested_count = effective_mcp_servers.len(),
        resolved_count = resolved.len(),
        "Lifecycle: resolved role MCP servers"
    );
    resolved
        .into_iter()
        .map(|(name, cfg)| (name, cfg.clone()))
        .collect()
}

/// Connect to resolved MCP servers and discover their tool definitions.
async fn connect_mcp_registry(
    task_short_id: &str,
    role_name: &str,
    resolved_mcp_servers: &[(String, crate::mcp_settings::McpServerConfig)],
    #[cfg(test)] mcp_registry_override: Option<McpToolRegistry>,
    app_state: &AgentContext,
) -> Option<McpToolRegistry> {
    #[cfg(test)]
    {
        if let Some(registry) = mcp_registry_override {
            return Some(registry);
        }
    }
    if resolved_mcp_servers.is_empty() {
        return None;
    }
    crate::mcp_client::connect_and_discover(
        task_short_id,
        role_name,
        resolved_mcp_servers,
        app_state,
    )
    .await
}

/// Load and resolve project skills from the worktree.
async fn load_project_skills(
    worktree_path: &Path,
    task_short_id: &str,
    role_name: &str,
    effective_skills: &[String],
) -> Vec<ResolvedSkill> {
    if effective_skills.is_empty() {
        return Vec::new();
    }
    match crate::skills_manifest::load_verified_skills(worktree_path, effective_skills) {
        Ok(loaded) => {
            tracing::info!(
                task_id = %task_short_id,
                role = %role_name,
                requested_count = effective_skills.len(),
                resolved_count = loaded.len(),
                "Lifecycle: resolved role skills"
            );
            loaded
        }
        Err(error) => {
            tracing::error!(
                task_id = %task_short_id,
                role = %role_name,
                requested_count = effective_skills.len(),
                error = %error,
                "Lifecycle: skills manifest verification failed"
            );
            Vec::new()
        }
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

    /// Shorthand for the authoring trigger used in tests.
    const AUTHORING: Option<NativeSkillTrigger> = Some(NativeSkillTrigger::ProposalAuthoring);

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

    // ── Authoring trigger: native skills loaded ──────────────────────────

    #[test]
    fn authoring_planner_receives_visual_spec_native_skill() {
        let (merged, native_names) = merge_native_skills("planner", Vec::new(), AUTHORING);
        assert!(
            native_names.contains(&"visual-spec".to_string()),
            "authoring planner should have visual-spec as a native skill"
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "visual-spec");
        assert_eq!(merged[0].trust_level, "platform");
        assert!(merged[0].required);
    }

    #[test]
    fn native_skills_prepended_before_project_skills() {
        let project = vec![project_skill("git"), project_skill("testing")];
        let (merged, native_names) = merge_native_skills("planner", project, AUTHORING);

        assert_eq!(native_names, vec!["visual-spec"]);
        assert_eq!(merged.len(), 3);
        // Native skill comes first.
        assert_eq!(merged[0].name, "visual-spec");
        assert_eq!(merged[0].trust_level, "platform");
        // Project skills follow in original order.
        assert_eq!(merged[1].name, "git");
        assert_eq!(merged[2].name, "testing");
    }

    // ── Non-authoring planner: native skills NOT loaded ──────────────────

    #[test]
    fn non_authoring_planner_does_not_load_native_skills() {
        let (merged, native_names) = merge_native_skills("planner", Vec::new(), None);
        assert!(
            native_names.is_empty(),
            "non-authoring planner should have no native skills"
        );
        assert!(
            merged.is_empty(),
            "non-authoring planner with no project skills should have empty resolved_skills"
        );
    }

    #[test]
    fn non_authoring_planner_preserves_project_skills() {
        let project = vec![project_skill("git"), project_skill("testing")];
        let (merged, native_names) = merge_native_skills("planner", project, None);

        assert!(native_names.is_empty());
        // Project skills pass through unmodified.
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "git");
        assert_eq!(merged[1].name, "testing");
    }

    #[test]
    fn non_authoring_planner_project_visual_spec_kept_as_project() {
        // A project "visual-spec" skill in a non-authoring planner session
        // should pass through as a project skill — not replaced by native.
        let project = vec![project_skill("git"), project_skill("visual-spec")];
        let (merged, native_names) = merge_native_skills("planner", project, None);

        assert!(native_names.is_empty());
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "git");
        assert_eq!(merged[1].name, "visual-spec");
        assert_eq!(merged[1].trust_level, "project");
    }

    // ── Duplicate-name handling with authoring trigger ───────────────────

    #[test]
    fn project_skill_named_visual_spec_does_not_shadow_native() {
        // A project/worktree skill named "visual-spec" must not replace or
        // override the native planner default body.
        let project = vec![
            project_skill("git"),
            project_skill("visual-spec"),
            project_skill("testing"),
        ];
        let (merged, native_names) = merge_native_skills("planner", project, AUTHORING);

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

    // ── Non-planner roles (unchanged behaviour) ─────────────────────────

    #[test]
    fn non_planner_roles_do_not_load_native_skills() {
        for role in ["worker", "reviewer", "lead", "architect"] {
            let (merged, native_names) = merge_native_skills(role, Vec::new(), AUTHORING);
            assert!(
                native_names.is_empty(),
                "{role} should have no native skills even with authoring trigger"
            );
            assert!(merged.is_empty(), "{role} should have empty merged skills");
        }
    }

    #[test]
    fn non_planner_roles_with_project_skills_are_unchanged() {
        let project = vec![project_skill("git"), project_skill("visual-spec")];
        for role in ["worker", "reviewer", "lead", "architect"] {
            let (merged, native_names) = merge_native_skills(role, project.clone(), None);
            assert!(
                native_names.is_empty(),
                "{role} should have no native names"
            );
            assert_eq!(
                merged.len(),
                2,
                "{role} should preserve both project skills"
            );
            assert_eq!(merged[0].name, "git");
            assert_eq!(merged[1].name, "visual-spec");
            assert_eq!(merged[1].trust_level, "project");
        }
    }

    // ── effective_skills telemetry ───────────────────────────────────────

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
        let (merged, native_names) = merge_native_skills("planner", project, AUTHORING);
        assert_eq!(native_names, vec!["visual-spec"]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "visual-spec");
        assert_eq!(merged[0].trust_level, "platform");
        assert_eq!(merged[1].name, "git");
    }
}
