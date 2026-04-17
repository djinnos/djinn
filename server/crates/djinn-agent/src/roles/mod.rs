use super::AgentType;
use crate::context::AgentContext;
use crate::output_parser::ParsedAgentOutput;
use crate::prompts::TaskContext;
use djinn_core::models::{Task, TransitionAction};
use futures::future::BoxFuture;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

mod architect;
pub mod finalize;
mod lead;
mod planner;
mod reviewer;
mod worker;

pub(crate) use architect::{ARCHITECT_CONFIG, ArchitectRole};
pub(crate) use lead::{LEAD_CONFIG, LeadRole};
pub(crate) use planner::{PLANNER_CONFIG, PlannerRole};
pub(crate) use reviewer::{REVIEWER_CONFIG, ReviewerRole};
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
    /// Tool names the agent can call to signal completion for this role.
    /// The first entry is the primary finalize tool; additional entries are
    /// alternate exit paths (e.g. `request_lead` for workers).
    pub(crate) finalize_tool_names: &'static [&'static str],
}

pub(crate) fn config_for(agent_type: AgentType) -> &'static RoleConfig {
    match agent_type {
        AgentType::Worker => &WORKER_CONFIG,
        AgentType::Reviewer => &REVIEWER_CONFIG,
        AgentType::Lead => &LEAD_CONFIG,
        AgentType::Planner => &PLANNER_CONFIG,
        AgentType::Architect => &ARCHITECT_CONFIG,
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
    /// The primary MCP tool name this role uses to signal session completion.
    fn finalize_tool_name(&self) -> &'static str {
        self.config()
            .finalize_tool_names
            .first()
            .copied()
            .unwrap_or("")
    }
    /// Whether this role should build epic context for the prompt.
    fn needs_epic_context(&self) -> bool {
        true
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
        AgentType::Reviewer => Arc::new(ReviewerRole),
        AgentType::Lead => Arc::new(LeadRole),
        AgentType::Planner => Arc::new(PlannerRole),
        AgentType::Architect => Arc::new(ArchitectRole),
    }
}

/// Resolve `Arc<dyn AgentRole>` directly from a task and dispatch context,
/// without exposing `AgentType` to the caller.
///
/// Routing rules:
/// - `task.agent_type` (when non-empty) always wins — the slot lifecycle
///   will reload the specialist config from the DB; this function only
///   chooses the fallback *base* role for that path.
/// - `planning` / `decomposition` → Planner (wave decomposition).
/// - `review` → Planner.  Under ADR-051 the Planner owns the board
///   patrol *and* is the escalation ceiling above Lead, so every review
///   task (patrol + `request_planner` escalation) dispatches as a
///   Planner session.  The previous rule routed reviews to Architect
///   (ADR-034), which no longer matches the role hierarchy.
/// - `spike` → Architect.  Architect is the on-demand consultant per
///   ADR-051 §2; spikes are how the Planner asks for deep code
///   reasoning.
/// - anything else → whatever `AgentType::for_task_status` decides
///   (typically Worker for open/in_progress tasks).
// Task #7: the production dispatch path no longer calls
// `role_for_task_dispatch` — flow selection is now driven by
// [`flow_for_task_dispatch`] and the supervisor resolves the concrete
// `AgentRole` per stage internally via `stage::role_arc_for`.  We keep the
// function for test coverage (and for the documented parity with
// `flow_for_task_dispatch`) but mark it dead-code in non-test builds.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn role_for_task_dispatch(
    task: &Task,
    _has_conflict_context: bool,
) -> Arc<dyn AgentRole> {
    // If the task already carries an explicit agent_type, honour it.
    // The slot lifecycle will reload the specialist from the agents
    // table, but we still need a sensible *base* role in case the
    // specialist lookup fails (e.g. orphaned agent_type string).
    if let Some(ref specialist) = task.agent_type
        && !specialist.is_empty()
        && let Some(base) = match specialist.as_str() {
            "worker" => Some(AgentType::Worker),
            "reviewer" => Some(AgentType::Reviewer),
            "lead" | "pm" => Some(AgentType::Lead),
            "planner" => Some(AgentType::Planner),
            "architect" => Some(AgentType::Architect),
            _ => None,
        }
    {
        return role_impl_for(base);
    }

    // Issue-type-specific routing takes priority over status-based routing.
    match task.issue_type.as_str() {
        "planning" | "decomposition" => return role_impl_for(AgentType::Planner),
        // ADR-051 §1 + §8: review tasks are Planner-owned (patrol +
        // lead escalation ceiling).  Previously this routed to Architect
        // per ADR-034 before the split.
        "review" => return role_impl_for(AgentType::Planner),
        // Spikes remain the Architect's territory — they are how the
        // Planner asks for deep code-structural reasoning (ADR-051 §2).
        "spike" => return role_impl_for(AgentType::Architect),
        _ => {}
    }
    role_impl_for(AgentType::for_task_status(task.status.as_str(), false))
}

