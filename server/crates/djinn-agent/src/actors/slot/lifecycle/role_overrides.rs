//! Per-stage role-config resolution: specialist override or default role config. DB failures non-fatal.

use std::str::FromStr;
use std::sync::Arc;

use djinn_core::models::{Agent, Task, parse_json_array};
use djinn_db::AgentRepository;
use djinn_runtime::spec::RoleKind;

use crate::AgentType;
use crate::context::AgentContext;
use crate::roles::{AgentRole, role_impl_for};

/// Per-stage role-config bundle consumed by `execute_stage`.
/// All fields have sensible empty defaults so the stage can proceed
/// even when the project has no DB-level role rows configured.
pub(crate) struct ResolvedRoleOverrides {
    /// Runtime role that renders the base prompt.
    pub runtime_role: Arc<dyn AgentRole>,
    /// Project/specialist system prompt extensions; empty when absent.
    pub system_prompt_extensions: String,
    /// MCP server names from the DB row's JSON array. `None` means no row.
    pub mcp_servers: Option<Vec<String>>,
    /// Skill names from the DB row's JSON array; empty when absent.
    pub skills: Vec<String>,
    /// Role-level model preference used for `TaskRunSpec` seeding.
    pub model_preference: Option<String>,
    /// `true` when a specialist override swapped the runtime role.
    pub specialist_overrode_runtime_role: bool,
}

impl ResolvedRoleOverrides {
    fn empty(runtime_role: Arc<dyn AgentRole>) -> Self {
        Self {
            runtime_role,
            system_prompt_extensions: String::new(),
            mcp_servers: None,
            skills: Vec::new(),
            model_preference: None,
            specialist_overrode_runtime_role: false,
        }
    }
    fn apply_agent_fields(&mut self, agent: &Agent) {
        self.system_prompt_extensions = agent.system_prompt_extensions.clone();
        self.mcp_servers = Some(parse_json_array(&agent.mcp_servers));
        self.skills = parse_json_array(&agent.skills);
        self.model_preference = agent.model_preference.clone();
    }
}

/// Map a [`RoleKind`] to the in-tree [`AgentType`] used by `role_impl_for`.
fn agent_type_for_role_kind(kind: RoleKind) -> AgentType {
    match kind {
        RoleKind::Planner => AgentType::Planner,
        RoleKind::Worker => AgentType::Worker,
        RoleKind::Reviewer => AgentType::Reviewer,
        RoleKind::Verifier => AgentType::Worker,
        RoleKind::Architect => AgentType::Architect,
        RoleKind::Lead => AgentType::Lead,
        RoleKind::Refinement => AgentType::Advocate,
    }
}

/// Resolve a specialist or tribunal override for the stage, populating
/// `out` and returning `true` when the override fully determines the role.
async fn resolve_runtime_role_override(
    task: &Task,
    role_kind: RoleKind,
    injected_agent_type: AgentType,
    role_repo: &AgentRepository,
    out: &mut ResolvedRoleOverrides,
) -> bool {
    // Specialist override path (Worker stage only).
    if role_kind == RoleKind::Worker
        && let Some(specialist_name) = task.agent_type.as_deref()
        && !specialist_name.is_empty()
    {
        match role_repo
            .get_by_name_for_project(&task.project_id, specialist_name)
            .await
        {
            Ok(Some(agent)) => {
                tracing::debug!(
                    task_id = %task.short_id,
                    specialist = %agent.name,
                    base_role = %agent.base_role,
                    "Stage: overriding role config from specialist agent_type"
                );
                match AgentType::from_str(&agent.base_role) {
                    Ok(agent_type) => {
                        out.runtime_role = role_impl_for(agent_type);
                        out.specialist_overrode_runtime_role = agent_type != injected_agent_type;
                    }
                    Err(_) => {
                        tracing::warn!(
                            task_id = %task.short_id,
                            specialist = %agent.name,
                            base_role = %agent.base_role,
                            "Stage: specialist base_role is unknown; keeping injected role"
                        );
                    }
                }
                out.apply_agent_fields(&agent);
                return true;
            }
            Ok(None) => {
                tracing::debug!(
                    task_id = %task.short_id,
                    specialist = %specialist_name,
                    "Stage: task.agent_type does not resolve to a specialist row; \
                     falling back to default role config"
                );
            }
            Err(e) => {
                tracing::warn!(
                    task_id = %task.short_id,
                    specialist = %specialist_name,
                    error = %e,
                    "Stage: failed to load specialist agent row; falling back to default role config"
                );
            }
        }
    }
    // Refinement tribunal override path.
    if role_kind == RoleKind::Refinement
        && let Some(tribunal_role) = task.agent_type.as_deref()
        && !tribunal_role.is_empty()
    {
        match AgentType::from_str(tribunal_role) {
            Ok(agent_type) => {
                out.runtime_role = role_impl_for(agent_type);
                out.specialist_overrode_runtime_role = agent_type != injected_agent_type;
            }
            Err(_) => {
                tracing::warn!(
                    task_id = %task.short_id,
                    tribunal_role,
                    "Stage: unknown refinement tribunal role; keeping default Advocate"
                );
            }
        }
    }
    false
}

