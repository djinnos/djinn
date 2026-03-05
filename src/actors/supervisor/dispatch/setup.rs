use super::*;

impl AgentSupervisor {
    /// Runs the project's setup commands in the task worktree.
    /// Called after conflict resolution to refresh the environment (e.g. reinstall
    /// dependencies that changed as a result of merging main into the task branch).
    /// Failures are logged as warnings but do not abort the session.
    /// Runs the project's setup commands in the task worktree.
    /// Returns `None` if all commands pass or there are no setup commands.
    /// Returns `Some(feedback)` if any command fails, with the failure details.
    pub(super) async fn run_setup_commands_checked(
        &self,
        task_id: &str,
        worktree_path: &Path,
    ) -> Option<String> {
        let task = self.load_task(task_id).await.ok()?;
        let project_repo =
            ProjectRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let project = project_repo.get(&task.project_id).await.ok()??;
        let specs: Vec<CommandSpec> =
            serde_json::from_str(&project.setup_commands).unwrap_or_default();
        if specs.is_empty() {
            return None;
        }
        tracing::info!(
            task_id = %task_id,
            command_count = specs.len(),
            "Supervisor: running setup commands"
        );
        match run_commands(&specs, worktree_path).await {
            Ok(results) => {
                let failed = results.iter().find(|r| r.exit_code != 0)?;
                tracing::info!(
                    task_id = %task_id,
                    command = %failed.name,
                    exit_code = failed.exit_code,
                    "Supervisor: setup command failed"
                );
                let trim_output = |s: &str| -> String {
                    let lines: Vec<&str> = s.trim().lines().collect();
                    if lines.len() > 50 {
                        format!(
                            "... ({} lines truncated) ...\n{}",
                            lines.len() - 50,
                            lines[lines.len() - 50..].join("\n")
                        )
                    } else {
                        lines.join("\n")
                    }
                };
                Some(format!(
                    "Setup command '{}' failed with exit code {}.\n\nYour changes likely broke a setup step (e.g. lockfile out of sync with package.json). Use your shell tools to fix the issue, then signal WORKER_RESULT: DONE.\n\nstdout:\n{}\nstderr:\n{}",
                    failed.name,
                    failed.exit_code,
                    trim_output(&failed.stdout),
                    trim_output(&failed.stderr),
                ))
            }
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "Supervisor: setup command system error");
                Some(format!(
                    "Setup commands could not run: {e}\n\nFix the issue and signal WORKER_RESULT: DONE when complete."
                ))
            }
        }
    }

    /// Runs the project's verification commands in the task worktree.
    /// Returns `None` if all commands pass or there are no verification commands.
    /// Returns `Some(feedback)` if any command fails, with the failure details.
    pub(super) async fn run_verification_commands(
        &self,
        task_id: &str,
        worktree_path: &Path,
    ) -> Option<String> {
        let task = self.load_task(task_id).await.ok()?;
        let project_repo =
            ProjectRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let project = project_repo.get(&task.project_id).await.ok()??;
        let specs: Vec<CommandSpec> =
            serde_json::from_str(&project.verification_commands).unwrap_or_default();
        if specs.is_empty() {
            return None;
        }
        tracing::info!(
            task_id = %task_id,
            command_count = specs.len(),
            "Supervisor: running verification commands"
        );
        match run_commands(&specs, worktree_path).await {
            Ok(results) => {
                let failed = results.iter().find(|r| r.exit_code != 0)?;
                tracing::info!(
                    task_id = %task_id,
                    command = %failed.name,
                    exit_code = failed.exit_code,
                    "Supervisor: verification command failed"
                );
                // Trim output to last 50 lines to avoid context overflow on noisy tools like tsc.
                // The agent has shell tools and can re-run the command or read files as needed.
                let trim_output = |s: &str| -> String {
                    let lines: Vec<&str> = s.trim().lines().collect();
                    if lines.len() > 50 {
                        format!(
                            "... ({} lines truncated) ...\n{}",
                            lines.len() - 50,
                            lines[lines.len() - 50..].join("\n")
                        )
                    } else {
                        lines.join("\n")
                    }
                };
                Some(format!(
                    "Verification command '{}' failed with exit code {}.\n\nUse your shell and editor tools to inspect and fix the issue, then signal WORKER_RESULT: DONE.\n\nstdout:\n{}\nstderr:\n{}",
                    failed.name,
                    failed.exit_code,
                    trim_output(&failed.stdout),
                    trim_output(&failed.stderr),
                ))
            }
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "Supervisor: verification command system error");
                Some(format!(
                    "Verification commands could not run: {e}\n\nFix the issue and signal WORKER_RESULT: DONE when complete."
                ))
            }
        }
    }

    /// Logs the verification failure as a task comment and queues a ResumeSession message.
    pub(super) async fn queue_resume_after_verification_failure(
        &self,
        task_id: &str,
        session: &SessionClosure,
        worktree_path: &Path,
        feedback: &str,
        tokens_in: i64,
    ) {
        let repo =
            TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let payload = serde_json::json!({ "body": feedback }).to_string();
        if let Err(e) = repo
            .log_activity(
                Some(task_id),
                "agent-supervisor",
                "verification",
                "comment",
                &payload,
            )
            .await
        {
            tracing::warn!(task_id = %task_id, error = %e, "failed to log verification failure comment");
        }

        let Some(model_id) = session.model_id.clone() else {
            tracing::warn!(
                task_id = %task_id,
                "no model_id in session closure; cannot resume after verification failure"
            );
            return;
        };

        let msg = SupervisorMessage::ResumeSession {
            task_id: task_id.to_owned(),
            model_id,
            goose_session_id: session.goose_session_id.clone(),
            worktree_path: worktree_path.to_owned(),
            resume_prompt: feedback.to_owned(),
            tokens_in,
            old_record_id: session.record_id.clone(),
        };
        if let Err(e) = self.sender.send(msg).await {
            tracing::warn!(task_id = %task_id, error = %e, "failed to queue resume session after verification failure");
        }
    }

    pub(super) async fn success_transition(
        &self,
        task_id: &str,
        agent_type: AgentType,
        output: &ParsedAgentOutput,
    ) -> Option<(TransitionAction, Option<String>)> {
        match agent_type {
            AgentType::Worker | AgentType::ConflictResolver => match output.worker_signal {
                Some(WorkerSignal::Done) => Some((TransitionAction::SubmitTaskReview, None)),
                None => {
                    let reason = output.runtime_error.clone().unwrap_or_else(|| {
                        "worker session completed without DONE marker".to_string()
                    });
                    tracing::warn!(reason = %reason, "worker session completed without structured result marker");
                    Some((TransitionAction::Release, Some(reason)))
                }
            },
            AgentType::TaskReviewer => match output.reviewer_verdict {
                Some(ReviewerVerdict::Verified) => self.merge_after_task_review(task_id).await,
                Some(ReviewerVerdict::Reopen) => Some((
                    TransitionAction::TaskReviewReject,
                    Some(
                        output
                            .reviewer_feedback
                            .clone()
                            .unwrap_or_else(|| "reviewer requested REOPEN".to_string()),
                    ),
                )),
                None => {
                    tracing::warn!("task reviewer session completed without REVIEW_RESULT marker");
                    Some((
                        TransitionAction::ReleaseTaskReview,
                        Some("reviewer session completed without REVIEW_RESULT marker".to_string()),
                    ))
                }
            },
            AgentType::EpicReviewer => match output.epic_verdict {
                Some(EpicReviewVerdict::Clean) => None,
                Some(EpicReviewVerdict::IssuesFound) => None,
                None => {
                    tracing::warn!(
                        "epic reviewer session completed without EPIC_REVIEW_RESULT marker"
                    );
                    None
                }
            },
        }
    }
}