/// Coordinator-side flow selector mirroring [`role_for_task_dispatch`] for the
/// supervisor-driven dispatch path (task #7 switch).
///
/// Decides which [`crate::supervisor::SupervisorFlow`] a task-run should drive
/// based on task state + ambient dispatch context (merge-conflict metadata,
/// review-response state).  Rules:
///
/// - `issue_type=spike` → [`SupervisorFlow::Spike`] (architect-only).
/// - `issue_type=planning` / `decomposition` / `review` → [`SupervisorFlow::Planning`]
///   (planner-only; these are the simple-lifecycle types — they do not flow
///   through worker/reviewer/verifier).
/// - `status=needs_task_review` / `in_task_review` → [`SupervisorFlow::ReviewResponse`]
///   (worker → reviewer → verifier; the planner stage is skipped because the
///   planner already decided `execute` on a prior run).
/// - Any conflict context (merge-conflict or post-review merge-validation) →
///   [`SupervisorFlow::ConflictRetry`] (worker → reviewer → verifier; conflict
///   fixups bypass the planner).
/// - Default → [`SupervisorFlow::NewTask`] (planner → worker → reviewer →
///   verifier, the canonical NewTask flow).
///
/// Mirrors the two-layer dispatch routing doc note (see project memory): the
/// coordinator keeps both `role_for_task_dispatch` and this flow-selector free
/// function in sync — if you change one, consider whether the other needs
/// parity.
pub(crate) fn flow_for_task_dispatch(
    task: &Task,
    has_conflict_context: bool,
    has_review_response_context: bool,
) -> crate::supervisor::SupervisorFlow {
    use crate::supervisor::SupervisorFlow;

    // Issue-type-specific routing takes priority over status-based routing,
    // matching `role_for_task_dispatch`.
    match task.issue_type.as_str() {
        "spike" => return SupervisorFlow::Spike,
        // Simple-lifecycle types — planner-only flow.
        "planning" | "decomposition" | "review" => return SupervisorFlow::Planning,
        _ => {}
    }

    // Merge-conflict retry (detected via persistent metadata or activity-log
    // fallback) bypasses the planner and re-enters the worker→reviewer→verifier
    // pipeline directly.
    if has_conflict_context {
        return SupervisorFlow::ConflictRetry;
    }

    // Review response: the reviewer rejected a prior submission or a human
    // requested more work.  The planner decision is preserved from the
    // previous run, so the new run re-enters at worker.
    if has_review_response_context
        || matches!(
            task.status.as_str(),
            "needs_task_review" | "in_task_review"
        )
    {
        return SupervisorFlow::ReviewResponse;
    }

    SupervisorFlow::NewTask
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
            ("reviewer", AgentType::Reviewer),
            ("lead", AgentType::Lead),
            ("planner", AgentType::Planner),
            ("architect", AgentType::Architect),
        ]);

        let dispatch_rules = vec![
            // ADR-051 §1 + §8: review tasks (patrol + escalation) are
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

/// Returns `true` if the task is an open/in-progress spike — the
/// Architect's on-demand consultant loop (ADR-051 §2).  Review tasks
/// are handled by `planner_review_claims` and must not reach here.
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
/// Under ADR-051 §1 + §8 the Planner owns both the board patrol and
/// the escalation ceiling above Lead, so every review task dispatches
/// as a Planner session.
fn planner_review_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    matches!(task.status.as_str(), "open" | "in_progress") && task.issue_type.as_str() == "review"
}

fn planner_review_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "planner",
        claims: planner_review_claims,
    }
}

