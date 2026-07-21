//! Agent roles facade.
//!
//! After Phase 3, core role data (`AgentType`, `RoleConfig`, prompt templates
//! and rendering) lives in [`djinn_roles`].  This module re-exports those
//! public symbols under the old `djinn_agent::roles::*` paths so existing
//! consumers keep compiling unchanged, and adds the agent-specific pieces
//! that depend on `djinn-agent` internals (the `AgentRole` trait, concrete
//! role implementations, dispatch logic, and the `RoleRegistry`).

use crate::context::AgentContext;
use crate::prompts::TaskContext;
use djinn_core::models::Task;
use futures::future::BoxFuture;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

// ─── Re-exports from djinn-roles (facade paths) ─────────────────────────────

pub use djinn_roles::config::RoleConfig;
pub use djinn_roles::config::config_for;

// ─── Agent-specific role implementations ─────────────────────────────────────

mod adversary;
mod advocate;
mod architect;
pub mod finalize;
mod judge;
mod lead;
mod planner;
mod reviewer;
mod worker;

pub(crate) use adversary::AdversaryRole;
pub(crate) use advocate::AdvocateRole;
pub(crate) use architect::ArchitectRole;
pub(crate) use judge::JudgeRole;
pub(crate) use lead::LeadRole;
pub(crate) use planner::PlannerRole;
pub(crate) use reviewer::ReviewerRole;
pub(crate) use worker::WorkerRole;

/// Resolve the concrete tool schemas for an `AgentType` using the
/// djinn-roles registry.
pub(crate) fn tool_schemas_for(agent_type: crate::AgentType) -> Vec<serde_json::Value> {
    djinn_roles::tool_schemas_for(agent_type)
}

/// Thin role trait that every agent role must implement.
///
/// Object-safe: async methods return `BoxFuture` so `dyn AgentRole` works.
pub(crate) trait AgentRole: Send + Sync + 'static {
    fn config(&self) -> &RoleConfig;
    fn render_prompt(&self, task: &Task, ctx: &TaskContext) -> String;
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
pub fn finalize_tool_name_for(agent_type: crate::AgentType) -> &'static str {
    role_impl_for(agent_type).finalize_tool_name()
}

