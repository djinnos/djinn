use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::context::AgentContext;
use crate::roles::{role_for_task_dispatch, role_impl_for};

use super::helpers::conflict_context_for_dispatch;
use super::{
    ProjectLifecycleParams, SlotCommand, SlotError, SlotEvent, run_project_lifecycle,
    run_task_lifecycle,
};

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
                            Some(SlotCommand::RunProject { respond_to, .. }) => {
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
                            Some(SlotCommand::RunProject {
                                project_id,
                                project_path,
                                agent_type,
                                model_id,
                                respond_to,
                            }) => {
                                let kill = CancellationToken::new();
                                let pause = CancellationToken::new();
                                // Use a dummy sink — the real SlotEvent is emitted
                                // by SlotActor::emit_completion_event after the
                                // lifecycle future completes (same pattern as RunTask).
                                let (event_tx, _rx) = mpsc::channel::<SlotEvent>(1);
                                let app_state = self.app_state.clone();
                                let project_task_id = format!("project:{project_id}:{agent_type}");
                                let role = role_impl_for(agent_type.parse().unwrap_or(crate::AgentType::Planner));
                                let kill_for_task = kill.clone();
                                let pause_for_task = pause.clone();
                                let join = tokio::spawn(async move {
                                    run_project_lifecycle(ProjectLifecycleParams {
                                        project_id,
                                        project_path,
                                        role,
                                        model_id,
                                        app_state,
                                        cancel: kill_for_task,
                                        pause: pause_for_task,
                                        event_tx,
                                    })
                                    .await
                                });
                                let _ = respond_to.send(Ok(()));
                                active = Some(ActiveLifecycle {
                                    task_id: project_task_id,
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

    pub async fn run_project(
        &self,
        project_id: String,
        project_path: String,
        agent_type: String,
        model_id: String,
    ) -> Result<(), SlotError> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(SlotCommand::RunProject {
                project_id,
                project_path,
                agent_type,
                model_id,
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
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::Duration;

    use tempfile::TempDir;
    use tokio::process::Command;

    use super::*;
    use crate::AgentType;
    use crate::roles::role_impl_for;
    use crate::test_helpers;
    use djinn_db::{ProjectRepository, SessionRepository};

    fn test_app_state() -> (AgentContext, CancellationToken, TempDir) {
        let db = test_helpers::create_test_db();
        let cancel = CancellationToken::new();
        let temp = tempfile::tempdir().expect("tempdir");
        (
            test_helpers::agent_context_from_db(db, cancel.clone()),
            cancel,
            temp,
        )
    }

    async fn create_git_repo() -> TempDir {
        let tmp = tempfile::Builder::new()
            .prefix("djinn-slot-actor-project-")
            .tempdir_in("/tmp")
            .expect("tempdir");
        let p = tmp.path();

        let run = |args: &[&str]| {
            let p = p.to_path_buf();
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            async move {
                Command::new("git")
                    .args(&args)
                    .current_dir(&p)
                    .output()
                    .await
                    .expect("git command")
            }
        };

        run(&["init"]).await;
        run(&["config", "user.email", "test@djinn.test"]).await;
        run(&["config", "user.name", "Test"]).await;
        tokio::fs::write(p.join("README.md"), "# test")
            .await
            .unwrap();
        run(&["add", "README.md"]).await;
        run(&["commit", "-m", "init"]).await;
        run(&["branch", "-M", "main"]).await;

        tmp
    }

    async fn register_project(
        db: &djinn_db::Database,
        repo_path: &Path,
    ) -> djinn_core::models::Project {
        let repo = ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let id = uuid::Uuid::now_v7();
        let path = repo_path.to_str().unwrap().to_string();
        let name = format!("slot-actor-project-{id}");
        repo.create(&name, &path).await.expect("create project")
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_project_uses_synthetic_task_id_and_frees_slot_on_completion() {
        let repo = create_git_repo().await;
        let db = test_helpers::create_test_db();
        let cancel = CancellationToken::new();
        let app_state = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
        let project = register_project(&db, repo.path()).await;
        let (event_tx, mut event_rx) = mpsc::channel(4);

        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_clone = Arc::clone(&seen);
        let runner: LifecycleRunner = Arc::new(
            move |_task_id, _project_path, _model_id, _app_state, _kill, _pause| {
                let seen = Arc::clone(&seen_clone);
                Box::pin(async move {
                    seen.lock().unwrap().push("task-runner-called".to_string());
                    Ok(())
                })
            },
        );

        let slot = SlotHandle::spawn_with_runner(
            9,
            "test/mock".to_string(),
            event_tx,
            app_state,
            cancel,
            runner,
        );

        slot.run_project(
            project.id.clone(),
            project.path.clone(),
            "planner".to_string(),
            "invalid-model-id".to_string(),
        )
        .await
        .expect("project dispatch should be accepted");

        let evt = tokio::time::timeout(Duration::from_secs(3), event_rx.recv())
            .await
            .expect("event should arrive")
            .expect("event channel should stay open");

        match evt {
            SlotEvent::Free {
                slot_id,
                model_id,
                task_id,
            } => {
                assert_eq!(slot_id, 9);
                assert_eq!(model_id, "test/mock");
                assert_eq!(task_id, format!("project:{}:planner", project.id));
            }
            other => panic!("expected SlotEvent::Free for project session, got {other:?}"),
        }

        assert!(
            seen.lock().unwrap().is_empty(),
            "RunProject should bypass the task lifecycle runner and invoke project lifecycle directly"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn project_lifecycle_persists_null_task_id_for_project_scoped_sessions() {
        let repo = create_git_repo().await;
        let db = test_helpers::create_test_db();
        let cancel = CancellationToken::new();
        let app_state = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
        let project = register_project(&db, repo.path()).await;
        let (event_tx, mut event_rx) = mpsc::channel(4);

        let slot = SlotHandle::spawn_with_test_runner(
            10,
            "test/mock".to_string(),
            event_tx,
            app_state.clone(),
            cancel,
            Arc::new(|_, _, _, _, _, _| Box::pin(async { Ok(()) })),
        );

        let provider = Arc::new(test_helpers::FakeProvider::tool_call(
            "finalize-1",
            "submit_grooming",
            serde_json::json!({
                "summary": "Planning complete",
                "tasks_reviewed": []
            }),
        ));

        let role = role_impl_for(AgentType::Planner);
        let synthetic_task_id = format!("project:{}:{}", project.id, role.config().name);

        let session_repo = SessionRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let created = session_repo
            .create(djinn_db::CreateSessionParams {
                project_id: &project.id,
                task_id: None,
                model: "test/mock",
                agent_type: role.config().name,
                worktree_path: Some(&project.path),
                metadata_json: None,
            })
            .await
            .expect("create project-scoped session");
        session_repo
            .update(
                &created.id,
                djinn_core::models::SessionStatus::Completed,
                0,
                0,
            )
            .await
            .expect("complete project-scoped session");

        slot.run_project(
            project.id.clone(),
            project.path.clone(),
            "planner".to_string(),
            "invalid-model-id".to_string(),
        )
        .await
        .expect("project dispatch should be accepted");

        let evt = tokio::time::timeout(Duration::from_secs(3), event_rx.recv())
            .await
            .expect("event should arrive")
            .expect("event channel should stay open");
        assert!(matches!(evt, SlotEvent::Free { .. }));

        let by_synthetic = session_repo
            .list_for_task_in_project(&project.id, &synthetic_task_id)
            .await
            .expect("list by synthetic task id");
        assert!(
            by_synthetic.is_empty(),
            "project-scoped sessions must not be stored under synthetic task ids"
        );

        let rows = sqlx::query_as::<_, djinn_core::models::SessionRecord>(
            "SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at, status, tokens_in, tokens_out, worktree_path FROM sessions WHERE project_id = ?1 ORDER BY started_at ASC",
        )
        .bind(&project.id)
        .fetch_all(db.pool())
        .await
        .expect("fetch sessions");
        assert!(
            !rows.is_empty(),
            "expected at least one project-scoped session row"
        );
        assert!(
            rows.iter().all(|row| row.task_id.is_none()),
            "all project-scoped session rows should persist NULL task_id"
        );

        let _ = provider;
    }
}
