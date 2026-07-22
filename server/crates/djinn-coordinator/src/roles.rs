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

/// Label marking a task as a human-only review hold (auto-park terminal
/// escalation). Tasks carrying it must NEVER be auto-dispatched to any
/// agent role — they wait for a human. The auto-park mechanism (see
/// `dispatch/retry.rs`) stamps this label on the `review`-type hold task
/// it creates to break a CI/rework loop.
pub(crate) const HUMAN_REVIEW_HOLD_LABEL: &str = "human-review-hold";

/// Returns `true` if `task` carries the `human-review-hold` label.
///
/// `labels` is a JSON-array string (e.g. `["human-review-hold"]`); a
/// substring match is sufficient and is the same shape used by the
/// dispatch-time hold check in `dispatch/task_dispatch.rs`.
pub(crate) fn is_human_review_hold(task: &Task) -> bool {
    task.labels.contains(HUMAN_REVIEW_HOLD_LABEL)
}

/// Label marking a task as an autonomous **arbiter-park planner escalation**.
///
/// Unlike [`HUMAN_REVIEW_HOLD_LABEL`], a task carrying this label is a NORMAL,
/// planner-dispatchable `review` task: the arbiter's `park` decision hands the
/// full park dossier to a Planner SESSION that resolves it autonomously
/// (decompose + supersede, close as won't-fix, or re-scope + reopen the
/// source) — no human is ever required. It therefore must NOT carry the
/// human-review-hold label (which would exclude it from dispatch and defend it
/// from auto-close). But because it blocks a parked source task, its CLOSE
/// must run exactly the same source-release semantics as a human hold: stamp
/// `human_review_resolved_at` and emit `tripwire.hold.released` when the source
/// carries an active tripwire hold.
pub(crate) const PLANNER_PARK_ESCALATION_LABEL: &str = "planner-park-escalation";

/// Returns `true` if closing `task` must run the hold-release semantics on the
/// source task(s) it was blocking — stamping `human_review_resolved_at` and
/// emitting `tripwire.hold.released`.
///
/// This is `true` for both the legacy human-review hold ([`is_human_review_hold`])
/// and the autonomous arbiter-park planner escalation
/// ([`PLANNER_PARK_ESCALATION_LABEL`]). It is deliberately SEPARATE from
/// [`is_human_review_hold`], which still gates dispatch exclusion and auto-close
/// protection (the escalation task must stay dispatchable + planner-closable).
pub(crate) fn releases_source_on_close(task: &Task) -> bool {
    is_human_review_hold(task) || task.labels.contains(PLANNER_PARK_ESCALATION_LABEL)
}

/// Remove the `human-review-hold` label from an existing labels JSON array,
/// returning the updated JSON string. Idempotent: if the label is absent the
/// input's label set is returned unchanged (re-serialised).
///
/// `existing_labels` is a JSON-array string (e.g. `["bug","human-review-hold"]`).
/// Malformed JSON is treated as an empty label set (yielding `[]`).
///
/// Used by the hold-release path: once a hold is released (human close or
/// arbiter `tripwire_release`) the source task must shed the label so the
/// dispatch guard stops skipping it and the tamper reconciler does not
/// re-apply it (the label-only-ever-added regression).
pub(crate) fn remove_hold_label_from_existing(existing_labels: &str) -> String {
    let labels: Vec<String> = serde_json::from_str(existing_labels).unwrap_or_default();
    let kept: Vec<String> = labels
        .into_iter()
        .filter(|l| l != HUMAN_REVIEW_HOLD_LABEL)
        .collect();
    serde_json::to_string(&kept).unwrap_or_else(|_| "[]".to_owned())
}

