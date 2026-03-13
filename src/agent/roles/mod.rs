#![allow(dead_code)]
use super::AgentType;
use crate::models::{Task, TransitionAction};
use std::collections::{HashMap, HashSet};

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
pub(crate) struct CompactionPrompts {
    pub(crate) mid_session: &'static str,
    pub(crate) mid_session_system: &'static str,
    pub(crate) pre_resume: &'static str,
    pub(crate) pre_resume_system: &'static str,
}

#[derive(Clone, Copy)]
pub(crate) struct RoleConfig {
    pub(crate) name: &'static str,
    pub(crate) dispatch_role: &'static str,
    pub(crate) tool_schemas: fn() -> Vec<serde_json::Value>,
    pub(crate) initial_message: &'static str,
    pub(crate) compaction: CompactionPrompts,
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

#[derive(Default)]
pub(crate) struct DispatchContext {
    pub(crate) has_conflict_context: bool,
}

pub(crate) struct DispatchRule {
    pub(crate) role_name: &'static str,
    pub(crate) claims: fn(&Task, &DispatchContext) -> bool,
    pub(crate) start_action: fn(&str) -> Option<TransitionAction>,
    pub(crate) release_action: TransitionAction,
}

pub(crate) struct RoleRegistry {
    pub(crate) roles: HashMap<&'static str, AgentType>,
    pub(crate) dispatch_rules: Vec<DispatchRule>,
}

impl RoleRegistry {
    pub(crate) fn new() -> Self {
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

        Self { roles, dispatch_rules }
    }

    pub(crate) fn role_for_task(&self, task: &Task, ctx: &DispatchContext) -> Option<&'static str> {
        self.dispatch_rules
            .iter()
            .find(|rule| (rule.claims)(task, ctx))
            .map(|rule| rule.role_name)
    }

    pub(crate) fn dispatch_roles(&self) -> Vec<&'static str> {
        self.dispatch_rules
            .iter()
            .map(|r| r.role_name)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    }
}

fn worker_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    !matches!(
        task.status.as_str(),
        "needs_task_review" | "in_task_review" | "needs_pm_intervention" | "in_pm_intervention"
    )
}

fn worker_start_action(status: &str) -> Option<TransitionAction> {
    AgentType::Worker.start_action(status)
}

fn worker_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "worker",
        claims: worker_claims,
        start_action: worker_start_action,
        release_action: AgentType::Worker.release_action(),
    }
}

fn conflict_resolver_claims(task: &Task, ctx: &DispatchContext) -> bool {
    task.status == "open" && ctx.has_conflict_context
}

fn conflict_resolver_start_action(status: &str) -> Option<TransitionAction> {
    AgentType::ConflictResolver.start_action(status)
}

fn conflict_resolver_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "conflict_resolver",
        claims: conflict_resolver_claims,
        start_action: conflict_resolver_start_action,
        release_action: AgentType::ConflictResolver.release_action(),
    }
}

fn task_reviewer_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    matches!(task.status.as_str(), "needs_task_review" | "in_task_review")
}

fn task_reviewer_start_action(status: &str) -> Option<TransitionAction> {
    AgentType::TaskReviewer.start_action(status)
}

fn task_reviewer_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "task_reviewer",
        claims: task_reviewer_claims,
        start_action: task_reviewer_start_action,
        release_action: AgentType::TaskReviewer.release_action(),
    }
}

fn pm_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    matches!(task.status.as_str(), "needs_pm_intervention" | "in_pm_intervention")
}

fn pm_start_action(status: &str) -> Option<TransitionAction> {
    AgentType::PM.start_action(status)
}

fn pm_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "pm",
        claims: pm_claims,
        start_action: pm_start_action,
        release_action: AgentType::PM.release_action(),
    }
}

fn groomer_claims(_task: &Task, _ctx: &DispatchContext) -> bool {
    false
}

fn groomer_start_action(status: &str) -> Option<TransitionAction> {
    AgentType::Groomer.start_action(status)
}

fn groomer_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "groomer",
        claims: groomer_claims,
        start_action: groomer_start_action,
        release_action: AgentType::Groomer.release_action(),
    }
}
