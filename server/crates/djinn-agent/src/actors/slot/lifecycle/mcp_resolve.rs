//! Resolves effective MCP servers and skills from `environment_config`, connects
//! to servers, loads skill markdown files, and merges native skills for authoring
//! planner sessions. All failures are non-fatal and logged.

use std::path::Path;

use djinn_core::extension_diagnostics::ExtensionLoadDiagnosticV1;
use djinn_db::ExtensionLoadDiagnosticRepository;

use crate::context::AgentContext;
use crate::environment::environment_config_for_path;
use crate::extension_diagnostics::{
    ExtensionDiagnosticAssociations, ExtensionDiagnosticFact, persist_extension_diagnostic_batch,
};
use crate::mcp_client::McpToolRegistry;
use crate::mcp_settings::{effective_mcp_server_names, effective_skill_names};
use crate::native_skills;
use crate::roles::AgentRole;
use crate::skills::ResolvedSkill;

use super::task_classifier::NativeSkillTrigger;

/// Resolved MCP + skills bundle for the upcoming session.
pub(crate) struct McpAndSkills {
    pub effective_mcp_servers: Vec<String>,
    pub effective_skills: Vec<String>,
    pub mcp_registry: Option<McpToolRegistry>,
    pub resolved_skills: Vec<ResolvedSkill>,
    /// Names of native skills prepended for telemetry tracking.
    pub native_skill_names: Vec<String>,
    /// Per-server MCP instructions from connected servers, in deterministic
    /// server-name order. Failed or no-instruction servers are omitted.
    pub mcp_server_instructions: std::collections::BTreeMap<String, String>,
    /// Canonical repository rows for this combined MCP + skill load pass.
    pub extension_diagnostics: Vec<ExtensionLoadDiagnosticV1>,
}

/// Fetch project environment config, resolve the effective MCP server + skill
/// lists for the current role, connect to the resolved MCP servers, and load
/// the skill markdown files.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn resolve_mcp_and_skills(
    worktree_path: &Path,
    runtime_role: &dyn AgentRole,
    project_id: &str,
    task_id: &str,
    session_id: &str,
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
    let load_attempt_id = uuid::Uuid::now_v7().to_string();
    let mcp_result = connect_mcp_registry(
        task_short_id,
        role_name,
        &resolved_mcp_servers,
        #[cfg(test)]
        mcp_registry_override,
        app_state,
    )
    .await;
    let mcp_server_instructions = mcp_result
        .registry
        .as_ref()
        .map(|r| r.server_instructions().clone())
        .unwrap_or_default();
    let project_skills =
        load_project_skills(worktree_path, task_short_id, role_name, &effective_skills).await;
    let (resolved_skills, native_skill_names) =
        merge_native_skills(role_name, project_skills.skills, authoring_trigger);
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
    let mut facts = mcp_result.diagnostics;
    facts.extend(project_skills.diagnostics);
    let extension_diagnostics = persist_load_diagnostics(
        project_id,
        task_id,
        session_id,
        &load_attempt_id,
        facts,
        app_state,
    )
    .await;
    McpAndSkills {
        effective_mcp_servers,
        effective_skills,
        mcp_registry: mcp_result.registry,
        resolved_skills,
        native_skill_names,
        mcp_server_instructions,
        extension_diagnostics,
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
    resolved.into_iter().map(|(n, c)| (n, c.clone())).collect()
}

