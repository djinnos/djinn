// HTTP handlers for project-level MCP server and skill management.
// MCP servers are stored in `mcp.json` at the project root.
// Skills are stored as `.md` files in `.djinn/skills/`.

use std::collections::HashMap;
use std::path::Path;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    routing::{delete, get, put},
};
use serde::{Deserialize, Serialize};

use crate::server::AppState;
use djinn_db::ProjectRepository;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/project/mcp-servers", get(list_mcp_servers).post(create_mcp_server))
        .route("/project/mcp-servers/update", put(update_mcp_server))
        .route("/project/mcp-servers/delete", delete(delete_mcp_server))
        .route("/project/skills", get(list_skills).post(create_skill))
        .route("/project/skills/update", put(update_skill))
        .route("/project/skills/delete", delete(delete_skill))
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
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("project not found: {project_id}")))?;
    Ok(project.path)
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
    Json(body): Json<CreateMcpServerBody>,
) -> Result<Json<McpServerResponse>, (StatusCode, String)> {
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
    write_mcp_json(&project_path, &config)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
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
    Json(body): Json<UpdateMcpServerBody>,
) -> Result<Json<McpServerResponse>, (StatusCode, String)> {
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
    write_mcp_json(&project_path, &config)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
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
    Query(q): Query<DeleteMcpServerQuery>,
) -> Result<StatusCode, (StatusCode, String)> {
    let project_path = resolve_project_path(&state, &q.project_id).await?;
    let mut config = read_mcp_json(&project_path);
    if config.mcp_servers.remove(&q.name).is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("MCP server '{}' not found", q.name),
        ));
    }
    write_mcp_json(&project_path, &config)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Skills API ───────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct SkillResponse {
    name: String,
    description: Option<String>,
    content: String,
}

#[derive(Serialize)]
struct SkillListResponse {
    skills: Vec<SkillResponse>,
}

fn skills_dir(project_path: &str) -> std::path::PathBuf {
    Path::new(project_path).join(".djinn").join("skills")
}

fn read_skill_file(path: &Path) -> Option<(Option<String>, String)> {
    let content = std::fs::read_to_string(path).ok()?;
    // Parse YAML frontmatter
    if let Some(rest) = content.strip_prefix("---") {
        if let Some(end_idx) = rest.find("---") {
            let frontmatter = &rest[..end_idx];
            let body = rest[end_idx + 3..].trim_start_matches('\n');
            let description = frontmatter
                .lines()
                .find(|l| l.starts_with("description:"))
                .map(|l| l["description:".len()..].trim().to_string());
            return Some((description, body.to_string()));
        }
    }
    Some((None, content))
}

async fn list_skills(
    State(state): State<AppState>,
    Query(q): Query<ProjectQuery>,
) -> Result<Json<SkillListResponse>, (StatusCode, String)> {
    let project_path = resolve_project_path(&state, &q.project_id).await?;
    let dir = skills_dir(&project_path);
    let mut skills = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name_str = file_name.to_string_lossy();

            if entry.path().is_dir() {
                let skill_md = entry.path().join("SKILL.md");
                if let Some((description, content)) = read_skill_file(&skill_md) {
                    skills.push(SkillResponse {
                        name: name_str.to_string(),
                        description,
                        content,
                    });
                }
            } else if let Some(stem) = name_str.strip_suffix(".md") {
                if let Some((description, content)) = read_skill_file(&entry.path()) {
                    skills.push(SkillResponse {
                        name: stem.to_string(),
                        description,
                        content,
                    });
                }
            }
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(SkillListResponse { skills }))
}

#[derive(Deserialize)]
struct CreateSkillBody {
    project_id: String,
    name: String,
    description: Option<String>,
    content: String,
}

async fn create_skill(
    State(state): State<AppState>,
    Json(body): Json<CreateSkillBody>,
) -> Result<Json<SkillResponse>, (StatusCode, String)> {
    let project_path = resolve_project_path(&state, &body.project_id).await?;
    let dir = skills_dir(&project_path);
    let file_path = dir.join(format!("{}.md", body.name));

    if file_path.exists() {
        return Err((
            StatusCode::CONFLICT,
            format!("Skill '{}' already exists", body.name),
        ));
    }

    std::fs::create_dir_all(&dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to create skills dir: {e}")))?;

    let file_content = format_skill_file(body.description.as_deref(), &body.content);
    std::fs::write(&file_path, &file_content)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to write skill: {e}")))?;

    Ok(Json(SkillResponse {
        name: body.name,
        description: body.description,
        content: body.content,
    }))
}

#[derive(Deserialize)]
struct UpdateSkillBody {
    project_id: String,
    name: String,
    description: Option<String>,
    content: String,
}

async fn update_skill(
    State(state): State<AppState>,
    Json(body): Json<UpdateSkillBody>,
) -> Result<Json<SkillResponse>, (StatusCode, String)> {
    let project_path = resolve_project_path(&state, &body.project_id).await?;
    let dir = skills_dir(&project_path);
    let file_path = dir.join(format!("{}.md", body.name));

    if !file_path.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("Skill '{}' not found", body.name),
        ));
    }

    let file_content = format_skill_file(body.description.as_deref(), &body.content);
    std::fs::write(&file_path, &file_content)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to write skill: {e}")))?;

    Ok(Json(SkillResponse {
        name: body.name,
        description: body.description,
        content: body.content,
    }))
}

#[derive(Deserialize)]
struct DeleteSkillQuery {
    project_id: String,
    name: String,
}

async fn delete_skill(
    State(state): State<AppState>,
    Query(q): Query<DeleteSkillQuery>,
) -> Result<StatusCode, (StatusCode, String)> {
    let project_path = resolve_project_path(&state, &q.project_id).await?;
    let dir = skills_dir(&project_path);
    let file_path = dir.join(format!("{}.md", q.name));

    if !file_path.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("Skill '{}' not found", q.name),
        ));
    }

    std::fs::remove_file(&file_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to delete skill: {e}")))?;

    Ok(StatusCode::NO_CONTENT)
}

fn format_skill_file(description: Option<&str>, content: &str) -> String {
    match description {
        Some(desc) if !desc.is_empty() => {
            format!("---\ndescription: {desc}\n---\n\n{content}\n")
        }
        _ => format!("{content}\n"),
    }
}
