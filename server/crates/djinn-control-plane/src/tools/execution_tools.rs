// MCP tools for task-scoped execution control.
//
// Global execution toggles (start/pause/resume/status) were removed in the
// K8s-mode cut-over: the coordinator is always active and dispatches tasks
// unconditionally. The remaining tools operate on individual tasks:
//   - `execution_kill_task`: interrupt the agent session for one task.
//   - `session_for_task`: resolve the session + workspace for a task.

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::bridge::{ReconcileTerminateExecution, ReconcileTerminateKind, ReconcileTerminateObservations, ReconcileTerminateSnapshot};
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

#[derive(Debug, Clone, Copy, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionKillTaskKind {
    GenuinelyAbsent,
    Terminated,
    DesyncReconciled,
    TeardownFailed,
    SettlementFailed,
    ReconciliationIncomplete,
    TaskNotFound,
    ProjectNotFound,
    PoolUnavailable,
    PoolError,
    AuditFailed,
}

impl From<ReconcileTerminateKind> for ExecutionKillTaskKind {
    fn from(kind: ReconcileTerminateKind) -> Self {
        match kind {
            ReconcileTerminateKind::GenuinelyAbsent => Self::GenuinelyAbsent,
            ReconcileTerminateKind::Terminated => Self::Terminated,
            ReconcileTerminateKind::DesyncReconciled => Self::DesyncReconciled,
            ReconcileTerminateKind::TeardownFailed => Self::TeardownFailed,
            ReconcileTerminateKind::SettlementFailed => Self::SettlementFailed,
            ReconcileTerminateKind::ReconciliationIncomplete => Self::ReconciliationIncomplete,
        }
    }
}

#[derive(Clone, Serialize, schemars::JsonSchema)]
pub struct ExecutionKillTaskExecution {
    pub session_id: String,
    pub task_run_id: Option<String>,
    pub teardown_owner: bool,
    pub teardown_attempted: bool,
    pub teardown_error: Option<String>,
    pub settlement_attempted: bool,
    pub settlement_error: Option<String>,
}

impl From<ReconcileTerminateExecution> for ExecutionKillTaskExecution {
    fn from(execution: ReconcileTerminateExecution) -> Self {
        Self {
            session_id: execution.session_id,
            task_run_id: execution.task_run_id,
            teardown_owner: execution.teardown_owner,
            teardown_attempted: execution.teardown_attempted,
            teardown_error: execution.teardown_error,
            settlement_attempted: execution.settlement_attempted,
            settlement_error: execution.settlement_error,
        }
    }
}

#[derive(Clone, Serialize, schemars::JsonSchema)]
pub struct ExecutionKillTaskObservations {
    pub initial_non_terminal_ids: Vec<String>,
    pub initial_mapping_slot_id: Option<usize>,
    pub initial_pending_teardown: bool,
    pub initial_compacting: bool,
    pub fenced_generation: Option<i64>,
    pub initial_capture_error: Option<String>,
    pub final_non_terminal_ids: Vec<String>,
    pub final_mapping_slot_id: Option<usize>,
    pub final_pending_teardown: bool,
    pub final_reread_error: Option<String>,
    pub pool_cleanup_error: Option<String>,
    pub completion_source: String,
    pub underlying_kind: Option<ExecutionKillTaskKind>,
}