/// Connect to resolved MCP servers and discover their tools.
async fn connect_mcp_registry(
    task_short_id: &str,
    role_name: &str,
    resolved_mcp_servers: &[(String, crate::mcp_settings::McpServerConfig)],
    #[cfg(test)] mcp_registry_override: Option<McpToolRegistry>,
    app_state: &AgentContext,
) -> crate::mcp_client::McpDiscoveryResult {
    #[cfg(test)]
    {
        if let Some(registry) = mcp_registry_override {
            return crate::mcp_client::McpDiscoveryResult {
                registry: Some(registry),
                diagnostics: Vec::new(),
            };
        }
    }
    let _ = (task_short_id, role_name); // used in cfg(test) and tracing
    if !resolved_mcp_servers.is_empty() {
        crate::mcp_client::connect_and_discover_with_diagnostics(
            task_short_id,
            role_name,
            resolved_mcp_servers,
            app_state,
        )
        .await
    } else {
        crate::mcp_client::McpDiscoveryResult {
            registry: None,
            diagnostics: Vec::new(),
        }
    }
}

/// Load read-only repository-convention skills from the worktree.
async fn load_project_skills(
    worktree_path: &Path,
    task_short_id: &str,
    role_name: &str,
    effective_skills: &[String],
) -> crate::skills::DetailedSkillsLoad {
    let loaded = crate::skills::load_skills_detailed(worktree_path, effective_skills);
    tracing::info!(task_id = %task_short_id, role = %role_name, requested_count = effective_skills.len(), resolved_count = loaded.skills.len(), "Lifecycle: resolved repository-convention skills");
    loaded
}

/// Persist a lifecycle load pass and return its canonical scoped rows.
///
/// This is crate-visible for the post-discovery integration regression, which
/// drives the same session-associated persistence/read boundary as stage.
pub(crate) async fn persist_load_diagnostics(
    project_id: &str,
    task_id: &str,
    session_id: &str,
    load_attempt_id: &str,
    facts: Vec<ExtensionDiagnosticFact>,
    app_state: &AgentContext,
) -> Vec<ExtensionLoadDiagnosticV1> {
    let repository = ExtensionLoadDiagnosticRepository::new(app_state.db.clone());
    let associations = ExtensionDiagnosticAssociations {
        project_id: project_id.to_owned(),
        task_id: Some(task_id.to_owned()),
        session_id: Some(session_id.to_owned()),
        load_attempt_id: load_attempt_id.to_owned(),
    };
    match persist_extension_diagnostic_batch(
        |error| {
            tracing::error!(
                project_id,
                task_id,
                session_id,
                load_attempt_id,
                error = %error,
                "Lifecycle: failed to persist extension diagnostic"
            );
            true
        },
        &repository,
        associations,
        facts,
    )
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!(project_id, task_id, session_id, load_attempt_id, error = %error, "Lifecycle: failed to read persisted extension diagnostics");
            // Prompt rendering only consumes the repository's scoped, canonically
            // ordered read. Do not substitute write-return values when that read
            // fails: they are neither the complete attempt nor its canonical order.
            Vec::new()
        }
    }
}

/// Merge native skills for `role_name` with project-resolved skills.
///
/// When `authoring_trigger` is `Some`, native skills recommended for the role
/// are prepended to the project list. Any project skill whose name matches a
/// native skill name is filtered out to prevent shadowing.
pub(crate) fn merge_native_skills(
    role_name: &str,
    project_skills: Vec<ResolvedSkill>,
    authoring_trigger: Option<NativeSkillTrigger>,
) -> (Vec<ResolvedSkill>, Vec<String>) {
    if authoring_trigger.is_none() {
        return (project_skills, Vec::new());
    }
    let native = native_skills::resolved_native_skills_for_role(role_name);
    let native_names: Vec<String> = native.iter().map(|s| s.name.clone()).collect();
    if native.is_empty() {
        return (project_skills, native_names);
    }
    let filtered: Vec<ResolvedSkill> = project_skills
        .into_iter()
        .filter(|s| !native_names.contains(&s.name))
        .collect();
    let merged: Vec<ResolvedSkill> = native.into_iter().chain(filtered).collect();
    (merged, native_names)
}

