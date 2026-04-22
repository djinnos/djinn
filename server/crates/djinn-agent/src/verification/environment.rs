//! Fetch a project's `environment_config.verification` block from Dolt.
//!
//! This is the post-P8 replacement for the `.djinn/settings.json` reader in
//! `verification/settings.rs`. All verification setup commands and glob rules
//! now live in `projects.environment_config` (JSON column), written by the P5
//! boot reseed hook and edited via the `project_environment_config_set` MCP
//! tool.
//!
//! ## Lookup modes
//!
//! The fetch helpers accept either a project id (preferred — the in-process
//! caller already has it) or a workspace path (used by the verification
//! pipeline, which runs against an ephemeral mirror clone and doesn't know the
//! project id up-front). The path form delegates to
//! [`djinn_db::ProjectRepository::resolve_id_by_path_fuzzy`] so that a
//! subdirectory inside a project still resolves to the correct row.
//!
//! ## Soft-failure policy
//!
//! Every edge case — missing row, `'{}'` (pre-reseed) column, malformed JSON,
//! forward-incompatible `schema_version` — degrades to an empty
//! [`djinn_stack::environment::Verification`]. The verification pipeline then
//! treats the project as having no rules/setup, which is the correct
//! "no-op / vacuous pass" behaviour. We log a `warn` so misconfiguration is
//! visible in the logs without blocking the task.

use std::path::Path;

use djinn_db::{Database, ProjectRepository};
use djinn_stack::environment::{EnvironmentConfig, Verification};

/// Resolve a project id from a workspace path (exact or fuzzy prefix match).
///
/// Returns `None` when no project row has a path that is a prefix of
/// `worktree_path`. Errors from the Dolt lookup are also surfaced as `None`
/// (with a warn log) so a broken DB connection can't block verification.
async fn resolve_project_id_for_path(db: &Database, worktree_path: &Path) -> Option<String> {
    let repo = ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    let path_str = worktree_path.to_string_lossy();
    match repo.resolve_id_by_path_fuzzy(&path_str).await {
        Ok(Some(id)) => Some(id),
        Ok(None) => {
            tracing::debug!(
                path = %path_str,
                "verification::environment: no project row matched path; using empty verification config"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path_str,
                "verification::environment: failed to resolve project id from path; using empty verification config"
            );
            None
        }
    }
}

/// Fetch + deserialize the full `environment_config` blob for a project id.
///
/// Returns [`EnvironmentConfig::empty`] for every failure / missing path
/// described in the module-level docs. This is the single entry point used by
/// verification, MCP default resolution, and skills; callers extract whichever
/// sub-field they need from the returned config.
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
                "verification::environment: no projects row; using empty environment config"
            );
            return EnvironmentConfig::empty();
        }
        Err(e) => {
            tracing::warn!(
                project_id = %project_id,
                error = %e,
                "verification::environment: failed to fetch environment_config; using empty environment config"
            );
            return EnvironmentConfig::empty();
        }
    };

    match serde_json::from_str::<EnvironmentConfig>(&raw) {
        Ok(cfg) if cfg.schema_version == 0 => {
            // Column still holds the migration-10 `'{}'` default; P5 reseed
            // hook hasn't run yet (or the row pre-dates the hook).
            tracing::debug!(
                project_id = %project_id,
                "verification::environment: environment_config schema_version=0 (pre-reseed); using empty environment config"
            );
            EnvironmentConfig::empty()
        }
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!(
                project_id = %project_id,
                error = %e,
                "verification::environment: failed to deserialize environment_config; using empty environment config"
            );
            EnvironmentConfig::empty()
        }
    }
}

/// Fetch + deserialize the full `environment_config` blob for a workspace
/// path. Convenience wrapper over [`environment_config_for_project_id`] for
/// callers that only have a path (e.g. the chat endpoint + in-worktree session
/// lifecycle), not a project id.
pub async fn environment_config_for_path(db: &Database, worktree_path: &Path) -> EnvironmentConfig {
    match resolve_project_id_for_path(db, worktree_path).await {
        Some(id) => environment_config_for_project_id(db, &id).await,
        None => EnvironmentConfig::empty(),
    }
}

/// Fetch + deserialize the `environment_config.verification` block for a
/// project id. Thin wrapper over [`environment_config_for_project_id`]; kept
/// for call-site readability on the verification pipeline.
pub async fn verification_for_project_id(db: &Database, project_id: &str) -> Verification {
    environment_config_for_project_id(db, project_id)
        .await
        .verification
}

/// Fetch + deserialize the `environment_config.verification` block for a
/// workspace path. Convenience wrapper over [`verification_for_project_id`]
/// for the verification pipeline which only has a path to the ephemeral
/// mirror clone, not a project id.
pub async fn verification_for_path(db: &Database, worktree_path: &Path) -> Verification {
    environment_config_for_path(db, worktree_path).await.verification
}

