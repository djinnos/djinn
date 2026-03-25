use crate::tools::json_object::AnyJson;
use djinn_core::models::Agent;
use djinn_db::repositories::agent::ExtractionQualityMetrics as DbExtractionQualityMetrics;
use djinn_db::{
    AgentCreateInput, AgentListQuery, AgentMetrics as DbAgentMetrics, AgentRepository,
    VALID_BASE_ROLES,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AgentModel {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub base_role: String,
    pub description: String,
    pub system_prompt_extensions: String,
    pub model_preference: Option<String>,
    pub verification_command: Option<String>,
    pub mcp_servers: Vec<AnyJson>,
    pub skills: Vec<AnyJson>,
    pub is_default: bool,
    /// Auto-improvement loop amendments. None if not yet set.
    pub learned_prompt: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<&Agent> for AgentModel {
    fn from(r: &Agent) -> Self {
        Self {
            id: r.id.clone(),
            project_id: r.project_id.clone(),
            name: r.name.clone(),
            base_role: r.base_role.clone(),
            description: r.description.clone(),
            system_prompt_extensions: r.system_prompt_extensions.clone(),
            model_preference: r.model_preference.clone(),
            verification_command: r.verification_command.clone(),
            mcp_servers: parse_json_array_any(&r.mcp_servers),
            skills: parse_json_array_any(&r.skills),
            is_default: r.is_default,
            learned_prompt: r.learned_prompt.clone(),
            created_at: r.created_at.clone(),
            updated_at: r.updated_at.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AgentSingleResponse {
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AgentCreateParams {
    /// Absolute project path.
    pub project: String,
    /// Unique agent name within the project.
    pub name: String,
    /// Base role to extend. One of: worker, lead, planner, architect, reviewer, resolver.
    pub base_role: String,
    pub description: Option<String>,
    /// Additional system prompt content appended to the base role prompt.
    pub system_prompt_extensions: Option<String>,
    /// Preferred model ID (falls back to project default).
    pub model_preference: Option<String>,
    /// Custom verification command (falls back to project default).
    pub verification_command: Option<String>,
    /// Additional MCP server refs for this agent.
    pub mcp_servers: Option<Vec<AnyJson>>,
    /// Skills (prompt templates) available to this agent.
    pub skills: Option<Vec<AnyJson>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AgentMetricEntry {
    pub agent_id: String,
    pub agent_name: String,
    pub base_role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub learned_prompt: Option<String>,
    pub success_rate: f64,
    pub avg_tokens: f64,
    pub avg_time_seconds: f64,
    pub verification_pass_rate: f64,
    pub avg_reopens: f64,
    pub completed_task_count: i64,
    pub extraction_quality: ExtractionQualityMetricEntry,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExtractionQualityMetricEntry {
    pub extracted: i64,
    pub dedup_skipped: i64,
    pub novelty_skipped: i64,
    pub written: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AgentMetricsResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<AgentMetricEntry>>,
    pub window_days: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AgentMetricsParams {
    pub project: String,
    pub agent_id: Option<String>,
    pub window_days: Option<i64>,
}

pub fn parse_json_array_any(raw: &str) -> Vec<AnyJson> {
    serde_json::from_str(raw).unwrap_or_default()
}

pub fn agent_not_found_error(id: &str) -> String {
    format!("agent not found: {id}")
}

pub fn validate_base_role(base_role: &str) -> Result<(), String> {
    if VALID_BASE_ROLES.contains(&base_role) {
        Ok(())
    } else {
        Err(format!(
            "invalid base_role '{}'; must be one of: {}",
            base_role,
            VALID_BASE_ROLES.join(", ")
        ))
    }
}

pub fn validate_agent_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("name must not be empty".to_string());
    }
    if trimmed.len() > 100 {
        return Err("name must be 100 characters or fewer".to_string());
    }
    Ok(trimmed.to_string())
}

pub async fn create_agent(
    repo: &AgentRepository,
    project_id: &str,
    params: AgentCreateParams,
) -> AgentSingleResponse {
    let name = match validate_agent_name(&params.name) {
        Ok(n) => n,
        Err(e) => {
            return AgentSingleResponse {
                agent: None,
                error: Some(e),
            };
        }
    };

    if let Err(e) = validate_base_role(&params.base_role) {
        return AgentSingleResponse {
            agent: None,
            error: Some(e),
        };
    }

    match repo.get_by_name_for_project(project_id, &name).await {
        Ok(Some(_)) => {
            return AgentSingleResponse {
                agent: None,
                error: Some(format!(
                    "an agent named '{name}' already exists in this project"
                )),
            };
        }
        Err(e) => {
            return AgentSingleResponse {
                agent: None,
                error: Some(e.to_string()),
            };
        }
        Ok(None) => {}
    }

    let mcp_servers_json = params
        .mcp_servers
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string()));
    let skills_json = params
        .skills
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string()));

    match repo
        .create_for_project(
            project_id,
            AgentCreateInput {
                name: &name,
                base_role: &params.base_role,
                description: params.description.as_deref().unwrap_or(""),
                system_prompt_extensions: params.system_prompt_extensions.as_deref().unwrap_or(""),
                model_preference: params.model_preference.as_deref(),
                verification_command: params.verification_command.as_deref(),
                mcp_servers: mcp_servers_json.as_deref(),
                skills: skills_json.as_deref(),
                is_default: false,
            },
        )
        .await
    {
        Ok(agent) => AgentSingleResponse {
            agent: Some(AgentModel::from(&agent)),
            error: None,
        },
        Err(e) => AgentSingleResponse {
            agent: None,
            error: Some(e.to_string()),
        },
    }
}

fn base_role_to_agent_type(base_role: &str) -> &str {
    match base_role {
        "worker" | "resolver" => "worker",
        "reviewer" => "reviewer",
        "planner" => "planner",
        "lead" => "lead",
        other => other,
    }
}

pub async fn metrics_for_agents(
    repo: &AgentRepository,
    project_id: &str,
    params: AgentMetricsParams,
) -> AgentMetricsResponse {
    let window_days = params.window_days.unwrap_or(30).max(1);
    let agent_id = params.agent_id.filter(|s| !s.trim().is_empty());

    let agents: Vec<Agent> = if let Some(ref id_or_name) = agent_id {
        match resolve_agent(repo, project_id, id_or_name).await {
            Ok(Some(r)) => vec![r],
            Ok(None) => {
                return AgentMetricsResponse {
                    agents: None,
                    window_days,
                    error: Some(agent_not_found_error(id_or_name)),
                };
            }
            Err(e) => {
                return AgentMetricsResponse {
                    agents: None,
                    window_days,
                    error: Some(e),
                };
            }
        }
    } else {
        match repo
            .list_for_project(AgentListQuery {
                project_id: project_id.to_string(),
                base_role: None,
                limit: 200,
                offset: 0,
            })
            .await
        {
            Ok(result) => result.agents,
            Err(e) => {
                return AgentMetricsResponse {
                    agents: None,
                    window_days,
                    error: Some(e.to_string()),
                };
            }
        }
    };

    let mut entries: Vec<AgentMetricEntry> = Vec::with_capacity(agents.len());

    for agent in &agents {
        let agent_type = base_role_to_agent_type(&agent.base_role);
        let m: DbAgentMetrics = repo
            .get_metrics(project_id, agent_type, window_days)
            .await
            .unwrap_or(DbAgentMetrics {
                success_rate: 0.0,
                avg_reopens: 0.0,
                verification_pass_rate: 0.0,
                completed_task_count: 0,
                avg_tokens: 0.0,
                avg_time_seconds: 0.0,
                extraction_quality: DbExtractionQualityMetrics::default(),
            });

        entries.push(AgentMetricEntry {
            agent_id: agent.id.clone(),
            agent_name: agent.name.clone(),
            base_role: agent.base_role.clone(),
            learned_prompt: agent.learned_prompt.clone(),
            success_rate: m.success_rate,
            avg_reopens: m.avg_reopens,
            verification_pass_rate: m.verification_pass_rate,
            completed_task_count: m.completed_task_count,
            avg_tokens: m.avg_tokens,
            avg_time_seconds: m.avg_time_seconds,
            extraction_quality: ExtractionQualityMetricEntry {
                extracted: m.extraction_quality.extracted,
                dedup_skipped: m.extraction_quality.dedup_skipped,
                novelty_skipped: m.extraction_quality.novelty_skipped,
                written: m.extraction_quality.written,
            },
        });
    }

    AgentMetricsResponse {
        agents: Some(entries),
        window_days,
        error: None,
    }
}

pub async fn resolve_agent(
    repo: &AgentRepository,
    project_id: &str,
    id_or_name: &str,
) -> Result<Option<Agent>, String> {
    if let Ok(Some(role)) = repo.get(id_or_name).await
        && role.project_id == project_id
    {
        return Ok(Some(role));
    }

    repo.get_by_name_for_project(project_id, id_or_name)
        .await
        .map_err(|e| e.to_string())
}
