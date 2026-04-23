// HTTP handlers for the /agents REST endpoints consumed by the desktop frontend.

use std::collections::BTreeSet;
use std::path::Path as StdPath;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, put},
};
use serde::{Deserialize, Serialize};

use crate::server::AppState;
use djinn_core::models::Agent;
use djinn_db::repositories::agent::ExtractionQualityMetrics as DbExtractionQualityMetrics;
use djinn_db::{
    AgentCreateInput, AgentListQuery, AgentMetrics as DbAgentMetrics, AgentRepository,
    AgentUpdateInput, ProjectRepository,
};

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/agents", get(list_agents).post(create_agent))
        // /agents/metrics must be registered before /agents/:id to avoid being
        // captured as a path parameter.
        .route("/agents/metrics", get(agent_metrics))
        .route("/agents/available-mcp-servers", get(available_mcp_servers))
        .route("/agents/available-skills", get(available_skills))
        .route(
            "/agents/{id}/learned-prompt/history",
            get(learned_prompt_history),
        )
        .route("/agents/{id}/learned-prompt", delete(clear_learned_prompt))
        .route("/agents/{id}", put(update_agent).delete(delete_agent))
}

// ── Serialisation helpers ─────────────────────────────────────────────────────

/// Split a newline-delimited DB string into a `Vec<String>`, dropping blank lines.
fn split_extensions(s: &str) -> Vec<String> {
    s.lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn now_rfc3339() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

// ── Role response ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct AgentResponse {
    id: String,
    project_id: String,
    name: String,
    base_role: String,
    description: String,
    system_prompt_extensions: Vec<String>,
    mcp_servers: Vec<String>,
    skills: Vec<String>,
    model_preference: Option<String>,
    verification_command: Option<String>,
    is_default: bool,
    learned_prompt: Option<String>,
    created_at: String,
    updated_at: String,
}

/// Parse a JSON array string (e.g. `'["a","b"]'`) into `Vec<String>`.
/// Returns an empty vec on empty/invalid input.
fn parse_json_string_array(s: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(s).unwrap_or_default()
}

impl From<&Agent> for AgentResponse {
    fn from(r: &Agent) -> Self {
        Self {
            id: r.id.clone(),
            project_id: r.project_id.clone(),
            name: r.name.clone(),
            base_role: r.base_role.clone(),
            description: r.description.clone(),
            system_prompt_extensions: split_extensions(&r.system_prompt_extensions),
            mcp_servers: parse_json_string_array(&r.mcp_servers),
            skills: parse_json_string_array(&r.skills),
            model_preference: r.model_preference.clone(),
            verification_command: r.verification_command.clone(),
            is_default: r.is_default,
            learned_prompt: r.learned_prompt.clone(),
            created_at: r.created_at.clone(),
            updated_at: r.updated_at.clone(),
        }
    }
}

// ── GET /agents ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ListQuery {
    project_id: Option<String>,
}

#[derive(Serialize)]
struct ListResponse {
    agents: Vec<AgentResponse>,
}

