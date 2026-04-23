//! Per-stage role-config resolution shared by the supervisor-driven dispatch
//! path.
//!
//! Every stage in a supervisor task-run is invoked with a
//! [`djinn_runtime::RoleKind`] (the flow-enum role: `Planner`, `Worker`,
//! `Reviewer`, `Verifier`, `Architect`).  The RoleKind alone is not enough
//! to render a session prompt — the stage also needs the project-level DB
//! fields that used to be loaded by the legacy `run_task_lifecycle` path:
//!
//! - `system_prompt_extensions` — free-form text appended after the base
//!   template.
//! - `learned_prompt` — auto-improvement amendments, also appended.
//! - `mcp_servers` / `skills` — per-role tool/skill lists (override project
//!   defaults from `settings.json`).
//! - `verification_command` — role-level override for the project's
//!   `environment_config.verification` rules (fetched from Dolt in P8+).
//! - `model_preference` — role-level preference that dispatch uses to seed
//!   `TaskRunSpec::model_id_per_role`.
//!
//! Two resolution paths feed the same output struct:
//!
//! 1. **Specialist override** (Worker stage only): when the task row carries
//!    an `agent_type` that resolves to a non-empty specialist `Agent` row, the
//!    specialist takes over the stage — its `base_role` picks the runtime
//!    `AgentRole` impl, and its fields populate every override slot.  This is
//!    how the planner can route a task to `"rust-expert"` (base_role=worker)
//!    or `"senior-worker"` (base_role=worker) and have that role's
//!    prompt/skills/MCP config land on the Worker stage.
//! 2. **Default role config**: when no specialist applies, we fall back to
//!    the project's `is_default = 1` row for the RoleKind's `base_role`.
//!    That row carries the project-wide prompt extensions + learned_prompt
//!    + MCP/skill defaults.  Missing rows just yield empty defaults — the
//!      stage still runs against the code-level `AgentRole` template.
//!
//! Every DB failure is non-fatal: we log + return defaults so the stage
//! keeps running rather than failing a task-run on a transient DB blip.

use std::str::FromStr;
use std::sync::Arc;

use djinn_core::models::{Agent, Task, parse_json_array};
use djinn_db::AgentRepository;
use djinn_runtime::spec::RoleKind;

use crate::AgentType;
use crate::context::AgentContext;
use crate::roles::{AgentRole, role_impl_for};

/// Per-stage role-config bundle consumed by `execute_stage` when composing
/// `PromptContextInputs`, `resolve_mcp_and_skills`, and
/// `resolve_setup_and_verification_context`.
///
/// All fields have sensible empty defaults so the stage can proceed even when
/// the project has no DB-level role rows configured.
pub(crate) struct ResolvedRoleOverrides {
    /// The effective runtime role whose template renders the base prompt.
    /// Same as the flow-enum role unless a specialist override switched in a
    /// different `base_role`.
    pub runtime_role: Arc<dyn AgentRole>,
    /// Role-level system prompt extensions (project default row or specialist).
    /// Empty string when no DB row exists.
    pub system_prompt_extensions: String,
    /// Learned-prompt amendments from `learned_prompt_history`.
    pub learned_prompt: Option<String>,
    /// MCP server names from the DB row's `mcp_servers` JSON array.
    /// `None` means the DB row is absent (so settings defaults apply);
    /// `Some(vec)` — including `Some(vec![])` — is an explicit override.
    pub mcp_servers: Option<Vec<String>>,
    /// Skill names from the DB row's `skills` JSON array.  Empty when no DB
    /// row exists.
    pub skills: Vec<String>,
    /// Role-level override for the project's verification command.
    pub verification_command: Option<String>,
    /// Role-level `model_preference` (consulted by the supervisor-runner for
    /// `TaskRunSpec::model_id_per_role` seeding; threaded here for
    /// completeness and future use).
    pub model_preference: Option<String>,
    /// `true` when a specialist override swapped the runtime role (i.e. the
    /// stage is no longer rendering the injected RoleKind's template).
    /// Drives the `role_for_epic_check` decision in the caller.
    pub specialist_overrode_runtime_role: bool,
}

impl ResolvedRoleOverrides {
    /// Build an empty `ResolvedRoleOverrides` bound to `runtime_role`.
    fn empty(runtime_role: Arc<dyn AgentRole>) -> Self {
        Self {
            runtime_role,
            system_prompt_extensions: String::new(),
            learned_prompt: None,
            mcp_servers: None,
            skills: Vec::new(),
            verification_command: None,
            model_preference: None,
            specialist_overrode_runtime_role: false,
        }
    }

