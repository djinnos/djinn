/// Scoped verification command resolution.
///
/// Resolves which verification commands should run for the current state of the
/// branch by diffing against the target branch and matching changed files against
/// the project's `verification_rules` glob patterns.
use std::path::Path;

use super::settings::{VerificationRule, load_settings};

/// Resolve the set of verification commands to run for the current branch.
///
/// Resolution order (highest to lowest priority):
/// 1. If `role_verification_override` is `Some(cmd)` → return `vec![cmd]`.
/// 2. Run `git diff --name-only <target_branch>..HEAD` to get changed files.
/// 3. Match each changed file against `verification_rules` glob patterns (in
///    config order).  Collect + deduplicate commands from all matching rules.
/// 4. If no rules are configured, or no changed file matches any rule, fall
///    back to the full-project verification commands from `.djinn/settings.json`.
///
/// Returns an empty `Vec` when no verification commands are configured at all.
pub fn resolve_scoped_commands(
    worktree_path: &Path,
    target_branch: &str,
    role_verification_override: Option<&str>,
) -> Vec<String> {
    // AC-1: role/specialist override takes absolute priority.
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

    // Load settings (rules + full-project verification commands).
    let settings = load_settings(worktree_path).unwrap_or_else(|e| {
        tracing::warn!(
            error = %e,
            "resolve_scoped_commands: failed to load .djinn/settings.json; using defaults"
        );
        Default::default()
    });

    let rules = &settings.verification_rules;

    // No rules configured → nothing to verify.
    if rules.is_empty() {
        tracing::debug!(
            "resolve_scoped_commands: no verification_rules configured; skipping verification"
        );
        return Vec::new();
    }

    // Get changed files via git diff.
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

    // Match changed files against rules (in config order), collect commands
    // from each matching rule, deduplicate while preserving first-seen order.
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
        let matcher = match globset::GlobBuilder::new(&rule.pattern)
            .case_insensitive(false)
            .build()
            .and_then(|g| globset::GlobSet::builder().add(g).build())
        {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    pattern = %rule.pattern,
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
    use std::fs;

    fn tempdir_in_tmp() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("djinn-scoped-")
            .tempdir_in("/tmp")
            .expect("tempdir")
    }

    fn write_settings(dir: &tempfile::TempDir, content: &str) {
        let djinn_dir = dir.path().join(".djinn");
        fs::create_dir_all(&djinn_dir).unwrap();
        fs::write(djinn_dir.join("settings.json"), content).unwrap();
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
        // Empty initial commit so the base branch reference exists.
        run(&["commit", "--allow-empty", "-m", "init"]);
        // Switch to the task branch so further commits diverge from base.
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

    // ── AC-1: role override ───────────────────────────────────────────────────

    #[test]
    fn role_override_returns_single_command_immediately() {
        let dir = tempdir_in_tmp();
        let result = resolve_scoped_commands(dir.path(), "main", Some("cargo test --workspace"));
        assert_eq!(result, vec!["cargo test --workspace"]);
    }

    #[test]
    fn role_override_whitespace_only_falls_through_to_rules() {
        let dir = tempdir_in_tmp();
        // No settings file → no rules → no full-project commands → empty.
        let result = resolve_scoped_commands(dir.path(), "main", Some("   "));
        assert!(result.is_empty());
    }

    // ── No rules configured → empty ────────────────────────────────────────────

    #[test]
    fn no_rules_configured_returns_empty() {
        let dir = tempdir_in_tmp();
        write_settings(
            &dir,
            r#"{
                "setup": [{"name": "build", "command": "cargo build", "timeout_secs": 300}]
            }"#,
        );
        let result = resolve_scoped_commands(dir.path(), "main", None);
        assert!(result.is_empty());
    }

    #[test]
    fn no_settings_file_returns_empty() {
        let dir = tempdir_in_tmp();
        let result = resolve_scoped_commands(dir.path(), "main", None);
        assert!(result.is_empty());
    }

    // ── AC-2/3: rule matching ─────────────────────────────────────────────────

    #[test]
    fn single_crate_change_matches_crate_specific_rule() {
        let dir = tempdir_in_tmp();
        let base = init_git_repo_with_task_branch(dir.path(), "main", "task/test");
        write_settings(
            &dir,
            r#"{
                "verification_rules": [
                    {"match": "crates/djinn-mcp/**", "commands": ["cargo test -p djinn-mcp"]},
                    {"match": "crates/djinn-core/**", "commands": ["cargo test -p djinn-core"]}
                ]
            }"#,
        );
        git_commit_file(dir.path(), "crates/djinn-mcp/src/lib.rs", "// mcp change");

        let result = resolve_scoped_commands(dir.path(), &base, None);
        assert_eq!(result, vec!["cargo test -p djinn-mcp"]);
    }

    #[test]
    fn multi_crate_change_collects_multiple_rules() {
        let dir = tempdir_in_tmp();
        let base = init_git_repo_with_task_branch(dir.path(), "main", "task/test");
        write_settings(
            &dir,
            r#"{
                "verification_rules": [
                    {"match": "crates/djinn-mcp/**", "commands": ["cargo test -p djinn-mcp"]},
                    {"match": "crates/djinn-core/**", "commands": ["cargo test -p djinn-core"]}
                ]
            }"#,
        );
        git_commit_file(dir.path(), "crates/djinn-mcp/src/lib.rs", "// mcp");
        git_commit_file(dir.path(), "crates/djinn-core/src/lib.rs", "// core");

        let result = resolve_scoped_commands(dir.path(), &base, None);
        assert_eq!(
            result,
            vec!["cargo test -p djinn-mcp", "cargo test -p djinn-core"]
        );
    }

    #[test]
    fn commands_deduplicated_across_matching_rules() {
        let dir = tempdir_in_tmp();
        let base = init_git_repo_with_task_branch(dir.path(), "main", "task/test");
        write_settings(
            &dir,
            r#"{
                "verification_rules": [
                    {"match": "crates/djinn-mcp/**", "commands": ["cargo test --workspace"]},
                    {"match": "crates/djinn-core/**", "commands": ["cargo test --workspace"]}
                ]
            }"#,
        );
        git_commit_file(dir.path(), "crates/djinn-mcp/src/lib.rs", "// mcp");
        git_commit_file(dir.path(), "crates/djinn-core/src/lib.rs", "// core");

        let result = resolve_scoped_commands(dir.path(), &base, None);
        // Same command from two matching rules should appear only once.
        assert_eq!(result, vec!["cargo test --workspace"]);
    }

    // ── No rules match → empty ─────────────────────────────────────────────────

    #[test]
    fn no_matching_rules_returns_empty() {
        let dir = tempdir_in_tmp();
        let base = init_git_repo_with_task_branch(dir.path(), "main", "task/test");
        write_settings(
            &dir,
            r#"{
                "verification_rules": [
                    {"match": "crates/djinn-mcp/**", "commands": ["cargo test -p djinn-mcp"]}
                ]
            }"#,
        );
        // Change a file that doesn't match the rule.
        git_commit_file(dir.path(), "docs/README.md", "# readme");

        let result = resolve_scoped_commands(dir.path(), &base, None);
        assert!(result.is_empty());
    }
}
