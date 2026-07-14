use std::path::Path;

use djinn_core::extension_diagnostics::ExtensionLoadDiagnosticV1;
use djinn_db::ExtensionLoadDiagnosticRepository;

use crate::context::AgentContext;
use crate::environment::environment_config_for_project_id;
use crate::extension_diagnostics::{ExtensionDiagnosticAssociations, persist_extension_diagnostic_batch};
use crate::mcp_client::connect_and_discover_with_diagnostics;
use crate::mcp_settings::{effective_mcp_server_names, effective_skill_names, load_mcp_server_registry, resolve_mcp_servers};
use crate::skills_manifest::load_verified_skills_detailed;

/// Runs one fresh, read-only doctor extension diagnostics attempt.
pub async fn probe_project_extensions(project_id: &str, canonical_workspace: &Path, app_state: &AgentContext) -> Result<Vec<ExtensionLoadDiagnosticV1>, String> {
    let env = environment_config_for_project_id(&app_state.db, project_id).await;
    let mcp_names = effective_mcp_server_names(&env.agent_mcp_defaults, "doctor", None);
    let skill_names = effective_skill_names(&env.global_skills, &[]);
    let load_attempt_id = uuid::Uuid::now_v7().to_string();
    let registry = load_mcp_server_registry(canonical_workspace);
    let servers = resolve_mcp_servers("doctor", "doctor", &mcp_names, &registry).into_iter().map(|(name, config)| (name, config.clone())).collect::<Vec<_>>();
    let mcp = connect_and_discover_with_diagnostics("doctor", "doctor", &servers, app_state).await;
    let skills = load_verified_skills_detailed(canonical_workspace, &skill_names);
    let mut facts = mcp.diagnostics;
    facts.extend(skills.diagnostics);
    persist_extension_diagnostic_batch(&ExtensionLoadDiagnosticRepository::new(app_state.db.clone()), ExtensionDiagnosticAssociations { project_id: project_id.to_owned(), task_id: None, session_id: None, load_attempt_id }, facts).await.map_err(|error| format!("failed to persist extension diagnostics probe: {error}"))
}
