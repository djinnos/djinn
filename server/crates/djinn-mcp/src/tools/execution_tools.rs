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
use djinn_db::SessionRepository;
use djinn_db::TaskRepository;

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
        description = "Kill the active agent session for a task. Aborts the session, commits WIP, releases worktree and session slot. Safe to call on non-running tasks (no-op)."
    )]
    pub async fn execution_kill_task(
        &self,
        Parameters(p): Parameters<ExecutionKillTaskParams>,
    ) -> Json<ExecutionKillTaskResponse> {
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

        if let Err(e) = pool.kill_session(&p.task_id).await {
            return Json(ExecutionKillTaskResponse {
                ok: false,
                task_id: Some(p.task_id),
                error: Some(e.to_string()),
            });
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
