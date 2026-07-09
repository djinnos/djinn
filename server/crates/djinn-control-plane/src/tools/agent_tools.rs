// MCP tools for agent CRUD (agent_create, agent_update, agent_list, agent_show, agent_metrics).

use std::borrow::Cow;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;
use crate::tools::AnyJson;
use crate::tools::list_response::{
    self, ListMeta, NamedListResponse, named_list_response_schema, serialize_named_list_response,
};
use crate::tools::validation::{validate_limit, validate_offset};
use djinn_db::{AgentListQuery, AgentRepository, AgentUpdateInput};

mod ops;

pub use self::ops::{
    AgentCreateParams, AgentMetricEntry, AgentMetricsParams, AgentMetricsResponse, AgentModel,
    AgentSingleResponse, create_agent, filter_native_skill_entries, is_native_skill_name,
    metrics_for_agents, reject_native_skill_entries,
};

use self::ops::{agent_not_found_error, resolve_agent, validate_agent_name, validate_base_role};

// ── Response types ────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AgentListResponse {
    pub agents: Option<Vec<AgentModel>>,
    pub meta: ListMeta,
}

impl NamedListResponse for AgentListResponse {
    type Item = AgentModel;

    const FIELD_NAME: &'static str = "agents";
    const TITLE: &'static str = "AgentListResponse";

    fn from_parts(items: Option<Vec<Self::Item>>, meta: ListMeta) -> Self {
        Self {
            agents: items,
            meta,
        }
    }

    fn items(&self) -> Option<&Vec<Self::Item>> {
        self.agents.as_ref()
    }

    fn meta(&self) -> &ListMeta {
        &self.meta
    }
}

impl Serialize for AgentListResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serialize_named_list_response(self, serializer)
    }
}

impl schemars::JsonSchema for AgentListResponse {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed(Self::TITLE)
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        named_list_response_schema::<AgentModel>(generator, Self::TITLE, Self::FIELD_NAME)
    }
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
    /// Filter by base role: worker, lead, planner, architect, reviewer.
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
    pub mcp_servers: Option<Vec<AnyJson>>,
    pub skills: Option<Vec<AnyJson>>,
}

// ── Tool implementations ──────────────────────────────────────────────────────

