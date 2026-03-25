use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::context::AgentContext;
use crate::roles::role_for_task_dispatch;

use super::helpers::conflict_context_for_dispatch;
use super::{SlotCommand, SlotError, SlotEvent, run_task_lifecycle};

type LifecycleFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'static>>;
type LifecycleRunner = Arc<
    dyn Fn(
            String,
            String,
            String,
            AgentContext,
            CancellationToken,
            CancellationToken,
        ) -> LifecycleFuture
        + Send
        + Sync,
>;

#[cfg(test)]
pub(crate) type TestLifecycleRunner = LifecycleRunner;

struct ActiveLifecycle {
    task_id: String,
    join: tokio::task::JoinHandle<anyhow::Result<()>>,
    kill: CancellationToken,
    pause: CancellationToken,
    killed: bool,
}

pub struct SlotActor {
    id: usize,
    model_id: String,
    receiver: mpsc::Receiver<SlotCommand>,
    event_tx: mpsc::Sender<SlotEvent>,
    app_state: AgentContext,
    cancel: CancellationToken,
    runner: LifecycleRunner,
}

impl SlotActor {
    pub async fn run(mut self) {
        let mut active: Option<ActiveLifecycle> = None;
        let mut drain_requested = false;

        loop {
            if let Some(mut running) = active.take() {
                tokio::select! {
                    _ = self.cancel.cancelled() => {
                        running.kill.cancel();
                        let _ = running.join.await;
                        break;
                    }
                    join_result = &mut running.join => {
                        if let Err(e) = join_result {
                            tracing::warn!(slot_id = self.id, model_id = %self.model_id, error = %e, "slot lifecycle join failed");
                        }
                        self.emit_completion_event(&running).await;
                        if drain_requested {
                            break;
                        }
                    }
                    cmd = self.receiver.recv() => {
                        match cmd {
                            Some(SlotCommand::RunTask { respond_to, .. }) => {
                                let _ = respond_to.send(Err(SlotError::SlotBusy));
                                active = Some(running);
                            }
                            Some(SlotCommand::Kill) => {
                                running.killed = true;
                                running.kill.cancel();
                                active = Some(running);
                            }
                            Some(SlotCommand::Pause) => {
                                running.pause.cancel();
                                active = Some(running);
                            }
                            Some(SlotCommand::Drain) => {
                                drain_requested = true;
                                active = Some(running);
                            }
                            None => {
                                running.kill.cancel();
                                let _ = running.join.await;
                                break;
                            }
                        }
                    }
                }
            } else {
                tokio::select! {
                    _ = self.cancel.cancelled() => {
                        break;
                    }
                    cmd = self.receiver.recv() => {
                        match cmd {
                            Some(SlotCommand::RunTask { task_id, project_path, respond_to }) => {
                                let kill = CancellationToken::new();
                                let pause = CancellationToken::new();
                                let run = (self.runner)(
                                    task_id.clone(),
                                    project_path,
                                    self.model_id.clone(),
                                    self.app_state.clone(),
                                    kill.clone(),
                                    pause.clone(),
                                );

                                let join = tokio::spawn(run);
                                let _ = respond_to.send(Ok(()));
                                active = Some(ActiveLifecycle {
                                    task_id,
                                    join,
                                    kill,
                                    pause,
                                    killed: false,
                                });
                            }
                            Some(SlotCommand::Kill) | Some(SlotCommand::Pause) => {
                                // No active lifecycle; command is a no-op.
                            }
                            Some(SlotCommand::Drain) | None => {
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    async fn emit_completion_event(&self, running: &ActiveLifecycle) {
        let event = if running.killed {
            SlotEvent::Killed {
                slot_id: self.id,
                model_id: self.model_id.clone(),
                task_id: running.task_id.clone(),
            }
        } else {
            SlotEvent::Free {
                slot_id: self.id,
                model_id: self.model_id.clone(),
                task_id: running.task_id.clone(),
            }
        };
        let _ = self.event_tx.send(event).await;
    }
}

#[derive(Debug, Clone)]
pub struct SlotHandle {
    id: usize,
    model_id: String,
    sender: mpsc::Sender<SlotCommand>,
}

impl SlotHandle {
    pub fn spawn(
        id: usize,
        model_id: String,
        event_tx: mpsc::Sender<SlotEvent>,
        app_state: AgentContext,
        cancel: CancellationToken,
    ) -> Self {
        let runner: LifecycleRunner = Arc::new(
            |task_id, project_path, model_id, app_state, kill, pause| {
                Box::pin(async move {
                    let (sink, _rx) = mpsc::channel::<SlotEvent>(1);
                    // Resolve role before entering the lifecycle so the lifecycle
                    // function receives a concrete `Arc<dyn AgentRole>` rather than
                    // performing the lookup internally.
                    let conflict_ctx = conflict_context_for_dispatch(&task_id, &app_state).await;
                    let task = {
                        use djinn_db::TaskRepository;
                        let repo =
                            TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
                        repo.get(&task_id).await.ok().flatten()
                    };
                    let role = match task {
                        Some(ref t) => role_for_task_dispatch(t, conflict_ctx.is_some()),
                        None => {
                            // Task not found — lifecycle will handle this gracefully.
                            crate::roles::role_impl_for(crate::AgentType::Worker)
                        }
                    };

                    // Look up the default DB AgentRole for this task's project + base_role.
                    // Provides system_prompt_extensions, learned_prompt, mcp_servers, skills,
                    // and verification_command overrides from the configurable role system.
                    let (
                        system_prompt_extensions,
                        learned_prompt,
                        mcp_servers,
                        skills,
                        role_verification_command,
                    ) = if let Some(ref t) = task {
                        use djinn_db::AgentRepository;
                        let role_repo =
                            AgentRepository::new(app_state.db.clone(), app_state.event_bus.clone());
                        let base_role_name = role.config().name;
                        match role_repo
                            .get_default_for_base_role(&t.project_id, base_role_name)
                            .await
                        {
                            Ok(Some(db_role)) => {
                                let mcp_servers =
                                    djinn_core::models::parse_json_array(&db_role.mcp_servers);
                                let skills = djinn_core::models::parse_json_array(&db_role.skills);
                                tracing::debug!(
                                    task_id = %t.short_id,
                                    role_name = %db_role.name,
                                    base_role = %base_role_name,
                                    has_extensions = !db_role.system_prompt_extensions.trim().is_empty(),
                                    has_learned_prompt = db_role.learned_prompt.is_some(),
                                    mcp_server_count = mcp_servers.len(),
                                    skill_count = skills.len(),
                                    has_model_preference = db_role.model_preference.is_some(),
                                    has_verification_command = db_role.verification_command.is_some(),
                                    "Lifecycle: resolved default DB role config"
                                );
                                (
                                    db_role.system_prompt_extensions,
                                    db_role.learned_prompt,
                                    mcp_servers,
                                    skills,
                                    db_role.verification_command,
                                )
                            }
                            Ok(None) => {
                                tracing::debug!(
                                    task_id = %t.short_id,
                                    base_role = %base_role_name,
                                    project_id = %t.project_id,
                                    "Lifecycle: no default DB role configured; using base role defaults"
                                );
                                (String::new(), None, Vec::new(), Vec::new(), None)
                            }
                            Err(e) => {
                                tracing::warn!(
                                    task_id = %t.short_id,
                                    base_role = %base_role_name,
                                    error = %e,
                                    "Lifecycle: failed to load default DB role; using base role defaults"
                                );
                                (String::new(), None, Vec::new(), Vec::new(), None)
                            }
                        }
                    } else {
                        (String::new(), None, Vec::new(), Vec::new(), None)
                    };

                    run_task_lifecycle(crate::actors::slot::lifecycle::TaskLifecycleParams {
                        task_id,
                        project_path,
                        model_id,
                        role,
                        app_state,
                        cancel: kill,
                        pause,
                        event_tx: sink,
                        system_prompt_extensions,
                        learned_prompt,
                        mcp_servers,
                        skills,
                        role_verification_command,
                        #[cfg(test)]
                        provider_override: None,
                    })
                    .await
                })
            },
        );
        Self::spawn_with_runner(id, model_id, event_tx, app_state, cancel, runner)
    }

    fn spawn_with_runner(
        id: usize,
        model_id: String,
        event_tx: mpsc::Sender<SlotEvent>,
        app_state: AgentContext,
        cancel: CancellationToken,
        runner: LifecycleRunner,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(16);
        let actor = SlotActor {
            id,
            model_id: model_id.clone(),
            receiver,
            event_tx,
            app_state,
            cancel,
            runner,
        };
        tokio::spawn(actor.run());
        Self {
            id,
            model_id,
            sender,
        }
    }

    #[cfg(test)]
    pub(crate) fn spawn_with_test_runner(
        id: usize,
        model_id: String,
        event_tx: mpsc::Sender<SlotEvent>,
        app_state: AgentContext,
        cancel: CancellationToken,
        runner: TestLifecycleRunner,
    ) -> Self {
        Self::spawn_with_runner(id, model_id, event_tx, app_state, cancel, runner)
    }

    pub fn id(&self) -> usize {
        self.id
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    pub async fn run_task(&self, task_id: String, project_path: String) -> Result<(), SlotError> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(SlotCommand::RunTask {
                task_id,
                project_path,
                respond_to: tx,
            })
            .await
            .map_err(|_| SlotError::SessionFailed("slot actor channel closed".to_string()))?;
        rx.await
            .map_err(|_| SlotError::SessionFailed("slot actor did not ack dispatch".to_string()))?
    }

    pub async fn kill(&self) -> Result<(), SlotError> {
        self.sender
            .send(SlotCommand::Kill)
            .await
            .map_err(|_| SlotError::SessionFailed("slot actor channel closed".to_string()))
    }

    pub async fn pause(&self) -> Result<(), SlotError> {
        self.sender
            .send(SlotCommand::Pause)
            .await
            .map_err(|_| SlotError::SessionFailed("slot actor channel closed".to_string()))
    }

    pub async fn drain(&self) -> Result<(), SlotError> {
        self.sender
            .send(SlotCommand::Drain)
            .await
            .map_err(|_| SlotError::SessionFailed("slot actor channel closed".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;
    use crate::test_helpers;

    fn test_app_state() -> (AgentContext, CancellationToken, TempDir) {
        let db = test_helpers::create_test_db();
        let cancel = CancellationToken::new();
        let temp = test_helpers::test_tempdir("djinn-slot-actor-");
        (
            test_helpers::agent_context_from_db(db, cancel.clone()),
            cancel,
            temp,
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_task_completes_and_emits_free_event() {
        let (app_state, cancel, _temp) = test_app_state();
        let (event_tx, mut event_rx) = mpsc::channel(4);

        let runner: LifecycleRunner = Arc::new(
            |_task_id, _project_path, _model_id, _app_state, _kill, _pause| {
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    Ok(())
                })
            },
        );

        let slot = SlotHandle::spawn_with_runner(
            7,
            "test/mock".to_string(),
            event_tx,
            app_state,
            cancel,
            runner,
        );

        slot.run_task("task-123".to_string(), "/tmp/project".to_string())
            .await
            .expect("dispatch should be accepted");

        let evt = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("event should arrive")
            .expect("event channel should stay open");

        match evt {
            SlotEvent::Free {
                slot_id,
                model_id,
                task_id,
            } => {
                assert_eq!(slot_id, 7);
                assert_eq!(model_id, "test/mock");
                assert_eq!(task_id, "task-123");
            }
            other => panic!("expected SlotEvent::Free, got {other:?}"),
        }
    }
}
