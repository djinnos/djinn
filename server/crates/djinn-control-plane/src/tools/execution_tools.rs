// MCP tools for task-scoped execution control.
//
// Global execution toggles (start/pause/resume/status) were removed in the
// K8s-mode cut-over: the coordinator is always active and dispatches tasks
// unconditionally. The remaining tools operate on individual tasks:
//   - `execution_kill_task`: interrupt the agent session for one task.
//   - `session_for_task`: resolve the session + workspace for a task.

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::bridge::{
    ReconcileTerminateExecution, ReconcileTerminateKind, ReconcileTerminateObservations,
    ReconcileTerminateSnapshot,
};
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
                return unresolved_kill_response(
                    ExecutionKillTaskKind::PoolError,
                    None,
                    error.to_string(),
                );
            }
        };
        let canonical_task_id = task.id;
        let Some(pool) = self.state.pool().await else {
            let error = "slot pool actor not initialized";
            let evidence = LivenessEvidenceSnapshot {
                session_id: None,
                task_id: Some(canonical_task_id.clone()),
                task_run_id: None,
                verdict: "protocol_violation".to_owned(),
                outcome_kind: Some("reconciliation_incomplete".to_owned()),
                outcome_reason: None,
                evidence: serde_json::json!({ "transport_error": error }),
            };
            let audit_error = LivenessRepository::new(self.state.db().clone())
                .persist_evidence(&evidence)
                .await
                .err();
            return unresolved_kill_response(
                if audit_error.is_some() {
                    ExecutionKillTaskKind::AuditFailed
                } else {
                    ExecutionKillTaskKind::PoolUnavailable
                },
                Some(canonical_task_id),
                audit_error.map_or_else(|| error.to_owned(), |audit| audit.to_string()),
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
            verdict: if snapshot.ok {
                "dead"
            } else {
                "protocol_violation"
            }
            .to_owned(),
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
    use super::*;
    use crate::bridge::{PoolStatus, RunningTaskInfo, SlotPoolOps};
    use crate::state::McpState;
    use async_trait::async_trait;
    use djinn_db::{EffectiveCreatorProvenance, ProjectRepository, UserRepository};
    use rmcp::handler::server::wrapper::Parameters;
    use std::sync::{Arc, Mutex};

    struct RecordingSlotPool {
        reconciled: Mutex<Vec<String>>,
        result: Mutex<Result<ReconcileTerminateSnapshot, String>>,
    }
    impl RecordingSlotPool {
        fn reconciled(&self) -> Vec<String> {
            self.reconciled.lock().expect("pool mutex").clone()
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
        async fn kill_session(&self, _: &str) -> Result<(), String> {
            Err("legacy kill_session must not be used".into())
        }
        async fn terminate_session(&self, _: &str) -> Result<(), String> {
            Err("legacy terminate_session must not be used".into())
        }
        async fn reconcile_terminate(
            &self,
            task_id: &str,
        ) -> Result<ReconcileTerminateSnapshot, String> {
            self.reconciled
                .lock()
                .expect("pool mutex")
                .push(task_id.into());
            self.result.lock().expect("pool mutex").clone()
        }
        async fn session_for_task(&self, _: &str) -> Result<Option<RunningTaskInfo>, String> {
            Ok(None)
        }
        async fn has_session(&self, _: &str) -> Result<bool, String> {
            Err("legacy has_session must not be used".into())
        }
    }
    fn server_with_pool(
        db: djinn_db::Database,
        pool: Option<Arc<RecordingSlotPool>>,
    ) -> DjinnMcpServer {
        DjinnMcpServer::new(McpState::new(
            db,
            djinn_core::events::EventBus::noop(),
            djinn_provider::catalog::CatalogService::new(),
            djinn_provider::catalog::HealthTracker::new(),
            None,
            pool.map(|pool| pool as Arc<dyn SlotPoolOps>),
            None,
            None,
            Arc::new(crate::state::stubs::StubLspOps),
            Arc::new(crate::state::stubs::StubRuntimeOps),
            Arc::new(crate::state::stubs::StubGitOps),
            Arc::new(crate::state::stubs::StubRepoGraphOps),
        ))
    }
    async fn create_task(db: &djinn_db::Database) -> (String, String) {
        let events = djinn_core::events::EventBus::noop();
        let project = ProjectRepository::new(db.clone(), events.clone())
            .create("kill-test-project", "owner", "repo")
            .await
            .expect("project");
        let user = UserRepository::new(db.clone())
            .upsert_from_github(999_991, "kill-test-user", None, None)
            .await
            .expect("user");
        let task = djinn_db::TaskRepository::new(db.clone(), events)
            .create_in_project_with_provenance(
                &project.id,
                None,
                EffectiveCreatorProvenance {
                    explicit_user_id: Some(&user.id),
                    source_task_id: None,
                    proposal_id: None,
                },
                "kill test",
                "",
                "",
                "task",
                0,
                "",
                Some("in_progress"),
                None,
            )
            .await
            .expect("task");
        (task.id, task.short_id)
    }
    fn snapshot(
        task_id: String,
        kind: ReconcileTerminateKind,
        ok: bool,
    ) -> ReconcileTerminateSnapshot {
        ReconcileTerminateSnapshot {
            ok,
            kind,
            task_id,
            executions: vec![
                ReconcileTerminateExecution {
                    session_id: "second".into(),
                    task_run_id: Some("run-2".into()),
                    teardown_owner: true,
                    teardown_attempted: true,
                    teardown_error: None,
                    settlement_attempted: true,
                    settlement_error: None,
                },
                ReconcileTerminateExecution {
                    session_id: "first".into(),
                    task_run_id: None,
                    teardown_owner: false,
                    teardown_attempted: false,
                    teardown_error: Some("shared run".into()),
                    settlement_attempted: true,
                    settlement_error: None,
                },
            ],
            observations: ReconcileTerminateObservations {
                initial_non_terminal_ids: vec!["second".into(), "first".into()],
                initial_mapping_slot_id: Some(7),
                initial_pending_teardown: false,
                initial_compacting: false,
                fenced_generation: Some(4),
                initial_capture_error: None,
                final_non_terminal_ids: vec![],
                final_mapping_slot_id: None,
                final_pending_teardown: false,
                final_reread_error: None,
                pool_cleanup_error: None,
                completion_source: "immediate".into(),
                underlying_kind: None,
            },
        }
    }
    #[tokio::test]
    async fn execution_kill_task_resolves_uuid_and_short_id_to_canonical_snapshot() {
        let db = djinn_db::Database::open_in_memory().expect("db");
        let (task_id, short_id) = create_task(&db).await;
        let pool = Arc::new(RecordingSlotPool {
            reconciled: Mutex::new(Vec::new()),
            result: Mutex::new(Ok(snapshot(
                task_id.clone(),
                ReconcileTerminateKind::Terminated,
                true,
            ))),
        });

        let Json(short_response) = server_with_pool(db.clone(), Some(pool.clone()))
            .execution_kill_task(Parameters(ExecutionKillTaskParams {
                task_id: short_id,
                project: None,
            }))
            .await;
        let Json(uuid_response) = server_with_pool(db.clone(), Some(pool.clone()))
            .execution_kill_task(Parameters(ExecutionKillTaskParams {
                task_id: task_id.clone(),
                project: None,
            }))
            .await;

        for response in [&short_response, &uuid_response] {
            assert!(response.ok);
            assert!(matches!(response.kind, ExecutionKillTaskKind::Terminated));
            assert_eq!(response.task_id.as_deref(), Some(task_id.as_str()));
        }
        assert_eq!(pool.reconciled(), vec![task_id.clone(), task_id.clone()]);
        assert_eq!(short_response.executions[0].session_id, "second");
        assert_eq!(
            short_response
                .observations
                .as_ref()
                .expect("observations")
                .initial_non_terminal_ids,
            vec!["second", "first"]
        );
        assert_eq!(
            LivenessRepository::new(db)
                .count_evidence_for_task(&task_id)
                .await
                .expect("evidence count"),
            2,
            "each outward invocation records its own evidence row"
        );
    }
    #[tokio::test]
    async fn execution_kill_task_task_not_found_writes_no_audit() {
        let db = djinn_db::Database::open_in_memory().expect("db");
        let Json(response) = server_with_pool(db.clone(), None)
            .execution_kill_task(Parameters(ExecutionKillTaskParams {
                task_id: "missing".into(),
                project: None,
            }))
            .await;
        assert!(!response.ok);
        assert!(matches!(response.kind, ExecutionKillTaskKind::TaskNotFound));
        let count = LivenessRepository::new(db)
            .count_evidence_for_task("missing")
            .await
            .expect("count");
        assert_eq!(count, 0);
    }
    #[tokio::test]
    async fn execution_kill_task_persists_transport_evidence_when_pool_is_unavailable() {
        let db = djinn_db::Database::open_in_memory().expect("db");
        let (task_id, _) = create_task(&db).await;
        let Json(response) = server_with_pool(db.clone(), None)
            .execution_kill_task(Parameters(ExecutionKillTaskParams {
                task_id: task_id.clone(),
                project: None,
            }))
            .await;
        assert!(matches!(
            response.kind,
            ExecutionKillTaskKind::PoolUnavailable
        ));
        let count = LivenessRepository::new(db)
            .count_evidence_for_task(&task_id)
            .await
            .expect("count");
        assert_eq!(count, 1);
    }
    #[tokio::test]
    async fn execution_kill_task_preserves_typed_reconciliation_failure() {
        let db = djinn_db::Database::open_in_memory().expect("db");
        let (task_id, _) = create_task(&db).await;
        let pool = Arc::new(RecordingSlotPool {
            reconciled: Mutex::new(Vec::new()),
            result: Mutex::new(Ok(snapshot(
                task_id.clone(),
                ReconcileTerminateKind::TeardownFailed,
                false,
            ))),
        });
        let Json(response) = server_with_pool(db, Some(pool))
            .execution_kill_task(Parameters(ExecutionKillTaskParams {
                task_id,
                project: None,
            }))
            .await;
        assert!(!response.ok);
        assert!(matches!(
            response.kind,
            ExecutionKillTaskKind::TeardownFailed
        ));
    }

    #[tokio::test]
    async fn execution_kill_task_audit_failure_preserves_snapshot() {
        let db = djinn_db::Database::open_in_memory().expect("db");
        let (task_id, _) = create_task(&db).await;
        let pool = Arc::new(RecordingSlotPool {
            reconciled: Mutex::new(Vec::new()),
            result: Mutex::new(Ok(snapshot(
                task_id.clone(),
                ReconcileTerminateKind::TeardownFailed,
                false,
            ))),
        });
        // A scalar execution is eligible for scalar FK persistence. A missing
        // session ID makes that insert fail while leaving the schema untouched.
        *pool.result.lock().expect("pool mutex") = Ok(ReconcileTerminateSnapshot {
            executions: vec![ReconcileTerminateExecution {
                session_id: "missing-session".into(),
                task_run_id: None,
                teardown_owner: true,
                teardown_attempted: true,
                teardown_error: None,
                settlement_attempted: true,
                settlement_error: None,
            }],
            ..snapshot(
                task_id.clone(),
                ReconcileTerminateKind::TeardownFailed,
                false,
            )
        });
        let Json(response) = server_with_pool(db, Some(pool))
            .execution_kill_task(Parameters(ExecutionKillTaskParams {
                task_id,
                project: None,
            }))
            .await;
        assert!(!response.ok);
        assert!(matches!(response.kind, ExecutionKillTaskKind::AuditFailed));
        assert!(matches!(
            response.underlying_kind,
            Some(ExecutionKillTaskKind::TeardownFailed)
        ));
        assert_eq!(response.executions[0].session_id, "missing-session");
        assert_eq!(
            response
                .observations
                .expect("observations")
                .completion_source,
            "immediate"
        );
    }
}
