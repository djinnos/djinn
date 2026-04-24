//! Scoped verification command resolution.
//!
//! Resolves which verification commands should run for the current state of
//! the branch by diffing against the target branch and matching changed files
//! against the project's `verification.rules` glob patterns from
//! `projects.environment_config` in Dolt.

use std::path::Path;

use djinn_db::Database;
use djinn_stack::environment::{Verification, VerificationRule};

use super::environment::verification_for_path;

/// Resolve the set of verification commands to run for the current branch.
///
/// Resolution order (highest to lowest priority):
/// 1. If `role_verification_override` is `Some(cmd)` → return `vec![cmd]`.
/// 2. Fetch `environment_config.verification` from Dolt for the project that
///    owns `worktree_path` (fuzzy prefix match).
/// 3. Run `git diff --name-only <target_branch>..HEAD` to get changed files.
/// 4. Match each changed file against `verification.rules` glob patterns
///    (in config order). Collect + deduplicate commands from all matching
///    rules.
///
/// Returns an empty `Vec` when no verification commands are configured at
/// all, no rules match, or the project row / environment_config can't be
/// found (see [`crate::verification::environment`] for soft-failure rules).
pub async fn resolve_scoped_commands(
    db: &Database,
    worktree_path: &Path,
    target_branch: &str,
    role_verification_override: Option<&str>,
) -> Vec<String> {
    // Role/specialist override takes absolute priority.
    if let Some(cmd) = role_verification_override {
        let trimmed = cmd.trim();
        if !trimmed.is_empty() {
            tracing::debug!(
                command = %trimmed,
                "resolve_scoped_commands: using role-level verification_command override"
            );
            return vec![trimmed.to_string()];
        }
    }

    let verification = verification_for_path(db, worktree_path).await;
    resolve_scoped_commands_from_config(&verification, worktree_path, target_branch)
}

/// Pure-function variant used by [`resolve_scoped_commands`] and by unit
/// tests. Accepts an already-fetched [`Verification`] so the tests don't need
/// a live Dolt instance.
fn resolve_scoped_commands_from_config(
    verification: &Verification,
    worktree_path: &Path,
    target_branch: &str,
) -> Vec<String> {
    let rules = &verification.rules;

    if rules.is_empty() {
        tracing::debug!(
            "resolve_scoped_commands: no verification.rules configured; skipping verification"
        );
        return Vec::new();
    }

    let changed_files = git_diff_changed_files(worktree_path, target_branch);
    tracing::debug!(
        target_branch = %target_branch,
        changed_file_count = changed_files.len(),
        "resolve_scoped_commands: changed files"
    );

    if changed_files.is_empty() {
        tracing::debug!(
            "resolve_scoped_commands: no changed files detected; skipping verification"
        );
        return Vec::new();
    }

    let matched = collect_commands_for_changed_files(rules, &changed_files);

    if matched.is_empty() {
        tracing::debug!(
            "resolve_scoped_commands: no rules matched changed files; skipping verification"
        );
    } else {
        tracing::debug!(
            command_count = matched.len(),
            "resolve_scoped_commands: using scoped commands from matching rules"
        );
    }
    matched
}

/// Run `git diff --name-only <target_branch>..HEAD` in `worktree_path` and
/// return the list of changed file paths.  Returns an empty `Vec` on any error
/// (e.g. the target branch doesn't exist yet).
fn git_diff_changed_files(worktree_path: &Path, target_branch: &str) -> Vec<String> {
    let range = format!("{}..HEAD", target_branch);
    let output = match std::process::Command::new("git")
        .args(["diff", "--name-only", &range])
        .current_dir(worktree_path)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(
                error = %e,
                target_branch = %target_branch,
                "resolve_scoped_commands: git diff failed"
            );
            return Vec::new();
        }
    };

    if !output.status.success() {
        tracing::warn!(
            target_branch = %target_branch,
            stderr = %String::from_utf8_lossy(&output.stderr),
            "resolve_scoped_commands: git diff returned non-zero exit code"
        );
        return Vec::new();
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect()
}

