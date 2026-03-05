use super::*;

impl AgentSupervisor {
    pub(in super::super) async fn handle_session_result(
        &self,
        task_id: &str,
        session: SessionClosure,
        result: Result<(), String>,
        output: ParsedAgentOutput,
    ) {
        let agent_type = session.agent_type;
        let repo =
            TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());

        if let Some(model_id) = session.model_id.as_deref() {
            match &result {
                Ok(()) => self.app_state.health_tracker().record_success(model_id),
                Err(_) => self.app_state.health_tracker().record_failure(model_id),
            }
            self.app_state.persist_model_health_state().await;
        }

        let (tokens_in, tokens_out) = self.tokens_for_session(&session.goose_session_id).await;

        // Worker Done: pause session record (keep worktree alive for resume after review).
        // All other cases: complete or fail the session record.
        let is_worker_done = result.is_ok()
            && matches!(agent_type, AgentType::Worker | AgentType::ConflictResolver)
            && matches!(output.worker_signal, Some(WorkerSignal::Done));

        if is_worker_done {
            self.update_session_record_paused(session.record_id.as_deref(), tokens_in, tokens_out)
                .await;
        } else {
            let session_status = if result.is_ok() {
                SessionStatus::Completed
            } else {
                SessionStatus::Failed
            };
            self.update_session_record(
                session.record_id.as_deref(),
                session_status,
                tokens_in,
                tokens_out,
            )
            .await;
        }

        if let Some(worktree_path) = session.worktree_path.as_ref() {
            // Post-DONE validation pipeline: setup → verification.
            // Any failure at either step bounces back to the worker with feedback.
            if is_worker_done {
                // Re-run setup commands to catch issues like stale lockfiles,
                // missing dependencies, etc. introduced by the worker's changes.
                if let Some(feedback) =
                    self.run_setup_commands_checked(task_id, worktree_path).await
                {
                    self.queue_resume_after_verification_failure(
                        task_id,
                        &session,
                        worktree_path,
                        &feedback,
                        tokens_in,
                    )
                    .await;
                    return;
                }

                // Run verification commands (tsc, cargo check, etc.).
                if let Some(feedback) = self.run_verification_commands(task_id, worktree_path).await
                {
                    self.queue_resume_after_verification_failure(
                        task_id,
                        &session,
                        worktree_path,
                        &feedback,
                        tokens_in,
                    )
                    .await;
                    return;
                }
            }

            if is_worker_done {
                // Commit final work and keep worktree alive for the review→resume cycle.
                if let Err(e) = self
                    .commit_final_work_if_needed(task_id, worktree_path)
                    .await
                {
                    tracing::warn!(
                        task_id = %task_id,
                        worktree_path = %worktree_path.display(),
                        error = %e,
                        "failed to commit work before pausing for review; preserving worktree"
                    );
                }
                // Worktree intentionally kept — cleaned up in cleanup_paused_worker_session
                // when the task is finally approved.
            } else {
                self.cleanup_worktree(task_id, worktree_path).await;
            }
        }

        if let Some(feedback) = output.reviewer_feedback.as_deref() {
            let payload = serde_json::json!({ "body": feedback }).to_string();
            if let Err(e) = repo
                .log_activity(
                    Some(task_id),
                    "agent-supervisor",
                    "task_reviewer",
                    "comment",
                    &payload,
                )
                .await
            {
                tracing::warn!(task_id = %task_id, error = %e, "failed to store reviewer feedback comment");
            }
        }

        if let Err(reason) = &result {
            let payload = serde_json::json!({
                "error": reason,
                "agent_type": agent_type.as_str(),
            })
            .to_string();
            if let Err(e) = repo
                .log_activity(
                    Some(task_id),
                    "agent-supervisor",
                    "system",
                    "session_error",
                    &payload,
                )
                .await
            {
                tracing::warn!(task_id = %task_id, error = %e, "failed to store session error activity");
            }
        }

        if result.is_ok()
            && let Some(reason) = output.runtime_error.as_deref()
        {
            let payload = serde_json::json!({
                "error": reason,
                "agent_type": agent_type.as_str(),
            })
            .to_string();
            if let Err(e) = repo
                .log_activity(
                    Some(task_id),
                    "agent-supervisor",
                    "system",
                    "session_error",
                    &payload,
                )
                .await
            {
                tracing::warn!(task_id = %task_id, error = %e, "failed to store session error activity");
            }
        }

        let epic_error = result.as_ref().err().cloned();
        let transition = match result {
            Ok(()) => self.success_transition(task_id, agent_type, &output).await,
            Err(reason) => match agent_type {
                AgentType::Worker | AgentType::ConflictResolver => {
                    Some((TransitionAction::Release, Some(reason)))
                }
                AgentType::TaskReviewer => {
                    Some((TransitionAction::ReleaseTaskReview, Some(reason)))
                }
                AgentType::EpicReviewer => None,
            },
        };

        if agent_type == AgentType::EpicReviewer {
            self.finalize_epic_batch(task_id, &output, epic_error.as_deref())
                .await;
        }

        if let Some((action, reason)) = transition {
            tracing::info!(
                task_id = %task_id,
                agent_type = %agent_type.as_str(),
                transition_action = ?action,
                transition_reason = reason.as_deref().unwrap_or("<none>"),
                tokens_in,
                tokens_out,
                "Supervisor: applying session transition"
            );
            let is_reviewer_rejection = matches!(
                action,
                TransitionAction::TaskReviewReject | TransitionAction::TaskReviewRejectConflict
            );
            if let Err(e) = repo
                .transition(
                    task_id,
                    action,
                    "agent-supervisor",
                    "system",
                    reason.as_deref(),
                    None,
                )
                .await
            {
                tracing::warn!(task_id = %task_id, error = %e, "failed to transition task after session");
            }

            // After a reviewer rejection, interrupt any paused worker session so the
            // next dispatch starts a fresh Goose session. Without this, the resumed
            // worker sees its own "I already completed this" conversation history and
            // outputs DONE immediately without doing real work → infinite reject loop.
            // The worktree is preserved so the fresh worker can inspect existing state.
            if is_reviewer_rejection {
                self.interrupt_paused_worker_session(task_id).await;
            }
        } else {
            tracing::info!(
                task_id = %task_id,
                agent_type = %agent_type.as_str(),
                tokens_in,
                tokens_out,
                "Supervisor: session completed with no task transition"
            );
        }

        // Capacity has just been released by this session completion. Trigger an
        // immediate dispatch pass for the same project so the next ready task
        // starts without waiting for the coordinator interval tick.
        if let Ok(task) = self.load_task(task_id).await
            && let Some(coordinator) = self.app_state.coordinator().await
        {
            let _ = coordinator
                .trigger_dispatch_for_project(&task.project_id)
                .await;
        }
    }
}