async fn list_agents(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<ListResponse>, (StatusCode, String)> {
    let repo = AgentRepository::new(state.db().clone(), state.event_bus());
    let agents = if let Some(project_id) = q.project_id {
        repo.list_for_project(AgentListQuery {
            project_id,
            base_role: None,
            limit: 500,
            offset: 0,
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .agents
    } else {
        repo.list_all()
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    Ok(Json(ListResponse {
        agents: agents.iter().map(AgentResponse::from).collect(),
    }))
}

// ── POST /agents ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateBody {
    project_id: String,
    name: String,
    base_role: String,
    description: Option<String>,
    system_prompt_extensions: Option<Vec<String>>,
    mcp_servers: Option<Vec<String>>,
    skills: Option<Vec<String>>,
    model_preference: Option<String>,
    verification_command: Option<String>,
}

async fn create_agent(
    State(state): State<AppState>,
    Json(body): Json<CreateBody>,
) -> Result<Json<AgentResponse>, (StatusCode, String)> {
    let repo = AgentRepository::new(state.db().clone(), state.event_bus());
    let extensions = body.system_prompt_extensions.unwrap_or_default().join("\n");
    let mcp_servers_json = body
        .mcp_servers
        .map(|v| serde_json::to_string(&v).unwrap_or_else(|_| "[]".to_string()));
    let skills_json = body
        .skills
        .map(|v| serde_json::to_string(&v).unwrap_or_else(|_| "[]".to_string()));
    let role = repo
        .create_for_project(
            &body.project_id,
            AgentCreateInput {
                name: &body.name,
                base_role: &body.base_role,
                description: body.description.as_deref().unwrap_or(""),
                system_prompt_extensions: &extensions,
                model_preference: body.model_preference.as_deref(),
                verification_command: body.verification_command.as_deref(),
                mcp_servers: mcp_servers_json.as_deref(),
                skills: skills_json.as_deref(),
                is_default: false,
            },
        )
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(AgentResponse::from(&role)))
}

// ── PUT /agents/:id ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct UpdateBody {
    name: Option<String>,
    description: Option<String>,
    system_prompt_extensions: Option<Vec<String>>,
    mcp_servers: Option<Vec<String>>,
    skills: Option<Vec<String>>,
    model_preference: Option<String>,
    verification_command: Option<String>,
}

async fn update_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateBody>,
) -> Result<Json<AgentResponse>, (StatusCode, String)> {
    let repo = AgentRepository::new(state.db().clone(), state.event_bus());
    let existing = repo
        .get(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("agent not found: {id}")))?;

    let name = body.name.as_deref().unwrap_or(&existing.name).to_string();
    let description = body
        .description
        .as_deref()
        .unwrap_or(&existing.description)
        .to_string();
    let extensions = body
        .system_prompt_extensions
        .map(|v| v.join("\n"))
        .unwrap_or_else(|| existing.system_prompt_extensions.clone());
    let mcp_servers_str = body
        .mcp_servers
        .map(|v| serde_json::to_string(&v).unwrap_or_else(|_| "[]".to_string()))
        .unwrap_or_else(|| existing.mcp_servers.clone());
    let skills_str = body
        .skills
        .map(|v| serde_json::to_string(&v).unwrap_or_else(|_| "[]".to_string()))
        .unwrap_or_else(|| existing.skills.clone());
    let model_preference = if body.model_preference.is_some() {
        body.model_preference.as_deref()
    } else {
        existing.model_preference.as_deref()
    };
    let verification_command = if body.verification_command.is_some() {
        body.verification_command.as_deref()
    } else {
        existing.verification_command.as_deref()
    };

    let updated = repo
        .update(
            &id,
            AgentUpdateInput {
                name: &name,
                description: &description,
                system_prompt_extensions: &extensions,
                model_preference,
                verification_command,
                mcp_servers: &mcp_servers_str,
                skills: &skills_str,
                learned_prompt: existing.learned_prompt.as_deref(),
            },
        )
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(AgentResponse::from(&updated)))
}

// ── DELETE /agents/:id ────────────────────────────────────────────────────────

async fn delete_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let repo = AgentRepository::new(state.db().clone(), state.event_bus());
    let role = repo
        .get(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("agent not found: {id}")))?;
    repo.delete(&id, &role.project_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

// ── GET /agents/metrics ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct MetricsQuery {
    project_id: Option<String>,
}