/// For each rule (in config order), check whether any changed file matches
/// the rule's glob pattern.  Collect all matching commands and deduplicate
/// them, preserving first-seen order.
fn collect_commands_for_changed_files(
    rules: &[VerificationRule],
    changed_files: &[String],
) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();

    for rule in rules {
        let matcher = match globset::GlobBuilder::new(&rule.match_pattern)
            .case_insensitive(false)
            .build()
            .and_then(|g| globset::GlobSet::builder().add(g).build())
        {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    pattern = %rule.match_pattern,
                    error = %e,
                    "resolve_scoped_commands: invalid glob pattern in rule; skipping"
                );
                continue;
            }
        };

        let rule_matches = changed_files.iter().any(|f| matcher.is_match(f));
        if rule_matches {
            for cmd in &rule.commands {
                if seen.insert(cmd.clone()) {
                    result.push(cmd.clone());
                }
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::events::EventBus;
    use djinn_db::ProjectRepository;
    use djinn_stack::environment::{EnvironmentConfig, VerificationRule};
    use std::fs;

    fn tempdir_in_tmp() -> tempfile::TempDir {
        crate::test_helpers::test_tempdir("djinn-scoped-")
    }

    /// Initialise a git repo in `dir` with one commit on `base_branch`, then
    /// check out a new `task_branch` so that subsequent commits appear in
    /// `git diff --name-only <base_branch>..HEAD`.
    ///
    /// Returns the base branch name to use as `target_branch` in assertions.
    fn init_git_repo_with_task_branch(dir: &Path, base_branch: &str, task_branch: &str) -> String {
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .expect("git command");
        };
        run(&["init", "-b", base_branch]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        run(&["commit", "--allow-empty", "-m", "init"]);
        run(&["checkout", "-b", task_branch]);
        base_branch.to_string()
    }

    /// Create a file in `dir`, stage it, and commit it on the current branch.
    fn git_commit_file(dir: &Path, filename: &str, content: &str) {
        let path = dir.join(filename);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
        std::process::Command::new("git")
            .args(["add", filename])
            .current_dir(dir)
            .output()
            .expect("git add");
        std::process::Command::new("git")
            .args(["commit", "-m", &format!("add {filename}")])
            .current_dir(dir)
            .output()
            .expect("git commit");
    }

    fn make_verification(rules: Vec<VerificationRule>) -> Verification {
        Verification { rules }
    }

    async fn seed_project_with_verification(
        db: &Database,
        id: &str,
        path: &Path,
        verification: Verification,
    ) {
        db.ensure_initialized().await.unwrap();
        let _ = path; // path is derived at runtime; retained for fixture compat
        let repo = ProjectRepository::new(db.clone(), EventBus::noop());
        repo.create_with_id(id, &format!("p-{id}"), "test", id)
            .await
            .unwrap();
        let mut cfg = EnvironmentConfig::empty();
        cfg.verification = verification;
        let raw = serde_json::to_string(&cfg).unwrap();
        repo.set_environment_config(id, &raw).await.unwrap();
    }

    // ── role override ───────────────────────────────────────────────────

    #[tokio::test]
    async fn role_override_returns_single_command_immediately() {
        let db = Database::open_in_memory().unwrap();
        let dir = tempdir_in_tmp();
        let result = resolve_scoped_commands(
            &db,
            dir.path(),
            "main",
            Some("cargo test --workspace"),
        )
        .await;
        assert_eq!(result, vec!["cargo test --workspace"]);
    }

    #[tokio::test]
    async fn role_override_whitespace_only_falls_through_to_rules() {
        let db = Database::open_in_memory().unwrap();
        let dir = tempdir_in_tmp();
        // No env config → no rules → empty.
        let result = resolve_scoped_commands(&db, dir.path(), "main", Some("   ")).await;
        assert!(result.is_empty());
    }

    // ── No rules configured → empty ──────────────────────────────────

    #[tokio::test]
    async fn no_rules_configured_returns_empty() {
        let db = Database::open_in_memory().unwrap();
        let dir = tempdir_in_tmp();
        seed_project_with_verification(&db, "p1", dir.path(), make_verification(vec![])).await;
        let result = resolve_scoped_commands(&db, dir.path(), "main", None).await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn no_project_row_returns_empty() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let dir = tempdir_in_tmp();
        let result = resolve_scoped_commands(&db, dir.path(), "main", None).await;
        assert!(result.is_empty());
    }

    // ── rule matching (pure-function form via from_config) ─────────────

    #[test]
    fn single_crate_change_matches_crate_specific_rule() {
        let dir = tempdir_in_tmp();
        let base = init_git_repo_with_task_branch(dir.path(), "main", "task/test");
        let verification = make_verification(vec![
            VerificationRule {
                match_pattern: "crates/djinn-control-plane/**".into(),
                commands: vec!["cargo test -p djinn-control-plane".into()],
            },
            VerificationRule {
                match_pattern: "crates/djinn-core/**".into(),
                commands: vec!["cargo test -p djinn-core".into()],
            },
        ]);
        git_commit_file(dir.path(), "crates/djinn-control-plane/src/lib.rs", "// mcp change");

        let result = resolve_scoped_commands_from_config(&verification, dir.path(), &base);
        assert_eq!(result, vec!["cargo test -p djinn-control-plane"]);
    }

    #[test]
    fn multi_crate_change_collects_multiple_rules() {
        let dir = tempdir_in_tmp();
        let base = init_git_repo_with_task_branch(dir.path(), "main", "task/test");
        let verification = make_verification(vec![
            VerificationRule {
                match_pattern: "crates/djinn-control-plane/**".into(),
                commands: vec!["cargo test -p djinn-control-plane".into()],
            },
            VerificationRule {
                match_pattern: "crates/djinn-core/**".into(),
                commands: vec!["cargo test -p djinn-core".into()],
            },
        ]);
        git_commit_file(dir.path(), "crates/djinn-control-plane/src/lib.rs", "// mcp");
        git_commit_file(dir.path(), "crates/djinn-core/src/lib.rs", "// core");

        let result = resolve_scoped_commands_from_config(&verification, dir.path(), &base);
        assert_eq!(
            result,
            vec!["cargo test -p djinn-control-plane", "cargo test -p djinn-core"]
        );
    }

    #[test]
    fn commands_deduplicated_across_matching_rules() {
        let dir = tempdir_in_tmp();
        let base = init_git_repo_with_task_branch(dir.path(), "main", "task/test");
        let verification = make_verification(vec![
            VerificationRule {
                match_pattern: "crates/djinn-control-plane/**".into(),
                commands: vec!["cargo test --workspace".into()],
            },
            VerificationRule {
                match_pattern: "crates/djinn-core/**".into(),
                commands: vec!["cargo test --workspace".into()],
            },
        ]);
        git_commit_file(dir.path(), "crates/djinn-control-plane/src/lib.rs", "// mcp");
        git_commit_file(dir.path(), "crates/djinn-core/src/lib.rs", "// core");

        let result = resolve_scoped_commands_from_config(&verification, dir.path(), &base);
        // Same command from two matching rules should appear only once.
        assert_eq!(result, vec!["cargo test --workspace"]);
    }

    #[test]
    fn no_matching_rules_returns_empty() {
        let dir = tempdir_in_tmp();
        let base = init_git_repo_with_task_branch(dir.path(), "main", "task/test");
        let verification = make_verification(vec![VerificationRule {
            match_pattern: "crates/djinn-control-plane/**".into(),
            commands: vec!["cargo test -p djinn-control-plane".into()],
        }]);
        git_commit_file(dir.path(), "docs/README.md", "# readme");

        let result = resolve_scoped_commands_from_config(&verification, dir.path(), &base);
        assert!(result.is_empty());
    }

    // ── full DB-backed flow ─────────────────────────────────────────────

    #[tokio::test]
    async fn end_to_end_dolt_backed_resolution_matches_rule() {
        let db = Database::open_in_memory().unwrap();
        let dir = tempdir_in_tmp();
        // Path-based project resolution is now reverse-parse of `{owner}/{repo}`
        // ancestor components (see `resolve_project_id_for_path`). Lay the
        // tempdir out as `.../test/p1/` so the walk finds the seeded project.
        let project_root = dir.keep().join("test").join("p1");
        std::fs::create_dir_all(&project_root).expect("create project root");
        let base = init_git_repo_with_task_branch(&project_root, "main", "task/test");
        let verification = make_verification(vec![VerificationRule {
            match_pattern: "crates/djinn-control-plane/**".into(),
            commands: vec!["cargo test -p djinn-control-plane".into()],
        }]);
        seed_project_with_verification(&db, "p1", &project_root, verification).await;
        git_commit_file(&project_root, "crates/djinn-control-plane/src/lib.rs", "// mcp");

        let result = resolve_scoped_commands(&db, &project_root, &base, None).await;
        assert_eq!(result, vec!["cargo test -p djinn-control-plane"]);
    }
}