/// Resolve the concrete `AgentRole` implementation for an `AgentType`.
pub(crate) fn role_impl_for(agent_type: crate::AgentType) -> Arc<dyn AgentRole> {
    match agent_type {
        crate::AgentType::Worker => Arc::new(WorkerRole),
        crate::AgentType::Reviewer => Arc::new(ReviewerRole),
        crate::AgentType::Lead => Arc::new(LeadRole),
        crate::AgentType::Planner => Arc::new(PlannerRole),
        crate::AgentType::Architect => Arc::new(ArchitectRole),
        crate::AgentType::Advocate => Arc::new(AdvocateRole),
        crate::AgentType::Adversary => Arc::new(AdversaryRole),
        crate::AgentType::Judge => Arc::new(JudgeRole),
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
///   board *and* is the escalation ceiling above Lead, so every review
///   task (`request_planner` escalation) dispatches as a
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
            "worker" => Some(crate::AgentType::Worker),
            "reviewer" => Some(crate::AgentType::Reviewer),
            "lead" | "pm" => Some(crate::AgentType::Lead),
            "planner" => Some(crate::AgentType::Planner),
            "architect" => Some(crate::AgentType::Architect),
            "advocate" => Some(crate::AgentType::Advocate),
            "adversary" => Some(crate::AgentType::Adversary),
            "judge" => Some(crate::AgentType::Judge),
            _ => None,
        }
    {
        return role_impl_for(base);
    }

    // Issue-type-specific routing takes priority over status-based routing.
    match task.issue_type.as_str() {
        // `epic_breakdown` is proposal decomposition — Planner Mode D.
        "planning" | "decomposition" | "epic_breakdown" => {
            return role_impl_for(crate::AgentType::Planner);
        }
        // ADR-051 §1 + §8: review tasks are Planner-owned (escalation +
        // lead escalation ceiling).  Previously this routed to Architect
        // per ADR-034 before the split.
        "review" => return role_impl_for(crate::AgentType::Planner),
        // Spikes remain the Architect's territory — they are how the
        // Planner asks for deep code-structural reasoning (ADR-051 §2).
        "spike" => return role_impl_for(crate::AgentType::Architect),
        _ => {}
    }
    role_impl_for(crate::AgentType::for_task_status(
        task.status.as_str(),
        false,
    ))
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
///   planner already decided `execute` on a prior run). The host
///   (`supervisor_runner::resume_flow`) may further upgrade this to the
///   reviewer-only [`SupervisorFlow::ReviewResume`] when the worker's commits
///   are already durable on the mirror task_branch — stage-aware resume after a
///   reviewer-stage pod kill, skipping the redundant worker redo.
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
        // Simple-lifecycle types — planner-only flow. `epic_breakdown` is the
        // proposal-decomposition planner (Mode D).
        "planning" | "decomposition" | "review" | "epic_breakdown" => {
            return SupervisorFlow::Planning;
        }
        // Proposal-refinement tribunal: single-stage flow that runs one
        // refinement role (advocate, adversary, or judge). The concrete
        // agent type is resolved from task.agent_type in role_overrides.
        "refinement" => return SupervisorFlow::Refinement,
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
        || matches!(task.status.as_str(), "needs_task_review" | "in_task_review")
    {
        return SupervisorFlow::ReviewResponse;
    }

    // Lead intervention: a task parked in the lead queue runs the single-stage
    // Lead flow. WITHOUT this arm these statuses fell through to `NewTask`
    // (worker→reviewer), so the Lead never ran, the task never left
    // `needs_lead_intervention`, and the coordinator re-dispatched it forever
    // (the 82g0/78y9 wedge). Keep in sync with `role_for_task_dispatch`, which
    // already maps these statuses to `AgentType::Lead`.
    //
    // Note: only the coordinator arbiter park-rung / second-strike path
    // transitions a task INTO `needs_lead_intervention` (via `Escalate`).
    // The deprecated `request_lead` handler routes to Planner instead
    // (10qg/aizl).
    if matches!(
        task.status.as_str(),
        "needs_lead_intervention" | "in_lead_intervention"
    ) {
        return SupervisorFlow::Lead;
    }

    SupervisorFlow::NewTask
}

#[derive(Default)]
pub(crate) struct DispatchContext;

pub(crate) struct DispatchRule {
    #[allow(dead_code)]
    pub(crate) role_name: &'static str,
    #[allow(dead_code)]
    pub(crate) claims: fn(&Task, &DispatchContext) -> bool,
}

pub struct RoleRegistry {
    pub(crate) roles: HashMap<&'static str, crate::AgentType>,
    #[allow(dead_code)]
    pub(crate) dispatch_rules: Vec<DispatchRule>,
}

