//! Fetch a project's `environment_config` block from Postgres.
//!
//! All workspace/setup config now lives in `projects.environment_config` (JSON
//! column), written by the boot reseed hook and edited via the
//! `project_environment_config_set` MCP tool.
//!
//! ## Lookup modes
//!
//! The fetch helpers accept either a project id (preferred — the in-process
//! caller already has it) or a workspace path (used by callers that run against
//! an ephemeral mirror clone and don't know the project id up-front). The path
//! form reverse-parses the canonical `{projects_root}/{owner}/{repo}`
//! clone-path shape and looks the project up by GitHub coords via
//! [`djinn_db::ProjectRepository::get_by_github`].
//!
//! ## Soft-failure policy
//!
//! Every edge case — missing row, `'{}'` (pre-reseed) column, malformed JSON,
//! forward-incompatible `schema_version` — degrades to an empty
//! [`djinn_stack::environment::EnvironmentConfig`]. We log a `warn` so
//! misconfiguration is visible in the logs without blocking the task.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::process::Command;

use djinn_db::{Database, ProjectRepository};
use djinn_stack::environment::EnvironmentConfig;

/// The explicitly admitted environment for a child that is not launched by the
/// cgroup launcher.
///
/// The launcher receives a complete `CommandSpec` environment and validates it
/// before `execve`; a launcher-free child used to inherit the worker's entire
/// environment instead. Keep the two paths under the same entry policy. In
/// particular, `GIT_CONFIG_SYSTEM` is an entry-level decision: its value must
/// be the launcher trust anchor, not merely a variable with a familiar name.
///
/// Explicit command values override inherited values, while `env_remove` wins
/// over both. Entries outside the policy are omitted rather than passed to the
/// child. The policy is deliberately delegated to the launcher crate so this
/// path shares its `git_trust::is_anchor_value` check instead of growing a
/// second, key-name-only trust predicate.
pub(crate) fn admitted_child_environment(command: &Command) -> io::Result<Vec<(String, String)>> {
    let mut environment: BTreeMap<String, String> = std::env::vars()
        .filter(|(key, value)| djinn_cgroup_launcher::is_allowed_environment_entry(key, value))
        .collect();

    for (key, value) in command.get_envs() {
        let key = key.to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "environment key must be UTF-8")
        })?;
        match value {
            // `Command::env_remove` is represented as a `None` value.
            None => {
                environment.remove(key);
            }
            Some(value) => {
                let value = value.to_str().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "environment value must be UTF-8",
                    )
                })?;
                if djinn_cgroup_launcher::is_allowed_environment_entry(key, value) {
                    environment.insert(key.to_owned(), value.to_owned());
                } else {
                    environment.remove(key);
                }
            }
        }
    }

    Ok(environment.into_iter().collect())
}

/// Replace inherited state with only [`admitted_child_environment`] before a
/// launcher-free child is spawned.
///
/// `Command::env_clear` is load-bearing: filtering only `get_envs()` would
/// leave inherited variables such as loader injection and Git's indexed config
/// protocol intact.
pub(crate) fn clear_and_admit_child_environment(command: &mut Command) -> io::Result<()> {
    let environment = admitted_child_environment(command)?;
    command.env_clear().envs(environment);
    Ok(())
}

/// Resolve a project id from a workspace path (exact or fuzzy prefix match).
///
/// Returns `None` when no project row has a path that is a prefix of
/// `worktree_path`. Errors from the lookup are also surfaced as `None`
/// (with a warn log) so a broken DB connection can't block the caller.
async fn resolve_project_id_for_path(db: &Database, worktree_path: &Path) -> Option<String> {
    let repo = ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    let path_str = worktree_path.to_string_lossy();
    // The server-managed clone path has the shape
    // `{projects_root}/{owner}/{repo}` — but workers operate on worktrees
    // rooted *under* that path (e.g. `{projects_root}/{owner}/{repo}/.task-runtime/worktrees/<id>`),
    // so an exact last-two-components parse misses anything deeper than
    // the clone root. Walk the ancestry: at each ancestor, take its last
    // two components and try a GitHub lookup. First hit wins.
    let components: Vec<String> = worktree_path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if components.len() < 2 {
        tracing::debug!(
            path = %path_str,
            "environment: path too short to parse owner/repo; using empty environment config"
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
                    "environment: failed to resolve project id from path; using empty environment config"
                );
                return None;
            }
        }
    }
    tracing::debug!(
        path = %path_str,
        "environment: no project row matched any ancestor owner/repo; using empty environment config"
    );
    None
}

