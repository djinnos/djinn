// MCP tools for agent CRUD (agent_create, agent_update, agent_list, agent_show, agent_metrics).

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;
use crate::tools::json_object::AnyJson;
use crate::tools::validation::{validate_limit, validate_offset};
use djinn_core::models::Agent;
use djinn_db::AgentMetrics as DbAgentMetrics;
use djinn_db::{
    AgentCreateInput, AgentListQuery, AgentRepository, AgentUpdateInput, VALID_BASE_ROLES,
};

// ── View model ───────────────────────────────────────────────────────────────

#[derive(Serialize, schemars::JsonSchema)]
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

fn parse_json_array_any(raw: &str) -> Vec<AnyJson> {
    serde_json::from_str(raw).unwrap_or_default()
}

// ── Response types ────────────────────────────────────────────────────────────

#[derive(Serialize, schemars::JsonSchema)]
pub struct AgentSingleResponse {
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct AgentListResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<AgentModel>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn agent_not_found_error(id: &str) -> String {
    format!("agent not found: {id}")
}

fn validate_base_role(base_role: &str) -> Result<(), String> {
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

fn validate_agent_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("name must not be empty".to_string());
    }
    if trimmed.len() > 100 {
        return Err("name must be 100 characters or fewer".to_string());
    }
    Ok(trimmed.to_string())
}

// ── Param structs ─────────────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
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

