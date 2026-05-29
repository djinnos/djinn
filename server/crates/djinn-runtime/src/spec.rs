//! Wire-capable task-run spec + outcome types.
//!
//! These types were previously `djinn_agent::supervisor::{spec, flow}`; Phase
//! 2 PR 1 moved them here so that the future in-container supervisor (running
//! inside `djinn-agent-worker`) can share the exact type definitions with the
//! host-side coordinator without re-exporting them across an `AppState`-heavy
//! crate boundary.
//!
//! All types derive `Serialize + Deserialize` so they can ride a
//! `bincode::serialize`/`deserialize` frame between the host and the
//! container (bincode 1.3 is serde-driven, so no separate `Encode`/`Decode`
//! derives are needed).

use std::collections::HashMap;

use djinn_core::models::TaskRunTrigger;
use serde::{Deserialize, Serialize};

/// Which role executes at each stage of a task-run.
///
/// Not the same as `djinn-agent`'s existing `AgentRole` trait objects — this
/// is a lightweight enum suitable for flow templates and telemetry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleKind {
    Planner,
    Worker,
    Reviewer,
    Verifier,
    Architect,
}

impl RoleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RoleKind::Planner => "planner",
            RoleKind::Worker => "worker",
            RoleKind::Reviewer => "reviewer",
            RoleKind::Verifier => "verifier",
            RoleKind::Architect => "architect",
        }
    }
}

/// Template for a task-run's role sequence.
///
/// `NewTask` is the canonical "work" flow: plan, execute, review, verify,
/// PR. `ReviewResponse` and `ConflictRetry` re-enter mid-flow when the
/// planner's decision is already implicit in the task's existence. `Spike`
/// routes the architect onto scoped research tasks the planner created
/// during a prior NewTask. `Planning` runs the planner alone — useful for
/// explicit "just re-plan this" invocations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorFlow {
    NewTask,
    ReviewResponse,
    ConflictRetry,
    Spike,
    Planning,
}

impl SupervisorFlow {
    pub fn role_sequence(self) -> &'static [RoleKind] {
        role_sequence(self)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SupervisorFlow::NewTask => "new_task",
            SupervisorFlow::ReviewResponse => "review_response",
            SupervisorFlow::ConflictRetry => "conflict_retry",
            SupervisorFlow::Spike => "spike",
            SupervisorFlow::Planning => "planning",
        }
    }
}

/// Free-function form of [`SupervisorFlow::role_sequence`] — exposed at the
/// crate root so call sites that only need the sequence can avoid pulling in
/// the full `SupervisorFlow` enum scope (matches the `lib.rs` re-export).
pub fn role_sequence(flow: SupervisorFlow) -> &'static [RoleKind] {
    use RoleKind::*;
    match flow {
        // Worker → Reviewer → (PR opens). The Verifier stage is unimplemented
        // (stage.rs returns "verifier stage not yet wired"); add it back as
        // the middle hop once `verify_commit` is plumbed in. For now the
        // reviewer is the only gate before PR.
        // The wave-planner already broke the work down upstream, so no
        // upfront Planner stage here.
        SupervisorFlow::NewTask => &[Worker, Reviewer],
        SupervisorFlow::ReviewResponse | SupervisorFlow::ConflictRetry => {
            // Verifier currently stubbed; matches NewTask shape.
            &[Worker, Reviewer]
        }
        SupervisorFlow::Spike => &[Architect],
        SupervisorFlow::Planning => &[Planner],
    }
}

