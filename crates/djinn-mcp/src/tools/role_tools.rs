// MCP tools for agent role CRUD (role_create, role_update, role_list, role_show, role_metrics).

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;
use crate::tools::json_object::AnyJson;
use crate::tools::validation::{validate_limit, validate_offset};
use djinn_core::models::AgentRole;
use djinn_db::{
    AgentRoleCreateInput, AgentRoleListQuery, AgentRoleRepository, AgentRoleUpdateInput,
    VALID_BASE_ROLES,
};
use djinn_db::AgentRoleMetrics as DbRoleMetrics;

// ── View model ───────────────────────────────────────────────────────────────

#[derive(Serialize, schemars::JsonSchema)]
pub struct AgentRoleModel {
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

impl From<&AgentRole> for AgentRoleModel {
    fn from(r: &AgentRole) -> Self {
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
pub struct RoleSingleResponse {
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub role: Option<AgentRoleModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct RoleListResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roles: Option<Vec<AgentRoleModel>>,
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

fn role_not_found_error(id: &str) -> String {
    format!("agent_role not found: {id}")
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

fn validate_role_name(name: &str) -> Result<String, String> {
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
pub struct RoleCreateParams {
    /// Absolute project path.
    pub project: String,
    /// Unique role name within the project.
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
    /// Additional MCP server refs for this role.
    pub mcp_servers: Option<Vec<AnyJson>>,
    /// Skills (prompt templates) available to this role.
    pub skills: Option<Vec<AnyJson>>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RoleShowParams {
    /// Absolute project path.
    pub project: String,
    /// Role UUID or name.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RoleListParams {
    /// Absolute project path.
    pub project: String,
    /// Filter by base role: worker, lead, planner, architect, reviewer, resolver.
    pub base_role: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RoleUpdateParams {
    /// Absolute project path.
    pub project: String,
    /// Role UUID or name.
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

#[tool_router(router = role_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Create a specialist agent role that extends a base role with domain-specific config.
    #[tool(
        description = "Create a specialist agent role extending a base role (worker, lead, planner, architect, reviewer, resolver). Returns the created role."
    )]
    pub async fn role_create(
        &self,
        Parameters(p): Parameters<RoleCreateParams>,
    ) -> Json<RoleSingleResponse> {
        let name = match validate_role_name(&p.name) {
            Ok(n) => n,
            Err(e) => return Json(RoleSingleResponse { role: None, error: Some(e) }),
        };
        if let Err(e) = validate_base_role(&p.base_role) {
            return Json(RoleSingleResponse { role: None, error: Some(e) });
        }
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(RoleSingleResponse { role: None, error: Some(e) }),
        };

        let repo = AgentRoleRepository::new(self.state.db().clone(), self.state.event_bus());

        // Enforce name uniqueness within project.
        match repo.get_by_name_for_project(&project_id, &name).await {
            Ok(Some(_)) => {
                return Json(RoleSingleResponse {
                    role: None,
                    error: Some(format!("a role named '{name}' already exists in this project")),
                });
            }
            Err(e) => return Json(RoleSingleResponse { role: None, error: Some(e.to_string()) }),
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
                AgentRoleCreateInput {
                    name: &name,
                    base_role: &p.base_role,
                    description: p.description.as_deref().unwrap_or(""),
                    system_prompt_extensions: p
                        .system_prompt_extensions
                        .as_deref()
                        .unwrap_or(""),
                    model_preference: p.model_preference.as_deref(),
                    verification_command: p.verification_command.as_deref(),
                    mcp_servers: mcp_servers_json.as_deref(),
                    skills: skills_json.as_deref(),
                    is_default: false,
                },
            )
            .await
        {
            Ok(role) => Json(RoleSingleResponse {
                role: Some(AgentRoleModel::from(&role)),
                error: None,
            }),
            Err(e) => Json(RoleSingleResponse {
                role: None,
                error: Some(e.to_string()),
            }),
        }
    }

    /// Show full details of an agent role by UUID or name.
    #[tool(
        description = "Show full details of an agent role. Accepts role UUID or name."
    )]
    pub async fn role_show(
        &self,
        Parameters(p): Parameters<RoleShowParams>,
    ) -> Json<RoleSingleResponse> {
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(RoleSingleResponse { role: None, error: Some(e) }),
        };
        let repo = AgentRoleRepository::new(self.state.db().clone(), self.state.event_bus());

        let role = match resolve_role(&repo, &project_id, &p.id).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                return Json(RoleSingleResponse {
                    role: None,
                    error: Some(role_not_found_error(&p.id)),
                });
            }
            Err(e) => return Json(RoleSingleResponse { role: None, error: Some(e) }),
        };

