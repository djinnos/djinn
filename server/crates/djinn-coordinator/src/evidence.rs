use super::*;
use crate::supervisor_impl::disposition::LiveMoverEvidence;
use djinn_core::models::Task;

const READY_FOR_DISPATCH_STATUSES: &[&str] =
    &["open", "needs_task_review", "needs_lead_intervention"];
const PR_POLLER_OWNED_STATUSES: &[&str] = &["pr_draft", "pr_review"];
const REVIEW_PENDING_STATUSES: &[&str] = &["needs_task_review", "in_task_review"];

impl CoordinatorActor {
    /// Gather hard coordinator/board facts for the pure live-mover predicate.
    ///
    /// This method is deliberately an adapter: it performs fallible slot-pool and
    /// repository reads, translates runtime/board state into [`LiveMoverEvidence`],
    /// and leaves the actual live-mover decision to the pure predicate in
    /// `supervisor_impl::disposition`.
    #[allow(dead_code)]
    pub(crate) async fn collect_live_mover_evidence(
        &self,
        task: &Task,
    ) -> Result<LiveMoverEvidence, LiveMoverEvidenceError> {
        let active_session = self
            .pool
            .has_session(&task.id)
            .await
            .map_err(LiveMoverEvidenceError::Pool)?;
        let unresolved_blockers = task_has_unresolved_blockers(&self.task_repo(), task).await?;

        Ok(live_mover_evidence_from_coordinator_state(
            task,
            active_session,
            self.inflight_dispatches.contains_key(&task.id),
            self.last_dispatched.contains_key(&task.id),
            unresolved_blockers,
        ))
    }

    /// Predicate-driven no-mover disposition call site for settled/orphaned
    /// tasks that did not take the PR-open zero-commit fork.
    pub(crate) async fn route_settled_noop_without_live_mover(&self, task_id: &str) {
        let task_repo = self.task_repo();
        let task = match task_repo.get(task_id).await {
            Ok(Some(task)) => task,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(
                    task_id = %task_id,
                    error = %e,
                    "CoordinatorActor: no-mover disposition skipped; failed to load task"
                );
                return;
            }
        };

        let evidence = match self.collect_live_mover_evidence(&task).await {
            Ok(evidence) => evidence,
            Err(e) => {
                tracing::warn!(
                    task_id = %task.id,
                    error = %e,
                    "CoordinatorActor: no-mover disposition skipped; failed to collect live-mover evidence"
                );
                return;
            }
        };

        let merge_target = match ProjectRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        )
        .get_config(&task.project_id)
        .await
        {
            Ok(Some(config)) => config.target_branch,
            _ => "main".to_owned(),
        };

        let _ = crate::supervisor_impl::pr::handle_settled_noop_without_live_mover(
            &task,
            &task_repo,
            &merge_target,
            &evidence,
        )
        .await;
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum LiveMoverEvidenceError {
    #[error("slot pool evidence query failed: {0}")]
    Pool(#[from] PoolError),
    #[error("task repository evidence query failed: {0}")]
    TaskRepository(#[from] djinn_db::Error),
}

async fn task_has_unresolved_blockers(
    task_repo: &TaskRepository,
    task: &Task,
) -> Result<bool, LiveMoverEvidenceError> {
    if task.unresolved_blocker_count > 0 {
        return Ok(true);
    }

    let blockers = task_repo
        .list_blockers(&task.id)
        .await
        .map_err(LiveMoverEvidenceError::TaskRepository)?;
    Ok(blockers.iter().any(|blocker| blocker.status != "closed"))
}

fn live_mover_evidence_from_coordinator_state(
    task: &Task,
    active_session: bool,
    dispatch_inflight: bool,
    recently_dispatched: bool,
    unresolved_blockers: bool,
) -> LiveMoverEvidence {
    let has_pr_url = task.pr_url.as_deref().is_some_and(|url| !url.is_empty());
    let pr_poller_owned = has_pr_url && PR_POLLER_OWNED_STATUSES.contains(&task.status.as_str());

    LiveMoverEvidence {
        active_session,
        queued_dispatch: READY_FOR_DISPATCH_STATUSES.contains(&task.status.as_str()),
        dispatch_inflight,
        recently_dispatched,
        open_pr: has_pr_url && task.status != "closed",
        pr_poller_owned,
        review_pending_with_reviewer: REVIEW_PENDING_STATUSES.contains(&task.status.as_str()),
        unresolved_blockers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_with_status(status: &str) -> Task {
        Task {
            id: "task-id".to_owned(),
            project_id: "project-id".to_owned(),
            short_id: "task".to_owned(),
            epic_id: None,
            title: "Task".to_owned(),
            description: String::new(),
            design: String::new(),
            issue_type: "task".to_owned(),
            status: status.to_owned(),
            priority: 2,
            owner: String::new(),
            labels: "[]".to_owned(),
            acceptance_criteria: "[]".to_owned(),
            reopen_count: 0,
            continuation_count: 0,
            total_reopen_count: 0,
            intervention_count: 0,
            last_intervention_at: None,
            created_at: "2026-06-15T00:00:00.000Z".to_owned(),
            updated_at: "2026-06-15T00:00:00.000Z".to_owned(),
            closed_at: None,
            close_reason: None,
            merge_commit_sha: None,
            pr_url: None,
            merge_conflict_metadata: None,
            memory_refs: "[]".to_owned(),
            agent_type: None,
            created_by_user_id: None,
            ci_status: "unknown".to_owned(),
            ci_head_sha: None,
            ci_pr_number: None,
            ci_blocking_required_check_names: "[]".to_owned(),
            ci_failure_fingerprint: None,
            ci_first_seen_at: None,
            ci_last_seen_at: None,
            ci_same_signature_count: 0,
            ci_last_remediation_base_sha: None,
            ci_mirror_head_sha: None,
            ci_github_head_sha: None,
            ci_heads_diverged: None,
            ci_head_observation_error: None,
            ci_mq_state: None,
            ci_mq_run_id: None,
            ci_mq_head_sha: None,
            ci_mq_failed_check_names: None,
            ci_mq_failure_fingerprint: None,
            ci_mq_same_signature_count: None,
            ci_mq_first_seen_at: None,
            ci_mq_last_seen_at: None,
            unresolved_blocker_count: 0,
        }
    }

    #[test]
    fn maps_pr_and_review_statuses_without_repository_access() {
        let mut task = task_with_status("pr_review");
        task.pr_url = Some("https://github.com/example/repo/pull/1".to_owned());

        let evidence =
            live_mover_evidence_from_coordinator_state(&task, false, false, false, false);

        assert!(evidence.open_pr);
        assert!(evidence.pr_poller_owned);
        assert!(!evidence.review_pending_with_reviewer);

        let task = task_with_status("needs_task_review");
        let evidence =
            live_mover_evidence_from_coordinator_state(&task, false, false, false, false);
        assert!(evidence.queued_dispatch);
        assert!(evidence.review_pending_with_reviewer);
    }

    #[test]
    fn no_evidence_case_is_empty() {
        let task = task_with_status("closed");

        let evidence =
            live_mover_evidence_from_coordinator_state(&task, false, false, false, false);

        assert_eq!(evidence, LiveMoverEvidence::default());
    }
}