impl From<ReconcileTerminateObservations> for ExecutionKillTaskObservations {
    fn from(observations: ReconcileTerminateObservations) -> Self {
        Self {
            initial_non_terminal_ids: observations.initial_non_terminal_ids,
            initial_mapping_slot_id: observations.initial_mapping_slot_id,
            initial_pending_teardown: observations.initial_pending_teardown,
            initial_compacting: observations.initial_compacting,
            fenced_generation: observations.fenced_generation,
            initial_capture_error: observations.initial_capture_error,
            final_non_terminal_ids: observations.final_non_terminal_ids,
            final_mapping_slot_id: observations.final_mapping_slot_id,
            final_pending_teardown: observations.final_pending_teardown,
            final_reread_error: observations.final_reread_error,
            pool_cleanup_error: observations.pool_cleanup_error,
            completion_source: observations.completion_source,
            underlying_kind: observations.underlying_kind.map(Into::into),
        }
    }
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct ExecutionKillTaskResponse {
    pub ok: bool,
    pub kind: ExecutionKillTaskKind,
    pub task_id: Option<String>,
    pub executions: Vec<ExecutionKillTaskExecution>,
    pub observations: Option<ExecutionKillTaskObservations>,
    pub underlying_kind: Option<ExecutionKillTaskKind>,
    pub error: Option<String>,
}

fn response_from_snapshot(snapshot: ReconcileTerminateSnapshot) -> ExecutionKillTaskResponse {
    ExecutionKillTaskResponse {
        ok: snapshot.ok,
        kind: snapshot.kind.into(),
        task_id: Some(snapshot.task_id),
        executions: snapshot.executions.into_iter().map(Into::into).collect(),
        observations: Some(snapshot.observations.into()),
        underlying_kind: None,
        error: None,
    }
}

fn unresolved_kill_response(
    kind: ExecutionKillTaskKind,
    task_id: Option<String>,
    error: impl Into<String>,
) -> Json<ExecutionKillTaskResponse> {
    Json(ExecutionKillTaskResponse {
        ok: false,
        kind,
        task_id,
        executions: Vec::new(),
        observations: None,
        underlying_kind: None,
        error: Some(error.into()),
    })
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
        if let Some(path) = &p.project
            && self.project_id_for_path(path).await.is_none()
        {
            return unresolved_kill_response(
                ExecutionKillTaskKind::ProjectNotFound,
                None,
                format!("project not found: {path}"),
            );
        }

        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
        let task = match task_repo.resolve(&p.task_id).await {
            Ok(Some(task)) => task,
            Ok(None) => {
                return unresolved_kill_response(
                    ExecutionKillTaskKind::TaskNotFound,
                    None,
                    format!("task not found: {}", p.task_id),
                );
            }
            Err(error) => {
                return unresolved_kill_response(ExecutionKillTaskKind::PoolError, None, error.to_string());
            }
        };
        let canonical_task_id = task.id;
        let Some(pool) = self.state.pool().await else {
            return unresolved_kill_response(
                ExecutionKillTaskKind::PoolUnavailable,
                Some(canonical_task_id),
                "slot pool actor not initialized",
            );
        };

        let snapshot = match pool.reconcile_terminate(&canonical_task_id).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let evidence = LivenessEvidenceSnapshot {
                    session_id: None,
                    task_id: Some(canonical_task_id.clone()),
                    task_run_id: None,
                    verdict: "protocol_violation".to_owned(),
                    outcome_kind: Some("reconciliation_incomplete".to_owned()),
                    outcome_reason: None,
                    evidence: serde_json::json!({ "transport_error": error.to_string() }),
                };
                let audit_error = LivenessRepository::new(self.state.db().clone())
                    .persist_evidence(&evidence)
                    .await
                    .err();
                return unresolved_kill_response(
                    if audit_error.is_some() {
                        ExecutionKillTaskKind::AuditFailed
                    } else {
                        ExecutionKillTaskKind::PoolError
                    },
                    Some(canonical_task_id),
                    audit_error.map_or_else(|| error.to_string(), |audit| audit.to_string()),
                );
            }
        };

        let scalar_execution = (snapshot.executions.len() == 1).then(|| &snapshot.executions[0]);
        let outcome_kind = serde_json::to_value(snapshot.kind)
            .expect("reconciliation kind serializes")
            .as_str()
            .expect("reconciliation kind is a string")
            .to_owned();
        let evidence = LivenessEvidenceSnapshot {
            session_id: scalar_execution.map(|execution| execution.session_id.clone()),
            task_id: Some(snapshot.task_id.clone()),
            task_run_id: scalar_execution.and_then(|execution| execution.task_run_id.clone()),
            verdict: if snapshot.ok { "dead" } else { "protocol_violation" }.to_owned(),
            outcome_kind: Some(outcome_kind),
            outcome_reason: None,
            evidence: serde_json::to_value(&snapshot).expect("reconciliation snapshot serializes"),
        };
        let audit_error = LivenessRepository::new(self.state.db().clone())
            .persist_evidence(&evidence)
            .await
            .err();
        let mut response = response_from_snapshot(snapshot);
        if let Some(error) = audit_error {
            response.ok = false;
            response.underlying_kind = Some(response.kind);
            response.kind = ExecutionKillTaskKind::AuditFailed;
            response.error = Some(error.to_string());
        }
        Json(response)
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
