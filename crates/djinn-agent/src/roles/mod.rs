use super::AgentType;
use crate::context::AgentContext;
use crate::output_parser::ParsedAgentOutput;
use crate::prompts::TaskContext;
use djinn_core::models::{Task, TransitionAction};
use futures::future::BoxFuture;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

pub mod finalize;
mod groomer;
mod pm;
mod reviewer;
mod worker;

pub(crate) use groomer::{GROOMER_CONFIG, GroomerRole};
pub(crate) use pm::{PM_CONFIG, PmRole};
pub(crate) use reviewer::{TASK_REVIEWER_CONFIG, TaskReviewerRole};
pub(crate) use worker::{WORKER_CONFIG, WorkerRole};

#[derive(Clone, Copy)]
pub(crate) struct RoleConfig {
    pub(crate) name: &'static str,
    pub(crate) display_name: &'static str,
    pub(crate) dispatch_role: &'static str,
    pub(crate) tool_schemas: fn() -> Vec<serde_json::Value>,
    pub(crate) start_action: fn(&str) -> Option<TransitionAction>,
    pub(crate) release_action: fn() -> TransitionAction,
    pub(crate) initial_message: &'static str,
    pub(crate) preserves_session: bool,
    pub(crate) is_project_scoped: bool,
    /// Tool names the agent can call to signal completion for this role.
    /// The first entry is the primary finalize tool; additional entries are
    /// alternate exit paths (e.g. `request_pm` for workers).
    pub(crate) finalize_tool_names: &'static [&'static str],
}

pub(crate) fn config_for(agent_type: AgentType) -> &'static RoleConfig {
    match agent_type {
        AgentType::Worker => &WORKER_CONFIG,
        AgentType::TaskReviewer => &TASK_REVIEWER_CONFIG,
        AgentType::PM => &PM_CONFIG,
        AgentType::Groomer => &GROOMER_CONFIG,
    }
}

/// Thin role trait that every agent role must implement.
///
/// Object-safe: async methods return `BoxFuture` so `dyn AgentRole` works.
pub(crate) trait AgentRole: Send + Sync + 'static {
    fn config(&self) -> &RoleConfig;
    fn render_prompt(&self, task: &Task, ctx: &TaskContext) -> String;
    fn on_complete<'a>(
        &'a self,
        task_id: &'a str,
        output: &'a ParsedAgentOutput,
        app_state: &'a AgentContext,
    ) -> BoxFuture<'a, Option<(TransitionAction, Option<String>)>>;
    fn prepare_worktree<'a>(
        &'a self,
        _worktree: &'a Path,
        _task: &'a Task,
        _app_state: &'a AgentContext,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    /// The primary MCP tool name this role uses to signal session completion.
    fn finalize_tool_name(&self) -> &'static str {
        self.config().finalize_tool_names.first().copied().unwrap_or("")
    }
    /// Whether this role should build epic context for the prompt.
    fn needs_epic_context(&self) -> bool {
        false
    }
    /// Build the initial user message for a fresh session.
    /// Workers override this to include recent feedback from the activity log.
    fn initial_user_message<'a>(
        &'a self,
        _task_id: &'a str,
        _app_state: &'a AgentContext,
    ) -> BoxFuture<'a, String> {
        Box::pin(async {
            "Start by understanding the task context and execute it fully before stopping."
                .to_string()
        })
    }
}

/// Return the finalize tool name for the given agent type.
///
/// This is the tool name the agent must call to signal session completion.
/// Convenience wrapper over `role_impl_for(agent_type).finalize_tool_name()`.
pub fn finalize_tool_name_for(agent_type: AgentType) -> &'static str {
    role_impl_for(agent_type).finalize_tool_name()
}

/// Resolve the concrete `AgentRole` implementation for an `AgentType`.
pub(crate) fn role_impl_for(agent_type: AgentType) -> Arc<dyn AgentRole> {
    match agent_type {
        AgentType::Worker => Arc::new(WorkerRole),
        AgentType::TaskReviewer => Arc::new(TaskReviewerRole),
        AgentType::PM => Arc::new(PmRole),
        AgentType::Groomer => Arc::new(GroomerRole),
    }
}

/// Resolve `Arc<dyn AgentRole>` directly from a task and dispatch context,
/// without exposing `AgentType` to the caller.
pub(crate) fn role_for_task_dispatch(
    task: &Task,
    _has_conflict_context: bool,
) -> Arc<dyn AgentRole> {
    role_impl_for(AgentType::for_task_status(task.status.as_str(), false))
}

#[derive(Default)]
pub(crate) struct DispatchContext;

pub(crate) struct DispatchRule {
    pub(crate) role_name: &'static str,
    pub(crate) claims: fn(&Task, &DispatchContext) -> bool,
}

pub struct RoleRegistry {
    pub(crate) roles: HashMap<&'static str, AgentType>,
    pub(crate) dispatch_rules: Vec<DispatchRule>,
}