/// Fetch + deserialize the full `environment_config` blob for a project id.
///
/// Returns [`EnvironmentConfig::empty`] for every failure / missing path
/// described in the module-level docs. This is the single entry point used by
/// MCP default resolution and skills; callers extract whichever sub-field they
/// need from the returned config.
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
                "environment: no projects row; using empty environment config"
            );
            return EnvironmentConfig::empty();
        }
        Err(e) => {
            tracing::warn!(
                project_id = %project_id,
                error = %e,
                "environment: failed to fetch environment_config; using empty environment config"
            );
            return EnvironmentConfig::empty();
        }
    };

    match serde_json::from_str::<EnvironmentConfig>(&raw) {
        Ok(cfg) if cfg.schema_version == 0 => {
            // Column still holds the migration-10 `'{}'` default; reseed
            // hook hasn't run yet (or the row pre-dates the hook).
            tracing::debug!(
                project_id = %project_id,
                "environment: environment_config schema_version=0 (pre-reseed); using empty environment config"
            );
            EnvironmentConfig::empty()
        }
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!(
                project_id = %project_id,
                error = %e,
                "environment: failed to deserialize environment_config; using empty environment config"
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

/// A workspace path resolved to its owning project id (if any) plus the
/// project's `environment_config`.
///
/// Bundling both lets callers that need the project id *and* the config (e.g.
/// the build-drift soft gate, which folds the project id into its deny key and
/// reads the lifecycle configuration from the config) do a
/// single project-id resolution instead of two.
pub struct ResolvedProjectEnvironment {
    /// The resolved owning project id, or `None` when the path matches no row.
    pub project_id: Option<String>,
    /// The project's environment config (empty on any soft-failure).
    pub config: EnvironmentConfig,
}

/// Resolve `worktree_path` to its owning project id and environment config in
/// one pass. Both degrade softly: an unresolved path yields
/// `project_id = None` and an empty config.
pub async fn resolved_project_environment_for_path(
    db: &Database,
    worktree_path: &Path,
) -> ResolvedProjectEnvironment {
    match resolve_project_id_for_path(db, worktree_path).await {
        Some(id) => {
            let config = environment_config_for_project_id(db, &id).await;
            ResolvedProjectEnvironment {
                project_id: Some(id),
                config,
            }
        }
        None => ResolvedProjectEnvironment {
            project_id: None,
            config: EnvironmentConfig::empty(),
        },
    }
}

/// Decide whether a project has indexable code for the canonical-graph
/// warmer — the gate behind the "code-less repo" warm skip.
///
/// The naive check (`environment_config.languages.has_any()`) is
/// catalog-image-blind: `project_set_image` deliberately does NOT copy the
/// image's config into the project (migration 46), so a project on a shared
/// catalog image keeps an all-null per-project `languages` block while having
/// plenty of code. Gating warm on that block alone freezes such a project's
/// graph forever and stamps the `__djinn_no_code__` sentinel every tick.
///
/// Effective signal, in order:
/// 1. per-project `languages` configured, or
/// 2. per-project `workspaces` declared (each carries a language), or
/// 3. the assigned catalog image's config declares languages/workspaces.
///
/// Lookup *errors* on the catalog fallback resolve to `true` (attempt the
/// warm) — wrongly skipping freezes the graph indefinitely, while wrongly
/// warming costs one Job that the warmer's own freshness gate bounds.
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
                    "environment: catalog image config unparseable; assuming project has code"
                );
                true
            }
        },
        Ok(None) => false,
        Err(e) => {
            tracing::warn!(
                project_id = %project_id,
                error = %e,
                "environment: failed to resolve catalog image; assuming project has code"
            );
            true
        }
    }
}