/// Returns `true` if the task's `issue_type` is `planning` (simple lifecycle,
/// dispatched to the Planner role). Also matches legacy `decomposition` for
/// backward compatibility with existing DB rows.
fn planning_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    matches!(task.status.as_str(), "open" | "in_progress")
        && matches!(task.issue_type.as_str(), "planning" | "decomposition")
}

fn planning_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "planner",
        claims: planning_claims,
    }
}

fn worker_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    // Spike, research, review, and planning tasks with simple lifecycle
    // are handled by architect_claims / planning_claims / a direct worker path.
    // Research tasks go to the worker role (open-ended but same execution model).
    !matches!(
        task.status.as_str(),
        "needs_task_review" | "in_task_review" | "needs_lead_intervention" | "in_lead_intervention"
    ) && !matches!(
        task.issue_type.as_str(),
        "spike" | "review" | "planning" | "decomposition"
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

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::models::Task;

    fn make_task(status: &str) -> Task {
        make_task_with_type(status, "task")
    }

    fn make_task_with_type(status: &str, issue_type: &str) -> Task {
        Task {
            id: "task-123".into(),
            project_id: "project-1".into(),
            short_id: "t123".into(),
            epic_id: None,
            title: "Test task".into(),
            description: "Test description".into(),
            design: "Test design".into(),
            issue_type: issue_type.into(),
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
            pr_url: None,
            merge_conflict_metadata: None,
            memory_refs: "[]".into(),
            agent_type: None,
            unresolved_blocker_count: 0,
            total_reopen_count: 0,
            total_verification_failure_count: 0,
            intervention_count: 0,
            last_intervention_at: None,
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
    fn task_reviewer_statuses_dispatches_to_reviewer() {
        let registry = RoleRegistry::new();
        let ctx = DispatchContext;

        for status in ["needs_task_review", "in_task_review"] {
            let task = make_task(status);
            let role = registry.role_for_task(&task, &ctx);
            assert_eq!(
                role,
                Some("reviewer"),
                "{status} should dispatch to reviewer"
            );
        }
    }

    #[test]
    fn pm_intervention_statuses_dispatches_to_lead() {
        let registry = RoleRegistry::new();
        let ctx = DispatchContext;

        for status in ["needs_lead_intervention", "in_lead_intervention"] {
            let task = make_task(status);
            let role = registry.role_for_task(&task, &ctx);
            assert_eq!(role, Some("lead"), "{status} should dispatch to lead");
        }
    }

    #[test]
    fn role_for_task_dispatch_returns_worker_role() {
        let task = make_task("open");
        // Test that conflict-context tasks route to Worker
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

    #[test]
    fn spike_tasks_dispatch_to_architect_review_tasks_dispatch_to_planner() {
        // ADR-051 §1 + §8: review tasks (patrol + lead escalation)
        // are Planner-owned; spike tasks remain Architect-owned
        // (on-demand consultant loop per ADR-051 §2).
        let registry = RoleRegistry::new();
        let ctx = DispatchContext;

        for status in ["open", "in_progress"] {
            let spike = make_task_with_type(status, "spike");
            assert_eq!(
                registry.role_for_task(&spike, &ctx),
                Some("architect"),
                "spike/{status} task should dispatch to architect"
            );
            assert_eq!(
                registry.dispatch_role_for_task(&spike, &ctx),
                Some("architect"),
                "spike/{status} task should have architect dispatch_role"
            );

            let review = make_task_with_type(status, "review");
            assert_eq!(
                registry.role_for_task(&review, &ctx),
                Some("planner"),
                "review/{status} task should dispatch to planner per ADR-051"
            );
            assert_eq!(
                registry.dispatch_role_for_task(&review, &ctx),
                Some("planner"),
                "review/{status} task should have planner dispatch_role"
            );
        }
    }

    #[test]
    fn planning_tasks_dispatch_to_planner() {
        let registry = RoleRegistry::new();
        let ctx = DispatchContext;

        for status in ["open", "in_progress"] {
            let task = make_task_with_type(status, "planning");
            let role = registry.role_for_task(&task, &ctx);
            assert_eq!(
                role,
                Some("planner"),
                "planning/{status} task should dispatch to planner"
            );
        }
    }

    #[test]
    fn legacy_decomposition_tasks_dispatch_to_planner() {
        let registry = RoleRegistry::new();
        let ctx = DispatchContext;

        // Backward compat: existing DB rows with "decomposition" still route to planner.
        let task = make_task_with_type("open", "decomposition");
        let role = registry.role_for_task(&task, &ctx);
        assert_eq!(role, Some("planner"));
    }

    #[test]
    fn research_tasks_dispatch_to_worker() {
        let registry = RoleRegistry::new();
        let ctx = DispatchContext;

        // Research uses the simple lifecycle but still goes to the worker role.
        for status in ["open", "in_progress"] {
            let task = make_task_with_type(status, "research");
            let role = registry.role_for_task(&task, &ctx);
            assert_eq!(
                role,
                Some("worker"),
                "research/{status} task should dispatch to worker"
            );
        }
    }

    #[test]
    fn registry_includes_architect_role() {
        let registry = RoleRegistry::new();
        assert!(
            registry.roles.contains_key("architect"),
            "RoleRegistry should contain 'architect'"
        );
        let model_pool_roles = registry.model_pool_roles();
        assert!(
            model_pool_roles.contains(&"architect"),
            "model_pool_roles should include 'architect'"
        );
    }

    // ── flow_for_task_dispatch ────────────────────────────────────────────────

    #[test]
    fn flow_for_spike_is_spike() {
        use crate::supervisor::SupervisorFlow;
        let task = make_task_with_type("open", "spike");
        assert_eq!(
            flow_for_task_dispatch(&task, false, false),
            SupervisorFlow::Spike
        );
    }

    #[test]
    fn flow_for_planning_is_planning() {
        use crate::supervisor::SupervisorFlow;
        let task = make_task_with_type("open", "planning");
        assert_eq!(
            flow_for_task_dispatch(&task, false, false),
            SupervisorFlow::Planning
        );
        // Legacy `decomposition` alias routes the same way.
        let legacy = make_task_with_type("open", "decomposition");
        assert_eq!(
            flow_for_task_dispatch(&legacy, false, false),
            SupervisorFlow::Planning
        );
        // Review tasks are simple-lifecycle / planner-only.
        let review = make_task_with_type("open", "review");
        assert_eq!(
            flow_for_task_dispatch(&review, false, false),
            SupervisorFlow::Planning
        );
    }

    #[test]
    fn flow_with_conflict_context_is_conflict_retry() {
        use crate::supervisor::SupervisorFlow;
        let task = make_task("open");
        assert_eq!(
            flow_for_task_dispatch(&task, true, false),
            SupervisorFlow::ConflictRetry
        );
    }

    #[test]
    fn flow_for_needs_task_review_is_review_response() {
        use crate::supervisor::SupervisorFlow;
        let task = make_task("needs_task_review");
        assert_eq!(
            flow_for_task_dispatch(&task, false, false),
            SupervisorFlow::ReviewResponse
        );
        let in_review = make_task("in_task_review");
        assert_eq!(
            flow_for_task_dispatch(&in_review, false, false),
            SupervisorFlow::ReviewResponse
        );
    }

    #[test]
    fn flow_default_is_new_task() {
        use crate::supervisor::SupervisorFlow;
        let task = make_task("open");
        assert_eq!(
            flow_for_task_dispatch(&task, false, false),
            SupervisorFlow::NewTask
        );
    }

    #[test]
    fn flow_conflict_takes_precedence_over_review_response() {
        use crate::supervisor::SupervisorFlow;
        // If a task is in needs_task_review AND has a merge conflict,
        // conflict-retry wins because that's the blocker on landing.
        let task = make_task("needs_task_review");
        assert_eq!(
            flow_for_task_dispatch(&task, true, true),
            SupervisorFlow::ConflictRetry
        );
    }

    #[test]
    fn flow_spike_takes_precedence_over_status() {
        use crate::supervisor::SupervisorFlow;
        // Even if the task status is in_task_review, a spike issue_type
        // short-circuits to the Spike flow.
        let task = make_task_with_type("needs_task_review", "spike");
        assert_eq!(
            flow_for_task_dispatch(&task, false, true),
            SupervisorFlow::Spike
        );
    }
}
