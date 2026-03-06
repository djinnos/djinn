use super::*;

impl AgentSupervisor {
    pub(super) async fn kill_session(&mut self, task_id: String) -> Result<(), SupervisorError> {
        if let Some(mut handle) = self.lifecycle_handles.remove(&task_id) {
            handle.kill.cancel();
            let _ = tokio::time::timeout(Duration::from_secs(30), &mut handle.join).await;
            return Ok(());
        }

        let Some(mut handle) = self.sessions.remove(&task_id) else {
            return Ok(());
        };

        handle.cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(30), &mut handle.join).await;

        self.interrupted_sessions.insert(task_id.clone());
        let model_id = self.session_models.remove(&task_id);
        self.session_agent_types.remove(&task_id);
        self.session_projects.remove(&task_id);
        self.task_session_records.remove(&task_id);
        self.decrement_capacity_for_model(model_id.as_deref());
        self.in_flight.remove(&task_id);

        Ok(())
    }

    pub(super) async fn pause_session(&mut self, task_id: String) -> Result<(), SupervisorError> {
        if let Some(mut handle) = self.lifecycle_handles.remove(&task_id) {
            handle.pause.cancel();
            let _ = tokio::time::timeout(Duration::from_secs(30), &mut handle.join).await;
            return Ok(());
        }

        let Some(mut handle) = self.sessions.remove(&task_id) else {
            return Ok(());
        };

        handle.cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(30), &mut handle.join).await;

        self.interrupted_sessions.insert(task_id.clone());
        let model_id = self.session_models.remove(&task_id);
        self.session_agent_types.remove(&task_id);
        self.session_projects.remove(&task_id);
        self.task_session_records.remove(&task_id);
        self.decrement_capacity_for_model(model_id.as_deref());
        self.in_flight.remove(&task_id);

        Ok(())
    }

    pub(super) async fn commit_wip_if_needed(&self, task_id: &str, worktree_path: &Path) {
        let git = match self.app_state.git_actor(worktree_path).await {
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

    pub(super) async fn cleanup_worktree(&self, task_id: &str, worktree_path: &Path) {
        let task = match self.load_task(task_id).await {
            Ok(task) => task,
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "failed to load task for worktree cleanup");
                return;
            }
        };

        let Some(project_path) = self.project_path_for_id(&task.project_id).await else {
            tracing::warn!(task_id = %task_id, "project path not found for worktree cleanup");
            return;
        };

        let git = match self.app_state.git_actor(Path::new(&project_path)).await {
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

    pub(super) async fn transition_interrupted(
        &self,
        task_id: &str,
        agent_type: AgentType,
        reason: &str,
    ) {
        let action = match agent_type {
            AgentType::Worker | AgentType::ConflictResolver => Some(TransitionAction::Release),
            AgentType::TaskReviewer => Some(TransitionAction::ReleaseTaskReview),
            AgentType::EpicReviewer => None,
        };

        let Some(action) = action else {
            return;
        };

        let repo =
            TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        if let Err(e) = repo
            .transition(
                task_id,
                action,
                "agent-supervisor",
                "system",
                Some(reason),
                None,
            )
            .await
        {
            tracing::warn!(task_id = %task_id, error = %e, "failed to transition interrupted task");
        }
    }
}