    /// Populate the override fields from a DB `Agent` row, keeping
    /// `runtime_role` and `specialist_overrode_runtime_role` untouched.
    fn apply_agent_fields(&mut self, agent: &Agent) {
        self.system_prompt_extensions = agent.system_prompt_extensions.clone();
        self.learned_prompt = agent.learned_prompt.clone();
        self.mcp_servers = Some(parse_json_array(&agent.mcp_servers));
        self.skills = parse_json_array(&agent.skills);
        self.verification_command = agent.verification_command.clone();
        self.model_preference = agent.model_preference.clone();
    }
}

/// Map a [`RoleKind`] to the in-tree [`AgentType`] used by `role_impl_for`.
///
/// `RoleKind::Verifier` maps to `AgentType::Worker` to preserve the
/// `stage::role_arc_for` behaviour (verifier stage currently reuses the
/// worker role impl).
fn agent_type_for_role_kind(kind: RoleKind) -> AgentType {
    match kind {
        RoleKind::Planner => AgentType::Planner,
        RoleKind::Worker => AgentType::Worker,
        RoleKind::Reviewer => AgentType::Reviewer,
        RoleKind::Verifier => AgentType::Worker,
        RoleKind::Architect => AgentType::Architect,
    }
}

/// Resolve the effective role configuration for one stage of a supervisor
/// task-run.
///
/// Mirrors the legacy `run_task_lifecycle` logic:
///
/// 1. If `task.agent_type` names a specialist that resolves to an `Agent`
///    row, the specialist wins: its `base_role` picks the runtime
///    `AgentRole`, and its field values (prompt extensions, learned prompt,
///    MCP servers, skills, verification command, model preference) populate
///    every override slot.  If the specialist's `base_role` string fails to
///    parse, we keep the injected role but still pick up the specialist's
///    override fields (legacy-parity behaviour — `AgentType::from_str`
///    failure was logged and the role was left alone).
///
///    Specialist override is only consulted for the Worker stage because
///    specialists are defined as worker specializations (see
///    `role_for_task_dispatch` in `roles/mod.rs`, which limits `task.agent_type`
///    routing to the five base roles).  Other stages (Planner/Reviewer/
///    Verifier/Architect) always use the default-role path.
///
/// 2. Otherwise, load the project's `is_default = 1` row for the RoleKind's
///    `base_role`.  A present row populates the override slots but leaves
///    `runtime_role` alone.  A missing row yields all-empty defaults.
///
/// DB errors on either path are logged at `warn` and fall through to empty
/// defaults — the stage never fails to run because of a role-config lookup.
pub(crate) async fn resolve_role_overrides(
    task: &Task,
    role_kind: RoleKind,
    app_state: &AgentContext,
) -> ResolvedRoleOverrides {
    let injected_agent_type = agent_type_for_role_kind(role_kind);
    let injected_role = role_impl_for(injected_agent_type);
    let mut out = ResolvedRoleOverrides::empty(injected_role.clone());

    let role_repo = AgentRepository::new(app_state.db.clone(), app_state.event_bus.clone());

    // ── Specialist override path ─────────────────────────────────────────
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
                        let specialist_role = role_impl_for(agent_type);
                        out.runtime_role = specialist_role;
                        out.specialist_overrode_runtime_role =
                            agent_type != injected_agent_type;
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
                return out;
            }
            Ok(None) => {
                tracing::debug!(
                    task_id = %task.short_id,
                    specialist = %specialist_name,
                    "Stage: task.agent_type does not resolve to a specialist row; \
                     falling back to default role config"
                );
                // Fall through to the default-role path.
            }
            Err(e) => {
                tracing::warn!(
                    task_id = %task.short_id,
                    specialist = %specialist_name,
                    error = %e,
                    "Stage: failed to load specialist agent row; falling back to default role config"
                );
                // Fall through to the default-role path.
            }
        }
    }

    // ── Default role config path ─────────────────────────────────────────
    let base_role = injected_agent_type.as_str();
    match role_repo
        .get_default_for_base_role(&task.project_id, base_role)
        .await
    {
        Ok(Some(agent)) => {
            out.apply_agent_fields(&agent);
        }
        Ok(None) => {
            // No project-level default configured — legit for fresh projects
            // and for RoleKinds that have no DB row (e.g. architect before a
            // user configures one).  Empty overrides are the right answer.
        }
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
    use djinn_db::{Database, ProjectRepository};
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
        let task = repo
            .create_in_project(
                project_id,
                None,
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
        // `ProjectRepository::create` seeds a default row for every base_role
        // with empty JSON fields. That row populates `mcp_servers =
        // Some(vec![])` / `skills = vec![]` / empty extensions — the "no
        // project-level customisation" baseline.
        let db = crate::test_helpers::create_test_db();
        let project_id = setup_project(&db).await;
        let task = make_task(&db, &project_id, None).await;
        let ctx = agent_context(db);
        let out = resolve_role_overrides(&task, RoleKind::Worker, &ctx).await;
        assert_eq!(out.system_prompt_extensions, "");
        assert!(out.learned_prompt.is_none());
        assert_eq!(out.mcp_servers, Some(Vec::<String>::new()));
        assert!(out.skills.is_empty());
        assert!(out.verification_command.is_none());
        assert!(out.model_preference.is_none());
        assert!(!out.specialist_overrode_runtime_role);
        // Injected role stays put.
        assert_eq!(out.runtime_role.config().name, "worker");
    }

    #[tokio::test]
    async fn default_worker_row_populates_overrides() {
        let db = crate::test_helpers::create_test_db();
        let project_id = setup_project(&db).await;
        let role_repo = AgentRepository::new(db.clone(), EventBus::noop());
        // Clear the auto-seeded worker row so we can install one with real
        // overrides in its place.
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
                    verification_command: Some("cargo test"),
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
        assert_eq!(out.verification_command.as_deref(), Some("cargo test"));
        assert_eq!(out.model_preference.as_deref(), Some("claude-opus-4-6"));
        assert!(!out.specialist_overrode_runtime_role);
        assert_eq!(out.runtime_role.config().name, "worker");
    }

    #[tokio::test]
    async fn worker_specialist_override_swaps_role_and_fields() {
        let db = crate::test_helpers::create_test_db();
        let project_id = setup_project(&db).await;
        let role_repo = AgentRepository::new(db.clone(), EventBus::noop());
        // Create a specialist under base_role=planner so the runtime role
        // swaps from Worker → Planner, proving the specialist_overrode_…
        // flag flips correctly.
        role_repo
            .create_for_project(
                &project_id,
                AgentCreateInput {
                    name: "senior-planner",
                    base_role: "planner",
                    description: "planner specialist",
                    system_prompt_extensions: "plan carefully",
                    model_preference: None,
                    verification_command: None,
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
        // `Some(vec![])` is an explicit opt-out of MCP defaults (distinct
        // from `None`).
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
                    verification_command: None,
                    mcp_servers: Some("[]"),
                    skills: Some("[]"),
                    is_default: false,
                },
            )
            .await
            .unwrap();
        // Customise the auto-seeded default planner row so we can verify the
        // planner-stage fallback reads from it.
        role_repo
            .set_default_system_prompt_extensions(&project_id, "planner", "default-planner-ext")
            .await
            .unwrap();

        let task = make_task(&db, &project_id, Some("rust-expert")).await;
        let ctx = agent_context(db);
        let out = resolve_role_overrides(&task, RoleKind::Planner, &ctx).await;
        // Planner stage ignores the specialist and falls back to default.
        assert_eq!(out.runtime_role.config().name, "planner");
        assert!(!out.specialist_overrode_runtime_role);
        assert_eq!(out.system_prompt_extensions, "default-planner-ext");
    }

    #[tokio::test]
    async fn missing_specialist_falls_back_to_default() {
        let db = crate::test_helpers::create_test_db();
        let project_id = setup_project(&db).await;
        // Customise the auto-seeded default worker row so the fallback has
        // something non-empty to assert against.
        let role_repo = AgentRepository::new(db.clone(), EventBus::noop());
        role_repo
            .set_default_system_prompt_extensions(&project_id, "worker", "default-worker-ext")
            .await
            .unwrap();
        // task.agent_type points at a name that does not exist.
        let task = make_task(&db, &project_id, Some("nonexistent-expert")).await;
        let ctx = agent_context(db);
        let out = resolve_role_overrides(&task, RoleKind::Worker, &ctx).await;
        assert_eq!(out.runtime_role.config().name, "worker");
        assert!(!out.specialist_overrode_runtime_role);
        assert_eq!(out.system_prompt_extensions, "default-worker-ext");
    }
}
