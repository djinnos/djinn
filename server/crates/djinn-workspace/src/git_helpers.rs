use std::path::Path;

use tokio::time::{Duration, timeout};

const GIT_CMD_TIMEOUT: Duration = Duration::from_secs(30);

/// Wrapper result type used by the workspace helpers. The command is executed in
/// `cwd`, returns the trimmed stdout on success, or an error string on failure.
async fn run_git_simple(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let owned_args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let output = timeout(
        GIT_CMD_TIMEOUT,
        djinn_git::run_git_command_in(cwd, owned_args),
    )
    .await
    .map_err(|_| "git command timed out".to_string())?
    .map_err(|e| format!("git command failed: {e}"))?;
    if output.code != 0 {
        return Err(format!("git {}: {}", args.join(" "), output.stderr.trim()));
    }
    Ok(output.stdout)
}

/// `git merge-base --is-ancestor A B` returning `Ok(true)` when A is an ancestor
/// of B, `Ok(false)` when it is not, and `Err` for any other failure.
/// Equivalent to the original workspace logic but routed through djinn-git.
pub async fn is_ancestor(cwd: &Path, ancestor: &str, descendant: &str) -> Result<bool, String> {
    let output = timeout(
        GIT_CMD_TIMEOUT,
        djinn_git::run_git_command_in(
            cwd,
            vec![
                "merge-base".to_string(),
                "--is-ancestor".to_string(),
                ancestor.to_string(),
                descendant.to_string(),
            ],
        ),
    )
    .await;

    let output = match output {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(format!("git merge-base failed: {e}")),
        Err(_) => return Err("git merge-base timed out".to_string()),
    };

    match output.code {
        0 => Ok(true),
        1 => Ok(false),
        _ => Err(format!(
            "git merge-base --is-ancestor {ancestor} {descendant}: {}",
            output.stderr.trim()
        )),
    }
}

/// `git merge --no-commit --no-ff <merge_ref>` with the standard bot author/committer
/// identity injected. Returns `Ok(true)` on a clean merge, `Ok(false)` when the
/// merge stops on conflicts (no error; caller must inspect unmerged files), and
/// `Err` on any other failure.
pub async fn try_merge_no_commit_no_ff(cwd: &Path, merge_ref: &str) -> Result<bool, String> {
    let output = timeout(
        GIT_CMD_TIMEOUT,
        djinn_git::run_git_command_in_with_env(
            cwd,
            vec![
                "merge".to_string(),
                "--no-commit".to_string(),
                "--no-ff".to_string(),
                merge_ref.to_string(),
            ],
            vec![
                ("GIT_AUTHOR_NAME".to_string(), "djinn-bot".to_string()),
                (
                    "GIT_AUTHOR_EMAIL".to_string(),
                    "bot@djinn.local".to_string(),
                ),
                ("GIT_COMMITTER_NAME".to_string(), "djinn-bot".to_string()),
                (
                    "GIT_COMMITTER_EMAIL".to_string(),
                    "bot@djinn.local".to_string(),
                ),
            ],
        ),
    )
    .await
    .map_err(|_| "git merge timed out".to_string())?
    .map_err(|e| format!("git merge failed: {e}"))?;
    if output.is_success() {
        return Ok(true);
    }

    let stderr = output.stderr.trim().to_string();
    if output.exit_code() == Some(1) {
        Ok(false)
    } else {
        Err(format!(
            "git merge --no-commit --no-ff {merge_ref}: {stderr}"
        ))
    }
}

/// `git rev-parse <rev>` returning the trimmed stdout on success, `Err` on
/// failure. Routed through djinn-git for consistent safe.directory handling.
pub async fn resolve_ref(cwd: &Path, rev: &str) -> Result<String, String> {
    run_git_simple(cwd, &["rev-parse", rev]).await
}

/// `git diff --name-only --diff-filter=U` returning the list of unmerged paths.
pub async fn unmerged_files(cwd: &Path) -> Result<Vec<String>, String> {
    let out = run_git_simple(cwd, &["diff", "--name-only", "--diff-filter=U"]).await?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

/// `git fetch origin <branch>` in `cwd`.
pub async fn fetch_origin(cwd: &Path, branch: &str) -> Result<(), String> {
    run_git_simple(cwd, &["fetch", "origin", branch]).await?;
    Ok(())
}

/// `git add -A` in `cwd`.
pub async fn add_all(cwd: &Path) -> Result<(), String> {
    run_git_simple(cwd, &["add", "-A"]).await?;
    Ok(())
}

/// `git write-tree` in `cwd`, returning the tree SHA.
pub async fn write_tree(cwd: &Path) -> Result<String, String> {
    run_git_simple(cwd, &["write-tree"]).await
}

/// `git commit-tree <tree> -p HEAD -p <merge_target_sha>` with the standard bot
/// author/committer identity. Returns the new commit SHA on success.
pub async fn commit_tree_merge(
    cwd: &Path,
    tree_sha: &str,
    merge_target_sha: &str,
) -> Result<String, String> {
    let output = timeout(
        GIT_CMD_TIMEOUT,
        djinn_git::run_git_command_in_with_env(
            cwd,
            vec![
                "commit-tree".to_string(),
                tree_sha.to_string(),
                "-p".to_string(),
                "HEAD".to_string(),
                "-p".to_string(),
                merge_target_sha.to_string(),
                "-m".to_string(),
                "Merge resolution (enforced by supervisor)".to_string(),
            ],
            vec![
                ("GIT_AUTHOR_NAME".to_string(), "djinn-bot".to_string()),
                (
                    "GIT_AUTHOR_EMAIL".to_string(),
                    "bot@djinn.local".to_string(),
                ),
                ("GIT_COMMITTER_NAME".to_string(), "djinn-bot".to_string()),
                (
                    "GIT_COMMITTER_EMAIL".to_string(),
                    "bot@djinn.local".to_string(),
                ),
            ],
        ),
    )
    .await
    .map_err(|_| "git commit-tree timed out".to_string())?
    .map_err(|e| format!("git commit-tree failed: {e}"))?;
    if !output.is_success() {
        let stderr = output.stderr.trim().to_string();
        return Err(format!(
            "git commit-tree {tree_sha} -p HEAD -p {merge_target_sha}: {stderr}"
        ));
    }
    Ok(output.stdout.trim().to_string())
}

/// `git reset --hard <commit>` in `cwd`. Used to move the current branch to the
/// enforced merge commit after `commit_tree_merge`.
pub async fn reset_hard(cwd: &Path, commit_sha: &str) -> Result<(), String> {
    run_git_simple(cwd, &["reset", "--hard", commit_sha]).await?;
    Ok(())
}

/// `git rev-parse HEAD` in `cwd`.
pub async fn head_sha(cwd: &Path) -> Result<String, String> {
    resolve_ref(cwd, "HEAD").await
}