#[derive(Serialize)]
struct AgentMetricsItem {
    agent_id: String,
    agent_name: String,
    base_role: String,
    is_default: bool,
    task_count: i64,
    success_rate: Option<f64>,
    avg_token_usage: Option<f64>,
    avg_tokens_in: Option<f64>,
    avg_tokens_out: Option<f64>,
    avg_time_to_complete_seconds: Option<f64>,
    verification_pass_rate: Option<f64>,
    reopen_rate: Option<f64>,
    success_rate_trend: Option<f64>,
    history: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct AgentMetricsResponse {
    metrics: Vec<AgentMetricsItem>,
    generated_at: String,
}

fn base_role_to_agent_type(base_role: &str) -> &str {
    match base_role {
        "worker" => "worker",
        "reviewer" => "reviewer",
        "planner" => "planner",
        "lead" => "lead",
        other => other,
    }
}

async fn agent_metrics(
    State(state): State<AppState>,
    Query(q): Query<MetricsQuery>,
) -> Result<Json<AgentMetricsResponse>, (StatusCode, String)> {
    let generated_at = now_rfc3339();

    let Some(project_id) = q.project_id else {
        return Ok(Json(AgentMetricsResponse {
            metrics: vec![],
            generated_at,
        }));
    };

    let repo = AgentRepository::new(state.db().clone(), state.event_bus());
    let agents = repo
        .list_for_project(AgentListQuery {
            project_id: project_id.clone(),
            base_role: None,
            limit: 500,
            offset: 0,
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .agents;

    let mut metrics = Vec::with_capacity(agents.len());
    for agent in &agents {
        let agent_type = base_role_to_agent_type(&agent.base_role);
        let m = repo
            .get_metrics(&project_id, agent_type, 30)
            .await
            .unwrap_or(DbAgentMetrics {
                success_rate: 0.0,
                avg_reopens: 0.0,
                verification_pass_rate: 0.0,
                completed_task_count: 0,
                avg_tokens: 0.0,
                avg_tokens_in: 0.0,
                avg_tokens_out: 0.0,
                avg_time_seconds: 0.0,
                extraction_quality: DbExtractionQualityMetrics::default(),
            });
        let has_data = m.completed_task_count > 0;
        metrics.push(AgentMetricsItem {
            agent_id: agent.id.clone(),
            agent_name: agent.name.clone(),
            base_role: agent.base_role.clone(),
            is_default: agent.is_default,
            task_count: m.completed_task_count,
            success_rate: has_data.then_some(m.success_rate),
            avg_token_usage: has_data.then_some(m.avg_tokens),
            avg_tokens_in: has_data.then_some(m.avg_tokens_in),
            avg_tokens_out: has_data.then_some(m.avg_tokens_out),
            avg_time_to_complete_seconds: has_data.then_some(m.avg_time_seconds),
            verification_pass_rate: has_data.then_some(m.verification_pass_rate),
            reopen_rate: has_data.then_some(m.avg_reopens),
            success_rate_trend: None,
            history: vec![],
        });
    }

    Ok(Json(AgentMetricsResponse {
        metrics,
        generated_at,
    }))
}

// ── GET /agents/available-mcp-servers ─────────────────────────────────────────

#[derive(Serialize)]
struct AvailableMcpServer {
    name: String,
    transport: String,
}

#[derive(Serialize)]
struct AvailableMcpServersResponse {
    servers: Vec<AvailableMcpServer>,
}

async fn available_mcp_servers(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<AvailableMcpServersResponse>, (StatusCode, String)> {
    let Some(project_id) = q.project_id else {
        return Ok(Json(AvailableMcpServersResponse { servers: vec![] }));
    };
    let project_repo = ProjectRepository::new(state.db().clone(), state.event_bus());
    let project = project_repo
        .get(&project_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("project not found: {project_id}"),
            )
        })?;

    let project_root =
        djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
    let registry =
        djinn_agent::verification::settings::load_mcp_server_registry(&project_root);
    let servers = registry
        .into_iter()
        .map(|(name, config)| {
            let transport = if config.url.is_some() {
                "http".to_string()
            } else {
                "stdio".to_string()
            };
            AvailableMcpServer { name, transport }
        })
        .collect::<Vec<_>>();

    Ok(Json(AvailableMcpServersResponse { servers }))
}

// ── GET /agents/available-skills ─────────────────────────────────────────────

#[derive(Serialize)]
struct AvailableSkill {
    name: String,
    description: Option<String>,
}

#[derive(Serialize)]
struct AvailableSkillsResponse {
    skills: Vec<AvailableSkill>,
}

/// Discover skill names from the standard search directories.
fn discover_skill_names(project_root: &StdPath) -> Vec<String> {
    let mut names = BTreeSet::new();
    let dirs = [
        project_root.join(".claude").join("skills"),
        project_root.join(".opencode").join("skills"),
        project_root.join(".djinn").join("skills"),
    ];
    for dir in &dirs {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                let name_str = file_name.to_string_lossy();
                if entry.path().is_dir() {
                    // Directory-based skill — name is the dir name
                    if entry.path().join("SKILL.md").exists() {
                        names.insert(name_str.to_string());
                    }
                } else if let Some(stem) = name_str.strip_suffix(".md") {
                    // Flat file skill
                    names.insert(stem.to_string());
                }
            }
        }
    }
    names.into_iter().collect()
}

