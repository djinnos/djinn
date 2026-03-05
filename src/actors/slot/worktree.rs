use std::path::{Path, PathBuf};

use crate::actors::git::GitError;
use crate::db::repositories::session::SessionRepository;
use crate::server::AppState;

use super::*;

// ─── Git / worktree helpers ───────────────────────────────────────────────────

pub(crate) async fn prepare_worktree(
    project_dir: &Path,
    task: &crate::models::task::Task,
    app_state: &AppState,
) -> anyhow::Result<PathBuf> {
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

    let session_repo =
        SessionRepository::new(app_state.db().clone(), app_state.events().clone());
    let has_paused_session = session_repo
        .paused_for_task(&task.id)
        .await
        .ok()
        .flatten()
        .is_some();
    if has_paused_session
        && stale_worktree_path.exists()
        && stale_worktree_path.join(".git").exists()
    {
        tracing::info!(
            task_id = %task.short_id,
            worktree = %stale_worktree_path.display(),
            "Lifecycle: reusing existing worktree from paused session"
        );
        return Ok(stale_worktree_path);
    }

    let _ = git.remove_worktree(&stale_worktree_path).await;
    if stale_worktree_path.exists() {
        let _ = std::fs::remove_dir_all(&stale_worktree_path);
    }

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
        try_rebase_existing_task_branch(project_dir, &branch, &target_branch, app_state).await;
    }

    git.create_worktree(&task.short_id, &branch, false)
        .await
        .map_err(|e| anyhow::anyhow!("create worktree: {e}"))
}

pub(crate) async fn prepare_epic_reviewer_worktree(
    project_dir: &Path,
    batch_id: &str,
    app_state: &AppState,
) -> anyhow::Result<PathBuf> {
    let git = app_state
        .git_actor(project_dir)
        .await
        .map_err(|e| anyhow::anyhow!("git actor: {e}"))?;

    let folder_name = format!("batch-{batch_id}");
    let stale_path = project_dir
        .join(".djinn")
        .join("worktrees")
        .join(&folder_name);
    let _ = git.remove_worktree(&stale_path).await;
    if stale_path.exists() {
        let _ = std::fs::remove_dir_all(&stale_path);
    }

    git.create_worktree(&folder_name, "HEAD", true)
        .await
        .map_err(|e| anyhow::anyhow!("create epic reviewer worktree: {e}"))
}

pub(crate) async fn try_rebase_existing_task_branch(
    project_dir: &Path,
    branch: &str,
    target_branch: &str,
    app_state: &AppState,
) {
    let git = match app_state.git_actor(project_dir).await {
        Ok(git) => git,
        Err(e) => {
            tracing::warn!(branch = %branch, error = %e, "failed to open git actor for branch sync");
            return;
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
            return;
        }
    };

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
        return;
    }

    let sync_git = match app_state.git_actor(&sync_worktree_path).await {
        Ok(git) => git,
        Err(e) => {
            tracing::warn!(branch = %branch, error = %e, "failed to open sync worktree git actor");
            let _ = git.remove_worktree(&sync_worktree_path).await;
            if sync_worktree_path.exists() {
                let _ = std::fs::remove_dir_all(&sync_worktree_path);
            }
            return;
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
}

pub(crate) async fn commit_wip_if_needed(
    task_id: &str,
    worktree_path: &Path,
    app_state: &AppState,
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
    app_state: &AppState,
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

    let message = format!("WIP: auto-save completed session {task_id}");
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

pub(crate) async fn cleanup_worktree(
    task_id: &str,
    worktree_path: &Path,
    app_state: &AppState,
) {
    let session_repo =
        SessionRepository::new(app_state.db().clone(), app_state.events().clone());
    if let Ok(Some(paused)) = session_repo.paused_for_task(task_id).await
        && paused.worktree_path.as_deref() == Some(worktree_path.to_str().unwrap_or("")) {
            tracing::info!(
                task_id = %task_id,
                worktree = %worktree_path.display(),
                "Lifecycle: skipping worktree cleanup — paused session still references it"
            );
            return;
        }

    let task = match load_task(task_id, app_state).await {
        Ok(task) => task,
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "failed to load task for worktree cleanup");
            return;
        }
    };

    let Some(project_path) = project_path_for_id(&task.project_id, app_state).await else {
        tracing::warn!(task_id = %task_id, "project path not found for worktree cleanup");
        return;
    };

    let git = match app_state.git_actor(Path::new(&project_path)).await {
        Ok(git) => git,
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "failed to open git actor for worktree cleanup");
            return;
        }
    };

    if let Err(e) = git.remove_worktree(worktree_path).await {
        tracing::warn!(task_id = %task_id, error = %e, "failed to remove worktree; attempting filesystem cleanup");
        if worktree_path.exists()
            && let Err(remove_err) = std::fs::remove_dir_all(worktree_path)
        {
            tracing::warn!(task_id = %task_id, error = %remove_err, "failed to remove worktree directory");
        }
    }
}
