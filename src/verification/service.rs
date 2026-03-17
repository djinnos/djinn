use std::path::Path;
use std::time::Instant;

use djinn_agent::commands::run_commands;
use djinn_core::commands::CommandResult;
use crate::db::VerificationCacheRepository;
use crate::error::Result;

use super::settings::load_commands;

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
/// Setup commands always run (even on cache hit).
/// Verification commands are skipped when a passing cached result exists for
/// (project_id, commit_sha).
pub async fn verify_commit(
    project_id: &str,
    commit_sha: &str,
    worktree_path: &Path,
    db: &crate::db::connection::Database,
) -> Result<VerificationResult> {
    let start = Instant::now();
    let cache_repo = VerificationCacheRepository::new(db.clone());

    let (setup_commands, verification_commands) = load_commands(worktree_path)
        .map_err(crate::error::Error::Internal)?;

    let setup_results = run_commands(&setup_commands, worktree_path)
        .await
        .map_err(|e| crate::error::Error::Internal(e.to_string()))?;

    let cached = cache_repo.get(project_id, commit_sha).await?.is_some();
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

    let verification_results = run_commands(&verification_commands, worktree_path)
        .await
        .map_err(|e| crate::error::Error::Internal(e.to_string()))?;
    let passed = verification_results
        .last()
        .map(|r| r.exit_code == 0)
        .unwrap_or(true);

    if passed {
        let output_json = serde_json::to_string(&verification_results).map_err(|e| {
            crate::error::Error::Internal(format!("failed to serialize verification results: {e}"))
        })?;
        let verification_duration_ms: u64 = verification_results.iter().map(|r| r.duration_ms).sum();
        cache_repo
            .insert(
                project_id,
                commit_sha,
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
    use crate::db::VerificationCacheRepository;
    use crate::db::connection::Database;
    use tempfile::tempdir;

    fn test_db() -> Database {
        Database::open_in_memory().expect("in-memory db")
    }

    /// Write a `.djinn/settings.json` into the given directory.
    fn write_settings(dir: &Path, setup_json: &str, verification_json: &str) {
        let djinn_dir = dir.join(".djinn");
        std::fs::create_dir_all(&djinn_dir).expect("create .djinn");
        std::fs::write(
            djinn_dir.join("settings.json"),
            format!(r#"{{"setup": {setup_json}, "verification": {verification_json}}}"#),
        )
        .expect("write settings.json");
    }

    #[tokio::test]
    async fn verify_commit_cache_miss_runs_full_pipeline_and_caches() {
        let dir = tempdir().expect("tempdir");
        let marker = dir.path().join("setup_ran");
        write_settings(
            dir.path(),
            &format!(
                r#"[{{"name":"setup","command":"touch {}","timeout_secs":10}}]"#,
                marker.display()
            ),
            r#"[{"name":"verify","command":"echo ok","timeout_secs":10}]"#,
        );
        let state = test_db();

        let result = verify_commit("p1", "sha1", dir.path(), &state)
            .await
            .expect("verify");

        assert!(result.passed);
        assert!(!result.cached);
        assert_eq!(result.setup_results.len(), 1);
        assert_eq!(result.verification_results.len(), 1);
        assert!(marker.exists());

        let repo = VerificationCacheRepository::new(state.clone());
        let cached = repo.get("p1", "sha1").await.expect("get cache");
        assert!(cached.is_some());
    }

    #[tokio::test]
    async fn verify_commit_cache_hit_runs_setup_only_and_skips_verification() {
        let dir = tempdir().expect("tempdir");
        let setup_marker = dir.path().join("setup_ran");
        let verify_marker = dir.path().join("verify_ran");
        write_settings(
            dir.path(),
            &format!(
                r#"[{{"name":"setup","command":"touch {}","timeout_secs":10}}]"#,
                setup_marker.display()
            ),
            &format!(
                r#"[{{"name":"verify","command":"touch {}","timeout_secs":10}}]"#,
                verify_marker.display()
            ),
        );
        let state = test_db();
        let repo = VerificationCacheRepository::new(state.clone());
        let serialized = serde_json::to_string(&vec![CommandResult {
            name: "verify".into(),
            exit_code: 0,
            stdout: "ok".into(),
            stderr: String::new(),
            duration_ms: 1,
        }])
        .expect("serialize");
        repo.insert("p1", "sha2", &serialized, 1)
            .await
            .expect("seed cache");

        let result = verify_commit("p1", "sha2", dir.path(), &state)
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
        let dir = tempdir().expect("tempdir");
        write_settings(
            dir.path(),
            r#"[{"name":"setup","command":"echo setup","timeout_secs":10}]"#,
            r#"[{"name":"verify","command":"false","timeout_secs":10}]"#,
        );
        let state = test_db();

        let result = verify_commit("p1", "sha3", dir.path(), &state)
            .await
            .expect("verify");

        assert!(!result.passed);
        assert!(!result.cached);
        assert_eq!(result.verification_results.len(), 1);
        assert_ne!(result.verification_results[0].exit_code, 0);

        let repo = VerificationCacheRepository::new(state.clone());
        let cached = repo.get("p1", "sha3").await.expect("get cache");
        assert!(cached.is_none());
    }

    #[tokio::test]
    async fn verify_commit_no_settings_file_passes_with_no_commands() {
        let dir = tempdir().expect("tempdir");
        let state = test_db();

        let result = verify_commit("p1", "sha5", dir.path(), &state)
            .await
            .expect("verify");

        assert!(result.passed);
        assert!(!result.cached);
        assert!(result.setup_results.is_empty());
        assert!(result.verification_results.is_empty());
    }
}
