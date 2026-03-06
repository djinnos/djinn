use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::db::repositories::session::SessionRepository;
use crate::db::repositories::task::TaskRepository;
use crate::mcp::server::DjinnMcpServer;
use crate::models::session::SessionRecord;

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SessionListParams {
    /// Task UUID or short_id.
    pub task_id: String,
    /// Absolute project path (required).
    pub project: String,
    /// When true, return sessions in continuation-chain order (oldest root first, each
    /// subsequent session linked via continuation_of) instead of started_at DESC.
    /// Useful for rendering the session timeline with compaction boundaries.
    pub chain_ordered: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SessionShowParams {
    /// Session UUID.
    pub id: String,
    /// Absolute project path (required).
    pub project: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SessionActiveParams {
    /// Absolute project path (required).
    pub project: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SessionToolSession {
    pub id: String,
    pub project_id: String,
    pub task_id: String,
    pub model_id: String,
    pub agent_type: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub status: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub worktree_path: Option<String>,
    pub goose_session_id: Option<String>,
    pub continuation_of: Option<String>,
}

impl From<SessionRecord> for SessionToolSession {
    fn from(value: SessionRecord) -> Self {
        Self {
            id: value.id,
            project_id: value.project_id,
            task_id: value.task_id,
            model_id: value.model_id,
            agent_type: value.agent_type,
            started_at: value.started_at,
            ended_at: value.ended_at,
            status: value.status,
            tokens_in: value.tokens_in,
            tokens_out: value.tokens_out,
            worktree_path: value.worktree_path,
            goose_session_id: value.goose_session_id,
            continuation_of: value.continuation_of,
        }
    }
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SessionListResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<SessionToolSession>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SessionActiveResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<SessionToolSession>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_sessions: Option<Vec<SessionToolSession>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_triggered: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SessionShowResponse {
    #[serde(flatten)]
    pub session: Option<SessionToolSession>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[tool_router(router = session_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// List all sessions for a task, newest first by started_at.
    #[tool(
        description = "session_list(task_id) returns all sessions for a task ordered by started_at"
    )]
    pub async fn session_list(
        &self,
        Parameters(p): Parameters<SessionListParams>,
    ) -> Json<SessionListResponse> {
        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(SessionListResponse {
                    task_id: None,
                    sessions: None,
                    error: Some(e),
                });
            }
        };
        let Some(task) = task_repo
            .resolve_in_project(&project_id, &p.task_id)
            .await
            .ok()
            .flatten()
        else {
            return Json(SessionListResponse {
                task_id: None,
                sessions: None,
                error: Some(format!("task not found: {}", p.task_id)),
            });
        };

        let repo = SessionRepository::new(self.state.db().clone(), self.state.events().clone());
        let result = if p.chain_ordered.unwrap_or(false) {
            repo.chain_for_task(&task.id).await
        } else {
            repo.list_for_task_in_project(&project_id, &task.id).await
        };
        match result {
            Ok(sessions) => Json(SessionListResponse {
                task_id: Some(task.id),
                sessions: Some(sessions.into_iter().map(Into::into).collect()),
                error: None,
            }),
            Err(e) => Json(SessionListResponse {
                task_id: None,
                sessions: None,
                error: Some(e.to_string()),
            }),
        }
    }

    /// List currently running sessions across all tasks.
    #[tool(description = "session_active() returns all currently running sessions across tasks")]
    pub async fn session_active(
        &self,
        Parameters(p): Parameters<SessionActiveParams>,
    ) -> Json<SessionActiveResponse> {
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(SessionActiveResponse {
                    sessions: None,
                    stale_sessions: None,
                    recovery_triggered: None,
                    error: Some(e),
                });
            }
        };
        let Some(pool) = self.state.pool().await else {
            return Json(SessionActiveResponse {
                sessions: None,
                stale_sessions: None,
                recovery_triggered: None,
                error: Some("slot pool actor not initialized".to_string()),
            });
        };
        let coordinator = self.state.coordinator().await;
        let repo = SessionRepository::new(self.state.db().clone(), self.state.events().clone());
        match repo.list_active_in_project(&project_id).await {
            Ok(sessions) => {
                let mut runtime_sessions = Vec::new();
                let mut stale_sessions = Vec::new();

                for session in sessions {
                    match pool.has_session(&session.task_id).await {
                        Ok(true) => runtime_sessions.push(session),
                        Ok(false) => stale_sessions.push(session),
                        Err(e) => {
                            return Json(SessionActiveResponse {
                                sessions: None,
                                stale_sessions: None,
                                recovery_triggered: None,
                                error: Some(e.to_string()),
                            });
                        }
                    }
                }

                let mut recovery_triggered = false;
                if !stale_sessions.is_empty()
                    && let Some(coordinator) = coordinator
                    && let Ok(status) = coordinator.get_project_status(&project_id)
                    && !status.paused
                    && coordinator
                        .trigger_dispatch_for_project(&project_id)
                        .await
                        .is_ok()
                {
                    recovery_triggered = true;
                }

                Json(SessionActiveResponse {
                    sessions: Some(runtime_sessions.into_iter().map(Into::into).collect()),
                    stale_sessions: Some(stale_sessions.into_iter().map(Into::into).collect()),
                    recovery_triggered: Some(recovery_triggered),
                    error: None,
                })
            }
            Err(e) => Json(SessionActiveResponse {
                sessions: None,
                stale_sessions: None,
                recovery_triggered: None,
                error: Some(e.to_string()),
            }),
        }
    }

    /// Get a single session by id.
    #[tool(
        description = "session_show(id) returns session details: id, task_id, model_id, agent_type, started_at, ended_at, status, tokens_in, tokens_out"
    )]
    pub async fn session_show(
        &self,
        Parameters(p): Parameters<SessionShowParams>,
    ) -> Json<SessionShowResponse> {
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(SessionShowResponse {
                    session: None,
                    error: Some(e),
                });
            }
        };
        let repo = SessionRepository::new(self.state.db().clone(), self.state.events().clone());
        match repo.get_in_project(&project_id, &p.id).await {
            Ok(Some(session)) => Json(SessionShowResponse {
                session: Some(session.into()),
                error: None,
            }),
            Ok(None) => Json(SessionShowResponse {
                session: None,
                error: Some(format!("session not found: {}", p.id)),
            }),
            Err(e) => Json(SessionShowResponse {
                session: None,
                error: Some(e.to_string()),
            }),
        }
    }
}
