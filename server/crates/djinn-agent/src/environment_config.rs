//! Fetch a project's `environment_config` block from Dolt.
//!
//! Extracted from the former `verification::environment` module when the
//! verification pre-PR gate was removed.  These helpers remain because
//! environment-config lookup (MCP defaults, languages, workspaces) is still
//! used by the MCP/skills resolution path and the graph-warmer code-less gate.

use std::path::Path;

use djinn_db::{Database, ProjectRepository};
use djinn_stack::environment::EnvironmentConfig;

/// Resolve a project id from a workspace path (exact or fuzzy prefix match).
///
/// Returns `None` when no project row has a path that is a prefix of
/// `worktree_path`. Errors from the Dolt lookup are also surfaced as `None`
/// (with a warn log) so a broken DB connection can't block resolution.
async fn resolve_project_id_for_path(db: &Database, worktree_path: &Path) -> Option<String> {
    let repo = ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    let path_str = worktree_path.to_string_lossy();
    let components: Vec<String> = worktree_path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if components.len() < 2 {
        tracing::debug!(
            path = %path_str,
            "environment_config: path too short to parse owner/repo; using empty config"
        );
        return None;
    }
    for window_end in (2..=components.len()).rev() {
        let owner_name = &components[window_end - 2];
        let repo_name = &components[window_end - 1];
        match repo.get_by_github(owner_name, repo_name).await {
            Ok(Some(p)) => return Some(p.id),
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %path_str,
                    "environment_config: failed to resolve project id from path; using empty config"
                );
                return None;
            }
        }
    }
    tracing::debug!(
        path = %path_str,
        "environment_config: no project row matched any ancestor owner/repo; using empty config"
    );
    None
}

/// Fetch + deserialize the full `environment_config` blob for a project id.
///
/// Returns [`EnvironmentConfig::empty`] for every failure / missing path.
pub async fn environment_config_for_project_id(
    db: &Database,
    project_id: &str,
) -> EnvironmentConfig {
    let raw = match ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .get_environment_config(project_id)
        .await
    {
        Ok(Some(raw)) => raw,
        Ok(None) => {
            tracing::warn!(
                project_id = %project_id,
                "environment_config: no projects row; using empty environment config"
            );
            return EnvironmentConfig::empty();
        }
        Err(e) => {
            tracing::warn!(
                project_id = %project_id,
                error = %e,
                "environment_config: failed to fetch environment_config; using empty environment config"
            );
            return EnvironmentConfig::empty();
        }
    };

    match serde_json::from_str::<EnvironmentConfig>(&raw) {
        Ok(cfg) if cfg.schema_version == 0 => {
            tracing::debug!(
                project_id = %project_id,
                "environment_config: environment_config schema_version=0 (pre-reseed); using empty environment config"
            );
            EnvironmentConfig::empty()
        }
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!(
                project_id = %project_id,
                error = %e,
                "environment_config: failed to deserialize environment_config; using empty environment config"
            );
            EnvironmentConfig::empty()
        }
    }
}

/// Fetch + deserialize the full `environment_config` blob for a workspace
/// path. Convenience wrapper over [`environment_config_for_project_id`] for
/// callers that only have a path.
pub async fn environment_config_for_path(db: &Database, worktree_path: &Path) -> EnvironmentConfig {
    match resolve_project_id_for_path(db, worktree_path).await {
        Some(id) => environment_config_for_project_id(db, &id).await,
        None => EnvironmentConfig::empty(),
    }
}

/// Decide whether a project has indexable code for the canonical-graph
/// warmer — the gate behind the "code-less repo" warm skip.
pub async fn project_has_indexable_code(db: &Database, project_id: &str) -> bool {
    let cfg = environment_config_for_project_id(db, project_id).await;
    if cfg.languages.has_any() || !cfg.workspaces.is_empty() {
        return true;
    }
    match djinn_db::ImageRepository::new(db.clone())
        .resolve_for_project(project_id)
        .await
    {
        Ok(Some(image)) => match serde_json::from_str::<EnvironmentConfig>(&image.config) {
            Ok(image_cfg) => image_cfg.languages.has_any() || !image_cfg.workspaces.is_empty(),
            Err(e) => {
                tracing::warn!(
                    project_id = %project_id,
                    image_id = %image.id,
                    error = %e,
                    "environment_config: catalog image config unparseable; assuming project has code"
                );
                true
            }
        },
        Ok(None) => false,
        Err(e) => {
            tracing::warn!(
                project_id = %project_id,
                error = %e,
                "environment_config: failed to resolve catalog image; assuming project has code"
            );
            true
        }
    }
}

/// Convert a [`djinn_stack::environment::HookCommand`] list into the legacy
/// [`djinn_core::commands::CommandSpec`] shape so the existing command runner
/// can execute it unchanged.
pub fn hook_commands_to_specs(
    hooks: &[djinn_stack::environment::HookCommand],
) -> Vec<djinn_core::commands::CommandSpec> {
    let mut specs = Vec::with_capacity(hooks.len());
    for (idx, hook) in hooks.iter().enumerate() {
        let name = format!("setup-{}", idx + 1);
        match hook {
            djinn_stack::environment::HookCommand::Shell(cmd) => {
                specs.push(djinn_core::commands::CommandSpec {
                    name,
                    command: cmd.clone(),
                    timeout_secs: None,
                });
            }
            djinn_stack::environment::HookCommand::Exec(argv) => {
                tracing::warn!(
                    index = idx,
                    "environment_config: Exec-form setup hooks are flattened to `sh -c`; prefer Shell form"
                );
                let joined = shell_join_argv(argv);
                specs.push(djinn_core::commands::CommandSpec {
                    name,
                    command: joined,
                    timeout_secs: None,
                });
            }
            djinn_stack::environment::HookCommand::Parallel(map) => {
                tracing::warn!(
                    index = idx,
                    group_size = map.len(),
                    "environment_config: Parallel-form setup hooks run sequentially on the agent side"
                );
                for (child_name, child) in map {
                    let child_specs = hook_commands_to_specs(std::slice::from_ref(child));
                    for mut spec in child_specs {
                        spec.name = format!("{name}-{child_name}");
                        specs.push(spec);
                    }
                }
            }
        }
    }
    specs
}

fn shell_join_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.is_empty() {
                "''".to_string()
            } else {
                let escaped = a.replace('\'', "'\\''");
                format!("'{escaped}'")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
