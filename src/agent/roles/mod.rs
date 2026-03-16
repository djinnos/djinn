use super::AgentType;
use crate::agent::output_parser::ParsedAgentOutput;
use crate::agent::prompts::TaskContext;
use crate::models::{Task, TransitionAction};
use crate::server::AppState;
use futures::future::BoxFuture;
use std::collections::{HashMap, HashSet};
use std::path::Path;

mod conflict;
mod groomer;
mod pm;
mod reviewer;
mod worker;

pub(crate) use conflict::CONFLICT_RESOLVER_CONFIG;
pub(crate) use groomer::GROOMER_CONFIG;
pub(crate) use pm::PM_CONFIG;
pub(crate) use reviewer::TASK_REVIEWER_CONFIG;
pub(crate) use worker::WORKER_CONFIG;

#[derive(Clone, Copy)]
#[allow(dead_code)]
pub(crate) struct CompactionPrompts {
    pub(crate) mid_session: &'static str,
    pub(crate) mid_session_system: &'static str,
    pub(crate) pre_resume: &'static str,
    pub(crate) pre_resume_system: &'static str,
}

#[derive(Clone, Copy)]
pub(crate) struct RoleConfig {
    pub(crate) name: &'static str,
    pub(crate) display_name: &'static str,
    pub(crate) dispatch_role: &'static str,
    pub(crate) tool_schemas: fn() -> Vec<serde_json::Value>,
    pub(crate) start_action: fn(&str) -> Option<TransitionAction>,
    pub(crate) release_action: fn() -> TransitionAction,
    pub(crate) initial_message: &'static str,
    #[allow(dead_code)]
    pub(crate) compaction: CompactionPrompts,
    #[allow(dead_code)]
    pub(crate) preserves_session: bool,
    pub(crate) is_project_scoped: bool,
}

pub(crate) fn config_for(agent_type: AgentType) -> &'static RoleConfig {
    match agent_type {
        AgentType::Worker => &WORKER_CONFIG,
        AgentType::ConflictResolver => &CONFLICT_RESOLVER_CONFIG,
        AgentType::TaskReviewer => &TASK_REVIEWER_CONFIG,
        AgentType::PM => &PM_CONFIG,
        AgentType::Groomer => &GROOMER_CONFIG,
    }
}

/// Thin role trait that every agent role must implement.
///
/// Object-safe: async methods return `BoxFuture` so `dyn AgentRole` works.
#[allow(dead_code)]
pub(crate) trait AgentRole: Send + Sync + 'static {
    fn config(&self) -> &RoleConfig;
    fn render_prompt(&self, ctx: &TaskContext) -> String;
    fn on_complete<'a>(
        &'a self,
        task_id: &'a str,
        output: &'a ParsedAgentOutput,
        app_state: &'a AppState,
    ) -> BoxFuture<'a, Option<(TransitionAction, Option<String>)>>;
    fn prepare_worktree<'a>(
        &'a self,
        _worktree: &'a Path,
        _task: &'a Task,
        _app_state: &'a AppState,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Default)]
pub(crate) struct DispatchContext {
    pub(crate) has_conflict_context: bool,
}

pub(crate) struct DispatchRule {
    pub(crate) role_name: &'static str,
    pub(crate) claims: fn(&Task, &DispatchContext) -> bool,
}

pub struct RoleRegistry {
    pub(crate) roles: HashMap<&'static str, AgentType>,
    pub(crate) dispatch_rules: Vec<DispatchRule>,
}

impl RoleRegistry {
    pub fn new() -> Self {
        let roles = HashMap::from([
            ("worker", AgentType::Worker),
            ("conflict_resolver", AgentType::ConflictResolver),
            ("task_reviewer", AgentType::TaskReviewer),
            ("pm", AgentType::PM),
            ("groomer", AgentType::Groomer),
        ]);

        let dispatch_rules = vec![
            conflict_resolver_dispatch_rule(),
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

    #[allow(dead_code)]
    pub(crate) fn dispatch_roles(&self) -> Vec<&'static str> {
        self.dispatch_rules
            .iter()
            .map(|r| r.role_name)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    }

    #[allow(dead_code)]
    pub(crate) fn dispatch_rule_for_role(&self, role_name: &str) -> Option<&DispatchRule> {
        self.dispatch_rules
            .iter()
            .find(|rule| rule.role_name == role_name)
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

fn conflict_resolver_claims(task: &Task, ctx: &DispatchContext) -> bool {
    task.status == "open" && ctx.has_conflict_context
}

fn conflict_resolver_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "conflict_resolver",
        claims: conflict_resolver_claims,
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
