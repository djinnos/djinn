use std::path::{Path, PathBuf};

use crate::context::AgentContext;
use djinn_db::ProjectRepository;
use djinn_git::GitError;

use super::*;

// ─── Git / worktree helpers ───────────────────────────────────────────────────

/// Ensure the target branch exists and has at least one commit.
///
/// Handles two cases:
/// 1. Repo has commits but target branch doesn't exist → create it from HEAD.
/// 2. Repo has no commits at all → stage `.djinn/.gitignore`, create initial
///    commit on the target branch.
///
/// This is a safety net for `prepare_worktree`; the primary bootstrap happens
/// in `project_add` via `ensure_git_repo_ready`.
async fn ensure_target_branch_ready(
    git: &djinn_git::GitActorHandle,
    target_branch: &str,
) -> anyhow::Result<()> {
    // Fast path: target branch already exists (git2 — no process spawn).
    match git.branch_exists(target_branch).await {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(
                target_branch,
                error = %e,
                "Lifecycle: git2 branch_exists check failed; falling through to bootstrap"
            );
        }
    }

    // Check if the repo has *any* commits (git2 — no process spawn).
    let head_exists = git.has_commits().await.unwrap_or(false);

    if head_exists {
        // Repo has commits, but the target branch doesn't exist.
        // Create it from HEAD (user may have switched target_branch in settings).
        tracing::info!(
            target_branch,
            "Lifecycle: creating target branch '{target_branch}' from HEAD"
        );
        git.run_command(vec![
            "branch".into(),
            target_branch.to_string(),
            "HEAD".into(),
        ])
        .await
        .map_err(|e| {
            anyhow::anyhow!("failed to create target branch '{target_branch}' from HEAD: {e}")
        })?;
        return Ok(());
    }

    // No commits at all — bootstrap the repo.
    let current = git
        .run_command(vec!["symbolic-ref".into(), "--short".into(), "HEAD".into()])
        .await
        .map(|o| o.stdout.trim().to_string())
        .unwrap_or_default();

    if current != target_branch {
        let _ = git
            .run_command(vec![
                "checkout".into(),
                "-B".into(),
                target_branch.to_string(),
            ])
            .await;
    }

    tracing::info!(
        target_branch,
        "Lifecycle: bootstrapping repo with initial commit on '{target_branch}'"
    );

    // Stage .djinn/.gitignore and create initial commit.
    //
    // Multiple slots may race here for the same repo.  If the git add or
    // commit fails (e.g. EBADF from concurrent git operations), re-check
    // whether another slot already bootstrapped the branch.  If so, we're
    // done — no need to fail.
    let stage_result = git
        .run_command(vec!["add".into(), ".djinn/.gitignore".into()])
        .await;

    if let Err(e) = &stage_result {
        // Another slot may have completed the bootstrap while we raced.
        if git
            .run_command(vec![
                "rev-parse".into(),
                "--verify".into(),
                "--quiet".into(),
                format!("refs/heads/{target_branch}"),
            ])
            .await
            .is_ok()
        {
            tracing::info!(
                target_branch,
                "Lifecycle: bootstrap race resolved — branch now exists"
            );
            return Ok(());
        }
        return Err(anyhow::anyhow!(
            "failed to stage .djinn/.gitignore for initial commit: {e}"
        ));
    }

    let commit_result = git
        .run_command(vec![
            "commit".into(),
            "--no-verify".into(),
            "-m".into(),
            format!("chore: initialize {target_branch} branch"),
        ])
        .await;

    if let Err(e) = &commit_result {
        // Same race check — another slot may have finished first.
        if git
            .run_command(vec![
                "rev-parse".into(),
                "--verify".into(),
                "--quiet".into(),
                format!("refs/heads/{target_branch}"),
            ])
            .await
            .is_ok()
        {
            tracing::info!(
                target_branch,
                "Lifecycle: bootstrap race resolved — branch now exists"
            );
            return Ok(());
        }
        return Err(anyhow::anyhow!(
            "failed to bootstrap repo with initial commit: {e}"
        ));
    }

    Ok(())
}