async fn available_skills(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<AvailableSkillsResponse>, (StatusCode, String)> {
    let Some(project_id) = q.project_id else {
        return Ok(Json(AvailableSkillsResponse { skills: vec![] }));
    };
    let project_repo = ProjectRepository::new(state.db().clone(), state.event_bus());
    let project = project_repo
        .get(&project_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("project not found: {project_id}"),
            )
        })?;

    let project_root =
        djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
    let skill_names = discover_skill_names(&project_root);

    // Load each discovered skill to get its description
    let resolved = djinn_agent::skills::load_skills(&project_root, &skill_names);
    let mut skills: Vec<AvailableSkill> = resolved
        .into_iter()
        .map(|s| AvailableSkill {
            name: s.name,
            description: if s.description.is_empty() {
                None
            } else {
                Some(s.description)
            },
        })
        .collect();

    // Include any names that failed to load (no description)
    let loaded: BTreeSet<String> = skills.iter().map(|s| s.name.clone()).collect();
    for name in skill_names {
        if !loaded.contains(&name) {
            skills.push(AvailableSkill {
                name,
                description: None,
            });
        }
    }

    Ok(Json(AvailableSkillsResponse { skills }))
}

// ── GET /agents/:id/learned-prompt/history ────────────────────────────────────

#[derive(Serialize)]
struct AmendmentResponse {
    id: String,
    proposed_text: String,
    action: String,
    metrics_before: serde_json::Value,
    metrics_after: serde_json::Value,
    created_at: String,
}

#[derive(Serialize)]
struct LearnedPromptHistoryResponse {
    learned_prompt: Option<String>,
    amendments: Vec<AmendmentResponse>,
}

async fn learned_prompt_history(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<LearnedPromptHistoryResponse>, (StatusCode, String)> {
    let repo = AgentRepository::new(state.db().clone(), state.event_bus());
    let role = repo
        .get(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("agent not found: {id}")))?;

    let entries = repo
        .get_history(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let amendments = entries
        .into_iter()
        .map(|e| AmendmentResponse {
            id: e.id,
            proposed_text: e.proposed_text,
            action: e.action,
            metrics_before: e
                .metrics_before
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(serde_json::Value::Object(Default::default())),
            metrics_after: e
                .metrics_after
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(serde_json::Value::Object(Default::default())),
            created_at: e.created_at,
        })
        .collect();

    Ok(Json(LearnedPromptHistoryResponse {
        learned_prompt: role.learned_prompt,
        amendments,
    }))
}

// ── DELETE /agents/:id/learned-prompt ────────────────────────────────────────

async fn clear_learned_prompt(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let repo = AgentRepository::new(state.db().clone(), state.event_bus());
    repo.clear_learned_prompt(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}
