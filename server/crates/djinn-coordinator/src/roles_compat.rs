//! Compatibility shim: `RoleRegistry` and `DispatchContext`.
//!
//! These mirror the definitions in `djinn-agent::roles` for coordinator
//! dispatch logic. The coordinator only uses `RoleRegistry::role_for_task`
//! and `DispatchContext`.

use djinn_core::models::Task;
use djinn_roles::AgentType;

/// Marker struct for dispatch context.
pub struct DispatchContext;

/// Registry that maps tasks to agent roles.
///
/// This is a minimal compatibility shim. The full `RoleRegistry` in
/// `djinn-agent` has role implementations (AgentRole trait objects);
/// this shim delegates to `djinn_roles` where possible.
pub struct RoleRegistry {
    /// Static role lookup table.
    entries: Vec<RoleEntry>,
}

struct RoleEntry {
    agent_type: AgentType,
    claims: fn(&Task, &DispatchContext) -> bool,
}

impl RoleRegistry {
    pub fn new() -> Self {
        Self {
            entries: vec![
                // ADR-051 §1 + §8: review tasks (escalation + intervention) are
                // Planner-owned.  This rule must come before the architect
                // rule so spike tasks still fall through to Architect.
                RoleEntry {
                    agent_type: AgentType::Planner,
                    claims: planner_review_claims,
                },
                // Architect claims spike tasks (open status) — the
                // on-demand consultant loop per ADR-051 §2.
                RoleEntry {
                    agent_type: AgentType::Architect,
                    claims: architect_claims,
                },
                // Planning / decomposition tasks go to Planner.
                RoleEntry {
                    agent_type: AgentType::Planner,
                    claims: planning_claims,
                },
                RoleEntry {
                    agent_type: AgentType::Worker,
                    claims: worker_claims,
                },
                RoleEntry {
                    agent_type: AgentType::Reviewer,
                    claims: reviewer_claims,
                },
                RoleEntry {
                    agent_type: AgentType::Lead,
                    claims: lead_claims,
                },
                RoleEntry {
                    agent_type: AgentType::Planner,
                    claims: planner_fallback_claims,
                },
            ],
        }
    }

    /// Resolve the role for a task.
    pub fn role_for_task(&self, task: &Task, ctx: &DispatchContext) -> Option<&'static str> {
        for entry in &self.entries {
            if (entry.claims)(task, ctx) {
                return Some(entry.agent_type.dispatch_role());
            }
        }
        None
    }

    /// Return the dispatch role string for a task (convenience wrapper).
    pub fn dispatch_role_for_task(
        &self,
        task: &Task,
        ctx: &DispatchContext,
    ) -> Option<&'static str> {
        self.role_for_task(task, ctx)
    }
}

fn architect_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    matches!(task.status.as_str(), "open" | "in_progress")
        && matches!(task.issue_type.as_str(), "spike")
}

/// Review tasks dispatch as Planner (ADR-051 §1 + §8).
fn planner_review_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    matches!(task.status.as_str(), "open" | "in_progress") && task.issue_type.as_str() == "review"
}

/// Planning / decomposition / epic_breakdown tasks dispatch as Planner.
fn planning_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    matches!(
        task.issue_type.as_str(),
        "planning" | "decomposition" | "epic_breakdown"
    )
}

fn lead_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    task.status == "needs_lead_intervention" || task.status == "in_lead_intervention"
}

fn reviewer_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    task.status == "needs_task_review" || task.status == "in_task_review"
}

/// Worker claims non-specialist open/in-progress tasks: not spike, not
/// review, not planning/decomposition/epic_breakdown, and not in a
/// review/lead-intervention status.
fn worker_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    !matches!(
        task.status.as_str(),
        "needs_task_review" | "in_task_review" | "needs_lead_intervention" | "in_lead_intervention"
    ) && !matches!(
        task.issue_type.as_str(),
        "spike" | "review" | "planning" | "decomposition" | "epic_breakdown"
    )
}

/// Planner fallback — never claims anything (the planner-specific rules
/// above handle review and planning tasks; this is the catch-all entry
/// that matches the original `planner_claims` that always returns false).
fn planner_fallback_claims(_task: &Task, _ctx: &DispatchContext) -> bool {
    false
}