/// Input to `TaskRunSupervisor::run`.
///
/// All runtime-variable data the supervisor needs to execute one task-run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskRunSpec {
    /// The canonical task-run id, minted once by the host coordinator before
    /// `prepare`. This is the SINGLE id for the whole run: the K8s runtime
    /// derives its resource name / registry key from it, the in-pod
    /// `TaskRunSupervisor` uses it for the `task_runs` row + every session, and
    /// the terminal `TaskRunReport` carries it back. Unifying it here removes
    /// the old split where `prepare` and the supervisor each minted their own
    /// `Uuid`, leaving the host's report id pointing at a row that never
    /// existed (the bug that silently disabled post-session extraction).
    pub task_run_id: String,
    pub task_id: String,
    pub project_id: String,
    pub trigger: TaskRunTrigger,
    /// Existing branch in the mirror to start from (e.g. `main`).
    pub base_branch: String,
    /// Branch the task-run commits onto; created locally from `base_branch`
    /// when needed. Pushed to origin at PR-open time.
    pub task_branch: String,
    pub flow: SupervisorFlow,
    /// Optional per-role model override.  When a [`RoleKind`] key is present,
    /// `execute_stage` uses the mapped `provider/model` id for that stage
    /// instead of the catalog-default fallback.  The coordinator populates
    /// this from its per-role model resolution (dispatch priorities + project
    /// `model_preference`) so the supervisor path keeps parity with the
    /// legacy `run_task_lifecycle` model selection.  Empty = fall back to
    /// catalog-default for every stage.
    pub model_id_per_role: HashMap<RoleKind, String>,
    /// Project IDs this task-run may READ in addition to its own
    /// `project_id` (the write target). Resolved at dispatch from the
    /// task's epic `epic_read_sources`. The worker materializes each
    /// read-only alongside the primary workspace and the agent's prompt
    /// is told it may read them. Empty for tasks without read-source
    /// grants. `#[serde(default)]` keeps older specs (host/worker version
    /// skew during a rolling deploy) deserializable.
    #[serde(default)]
    pub read_source_project_ids: Vec<String>,
    /// The project's GitHub owner (org/user), used to scope private-dependency
    /// fetching in the worker Pod: `GOPRIVATE=github.com/<owner>/*` plus a git
    /// `url.insteadOf` rewrite so `go mod download` / cargo git deps / pnpm git
    /// deps authenticate to that org's private repos with the installation
    /// token below. Derived from `projects.github_owner` at dispatch — never a
    /// hardcoded org. `None`/empty disables the rewrite. `#[serde(default)]` for
    /// host/worker version skew during a rolling deploy.
    #[serde(default)]
    pub github_owner: Option<String>,
    /// Short-lived GitHub App installation token for the project's owner,
    /// minted host-side at dispatch (rotates ~hourly). Injected into the Pod's
    /// git config (`url.insteadOf`) so the agent's build/test commands can pull
    /// the org's PRIVATE transitive deps. Rides the per-task-run Secret like the
    /// rest of the spec; lives only for the Pod's lifetime.
    #[serde(default)]
    pub github_install_token: Option<String>,
    /// Git author `name` for commits the supervisor creates on the task
    /// branch. Resolved host-side at dispatch from the task's
    /// `created_by_user_id` (the human who triggered the task). `None` for
    /// system/patrol tasks with no human creator, or for host/worker version
    /// skew during a rolling deploy — the supervisor falls back to the bot
    /// identity. `#[serde(default)]` keeps older specs deserializable.
    #[serde(default)]
    pub commit_author_name: Option<String>,
    /// Git author `email` paired with [`Self::commit_author_name`]. Set to the
    /// GitHub per-user no-reply form
    /// `<github_id>+<github_login>@users.noreply.github.com`, which links the
    /// commit to that GitHub account so it's attributed to the human AND
    /// Vercel's commit-author authorization (which rejects commits whose
    /// author email matches no GitHub account) lets the deployment through.
    /// The PR itself is still opened by the App (`djinn-bot[bot]`), so the
    /// creator can review/approve their own commits.
    #[serde(default)]
    pub commit_author_email: Option<String>,
}

/// Terminal outcome of a task-run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TaskRunOutcome {
    PrOpened { url: String, sha: String },
    /// Planner decided the task should not execute.
    Closed { reason: String },
    /// Planner/architect surfaced a question that blocks automated execution
    /// (e.g. ambiguous scope, missing design decision).
    Escalated { reason: String },
    Failed { stage: String, reason: String },
    Interrupted,
}

