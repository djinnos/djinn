//! Task-based classifier for native-skill loading triggers.
//!
//! The planner role serves two distinct session shapes:
//!
//! 1. **Proposal authoring** — `epic_breakdown` tasks that decompose a
//!    graduated proposal into epics (Workflow D), reconcile acceptance
//!    criteria as epics close (Workflow E), or refine the proposal spec.
//!    These sessions benefit from native skills such as `visual-spec`.
//!
//! 2. **Wave planning / dispatch** — `planning` / `decomposition` tasks that
//!    break an epic into the next batch of worker tasks (Workflow B).
//!    These sessions do not need native authoring skills and should not
//!    pay the context cost.
//!
//! [`classify_native_skill_trigger`] returns the appropriate
//! [`NativeSkillTrigger`] signal for a `(role_name, task)` pair so that
//! downstream session-setup code can decide which native skills to load
//! lazily.

use djinn_core::models::Task;

/// Signal indicating whether native skills should be loaded for a session.
///
/// Returned by [`classify_native_skill_trigger`].  The variant set is
/// intentionally small — only enough to gate lazy loading of platform-owned
/// authoring skills such as `visual-spec`.
///
/// Downstream consumers (e.g. `resolve_mcp_and_skills`) use this to decide
/// which native skills to prepend before project skills in the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeSkillTrigger {
    /// The session is a planner proposal-authoring / grooming / refinement /
    /// reconcile task.  Native skills recommended for the planner (such as
    /// `visual-spec`) should be loaded.
    ProposalAuthoring,
}

/// Classify whether native skills should load for a `(role_name, task)` pair.
///
/// Returns `Some(ProposalAuthoring)` when:
/// - The role is `"planner"`, **and**
/// - The task's `issue_type` is `"epic_breakdown"` (proposal decomposition or
///   proposal AC reconciliation).
///
/// Returns `None` for:
/// - Non-planner roles (`"worker"`, `"reviewer"`, `"lead"`, `"architect"`).
/// - Planner tasks with `issue_type` of `"planning"` or `"decomposition"`
///   (ordinary wave-planning / dispatch sessions).
/// - Any other task shape not identified as proposal authoring.
///
/// The classifier is purely synchronous, needs no DB or filesystem access,
/// and is safe to call from unit tests with minimal `Task` stubs.
///
/// Canonical (role, issue_type) → trigger mapping. **This is the single source
/// of truth.** Every caller — session construction (`stage.rs` /
/// `mcp_resolve.rs`) AND the `skill_read` handler — must route through here (or
/// the `Task` wrapper below). They previously each inlined this match, which
/// drifted: the `skill_read` copy stayed planner-only and rejected the Advocate
/// ("not an assigned skill") even after session construction had assigned the
/// skill, so the Advocate could never load `visual-spec`.
pub(crate) fn classify_native_skill_trigger_by_type(
    role_name: &str,
    issue_type: &str,
) -> Option<NativeSkillTrigger> {
    match (role_name, issue_type) {
        // `epic_breakdown` is the proposal-decomposition / AC-reconciliation
        // planner mode (Workflow D / Workflow E).  These are always
        // proposal-authoring sessions.
        ("planner", "epic_breakdown") => Some(NativeSkillTrigger::ProposalAuthoring),
        // The tribunal **Advocate** authors and revises the proposal spec
        // during refinement — exactly the proposal-authoring work the planner
        // does in `epic_breakdown`. It therefore needs the same authoring
        // native skills (e.g. `visual-spec`) so the refined spec is rich,
        // visual MDX rather than shallow prose. Without this the Advocate runs
        // with no native skill and produces plain markdown.
        ("advocate", "refinement") => Some(NativeSkillTrigger::ProposalAuthoring),
        // `planning` / `decomposition` are ordinary wave-planning (Workflow B).
        // All other (role, issue_type) pairs are non-authoring.
        _ => None,
    }
}