impl Default for RoleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl RoleRegistry {
    pub fn new() -> Self {
        let roles = HashMap::from([
            ("worker", AgentType::Worker),
            ("task_reviewer", AgentType::TaskReviewer),
            ("pm", AgentType::PM),
            ("groomer", AgentType::Groomer),
        ]);

        let dispatch_rules = vec![
            worker_dispatch_rule(),
            task_reviewer_dispatch_rule(),
            pm_dispatch_rule(),
            groomer_dispatch_rule(),
        ];

        Self {
            roles,
            dispatch_rules,
        }
    }

    pub(crate) fn role_for_task(&self, task: &Task, ctx: &DispatchContext) -> Option<&'static str> {
        self.dispatch_rules
            .iter()
            .find(|rule| (rule.claims)(task, ctx))
            .map(|rule| rule.role_name)
    }
    /// Unique model-pool role names (dispatch_role from RoleConfig).
    pub(crate) fn model_pool_roles(&self) -> Vec<&'static str> {
        let mut seen = HashSet::new();
        self.roles
            .values()
            .filter_map(|at| {
                let dr = config_for(*at).dispatch_role;
                seen.insert(dr).then_some(dr)
            })
            .collect()
    }

    /// Get the model-pool role (dispatch_role) for a task.
    pub(crate) fn dispatch_role_for_task(
        &self,
        task: &Task,
        ctx: &DispatchContext,
    ) -> Option<&'static str> {
        let role_name = self.role_for_task(task, ctx)?;
        let agent_type = self.roles.get(role_name)?;
        Some(config_for(*agent_type).dispatch_role)
    }
}

fn worker_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    !matches!(
        task.status.as_str(),
        "needs_task_review" | "in_task_review" | "needs_pm_intervention" | "in_pm_intervention"
    )
}

fn worker_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "worker",
        claims: worker_claims,
    }
}

fn task_reviewer_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    matches!(task.status.as_str(), "needs_task_review" | "in_task_review")
}

fn task_reviewer_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "task_reviewer",
        claims: task_reviewer_claims,
    }
}

fn pm_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    matches!(
        task.status.as_str(),
        "needs_pm_intervention" | "in_pm_intervention"
    )
}

fn pm_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "pm",
        claims: pm_claims,
    }
}

fn groomer_claims(_task: &Task, _ctx: &DispatchContext) -> bool {
    false
}

fn groomer_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "groomer",
        claims: groomer_claims,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::models::Task;

    fn make_task(status: &str) -> Task {
        Task {
            id: "task-123".into(),
            project_id: "project-1".into(),
            short_id: "t123".into(),
            epic_id: None,
            title: "Test task".into(),
            description: "Test description".into(),
            design: "Test design".into(),
            issue_type: "task".into(),
            status: status.into(),
            priority: 1,
            owner: "dev@example.com".into(),
            labels: "[]".into(),
            acceptance_criteria: "[]".into(),
            reopen_count: 0,
            continuation_count: 0,
            verification_failure_count: 0,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            closed_at: None,
            close_reason: None,
            merge_commit_sha: None,
            memory_refs: "[]".into(),
            unresolved_blocker_count: 0,
        }
    }

    #[test]
    fn open_task_with_conflict_context_dispatches_to_worker() {
        let registry = RoleRegistry::new();
        let ctx = DispatchContext;

        // Test that open tasks dispatch to worker regardless of conflict context
        let task = make_task("open");
        let role = registry.role_for_task(&task, &ctx);
        assert_eq!(role, Some("worker"), "open task should dispatch to worker");

        // Verify the dispatch_role is "worker"
        let dispatch_role = registry.dispatch_role_for_task(&task, &ctx);
        assert_eq!(
            dispatch_role,
            Some("worker"),
            "open task should have worker dispatch role"
        );
    }

    #[test]
    fn in_progress_task_dispatches_to_worker() {
        let registry = RoleRegistry::new();
        let ctx = DispatchContext;

        let task = make_task("in_progress");
        let role = registry.role_for_task(&task, &ctx);
        assert_eq!(role, Some("worker"));
    }

    #[test]
    fn task_reviewer_statuses_dispatches_to_task_reviewer() {
        let registry = RoleRegistry::new();
        let ctx = DispatchContext;

        for status in ["needs_task_review", "in_task_review"] {
            let task = make_task(status);
            let role = registry.role_for_task(&task, &ctx);
            assert_eq!(
                role,
                Some("task_reviewer"),
                "{status} should dispatch to task_reviewer"
            );
        }
    }

    #[test]
    fn pm_intervention_statuses_dispatches_to_pm() {
        let registry = RoleRegistry::new();
        let ctx = DispatchContext;

        for status in ["needs_pm_intervention", "in_pm_intervention"] {
            let task = make_task(status);
            let role = registry.role_for_task(&task, &ctx);
            assert_eq!(role, Some("pm"), "{status} should dispatch to pm");
        }
    }

    #[test]
    fn role_for_task_dispatch_returns_worker_role() {
        let task = make_task("open");
        // Test that conflict-context tasks route to Worker (not a dedicated conflict_resolver)
        let role = role_for_task_dispatch(&task, true);
        assert_eq!(
            role.config().name,
            "worker",
            "conflict context task should dispatch to worker role"
        );

        // Also test without conflict context
        let role_no_conflict = role_for_task_dispatch(&task, false);
        assert_eq!(role_no_conflict.config().name, "worker");
    }
}