/// Return value of `TaskRunSupervisor::run`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskRunReport {
    pub task_run_id: String,
    pub outcome: TaskRunOutcome,
    pub stages_completed: Vec<RoleKind>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_task_flow_skips_planner() {
        // Planner ran upstream as a Planning task; NewTask is the worker's
        // domain and doesn't re-plan. Verifier dropped for now while the
        // supervisor stage is stubbed.
        let seq = SupervisorFlow::NewTask.role_sequence();
        assert!(!seq.contains(&RoleKind::Planner));
        assert_eq!(seq, &[RoleKind::Worker, RoleKind::Reviewer]);
    }

    #[test]
    fn spike_flow_is_architect_only() {
        assert_eq!(
            SupervisorFlow::Spike.role_sequence(),
            &[RoleKind::Architect]
        );
    }

    #[test]
    fn review_response_skips_planner() {
        let seq = SupervisorFlow::ReviewResponse.role_sequence();
        assert!(!seq.contains(&RoleKind::Planner));
        assert!(seq.contains(&RoleKind::Worker));
    }

    #[test]
    fn task_run_spec_bincode_roundtrip() {
        let mut per_role = HashMap::new();
        per_role.insert(RoleKind::Planner, "anthropic/claude-sonnet-4.5".to_string());
        per_role.insert(RoleKind::Worker, "anthropic/claude-opus-4.7".to_string());

        let spec = TaskRunSpec {
            task_run_id: "019e6a03-8aef-7201-9c9d-d7ba17613a0b".to_string(),
            task_id: "task-abc".to_string(),
            project_id: "proj-xyz".to_string(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".to_string(),
            task_branch: "djinn/task-abc".to_string(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: per_role,
            read_source_project_ids: vec!["proj-read-1".to_string()],
            github_owner: None,
            github_install_token: None,
            commit_author_name: Some("Ada Lovelace".to_string()),
            commit_author_email: Some("1+ada@users.noreply.github.com".to_string()),
        };

        let bytes = bincode::serialize(&spec).expect("serialize");
        let back: TaskRunSpec = bincode::deserialize(&bytes).expect("deserialize");

        assert_eq!(back.task_run_id, spec.task_run_id);
        assert_eq!(back.task_id, spec.task_id);
        assert_eq!(back.project_id, spec.project_id);
        assert_eq!(back.trigger, spec.trigger);
        assert_eq!(back.base_branch, spec.base_branch);
        assert_eq!(back.task_branch, spec.task_branch);
        assert_eq!(back.read_source_project_ids, spec.read_source_project_ids);
        assert_eq!(back.flow, spec.flow);
        assert_eq!(back.model_id_per_role, spec.model_id_per_role);
        assert_eq!(back.commit_author_name, spec.commit_author_name);
        assert_eq!(back.commit_author_email, spec.commit_author_email);
    }

    #[test]
    fn task_run_report_bincode_roundtrip() {
        let report = TaskRunReport {
            task_run_id: "run-1".to_string(),
            outcome: TaskRunOutcome::PrOpened {
                url: "https://github.com/o/r/pull/1".to_string(),
                sha: "deadbeef".to_string(),
            },
            stages_completed: vec![RoleKind::Planner, RoleKind::Worker],
        };

        let bytes = bincode::serialize(&report).expect("serialize");
        let back: TaskRunReport = bincode::deserialize(&bytes).expect("deserialize");

        assert_eq!(back.task_run_id, report.task_run_id);
        assert_eq!(back.stages_completed, report.stages_completed);
        match back.outcome {
            TaskRunOutcome::PrOpened { url, sha } => {
                assert_eq!(url, "https://github.com/o/r/pull/1");
                assert_eq!(sha, "deadbeef");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }
}
