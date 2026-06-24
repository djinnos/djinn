//! Role registry and dispatch rules for the coordinator.
//!
//! Contains [`RoleRegistry`] and [`DispatchContext`] — the coordinator's
//! dispatch-time role resolution.  Mirrors `djinn_agent::roles` but
//! without the tool-schema initialization seam (that lives in `djinn-agent`).

use std::collections::{HashMap, HashSet};

use djinn_core::models::Task;
use djinn_roles::AgentType;
use djinn_roles::config::config_for;

/// Marker context for dispatch rule evaluation.
///
/// Currently empty; reserved for future per-dispatch metadata (e.g.
/// user-level overrides, conflict context).
#[derive(Default)]
pub struct DispatchContext;

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
            ("reviewer", AgentType::Reviewer),
            ("lead", AgentType::Lead),
            ("planner", AgentType::Planner),
            ("architect", AgentType::Architect),
            // Tribunal refinement roles (k9zw).
            ("advocate", AgentType::Advocate),
            ("adversary", AgentType::Adversary),
            ("judge", AgentType::Judge),
        ]);

        let dispatch_rules = vec![
            // ADR-051 §1 + §8: review tasks (escalation + intervention) are
            // Planner-owned.  This rule must come before the architect
            // rule so spike tasks still fall through to Architect.
            planner_review_dispatch_rule(),
            // Architect claims spike tasks (open status) — the
            // on-demand consultant loop per ADR-051 §2.
            architect_dispatch_rule(),
            // Planning / decomposition tasks go to Planner.
            planning_dispatch_rule(),
            worker_dispatch_rule(),
            reviewer_dispatch_rule(),
            lead_dispatch_rule(),
            planner_dispatch_rule(),
        ];

        Self {
            roles,
            dispatch_rules,
        }
    }

    pub fn role_for_task(&self, task: &Task, ctx: &DispatchContext) -> Option<&'static str> {
        self.dispatch_rules
            .iter()
            .find(|rule| (rule.claims)(task, ctx))
            .map(|rule| rule.role_name)
    }

    /// Unique model-pool role names (dispatch_role from RoleConfig).
    #[allow(dead_code)]
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

/// Returns `true` if the task is an open/in-progress spike — the
/// Architect's on-demand consultant loop (ADR-051 §2).
fn architect_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    matches!(task.status.as_str(), "open" | "in_progress")
        && matches!(task.issue_type.as_str(), "spike")
}

fn architect_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "architect",
        claims: architect_claims,
    }
}

/// Returns `true` if the task is an open/in-progress review task.
fn planner_review_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    matches!(task.status.as_str(), "open" | "in_progress") && task.issue_type.as_str() == "review"
}

fn planner_review_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "planner",
        claims: planner_review_claims,
    }
}

/// Returns `true` if the task's `issue_type` is `planning`.
fn planning_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    matches!(
        task.issue_type.as_str(),
        "planning" | "decomposition" | "epic_breakdown"
    )
}

fn planning_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "planner",
        claims: planning_claims,
    }
}

fn worker_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    !matches!(
        task.status.as_str(),
        "needs_task_review" | "in_task_review" | "needs_lead_intervention" | "in_lead_intervention"
    ) && !matches!(
        task.issue_type.as_str(),
        "spike" | "review" | "planning" | "decomposition" | "epic_breakdown"
    )
}

fn worker_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "worker",
        claims: worker_claims,
    }
}

fn reviewer_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    matches!(task.status.as_str(), "needs_task_review" | "in_task_review")
}

fn reviewer_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "reviewer",
        claims: reviewer_claims,
    }
}

fn lead_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    matches!(
        task.status.as_str(),
        "needs_lead_intervention" | "in_lead_intervention"
    )
}

fn lead_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "lead",
        claims: lead_claims,
    }
}

fn planner_claims(_task: &Task, _ctx: &DispatchContext) -> bool {
    false
}

fn planner_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "planner",
        claims: planner_claims,
    }
}
