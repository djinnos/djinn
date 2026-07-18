// HTTP handlers for project-level MCP server management.
// MCP servers are stored in `mcp.json` at the project root.

use std::collections::HashMap;
use std::path::Path;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, put},
};
use serde::{Deserialize, Serialize};

use crate::server::AppState;
use crate::server::auth::require_admin;
use djinn_db::ProjectRepository;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/project/mcp-servers",
            get(list_mcp_servers).post(create_mcp_server),
        )
        .route("/project/mcp-servers/update", put(update_mcp_server))
        .route("/project/mcp-servers/delete", delete(delete_mcp_server))
        .route(
            "/project/mcp-defaults",
            get(get_mcp_defaults).put(set_mcp_defaults),
        )
}

// ── Helpers ──────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ProjectQuery {
    project_id: String,
}

async fn resolve_project_path(
    state: &AppState,
    project_id: &str,
) -> Result<String, (StatusCode, String)> {
    let repo = ProjectRepository::new(state.db().clone(), state.event_bus());
    let project = repo
        .get(project_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("project not found: {project_id}"),
            )
        })?;
    Ok(
        djinn_core::paths::project_dir(&project.github_owner, &project.github_repo)
            .to_string_lossy()
            .into_owned(),
    )
}

// ── MCP Server JSON format ───────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct McpJsonFile {
    #[serde(default)]
    mcp_servers: HashMap<String, McpServerEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
struct McpServerEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    env: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    headers: HashMap<String, String>,
}

fn read_mcp_json(project_path: &str) -> McpJsonFile {
    let path = Path::new(project_path).join("mcp.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

fn write_mcp_json(project_path: &str, config: &McpJsonFile) -> Result<(), String> {
    let path = Path::new(project_path).join("mcp.json");
    let content =
        serde_json::to_string_pretty(config).map_err(|e| format!("JSON serialize error: {e}"))?;
    std::fs::write(&path, content).map_err(|e| format!("Failed to write mcp.json: {e}"))
}

// ── MCP Server API ───────────────────────────────────────────────────────────

#[derive(Serialize)]
struct McpServerResponse {
    name: String,
    url: Option<String>,
    command: Option<String>,
    args: Vec<String>,
    env: HashMap<String, String>,
}

#[derive(Serialize)]
struct McpServerListResponse {
    servers: Vec<McpServerResponse>,
}

async fn list_mcp_servers(
    State(state): State<AppState>,
    Query(q): Query<ProjectQuery>,
) -> Result<Json<McpServerListResponse>, (StatusCode, String)> {
    let project_path = resolve_project_path(&state, &q.project_id).await?;
    let config = read_mcp_json(&project_path);
    let mut servers: Vec<McpServerResponse> = config
        .mcp_servers
        .into_iter()
        .map(|(name, entry)| McpServerResponse {
            name,
            url: entry.url,
            command: entry.command,
            args: entry.args,
            env: entry.env,
        })
        .collect();
    servers.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(McpServerListResponse { servers }))
}

#[derive(Deserialize)]
struct CreateMcpServerBody {
    project_id: String,
    name: String,
    url: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

async fn create_mcp_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateMcpServerBody>,
) -> Result<Json<McpServerResponse>, (StatusCode, String)> {
    require_admin(&state, &headers).await?;
    let project_path = resolve_project_path(&state, &body.project_id).await?;
    let mut config = read_mcp_json(&project_path);
    if config.mcp_servers.contains_key(&body.name) {
        return Err((
            StatusCode::CONFLICT,
            format!("MCP server '{}' already exists", body.name),
        ));
    }
    let entry = McpServerEntry {
        url: body.url.clone(),
        command: body.command.clone(),
        args: body.args.clone(),
        env: body.env.clone(),
        headers: HashMap::new(),
    };
    config.mcp_servers.insert(body.name.clone(), entry);
    write_mcp_json(&project_path, &config).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(McpServerResponse {
        name: body.name,
        url: body.url,
        command: body.command,
        args: body.args,
        env: body.env,
    }))
}

