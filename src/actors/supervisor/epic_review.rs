use super::*;

impl AgentSupervisor {
    pub(super) async fn merge_after_task_review(
        &self,
        task_id: &str,
    ) -> Option<(TransitionAction, Option<String>)> {
        let repo =
            TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let task = match repo.get(task_id).await {
            Ok(Some(task)) => task,
            Ok(None) => {
                return Some((
                    TransitionAction::ReleaseTaskReview,
                    Some("task missing during post-review merge".to_string()),
                ));
            }
            Err(e) => {
                return Some((
                    TransitionAction::ReleaseTaskReview,
                    Some(format!("failed to load task for merge: {e}")),
                ));
            }
        };

        let project_dir = self
            .project_path_for_id(&task.project_id)
            .await
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let git = match self.app_state.git_actor(&project_dir).await {
            Ok(git) => git,
            Err(e) => {
                return Some((
                    TransitionAction::ReleaseTaskReview,
                    Some(format!("failed to open git actor for merge: {e}")),
                ));
            }
        };

        let base_branch = format!("task/{}", task.short_id);
        let merge_target = self.default_target_branch(&task.project_id).await;
        let commit_type = if task.issue_type == "task" {
            "chore"
        } else {
            "feat"
        };
        let message = format!("{}({}): {}", commit_type, task.short_id, task.title);

        match git
            .squash_merge(&base_branch, &merge_target, &message)
            .await
        {
            Ok(result) => {
                tracing::info!(
                    task_id = %task.short_id,
                    task_uuid = %task.id,
                    base_branch = %base_branch,
                    merge_target = %merge_target,
                    commit_sha = %result.commit_sha,
                    "Supervisor: post-review squash merge succeeded"
                );
                if let Err(e) = git.delete_branch(&base_branch).await {
                    tracing::warn!(
                        task_id = %task.short_id,
                        branch = %base_branch,
                        error = %e,
                        "failed to delete task branch after successful merge"
                    );
                }
                if let Err(e) = repo.set_merge_commit_sha(task_id, &result.commit_sha).await {
                    return Some((
                        TransitionAction::ReleaseTaskReview,
                        Some(format!("merged but failed to store merge SHA: {e}")),
                    ));
                }
                // Clean up the worker's paused session (worktree + record finalization).
                self.cleanup_paused_worker_session(task_id).await;
                Some((TransitionAction::TaskReviewApprove, None))
            }
            Err(GitError::MergeConflict { files, .. }) => {
                tracing::warn!(
                    task_id = %task.short_id,
                    task_uuid = %task.id,
                    base_branch = %base_branch,
                    merge_target = %merge_target,
                    conflict_count = files.len(),
                    conflicting_files = ?files,
                    "Supervisor: post-review merge conflict"
                );
                let metadata = MergeConflictMetadata {
                    conflicting_files: files,
                    base_branch,
                    merge_target,
                };
                let reason = match serde_json::to_string(&metadata) {
                    Ok(v) => format!("{MERGE_CONFLICT_PREFIX}{v}"),
                    Err(_) => format!("{MERGE_CONFLICT_PREFIX}{{}}"),
                };
                let payload = serde_json::to_string(&metadata).unwrap_or_else(|_| "{}".to_string());
                let _ = repo
                    .log_activity(
                        Some(task_id),
                        "agent-supervisor",
                        "system",
                        "merge_conflict",
                        &payload,
                    )
                    .await;
                Some((TransitionAction::TaskReviewRejectConflict, Some(reason)))
            }
            Err(GitError::CommitRejected {
                code,
                command,
                cwd,
                stdout,
                stderr,
            }) => {
                tracing::warn!(
                    task_id = %task.short_id,
                    task_uuid = %task.id,
                    base_branch = %base_branch,
                    merge_target = %merge_target,
                    exit_code = code,
                    command = %command,
                    cwd = %cwd,
                    stdout_snippet = %log_snippet(&stdout, 400),
                    stderr_snippet = %log_snippet(&stderr, 400),
                    "Supervisor: post-review merge commit rejected"
                );
                let metadata = MergeValidationFailureMetadata {
                    base_branch,
                    merge_target,
                    command,
                    cwd,
                    exit_code: code,
                    stdout,
                    stderr,
                };
                let reason_payload =
                    serde_json::to_string(&metadata).unwrap_or_else(|_| "{}".to_string());
                let reason = format!("{MERGE_VALIDATION_PREFIX}{reason_payload}");
                let _ = repo
                    .log_activity(
                        Some(task_id),
                        "agent-supervisor",
                        "system",
                        "merge_validation_failed",
                        &reason_payload,
                    )
                    .await;
                Some((TransitionAction::TaskReviewRejectConflict, Some(reason)))
            }
            Err(e) => {
                tracing::warn!(
                    task_id = %task.short_id,
                    task_uuid = %task.id,
                    base_branch = %base_branch,
                    merge_target = %merge_target,
                    error = %e,
                    error_debug = ?e,
                    "Supervisor: post-review squash merge failed"
                );
                Some((
                    TransitionAction::ReleaseTaskReview,
                    Some(format!("post-review squash merge failed: {e} ({e:?})")),
                ))
            }
        }
    }

