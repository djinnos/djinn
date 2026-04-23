use std::path::Path;
use std::time::Instant;

use crate::commands::run_commands;
use anyhow::Result;
use djinn_core::commands::{CommandResult, CommandSpec};
use djinn_db::VerificationCacheRepository;

use super::environment::{environment_config_for_project_id, hook_commands_to_specs};
use super::settings::verification_cache_key;

#[derive(Debug, Clone)]
pub struct VerificationResult {
    pub passed: bool,
    pub cached: bool,
    pub setup_results: Vec<CommandResult>,
    pub verification_results: Vec<CommandResult>,
    pub total_duration_ms: u64,
}

/// Run setup + verification commands for a commit.
///
/// Setup commands are read from
/// `environment_config.lifecycle.pre_verification` in Dolt. They always
/// run, even on cache hit.
///
/// Verification commands are skipped when a passing cached result exists for
/// `(project_id, cache_key)` where `cache_key` encodes both the commit SHA and
/// the resolved scoped command set.
///
/// `scoped_commands` is the pre-resolved list of shell command strings to run
/// as the verification phase.  Pass an empty slice to skip verification
/// (vacuous pass, no caching).
pub async fn verify_commit(
    project_id: &str,
    commit_sha: &str,
    worktree_path: &Path,
    db: &djinn_db::Database,
    scoped_commands: &[String],
) -> Result<VerificationResult> {
    let start = Instant::now();
    let cache_repo = VerificationCacheRepository::new(db.clone());

    let env_config = environment_config_for_project_id(db, project_id).await;
    let setup_commands: Vec<CommandSpec> =
        hook_commands_to_specs(&env_config.lifecycle.pre_verification);

    let setup_results = run_commands(&setup_commands, worktree_path)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    let verification_specs: Vec<CommandSpec> = scoped_commands
        .iter()
        .enumerate()
        .map(|(i, cmd)| CommandSpec {
            name: format!("verify-{}", i + 1),
            command: cmd.clone(),
            timeout_secs: None,
        })
        .collect();

    let cache_key = verification_cache_key(commit_sha, scoped_commands);

    let cached = cache_repo.get(project_id, &cache_key).await?.is_some();
    if cached {
        let total_duration_ms = start.elapsed().as_millis() as u64;
        return Ok(VerificationResult {
            passed: true,
            cached: true,
            setup_results,
            verification_results: Vec::new(),
            total_duration_ms,
        });
    }

    let verification_results = run_commands(&verification_specs, worktree_path)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    let passed = verification_results
        .last()
        .map(|r| r.exit_code == 0)
        .unwrap_or(true);

    if passed {
        let output_json = serde_json::to_string(&verification_results)
            .map_err(|e| anyhow::anyhow!("failed to serialize verification results: {e}"))?;
        let verification_duration_ms: u64 =
            verification_results.iter().map(|r| r.duration_ms).sum();
        cache_repo
            .insert(
                project_id,
                &cache_key,
                &output_json,
                verification_duration_ms as i64,
            )
            .await?;
    }

    let total_duration_ms = start.elapsed().as_millis() as u64;
    Ok(VerificationResult {
        passed,
        cached: false,
        setup_results,
        verification_results,
        total_duration_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::commands::CommandResult;
    use djinn_core::events::EventBus;
    use djinn_db::Database;
    use djinn_db::ProjectRepository;
    use djinn_db::VerificationCacheRepository;
    use djinn_stack::environment::{EnvironmentConfig, HookCommand};

    fn test_db() -> Database {
        Database::open_in_memory().expect("in-memory db")
    }

    /// Insert a `projects` row with id `project_id` so that tests can seed
    /// `verification_cache` rows referencing it without tripping the
    /// `fk_verification_cache_project` foreign-key constraint on Dolt/MySQL.
    async fn seed_project(db: &Database, project_id: &str) {
        db.ensure_initialized().await.expect("init schema");
        let name = format!("test-project-{project_id}");
        let path = format!("/tmp/{project_id}");
        djinn_db::ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop())
            .create_with_id(project_id, &name, &path)
            .await
            .expect("seed project row for FK");
    }

    async fn seed_project_with_setup(db: &Database, project_id: &str, setup: Vec<HookCommand>) {
        seed_project(db, project_id).await;
        let repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let mut cfg = EnvironmentConfig::empty();
        cfg.lifecycle.pre_verification = setup;
        let raw = serde_json::to_string(&cfg).unwrap();
        repo.set_environment_config(project_id, &raw).await.unwrap();
    }

    fn tempdir_in_tmp() -> tempfile::TempDir {
        crate::test_helpers::test_tempdir("djinn-verification-")
    }

    #[tokio::test]
    async fn verify_commit_cache_miss_runs_full_pipeline_and_caches() {
        let dir = tempdir_in_tmp();
        let marker = dir.path().join("setup_ran");
        let state = test_db();
        seed_project_with_setup(
            &state,
            "p1",
            vec![HookCommand::Shell(format!("touch {}", marker.display()))],
        )
        .await;
        let scoped = vec!["echo ok".to_string()];

        let result = verify_commit("p1", "sha1", dir.path(), &state, &scoped)
            .await
            .expect("verify");

        assert!(result.passed);
        assert!(!result.cached);
        assert_eq!(result.setup_results.len(), 1);
        assert_eq!(result.verification_results.len(), 1);
        assert!(marker.exists());

        let repo = VerificationCacheRepository::new(state.clone());
        let cache_key = super::super::settings::verification_cache_key("sha1", &scoped);
        let cached = repo.get("p1", &cache_key).await.expect("get cache");
        assert!(cached.is_some());
    }

    #[tokio::test]
    async fn verify_commit_cache_hit_runs_setup_only_and_skips_verification() {
        let dir = tempdir_in_tmp();
        let setup_marker = dir.path().join("setup_ran");
        let verify_marker = dir.path().join("verify_ran");
        let state = test_db();
        seed_project_with_setup(
            &state,
            "p1",
            vec![HookCommand::Shell(format!(
                "touch {}",
                setup_marker.display()
            ))],
        )
        .await;
        let repo = VerificationCacheRepository::new(state.clone());
        let scoped = vec![format!("touch {}", verify_marker.display())];
        let cache_key = super::super::settings::verification_cache_key("sha2", &scoped);
        let serialized = serde_json::to_string(&vec![CommandResult {
            name: "verify-1".into(),
            command: scoped[0].clone(),
            exit_code: 0,
            stdout: "ok".into(),
            stderr: String::new(),
            duration_ms: 1,
        }])
        .expect("serialize");
        repo.insert("p1", &cache_key, &serialized, 1)
            .await
            .expect("seed cache");

        let result = verify_commit("p1", "sha2", dir.path(), &state, &scoped)
            .await
            .expect("verify");

        assert!(result.passed);
        assert!(result.cached);
        assert_eq!(result.setup_results.len(), 1);
        assert!(result.verification_results.is_empty());
        assert!(setup_marker.exists());
        assert!(!verify_marker.exists());
    }

    #[tokio::test]
    async fn verify_commit_failure_is_not_cached() {
        let dir = tempdir_in_tmp();
        let state = test_db();
        seed_project_with_setup(
            &state,
            "p1",
            vec![HookCommand::Shell("echo setup".into())],
        )
        .await;
        let scoped = vec!["false".to_string()];

        let result = verify_commit("p1", "sha3", dir.path(), &state, &scoped)
            .await
            .expect("verify");

        assert!(!result.passed);
        assert!(!result.cached);
        assert_eq!(result.verification_results.len(), 1);
        assert_ne!(result.verification_results[0].exit_code, 0);

        let repo = VerificationCacheRepository::new(state.clone());
        let cache_key = super::super::settings::verification_cache_key("sha3", &scoped);
        let cached = repo.get("p1", &cache_key).await.expect("get cache");
        assert!(cached.is_none());
    }

    #[tokio::test]
    async fn verify_commit_no_commands_passes_vacuously() {
        let dir = tempdir_in_tmp();
        let state = test_db();
        seed_project(&state, "p1").await;

        let result = verify_commit("p1", "sha5", dir.path(), &state, &[])
            .await
            .expect("verify");

        assert!(result.passed);
        assert!(!result.cached);
        assert!(result.setup_results.is_empty());
        assert!(result.verification_results.is_empty());
    }

    #[tokio::test]
    async fn verify_commit_different_scoped_commands_get_different_cache_keys() {
        let dir = tempdir_in_tmp();
        let state = test_db();
        seed_project(&state, "p1").await;

        let full_cmds = vec!["echo full".to_string()];
        let result = verify_commit("p1", "sha6", dir.path(), &state, &full_cmds)
            .await
            .expect("verify full");
        assert!(result.passed);

        let scoped_cmds = vec!["echo scoped".to_string()];
        let result = verify_commit("p1", "sha6", dir.path(), &state, &scoped_cmds)
            .await
            .expect("verify scoped");
        assert!(!result.cached);
    }
}
