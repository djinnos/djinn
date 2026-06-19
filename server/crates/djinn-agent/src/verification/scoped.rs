//! Scoped verification command resolution.
//!
//! Resolves which verification commands should run for the current state of
//! the branch by diffing against the target branch and matching changed files
//! against the project's `verification.rules` glob patterns from
//! `projects.environment_config` in Dolt.

use std::path::Path;

use djinn_db::Database;
use djinn_stack::environment::{Verification, VerificationRule};

use super::environment::{verification_for_path, verification_for_project_id};

/// Resolve the set of verification commands to run for the current branch.
///
/// Resolution order (highest to lowest priority):
/// 1. If `role_verification_override` is `Some(cmd)` → return `vec![cmd]`.
/// 2. Fetch `environment_config.verification` from Dolt for the project that
///    owns `worktree_path` (fuzzy prefix match).
/// 3. Run `git diff --name-only <merge_base>..HEAD` (where `<merge_base>` is
///    `git merge-base <target_branch> HEAD`) to get the files THIS branch
///    changed. Using the merge-base (three-dot semantics) rather than two-dot
///    `<target_branch>..HEAD` avoids over-firing rules on files the target
///    branch gained *after* this branch split off.
/// 4. Match each changed file against `verification.rules` glob patterns
///    (in config order). Collect + deduplicate commands from all matching
///    rules.
///
/// Returns an empty `Vec` when no verification commands are configured at
/// all, no rules match, or the project row / environment_config can't be
/// found (see [`crate::verification::environment`] for soft-failure rules).
pub async fn resolve_scoped_commands(
    db: &Database,
    project_id: Option<&str>,
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

    // Prefer resolving rules by the known project id. The verification pipeline
    // runs against an ephemeral clone at `/tmp/.tmp<random>`, whose path does
    // NOT carry the `{owner}/{repo}` shape `verification_for_path` reverse-parses
    // — so the path form silently resolves to empty config and skips ALL
    // verification (no quality gate, no cache warm). Callers that know the
    // project id (the slot-free pipeline) pass it; only path-only callers (and
    // tests) fall back to the path resolver.
    let verification = match project_id {
        Some(pid) => verification_for_project_id(db, pid).await,
        None => verification_for_path(db, worktree_path).await,
    };
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

    let changed_files = match git_diff_changed_files(worktree_path, target_branch) {
        Some(files) => files,
        None => {
            // The changed-file set could not be determined (target ref missing,
            // diff errored). NEVER skip — that's a false pass on the gate. Run
            // every configured command conservatively.
            tracing::warn!(
                target_branch = %target_branch,
                "resolve_scoped_commands: changed-file set undetermined; running ALL configured commands (conservative — never skip the gate)"
            );
            return all_commands_from_rules(rules);
        }
    };
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

/// Return the list of files THIS branch changed relative to `target_branch`,
/// by diffing `<merge_base>..HEAD` where `<merge_base>` is
/// `git merge-base <target_branch> HEAD`. This is equivalent to three-dot
/// `git diff <target_branch>...HEAD` and avoids the two-dot pitfall where
/// commits the target branch gained *after* this branch split off would show
/// up as branch deletions/changes.
///
/// Best-effort: if `git merge-base` fails (e.g. unrelated histories), falls
/// back to the two-dot `<target_branch>..HEAD` range. Returns an empty `Vec`
/// on any diff error (e.g. the target branch doesn't exist yet).
/// Returns `Some(changed_files)` when the diff base was resolved (the list may
/// be empty = genuinely no changes), or `None` when the changed-file set could
/// NOT be determined (target ref missing, diff errored). `None` is critical:
/// the caller must treat it as "run ALL configured commands" rather than "no
/// changes" — otherwise verification is silently skipped (a false pass on the
/// pre-PR gate).
fn git_diff_changed_files(worktree_path: &Path, target_branch: &str) -> Option<Vec<String>> {
    // Resolve the target to a ref that actually EXISTS in this worktree. The
    // in-pod verification path reuses the worker's ephemeral clone, which has
    // `origin/<target>` but NO local `<target>` branch — diffing against the
    // bare `<target>` name fails ("Not a valid object name"), which previously
    // yielded an empty changed-file set and SILENTLY SKIPPED all scoped
    // verification (a false pass). Try `<target>`, then `origin/<target>`.
    let resolved_target = [target_branch.to_string(), format!("origin/{target_branch}")]
        .into_iter()
        .find(|r| {
            std::process::Command::new("git")
                .args([
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{r}^{{commit}}"),
                ])
                .current_dir(worktree_path)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        });
    let Some(resolved_target) = resolved_target else {
        tracing::warn!(
            target_branch = %target_branch,
            "resolve_scoped_commands: target ref not found (tried <target> and origin/<target>); cannot scope — caller runs all configured commands"
        );
        return None;
    };

    // Resolve the merge-base so the diff is scoped to this branch's own changes
    // (three-dot semantics). Fall back to the resolved target ref itself.
    let base_ref = match std::process::Command::new("git")
        .args(["merge-base", &resolved_target, "HEAD"])
        .current_dir(worktree_path)
        .output()
    {
        Ok(o) if o.status.success() => {
            let sha = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if sha.is_empty() {
                resolved_target.clone()
            } else {
                sha
            }
        }
        _ => resolved_target.clone(),
    };

    let range = format!("{base_ref}..HEAD");
    let output = match std::process::Command::new("git")
        .args(["diff", "--name-only", &range])
        .current_dir(worktree_path)
        .output()
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            tracing::warn!(
                target_branch = %target_branch,
                stderr = %String::from_utf8_lossy(&o.stderr),
                "resolve_scoped_commands: git diff non-zero; cannot scope — caller runs all configured commands"
            );
            return None;
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                target_branch = %target_branch,
                "resolve_scoped_commands: git diff failed; cannot scope — caller runs all configured commands"
            );
            return None;
        }
    };

    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect(),
    )
}