/// Convert a [`djinn_stack::environment::HookCommand`] list (the canonical
/// schema) into the legacy [`djinn_core::commands::CommandSpec`] shape so the
/// existing command runner (`crate::commands::run_commands`) can execute it
/// unchanged.
///
/// Only the `Shell(String)` variant maps cleanly today — that's the shape the
/// P5 reseed hook emits, and the only shape the old `.djinn/settings.json`
/// setup list expressed. `Exec(argv)` is joined into a single `sh -c` string
/// best-effort; `Parallel` is flattened in declaration order (no actual
/// parallelism), matching the sequential behaviour of the old setup loop.
/// Both fallbacks emit a warn log so anyone leaning on exec/parallel in
/// `environment_config.verification.setup` finds out up-front that the
/// agent-side runner doesn't yet honour those variants.
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
                    "verification::environment: Exec-form setup hooks are flattened to `sh -c`; prefer Shell form in environment_config.verification.setup"
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
                    "verification::environment: Parallel-form setup hooks run sequentially on the agent side"
                );
                for (child_name, child) in map {
                    let child_specs =
                        hook_commands_to_specs(std::slice::from_ref(child));
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

/// Naïve argv → `sh -c` serialiser for `Exec` hooks. Wraps each arg in single
/// quotes and escapes embedded single quotes. Not a security boundary — the
/// argv came from the project's own `environment_config`, which is
/// user-controlled anyway.
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

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::events::EventBus;
    use djinn_stack::environment::{HookCommand, VerificationRule};

    fn test_db() -> Database {
        Database::open_in_memory().expect("in-memory db")
    }

    async fn seed_project_with_config(db: &Database, id: &str, config_json: &str) {
        db.ensure_initialized().await.unwrap();
        let repo = ProjectRepository::new(db.clone(), EventBus::noop());
        repo.create_with_id(id, &format!("p-{id}"), &format!("/tmp/p-{id}"))
            .await
            .unwrap();
        repo.set_environment_config(id, config_json).await.unwrap();
    }

    fn minimal_cfg_json_with_rules(rules: Vec<VerificationRule>) -> String {
        let mut cfg = EnvironmentConfig::empty();
        cfg.verification.rules = rules;
        serde_json::to_string(&cfg).unwrap()
    }

    #[tokio::test]
    async fn missing_project_returns_empty_verification() {
        let db = test_db();
        db.ensure_initialized().await.unwrap();
        let v = verification_for_project_id(&db, "does-not-exist").await;
        assert!(v.rules.is_empty());
        assert!(v.setup.is_empty());
    }

    #[tokio::test]
    async fn empty_schema_version_returns_empty_verification() {
        let db = test_db();
        // After create_with_id, the DEFAULT environment_config is '{}', which
        // deserializes to schema_version = 0. That's the pre-reseed sentinel.
        db.ensure_initialized().await.unwrap();
        ProjectRepository::new(db.clone(), EventBus::noop())
            .create_with_id("p1", "p1", "/tmp/p1")
            .await
            .unwrap();
        let v = verification_for_project_id(&db, "p1").await;
        assert!(v.rules.is_empty());
        assert!(v.setup.is_empty());
    }

    #[tokio::test]
    async fn returns_parsed_verification_rules() {
        let db = test_db();
        let rules = vec![VerificationRule {
            match_pattern: "crates/**".into(),
            commands: vec!["cargo test".into()],
        }];
        seed_project_with_config(&db, "p1", &minimal_cfg_json_with_rules(rules)).await;
        let v = verification_for_project_id(&db, "p1").await;
        assert_eq!(v.rules.len(), 1);
        assert_eq!(v.rules[0].match_pattern, "crates/**");
        assert_eq!(v.rules[0].commands, vec!["cargo test"]);
    }

    #[tokio::test]
    async fn malformed_config_returns_empty_verification() {
        let db = test_db();
        seed_project_with_config(&db, "p1", "{not valid json").await;
        let v = verification_for_project_id(&db, "p1").await;
        assert!(v.rules.is_empty());
    }

    #[test]
    fn hook_commands_to_specs_handles_shell_form() {
        let hooks = vec![
            HookCommand::Shell("cargo fetch".into()),
            HookCommand::Shell("pnpm install".into()),
        ];
        let specs = hook_commands_to_specs(&hooks);
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "setup-1");
        assert_eq!(specs[0].command, "cargo fetch");
        assert_eq!(specs[1].name, "setup-2");
        assert_eq!(specs[1].command, "pnpm install");
    }

    #[test]
    fn hook_commands_to_specs_flattens_exec_form() {
        let hooks = vec![HookCommand::Exec(vec![
            "echo".into(),
            "hello world".into(),
        ])];
        let specs = hook_commands_to_specs(&hooks);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].command, "'echo' 'hello world'");
    }

    #[test]
    fn hook_commands_to_specs_flattens_parallel_form_sequentially() {
        use std::collections::BTreeMap;
        let mut map = BTreeMap::new();
        map.insert("a".to_string(), HookCommand::Shell("echo a".into()));
        map.insert("b".to_string(), HookCommand::Shell("echo b".into()));
        let hooks = vec![HookCommand::Parallel(map)];
        let specs = hook_commands_to_specs(&hooks);
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "setup-1-a");
        assert_eq!(specs[0].command, "echo a");
        assert_eq!(specs[1].name, "setup-1-b");
        assert_eq!(specs[1].command, "echo b");
    }

    #[tokio::test]
    async fn verification_for_path_resolves_by_fuzzy_prefix() {
        let db = test_db();
        db.ensure_initialized().await.unwrap();
        let repo = ProjectRepository::new(db.clone(), EventBus::noop());
        repo.create_with_id("p1", "p1", "/tmp/fuzzy-proj")
            .await
            .unwrap();
        let rules = vec![VerificationRule {
            match_pattern: "**".into(),
            commands: vec!["cargo test".into()],
        }];
        repo.set_environment_config("p1", &minimal_cfg_json_with_rules(rules))
            .await
            .unwrap();

        // A subdirectory under the project path should resolve fuzzy-prefix.
        let v = verification_for_path(&db, Path::new("/tmp/fuzzy-proj/crates/foo")).await;
        assert_eq!(v.rules.len(), 1);
    }
}
