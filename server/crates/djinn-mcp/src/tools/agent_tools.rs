// MCP tools for agent CRUD (agent_create, agent_update, agent_list, agent_show, agent_metrics).

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;
use crate::tools::AnyJson;
use crate::tools::validation::{validate_limit, validate_offset};
use djinn_db::{AgentListQuery, AgentRepository, AgentUpdateInput};

mod ops;

pub use self::ops::{
    AgentCreateParams, AgentMetricEntry, AgentMetricsParams, AgentMetricsResponse, AgentModel,
    AgentSingleResponse, create_agent, metrics_for_agents,
};

use self::ops::{agent_not_found_error, resolve_agent, validate_agent_name, validate_base_role};

// ── Response types ────────────────────────────────────────────────────────────

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
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(AgentSingleResponse {
                    agent: None,
                    error: Some(e),
                });
            }
        };

        Json(
            create_agent(
                &AgentRepository::new(self.state.db().clone(), self.state.event_bus()),
                &project_id,
                p,
            )
            .await,
        )
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
        // Since learned_prompt is now derived from history rows, clearing means
        // marking all active amendments as discarded.
        if p.clear_learned_prompt.unwrap_or(false)
            && let Err(e) = repo.clear_amendments(&role.id).await
        {
            return Json(AgentSingleResponse {
                agent: None,
                error: Some(format!("failed to clear amendments: {e}")),
            });
        }
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

        Json(
            metrics_for_agents(
                &AgentRepository::new(self.state.db().clone(), self.state.event_bus()),
                &project_id,
                p,
            )
            .await,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::stubs::test_mcp_state;
    use djinn_db::ProjectRepository;
    use tempfile::TempDir;

    async fn test_server() -> (DjinnMcpServer, TempDir, String) {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db = djinn_db::Database::open_in_memory().expect("db");
        let project_repo = ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let project = project_repo
            .create("agent-tools", tempdir.path().to_str().expect("path"))
            .await
            .expect("create project");
        let state = test_mcp_state(db);
        (DjinnMcpServer::new(state), tempdir, project.path)
    }

    #[tokio::test]
    async fn agent_create_and_metrics_preserve_mcp_response_shapes() {
        let (server, _dir, project_path) = test_server().await;

        let create = server
            .dispatch_tool(
                "agent_create",
                serde_json::json!({
                    "project": project_path,
                    "name": "Shared Agent",
                    "base_role": "worker",
                    "description": "Uses shared ops",
                    "system_prompt_extensions": "Preserve MCP payload",
                    "model_preference": "gpt-5"
                }),
            )
            .await
            .expect("dispatch agent_create");

        assert_eq!(create.get("error"), None);
        assert_eq!(
            create.get("name").and_then(|value| value.as_str()),
            Some("Shared Agent")
        );
        assert_eq!(
            create.get("base_role").and_then(|value| value.as_str()),
            Some("worker")
        );
        let agent_id = create
            .get("id")
            .and_then(|value| value.as_str())
            .expect("created agent id")
            .to_string();

        let metrics = server
            .dispatch_tool(
                "agent_metrics",
                serde_json::json!({
                    "project": project_path,
                    "agent_id": agent_id,
                    "window_days": 7
                }),
            )
            .await
            .expect("dispatch agent_metrics");

        assert_eq!(
            metrics.get("window_days").and_then(|value| value.as_i64()),
            Some(7)
        );
        let agents = metrics
            .get("agents")
            .and_then(|value| value.as_array())
            .expect("agents array");
        assert_eq!(agents.len(), 1);
        assert_eq!(
            agents[0].get("agent_name").and_then(|value| value.as_str()),
            Some("Shared Agent")
        );
        assert!(
            agents[0]
                .get("success_rate")
                .and_then(|value| value.as_f64())
                .is_some(),
            "metrics entry should preserve numeric payload fields"
        );
    }
}