/// Returns `(worktree_path, conflicting_files)`.
/// `conflicting_files` is `Some` when a resumed worktree sync hit merge conflicts.
pub(crate) async fn prepare_worktree(
    project_dir: &Path,
    task: &djinn_core::models::Task,
    app_state: &AgentContext,
) -> anyhow::Result<(PathBuf, Option<Vec<String>>)> {
    let branch = format!("task/{}", task.short_id);
    let target_branch = default_target_branch(&task.project_id, app_state).await;
    let git = app_state
        .git_actor(project_dir)
        .await
        .map_err(|e| anyhow::anyhow!("git actor: {e}"))?;

    let stale_worktree_path = project_dir
        .join(".djinn")
        .join("worktrees")
        .join(&task.short_id);
    let resumed_worktree_exists =
        stale_worktree_path.exists() && stale_worktree_path.join(".git").exists();

    // Reuse existing worktree if it's still valid.  This preserves the
    // branch's own target/ build cache across worker → verify → review
    // cycles, avoiding full rebuilds on every session.  The worktree is
    // only nuked on task close/merge or when the PM calls
    // `task_delete_branch` (which invokes `teardown_worktree`).
    if resumed_worktree_exists {
        let conflict_files = try_rebase_existing_task_branch(
            project_dir,
            &branch,
            &target_branch,
            Some(&stale_worktree_path),
            app_state,
        )
        .await;
        tracing::info!(
            task_id = %task.short_id,
            worktree = %stale_worktree_path.display(),
            has_conflicts = conflict_files.is_some(),
            "Lifecycle: reusing existing worktree"
        );
        return Ok((stale_worktree_path, conflict_files));
    }

    // Clean up broken worktree remnants (dir exists but no .git file).
    // This can happen when a session is killed mid-cleanup (e.g. provider
    // outage, system crash).  Without this guard, `git worktree add` fails
    // with exit 128 and the task enters a dispatch → fail → stuck loop.
    if stale_worktree_path.exists() && !stale_worktree_path.join(".git").exists() {
        tracing::warn!(
            task_id = %task.short_id,
            worktree = %stale_worktree_path.display(),
            "Lifecycle: removing broken worktree remnant (no .git)"
        );
        let _ = git.remove_worktree(&stale_worktree_path).await;
        if stale_worktree_path.exists() {
            let _ = std::fs::remove_dir_all(&stale_worktree_path);
        }
        let _ = git
            .run_command(vec!["worktree".into(), "prune".into()])
            .await;
    }

    // Ensure the target branch has at least one commit — a bare `git init`
    // creates a branch ref with no commits, which makes `git branch task/x main`
    // fail with "not a valid object name".  Bootstrap with an empty initial
    // commit so the repo is usable.
    ensure_target_branch_ready(&git, &target_branch).await?;

    let branch_exists = match git
        .run_command(vec![
            "show-ref".into(),
            "--verify".into(),
            "--quiet".into(),
            format!("refs/heads/{branch}"),
        ])
        .await
    {
        Ok(_) => true,
        Err(GitError::CommandFailed { code: 1, .. }) => false,
        Err(e) => return Err(anyhow::anyhow!("show-ref failed: {e}")),
    };

    if !branch_exists {
        git.create_branch(&task.short_id, &target_branch)
            .await
            .map_err(|e| anyhow::anyhow!("create branch: {e}"))?;
    } else {
        try_rebase_existing_task_branch(project_dir, &branch, &target_branch, None, app_state)
            .await;
    }

    let path = git
        .create_worktree(&task.short_id, &branch, false)
        .await
        .map_err(|e| anyhow::anyhow!("create worktree: {e}"))?;
    Ok((path, None))
}