impl Default for RoleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl RoleRegistry {
    pub fn new() -> Self {
        // Ensure the djinn-roles tool schema registry is populated before any
        // prompt rendering or tool-schema resolution.  This is idempotent
        // (guarded by `Once` inside) so it's safe to call from tests too.
        crate::init_tool_schema_registry();

        let roles = HashMap::from([
            ("worker", crate::AgentType::Worker),
            ("reviewer", crate::AgentType::Reviewer),
            ("lead", crate::AgentType::Lead),
            ("planner", crate::AgentType::Planner),
            ("architect", crate::AgentType::Architect),
            // Tribunal refinement roles (k9zw).
            ("advocate", crate::AgentType::Advocate),
            ("adversary", crate::AgentType::Adversary),
            ("judge", crate::AgentType::Judge),
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

    #[allow(dead_code)]
    pub(crate) fn role_for_task(&self, task: &Task, ctx: &DispatchContext) -> Option<&'static str> {
        self.dispatch_rules
            .iter()
            .find(|rule| (rule.claims)(task, ctx))
            .map(|rule| rule.role_name)
    }
    /// Unique model-pool role names (dispatch_role from RoleConfig).
    ///
    /// Dispatch no longer enumerates roles up front (model eligibility is now
    /// resolved per task, scoped to its creator, via `dispatch_role_for_task`);
    /// retained as tested RoleRegistry API.
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
    #[allow(dead_code)]
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
/// Under ADR-051 §1 + §8 the Planner owns both board maintenance and
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
    // Spike, research, review, and planning tasks with simple lifecycle
    // are handled by architect_claims / planning_claims / a direct worker path.
    // Research tasks go to the worker role (open-ended but same execution model).
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
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            closed_at: None,
            close_reason: None,
            merge_commit_sha: None,
            pr_url: None,
            merge_conflict_metadata: None,
            memory_refs: "[]".into(),
            agent_type: None,
            created_by_user_id: "test-user".to_owned(),
            ci_status: "unknown".into(),
            ci_head_sha: None,
            ci_pr_number: None,
            ci_blocking_required_check_names: "[]".into(),
            ci_failure_fingerprint: None,
            ci_first_seen_at: None,
            ci_last_seen_at: None,
            ci_same_signature_count: 0,
            ci_last_remediation_base_sha: None,
            ci_mirror_head_sha: None,
            ci_github_head_sha: None,
            ci_heads_diverged: None,
            ci_head_observation_error: None,
            ci_mq_state: None,
            ci_mq_run_id: None,
            ci_mq_head_sha: None,
            ci_mq_failed_check_names: None,
            ci_mq_failure_fingerprint: None,
            ci_mq_same_signature_count: None,
            ci_mq_first_seen_at: None,
            ci_mq_last_seen_at: None,
            unresolved_blocker_count: 0,
            total_reopen_count: 0,
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
        // ADR-051 §1 + §8: review tasks (lead escalation + intervention)
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

    #[test]
    fn registry_includes_tribunal_roles() {
        let registry = RoleRegistry::new();
        for role_name in ["advocate", "adversary", "judge"] {
            assert!(
                registry.roles.contains_key(role_name),
                "RoleRegistry should contain '{role_name}'"
            );
        }
        let model_pool_roles = registry.model_pool_roles();
        for role_name in ["advocate", "adversary", "judge"] {
            assert!(
                model_pool_roles.contains(&role_name),
                "model_pool_roles should include '{role_name}'"
            );
        }
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

    #[test]
    fn flow_for_lead_intervention_is_lead() {
        use crate::supervisor::{RoleKind, SupervisorFlow};
        // Regression for the 82g0/78y9 wedge: a task parked in the lead queue
        // MUST route to the single-stage Lead flow. Before this arm existed
        // these statuses fell through to NewTask (worker→reviewer), so the Lead
        // never ran and the task could never leave needs_lead_intervention.
        for status in ["needs_lead_intervention", "in_lead_intervention"] {
            let task = make_task(status);
            assert_eq!(
                flow_for_task_dispatch(&task, false, false),
                SupervisorFlow::Lead,
                "status {status} should route to the Lead flow"
            );
            assert_eq!(SupervisorFlow::Lead.role_sequence(), &[RoleKind::Lead]);
        }
    }

    #[test]
    fn flow_for_refinement_is_refinement() {
        use crate::supervisor::SupervisorFlow;
        let task = make_task_with_type("open", "refinement");
        assert_eq!(
            flow_for_task_dispatch(&task, false, false),
            SupervisorFlow::Refinement
        );
    }

    #[test]
    fn refinement_issue_type_takes_precedence_over_status() {
        use crate::supervisor::SupervisorFlow;
        // Even if the task status is needs_task_review, a refinement issue_type
        // short-circuits to the Refinement flow.
        let task = make_task_with_type("needs_task_review", "refinement");
        assert_eq!(
            flow_for_task_dispatch(&task, false, true),
            SupervisorFlow::Refinement
        );
    }
}