#[derive(Deserialize, schemars::JsonSchema)]
pub struct AgentShowParams {
    /// Absolute project path.
    pub project: String,
    /// Agent UUID or name.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct AgentListParams {
    /// Absolute project path.
    pub project: String,
    /// Filter by base role: worker, lead, planner, architect, reviewer, resolver.
    pub base_role: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct AgentUpdateParams {
    /// Absolute project path.
    pub project: String,
    /// Agent UUID or name.
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub system_prompt_extensions: Option<String>,
    pub model_preference: Option<String>,
    pub verification_command: Option<String>,
    pub mcp_servers: Option<Vec<AnyJson>>,
    pub skills: Option<Vec<AnyJson>>,
    /// Set a new learned_prompt value (auto-improvement loop only).
    pub learned_prompt: Option<String>,
    /// Set to true to clear learned_prompt back to NULL. Takes precedence over learned_prompt.
    pub clear_learned_prompt: Option<bool>,
}

// ── Tool implementations ──────────────────────────────────────────────────────

#[tool_router(router = agent_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Create a specialist agent that extends a base role with domain-specific config.
    #[tool(
        description = "Create a specialist agent extending a base role (worker, lead, planner, architect, reviewer, resolver). Returns the created agent."
    )]
    pub async fn agent_create(
        &self,
        Parameters(p): Parameters<AgentCreateParams>,
    ) -> Json<AgentSingleResponse> {
        let name = match validate_agent_name(&p.name) {
            Ok(n) => n,
            Err(e) => {
                return Json(AgentSingleResponse {
                    agent: None,
                    error: Some(e),
                });
            }
        };
        if let Err(e) = validate_base_role(&p.base_role) {
            return Json(AgentSingleResponse {
                agent: None,
                error: Some(e),
            });
        }
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(AgentSingleResponse {
                    agent: None,
                    error: Some(e),
                });
            }
        };

        let repo = AgentRepository::new(self.state.db().clone(), self.state.event_bus());

        // Enforce name uniqueness within project.
        match repo.get_by_name_for_project(&project_id, &name).await {
            Ok(Some(_)) => {
                return Json(AgentSingleResponse {
                    agent: None,
                    error: Some(format!(
                        "an agent named '{name}' already exists in this project"
                    )),
                });
            }
            Err(e) => {
                return Json(AgentSingleResponse {
                    agent: None,
                    error: Some(e.to_string()),
                });
            }
            Ok(None) => {}
        }

        let mcp_servers_json = p
            .mcp_servers
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string()));
        let skills_json = p
            .skills
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string()));

        match repo
            .create_for_project(
                &project_id,
                AgentCreateInput {
                    name: &name,
                    base_role: &p.base_role,
                    description: p.description.as_deref().unwrap_or(""),
                    system_prompt_extensions: p.system_prompt_extensions.as_deref().unwrap_or(""),
                    model_preference: p.model_preference.as_deref(),
                    verification_command: p.verification_command.as_deref(),
                    mcp_servers: mcp_servers_json.as_deref(),
                    skills: skills_json.as_deref(),
                    is_default: false,
                },
            )
            .await
        {
            Ok(role) => Json(AgentSingleResponse {
                agent: Some(AgentModel::from(&role)),
                error: None,
            }),
            Err(e) => Json(AgentSingleResponse {
                agent: None,
                error: Some(e.to_string()),
            }),
        }
    }

    /// Show full details of an agent by UUID or name.
    #[tool(description = "Show full details of an agent. Accepts agent UUID or name.")]
    pub async fn agent_show(
        &self,
        Parameters(p): Parameters<AgentShowParams>,
    ) -> Json<AgentSingleResponse> {
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(AgentSingleResponse {
                    agent: None,
                    error: Some(e),
                });
            }
        };
        let repo = AgentRepository::new(self.state.db().clone(), self.state.event_bus());

        let role = match resolve_agent(&repo, &project_id, &p.id).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                return Json(AgentSingleResponse {
                    agent: None,
                    error: Some(agent_not_found_error(&p.id)),
                });
            }
            Err(e) => {
                return Json(AgentSingleResponse {
                    agent: None,
                    error: Some(e),
                });
            }
        };

        Json(AgentSingleResponse {
            agent: Some(AgentModel::from(&role)),
            error: None,
        })
    }

    /// List agents for a project with optional base_role filter and pagination.
    #[tool(
        description = "List agents for a project with optional base_role filter. Returns {agents[], total_count, limit, offset, has_more}. Defaults are ordered by base_role then name."
    )]
    pub async fn agent_list(
        &self,
        Parameters(p): Parameters<AgentListParams>,
    ) -> Json<AgentListResponse> {
        if let Some(ref br) = p.base_role
            && let Err(e) = validate_base_role(br)
        {
            return Json(AgentListResponse {
                agents: None,
                total_count: None,
                limit: None,
                offset: None,
                has_more: None,
                error: Some(e),
            });
        }
        let limit = validate_limit(p.limit.unwrap_or(25));
        let offset = validate_offset(p.offset.unwrap_or(0));
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(AgentListResponse {
                    agents: None,
                    total_count: None,
                    limit: None,
                    offset: None,
                    has_more: None,
                    error: Some(e),
                });
            }
        };
        let repo = AgentRepository::new(self.state.db().clone(), self.state.event_bus());
        match repo
            .list_for_project(AgentListQuery {
                project_id,
                base_role: p.base_role,
                limit,
                offset,
            })
            .await
        {
            Ok(result) => Json(AgentListResponse {
                agents: Some(result.agents.iter().map(AgentModel::from).collect()),
                total_count: Some(result.total_count),
                limit: Some(limit),
                offset: Some(offset),
                has_more: Some(offset + limit < result.total_count),
                error: None,
            }),
            Err(e) => Json(AgentListResponse {
                agents: None,
                total_count: None,
                limit: None,
                offset: None,
                has_more: None,
                error: Some(e.to_string()),
            }),
        }
    }

    /// Update a non-default agent's fields. Cannot modify is_default.
    #[tool(
        description = "Update a specialist agent (name, description, system_prompt_extensions, model_preference, verification_command, mcp_servers, skills). Cannot modify default agents' is_default flag. Accepts agent UUID or name."
    )]
    pub async fn agent_update(
        &self,
        Parameters(p): Parameters<AgentUpdateParams>,
    ) -> Json<AgentSingleResponse> {
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(AgentSingleResponse {
                    agent: None,
                    error: Some(e),
                });
            }
        };
        let repo = AgentRepository::new(self.state.db().clone(), self.state.event_bus());

        let role = match resolve_agent(&repo, &project_id, &p.id).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                return Json(AgentSingleResponse {
                    agent: None,
                    error: Some(agent_not_found_error(&p.id)),
                });
            }
            Err(e) => {
                return Json(AgentSingleResponse {
                    agent: None,
                    error: Some(e),
                });
            }
        };

        // Determine new name; check uniqueness if changed.
        let new_name = if let Some(ref n) = p.name {
            match validate_agent_name(n) {
                Ok(v) => v,
                Err(e) => {
                    return Json(AgentSingleResponse {
                        agent: None,
                        error: Some(e),
                    });
                }
            }
        } else {
            role.name.clone()
        };

        if new_name != role.name {
            match repo.get_by_name_for_project(&project_id, &new_name).await {
                Ok(Some(_)) => {
                    return Json(AgentSingleResponse {
                        agent: None,
                        error: Some(format!(
                            "an agent named '{new_name}' already exists in this project"
                        )),
                    });
                }
                Err(e) => {
                    return Json(AgentSingleResponse {
                        agent: None,
                        error: Some(e.to_string()),
                    });
                }
                Ok(None) => {}
            }
        }

        let description = p.description.as_deref().unwrap_or(&role.description);
        let system_prompt_extensions = p
            .system_prompt_extensions
            .as_deref()
            .unwrap_or(&role.system_prompt_extensions);
        let model_preference = if p.model_preference.is_some() {
            p.model_preference.as_deref()
        } else {
            role.model_preference.as_deref()
        };
        let verification_command = if p.verification_command.is_some() {
            p.verification_command.as_deref()
        } else {
            role.verification_command.as_deref()
        };
        let mcp_servers_str = p
            .mcp_servers
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string()))
            .unwrap_or_else(|| role.mcp_servers.clone());
        let skills_str = p
            .skills
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string()))
            .unwrap_or_else(|| role.skills.clone());
        // Resolve learned_prompt: clear wins over set; otherwise keep existing.
        let learned_prompt_value: Option<&str> = if p.clear_learned_prompt.unwrap_or(false) {
            None
        } else if let Some(ref lp) = p.learned_prompt {
            Some(lp.as_str())
        } else {
            role.learned_prompt.as_deref()
        };

        match repo
            .update(
                &role.id,
                AgentUpdateInput {
                    name: &new_name,
                    description,
                    system_prompt_extensions,
                    model_preference,
                    verification_command,
                    mcp_servers: &mcp_servers_str,
                    skills: &skills_str,
                    learned_prompt: learned_prompt_value,
                },
            )
            .await
        {
            Ok(updated) => Json(AgentSingleResponse {
                agent: Some(AgentModel::from(&updated)),
                error: None,
            }),
            Err(e) => Json(AgentSingleResponse {
                agent: None,
                error: Some(e.to_string()),
            }),
        }
    }
}