/// Sync an existing task branch against the latest target branch.
///
/// When `resumed_worktree_path` is provided (resumed task), the sync is performed
/// directly in that worktree so conflicts are surfaced to the worker. On conflict,
/// the rebase is aborted but conflict markers are left in place for the worker.
///
/// When `resumed_worktree_path` is None (fresh task), a temporary sync worktree
/// is used and discarded after the sync.
///
/// Returns `Some(conflicting_files)` when a resumed worktree sync hits conflicts.
pub(crate) async fn try_rebase_existing_task_branch(
    project_dir: &Path,
    branch: &str,
    target_branch: &str,
    resumed_worktree_path: Option<&Path>,
    app_state: &AgentContext,
) -> Option<Vec<String>> {
    let git = match app_state.git_actor(project_dir).await {
        Ok(git) => git,
        Err(e) => {
            tracing::warn!(branch = %branch, error = %e, "failed to open git actor for branch sync");
            return None;
        }
    };

    let _ = git
        .run_command(vec![
            "fetch".into(),
            "origin".into(),
            target_branch.to_string(),
        ])
        .await;

    let upstream = match git
        .run_command(vec![
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            format!("refs/remotes/origin/{target_branch}"),
        ])
        .await
    {
        Ok(_) => format!("origin/{target_branch}"),
        Err(GitError::CommandFailed { code: 1, .. }) => target_branch.to_string(),
        Err(e) => {
            tracing::warn!(
                branch = %branch,
                target_branch = %target_branch,
                error = %e,
                "failed to resolve upstream for branch sync"
            );
            return None;
        }
    };

    // If we're resuming a task with an existing worktree, sync directly in that
    // worktree so conflicts can be left in place for the worker to resolve.
    if let Some(resumed_path) = resumed_worktree_path {
        return sync_in_resumed_worktree(project_dir, branch, &upstream, resumed_path, app_state)
            .await;
    }

    // Fresh task: use a temporary sync worktree approach.
    let sync_name = format!(".sync-{}", branch.replace('/', "-"));
    let sync_worktree_path = project_dir.join(".djinn").join("worktrees").join(sync_name);
    let _ = git.remove_worktree(&sync_worktree_path).await;
    if sync_worktree_path.exists() {
        let _ = std::fs::remove_dir_all(&sync_worktree_path);
    }

    let sync_path = sync_worktree_path.to_str().unwrap_or_default().to_string();
    if let Err(e) = git
        .run_command(vec![
            "worktree".into(),
            "add".into(),
            "--detach".into(),
            sync_path.clone(),
            branch.to_string(),
        ])
        .await
    {
        tracing::warn!(branch = %branch, error = %e, "failed to create sync worktree for branch rebase");
        return None;
    }

    let sync_git = match app_state.git_actor(&sync_worktree_path).await {
        Ok(git) => git,
        Err(e) => {
            tracing::warn!(branch = %branch, error = %e, "failed to open sync worktree git actor");
            let _ = git.remove_worktree(&sync_worktree_path).await;
            if sync_worktree_path.exists() {
                let _ = std::fs::remove_dir_all(&sync_worktree_path);
            }
            return None;
        }
    };

    match sync_git.rebase_with_retry(&upstream).await {
        Ok(_) => {
            tracing::info!(branch = %branch, upstream = %upstream, "rebased existing task branch before dispatch");
        }
        Err(GitError::CommandFailed { .. }) => {
            tracing::warn!(
                branch = %branch,
                upstream = %upstream,
                "existing task branch could not be rebased cleanly; continuing without rebase"
            );
        }
        Err(e) => {
            tracing::warn!(
                branch = %branch,
                upstream = %upstream,
                error = %e,
                "failed to rebase existing task branch"
            );
        }
    }

    let _ = git.remove_worktree(&sync_worktree_path).await;
    if sync_worktree_path.exists() {
        let _ = std::fs::remove_dir_all(&sync_worktree_path);
    }
    None
}