/// `Task`-based wrapper over [`classify_native_skill_trigger_by_type`] used by
/// session construction, which has the full `Task`.
pub(crate) fn classify_native_skill_trigger(
    role_name: &str,
    task: &Task,
) -> Option<NativeSkillTrigger> {
    classify_native_skill_trigger_by_type(role_name, task.issue_type.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal `Task` stub for unit tests.  Only `issue_type` is
    /// material to the classifier; all other fields use harmless defaults.
    fn make_task(issue_type: &str) -> Task {
        Task {
            id: "task-001".into(),
            project_id: "proj-1".into(),
            short_id: "t001".into(),
            epic_id: None,
            title: "Test task".into(),
            description: String::new(),
            design: String::new(),
            issue_type: issue_type.into(),
            status: "open".into(),
            priority: 1,
            owner: "test".into(),
            labels: "[]".into(),
            acceptance_criteria: "[]".into(),
            reopen_count: 0,
            continuation_count: 0,
            total_reopen_count: 0,
            intervention_count: 0,
            last_intervention_at: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            closed_at: None,
            close_reason: None,
            merge_commit_sha: None,
            pr_url: None,
            merge_conflict_metadata: None,
            memory_refs: "[]".into(),
            agent_type: None,
            created_by_user_id: None,
            unresolved_blocker_count: 0,
        }
    }

    // ── Proposal authoring triggers ──────────────────────────────────────

    #[test]
    fn epic_breakdown_returns_proposal_authoring() {
        let task = make_task("epic_breakdown");
        assert_eq!(
            classify_native_skill_trigger("planner", &task),
            Some(NativeSkillTrigger::ProposalAuthoring),
            "epic_breakdown under planner should trigger proposal authoring"
        );
    }

    #[test]
    fn epic_breakdown_with_proposal_review_title_returns_authoring() {
        // Workflow E: proposal AC reconciliation dispatched as epic_breakdown.
        let mut task = make_task("epic_breakdown");
        task.title = "Reconcile proposal acceptance criteria for r0io".into();
        assert_eq!(
            classify_native_skill_trigger("planner", &task),
            Some(NativeSkillTrigger::ProposalAuthoring),
            "epic_breakdown proposal-review task should trigger proposal authoring"
        );
    }

    #[test]
    fn epic_breakdown_with_proposal_decompose_title_returns_authoring() {
        // Workflow D: proposal decomposition dispatched as epic_breakdown.
        let mut task = make_task("epic_breakdown");
        task.title = "Decompose proposal into epics for y8p2".into();
        assert_eq!(
            classify_native_skill_trigger("planner", &task),
            Some(NativeSkillTrigger::ProposalAuthoring),
            "epic_breakdown proposal-decompose task should trigger proposal authoring"
        );
    }

    #[test]
    fn advocate_refinement_returns_proposal_authoring() {
        // The tribunal Advocate authors/revises the proposal spec during
        // refinement and must receive the proposal-authoring native skills.
        let task = make_task("refinement");
        assert_eq!(
            classify_native_skill_trigger("advocate", &task),
            Some(NativeSkillTrigger::ProposalAuthoring),
            "advocate refinement task should trigger proposal authoring"
        );
    }

    #[test]
    fn adversary_and_judge_refinement_return_none() {
        // Only the Advocate authors; the Adversary objects and the Judge
        // adjudicates — neither needs the authoring native skill.
        let task = make_task("refinement");
        assert_eq!(classify_native_skill_trigger("adversary", &task), None);
        assert_eq!(classify_native_skill_trigger("judge", &task), None);
    }

    // ── Non-authoring planner tasks ──────────────────────────────────────

    #[test]
    fn planning_returns_none() {
        let task = make_task("planning");
        assert_eq!(
            classify_native_skill_trigger("planner", &task),
            None,
            "planning tasks should not trigger proposal authoring"
        );
    }

    #[test]
    fn planning_with_wave_title_returns_none() {
        let mut task = make_task("planning");
        task.title = "Plan next wave: Lazy native-skill prompt loading".into();
        assert_eq!(
            classify_native_skill_trigger("planner", &task),
            None,
            "Plan next wave task should not trigger proposal authoring"
        );
    }

    #[test]
    fn decomposition_returns_none() {
        // Legacy alias for planning.
        let task = make_task("decomposition");
        assert_eq!(
            classify_native_skill_trigger("planner", &task),
            None,
            "decomposition tasks should not trigger proposal authoring"
        );
    }

    #[test]
    fn review_returns_none() {
        // Review tasks are planner-owned but not proposal authoring.
        let task = make_task("review");
        assert_eq!(
            classify_native_skill_trigger("planner", &task),
            None,
            "review tasks should not trigger proposal authoring"
        );
    }

    #[test]
    fn task_issue_type_returns_none() {
        let task = make_task("task");
        assert_eq!(
            classify_native_skill_trigger("planner", &task),
            None,
            "task issue_type should not trigger proposal authoring"
        );
    }

    // ── Non-planner roles ────────────────────────────────────────────────

    #[test]
    fn worker_role_returns_none_even_for_epic_breakdown() {
        let task = make_task("epic_breakdown");
        assert_eq!(
            classify_native_skill_trigger("worker", &task),
            None,
            "non-planner role should not trigger proposal authoring"
        );
    }

    #[test]
    fn reviewer_role_returns_none() {
        let task = make_task("epic_breakdown");
        assert_eq!(
            classify_native_skill_trigger("reviewer", &task),
            None,
            "reviewer role should not trigger proposal authoring"
        );
    }

    #[test]
    fn lead_role_returns_none() {
        let task = make_task("epic_breakdown");
        assert_eq!(
            classify_native_skill_trigger("lead", &task),
            None,
            "lead role should not trigger proposal authoring"
        );
    }

    #[test]
    fn architect_role_returns_none() {
        let task = make_task("epic_breakdown");
        assert_eq!(
            classify_native_skill_trigger("architect", &task),
            None,
            "architect role should not trigger proposal authoring"
        );
    }

    // ── Edge cases ───────────────────────────────────────────────────────

    #[test]
    fn empty_issue_type_returns_none() {
        let task = make_task("");
        assert_eq!(
            classify_native_skill_trigger("planner", &task),
            None,
            "empty issue_type should not trigger proposal authoring"
        );
    }

    #[test]
    fn unknown_issue_type_returns_none() {
        let task = make_task("research");
        assert_eq!(
            classify_native_skill_trigger("planner", &task),
            None,
            "unknown issue_type should not trigger proposal authoring"
        );
    }

    #[test]
    fn empty_role_returns_none() {
        let task = make_task("epic_breakdown");
        assert_eq!(
            classify_native_skill_trigger("", &task),
            None,
            "empty role name should not trigger proposal authoring"
        );
    }
}