#[derive(Deserialize)]
struct UpdateMcpServerBody {
    project_id: String,
    name: String,
    url: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

async fn update_mcp_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<UpdateMcpServerBody>,
) -> Result<Json<McpServerResponse>, (StatusCode, String)> {
    require_admin(&state, &headers).await?;
    let project_path = resolve_project_path(&state, &body.project_id).await?;
    let mut config = read_mcp_json(&project_path);
    if !config.mcp_servers.contains_key(&body.name) {
        return Err((
            StatusCode::NOT_FOUND,
            format!("MCP server '{}' not found", body.name),
        ));
    }
    let entry = McpServerEntry {
        url: body.url.clone(),
        command: body.command.clone(),
        args: body.args.clone(),
        env: body.env.clone(),
        headers: HashMap::new(),
    };
    config.mcp_servers.insert(body.name.clone(), entry);
    write_mcp_json(&project_path, &config).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(McpServerResponse {
        name: body.name,
        url: body.url,
        command: body.command,
        args: body.args,
        env: body.env,
    }))
}

#[derive(Deserialize)]
struct DeleteMcpServerQuery {
    project_id: String,
    name: String,
}

async fn delete_mcp_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DeleteMcpServerQuery>,
) -> Result<StatusCode, (StatusCode, String)> {
    require_admin(&state, &headers).await?;
    let project_path = resolve_project_path(&state, &q.project_id).await?;
    let mut config = read_mcp_json(&project_path);
    if config.mcp_servers.remove(&q.name).is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("MCP server '{}' not found", q.name),
        ));
    }
    write_mcp_json(&project_path, &config).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(StatusCode::NO_CONTENT)
}

// ── MCP Default Assignments API ──────────────────────────────────────────────
//
// Reads/writes the `agent_mcp_defaults` and `global_skills` fields in the
// project's `environment_config` Dolt column. All other `environment_config`
// fields (base, languages, workspaces, lifecycle, …) are
// preserved across writes.

use djinn_stack::environment::EnvironmentConfig;

async fn load_environment_config(
    state: &AppState,
    project_id: &str,
) -> Result<EnvironmentConfig, (StatusCode, String)> {
    let repo = ProjectRepository::new(state.db().clone(), state.event_bus());
    let raw_opt = repo
        .get_environment_config(project_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let Some(raw) = raw_opt else {
        return Ok(EnvironmentConfig::empty());
    };
    match serde_json::from_str::<EnvironmentConfig>(&raw) {
        Ok(cfg) if cfg.schema_version == 0 => Ok(EnvironmentConfig::empty()),
        Ok(cfg) => Ok(cfg),
        Err(_) => Ok(EnvironmentConfig::empty()),
    }
}

async fn save_environment_config(
    state: &AppState,
    project_id: &str,
    config: &EnvironmentConfig,
) -> Result<(), (StatusCode, String)> {
    let repo = ProjectRepository::new(state.db().clone(), state.event_bus());
    let raw = serde_json::to_string(config).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("JSON serialize error: {e}"),
        )
    })?;
    repo.set_environment_config(project_id, &raw)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

#[derive(Serialize)]
struct McpDefaultsResponse {
    /// Per-agent-name defaults, e.g. {"chat": ["Tavilly"], "*": ["web-search"]}
    agent_mcp_defaults: HashMap<String, Vec<String>>,
    /// Skills applied to all agents globally
    global_skills: Vec<String>,
}

async fn get_mcp_defaults(
    State(state): State<AppState>,
    Query(q): Query<ProjectQuery>,
) -> Result<Json<McpDefaultsResponse>, (StatusCode, String)> {
    let cfg = load_environment_config(&state, &q.project_id).await?;
    Ok(Json(McpDefaultsResponse {
        agent_mcp_defaults: cfg.agent_mcp_defaults.into_iter().collect(),
        global_skills: cfg.global_skills,
    }))
}

#[derive(Deserialize)]
struct SetMcpDefaultsBody {
    project_id: String,
    agent_mcp_defaults: HashMap<String, Vec<String>>,
    global_skills: Vec<String>,
}

async fn set_mcp_defaults(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SetMcpDefaultsBody>,
) -> Result<Json<McpDefaultsResponse>, (StatusCode, String)> {
    require_admin(&state, &headers).await?;
    let mut cfg = load_environment_config(&state, &body.project_id).await?;
    cfg.agent_mcp_defaults = body.agent_mcp_defaults.clone().into_iter().collect();
    cfg.global_skills = body.global_skills.clone();
    save_environment_config(&state, &body.project_id, &cfg).await?;
    Ok(Json(McpDefaultsResponse {
        agent_mcp_defaults: body.agent_mcp_defaults,
        global_skills: body.global_skills,
    }))
}
