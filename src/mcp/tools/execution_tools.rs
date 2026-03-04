// MCP tools for simplified execution control (ADR-009).

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::Deserialize;

use crate::db::repositories::task::TaskRepository;
use crate::mcp::server::DjinnMcpServer;
use crate::mcp::tools::{ObjectJson, json_object};

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ExecutionStartParams {
    /// Optional project path for project-scoped execution start.
    pub project: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ExecutionPauseParams {
    /// Optional project path for project-scoped execution pause.
    pub project: Option<String>,
    /// Pause mode: "graceful" (default) or "immediate".
    pub mode: Option<String>,
    /// Optional reason used when mode is "immediate".
    pub reason: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ExecutionResumeParams {
    /// Optional project path for project-scoped execution resume.
    pub project: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ExecutionStatusParams {
    /// Optional project path for project-scoped execution status.
    pub project: Option<String>,
}

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

fn json_error(message: impl Into<String>) -> Json<ObjectJson> {
    json_object(serde_json::json!({ "ok": false, "error": message.into() }))
}

#[tool_router(router = execution_tool_router, vis = "pub")]
impl DjinnMcpServer {
    async fn resolve_optional_project_id(
        &self,
        project: &Option<String>,
    ) -> Result<Option<String>, Json<ObjectJson>> {
        if let Some(path) = project {
            if let Some(project_id) = self.project_id_for_path(path).await {
                return Ok(Some(project_id));
            }
            return Err(json_error(format!("project not found: {path}")));
        }
        Ok(None)
    }

    /// Enable coordinator dispatch for ready tasks.
    #[tool(description = "Enable coordinator dispatch for ready tasks")]
    pub async fn execution_start(
        &self,
        Parameters(p): Parameters<ExecutionStartParams>,
    ) -> Json<ObjectJson> {
        let project_id = match self.resolve_optional_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let Some(coordinator) = self.state.coordinator().await else {
            return json_error("coordinator actor not initialized");
        };

        let status_result = match project_id.as_deref() {
            Some(id) => coordinator.get_project_status(id).await,
            None => coordinator.get_status().await,
        };
        let status = match status_result {
            Ok(status) => status,
            Err(e) => return json_error(e.to_string()),
        };

        let result = match project_id.as_deref() {
            Some(id) if status.paused => coordinator.resume_project(id).await,
            Some(id) => coordinator.trigger_dispatch_for_project(id).await,
            None if status.paused => coordinator.resume().await,
            None => coordinator.trigger_dispatch().await,
        };

        if let Err(e) = result {
            return json_error(e.to_string());
        }

        json_object(serde_json::json!({
            "ok": true,
            "state": "active",
            "resumed": status.paused,
            "scope": if project_id.is_some() { "project" } else { "global" },
            "project_id": project_id,
        }))
    }

    /// Pause active project execution phases.
    #[tool(
        description = "Pause active project execution phases. Graceful mode blocks new task dispatch immediately and lets active sessions run to completion before pausing — no work is lost. Immediate mode commits work-in-progress to each task's git branch, then stops all active sessions; interrupted tasks return to open status for re-dispatch after resume. Feature branches and worktrees are preserved in both modes."
    )]
    pub async fn execution_pause(
        &self,
        Parameters(p): Parameters<ExecutionPauseParams>,
    ) -> Json<ObjectJson> {
        let project_id = match self.resolve_optional_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let Some(coordinator) = self.state.coordinator().await else {
            return json_error("coordinator actor not initialized");
        };

        let mode = p.mode.as_deref().unwrap_or("graceful");
        let pause_result = match (mode, project_id.as_deref()) {
            ("graceful", Some(id)) => coordinator.pause_project(id).await,
            ("graceful", None) => coordinator.pause().await,
            ("immediate", Some(_)) => {
                return json_error("execution_pause(immediate) is only supported for global scope");
            }
            ("immediate", None) => {
                let reason = p
                    .reason
                    .as_deref()
                    .unwrap_or("session interrupted by execution_pause(immediate)");
                coordinator.pause_immediate(reason).await
            }
            _ => {
                return json_error(format!(
                    "invalid pause mode '{mode}', expected 'graceful' or 'immediate'"
                ));
            }
        };

        if let Err(e) = pause_result {
            return json_error(e.to_string());
        }

        json_object(serde_json::json!({
            "ok": true,
            "state": "paused",
            "mode": mode,
            "scope": if project_id.is_some() { "project" } else { "global" },
            "project_id": project_id,
        }))
    }

    /// Resume the task executor.
    #[tool(description = "Resume the task executor")]
    pub async fn execution_resume(
        &self,
        Parameters(p): Parameters<ExecutionResumeParams>,
    ) -> Json<ObjectJson> {
        let project_id = match self.resolve_optional_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let Some(coordinator) = self.state.coordinator().await else {
            return json_error("coordinator actor not initialized");
        };

        let result = match project_id.as_deref() {
            Some(id) => coordinator.resume_project(id).await,
            None => coordinator.resume().await,
        };
        if let Err(e) = result {
            return json_error(e.to_string());
        }

        json_object(serde_json::json!({
            "ok": true,
            "state": "active",
            "scope": if project_id.is_some() { "project" } else { "global" },
            "project_id": project_id,
        }))
    }

    /// Get execution status.
    #[tool(
        description = "Get execution status. Returns state (active/paused), running session count, model capacity, and per-task session details."
    )]
    pub async fn execution_status(
        &self,
        Parameters(p): Parameters<ExecutionStatusParams>,
    ) -> Json<ObjectJson> {
        let project_id = match self.resolve_optional_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let Some(coordinator) = self.state.coordinator().await else {
            return json_error("coordinator actor not initialized");
        };
        let Some(supervisor) = self.state.supervisor().await else {
            return json_error("supervisor actor not initialized");
        };

        let coordinator_status_result = match project_id.as_deref() {
            Some(id) => coordinator.get_project_status(id).await,
            None => coordinator.get_status().await,
        };
        let coordinator_status = match coordinator_status_result {
            Ok(status) => status,
            Err(e) => return json_error(e.to_string()),
        };
        let supervisor_status = match supervisor.get_status().await {
            Ok(status) => status,
            Err(e) => return json_error(e.to_string()),
        };

        let capacity = supervisor_status
            .capacity
            .iter()
            .map(|(model, model_capacity)| {
                (
                    model.clone(),
                    serde_json::json!({
                        "active": model_capacity.active,
                        "max": model_capacity.max,
                    }),
                )
            })
            .collect::<serde_json::Map<String, serde_json::Value>>();
        let max_sessions: u32 = supervisor_status.capacity.values().map(|c| c.max).sum();

        let sessions = supervisor_status
            .running_sessions
            .into_iter()
            .map(|session| {
                serde_json::json!({
                    "task_id": session.task_id,
                    "model_id": session.model_id,
                    "session_id": session.session_id,
                    "duration_seconds": session.duration_seconds,
                    "worktree_path": session.worktree_path,
                })
            })
            .collect::<Vec<_>>();

        json_object(serde_json::json!({
            "state": if coordinator_status.paused { "paused" } else { "active" },
            "global_state": if coordinator_status.global_paused { "paused" } else { "active" },
            "scope": if project_id.is_some() { "project" } else { "global" },
            "project_id": project_id,
            "running_sessions": supervisor_status.active_sessions,
            "max_sessions": max_sessions,
            "capacity": capacity,
            "sessions": sessions,
            "metrics": {
                "tasks_dispatched": coordinator_status.tasks_dispatched,
                "sessions_recovered": coordinator_status.sessions_recovered,
            },
        }))
    }

    /// Kill the active agent session for a task.
    #[tool(
        description = "Kill the active agent session for a task. Aborts the session, commits WIP, releases worktree and session slot. Safe to call on non-running tasks (no-op)."
    )]
    pub async fn execution_kill_task(
        &self,
        Parameters(p): Parameters<ExecutionKillTaskParams>,
    ) -> Json<ObjectJson> {
        if let Err(e) = self.validate_optional_project(&p.project).await {
            return e;
        }
        let Some(supervisor) = self.state.supervisor().await else {
            return json_error("supervisor actor not initialized");
        };

        if let Err(e) = supervisor.kill_session(&p.task_id).await {
            return json_error(e.to_string());
        }

        json_object(serde_json::json!({
            "ok": true,
            "task_id": p.task_id,
        }))
    }

    /// Get the session ID and worktree path for a running task.
    #[tool(description = "Get the session ID and worktree path for a running task")]
    pub async fn session_for_task(
        &self,
        Parameters(p): Parameters<SessionForTaskParams>,
    ) -> Json<ObjectJson> {
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return json_error(e),
        };
        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(task) = task_repo
            .resolve_in_project(&project_id, &p.task_id)
            .await
            .ok()
            .flatten()
        else {
            return json_error(format!("task not found: {}", p.task_id));
        };
        let Some(supervisor) = self.state.supervisor().await else {
            return json_error("supervisor actor not initialized");
        };

        let session = match supervisor.session_for_task(&task.id).await {
            Ok(session) => session,
            Err(e) => return json_error(e.to_string()),
        };

        match session {
            Some(session) => json_object(serde_json::json!({
                "task_id": task.id,
                "model_id": session.model_id,
                "session_id": session.session_id,
                "duration_seconds": session.duration_seconds,
                "worktree_path": session.worktree_path,
            })),
            None => json_object(serde_json::json!({
                "task_id": task.id,
                "session": null,
            })),
        }
    }
}