        Json(RoleSingleResponse {
            role: Some(AgentRoleModel::from(&role)),
            error: None,
        })
    }

    /// List agent roles for a project with optional base_role filter and pagination.
    #[tool(
        description = "List agent roles for a project with optional base_role filter. Returns {roles[], total_count, limit, offset, has_more}. Defaults are ordered by base_role then name."
    )]
    pub async fn role_list(
        &self,
        Parameters(p): Parameters<RoleListParams>,
    ) -> Json<RoleListResponse> {
        if let Some(ref br) = p.base_role
            && let Err(e) = validate_base_role(br)
        {
            return Json(RoleListResponse {
                roles: None,
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
                return Json(RoleListResponse {
                    roles: None,
                    total_count: None,
                    limit: None,
                    offset: None,
                    has_more: None,
                    error: Some(e),
                });
            }
        };
        let repo = AgentRoleRepository::new(self.state.db().clone(), self.state.event_bus());
        match repo
            .list_for_project(AgentRoleListQuery {
                project_id,
                base_role: p.base_role,
                limit,
                offset,
            })
            .await
        {
            Ok(result) => Json(RoleListResponse {
                roles: Some(result.roles.iter().map(AgentRoleModel::from).collect()),
                total_count: Some(result.total_count),
                limit: Some(limit),
                offset: Some(offset),
                has_more: Some(offset + limit < result.total_count),
                error: None,
            }),
            Err(e) => Json(RoleListResponse {
                roles: None,
                total_count: None,
                limit: None,
                offset: None,
                has_more: None,
                error: Some(e.to_string()),
            }),
        }
    }

    /// Update a non-default agent role's fields. Cannot modify is_default.
    #[tool(
        description = "Update a specialist agent role (name, description, system_prompt_extensions, model_preference, verification_command, mcp_servers, skills). Cannot modify default roles' is_default flag. Accepts role UUID or name."
    )]
    pub async fn role_update(
        &self,
        Parameters(p): Parameters<RoleUpdateParams>,
    ) -> Json<RoleSingleResponse> {
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(RoleSingleResponse { role: None, error: Some(e) }),
        };
        let repo = AgentRoleRepository::new(self.state.db().clone(), self.state.event_bus());

        let role = match resolve_role(&repo, &project_id, &p.id).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                return Json(RoleSingleResponse {
                    role: None,
                    error: Some(role_not_found_error(&p.id)),
                });
            }
            Err(e) => return Json(RoleSingleResponse { role: None, error: Some(e) }),
        };

        // Determine new name; check uniqueness if changed.
        let new_name = if let Some(ref n) = p.name {
            match validate_role_name(n) {
                Ok(v) => v,
                Err(e) => return Json(RoleSingleResponse { role: None, error: Some(e) }),
            }
        } else {
            role.name.clone()
        };

        if new_name != role.name {
            match repo.get_by_name_for_project(&project_id, &new_name).await {
                Ok(Some(_)) => {
                    return Json(RoleSingleResponse {
                        role: None,
                        error: Some(format!(
                            "a role named '{new_name}' already exists in this project"
                        )),
                    });
                }
                Err(e) => {
                    return Json(RoleSingleResponse { role: None, error: Some(e.to_string()) });
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
                AgentRoleUpdateInput {
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
            Ok(updated) => Json(RoleSingleResponse {
                role: Some(AgentRoleModel::from(&updated)),
                error: None,
            }),
            Err(e) => Json(RoleSingleResponse {
                role: None,
                error: Some(e.to_string()),
            }),
        }
    }
}

// ── role_metrics types ────────────────────────────────────────────────────────

#[derive(Serialize, schemars::JsonSchema)]
pub struct RoleMetricEntry {
    pub role_id: String,
    pub role_name: String,
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
pub struct RoleMetricsResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roles: Option<Vec<RoleMetricEntry>>,
    /// Window used for session queries (days back from now).
    pub window_days: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RoleMetricsParams {
    /// Absolute project path.
    pub project: String,
    /// Optional role UUID or name — if omitted returns metrics for all roles.
    pub role_id: Option<String>,
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

#[tool_router(router = role_metrics_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Aggregate effectiveness metrics per agent role: success rate, token usage,
    /// session duration, verification pass rate, reopen rate.
    /// Optionally filter to a single role by UUID or name.
    #[tool(
        description = "Return aggregated effectiveness metrics per agent role (success_rate, avg_tokens, avg_time_seconds, verification_pass_rate, avg_reopens). Accepts optional role_id filter and window_days (default 30)."
    )]
    pub async fn role_metrics(
        &self,
        Parameters(p): Parameters<RoleMetricsParams>,
    ) -> Json<RoleMetricsResponse> {
        let window_days = p.window_days.unwrap_or(30).max(1);

        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(RoleMetricsResponse {
                    roles: None,
                    window_days,
                    error: Some(e),
                });
            }
        };

        let repo = AgentRoleRepository::new(self.state.db().clone(), self.state.event_bus());

        // Collect the roles to compute metrics for.
        let roles: Vec<AgentRole> = if let Some(ref id_or_name) = p.role_id {
            match resolve_role(&repo, &project_id, id_or_name).await {
                Ok(Some(r)) => vec![r],
                Ok(None) => {
                    return Json(RoleMetricsResponse {
                        roles: None,
                        window_days,
                        error: Some(role_not_found_error(id_or_name)),
                    });
                }
                Err(e) => {
                    return Json(RoleMetricsResponse {
                        roles: None,
                        window_days,
                        error: Some(e),
                    });
                }
            }
        } else {
            match repo
                .list_for_project(AgentRoleListQuery {
                    project_id: project_id.clone(),
                    base_role: None,
                    limit: 200,
                    offset: 0,
                })
                .await
            {
                Ok(result) => result.roles,
                Err(e) => {
                    return Json(RoleMetricsResponse {
                        roles: None,
                        window_days,
                        error: Some(e.to_string()),
                    });
                }
            }
        };

        let mut entries: Vec<RoleMetricEntry> = Vec::with_capacity(roles.len());

        for role in &roles {
            let agent_type = base_role_to_agent_type(&role.base_role);
            let m: DbRoleMetrics = repo
                .get_metrics(&project_id, agent_type, window_days)
                .await
                .unwrap_or(DbRoleMetrics {
                    success_rate: 0.0,
                    avg_reopens: 0.0,
                    verification_pass_rate: 0.0,
                    completed_task_count: 0,
                    avg_tokens: 0.0,
                    avg_time_seconds: 0.0,
                });

            entries.push(RoleMetricEntry {
                role_id: role.id.clone(),
                role_name: role.name.clone(),
                base_role: role.base_role.clone(),
                success_rate: m.success_rate,
                avg_reopens: m.avg_reopens,
                verification_pass_rate: m.verification_pass_rate,
                completed_task_count: m.completed_task_count,
                avg_tokens: m.avg_tokens,
                avg_time_seconds: m.avg_time_seconds,
            });
        }

        Json(RoleMetricsResponse {
            roles: Some(entries),
            window_days,
            error: None,
        })
    }
}

/// Resolve a role by UUID or name within a project.
async fn resolve_role(
    repo: &AgentRoleRepository,
    project_id: &str,
    id_or_name: &str,
) -> Result<Option<AgentRole>, String> {
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