/// Sync a resumed task's worktree against the latest target branch.
/// Performs the sync directly in the resumed worktree so conflicts are
/// surfaced to the worker. On conflict, aborts the rebase but leaves
/// conflict markers in place.
///
/// Returns `Some(conflicting_files)` when the merge leaves conflict markers,
/// `None` on clean sync.
async fn sync_in_resumed_worktree(
    _project_dir: &Path,
    branch: &str,
    upstream: &str,
    resumed_worktree_path: &Path,
    app_state: &AgentContext,
) -> Option<Vec<String>> {
    // We only need the worktree git actor for resumed worktree sync
    let worktree_git = match app_state.git_actor(resumed_worktree_path).await {
        Ok(git) => git,
        Err(e) => {
            tracing::warn!(
                branch = %branch,
                error = %e,
                "failed to open resumed worktree git actor for sync"
            );
            return None;
        }
    };

    // Ensure the branch is checked out in the resumed worktree
    if let Err(e) = worktree_git
        .run_command(vec!["checkout".into(), branch.to_string()])
        .await
    {
        tracing::warn!(
            branch = %branch,
            error = %e,
            "failed to checkout branch in resumed worktree"
        );
    }

    // Check if there are uncommitted changes before sync
    let has_uncommitted = match worktree_git
        .run_command(vec!["status".into(), "--porcelain".into()])
        .await
    {
        Ok(out) => !out.stdout.trim().is_empty(),
        Err(e) => {
            tracing::warn!(
                branch = %branch,
                error = %e,
                "failed to check worktree status before sync"
            );
            false
        }
    };

    if has_uncommitted {
        // Commit any uncommitted changes as WIP before sync
        let _ = worktree_git
            .run_command(vec!["add".into(), "-A".into()])
            .await;
        let _ = worktree_git
            .run_command(vec![
                "commit".into(),
                "--no-verify".into(),
                "-m".into(),
                "WIP: pre-sync save".into(),
            ])
            .await;
        tracing::info!(
            branch = %branch,
            "staged uncommitted changes before sync"
        );
    }

    // Attempt the rebase in the resumed worktree
    match worktree_git.rebase_with_retry(upstream).await {
        Ok(_) => {
            tracing::info!(
                branch = %branch,
                upstream = %upstream,
                "rebased resumed task branch in worktree"
            );
            None
        }
        Err(GitError::CommandFailed { .. }) => {
            // Rebase failed with conflicts - abort but leave conflict markers
            // This surfaces the conflicts directly in the worker worktree
            tracing::warn!(
                branch = %branch,
                upstream = %upstream,
                worktree = %resumed_worktree_path.display(),
                "sync conflict in resumed task worktree - aborting rebase and leaving conflict markers"
            );

            // Abort the rebase to clean up git state but leave the conflict markers
            // (they're already in the working directory from the failed rebase)
            let _ = worktree_git
                .run_command(vec!["rebase".into(), "--abort".into()])
                .await;

            // After abort, we need to bring the target changes into the worktree
            // with merge markers. Use a merge approach instead.
            if let Err(e) = worktree_git
                .run_command(vec![
                    "merge".into(),
                    "--no-commit".into(),
                    "--no-ff".into(),
                    upstream.to_string(),
                ])
                .await
            {
                tracing::info!(
                    branch = %branch,
                    upstream = %upstream,
                    error = %e,
                    "merge with conflict markers applied in resumed worktree"
                );
            }

            // Detect conflicting files via unmerged paths
            match worktree_git
                .run_command(vec![
                    "diff".into(),
                    "--name-only".into(),
                    "--diff-filter=U".into(),
                ])
                .await
            {
                Ok(out) => {
                    let files: Vec<String> = out
                        .stdout
                        .lines()
                        .map(|l| l.trim().to_string())
                        .filter(|l| !l.is_empty())
                        .collect();
                    if files.is_empty() { None } else { Some(files) }
                }
                Err(_) => None,
            }
        }
        Err(e) => {
            tracing::warn!(
                branch = %branch,
                upstream = %upstream,
                error = %e,
                "failed to rebase resumed task branch"
            );
            None
        }
    }
}

pub(crate) async fn commit_wip_if_needed(
    task_id: &str,
    worktree_path: &Path,
    app_state: &AgentContext,
) {
    let git = match app_state.git_actor(worktree_path).await {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "failed to open git actor for worktree");
            return;
        }
    };

    let status = match git
        .run_command(vec!["status".into(), "--porcelain".into()])
        .await
    {
        Ok(out) => out,
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "failed to read worktree status");
            return;
        }
    };

    if status.stdout.trim().is_empty() {
        return;
    }

    if let Err(e) = git.run_command(vec!["add".into(), "-A".into()]).await {
        tracing::warn!(task_id = %task_id, error = %e, "failed to stage interrupted session changes");
        return;
    }

    let message = format!("WIP: interrupted session {task_id}");
    if let Err(e) = git
        .run_command(vec![
            "commit".into(),
            "--no-verify".into(),
            "-m".into(),
            message,
        ])
        .await
    {
        tracing::warn!(task_id = %task_id, error = %e, "failed to commit interrupted session changes");
    }
}