#[cfg(test)]
mod tests {
    use super::*;
    const AUTHORING: Option<NativeSkillTrigger> = Some(NativeSkillTrigger::ProposalAuthoring);
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
    #[test]
    fn authoring_planner_receives_visual_spec_native_skill() {
        let (merged, native_names) = merge_native_skills("planner", Vec::new(), AUTHORING);
        assert!(native_names.contains(&"visual-spec".to_string()));
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
        assert_eq!(merged[0].name, "visual-spec");
        assert_eq!(merged[0].trust_level, "platform");
        assert_eq!(merged[1].name, "git");
        assert_eq!(merged[2].name, "testing");
    }
    #[test]
    fn empty_project_skills_with_authoring_planner() {
        let (merged, native_names) = merge_native_skills("planner", Vec::new(), AUTHORING);
        assert_eq!(native_names, vec!["visual-spec"]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "visual-spec");
    }
    #[test]
    fn non_authoring_planner_does_not_load_native_skills() {
        let (merged, native_names) = merge_native_skills("planner", Vec::new(), None);
        assert!(native_names.is_empty());
        assert!(merged.is_empty());
    }
    #[test]
    fn non_authoring_planner_preserves_project_skills() {
        let project = vec![project_skill("git"), project_skill("testing")];
        let (merged, native_names) = merge_native_skills("planner", project, None);
        assert!(native_names.is_empty());
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "git");
        assert_eq!(merged[1].name, "testing");
    }
    #[test]
    fn non_authoring_planner_project_visual_spec_kept_as_project() {
        let project = vec![project_skill("git"), project_skill("visual-spec")];
        let (merged, native_names) = merge_native_skills("planner", project, None);
        assert!(native_names.is_empty());
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "git");
        assert_eq!(merged[1].name, "visual-spec");
        assert_eq!(merged[1].trust_level, "project");
    }
    #[test]
    fn project_skill_named_visual_spec_does_not_shadow_native() {
        let project = vec![
            project_skill("git"),
            project_skill("visual-spec"),
            project_skill("testing"),
        ];
        let (merged, native_names) = merge_native_skills("planner", project, AUTHORING);
        assert_eq!(native_names, vec!["visual-spec"]);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].name, "visual-spec");
        assert_eq!(merged[0].trust_level, "platform");
        assert!(
            !merged[0].content.contains("from worktree"),
            "native visual-spec body must not be replaced by project content"
        );
        assert_eq!(merged[1].name, "git");
        assert_eq!(merged[2].name, "testing");
    }
    #[test]
    fn non_planner_roles_do_not_receive_native_skills() {
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
        let (merged, native_names) = merge_native_skills("worker", project, None);
        assert!(native_names.is_empty());
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "git");
        assert_eq!(merged[1].name, "visual-spec");
        assert_eq!(merged[1].trust_level, "project");
    }
    #[test]
    fn effective_skills_excludes_native_names() {
        let globals = vec!["git".to_string()];
        let role = vec!["visual-spec".to_string()];
        let effective = crate::mcp_settings::effective_skill_names(&globals, &role);
        assert_eq!(effective, vec!["git", "visual-spec"]);
        let project = vec![project_skill("git"), project_skill("visual-spec")];
        let (merged, native_names) = merge_native_skills("planner", project, AUTHORING);
        assert_eq!(native_names, vec!["visual-spec"]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "visual-spec");
        assert_eq!(merged[0].trust_level, "platform");
        assert_eq!(merged[1].name, "git");
    }
    #[test]
    fn no_authoring_trigger_means_no_native_skills() {
        let (merged, native_names) = merge_native_skills("planner", Vec::new(), None);
        assert!(native_names.is_empty());
        assert!(merged.is_empty());
    }
    #[test]
    fn no_trigger_preserves_project_skills() {
        let project = vec![project_skill("git"), project_skill("testing")];
        let (merged, native_names) = merge_native_skills("planner", project, None);
        assert!(native_names.is_empty());
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "git");
        assert_eq!(merged[1].name, "testing");
    }
}