/// Returns `true` if the task is an open/in-progress review task.
///
/// Excludes `human-review-hold` tasks: those are a terminal,
/// human-only escalation and must never be claimed by the Planner.
/// Without this guard a Planner session would claim the hold task,
/// choose "escalate"/"skip", and close it — which fires the
/// unblocked-tasks release and flips the parked source task back to
/// `open`, defeating the auto-park safety mechanism (the exact loop the
/// hold exists to break). Epic `pdn6` already excluded the blocked
/// *source* task from worker dispatch; this closes the gap of the HOLD
/// task itself being claimed by the Planner.
fn planner_review_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    matches!(task.status.as_str(), "open" | "in_progress")
        && task.issue_type.as_str() == "review"
        && !is_human_review_hold(task)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal `Task` for dispatch-rule unit tests.
    fn task(issue_type: &str, status: &str, labels: &str) -> Task {
        Task {
            id: "task-id".to_owned(),
            project_id: "project-id".to_owned(),
            short_id: "task".to_owned(),
            epic_id: None,
            title: "Task".to_owned(),
            description: String::new(),
            design: String::new(),
            issue_type: issue_type.to_owned(),
            status: status.to_owned(),
            priority: 2,
            owner: String::new(),
            labels: labels.to_owned(),
            acceptance_criteria: "[]".to_owned(),
            reopen_count: 0,
            continuation_count: 0,
            total_reopen_count: 0,
            intervention_count: 0,
            last_intervention_at: None,
            created_at: "2026-06-30T00:00:00.000Z".to_owned(),
            updated_at: "2026-06-30T00:00:00.000Z".to_owned(),
            closed_at: None,
            close_reason: None,
            merge_commit_sha: None,
            pr_url: None,
            merge_conflict_metadata: None,
            memory_refs: "[]".to_owned(),
            agent_type: None,
            created_by_user_id: "fixture-user".into(),
            ci_status: "unknown".to_owned(),
            ci_head_sha: None,
            ci_pr_number: None,
            ci_blocking_required_check_names: "[]".to_owned(),
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
            refinement_run_id: None,
            refinement_intent_id: None,
            refinement_generation: None,
            refinement_round: None,
            refinement_phase: None,
            refinement_role: None,
        }
    }

    /// A normal `review` task (no hold label) is still Planner-owned —
    /// the human-review-hold exclusion must not regress the escalation/
    /// intervention review dispatch path (ADR-051 §1 + §8).
    #[test]
    fn plain_review_task_is_claimed_by_planner() {
        let registry = RoleRegistry::new();
        let ctx = DispatchContext;
        for status in ["open", "in_progress"] {
            let t = task("review", status, "[]");
            assert!(planner_review_claims(&t, &ctx));
            assert_eq!(
                registry.role_for_task(&t, &ctx),
                Some("planner"),
                "plain review task ({status}) must route to the planner"
            );
        }
    }

    /// `remove_hold_label_from_existing` strips only the hold label, keeps
    /// siblings, and is idempotent when the label is absent / malformed.
    #[test]
    fn remove_hold_label_strips_only_the_hold_label() {
        assert_eq!(
            remove_hold_label_from_existing(r#"["bug","human-review-hold","blocked"]"#),
            r#"["bug","blocked"]"#
        );
        // Absent → unchanged set (re-serialised).
        assert_eq!(remove_hold_label_from_existing(r#"["bug"]"#), r#"["bug"]"#);
        // Only the hold label → empty set.
        assert_eq!(
            remove_hold_label_from_existing(r#"["human-review-hold"]"#),
            r#"[]"#
        );
        // Malformed JSON → empty set (fail-safe).
        assert_eq!(remove_hold_label_from_existing("not json"), r#"[]"#);
    }

    /// Regression (this fix): a `human-review-hold`-labeled `review` task
    /// must NOT be claimed by ANY dispatch rule — not planner, worker,
    /// architect, reviewer, or lead. It is a human-only terminal hold and
    /// must wait for a human; auto-dispatching + auto-closing it releases
    /// the parked source task, defeating the auto-park safety mechanism.
    #[test]
    fn human_review_hold_task_is_claimed_by_no_role() {
        let registry = RoleRegistry::new();
        let ctx = DispatchContext;
        let labels = r#"["human-review-hold"]"#;

        for status in ["open", "in_progress"] {
            let t = task("review", status, labels);

            assert!(
                is_human_review_hold(&t),
                "fixture must carry the human-review-hold label"
            );
            // The planner review rule must reject it.
            assert!(
                !planner_review_claims(&t, &ctx),
                "planner must not claim a human-review-hold task ({status})"
            );
            // No dispatch rule at all may claim it.
            assert_eq!(
                registry.role_for_task(&t, &ctx),
                None,
                "no role may claim a human-review-hold task ({status})"
            );
            // Defense check: each individual claims predicate is false.
            assert!(!architect_claims(&t, &ctx));
            assert!(!planning_claims(&t, &ctx));
            assert!(!worker_claims(&t, &ctx));
            assert!(!reviewer_claims(&t, &ctx));
            assert!(!lead_claims(&t, &ctx));
            assert!(!planner_claims(&t, &ctx));
        }
    }

    /// Part B (project-agnostic draft-PR CI-feedback flow): a task whose PR is
    /// still a GitHub draft sits in `pr_draft` while the project's OWN CI runs
    /// on the PR head. Reviewers must NOT be dispatched on a draft PR — the
    /// pr_poller owns the task until CI goes green (or no CI is configured), at
    /// which point it undrafts the PR and transitions the task to
    /// `needs_task_review`. `reviewer_claims` is gated on the review statuses,
    /// so a `pr_draft` task is never claimed by the reviewer (nor any other
    /// dispatch role). This locks the "reviewers not dispatched while draft"
    /// invariant that the draft flow relies on.
    #[test]
    fn reviewer_not_dispatched_while_pr_is_draft() {
        let ctx = DispatchContext;

        // While the PR is a draft the task is in `pr_draft` (or `pr_review`,
        // both poller-owned): the reviewer claims neither.
        for status in ["pr_draft", "pr_review"] {
            let draft = task("task", status, "[]");
            assert!(
                !reviewer_claims(&draft, &ctx),
                "reviewer must not claim a {status} task (PR still a draft / poller-owned, CI in flight)"
            );
        }

        // Once the poller undrafts (CI green / no CI configured) and the task
        // reaches `needs_task_review`, the reviewer DOES claim it — dispatch keys
        // off the ready transition, not the draft state.
        let ready = task("task", "needs_task_review", "[]");
        assert!(
            reviewer_claims(&ready, &ctx),
            "reviewer claims the task only after it is ready for review"
        );
    }

    /// An autonomous arbiter-park escalation (`planner-park-escalation`) is a
    /// NORMAL planner-dispatchable review task: unlike a human-review hold it
    /// must be claimed by the planner (so a session resolves it) and it must
    /// NOT be treated as a human-only hold. But closing it must still run the
    /// source-release semantics (`releases_source_on_close`).
    #[test]
    fn planner_park_escalation_is_dispatchable_but_releases_on_close() {
        let registry = RoleRegistry::new();
        let ctx = DispatchContext;
        let labels = r#"["planner-park-escalation"]"#;

        for status in ["open", "in_progress"] {
            let t = task("review", status, labels);

            // NOT a human-only hold — stays dispatchable + planner-closable.
            assert!(
                !is_human_review_hold(&t),
                "planner-park-escalation must NOT be a human-review hold ({status})"
            );
            assert!(
                planner_review_claims(&t, &ctx),
                "planner must claim a planner-park-escalation task ({status})"
            );
            assert_eq!(
                registry.role_for_task(&t, &ctx),
                Some("planner"),
                "planner-park-escalation ({status}) must route to the planner"
            );
            // But its close must release the blocked source.
            assert!(
                releases_source_on_close(&t),
                "closing a planner-park-escalation must release the source ({status})"
            );
        }

        // The legacy human-review hold also releases on close; a plain review
        // task does not.
        assert!(releases_source_on_close(&task(
            "review",
            "open",
            r#"["human-review-hold"]"#
        )));
        assert!(!releases_source_on_close(&task("review", "open", "[]")));
    }

    /// Evidence-spike tasks (carrying `refinement-evidence` + `read-only`
    /// labels from the Judge demand-evidence path in epic 6tjy) must still
    /// route to the Architect role — the runtime profile restriction
    /// happens at the agent stage, not at the coordinator dispatch level.
    #[test]
    fn evidence_spike_task_is_claimed_by_architect() {
        let registry = RoleRegistry::new();
        let ctx = DispatchContext;
        // Exact labels from proposal_refinement_demand_evidence.
        let labels = r#"["refinement-evidence","read-only","proposal:p1"]"#;

        for status in ["open", "in_progress"] {
            let t = task("spike", status, labels);

            assert!(
                architect_claims(&t, &ctx),
                "evidence-spike task ({status}) must be claimed by architect_claims"
            );
            assert_eq!(
                registry.role_for_task(&t, &ctx),
                Some("architect"),
                "evidence-spike task ({status}) must route to architect"
            );
        }
    }

    /// Generic spike tasks without evidence-spike labels still route to
    /// the Architect — the evidence-spike profile must not change generic
    /// routing.
    #[test]
    fn generic_spike_without_evidence_labels_still_routes_to_architect() {
        let registry = RoleRegistry::new();
        let ctx = DispatchContext;

        for status in ["open", "in_progress"] {
            let t = task("spike", status, "[]");
            assert_eq!(
                registry.role_for_task(&t, &ctx),
                Some("architect"),
                "generic spike ({status}) must still route to architect"
            );
        }
    }
}