/// Convert a [`djinn_stack::environment::HookCommand`] list (the canonical
/// schema) into the legacy [`djinn_core::commands::CommandSpec`] shape so the
/// existing command runner (`crate::commands::run_commands`) can execute it
/// unchanged.
///
/// Only the `Shell(String)` variant maps cleanly today — that's the shape the
/// reseed hook emits, and the only shape the old `.djinn/settings.json`
/// setup list expressed. `Exec(argv)` is joined into a single `sh -c` string
/// best-effort; `Parallel` is flattened in declaration order (no actual
/// parallelism), matching the sequential behaviour of the old setup loop.
/// Both fallbacks emit a warn log so anyone leaning on exec/parallel in
/// `environment_config.lifecycle.pre_verification` finds out up-front that
/// the agent-side runner doesn't yet honour those variants.
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
                    "environment: Exec-form setup hooks are flattened to `sh -c`; prefer Shell form in environment_config.lifecycle.pre_verification"
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
                    "environment: Parallel-form setup hooks run sequentially on the agent side"
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
    use djinn_stack::environment::HookCommand;

    fn test_db() -> Database {
        Database::open_in_memory().expect("in-memory db")
    }

    async fn seed_project(db: &Database, id: &str, slug: &str) {
        db.ensure_initialized().await.unwrap();
        ProjectRepository::new(db.clone(), EventBus::noop())
            .create_with_id(id, &format!("p-{id}"), "test", slug)
            .await
            .unwrap();
    }

    async fn set_env_config(db: &Database, id: &str, config: serde_json::Value) {
        ProjectRepository::new(db.clone(), EventBus::noop())
            .set_environment_config(id, &config.to_string())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn has_indexable_code_false_for_empty_config_without_image() {
        let db = test_db();
        seed_project(&db, "p1", "p1").await;
        assert!(!project_has_indexable_code(&db, "p1").await);
    }

    #[tokio::test]
    async fn has_indexable_code_true_when_languages_configured() {
        let db = test_db();
        seed_project(&db, "p1", "p1").await;
        set_env_config(
            &db,
            "p1",
            serde_json::json!({
                "schema_version": 1,
                "languages": { "rust": { "default_toolchain": "stable" } },
            }),
        )
        .await;
        assert!(project_has_indexable_code(&db, "p1").await);
    }

    #[tokio::test]
    async fn has_indexable_code_true_when_only_workspaces_declared() {
        let db = test_db();
        seed_project(&db, "p1", "p1").await;
        set_env_config(
            &db,
            "p1",
            serde_json::json!({
                "schema_version": 1,
                "workspaces": [{ "root": "server", "language": "rust" }],
            }),
        )
        .await;
        assert!(project_has_indexable_code(&db, "p1").await);
    }

    /// The catalog-image regression: `project_set_image` deliberately does
    /// not copy the image's config into the project, so a catalog project's
    /// own languages block stays empty. The gate must fall through to the
    /// image config instead of declaring the project code-less.
    #[tokio::test]
    async fn has_indexable_code_resolves_through_catalog_image() {
        let db = test_db();
        seed_project(&db, "p1", "p1").await;
        let image_repo = djinn_db::ImageRepository::new(db.clone());
        let image_cfg = serde_json::json!({
            "schema_version": 1,
            "languages": {
                "rust": { "default_toolchain": "stable" },
                "node": { "default_version": "26" },
            },
        });
        image_repo
            .create("img1", "Rust + Node", None, &image_cfg.to_string())
            .await
            .unwrap();
        image_repo
            .set_project_image("p1", Some("img1"))
            .await
            .unwrap();
        assert!(project_has_indexable_code(&db, "p1").await);
    }

    #[tokio::test]
    async fn has_indexable_code_false_when_catalog_image_is_also_codeless() {
        let db = test_db();
        seed_project(&db, "p1", "p1").await;
        let image_repo = djinn_db::ImageRepository::new(db.clone());
        image_repo
            .create(
                "img1",
                "Empty",
                None,
                &serde_json::json!({ "schema_version": 1 }).to_string(),
            )
            .await
            .unwrap();
        image_repo
            .set_project_image("p1", Some("img1"))
            .await
            .unwrap();
        assert!(!project_has_indexable_code(&db, "p1").await);
    }

    /// Serialises the tests below: they mutate the process environment, which
    /// every thread in this test binary shares.
    static CHILD_ENVIRONMENT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The platform DSN must not reach a process the agent's shell path spawns.
    ///
    /// Asserted on the child's ACTUAL environment — the bytes a `bash -lc`
    /// prints — rather than on the fact that a filter was called. `bash -lc`
    /// with `printenv` is the exact shape
    /// `extension::handlers::workspace::run_shell` builds, and
    /// `clear_and_admit_child_environment` is the exact call it makes on the
    /// launcher-free path.
    ///
    /// Measured on a live task-run Job spec, 2026-08-04: the worker container
    /// carries `DJINN_DATABASE_URL` pointing at the production platform
    /// database, and it reached this child because the launcher allow-list
    /// admits the whole `DJINN_` prefix.
    #[test]
    #[cfg(unix)]
    fn the_agent_shell_child_cannot_observe_the_platform_dsn() {
        const DSN: &str =
            "postgres://djinn:hunter2@djinn-postgres.djinn.svc.cluster.local:5432/djinn";
        let _guard = CHILD_ENVIRONMENT_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // SAFETY: serialised by `_guard`.
        unsafe {
            std::env::set_var("DJINN_DATABASE_URL", DSN);
            // A sibling on the same open `DJINN_` prefix that is NOT a secret:
            // it must still reach the child, or this would be a scrub of the
            // whole namespace wearing a narrower name.
            std::env::set_var("DJINN_TASK_RUN_ID", "task-run-fixture");
            std::env::set_var("PATH", "/usr/local/bin:/usr/bin:/bin");
        }

        let probe = "printf %s \"${DJINN_DATABASE_URL-}|${DJINN_TASK_RUN_ID-}\"";

        // The launcher-free production path.
        let mut command = Command::new("bash");
        command
            .arg("-lc")
            .arg(probe)
            .current_dir("/")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        clear_and_admit_child_environment(&mut command).expect("admit the child environment");
        let output = command.output().expect("spawn the probe child");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "|task-run-fixture",
            "the platform DSN must be absent from the child's environment, and the \
             non-secret DJINN_ sibling must still be present"
        );

        // The brokered production path builds a `CommandSpec` instead of
        // spawning directly, and the privileged broker re-checks the same
        // predicate before `execve`. Drive the REAL conversion.
        let mut brokered = Command::new("bash");
        // `CommandSpec::validate` restricts the cwd to the mounts a task-run Pod
        // actually has; `/workspace` is the one `run_shell` uses.
        brokered.arg("-lc").arg(probe).current_dir("/workspace");
        let spec = crate::process::command_spec(brokered).expect("build the brokered spec");
        assert!(
            !spec
                .environment
                .iter()
                .any(|(key, _)| key == "DJINN_DATABASE_URL"),
            "the brokered child's environment must not carry the platform DSN"
        );
        assert!(
            !spec
                .environment
                .iter()
                .any(|(_, value)| value.contains(DSN)),
            "and it must not carry the DSN under any other key either"
        );
        assert!(
            spec.environment
                .iter()
                .any(|(key, value)| key == "DJINN_TASK_RUN_ID" && value == "task-run-fixture"),
            "the non-secret DJINN_ namespace must still reach a brokered child"
        );

        // SAFETY: serialised by `_guard`.
        unsafe {
            std::env::remove_var("DJINN_DATABASE_URL");
            std::env::remove_var("DJINN_TASK_RUN_ID");
        }
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
        let hooks = vec![HookCommand::Exec(vec!["echo".into(), "hello world".into()])];
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
}
