// MCP tools for simplified execution control (ADR-009).

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::db::repositories::task::TaskRepository;
use crate::mcp::server::DjinnMcpServer;

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

#[derive(Serialize, schemars::JsonSchema)]
pub struct ExecutionStartResponse {
    pub ok: bool,
    pub state: Option<String>,
    pub resumed: Option<bool>,
    pub scope: Option<String>,
    pub project_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct ExecutionPauseResponse {
    pub ok: bool,
    pub state: Option<String>,
    pub mode: Option<String>,
    pub scope: Option<String>,
    pub project_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct ExecutionResumeResponse {
    pub ok: bool,
    pub state: Option<String>,
    pub scope: Option<String>,
    pub project_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct ExecutionStatusCapacity {
    #[schemars(with = "i64")]
    pub active: u32,
    #[schemars(with = "i64")]
    pub max: u32,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct ExecutionStatusSession {
    pub task_id: String,
    pub model_id: String,
    pub session_id: String,
    #[schemars(with = "i64")]
    pub duration_seconds: u64,
    pub worktree_path: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct ExecutionStatusMetrics {
    #[schemars(with = "i64")]
    pub tasks_dispatched: u64,
    #[schemars(with = "i64")]
    pub sessions_recovered: u64,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct ExecutionStatusResponse {
    pub ok: bool,
    pub state: Option<String>,
    pub scope: Option<String>,
    pub project_id: Option<String>,
    #[schemars(with = "Option<i64>")]
    pub running_sessions: Option<usize>,
    #[schemars(with = "Option<i64>")]
    pub max_sessions: Option<u32>,
    pub capacity: Option<HashMap<String, ExecutionStatusCapacity>>,
    pub sessions: Option<Vec<ExecutionStatusSession>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "ExecutionStatusMetrics")]
    pub metrics: Option<ExecutionStatusMetrics>,
    pub error: Option<String>,
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
    pub worktree_path: Option<String>,
    pub session: Option<String>,
    pub error: Option<String>,
}

#[tool_router(router = execution_tool_router, vis = "pub")]
impl DjinnMcpServer {
    async fn resolve_optional_project_id(
        &self,
        project: &Option<String>,
    ) -> Result<Option<String>, String> {
        if let Some(path) = project {
            if let Some(project_id) = self.project_id_for_path(path).await {
                return Ok(Some(project_id));
            }
            return Err(format!("project not found: {path}"));
        }
        Ok(None)
    }

    /// Enable coordinator dispatch for ready tasks.
    #[tool(description = "Enable coordinator dispatch for ready tasks")]
    pub async fn execution_start(
        &self,
        Parameters(p): Parameters<ExecutionStartParams>,
    ) -> Json<ExecutionStartResponse> {
        let project_id = match self.resolve_optional_project_id(&p.project).await {
            Ok(id) => id,
            Err(error) => {
                return Json(ExecutionStartResponse {
                    ok: false,
                    state: None,
                    resumed: None,
                    scope: None,
                    project_id: None,
                    error: Some(error),
                });
            }
        };
        let Some(coordinator) = self.state.coordinator().await else {
            return Json(ExecutionStartResponse {
                ok: false,
                state: None,
                resumed: None,
                scope: None,
                project_id: project_id.clone(),
                error: Some("coordinator actor not initialized".to_string()),
            });
        };

        // Trigger background health validation before dispatching (ADR-014).
        if let Err(e) = coordinator
            .validate_project_health(project_id.clone())
            .await
        {
            tracing::warn!(error = %e, "execution_start: failed to trigger project health validation");
        }

        let (resumed, result) = match project_id.as_deref() {
            Some(id) => {
                let paused = coordinator
                    .get_project_status(id)
                    .map(|s| s.paused)
                    .unwrap_or(false);
                let r = if paused {
                    coordinator.resume_project(id).await
                } else {
                    coordinator.trigger_dispatch_for_project(id).await
                };
                (paused, r)
            }
            // Global start: always resume (clears all project pauses + dispatches).
            None => (false, coordinator.resume().await),
        };

        if let Err(e) = result {
            return Json(ExecutionStartResponse {
                ok: false,
                state: None,
                resumed: None,
                scope: None,
                project_id: project_id.clone(),
                error: Some(e.to_string()),
            });
        }

        Json(ExecutionStartResponse {
            ok: true,
            state: Some("active".to_string()),
            resumed: Some(resumed),
            scope: Some(
                if project_id.is_some() {
                    "project"
                } else {
                    "global"
                }
                .to_string(),
            ),
            project_id,
            error: None,
        })
    }

    /// Pause active project execution phases.
    #[tool(
        description = "Pause active project execution phases. Graceful mode blocks new task dispatch immediately and lets active sessions run to completion before pausing — no work is lost. Immediate mode commits work-in-progress to each task's git branch, then stops all active sessions; interrupted tasks return to open status for re-dispatch after resume. Feature branches and worktrees are preserved in both modes."
    )]
    pub async fn execution_pause(
        &self,
        Parameters(p): Parameters<ExecutionPauseParams>,
    ) -> Json<ExecutionPauseResponse> {
        let project_id = match self.resolve_optional_project_id(&p.project).await {
            Ok(id) => id,
            Err(error) => {
                return Json(ExecutionPauseResponse {
                    ok: false,
                    state: None,
                    mode: None,
                    scope: None,
                    project_id: None,
                    error: Some(error),
                });
            }
        };
        let Some(coordinator) = self.state.coordinator().await else {
            return Json(ExecutionPauseResponse {
                ok: false,
                state: None,
                mode: None,
                scope: None,
                project_id: project_id.clone(),
                error: Some("coordinator actor not initialized".to_string()),
            });
        };

        let mode = p.mode.as_deref().unwrap_or("graceful");
        let pause_result = match (mode, project_id.as_deref()) {
            ("graceful", Some(id)) => coordinator.pause_project(id).await,
            ("graceful", None) => coordinator.pause().await,
            ("immediate", Some(id)) => {
                let reason = p
                    .reason
                    .as_deref()
                    .unwrap_or("session interrupted by execution_pause(immediate)");
                coordinator.pause_project_immediate(id, reason).await
            }
            ("immediate", None) => {
                let reason = p
                    .reason
                    .as_deref()
                    .unwrap_or("session interrupted by execution_pause(immediate)");
                coordinator.pause_immediate(reason).await
            }
            _ => {
                return Json(ExecutionPauseResponse {
                    ok: false,
                    state: None,
                    mode: Some(mode.to_string()),
                    scope: None,
                    project_id: project_id.clone(),
                    error: Some(format!(
                        "invalid pause mode '{mode}', expected 'graceful' or 'immediate'"
                    )),
                });
            }
        };

        if let Err(e) = pause_result {
            return Json(ExecutionPauseResponse {
                ok: false,
                state: None,
                mode: Some(mode.to_string()),
                scope: None,
                project_id: project_id.clone(),
                error: Some(e.to_string()),
            });
        }

        Json(ExecutionPauseResponse {
            ok: true,
            state: Some("paused".to_string()),
            mode: Some(mode.to_string()),
            scope: Some(
                if project_id.is_some() {
                    "project"
                } else {
                    "global"
                }
                .to_string(),
            ),
            project_id,
            error: None,
        })
    }

    /// Resume the task executor.
    #[tool(description = "Resume the task executor")]
    pub async fn execution_resume(
        &self,
        Parameters(p): Parameters<ExecutionResumeParams>,
    ) -> Json<ExecutionResumeResponse> {
        let project_id = match self.resolve_optional_project_id(&p.project).await {
            Ok(id) => id,
            Err(error) => {
                return Json(ExecutionResumeResponse {
                    ok: false,
                    state: None,
                    scope: None,
                    project_id: None,
                    error: Some(error),
                });
            }
        };
        let Some(coordinator) = self.state.coordinator().await else {
            return Json(ExecutionResumeResponse {
                ok: false,
                state: None,
                scope: None,
                project_id: project_id.clone(),
                error: Some("coordinator actor not initialized".to_string()),
            });
        };

        let result = match project_id.as_deref() {
            Some(id) => coordinator.resume_project(id).await,
            None => coordinator.resume().await,
        };
        if let Err(e) = result {
            return Json(ExecutionResumeResponse {
                ok: false,
                state: None,
                scope: None,
                project_id: project_id.clone(),
                error: Some(e.to_string()),
            });
        }

        Json(ExecutionResumeResponse {
            ok: true,
            state: Some("active".to_string()),
            scope: Some(
                if project_id.is_some() {
                    "project"
                } else {
                    "global"
                }
                .to_string(),
            ),
            project_id,
            error: None,
        })
    }

    /// Get execution status.
    #[tool(
        description = "Get execution status. Returns state (active/paused), running session count, model capacity, and per-task session details."
    )]
    pub async fn execution_status(
        &self,
        Parameters(p): Parameters<ExecutionStatusParams>,
    ) -> Json<ExecutionStatusResponse> {
        let project_id = match self.resolve_optional_project_id(&p.project).await {
            Ok(id) => id,
            Err(error) => {
                return Json(ExecutionStatusResponse {
                    ok: false,
                    state: None,
                    scope: None,
                    project_id: None,
                    running_sessions: None,
                    max_sessions: None,
                    capacity: None,
                    sessions: None,
                    metrics: None,
                    error: Some(error),
                });
            }
        };
        let Some(coordinator) = self.state.coordinator().await else {
            return Json(ExecutionStatusResponse {
                ok: false,
                state: None,
                scope: None,
                project_id: project_id.clone(),
                running_sessions: None,
                max_sessions: None,
                capacity: None,
                sessions: None,
                metrics: None,
                error: Some("coordinator actor not initialized".to_string()),
            });
        };
        let Some(supervisor) = self.state.supervisor().await else {
            return Json(ExecutionStatusResponse {
                ok: false,
                state: None,
                scope: None,
                project_id: project_id.clone(),
                running_sessions: None,
                max_sessions: None,
                capacity: None,
                sessions: None,
                metrics: None,
                error: Some("supervisor actor not initialized".to_string()),
            });
        };

        let coordinator_status_result = match project_id.as_deref() {
            Some(id) => coordinator.get_project_status(id),
            None => coordinator.get_status(),
        };
        let coordinator_status = match coordinator_status_result {
            Ok(status) => status,
            Err(e) => {
                return Json(ExecutionStatusResponse {
                    ok: false,
                    state: None,
                    scope: None,
                    project_id: project_id.clone(),
                    running_sessions: None,
                    max_sessions: None,
                    capacity: None,
                    sessions: None,
                    metrics: None,
                    error: Some(e.to_string()),
                });
            }
        };
        let supervisor_status = match supervisor.get_status().await {
            Ok(status) => status,
            Err(e) => {
                return Json(ExecutionStatusResponse {
                    ok: false,
                    state: None,
                    scope: None,
                    project_id: project_id.clone(),
                    running_sessions: None,
                    max_sessions: None,
                    capacity: None,
                    sessions: None,
                    metrics: None,
                    error: Some(e.to_string()),
                });
            }
        };

        let capacity = supervisor_status
            .capacity
            .iter()
            .map(|(model, model_capacity)| {
                (
                    model.clone(),
                    ExecutionStatusCapacity {
                        active: model_capacity.active,
                        max: model_capacity.max,
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        let max_sessions: u32 = supervisor_status.capacity.values().map(|c| c.max).sum();

        let sessions = supervisor_status
            .running_sessions
            .into_iter()
            .map(|session| ExecutionStatusSession {
                task_id: session.task_id,
                model_id: session.model_id,
                session_id: session.session_id,
                duration_seconds: session.duration_seconds,
                worktree_path: session.worktree_path,
            })
            .collect::<Vec<_>>();

        Json(ExecutionStatusResponse {
            ok: true,
            state: Some(
                if coordinator_status.paused {
                    "paused"
                } else {
                    "active"
                }
                .to_string(),
            ),
            scope: Some(
                if project_id.is_some() {
                    "project"
                } else {
                    "global"
                }
                .to_string(),
            ),
            project_id,
            running_sessions: Some(supervisor_status.active_sessions),
            max_sessions: Some(max_sessions),
            capacity: Some(capacity),
            sessions: Some(sessions),
            metrics: Some(ExecutionStatusMetrics {
                tasks_dispatched: coordinator_status.tasks_dispatched,
                sessions_recovered: coordinator_status.sessions_recovered,
            }),
            error: None,
        })
    }

    /// Kill the active agent session for a task.
    #[tool(
        description = "Kill the active agent session for a task. Aborts the session, commits WIP, releases worktree and session slot. Safe to call on non-running tasks (no-op)."
    )]
    pub async fn execution_kill_task(
        &self,
        Parameters(p): Parameters<ExecutionKillTaskParams>,
    ) -> Json<ExecutionKillTaskResponse> {
        if let Some(path) = &p.project
            && self.project_id_for_path(path).await.is_none() {
                return Json(ExecutionKillTaskResponse {
                    ok: false,
                    task_id: None,
                    error: Some(format!("project not found: {path}")),
                });
            }
        let Some(supervisor) = self.state.supervisor().await else {
            return Json(ExecutionKillTaskResponse {
                ok: false,
                task_id: None,
                error: Some("supervisor actor not initialized".to_string()),
            });
        };

        if let Err(e) = supervisor.kill_session(&p.task_id).await {
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
                    worktree_path: None,
                    session: None,
                    error: Some(e),
                });
            }
        };
        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
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
                worktree_path: None,
                session: None,
                error: Some(format!("task not found: {}", missing_task_id)),
            });
        };
        let Some(supervisor) = self.state.supervisor().await else {
            return Json(SessionForTaskResponse {
                ok: false,
                task_id: task.id,
                model_id: None,
                session_id: None,
                duration_seconds: None,
                worktree_path: None,
                session: None,
                error: Some("supervisor actor not initialized".to_string()),
            });
        };

        let session = match supervisor.session_for_task(&task.id).await {
            Ok(session) => session,
            Err(e) => {
                return Json(SessionForTaskResponse {
                    ok: false,
                    task_id: task.id,
                    model_id: None,
                    session_id: None,
                    duration_seconds: None,
                    worktree_path: None,
                    session: None,
                    error: Some(e.to_string()),
                });
            }
        };

        match session {
            Some(session) => Json(SessionForTaskResponse {
                ok: true,
                task_id: task.id,
                model_id: Some(session.model_id),
                session_id: Some(session.session_id),
                duration_seconds: Some(session.duration_seconds),
                worktree_path: session.worktree_path,
                session: None,
                error: None,
            }),
            None => Json(SessionForTaskResponse {
                ok: true,
                task_id: task.id,
                model_id: None,
                session_id: None,
                duration_seconds: None,
                worktree_path: None,
                session: None,
                error: None,
            }),
        }
    }
}