/// Every command across all rules, order-preserving + deduplicated. Used as the
/// CONSERVATIVE fallback when the changed-file set can't be determined: better
/// to over-run verification than to skip it (a false pass).
fn all_commands_from_rules(rules: &[djinn_stack::environment::VerificationRule]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for rule in rules {
        for cmd in &rule.commands {
            let trimmed = cmd.trim();
            if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
                out.push(trimmed.to_string());
            }
        }
    }
    out
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
    use djinn_stack::environment::VerificationRule;
    use std::fs;

    fn tempdir_in_tmp() -> tempfile::TempDir {
        crate::test_helpers::test_tempdir("djinn-scoped-")
    }

    /// Initialise a git repo in `dir` with one commit on `base_branch`, then
    /// check out a new `task_branch` so that subsequent commits appear in
    /// `git diff --name-only <merge_base(base_branch, HEAD)>..HEAD`.
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
        let _ = verification;
    }

    // ── role override ───────────────────────────────────────────────────

    #[tokio::test]
    async fn role_override_returns_single_command_immediately() {
        let db = Database::open_in_memory().unwrap();
        let dir = tempdir_in_tmp();
        let result = resolve_scoped_commands(
            &db,
            None,
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
        let result = resolve_scoped_commands(&db, None, dir.path(), "main", Some("   ")).await;
        assert!(result.is_empty());
    }

    // ── No rules configured → empty ──────────────────────────────────

    #[tokio::test]
    async fn no_rules_configured_returns_empty() {
        let db = Database::open_in_memory().unwrap();
        let dir = tempdir_in_tmp();
        seed_project_with_verification(&db, "p1", dir.path(), make_verification(vec![])).await;
        let result = resolve_scoped_commands(&db, None, dir.path(), "main", None).await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn no_project_row_returns_empty() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let dir = tempdir_in_tmp();
        let result = resolve_scoped_commands(&db, None, dir.path(), "main", None).await;
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
        git_commit_file(
            dir.path(),
            "crates/djinn-control-plane/src/lib.rs",
            "// mcp change",
        );

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
        git_commit_file(
            dir.path(),
            "crates/djinn-control-plane/src/lib.rs",
            "// mcp",
        );
        git_commit_file(dir.path(), "crates/djinn-core/src/lib.rs", "// core");

        let result = resolve_scoped_commands_from_config(&verification, dir.path(), &base);
        assert_eq!(
            result,
            vec![
                "cargo test -p djinn-control-plane",
                "cargo test -p djinn-core"
            ]
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
        git_commit_file(
            dir.path(),
            "crates/djinn-control-plane/src/lib.rs",
            "// mcp",
        );
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

    /// Regression: a file added on the *base* branch AFTER the task branch
    /// split off must NOT be reported as a change of the task branch.
    ///
    /// Two-dot `git diff <base>..HEAD` would list the base-only file (because
    /// it's present on `base` but not on `HEAD`'s merge-base view), falsely
    /// firing its verification rule. The merge-base range
    /// (`git diff <merge-base>..HEAD`) only includes what the task branch
    /// itself changed, so the base-only file is excluded. This test passes
    /// only with the merge-base implementation and fails on the old two-dot
    /// code.
    #[test]
    fn base_branch_post_split_file_not_reported_as_changed() {
        let dir = tempdir_in_tmp();
        let base = init_git_repo_with_task_branch(dir.path(), "main", "task/test");

        // The task branch makes its own change.
        git_commit_file(dir.path(), "crates/djinn-core/src/lib.rs", "// task change");

        // Meanwhile, the base branch gains a NEW file after the split.
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git command");
        };
        run(&["checkout", "main"]);
        git_commit_file(
            dir.path(),
            "crates/djinn-control-plane/src/lib.rs",
            "// added on main after branch point",
        );
        run(&["checkout", "task/test"]);

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

        let result = resolve_scoped_commands_from_config(&verification, dir.path(), &base);

        // Only the task branch's own change (djinn-core) should fire its rule.
        // The base-only djinn-control-plane file must NOT appear, so its
        // command must NOT be present.
        assert_eq!(result, vec!["cargo test -p djinn-core"]);
        assert!(
            !result.contains(&"cargo test -p djinn-control-plane".to_string()),
            "base-only file added after branch point must not fire its rule (two-dot bug)"
        );
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
        git_commit_file(
            &project_root,
            "crates/djinn-control-plane/src/lib.rs",
            "// mcp",
        );

        let result = resolve_scoped_commands(&db, None, &project_root, &base, None).await;

        assert_eq!(result, vec!["cargo test -p djinn-control-plane"]);
    }

    /// Regression: the slot-free verification pipeline runs against an ephemeral
    /// clone at `/tmp/.tmp<random>`, whose path can't reverse-parse to
    /// `{owner}/{repo}`. Passing the known `project_id` must resolve the rules
    /// regardless of the worktree path (previously this silently skipped ALL
    /// verification — no gate, no cache warm).
    #[tokio::test]
    async fn project_id_resolves_rules_for_non_owner_repo_path() {
        let db = Database::open_in_memory().unwrap();
        let dir = tempdir_in_tmp();
        // A path that does NOT carry an `{owner}/{repo}` shape, like the
        // ephemeral verification clone.
        let project_root = dir.keep().join(".tmpEPHEMERAL");
        std::fs::create_dir_all(&project_root).expect("create project root");
        let base = init_git_repo_with_task_branch(&project_root, "main", "task/test");
        let verification = make_verification(vec![VerificationRule {
            match_pattern: "crates/djinn-control-plane/**".into(),
            commands: vec!["cargo test -p djinn-control-plane".into()],
        }]);
        seed_project_with_verification(&db, "p1", &project_root, verification).await;
        git_commit_file(
            &project_root,
            "crates/djinn-control-plane/src/lib.rs",
            "// mcp",
        );

        // Path form would skip (no owner/repo); id form must resolve.
        let by_path = resolve_scoped_commands(&db, None, &project_root, &base, None).await;
        assert!(
            by_path.is_empty(),
            "non-owner/repo path should not resolve via path form"
        );
        let by_id = resolve_scoped_commands(&db, Some("p1"), &project_root, &base, None).await;
        assert_eq!(by_id, vec!["cargo test -p djinn-control-plane"]);
    }
}