    pub(super) async fn finalize_epic_batch(
        &self,
        task_id: &str,
        output: &ParsedAgentOutput,
        error_reason: Option<&str>,
    ) {
        let Some(batch_id) = self.active_epic_batch_for_task(task_id).await else {
            return;
        };
        let task_repo =
            TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let Some(task) = task_repo.get(task_id).await.ok().flatten() else {
            return;
        };
        let Some(epic_id) = task.epic_id.as_deref() else {
            return;
        };

        let batch_repo = EpicReviewBatchRepository::new(
            self.app_state.db().clone(),
            self.app_state.events().clone(),
        );
        let epic_repo =
            EpicRepository::new(self.app_state.db().clone(), self.app_state.events().clone());

        match output.epic_verdict {
            Some(EpicReviewVerdict::Clean) => {
                if let Err(e) = batch_repo.mark_clean(&batch_id).await {
                    tracing::warn!(batch_id = %batch_id, error = %e, "failed to mark epic review batch clean");
                    return;
                }

                let tasks = match task_repo.list_by_epic(epic_id).await {
                    Ok(tasks) => tasks,
                    Err(e) => {
                        tracing::warn!(epic_id = %epic_id, error = %e, "failed to list epic tasks after clean review");
                        return;
                    }
                };
                if tasks.iter().all(|t| t.status == "closed") {
                    let _ = epic_repo.close(epic_id).await;
                }
            }
            Some(EpicReviewVerdict::IssuesFound) => {
                let verdict = "epic reviewer reported EPIC_REVIEW_RESULT: ISSUES_FOUND";
                let _ = batch_repo.mark_issues_found(&batch_id, verdict).await;
                if let Ok(Some(epic)) = epic_repo.get(epic_id).await
                    && epic.status == "in_review"
                {
                    let _ = epic_repo.reopen(epic_id).await;
                }
            }
            None => {
                let verdict = error_reason
                    .unwrap_or("epic reviewer ended without required EPIC_REVIEW_RESULT marker");
                let _ = batch_repo.mark_issues_found(&batch_id, verdict).await;
                if let Ok(Some(epic)) = epic_repo.get(epic_id).await
                    && epic.status == "in_review"
                {
                    let _ = epic_repo.reopen(epic_id).await;
                }
            }
        }
    }

    pub(super) async fn active_epic_batch_for_task(&self, task_id: &str) -> Option<String> {
        let repo = EpicReviewBatchRepository::new(
            self.app_state.db().clone(),
            self.app_state.events().clone(),
        );
        repo.active_batch_for_task(task_id)
            .await
            .ok()
            .flatten()
            .map(|b| b.id)
    }
}