pub(crate) async fn commit_final_work_if_needed(
    task_id: &str,
    worktree_path: &Path,
    app_state: &AgentContext,
    commit_message: Option<&str>,
) -> Result<(), String> {
    let git = app_state
        .git_actor(worktree_path)
        .await
        .map_err(|e| format!("failed to open git actor for worktree: {e}"))?;

    let status = git
        .run_command(vec!["status".into(), "--porcelain".into()])
        .await
        .map_err(|e| format!("failed to read worktree status: {e}"))?;

    if status.stdout.trim().is_empty() {
        return Ok(());
    }

    git.run_command(vec!["add".into(), "-A".into()])
        .await
        .map_err(|e| format!("failed to stage completed session changes: {e}"))?;

    let message = match commit_message {
        Some(msg) if !msg.trim().is_empty() => msg.to_string(),
        _ => format!("WIP: auto-save completed session {task_id}"),
    };
    git.run_command(vec![
        "commit".into(),
        "--no-verify".into(),
        "-m".into(),
        message,
    ])
    .await
    .map_err(|e| format!("failed to commit completed session changes: {e}"))?;

    Ok(())
}

/// Post-session worktree cleanup.  The worktree is intentionally **kept alive**
/// so that subsequent sessions on the same task branch can reuse its `target/`
/// build cache.  Real cleanup happens via `teardown_worktree` on task
/// close/merge or when the PM calls `task_delete_branch`.
pub(crate) async fn cleanup_worktree(
    task_id: &str,
    worktree_path: &Path,
    _app_state: &AgentContext,
) {
    tracing::debug!(
        task_id = %task_id,
        worktree = %worktree_path.display(),
        "Lifecycle: keeping worktree alive for build cache reuse"
    );
}

/// Tear down a task worktree in the correct order to avoid git errors:
/// 1. LSP clients shut down (must happen before directory removal)
/// 2. Git worktree metadata removed (`git worktree remove`)
/// 3. Worktree directory removed (filesystem fallback if git remove failed)
/// 4. Task branch deleted — safe only AFTER the worktree is gone (set `delete_branch` true
///    for merge/close paths; false when releasing a task back for retry)
/// 5. Git worktree prune (clean stale metadata)
pub async fn teardown_worktree(
    task_short_id: &str,
    worktree_path: &Path,
    project_path: &Path,
    app_state: &AgentContext,
    delete_branch: bool,
) {
    // Safety: never operate on the main worktree (project root).  Linked
    // worktrees have `.git` as a *file*; the main worktree has it as a dir.
    if worktree_path.join(".git").is_dir() {
        tracing::debug!(
            task_id = %task_short_id,
            worktree = %worktree_path.display(),
            "teardown_worktree: skipping — path is the main worktree"
        );
        return;
    }

    // 1. Shut down any LSP clients whose root is inside this worktree.
    app_state.lsp.shutdown_for_worktree(worktree_path).await;

    // 2. Remove git worktree metadata.
    if let Ok(git) = app_state.git_actor(project_path).await {
        let _ = git.remove_worktree(worktree_path).await;
    }

    // 3. Remove the directory (fallback if git remove left it behind).
    if worktree_path.exists() {
        let _ = tokio::fs::remove_dir_all(worktree_path).await;
    }

    // 4. Delete the task branch (now safe — worktree is gone).
    if delete_branch {
        let branch = format!("task/{task_short_id}");
        if let Ok(git) = app_state.git_actor(project_path).await
            && let Err(e) = git.delete_branch(&branch).await
        {
            tracing::warn!(
                task_id = %task_short_id,
                branch = %branch,
                error = %e,
                "teardown_worktree: failed to delete task branch"
            );
        }
    }

    // 5. Prune stale worktree metadata.
    if let Ok(git) = app_state.git_actor(project_path).await {
        let _ = git
            .run_command(vec!["worktree".into(), "prune".into()])
            .await;
    }
}