/// Resolve the effective role configuration for one stage of a supervisor task-run.
///
/// DB errors on either path are logged and fall through to empty defaults.
pub(crate) async fn resolve_role_overrides(
    task: &Task,
    role_kind: RoleKind,
    app_state: &AgentContext,
) -> ResolvedRoleOverrides {
    let injected_agent_type = agent_type_for_role_kind(role_kind);
    let injected_role = role_impl_for(injected_agent_type);
    let mut out = ResolvedRoleOverrides::empty(injected_role.clone());
    let role_repo = AgentRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    // Try specialist/tribunal override first.
    if resolve_runtime_role_override(task, role_kind, injected_agent_type, &role_repo, &mut out)
        .await
    {
        return out;
    }
    // Default role config path.
    let base_role = injected_agent_type.as_str();
    match role_repo
        .get_default_for_base_role(&task.project_id, base_role)
        .await
    {
        Ok(Some(agent)) => {
            out.apply_agent_fields(&agent);
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(
                task_id = %task.short_id,
                base_role,
                error = %e,
                "Stage: failed to load default role agent row; using empty overrides"
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::events::EventBus;
    use djinn_db::TaskRepository;
    use djinn_db::repositories::agent::AgentCreateInput;
    use djinn_db::{Database, EffectiveCreatorProvenance, ProjectRepository};
    use tokio_util::sync::CancellationToken;
    async fn setup_project(db: &Database) -> String {
        db.ensure_initialized().await.unwrap();
        let repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let name = format!("test-proj-{}", uuid::Uuid::now_v7());
        let proj = repo.create(&name, "test", &name).await.unwrap();
        proj.id
    }
    async fn make_task(db: &Database, project_id: &str, agent_type: Option<&str>) -> Task {
        let repo = TaskRepository::new(db.clone(), EventBus::noop());
        let creator = crate::test_helpers::create_test_creator(db).await;
        let task = repo
            .create_in_project_with_provenance(
                project_id,
                None,
                EffectiveCreatorProvenance {
                    explicit_user_id: Some(&creator.id),
                    source_task_id: None,
                    proposal_id: None,
                },
                "role-overrides-test",
                "test",
                "test",
                "task",
                3,
                "test-owner",
                None,
                None,
            )
            .await
            .unwrap();
        if let Some(at) = agent_type {
            repo.update_agent_type(&task.id, Some(at)).await.unwrap()
        } else {
            task
        }
    }
    fn agent_context(db: Database) -> AgentContext {
        crate::test_helpers::agent_context_from_db(db, CancellationToken::new())
    }
    #[tokio::test]
    async fn seeded_project_default_row_yields_empty_overrides() {
        let db = crate::test_helpers::create_test_db();
        let project_id = setup_project(&db).await;
        let task = make_task(&db, &project_id, None).await;
        let ctx = agent_context(db);
        let out = resolve_role_overrides(&task, RoleKind::Worker, &ctx).await;
        assert_eq!(out.system_prompt_extensions, "");
        assert_eq!(out.mcp_servers, Some(Vec::<String>::new()));
        assert!(out.skills.is_empty());
        assert!(out.model_preference.is_none());
        assert!(!out.specialist_overrode_runtime_role);
        assert_eq!(out.runtime_role.config().name, "worker");
    }
    #[tokio::test]
    async fn default_worker_row_populates_overrides() {
        let db = crate::test_helpers::create_test_db();
        let project_id = setup_project(&db).await;
        let role_repo = AgentRepository::new(db.clone(), EventBus::noop());
        role_repo
            .delete_for_base_role(&project_id, "worker")
            .await
            .unwrap();
        role_repo
            .create_for_project(
                &project_id,
                AgentCreateInput {
                    name: "worker",
                    base_role: "worker",
                    description: "default worker",
                    system_prompt_extensions: "always write tests",
                    model_preference: Some("claude-opus-4-6"),
                    mcp_servers: Some(r#"["github"]"#),
                    skills: Some(r#"["tdd","rust"]"#),
                    is_default: true,
                },
            )
            .await
            .unwrap();
        let task = make_task(&db, &project_id, None).await;
        let ctx = agent_context(db);
        let out = resolve_role_overrides(&task, RoleKind::Worker, &ctx).await;
        assert_eq!(out.system_prompt_extensions, "always write tests");
        assert_eq!(out.mcp_servers, Some(vec!["github".to_string()]));
        assert_eq!(out.skills, vec!["tdd".to_string(), "rust".to_string()]);
        assert_eq!(out.model_preference.as_deref(), Some("claude-opus-4-6"));
        assert!(!out.specialist_overrode_runtime_role);
        assert_eq!(out.runtime_role.config().name, "worker");
    }
    #[tokio::test]
    async fn worker_specialist_override_swaps_role_and_fields() {
        let db = crate::test_helpers::create_test_db();
        let project_id = setup_project(&db).await;
        let role_repo = AgentRepository::new(db.clone(), EventBus::noop());
        role_repo
            .create_for_project(
                &project_id,
                AgentCreateInput {
                    name: "senior-planner",
                    base_role: "planner",
                    description: "planner specialist",
                    system_prompt_extensions: "plan carefully",
                    model_preference: None,
                    mcp_servers: Some(r#"[]"#),
                    skills: Some(r#"["planning"]"#),
                    is_default: false,
                },
            )
            .await
            .unwrap();
        let task = make_task(&db, &project_id, Some("senior-planner")).await;
        let ctx = agent_context(db);
        let out = resolve_role_overrides(&task, RoleKind::Worker, &ctx).await;
        assert_eq!(out.runtime_role.config().name, "planner");
        assert!(out.specialist_overrode_runtime_role);
        assert_eq!(out.system_prompt_extensions, "plan carefully");
        assert_eq!(out.skills, vec!["planning".to_string()]);
        assert_eq!(out.mcp_servers, Some(Vec::<String>::new()));
    }
    #[tokio::test]
    async fn specialist_only_applies_on_worker_stage() {
        let db = crate::test_helpers::create_test_db();
        let project_id = setup_project(&db).await;
        let role_repo = AgentRepository::new(db.clone(), EventBus::noop());
        role_repo
            .create_for_project(
                &project_id,
                AgentCreateInput {
                    name: "rust-expert",
                    base_role: "worker",
                    description: "worker specialist",
                    system_prompt_extensions: "specialist-ext",
                    model_preference: None,
                    mcp_servers: Some("[]"),
                    skills: Some("[]"),
                    is_default: false,
                },
            )
            .await
            .unwrap();
        role_repo
            .set_default_system_prompt_extensions(&project_id, "planner", "default-planner-ext")
            .await
            .unwrap();
        let task = make_task(&db, &project_id, Some("rust-expert")).await;
        let ctx = agent_context(db);
        let out = resolve_role_overrides(&task, RoleKind::Planner, &ctx).await;
        assert_eq!(out.runtime_role.config().name, "planner");
        assert!(!out.specialist_overrode_runtime_role);
        assert_eq!(out.system_prompt_extensions, "default-planner-ext");
    }
    #[tokio::test]
    async fn missing_specialist_falls_back_to_default() {
        let db = crate::test_helpers::create_test_db();
        let project_id = setup_project(&db).await;
        let role_repo = AgentRepository::new(db.clone(), EventBus::noop());
        role_repo
            .set_default_system_prompt_extensions(&project_id, "worker", "default-worker-ext")
            .await
            .unwrap();
        let task = make_task(&db, &project_id, Some("nonexistent-expert")).await;
        let ctx = agent_context(db);
        let out = resolve_role_overrides(&task, RoleKind::Worker, &ctx).await;
        assert_eq!(out.runtime_role.config().name, "worker");
        assert!(!out.specialist_overrode_runtime_role);
        assert_eq!(out.system_prompt_extensions, "default-worker-ext");
    }
}
