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
                RoleEntry {
                    agent_type: AgentType::Architect,
                    claims: architect_claims,
                },
                RoleEntry {
                    agent_type: AgentType::Planner,
                    claims: planner_claims,
                },
                RoleEntry {
                    agent_type: AgentType::Lead,
                    claims: lead_claims,
                },
                RoleEntry {
                    agent_type: AgentType::Reviewer,
                    claims: reviewer_claims,
                },
                RoleEntry {
                    agent_type: AgentType::Worker,
                    claims: worker_claims,
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
    task.issue_type == "task" && task.status == "open" && task.design.is_empty()
}

fn planner_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    task.status == "needs_planner"
        || task.status == "in_planner"
        || (task.issue_type == "task"
            && task.status == "open"
            && !task.design.is_empty()
            && task.description.is_empty())
}

fn lead_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    task.status == "needs_lead_intervention" || task.status == "in_lead_intervention"
}

fn reviewer_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    task.status == "needs_task_review" || task.status == "in_task_review"
}

fn worker_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    task.status == "open" || task.status == "in_progress"
}
