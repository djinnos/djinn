use super::*;
use djinn_db::repositories::task_run::TaskRunRepository;
use djinn_db::repositories::verify_run::{
    RecordTaskRejectedSubmissionParams, TaskRejectedSubmissionIntegrityRepository,
};

impl CoordinatorActor {
    /// Record a task-level rejected submission fingerprint when a PR reviewer
    /// requests changes. Looks up the task's latest workspace path, computes
    /// the diff fingerprint, and persists a rejected integrity entry.
    ///
    /// Best-effort: all error paths log and return without crashing the poller.
    pub(crate) async fn record_pr_rejection_fingerprint(&self, task_id: &str) {
        let task_run_repo = TaskRunRepository::new(self.db.clone());
        let runs = match task_run_repo.list_for_task(task_id).await {
            Ok(runs) => runs,
            Err(e) => {
                tracing::warn!(
                    task_id = %task_id,
                    error = %e,
                    "pr_review_watcher: failed to query task runs for rejected fingerprint"
                );
                return;
            }
        };

        let Some((task_run_id, workspace_path)) = runs
            .iter()
            .find(|r| r.workspace_path.is_some())
            .and_then(|r| Some((r.id.clone(), r.workspace_path.clone()?)))
        else {
            tracing::info!(
                task_id = %task_id,
                "pr_review_watcher: no worktree for rejected submission fingerprint; \
                 skipping (historical/no-worktree case)"
            );
            return;
        };

        let worktree = std::path::PathBuf::from(&workspace_path);
        let fingerprint = match djinn_git::compute_submission_diff_fingerprint(&worktree).await {
            Ok(fp) => fp,
            Err(e) => {
                tracing::warn!(
                    task_id = %task_id,
                    task_run_id = %task_run_id,
                    worktree = %workspace_path,
                    error = %e,
                    "pr_review_watcher: failed to compute submission diff fingerprint; \
                     skipping rejected fingerprint persistence"
                );
                return;
            }
        };

        let Some(digest) = fingerprint.fingerprint().map(|s| s.to_string()) else {
            tracing::info!(
                task_id = %task_id,
                task_run_id = %task_run_id,
                "pr_review_watcher: rejected submission worktree has no diff \
                 (NoDiff); skipping rejected fingerprint persistence"
            );
            return;
        };

        let integrity_repo = TaskRejectedSubmissionIntegrityRepository::new(self.db.clone());
        let current_streak = integrity_repo
            .latest_no_progress_streak_for_task(task_id)
            .await
            .unwrap_or(0);

        let rejected_at = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());

        let id = uuid::Uuid::now_v7().to_string();
        let params = RecordTaskRejectedSubmissionParams {
            id: &id,
            task_id,
            task_run_id: Some(&task_run_id),
            review_id: None,
            verdict_kind: djinn_core::models::RejectedVerdictKind::ReviewerReject.as_str(),
            activity_id: None,
            rejected_at: &rejected_at,
            diff_fingerprint: &digest,
            no_progress_streak: current_streak + 1,
        };

        if let Err(e) = integrity_repo.record(params).await {
            tracing::warn!(
                task_id = %task_id,
                task_run_id = %task_run_id,
                error = %e,
                "pr_review_watcher: failed to record rejected submission integrity"
            );
        } else {
            tracing::info!(
                task_id = %task_id,
                task_run_id = %task_run_id,
                fingerprint = %digest,
                no_progress_streak = current_streak + 1,
                "pr_review_watcher: recorded rejected submission integrity \
                 for PR changes-requested"
            );
        }
    }
}
