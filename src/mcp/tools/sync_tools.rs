// MCP tools for djinn/ namespace git sync (SYNC-01 through SYNC-05).

use std::path::PathBuf;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::Deserialize;

use crate::mcp::server::DjinnMcpServer;
use crate::mcp::tools::{ObjectJson, json_object};

// ── Param structs ─────────────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskSyncEnableParams {
    /// Absolute path to the project git repository.
    pub project: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskSyncDisableParams {
    /// If true, delete the remote `djinn/tasks` branch (team-wide disable).
    /// If false or absent, only clear the local enabled flag (machine opt-out).
    pub team_wide: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskSyncExportParams {
    /// Project path (accepted for API compatibility, currently unused).
    pub project: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskSyncImportParams {
    /// Project path (accepted for API compatibility, currently unused).
    pub project: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskSyncStatusParams {
    /// Project path (accepted for API compatibility, currently unused).
    pub project: Option<String>,
}

// ── Tool implementations ──────────────────────────────────────────────────────

#[tool_router(router = sync_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Enable task sync: creates djinn/tasks branch if needed, exports tasks,
    /// and pushes to remote. Requires a git remote.
    #[tool(
        description = "Enable task sync: creates djinn/tasks branch if needed, exports tasks, and pushes to remote. Requires a git remote."
    )]
    pub async fn task_sync_enable(
        &self,
        Parameters(p): Parameters<TaskSyncEnableParams>,
    ) -> Json<ObjectJson> {
        let project = PathBuf::from(&p.project);
        if !project.exists() {
            return json_object(serde_json::json!({
                "error": format!("project path not found: {}", p.project)
            }));
        }

        let mgr = self.state.sync_manager();
        if let Err(e) = mgr.enable("tasks", &project).await {
            return json_object(serde_json::json!({ "error": e.to_string() }));
        }

        // Trigger an initial export.
        let uid = self.state.sync_user_id();
        let results = mgr.export_all(Some(uid)).await;

        match results.into_iter().find(|r| r.channel == "tasks") {
            Some(r) if r.ok => json_object(serde_json::json!({
                "ok": true,
                "tasks_exported": r.count.unwrap_or(0),
            })),
            Some(r) => json_object(serde_json::json!({
                "ok": false,
                "error": r.error.unwrap_or_default(),
                "note": "sync enabled but initial export failed; will retry automatically",
            })),
            None => json_object(serde_json::json!({
                "ok": true,
                "note": "sync enabled; no tasks to export",
            })),
        }
    }

    /// Disable task sync for this machine (personal opt-out). Stops push/pull
    /// without deleting the remote branch.
    #[tool(
        description = "Disable task sync for this machine (personal opt-out). Stops push/pull without deleting the remote branch."
    )]
    pub async fn task_sync_disable(
        &self,
        Parameters(p): Parameters<TaskSyncDisableParams>,
    ) -> Json<ObjectJson> {
        let mgr = self.state.sync_manager();
        let team_wide = p.team_wide.unwrap_or(false);

        if team_wide {
            // Best-effort: delete remote branch. Log warning on failure but
            // continue with local disable regardless (SYNC-04).
            if let Err(e) = mgr.delete_remote_branch("tasks").await {
                tracing::warn!(
                    error = %e,
                    "failed to delete remote djinn/tasks branch; disabling locally anyway"
                );
            }
        }

        if let Err(e) = mgr.disable("tasks").await {
            return json_object(serde_json::json!({ "error": e.to_string() }));
        }

        json_object(serde_json::json!({ "ok": true, "team_wide": team_wide }))
    }

    /// Export task state to git sync branch.
    #[tool(description = "Export task state to git sync branch")]
    pub async fn task_sync_export(
        &self,
        Parameters(_p): Parameters<TaskSyncExportParams>,
    ) -> Json<ObjectJson> {
        let mgr = self.state.sync_manager();
        let uid = self.state.sync_user_id();
        let results = mgr.export_all(Some(uid)).await;

        if results.is_empty() {
            return json_object(serde_json::json!({
                "ok": false,
                "error": "sync not enabled — call task_sync_enable first"
            }));
        }

        let all_ok = results.iter().all(|r| r.ok);
        json_object(serde_json::json!({ "ok": all_ok, "channels": results }))
    }

    /// Import task state from git sync branch.
    #[tool(description = "Import task state from git sync branch")]
    pub async fn task_sync_import(
        &self,
        Parameters(_p): Parameters<TaskSyncImportParams>,
    ) -> Json<ObjectJson> {
        let mgr = self.state.sync_manager();
        let results = mgr.import_all().await;

        if results.is_empty() {
            return json_object(serde_json::json!({
                "ok": false,
                "error": "sync not enabled — call task_sync_enable first"
            }));
        }

        let all_ok = results.iter().all(|r| r.ok);
        json_object(serde_json::json!({ "ok": all_ok, "channels": results }))
    }

    /// Show full sync health status including backoff state and pending export count.
    #[tool(
        description = "Show full sync health status including backoff state and pending export count"
    )]
    pub async fn task_sync_status(
        &self,
        Parameters(_p): Parameters<TaskSyncStatusParams>,
    ) -> Json<ObjectJson> {
        let channels = self.state.sync_manager().status().await;
        json_object(serde_json::json!({ "channels": channels }))
    }
}