// ── role_metrics types ────────────────────────────────────────────────────────

#[derive(Serialize, schemars::JsonSchema)]
pub struct AgentMetricEntry {
    pub agent_id: String,
    pub agent_name: String,
    pub base_role: String,
    /// Fraction of completed tasks that closed as "completed" (0.0–1.0).
    pub success_rate: f64,
    /// Average total tokens (in + out) per completed session.
    pub avg_tokens: f64,
    /// Average session wall-clock duration in seconds (completed sessions only).
    pub avg_time_seconds: f64,
    /// Fraction of tasks with zero verification failures (0.0–1.0).
    pub verification_pass_rate: f64,
    /// Average reopen_count across tasks dispatched to this role.
    pub avg_reopens: f64,
    /// Number of completed tasks included in the calculation.
    pub completed_task_count: i64,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct AgentMetricsResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<AgentMetricEntry>>,
    /// Window used for session queries (days back from now).
    pub window_days: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct AgentMetricsParams {
    /// Absolute project path.
    pub project: String,
    /// Optional agent UUID or name — if omitted returns metrics for all agents.
    pub agent_id: Option<String>,
    /// How many days back to include session data (default 30).
    pub window_days: Option<i64>,
}

// ── role_metrics impl ─────────────────────────────────────────────────────────

/// Map a DB agent_role base_role to the session agent_type string.
fn base_role_to_agent_type(base_role: &str) -> &str {
    match base_role {
        "worker" | "resolver" => "worker",
        "reviewer" => "reviewer",
        "planner" => "planner",
        "lead" => "lead",
        other => other,
    }
}

#[tool_router(router = agent_metrics_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Aggregate effectiveness metrics per agent: success rate, token usage,
    /// session duration, verification pass rate, reopen rate.
    /// Optionally filter to a single agent by UUID or name.
    #[tool(
        description = "Return aggregated effectiveness metrics per agent (success_rate, avg_tokens, avg_time_seconds, verification_pass_rate, avg_reopens). Accepts optional agent_id filter and window_days (default 30)."
    )]
    pub async fn agent_metrics(
        &self,
        Parameters(p): Parameters<AgentMetricsParams>,
    ) -> Json<AgentMetricsResponse> {
        let window_days = p.window_days.unwrap_or(30).max(1);

        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(AgentMetricsResponse {
                    agents: None,
                    window_days,
                    error: Some(e),
                });
            }
        };

        let repo = AgentRepository::new(self.state.db().clone(), self.state.event_bus());

        // Normalize empty/whitespace-only agent_id to None (return all agents).
        let agent_id = p.agent_id.filter(|s| !s.trim().is_empty());

        // Collect the roles to compute metrics for.
        let agents: Vec<Agent> = if let Some(ref id_or_name) = agent_id {
            match resolve_agent(&repo, &project_id, id_or_name).await {
                Ok(Some(r)) => vec![r],
                Ok(None) => {
                    return Json(AgentMetricsResponse {
                        agents: None,
                        window_days,
                        error: Some(agent_not_found_error(id_or_name)),
                    });
                }
                Err(e) => {
                    return Json(AgentMetricsResponse {
                        agents: None,
                        window_days,
                        error: Some(e),
                    });
                }
            }
        } else {
            match repo
                .list_for_project(AgentListQuery {
                    project_id: project_id.clone(),
                    base_role: None,
                    limit: 200,
                    offset: 0,
                })
                .await
            {
                Ok(result) => result.agents,
                Err(e) => {
                    return Json(AgentMetricsResponse {
                        agents: None,
                        window_days,
                        error: Some(e.to_string()),
                    });
                }
            }
        };

        let mut entries: Vec<AgentMetricEntry> = Vec::with_capacity(agents.len());

        for agent in &agents {
            let agent_type = base_role_to_agent_type(&agent.base_role);
            let m: DbAgentMetrics = repo
                .get_metrics(&project_id, agent_type, window_days)
                .await
                .unwrap_or(DbAgentMetrics {
                    success_rate: 0.0,
                    avg_reopens: 0.0,
                    verification_pass_rate: 0.0,
                    completed_task_count: 0,
                    avg_tokens: 0.0,
                    avg_time_seconds: 0.0,
                });

            entries.push(AgentMetricEntry {
                agent_id: agent.id.clone(),
                agent_name: agent.name.clone(),
                base_role: agent.base_role.clone(),
                success_rate: m.success_rate,
                avg_reopens: m.avg_reopens,
                verification_pass_rate: m.verification_pass_rate,
                completed_task_count: m.completed_task_count,
                avg_tokens: m.avg_tokens,
                avg_time_seconds: m.avg_time_seconds,
            });
        }

        Json(AgentMetricsResponse {
            agents: Some(entries),
            window_days,
            error: None,
        })
    }
}

/// Resolve a role by UUID or name within a project.
async fn resolve_agent(
    repo: &AgentRepository,
    project_id: &str,
    id_or_name: &str,
) -> Result<Option<Agent>, String> {
    // Try by UUID first.
    if let Ok(Some(role)) = repo.get(id_or_name).await
        && role.project_id == project_id
    {
        return Ok(Some(role));
    }
    // Fall back to name lookup within project.
    repo.get_by_name_for_project(project_id, id_or_name)
        .await
        .map_err(|e| e.to_string())
}
