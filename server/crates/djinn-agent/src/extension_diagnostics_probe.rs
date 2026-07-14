use std::path::Path;

use djinn_core::extension_diagnostics::ExtensionLoadDiagnosticV1;
use djinn_db::ExtensionLoadDiagnosticRepository;

use crate::context::AgentContext;
use crate::environment::environment_config_for_project_id;
use crate::extension_diagnostics::{
    ExtensionDiagnosticAssociations, persist_extension_diagnostic_batch,
};
use crate::mcp_client::connect_and_discover_with_diagnostics;
use crate::mcp_settings::{
    effective_mcp_server_names, effective_skill_names, load_mcp_server_registry,
    resolve_mcp_servers,
};
use crate::skills_manifest::load_verified_skills_detailed;

/// Runs one fresh, read-only doctor extension diagnostics attempt.
pub async fn probe_project_extensions(
    project_id: &str,
    canonical_workspace: &Path,
    app_state: &AgentContext,
) -> Result<Vec<ExtensionLoadDiagnosticV1>, String> {
    let env = environment_config_for_project_id(&app_state.db, project_id).await;
    let mcp_names = effective_mcp_server_names(&env.agent_mcp_defaults, "doctor", None);
    let skill_names = effective_skill_names(&env.global_skills, &[]);
    let load_attempt_id = uuid::Uuid::now_v7().to_string();
    let registry = load_mcp_server_registry(canonical_workspace);
    let servers = resolve_mcp_servers("doctor", "doctor", &mcp_names, &registry)
        .into_iter()
        .map(|(name, config)| (name, config.clone()))
        .collect::<Vec<_>>();
    let mcp = connect_and_discover_with_diagnostics("doctor", "doctor", &servers, app_state).await;
    let skills = load_verified_skills_detailed(canonical_workspace, &skill_names);
    let mut facts = mcp.diagnostics;
    facts.extend(skills.diagnostics);
    persist_extension_diagnostic_batch(
        |_| false,
        &ExtensionLoadDiagnosticRepository::new(app_state.db.clone()),
        ExtensionDiagnosticAssociations {
            project_id: project_id.to_owned(),
            task_id: None,
            session_id: None,
            load_attempt_id,
        },
        facts,
    )
    .await
    .map_err(|error| format!("failed to persist extension diagnostics probe: {error}"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use djinn_core::events::EventBus;
    use djinn_core::extension_diagnostics::{ExtensionLoadPhase, ExtensionLoadSourceKind};
    use djinn_db::{ExtensionLoadDiagnosticRepository, ProjectRepository};
    use tokio_util::sync::CancellationToken;

    use super::*;

    #[tokio::test]
    async fn probe_uses_existing_detectors_and_returns_fresh_canonical_rows() {
        let db = djinn_db::Database::open_in_memory().expect("in-memory database");
        let project_id = uuid::Uuid::now_v7().to_string();
        ProjectRepository::new(db.clone(), EventBus::noop())
            .create_with_id(&project_id, "doctor probe", "test", "doctor-probe")
            .await
            .expect("project");
        ProjectRepository::new(db.clone(), EventBus::noop())
            .set_environment_config(
                &project_id,
                &serde_json::json!({
                    "schema_version": 1,
                    "agent_mcp_defaults": { "doctor": ["missing-placeholder"] },
                    "global_skills": ["missing-skill"]
                })
                .to_string(),
            )
            .await
            .expect("environment config");

        let workspace = crate::test_helpers::test_tempdir("doctor-extension-probe-");
        fs::write(
            workspace.path().join("mcp.json"),
            r#"{"mcpServers":{"missing-placeholder":{"url":"${DOCTOR_PROBE_UNSET}"}}}"#,
        )
        .expect("MCP fixture");
        let app_state =
            crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());

        let first = probe_project_extensions(&project_id, workspace.path(), &app_state)
            .await
            .expect("first probe");
        let second = probe_project_extensions(&project_id, workspace.path(), &app_state)
            .await
            .expect("second probe");

        assert!(first.iter().any(|row| {
            row.source_kind == ExtensionLoadSourceKind::ProjectMcp
                && row.phase == ExtensionLoadPhase::PlaceholderResolution
        }));
        assert!(
            first
                .iter()
                .any(|row| row.source_kind == ExtensionLoadSourceKind::ProjectSkill)
        );
        assert!(
            first
                .iter()
                .all(|row| row.task_id.is_none() && row.session_id.is_none())
        );
        assert!(!first.is_empty());
        assert!(!second.is_empty());
        assert_ne!(first[0].load_attempt_id, second[0].load_attempt_id);

        let repository = ExtensionLoadDiagnosticRepository::new(db);
        assert_eq!(
            first,
            repository
                .list_for_load_attempt(&project_id, &first[0].load_attempt_id)
                .await
                .expect("canonical first attempt")
        );
        assert_eq!(
            second,
            repository
                .list_for_load_attempt(&project_id, &second[0].load_attempt_id)
                .await
                .expect("canonical second attempt")
        );
    }
}
