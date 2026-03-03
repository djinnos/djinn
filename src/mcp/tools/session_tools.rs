use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::Deserialize;

use crate::db::repositories::session::SessionRepository;
use crate::db::repositories::task::TaskRepository;
use crate::mcp::server::DjinnMcpServer;

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SessionListParams {
    /// Task UUID or short_id.
    pub task_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SessionShowParams {
    /// Session UUID.
    pub id: String,
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
    ) -> Json<serde_json::Value> {
        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(task) = task_repo.resolve(&p.task_id).await.ok().flatten() else {
            return Json(serde_json::json!({ "error": format!("task not found: {}", p.task_id) }));
        };

        let repo = SessionRepository::new(self.state.db().clone(), self.state.events().clone());
        match repo.list_for_task(&task.id).await {
            Ok(sessions) => Json(serde_json::json!({ "task_id": task.id, "sessions": sessions })),
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// List currently running sessions across all tasks.
    #[tool(description = "session_active() returns all currently running sessions across tasks")]
    pub async fn session_active(&self) -> Json<serde_json::Value> {
        let repo = SessionRepository::new(self.state.db().clone(), self.state.events().clone());
        match repo.list_active().await {
            Ok(sessions) => Json(serde_json::json!({ "sessions": sessions })),
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Get a single session by id.
    #[tool(
        description = "session_show(id) returns session details: id, task_id, model_id, agent_type, started_at, ended_at, status, tokens_in, tokens_out"
    )]
    pub async fn session_show(
        &self,
        Parameters(p): Parameters<SessionShowParams>,
    ) -> Json<serde_json::Value> {
        let repo = SessionRepository::new(self.state.db().clone(), self.state.events().clone());
        match repo.get(&p.id).await {
            Ok(Some(session)) => Json(serde_json::json!(session)),
            Ok(None) => {
                Json(serde_json::json!({ "error": format!("session not found: {}", p.id) }))
            }
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        }
    }
}
