use super::*;

impl AgentSupervisor {
    pub(super) async fn prepare_worktree(
        &self,
        project_dir: &Path,
        task: &Task,
    ) -> Result<PathBuf, SupervisorError> {
        let branch = format!("task/{}", task.short_id);
        let target_branch = self.default_target_branch(&task.project_id).await;
        let git = self
            .app_state
            .git_actor(project_dir)
            .await
            .map_err(|e| SupervisorError::Goose(e.to_string()))?;

        let stale_worktree_path = project_dir
            .join(".djinn")
            .join("worktrees")
            .join(&task.short_id);

        // If a paused session still references this worktree, reuse it
        // instead of destroying and recreating. This prevents "worktree
        // missing" errors when a session is resumed after dispatch.
        let session_repo =
            SessionRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
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
                "Supervisor: reusing existing worktree from paused session"
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
            Err(e) => return Err(SupervisorError::Goose(e.to_string())),
        };

        if !branch_exists {
            git.create_branch(&task.short_id, &target_branch)
                .await
                .map_err(|e| SupervisorError::Goose(e.to_string()))?;
        } else {
            self.try_rebase_existing_task_branch(project_dir, &branch, &target_branch)
                .await;
        }

        git.create_worktree(&task.short_id, &branch, false)
            .await
            .map_err(|e| SupervisorError::Goose(e.to_string()))
    }

    pub(super) async fn prepare_epic_reviewer_worktree(
        &self,
        project_dir: &Path,
        batch_id: &str,
    ) -> Result<PathBuf, SupervisorError> {
        let git = self
            .app_state
            .git_actor(project_dir)
            .await
            .map_err(|e| SupervisorError::Goose(e.to_string()))?;

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
            .map_err(|e| SupervisorError::Goose(e.to_string()))
    }

    pub(super) async fn try_rebase_existing_task_branch(
        &self,
        project_dir: &Path,
        branch: &str,
        target_branch: &str,
    ) {
        let git = match self.app_state.git_actor(project_dir).await {
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

        let sync_git = match self.app_state.git_actor(&sync_worktree_path).await {
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

    pub(super) async fn default_target_branch(&self, project_id: &str) -> String {
        let repo = ProjectRepository::new(
            self.app_state.db().clone(),
            self.app_state.events().clone(),
        );
        if let Ok(Some(config)) = repo.get_config(project_id).await {
            return config.target_branch;
        }

        "main".to_string()
    }

    pub(super) async fn project_path_for_id(&self, project_id: &str) -> Option<String> {
        sqlx::query_scalar::<_, String>("SELECT path FROM projects WHERE id = ?1")
            .bind(project_id)
            .fetch_optional(self.app_state.db().pool())
            .await
            .ok()
            .flatten()
    }
}
