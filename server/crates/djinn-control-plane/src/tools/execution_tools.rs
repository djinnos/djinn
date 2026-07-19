// MCP tools for task-scoped execution control.
//
// Global execution toggles (start/pause/resume/status) were removed in the
// K8s-mode cut-over: the coordinator is always active and dispatches tasks
// unconditionally. The remaining tools operate on individual tasks:
//   - `execution_kill_task`: interrupt the agent session for one task.
//   - `session_for_task`: resolve the session + workspace for a task.

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;
use djinn_db::{LivenessEvidenceSnapshot, LivenessRepository, SessionRepository, TaskRepository};

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ExecutionKillTaskParams {
    /// Task ID to interrupt.
    pub task_id: String,
    /// Project path (accepted for API compatibility, currently unused).
    pub project: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SessionForTaskParams {
    /// Task ID to query.
    pub task_id: String,
    /// Absolute project path.
    pub project: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct ExecutionKillTaskResponse {
    pub ok: bool,
    pub task_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SessionForTaskResponse {
    pub ok: bool,
    pub task_id: String,
    pub model_id: Option<String>,
    pub session_id: Option<String>,
    #[schemars(with = "Option<i64>")]
    pub duration_seconds: Option<u64>,
    /// Workspace path resolved from the session's attached `task_run`.
    pub workspace_path: Option<String>,
    pub session: Option<String>,
    pub error: Option<String>,
}

#[tool_router(router = execution_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Kill the active agent session for a task.
    #[tool(
        description = "Kill the active agent session for a task. Aborts the session, commits WIP, releases worktree and session slot. Returns ok:true only after confirming no active pool session remains."
    )]
    pub async fn execution_kill_task(
        &self,
        Parameters(p): Parameters<ExecutionKillTaskParams>,
    ) -> Json<ExecutionKillTaskResponse> {
        // ── Liveness pre-checks: terminal task or no active session → kill_noop ──
        if let Some(path) = &p.project
            && self.project_id_for_path(path).await.is_none()
        {
            return Json(ExecutionKillTaskResponse {
                ok: false,
                task_id: None,
                error: Some(format!("project not found: {path}")),
            });
        }
        let Some(pool) = self.state.pool().await else {
            return Json(ExecutionKillTaskResponse {
                ok: false,
                task_id: None,
                error: Some("slot pool actor not initialized".to_string()),
            });
        };

        // Check if the task is already terminal — killing a terminal task is a
        // no-op that should record kill_noop evidence rather than claiming success.
        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
        let session_repo = SessionRepository::new(self.state.db().clone(), self.state.event_bus());
        let liveness_repo = LivenessRepository::new(self.state.db().clone());

        let task = task_repo.get(&p.task_id).await.ok().flatten();
        let task_is_terminal = task
            .as_ref()
            .is_some_and(|t| t.status == "closed" || t.status == "force_closed");

        if task_is_terminal {
            // Task is already terminal — no live work to free. Record kill_noop evidence.
            // We need a valid session_id for the FK constraint. Try active first,
            // then fall back to any session for the task (including terminal ones).
            let active_session = session_repo
                .active_for_task(&p.task_id)
                .await
                .ok()
                .flatten();
            let any_session = if active_session.is_some() {
                None // don't query again if we already have one
            } else {
                session_repo
                    .list_for_task(&p.task_id)
                    .await
                    .ok()
                    .and_then(|sessions| sessions.into_iter().next())
            };
            let evidence_session = active_session.as_ref().or(any_session.as_ref());
            if let Some(session) = evidence_session {
                let snapshot = LivenessEvidenceSnapshot {
                    session_id: session.id.clone(),
                    task_id: Some(p.task_id.clone()),
                    task_run_id: session.task_run_id.clone(),
                    verdict: "live".to_owned(), // verdict is moot for terminal tasks
                    outcome_kind: Some("kill_noop".to_owned()),
                    outcome_reason: None,
                    evidence: serde_json::json!({
                        "reason": "task_already_terminal",
                        "task_status": task.as_ref().map(|t| t.status.as_str()).unwrap_or("unknown"),
                        "kill_source": "execution_kill_task",
                    }),
                };
                let _ = liveness_repo.persist_evidence(&snapshot).await;
            }
            return Json(ExecutionKillTaskResponse {
                ok: false,
                task_id: Some(p.task_id),
                error: Some("task is already terminal; no session to kill (kill_noop)".to_string()),
            });
        }

        // Check if there is a DB-active session to kill. If no session exists,
        // the kill would be a no-op — record kill_noop evidence.
        // Pool errors are treated as "assume session exists" (fail-open) so
        // we still attempt the kill rather than silently skipping it.
        let active_session = session_repo
            .active_for_task(&p.task_id)
            .await
            .ok()
            .flatten();
        let has_pool_session = pool.has_session(&p.task_id).await.unwrap_or(true);
        if active_session.is_none() && !has_pool_session {
            // No live work to free — try to find any session for the task
            // to satisfy the liveness_evidence FK constraint on session_id.
            let any_session = session_repo
                .list_for_task(&p.task_id)
                .await
                .ok()
                .and_then(|sessions| sessions.into_iter().next());
            if let Some(session) = any_session.as_ref() {
                let snapshot = LivenessEvidenceSnapshot {
                    session_id: session.id.clone(),
                    task_id: Some(p.task_id.clone()),
                    task_run_id: session.task_run_id.clone(),
                    verdict: "live".to_owned(),
                    outcome_kind: Some("kill_noop".to_owned()),
                    outcome_reason: None,
                    evidence: serde_json::json!({
                        "reason": "no_active_session",
                        "kill_source": "execution_kill_task",
                    }),
                };
                let _ = liveness_repo.persist_evidence(&snapshot).await;
            }
            return Json(ExecutionKillTaskResponse {
                ok: false,
                task_id: Some(p.task_id),
                error: Some("no active session to kill (kill_noop)".to_string()),
            });
        }

        if let Err(e) = pool.terminate_session(&p.task_id).await {
            return Json(ExecutionKillTaskResponse {
                ok: false,
                task_id: Some(p.task_id),
                error: Some(e.to_string()),
            });
        }

        match pool.has_session(&p.task_id).await {
            Ok(false) => {
                // Terminate succeeded and session is gone — work was freed.
                // Record dead_reclaimed evidence with the session/task_run that
                // was reclaimed.
                let snapshot = LivenessEvidenceSnapshot {
                    session_id: active_session
                        .as_ref()
                        .map(|s| s.id.clone())
                        .unwrap_or_default(),
                    task_id: Some(p.task_id.clone()),
                    task_run_id: active_session.as_ref().and_then(|s| s.task_run_id.clone()),
                    verdict: "dead".to_owned(),
                    outcome_kind: Some("dead_reclaimed".to_owned()),
                    outcome_reason: None,
                    evidence: serde_json::json!({
                        "reason": "explicit_kill",
                        "kill_source": "execution_kill_task",
                        "had_pool_session": has_pool_session,
                    }),
                };
                if let Err(e) = liveness_repo.persist_evidence(&snapshot).await {
                    tracing::warn!(
                        task_id = %p.task_id,
                        error = %e,
                        "execution_kill_task: failed to persist dead_reclaimed evidence"
                    );
                }
            }
            Ok(true) => {
                return Json(ExecutionKillTaskResponse {
                    ok: false,
                    task_id: Some(p.task_id),
                    error: Some(
                        "termination confirmation failed: task still has an active slot pool session"
                            .to_string(),
                    ),
                });
            }
            Err(e) => {
                return Json(ExecutionKillTaskResponse {
                    ok: false,
                    task_id: Some(p.task_id),
                    error: Some(format!(
                        "termination confirmation failed while checking active session: {e}"
                    )),
                });
            }
        }

        Json(ExecutionKillTaskResponse {
            ok: true,
            task_id: Some(p.task_id),
            error: None,
        })
    }

    /// Get the session ID and worktree path for a running task.
    #[tool(description = "Get the session ID and worktree path for a running task")]
    pub async fn session_for_task(
        &self,
        Parameters(p): Parameters<SessionForTaskParams>,
    ) -> Json<SessionForTaskResponse> {
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(SessionForTaskResponse {
                    ok: false,
                    task_id: p.task_id,
                    model_id: None,
                    session_id: None,
                    duration_seconds: None,
                    workspace_path: None,
                    session: None,
                    error: Some(e),
                });
            }
        };
        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(task) = task_repo
            .resolve_in_project(&project_id, &p.task_id)
            .await
            .ok()
            .flatten()
        else {
            let missing_task_id = p.task_id.clone();
            return Json(SessionForTaskResponse {
                ok: false,
                task_id: missing_task_id.clone(),
                model_id: None,
                session_id: None,
                duration_seconds: None,
                workspace_path: None,
                session: None,
                error: Some(format!("task not found: {}", missing_task_id)),
            });
        };
        let Some(pool) = self.state.pool().await else {
            return Json(SessionForTaskResponse {
                ok: false,
                task_id: task.id,
                model_id: None,
                session_id: None,
                duration_seconds: None,
                workspace_path: None,
                session: None,
                error: Some("slot pool actor not initialized".to_string()),
            });
        };

        let running = match pool.session_for_task(&task.id).await {
            Ok(session) => session,
            Err(e) => {
                return Json(SessionForTaskResponse {
                    ok: false,
                    task_id: task.id,
                    model_id: None,
                    session_id: None,
                    duration_seconds: None,
                    workspace_path: None,
                    session: None,
                    error: Some(e.to_string()),
                });
            }
        };

        let session_repo = SessionRepository::new(self.state.db().clone(), self.state.event_bus());
        let db_session = session_repo.active_for_task(&task.id).await.ok().flatten();
        let task_run_repo =
            djinn_db::repositories::task_run::TaskRunRepository::new(self.state.db().clone());
        let workspace_path = match db_session.as_ref().and_then(|s| s.task_run_id.as_deref()) {
            Some(run_id) => task_run_repo
                .get(run_id)
                .await
                .ok()
                .flatten()
                .and_then(|run| run.workspace_path),
            None => None,
        };

        match running {
            Some(session) => Json(SessionForTaskResponse {
                ok: true,
                task_id: task.id,
                model_id: Some(session.model_id),
                session_id: Some(
                    db_session
                        .as_ref()
                        .map(|s| s.id.clone())
                        .unwrap_or_else(|| format!("slot-{}", session.slot_id)),
                ),
                duration_seconds: Some(session.duration_seconds),
                workspace_path,
                session: None,
                error: None,
            }),
            None => Json(SessionForTaskResponse {
                ok: true,
                task_id: task.id,
                model_id: None,
                session_id: None,
                duration_seconds: None,
                workspace_path: None,
                session: None,
                error: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use djinn_db::{EffectiveCreatorProvenance, UserRepository};
    use rmcp::handler::server::wrapper::Parameters;

    use super::*;
    use crate::bridge::{PoolStatus, RunningTaskInfo, SlotPoolOps};
    use crate::state::McpState;

    // Kept distinct from the user IDs used by other control-plane fixtures.
    const EXECUTION_KILL_LIVE_FIXTURE_GITHUB_ID: i64 = 999_991;
    const EXECUTION_KILL_TERMINAL_FIXTURE_GITHUB_ID: i64 = 999_990;

    struct RecordingSlotPool {
        killed: Mutex<Vec<String>>,
        terminated: Mutex<Vec<String>>,
        confirmations: Mutex<Vec<String>>,
        terminate_result: Mutex<Result<(), String>>,
        has_session_result: Mutex<Result<bool, String>>,
    }

    impl Default for RecordingSlotPool {
        fn default() -> Self {
            Self {
                killed: Mutex::new(Vec::new()),
                terminated: Mutex::new(Vec::new()),
                confirmations: Mutex::new(Vec::new()),
                terminate_result: Mutex::new(Ok(())),
                has_session_result: Mutex::new(Ok(false)),
            }
        }
    }

    impl RecordingSlotPool {
        fn terminated(&self) -> Vec<String> {
            self.terminated
                .lock()
                .expect("recording pool mutex")
                .clone()
        }

        fn with_has_session_result(result: Result<bool, String>) -> Self {
            Self {
                has_session_result: Mutex::new(result),
                ..Self::default()
            }
        }
    }

    #[async_trait]
    impl SlotPoolOps for RecordingSlotPool {
        async fn get_status(&self) -> Result<PoolStatus, String> {
            Ok(PoolStatus {
                active_slots: 0,
                total_slots: 0,
                per_model: Default::default(),
                running_tasks: Vec::new(),
            })
        }

        async fn kill_session(&self, task_id: &str) -> Result<(), String> {
            self.killed
                .lock()
                .expect("recording pool mutex")
                .push(task_id.to_string());
            Ok(())
        }

        async fn terminate_session(&self, task_id: &str) -> Result<(), String> {
            self.terminated
                .lock()
                .expect("recording pool mutex")
                .push(task_id.to_string());
            self.terminate_result
                .lock()
                .expect("recording pool mutex")
                .clone()
        }

        async fn session_for_task(&self, _: &str) -> Result<Option<RunningTaskInfo>, String> {
            Ok(None)
        }

        async fn has_session(&self, task_id: &str) -> Result<bool, String> {
            self.confirmations
                .lock()
                .expect("recording pool mutex")
                .push(task_id.to_string());
            self.has_session_result
                .lock()
                .expect("recording pool mutex")
                .clone()
        }
    }

    fn server_with_pool(pool: Arc<RecordingSlotPool>) -> DjinnMcpServer {
        let state = McpState::new(
            djinn_db::Database::open_in_memory().expect("open in-memory test database"),
            djinn_core::events::EventBus::noop(),
            djinn_provider::catalog::CatalogService::new(),
            djinn_provider::catalog::HealthTracker::new(),
            None,
            Some(pool.clone()),
            None,
            None,
            Arc::new(crate::state::stubs::StubLspOps),
            Arc::new(crate::state::stubs::StubRuntimeOps),
            Arc::new(crate::state::stubs::StubGitOps),
            Arc::new(crate::state::stubs::StubRepoGraphOps),
        );
        DjinnMcpServer::new(state)
    }

    #[tokio::test]
    async fn execution_kill_task_returns_kill_noop_when_no_active_session() {
        let pool = Arc::new(RecordingSlotPool::default());
        let server = server_with_pool(pool.clone());

        let Json(response) = server
            .execution_kill_task(Parameters(ExecutionKillTaskParams {
                task_id: "task-to-kill".to_string(),
                project: None,
            }))
            .await;

        // No task or session exists in the DB, and pool has no session —
        // kill is a no-op because there is no live work to free.
        assert!(!response.ok);
        assert_eq!(response.task_id.as_deref(), Some("task-to-kill"));
        assert!(response.error.as_deref().unwrap().contains("kill_noop"));
        // Terminate should NOT have been called — nothing to kill.
        assert_eq!(pool.terminated(), Vec::<String>::new());
    }

    #[tokio::test]
    async fn execution_kill_task_returns_kill_noop_when_pool_reports_active_but_terminate_errors() {
        // Pool has a session (so we skip the no-session no-op), but terminate
        // errors. This tests the existing error path still works.
        let pool = Arc::new(RecordingSlotPool {
            has_session_result: Mutex::new(Ok(true)),
            terminate_result: Mutex::new(Err("operator termination failed".to_string())),
            ..Default::default()
        });
        let server = server_with_pool(pool.clone());

        let Json(response) = server
            .execution_kill_task(Parameters(ExecutionKillTaskParams {
                task_id: "task-to-kill".to_string(),
                project: None,
            }))
            .await;

        assert!(!response.ok);
        assert_eq!(response.task_id.as_deref(), Some("task-to-kill"));
        assert_eq!(
            response.error.as_deref(),
            Some("operator termination failed")
        );
        assert_eq!(pool.terminated(), vec!["task-to-kill".to_string()]);
    }

    #[tokio::test]
    async fn execution_kill_task_returns_error_when_session_stuck_after_terminate() {
        // Pool has a session before terminate, and still has it after terminate.
        let pool = Arc::new(RecordingSlotPool::with_has_session_result(Ok(true)));
        let server = server_with_pool(pool.clone());

        let Json(response) = server
            .execution_kill_task(Parameters(ExecutionKillTaskParams {
                task_id: "task-to-kill".to_string(),
                project: None,
            }))
            .await;

        // Pool reports session before pre-check → reaches terminate →
        // has_session still true → error.
        assert!(!response.ok);
        assert_eq!(response.task_id.as_deref(), Some("task-to-kill"));
        // The pre-check finds has_pool_session=true → proceeds to terminate.
        // After terminate, has_session still returns true → stuck.
        assert_eq!(pool.terminated(), vec!["task-to-kill".to_string()]);
    }

    #[tokio::test]
    async fn execution_kill_task_returns_error_when_confirmation_errors() {
        // Pool has a session (passes pre-check), but the post-terminate
        // confirmation errors.
        let pool = Arc::new(RecordingSlotPool {
            // Pre-check: pool reports session present → skip noop
            // Post-terminate check: errors
            has_session_result: Mutex::new(Err("bridge unavailable".to_string())),
            ..Default::default()
        });
        let server = server_with_pool(pool.clone());

        let Json(response) = server
            .execution_kill_task(Parameters(ExecutionKillTaskParams {
                task_id: "task-to-kill".to_string(),
                project: None,
            }))
            .await;

        assert!(!response.ok);
        assert_eq!(response.task_id.as_deref(), Some("task-to-kill"));
        assert!(
            response
                .error
                .as_deref()
                .unwrap()
                .contains("bridge unavailable")
        );
        assert_eq!(pool.terminated(), vec!["task-to-kill".to_string()]);
    }

    #[tokio::test]
    async fn execution_kill_task_persists_dead_reclaimed_evidence_on_successful_kill() {
        // Seed a project + task + active session in the DB so the kill has
        // live work to free. Use a pool that reports session present before
        // terminate and absent after (via two-call recording).
        let db = djinn_db::Database::open_in_memory().expect("open in-memory test database");
        let events = djinn_core::events::EventBus::noop();

        // Create a project.
        let project_repo = djinn_db::ProjectRepository::new(db.clone(), events.clone());
        let project = project_repo
            .create("test-proj", "test-owner", "test-repo")
            .await
            .expect("create project");
        let user = UserRepository::new(db.clone())
            .upsert_from_github(
                EXECUTION_KILL_LIVE_FIXTURE_GITHUB_ID,
                "execution-kill-live-fixture",
                None,
                None,
            )
            .await
            .expect("create fixture user");

        // Create an in-progress task.
        let task_repo = djinn_db::TaskRepository::new(db.clone(), events.clone());
        let task = task_repo
            .create_in_project_with_provenance(
                &project.id,
                None,
                EffectiveCreatorProvenance {
                    explicit_user_id: Some(&user.id),
                    source_task_id: None,
                    proposal_id: None,
                },
                "Kill test task",
                "desc",
                "",
                "task",
                0,
                "",
                Some("in_progress"),
                None,
            )
            .await
            .expect("create task");

        // Create a running session linked to the task.
        let session_repo = djinn_db::SessionRepository::new(db.clone(), events.clone());
        let session = session_repo
            .create(djinn_db::CreateSessionParams {
                project_id: &project.id,
                task_id: Some(&task.id),
                model: "test/model",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .expect("create session");

        // Verify the session is active.
        let active = session_repo
            .active_for_task(&task.id)
            .await
            .expect("active_for_task");
        assert!(active.is_some(), "session should be active after creation");

        // Pool reports session present (passes pre-check) and absent after terminate.
        let pool = Arc::new(RecordingSlotPool::with_has_session_result(Ok(false)));
        let state = McpState::new(
            db.clone(),
            events.clone(),
            djinn_provider::catalog::CatalogService::new(),
            djinn_provider::catalog::HealthTracker::new(),
            None,
            Some(pool.clone()),
            None,
            None,
            Arc::new(crate::state::stubs::StubLspOps),
            Arc::new(crate::state::stubs::StubRuntimeOps),
            Arc::new(crate::state::stubs::StubGitOps),
            Arc::new(crate::state::stubs::StubRepoGraphOps),
        );
        let server = DjinnMcpServer::new(state);

        let Json(response) = server
            .execution_kill_task(Parameters(ExecutionKillTaskParams {
                task_id: task.id.clone(),
                project: None,
            }))
            .await;

        // Should succeed: session existed and terminate freed it.
        assert!(
            response.ok,
            "expected ok:true, got error: {:?}",
            response.error
        );
        assert_eq!(response.task_id.as_deref(), Some(task.id.as_str()));
        assert_eq!(response.error, None);

        // Verify dead_reclaimed evidence was persisted on the session.
        let liveness_repo = djinn_db::LivenessRepository::new(db.clone());
        let (verdict, outcome_kind) = liveness_repo
            .get_session_liveness_fields(&session.id)
            .await
            .expect("get session liveness fields");
        assert_eq!(verdict.as_deref(), Some("dead"));
        assert_eq!(outcome_kind.as_deref(), Some("dead_reclaimed"));

        // Verify evidence rows exist.
        let count = liveness_repo
            .count_evidence_for_session(&session.id, Some("dead_reclaimed"))
            .await
            .expect("count evidence");
        assert!(
            count >= 1,
            "expected at least 1 dead_reclaimed evidence row, got {count}"
        );
    }

    #[tokio::test]
    async fn execution_kill_task_persists_kill_noop_evidence_for_terminal_task() {
        let db = djinn_db::Database::open_in_memory().expect("open in-memory test database");
        let events = djinn_core::events::EventBus::noop();

        // Create a project + closed task.
        let project_repo = djinn_db::ProjectRepository::new(db.clone(), events.clone());
        let project = project_repo
            .create("test-proj-noop", "test-owner", "test-repo")
            .await
            .expect("create project");
        let user = UserRepository::new(db.clone())
            .upsert_from_github(
                EXECUTION_KILL_TERMINAL_FIXTURE_GITHUB_ID,
                "execution-kill-terminal-fixture",
                None,
                None,
            )
            .await
            .expect("create fixture user");
        let task_repo = djinn_db::TaskRepository::new(db.clone(), events.clone());
        let task = task_repo
            .create_in_project_with_provenance(
                &project.id,
                None,
                EffectiveCreatorProvenance {
                    explicit_user_id: Some(&user.id),
                    source_task_id: None,
                    proposal_id: None,
                },
                "Terminal kill noop task",
                "desc",
                "",
                "task",
                0,
                "",
                Some("closed"),
                None,
            )
            .await
            .expect("create task");

        // Create a (now-terminal) session linked to the task so evidence can
        // reference a valid session_id and satisfy the FK constraint.
        let session_repo = djinn_db::SessionRepository::new(db.clone(), events.clone());
        let session = session_repo
            .create(djinn_db::CreateSessionParams {
                project_id: &project.id,
                task_id: Some(&task.id),
                model: "test/model",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .expect("create session");

        let pool = Arc::new(RecordingSlotPool::default());
        let state = McpState::new(
            db.clone(),
            events.clone(),
            djinn_provider::catalog::CatalogService::new(),
            djinn_provider::catalog::HealthTracker::new(),
            None,
            Some(pool.clone()),
            None,
            None,
            Arc::new(crate::state::stubs::StubLspOps),
            Arc::new(crate::state::stubs::StubRuntimeOps),
            Arc::new(crate::state::stubs::StubGitOps),
            Arc::new(crate::state::stubs::StubRepoGraphOps),
        );
        let server = DjinnMcpServer::new(state);

        let Json(response) = server
            .execution_kill_task(Parameters(ExecutionKillTaskParams {
                task_id: task.id.clone(),
                project: None,
            }))
            .await;

        // Should be a no-op: task is already terminal.
        assert!(!response.ok);
        assert!(response.error.as_deref().unwrap().contains("kill_noop"));
        // Terminate should NOT have been called.
        assert_eq!(pool.terminated(), Vec::<String>::new());

        // Verify kill_noop evidence was persisted on the session.
        let liveness_repo = djinn_db::LivenessRepository::new(db.clone());
        let count = liveness_repo
            .count_evidence_for_session(&session.id, Some("kill_noop"))
            .await
            .expect("count evidence");
        assert!(
            count >= 1,
            "expected at least 1 kill_noop evidence row, got {count}"
        );
    }
}
