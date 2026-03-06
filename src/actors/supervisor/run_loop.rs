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
                evt = self.slot_event_rx.recv() => {
                    let Some(evt) = evt else { break; };
                    self.handle_slot_event(evt).await;
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
                    || self.lifecycle_handles.contains_key(&task_id)
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
                    active_sessions: self.sessions.len() + self.lifecycle_handles.len(),
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
                let session = session.or_else(|| {
                    self.lifecycle_handles
                        .get(&task_id)
                        .map(|h| RunningSessionInfo {
                            task_id: task_id.clone(),
                            model_id: h.model_id.clone(),
                            session_id: "lifecycle".to_string(),
                            duration_seconds: h.started_at.elapsed().as_secs(),
                            worktree_path: None,
                        })
                });
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
        }
    }

    pub(super) async fn handle_slot_event(&mut self, evt: SlotEvent) {
        match evt {
            SlotEvent::Free {
                model_id, task_id, ..
            }
            | SlotEvent::Killed {
                model_id, task_id, ..
            } => {
                let had_handle = self.lifecycle_handles.remove(&task_id).is_some();
                if had_handle {
                    self.decrement_capacity_for_model(Some(&model_id));
                }
                self.in_flight.remove(&task_id);
            }
        }
    }

    pub(super) async fn shutdown(&mut self) {
        self.interrupt_all_sessions("session interrupted by supervisor shutdown")
            .await;
    }
}