#[tool_router(router = agent_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Create a specialist agent that extends a base role with domain-specific config.
    #[tool(
        description = "Create a specialist agent extending a base role (worker, lead, planner, architect, reviewer). Returns the created agent."
    )]
    pub async fn agent_create(
        &self,
        Parameters(p): Parameters<AgentCreateParams>,
    ) -> Json<AgentSingleResponse> {
        // Mutating agents (which carry mcp_servers + skills config) is
        // admin-only. The no-user trusted path is still allowed for background
        // agents — see `acting_user::require_admin`.
        if let Err(e) = crate::tools::acting_user::require_admin(self.state.db()).await {
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
            return Json(list_response::error::<AgentListResponse>(e));
        }
        let limit = validate_limit(p.limit.unwrap_or(25));
        let offset = validate_offset(p.offset.unwrap_or(0));
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(list_response::error::<AgentListResponse>(e));
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
            Ok(result) => Json(list_response::success::<AgentListResponse>(
                result.agents.iter().map(AgentModel::from).collect(),
                result.total_count,
                limit,
                offset,
            )),
            Err(e) => Json(list_response::error::<AgentListResponse>(e.to_string())),
        }
    }

    /// Update a non-default agent's fields. Cannot modify is_default.
    #[tool(
        description = "Update a specialist agent (name, description, system_prompt_extensions, model_preference, mcp_servers, skills). Cannot modify default agents' is_default flag. Accepts agent UUID or name."
    )]
    pub async fn agent_update(
        &self,
        Parameters(p): Parameters<AgentUpdateParams>,
    ) -> Json<AgentSingleResponse> {
        // Admin-only — same rationale as `agent_create`.
        if let Err(e) = crate::tools::acting_user::require_admin(self.state.db()).await {
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
        // Reject native skill names before persisting.
        if let Some(ref skills) = p.skills
            && let Err(e) = reject_native_skill_entries(skills)
        {
            return Json(AgentSingleResponse {
                agent: None,
                error: Some(e),
            });
        }

        match repo
            .update(
                &role.id,
                AgentUpdateInput {
                    name: &new_name,
                    description,
                    system_prompt_extensions,
                    model_preference,
                    mcp_servers: &mcp_servers_str,
                    skills: &skills_str,
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
    /// session duration, reopen rate.
    /// Optionally filter to a single agent by UUID or name.
    #[tool(
        description = "Return aggregated effectiveness metrics per agent (success_rate, avg_tokens, avg_time_seconds, avg_reopens). Accepts optional agent_id filter and window_days (default 30)."
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

    fn workspace_tempdir() -> tempfile::TempDir {
        let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("test-tmp");
        std::fs::create_dir_all(&base).expect("create server crate test tempdir base");
        tempfile::tempdir_in(base).expect("create server crate tempdir")
    }
    use super::*;
    use crate::state::stubs::test_mcp_state;
    use djinn_db::ProjectRepository;
    use tempfile::TempDir;

    async fn test_server() -> (DjinnMcpServer, TempDir, String) {
        let tempdir = workspace_tempdir();
        let db = djinn_db::Database::open_in_memory().expect("db");
        let project_repo = ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let project = project_repo
            .create("agent-tools", "test", "agent-tools")
            .await
            .expect("create project");
        let state = test_mcp_state(db);
        // Pass the `owner/repo` slug — tool dispatch accepts either a UUID or
        // slug reference and this keeps the fixture independent of the new
        // runtime-synthesized `project_dir` layout.
        (DjinnMcpServer::new(state), tempdir, project.slug())
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
        let extraction_quality = agents[0]
            .get("extraction_quality")
            .and_then(|value| value.as_object())
            .expect("extraction quality payload");
        assert_eq!(
            extraction_quality
                .get("extracted")
                .and_then(|value| value.as_i64()),
            Some(0)
        );
    }

    #[tokio::test]
    async fn agent_create_rejects_native_skill_names() {
        let (server, _dir, project_path) = test_server().await;

        let result = server
            .dispatch_tool(
                "agent_create",
                serde_json::json!({
                    "project": project_path,
                    "name": "Native Skill Creator",
                    "base_role": "worker",
                    "skills": ["visual-spec"]
                }),
            )
            .await
            .expect("dispatch agent_create");

        let error = result
            .get("error")
            .and_then(|v| v.as_str())
            .expect("should have error");
        assert!(
            error.contains("native/immutable"),
            "error should mention native/immutable: {error}"
        );
        assert!(
            error.contains("'visual-spec'"),
            "error should list the offending name: {error}"
        );
        assert!(
            error.contains("platform-owned"),
            "error should explain platform ownership: {error}"
        );
    }

    #[tokio::test]
    async fn agent_create_rejects_native_skill_in_object_form() {
        let (server, _dir, project_path) = test_server().await;

        let result = server
            .dispatch_tool(
                "agent_create",
                serde_json::json!({
                    "project": project_path,
                    "name": "Native Obj Creator",
                    "base_role": "worker",
                    "skills": [{"name": "visual-spec"}]
                }),
            )
            .await
            .expect("dispatch agent_create");

        let error = result
            .get("error")
            .and_then(|v| v.as_str())
            .expect("should have error");
        assert!(error.contains("'visual-spec'"));
    }

    #[tokio::test]
    async fn agent_create_allows_non_native_skills() {
        let (server, _dir, project_path) = test_server().await;

        let result = server
            .dispatch_tool(
                "agent_create",
                serde_json::json!({
                    "project": project_path,
                    "name": "Good Creator",
                    "base_role": "worker",
                    "skills": ["my-skill", "another-skill"]
                }),
            )
            .await
            .expect("dispatch agent_create");

        assert_eq!(result.get("error"), None, "should succeed without error");
        assert_eq!(
            result.get("name").and_then(|v| v.as_str()),
            Some("Good Creator")
        );
    }

    #[tokio::test]
    async fn agent_update_rejects_native_skill_names() {
        let (server, _dir, project_path) = test_server().await;

        // First create an agent without native skills.
        let create = server
            .dispatch_tool(
                "agent_create",
                serde_json::json!({
                    "project": project_path,
                    "name": "Update Target",
                    "base_role": "worker"
                }),
            )
            .await
            .expect("dispatch agent_create");
        assert_eq!(create.get("error"), None);

        // Attempt to update with a native skill name.
        let result = server
            .dispatch_tool(
                "agent_update",
                serde_json::json!({
                    "project": project_path,
                    "id": "Update Target",
                    "skills": ["visual-spec"]
                }),
            )
            .await
            .expect("dispatch agent_update");

        let error = result
            .get("error")
            .and_then(|v| v.as_str())
            .expect("should have error");
        assert!(
            error.contains("native/immutable"),
            "error should mention native/immutable: {error}"
        );
        assert!(error.contains("'visual-spec'"));
    }

    #[tokio::test]
    async fn agent_update_allows_non_native_skills() {
        let (server, _dir, project_path) = test_server().await;

        // Create an agent.
        let create = server
            .dispatch_tool(
                "agent_create",
                serde_json::json!({
                    "project": project_path,
                    "name": "Update Good Target",
                    "base_role": "worker"
                }),
            )
            .await
            .expect("dispatch agent_create");
        assert_eq!(create.get("error"), None);

        // Update with non-native skills — should succeed.
        let result = server
            .dispatch_tool(
                "agent_update",
                serde_json::json!({
                    "project": project_path,
                    "id": "Update Good Target",
                    "skills": ["my-skill"]
                }),
            )
            .await
            .expect("dispatch agent_update");

        assert_eq!(result.get("error"), None, "should succeed without error");
    }

    #[tokio::test]
    async fn agent_show_filters_stale_native_skills() {
        let (server, _dir, project_path) = test_server().await;

        // Create a clean agent.
        let create = server
            .dispatch_tool(
                "agent_create",
                serde_json::json!({
                    "project": project_path,
                    "name": "Stale Show Agent",
                    "base_role": "worker",
                    "skills": ["good-skill"]
                }),
            )
            .await
            .expect("dispatch agent_create");
        assert_eq!(create.get("error"), None);

        // Show the agent — skills should only contain non-native entries.
        let show = server
            .dispatch_tool(
                "agent_show",
                serde_json::json!({
                    "project": project_path,
                    "id": "Stale Show Agent"
                }),
            )
            .await
            .expect("dispatch agent_show");

        let skills = show
            .get("skills")
            .and_then(|v| v.as_array())
            .expect("skills array");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].as_str(), Some("good-skill"));
        // Verify no native names leaked.
        for skill in skills {
            if let Some(name) = skill.as_str() {
                assert!(
                    !name.starts_with("visual-spec"),
                    "agent_show should not expose native skill '{name}'"
                );
            }
        }
    }

    #[tokio::test]
    async fn agent_list_filters_stale_native_skills() {
        let (server, _dir, project_path) = test_server().await;

        // Create an agent.
        let create = server
            .dispatch_tool(
                "agent_create",
                serde_json::json!({
                    "project": project_path,
                    "name": "Stale List Agent",
                    "base_role": "worker",
                    "skills": ["good-skill"]
                }),
            )
            .await
            .expect("dispatch agent_create");
        assert_eq!(create.get("error"), None);

        // List agents — skills should only contain non-native entries.
        let list = server
            .dispatch_tool(
                "agent_list",
                serde_json::json!({
                    "project": project_path
                }),
            )
            .await
            .expect("dispatch agent_list");

        let agents = list
            .get("agents")
            .and_then(|v| v.as_array())
            .expect("agents array");
        let agent = agents
            .iter()
            .find(|a| a.get("name").and_then(|v| v.as_str()) == Some("Stale List Agent"))
            .expect("found agent");

        let skills = agent
            .get("skills")
            .and_then(|v| v.as_array())
            .expect("skills array");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].as_str(), Some("good-skill"));
    }
}
