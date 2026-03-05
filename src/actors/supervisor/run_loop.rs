use super::*;

impl AgentSupervisor {
    pub(super) async fn run(mut self) {
        tracing::info!("AgentSupervisor started");
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    self.shutdown().await;
                    break;
                }
                msg = self.receiver.recv() => {
                    let Some(msg) = msg else { break; };
                    self.handle(msg).await;
                }
            }
        }
        tracing::info!("AgentSupervisor stopped");
    }

    pub(super) async fn handle(&mut self, msg: SupervisorMessage) {
        match msg {
            SupervisorMessage::Dispatch {
                task_id,
                project_path,
                model_id,
                respond_to,
            } => {
                let result = self.dispatch(task_id.clone(), project_path, model_id).await;
                if result.is_err() {
                    self.in_flight.remove(&task_id);
                }
                let _ = respond_to.send(result);
            }
            SupervisorMessage::HasSession {
                task_id,
                respond_to,
            } => {
                let active = self.sessions.contains_key(&task_id)
                    || self.compacting_tasks.contains(&task_id)
                    || self.in_flight.contains(&task_id);
                let _ = respond_to.send(Ok(active));
            }
            SupervisorMessage::KillSession {
                task_id,
                respond_to,
            } => {
                let _ = respond_to.send(self.kill_session(task_id).await);
            }
            SupervisorMessage::PauseSession {
                task_id,
                respond_to,
            } => {
                let _ = respond_to.send(self.pause_session(task_id).await);
            }
            SupervisorMessage::GetStatus { respond_to } => {
                let _ = respond_to.send(Ok(SupervisorStatus {
                    active_sessions: self.sessions.len(),
                    capacity: self.capacity.clone(),
                    running_sessions: self.running_sessions_snapshot(),
                }));
            }
            SupervisorMessage::GetSessionForTask {
                task_id,
                respond_to,
            } => {
                let session = self
                    .sessions
                    .get(&task_id)
                    .map(|handle| self.session_snapshot(&task_id, handle));
                let _ = respond_to.send(Ok(session));
            }
            SupervisorMessage::InterruptAll { reason, respond_to } => {
                self.interrupt_all_sessions(&reason).await;
                let _ = respond_to.send(Ok(()));
            }
            SupervisorMessage::InterruptProject {
                project_id,
                reason,
                respond_to,
            } => {
                self.interrupt_project_sessions(&project_id, &reason).await;
                let _ = respond_to.send(Ok(()));
            }
            SupervisorMessage::UpdateSessionLimits {
                max_sessions,
                default_max,
                respond_to,
            } => {
                self.apply_session_limits(max_sessions, default_max);
                let _ = respond_to.send(Ok(()));
            }
            SupervisorMessage::SessionCompleted {
                task_id,
                result,
                output,
            } => {
                if self.interrupted_sessions.remove(&task_id) {
                    tracing::info!(task_id = %task_id, "Supervisor: ignoring completion for interrupted session");
                    self.in_flight.remove(&task_id);
                    return;
                }
                tracing::info!(
                    task_id = %task_id,
                    result = if result.is_ok() { "ok" } else { "error" },
                    "Supervisor: session completion received"
                );

                // Detect context exhaustion and trigger a fresh continuation
                // instead of failing/releasing the task.
                if self.maybe_compact_on_context_exhaustion(&task_id, &result, &output).await {
                    return;
                }

                let session = self.remove_session(&task_id);
                self.handle_session_result(&task_id, session, result, output)
                    .await;
                // Remove in_flight AFTER all post-session work (verification, commit,
                // transition, cleanup) is done. If handle_session_result queued a
                // ResumeSession (verification failure path), this removal is safe:
                // no HasSession query can be processed between this remove and the
                // re-insert in dispatch_resume since the actor is still in its loop.
                self.in_flight.remove(&task_id);
            }
            SupervisorMessage::ResumeSession {
                task_id,
                model_id,
                goose_session_id,
                worktree_path,
                resume_prompt,
                tokens_in,
                old_record_id,
            } => {
                if let Err(e) = self
                    .dispatch_resume(
                        task_id.clone(),
                        model_id,
                        goose_session_id,
                        worktree_path,
                        resume_prompt,
                        tokens_in,
                        old_record_id,
                    )
                    .await
                {
                    tracing::warn!(error = %e, "Supervisor: failed to dispatch resume session after verification failure");
                    self.in_flight.remove(&task_id);
                }
            }
            SupervisorMessage::CompactionNeeded {
                task_id,
                old_goose_session_id,
                tokens_in,
                context_window,
            } => {
                self.handle_compaction_needed(
                    task_id,
                    old_goose_session_id,
                    tokens_in,
                    context_window,
                )
                .await;
            }
            SupervisorMessage::CompactionComplete {
                task_id,
                model_id,
                agent_type,
                project_id,
                new_goose_session_id,
                new_record_id,
                agent,
                worktree_path,
                summary,
                context_window,
            } => {
                self.handle_compaction_complete(
                    task_id,
                    model_id,
                    agent_type,
                    project_id,
                    new_goose_session_id,
                    new_record_id,
                    agent,
                    worktree_path,
                    summary,
                    context_window,
                )
                .await;
            }
            SupervisorMessage::CompactionAborted {
                task_id,
                model_id,
                agent_type,
                worktree_path,
            } => {
                self.handle_compaction_aborted(task_id, model_id, agent_type, worktree_path)
                    .await;
            }
        }
    }

    pub(super) async fn shutdown(&mut self) {
        self.interrupt_all_sessions("session interrupted by supervisor shutdown")
            .await;
    }
}
