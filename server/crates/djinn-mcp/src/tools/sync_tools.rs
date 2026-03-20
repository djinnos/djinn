// MCP tools for djinn/ namespace git sync (SYNC-01 through SYNC-05).

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::bridge::{ChannelStatus, SyncResult};
use crate::server::DjinnMcpServer;

// ── Param structs ─────────────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskSyncEnableParams {
    /// Absolute path to the project git repository.
    pub project: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskSyncDisableParams {
    /// Absolute path to the project git repository.
    pub project: String,
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

#[derive(Serialize, schemars::JsonSchema)]
pub struct SyncChannelResult {
    pub channel: String,
    pub ok: bool,
    #[schemars(with = "Option<i64>")]
    pub count: Option<usize>,
    pub error: Option<String>,
}

impl From<SyncResult> for SyncChannelResult {
    fn from(value: SyncResult) -> Self {
        Self {
            channel: value.channel,
            ok: value.ok,
            count: value.count,
            error: value.error,
        }
    }
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SyncChannelStatus {
    pub name: String,
    pub branch: String,
    pub enabled: bool,
    /// Sync-enabled project paths (SYNC-07).
    pub project_paths: Vec<String>,
    pub last_synced_at: Option<String>,
    pub last_error: Option<String>,
    #[schemars(with = "i64")]
    pub failure_count: u32,
    #[schemars(with = "i64")]
    pub backoff_secs: u64,
    /// Whether the channel needs attention (3+ failures) (SYNC-16).
    pub needs_attention: bool,
}

impl From<ChannelStatus> for SyncChannelStatus {
    fn from(value: ChannelStatus) -> Self {
        Self {
            name: value.name,
            branch: value.branch,
            enabled: value.enabled,
            project_paths: value.project_paths,
            last_synced_at: value.last_synced_at,
            last_error: value.last_error,
            failure_count: value.failure_count,
            backoff_secs: value.backoff_secs,
            needs_attention: value.needs_attention,
        }
    }
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TaskSyncEnableResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<i64>")]
    pub tasks_exported: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TaskSyncDisableResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_wide: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TaskSyncRunResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<Vec<SyncChannelResult>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TaskSyncStatusResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<Vec<SyncChannelStatus>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
    ) -> Json<TaskSyncEnableResponse> {
        let project_id = match self.project_id_for_path(&p.project).await {
            Some(id) => id,
            None => {
                return Json(TaskSyncEnableResponse {
                    ok: None,
                    tasks_exported: None,
                    note: None,
                    error: Some(format!("project not found: {}", p.project)),
                });
            }
        };

        let mgr = self.state.sync_manager();
        if let Err(e) = mgr.enable_project(&project_id).await {
            return Json(TaskSyncEnableResponse {
                ok: None,
                tasks_exported: None,
                note: None,
                error: Some(e.to_string()),
            });
        }

        // Trigger an initial export.
        let uid = self.state.sync_user_id();
        let results = mgr.export_all(Some(uid)).await;

        match results.into_iter().find(|r| r.channel == "tasks") {
            Some(r) if r.ok => Json(TaskSyncEnableResponse {
                ok: Some(true),
                tasks_exported: Some(r.count.unwrap_or(0)),
                note: None,
                error: None,
            }),
            Some(r) => Json(TaskSyncEnableResponse {
                ok: Some(false),
                tasks_exported: None,
                error: Some(r.error.unwrap_or_default()),
                note: Some(
                    "sync enabled but initial export failed; will retry automatically".to_string(),
                ),
            }),
            None => Json(TaskSyncEnableResponse {
                ok: Some(true),
                tasks_exported: None,
                note: Some("sync enabled; no tasks to export".to_string()),
                error: None,
            }),
        }
    }

    /// Disable task sync for a project. Stops push/pull
    /// without deleting the remote branch (unless team_wide=true).
    #[tool(
        description = "Disable task sync for a project. Stops push/pull without deleting the remote branch (unless team_wide=true)."
    )]
    pub async fn task_sync_disable(
        &self,
        Parameters(p): Parameters<TaskSyncDisableParams>,
    ) -> Json<TaskSyncDisableResponse> {
        let project_id = match self.project_id_for_path(&p.project).await {
            Some(id) => id,
            None => {
                return Json(TaskSyncDisableResponse {
                    ok: None,
                    team_wide: None,
                    error: Some(format!("project not found: {}", p.project)),
                });
            }
        };

        let mgr = self.state.sync_manager();
        let team_wide = p.team_wide.unwrap_or(false);

        if team_wide {
            let project_path = std::path::PathBuf::from(&p.project);
            if let Err(e) = mgr.delete_remote_branch("tasks", &project_path).await {
                tracing::warn!(
                    error = %e,
                    "failed to delete remote djinn/tasks branch; disabling locally anyway"
                );
            }
        }

        if let Err(e) = mgr.disable_project(&project_id).await {
            return Json(TaskSyncDisableResponse {
                ok: None,
                team_wide: None,
                error: Some(e.to_string()),
            });
        }

        Json(TaskSyncDisableResponse {
            ok: Some(true),
            team_wide: Some(team_wide),
            error: None,
        })
    }

    /// Export task state to git sync branch.
    #[tool(description = "Export task state to git sync branch")]
    pub async fn task_sync_export(
        &self,
        Parameters(p): Parameters<TaskSyncExportParams>,
    ) -> Json<TaskSyncRunResponse> {
        if let Some(path) = p.project.as_ref()
            && self.project_id_for_path(path).await.is_none()
        {
            return Json(TaskSyncRunResponse {
                ok: None,
                channels: None,
                error: Some(format!("project not found: {path}")),
            });
        }
        let mgr = self.state.sync_manager();
        let uid = self.state.sync_user_id();
        let results = mgr.export_all(Some(uid)).await;

        if results.is_empty() {
            return Json(TaskSyncRunResponse {
                ok: Some(false),
                channels: None,
                error: Some("sync not enabled — call task_sync_enable first".to_string()),
            });
        }

        let all_ok = results.iter().all(|r| r.ok);
        Json(TaskSyncRunResponse {
            ok: Some(all_ok),
            channels: Some(results.into_iter().map(SyncChannelResult::from).collect()),
            error: None,
        })
    }

    /// Import task state from git sync branch.
    #[tool(description = "Import task state from git sync branch")]
    pub async fn task_sync_import(
        &self,
        Parameters(p): Parameters<TaskSyncImportParams>,
    ) -> Json<TaskSyncRunResponse> {
        if let Some(path) = p.project.as_ref()
            && self.project_id_for_path(path).await.is_none()
        {
            return Json(TaskSyncRunResponse {
                ok: None,
                channels: None,
                error: Some(format!("project not found: {path}")),
            });
        }
        let mgr = self.state.sync_manager();
        let results = mgr.import_all().await;

        if results.is_empty() {
            return Json(TaskSyncRunResponse {
                ok: Some(false),
                channels: None,
                error: Some("sync not enabled — call task_sync_enable first".to_string()),
            });
        }

        let all_ok = results.iter().all(|r| r.ok);
        Json(TaskSyncRunResponse {
            ok: Some(all_ok),
            channels: Some(results.into_iter().map(SyncChannelResult::from).collect()),
            error: None,
        })
    }

    /// Show full sync health status including backoff state and pending export count.
    #[tool(
        description = "Show full sync health status including backoff state and pending export count"
    )]
    pub async fn task_sync_status(
        &self,
        Parameters(p): Parameters<TaskSyncStatusParams>,
    ) -> Json<TaskSyncStatusResponse> {
        if let Some(path) = p.project.as_ref()
            && self.project_id_for_path(path).await.is_none()
        {
            return Json(TaskSyncStatusResponse {
                channels: None,
                error: Some(format!("project not found: {path}")),
            });
        }
        let channels = self.state.sync_manager().status().await;
        Json(TaskSyncStatusResponse {
            channels: Some(channels.into_iter().map(SyncChannelStatus::from).collect()),
            error: None,
        })
    }
}