/// Remove all worktrees for all projects on execution start.
///
/// Cleans both git worktree metadata and leftover filesystem directories under
/// each project's `.djinn/worktrees/`. Skips entries that start with `.` (sync
/// worktrees are transient and handled separately).
pub async fn purge_all_worktrees(app_state: &AgentContext) {
    let project_repo = ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let projects = match project_repo.list().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "purge_all_worktrees: failed to list projects");
            return;
        }
    };

    for project in &projects {
        purge_project_worktrees(&project.path, app_state).await;
    }
}

async fn purge_project_worktrees(project_path: &str, app_state: &AgentContext) {
    let project_dir = Path::new(project_path);
    let worktrees_dir = project_dir.join(".djinn").join("worktrees");
    if !worktrees_dir.exists() {
        return;
    }

    let git = match app_state.git_actor(project_dir).await {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(project = %project_path, error = %e, "purge_project_worktrees: failed to open git actor");
            return;
        }
    };

    // First, prune stale git worktree metadata (references to directories that no longer exist).
    let _ = git
        .run_command(vec!["worktree".into(), "prune".into()])
        .await;

    // Then remove all worktree directories.
    let entries = match std::fs::read_dir(&worktrees_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(project = %project_path, error = %e, "purge_project_worktrees: failed to read worktrees dir");
            return;
        }
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip hidden entries (sync worktrees like `.sync-task-foo`).
        if name_str.starts_with('.') {
            continue;
        }

        let wt_path = entry.path();
        if !wt_path.is_dir() {
            continue;
        }

        tracing::info!(
            project = %project_path,
            worktree = %name_str,
            "purge_project_worktrees: removing worktree on execution start"
        );

        // Try git removal first (cleans metadata), then filesystem fallback.
        let _ = git.remove_worktree(&wt_path).await;
        if wt_path.exists()
            && let Err(e) = std::fs::remove_dir_all(&wt_path)
        {
            tracing::warn!(
                project = %project_path,
                worktree = %name_str,
                error = %e,
                "purge_project_worktrees: failed to remove worktree directory"
            );
        }
    }

    // Final prune to clean up any remaining stale metadata.
    let _ = git
        .run_command(vec!["worktree".into(), "prune".into()])
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;
    use tokio_util::sync::CancellationToken;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prepare_worktree_cleans_broken_remnant() {
        // Set up a real git repo in a temp directory.
        let tmp = tempfile::Builder::new()
            .prefix("djinn-worktree-")
            .tempdir_in("/tmp")
            .unwrap();
        let project_dir = tmp.path();

        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(project_dir)
            .output()
            .unwrap();

        // Create .djinn/.gitignore so the bootstrap commit has something to stage.
        let djinn_dir = project_dir.join(".djinn");
        std::fs::create_dir_all(&djinn_dir).unwrap();
        std::fs::write(djinn_dir.join(".gitignore"), "worktrees/\n").unwrap();

        std::process::Command::new("git")
            .args(["add", ".djinn/.gitignore"])
            .current_dir(project_dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "--no-verify", "-m", "init"])
            .current_dir(project_dir)
            .output()
            .unwrap();

        // Register the project in the DB so prepare_worktree can find it.
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
        let project_path = project_dir.to_str().unwrap();
        let project_repo =
            djinn_db::ProjectRepository::new(db.clone(), test_helpers::test_events());
        let project = project_repo.create("test", project_path).await.unwrap();

        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        // Create a broken worktree remnant: directory exists with target/ but no .git.
        let worktree_path = project_dir
            .join(".djinn")
            .join("worktrees")
            .join(&task.short_id);
        std::fs::create_dir_all(worktree_path.join("target")).unwrap();
        assert!(worktree_path.exists());
        assert!(!worktree_path.join(".git").exists());

        // prepare_worktree should clean the remnant and create a valid worktree.
        let result = prepare_worktree(project_dir, &task, &ctx).await;
        assert!(
            result.is_ok(),
            "prepare_worktree should succeed: {result:?}"
        );

        let (wt, _conflicts) = result.unwrap();
        assert!(wt.join(".git").exists(), "worktree should have .git file");
    }
}
