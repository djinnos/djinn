use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::db::ActivityQuery;
use crate::db::SessionMessageRepository;
use crate::db::SessionRepository;
use crate::db::TaskRepository;
use crate::mcp::server::DjinnMcpServer;
use crate::models::SessionRecord;

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SessionListParams {
    /// Task UUID or short_id.
    pub task_id: Option<String>,
    /// Absolute project path (required).
    pub project: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SessionActiveParams {
    /// Absolute project path (required).
    pub project: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SessionShowParams {
    /// Session UUID.
    pub id: String,
    /// Absolute project path (required).
    pub project: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SessionMessagesParams {
    /// Session UUID.
    pub id: String,
    /// Absolute project path (required).
    pub project: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SessionMessage {
    pub role: String,
    pub content: Vec<super::json_object::AnyJson>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SessionMessagesResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<SessionMessage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SessionToolSession {
    pub id: String,
    pub project_id: String,
    pub task_id: Option<String>,
    pub model_id: String,
    pub agent_type: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub status: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub worktree_path: Option<String>,
    pub goose_session_id: Option<String>,
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

// ── Task timeline (single-call) ──────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskTimelineParams {
    /// Task UUID or short_id.
    pub task_id: Option<String>,
    /// Absolute project path (required).
    pub project: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TimelineMessage {
    pub session_id: String,
    pub role: String,
    pub content: Vec<super::json_object::AnyJson>,
    pub agent_type: String,
    pub model_id: String,
    pub timestamp: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TimelineActivity {
    pub event_type: String,
    pub payload: super::json_object::AnyJson,
    pub timestamp: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TaskTimelineResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<SessionToolSession>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<TimelineMessage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<Vec<TimelineActivity>>,
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
        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
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
        let task_id = match p.task_id.as_deref() {
            Some(id) => id,
            None => {
                return Json(SessionListResponse {
                    task_id: None,
                    sessions: None,
                    error: Some("task_id is required".to_string()),
                });
            }
        };
        let Some(task) = task_repo
            .resolve_in_project(&project_id, task_id)
            .await
            .ok()
            .flatten()
        else {
            return Json(SessionListResponse {
                task_id: None,
                sessions: None,
                error: Some(format!("task not found: {}", task_id)),
            });
        };

        let repo = SessionRepository::new(self.state.db().clone(), self.state.event_bus());
        match repo.list_for_task_in_project(&project_id, &task.id).await {
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
        let repo = SessionRepository::new(self.state.db().clone(), self.state.event_bus());
        match repo.list_active_in_project(&project_id).await {
            Ok(sessions) => {
                let mut runtime_sessions = Vec::new();
                let mut stale_sessions = Vec::new();

                for session in sessions {
                    if let Some(task_id) = session.task_id.as_deref() {
                        match pool.has_session(task_id).await {
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
                    } else {
                        runtime_sessions.push(session);
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
        let repo = SessionRepository::new(self.state.db().clone(), self.state.event_bus());
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

    /// Return full conversation messages for a session (from DB).
    #[tool(
        description = "session_messages(id) returns conversation messages for a session: role + content blocks (text/tool_use/tool_result/thinking)"
    )]
    pub async fn session_messages(
        &self,
        Parameters(p): Parameters<SessionMessagesParams>,
    ) -> Json<SessionMessagesResponse> {
        let err = |e: String| {
            Json(SessionMessagesResponse {
                session_id: None,
                agent_type: None,
                model_id: None,
                messages: None,
                error: Some(e),
            })
        };

        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return err(e),
        };

        let session_repo =
            SessionRepository::new(self.state.db().clone(), self.state.event_bus());
        let session = match session_repo.get_in_project(&project_id, &p.id).await {
            Ok(Some(s)) => s,
            Ok(None) => return err(format!("session not found: {}", p.id)),
            Err(e) => return err(e.to_string()),
        };

        let msg_repo =
            SessionMessageRepository::new(self.state.db().clone(), self.state.event_bus());
        let conversation = match msg_repo.load_conversation(&session.id).await {
            Ok(c) => c,
            Err(e) => {
                return Json(SessionMessagesResponse {
                    session_id: Some(session.id),
                    agent_type: Some(session.agent_type),
                    model_id: Some(session.model_id),
                    messages: None,
                    error: Some(format!("failed to load messages: {e}")),
                });
            }
        };

        let messages: Vec<SessionMessage> = conversation
            .messages
            .into_iter()
            .map(|msg| {
                let role = match msg.role {
                    djinn_agent::message::Role::System => "system",
                    djinn_agent::message::Role::User => "user",
                    djinn_agent::message::Role::Assistant => "assistant",
                }
                .to_owned();
                let content = msg
                    .content
                    .into_iter()
                    .map(|block| {
                        super::json_object::AnyJson(serde_json::to_value(block).unwrap_or_default())
                    })
                    .collect();
                SessionMessage { role, content }
            })
            .collect();

        Json(SessionMessagesResponse {
            session_id: Some(session.id),
            agent_type: Some(session.agent_type),
            model_id: Some(session.model_id),
            messages: Some(messages),
            error: None,
        })
    }

    /// Return the full timeline for a task: sessions, messages, and activity — in one call.
    #[tool(
        description = "task_timeline(task_id) returns sessions, all conversation messages (with timestamps), and activity log entries for a task in a single call."
    )]
    pub async fn task_timeline(
        &self,
        Parameters(p): Parameters<TaskTimelineParams>,
    ) -> Json<TaskTimelineResponse> {
        let err = |e: String| {
            Json(TaskTimelineResponse {
                sessions: None,
                messages: None,
                activity: None,
                error: Some(e),
            })
        };

        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return err(e),
        };

        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
        let task_id = match p.task_id.as_deref() {
            Some(id) => id,
            None => return err("task_id is required".to_string()),
        };
        let task = match task_repo.resolve_in_project(&project_id, task_id).await {
            Ok(Some(t)) => t,
            Ok(None) => return err(format!("task not found: {}", task_id)),
            Err(e) => return err(e.to_string()),
        };

        let session_repo =
            SessionRepository::new(self.state.db().clone(), self.state.event_bus());
        let msg_repo =
            SessionMessageRepository::new(self.state.db().clone(), self.state.event_bus());

        // 1. Get sessions
        let sessions = match session_repo
            .list_for_task_in_project(&project_id, &task.id)
            .await
        {
            Ok(s) => s,
            Err(e) => return err(e.to_string()),
        };

        let session_ids: Vec<String> = sessions.iter().map(|s| s.id.clone()).collect();

        // Build a lookup map: session_id → (agent_type, model_id)
        let session_info: std::collections::HashMap<String, (&str, &str)> = sessions
            .iter()
            .map(|s| (s.id.clone(), (s.agent_type.as_str(), s.model_id.as_str())))
            .collect();

        // 2. Bulk-load messages for all sessions (1 query)
        let raw_messages = match msg_repo.load_for_sessions(&session_ids).await {
            Ok(m) => m,
            Err(e) => return err(e.to_string()),
        };

        let messages: Vec<TimelineMessage> = raw_messages
            .into_iter()
            .filter(|(_, role, _, _)| role != "system") // skip system prompts
            .map(|(session_id, role, content_json, created_at)| {
                let content: Vec<super::json_object::AnyJson> =
                    serde_json::from_str::<Vec<super::json_object::AnyJson>>(&content_json)
                        .unwrap_or_default();
                let (agent_type, model_id) = session_info
                    .get(&session_id)
                    .copied()
                    .unwrap_or(("unknown", "unknown"));
                TimelineMessage {
                    session_id,
                    role,
                    content,
                    agent_type: agent_type.to_owned(),
                    model_id: model_id.to_owned(),
                    timestamp: created_at,
                }
            })
            .collect();

        // 3. Get activity log entries
        let q = ActivityQuery {
            project_id: Some(project_id),
            task_id: Some(task.id),
            event_type: None,
            actor_role: None,
            from_time: None,
            to_time: None,
            limit: 500,
            offset: 0,
        };
        let activity = match task_repo.query_activity(q).await {
            Ok(entries) => entries
                .iter()
                .map(|e| TimelineActivity {
                    event_type: e.event_type.clone(),
                    payload: super::json_object::AnyJson(
                        serde_json::from_str(&e.payload).unwrap_or_else(|_| serde_json::json!({})),
                    ),
                    timestamp: e.created_at.clone(),
                })
                .collect(),
            Err(e) => return err(e.to_string()),
        };

        Json(TaskTimelineResponse {
            sessions: Some(sessions.into_iter().map(Into::into).collect()),
            messages: Some(messages),
            activity: Some(activity),
            error: None,
        })
    }
}
#[cfg(test)]
mod tests;
