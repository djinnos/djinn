// djinn:allow-oversize — runtime spec types over size-guard threshold; split when touched substantively.
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
use std::fmt;

use djinn_core::models::TaskRunTrigger;
use djinn_core::tool_error::ErrorClass;
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
    Lead,
    /// Refinement tribunal role (advocate, adversary, or judge). The concrete
    /// agent type is resolved from `task.agent_type` at the role-overrides layer.
    Refinement,
}

/// Reply-loop guard family that terminated a degenerate session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopGuardKind {
    IdenticalToolFailure,
    PermissionDenial,
    IdenticalOutput,
    ConsecutiveFailures,
}

/// Typed error used inside the agent reply loop before it is mapped onto a
/// terminal [`TaskRunOutcome`] / supervisor [`StageOutcome`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopGuardTrip {
    pub kind: LoopGuardKind,
    pub offending_signature: String,
    pub threshold: u32,
    pub observed: u32,
    pub turn_span: (u32, u32),
    pub session_id: String,
}

impl fmt::Display for LoopGuardTrip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "loop guard tripped: kind={:?} signature={} observed={} threshold={} turn_span={:?} session_id={}",
            self.kind,
            self.offending_signature,
            self.observed,
            self.threshold,
            self.turn_span,
            self.session_id
        )
    }
}

impl std::error::Error for LoopGuardTrip {}

impl RoleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RoleKind::Planner => "planner",
            RoleKind::Worker => "worker",
            RoleKind::Reviewer => "reviewer",
            RoleKind::Verifier => "verifier",
            RoleKind::Architect => "architect",
            RoleKind::Lead => "lead",
            RoleKind::Refinement => "refinement",
        }
    }
}

/// How *likely* a role's task-run is to run the project's compile/test
/// toolchain — a dispatch-admission prior, NOT a capability boundary.
///
/// # Two layers, and this enum governs only the coarse one
///
/// Compile pressure is governed in two independent layers, and only the first
/// one reads this enum:
///
/// * **Layer 1 — dispatch admission (coarse, role-derived, this enum).**
///   `djinn-coordinator` charges a scarce build slot only to work that is
///   *certain* to compile, and `djinn-k8s` sizes the pod's CPU **request** the
///   same way. Gating on a role that compiles ~5% of the time would queue it
///   behind builds it almost never competes with and collapse throughput.
/// * **Layer 2 — the invocation lease (fine, measured, role-AGNOSTIC).**
///   `djinn-agent`'s `LeaseInvocationRunner` queues a CPU lease purely on the
///   invocation's own measured `cpu.stat` usage crossing a threshold. It takes
///   no role input at all, and `BuildLeaseService::queue` is keyed only by
///   `{task_id, task_run_id, invocation_id}`. A Reviewer's compile takes a
///   lease on exactly the same terms as a Worker's.
///
/// So this enum answers "should dispatch pre-charge a slot?", never "is this
/// role allowed to compile?". The answer to the latter is *everyone*, and it is
/// enforced by measurement in layer 2. See [`Self::gated_at_dispatch`].
///
/// # Why this lives in `djinn-runtime` and not where it is used
///
/// Both layer-1 consumers need the same answer and must never disagree.
/// Before this existed, `build_admission.rs` recorded the assumption in a
/// comment ("All currently dispatchable task-run roles are build-producing
/// work") and `djinn-k8s` recorded the opposite in code. `djinn-core`'s
/// `is_test_path` is the precedent: one classifier, one home, no drift.
///
/// `djinn-core` would also have worked as a home, but `RoleKind` already lives
/// here and both consumers already depend on this crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RoleResourceClass {
    /// Orchestration-first roles — Planner, Reviewer, Lead, and every
    /// Refinement sub-role — which are *unlikely* to run the project's
    /// compile/test toolchain, not incapable of it.
    ///
    /// Measured against production transcripts on 2026-07-25: 4 of 73 light
    /// task-run sessions (5.5%) ran a real compile; reviewers alone were 3 of
    /// 37 (8.1%), one of them running `cargo check -p …`, `cargo clippy
    /// --all-targets` and `cargo test --no-run` in a single session. Commit
    /// `1719ef8c3` (2026-06-20) had already recorded a reviewer burning ~12
    /// minutes on a cold `cargo check` "despite task-reviewer.md already
    /// instructing it not to". An earlier revision of this comment claimed
    /// these roles "never run the project's compile/test toolchain"; that was
    /// false when it was written and is corrected here.
    ///
    /// Light therefore means: fractional-core CPU **request**, and no
    /// build-admission slot — because pre-charging a slot 100% of the time for
    /// a ~5% event on an oversubscribed pool is the wrong trade. The ~5% that
    /// do compile are not unaccounted for; they are governed by the measured
    /// invocation lease (layer 2), exactly like a Worker's compile.
    Light,
    /// Roles whose task-run is *expected* to compile/build/test: Worker,
    /// Verifier, Architect, and any retry/resume of those. Full-core CPU
    /// request, and one build slot pre-charged at dispatch. Also the FAIL-SAFE
    /// default for a missing/unknown/newly-added role.
    BuildCapable,
}

impl RoleResourceClass {
    /// Classify the role that executes a task-run.
    ///
    /// Deliberately a catch-all (`_ => BuildCapable`) rather than an exhaustive
    /// match: an unrecognized, missing (`None`), or newly-introduced role must
    /// **fail safe to build-capable** so a pod that might compile is never
    /// under-provisioned and never escapes the admission cap. Only the
    /// explicitly-listed light roles are ever classed light.
    ///
    /// This is a *prior*, not a permission: see the type-level docs for why a
    /// Light role that does compile is still governed, by the measured
    /// invocation lease rather than by this enum.
    pub fn for_role(role: Option<RoleKind>) -> Self {
        match role {
            Some(
                RoleKind::Planner | RoleKind::Reviewer | RoleKind::Lead | RoleKind::Refinement,
            ) => Self::Light,
            _ => Self::BuildCapable,
        }
    }

    /// Classify by role NAME, for the layers that carry a role as a string.
    ///
    /// The coordinator's dispatch path names the concrete refinement sub-roles
    /// (`advocate`, `adversary`, `judge`) that [`RoleKind`] collapses into
    /// `Refinement`, so they are listed here explicitly. Matching is
    /// case-insensitive and, like [`Self::for_role`], anything unrecognized
    /// fails safe to build-capable.
    ///
    /// There is deliberately no `"grooming"` arm. Grooming is a Planner-driven
    /// flow, and it dispatches under the role NAME `planner`
    /// (`djinn-agent/src/roles/mod.rs`) — no layer ever produces the string
    /// `"grooming"` as a role. `djinn_coordinator::TaskRunRole::parse` has no
    /// grooming arm either, and its own test already asserts that
    /// `Some("grooming")` is Unclassified at admission. An arm here was
    /// unreachable in every caller and read as if a second grooming role
    /// existed; it was removed rather than made reachable, because making it
    /// reachable would mean inventing a dispatch role that nothing emits.
    pub fn for_role_name(role: &str) -> Self {
        match role.trim().to_ascii_lowercase().as_str() {
            "planner" | "reviewer" | "lead" | "refinement" | "advocate" | "adversary" | "judge" => {
                Self::Light
            }
            _ => Self::BuildCapable,
        }
    }

    /// Is a task-run of this class pre-charged a build slot at DISPATCH?
    ///
    /// This is the whole of layer 1 (see the type-level docs). It answers only
    /// "does admission reserve capacity before this task-run starts?" — never
    /// "may this task-run compile?".
    ///
    /// # There is deliberately no `may_take_invocation_lease` companion
    ///
    /// Lease eligibility is role-INDEPENDENT: every invocation, from every
    /// role, becomes lease-eligible by crossing the measured `cpu.stat`
    /// threshold in `djinn-agent`'s `LeaseInvocationRunner`. A predicate on
    /// this enum would therefore return `true` for every input — a constant
    /// dressed as a classification, and an open invitation for a future author
    /// to "fix" it into a role-dependent gate, which would silently starve the
    /// ~5% of light task-runs that do compile. The invariant is encoded as a
    /// documented absence plus tests: `LeaseInvocationRunner`'s config struct
    /// carries no role field (`djinn-agent`, exhaustive-destructure test), and
    /// the rendered launcher sidecar and `DJINN_LAUNCHER_LEASED_MILLICORES`
    /// are asserted identical across both classes (`djinn-k8s`).
    pub fn gated_at_dispatch(self) -> bool {
        matches!(self, Self::BuildCapable)
    }

    /// Stable telemetry/label string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::BuildCapable => "build-capable",
        }
    }
}

/// How much build capacity one workload occupies, in **build slots**.
///
/// # The unit is defined by the manifests, not by opinion
///
/// One slot is the CPU quota a granted build actually runs under:
/// `launcher_leased_millicores` (that is, the task-run pod's rendered
/// `cpu_limit`), 4000m on the default render. Weight is then
///
/// ```text
/// weight = ceil(cpu_millicores / slot_millicores)
/// ```
///
/// so it is DERIVED from the rendered manifests and never hand-picked. Raising
/// `DJINN_K8S_WARM_CPU_REQUEST` to `8` makes a warm Job weigh 2 automatically,
/// with no code change and no second place to remember to update.
///
/// # Why a warm Job and a task invocation weigh the same
///
/// It is tempting to assume a graph-warm Job -- a full workspace compile --
/// must outweigh a task-run that merely *might* compile briefly. Measured
/// against the actual render, that is false: the warm Job requests **4000m**
/// (`warm_cpu_request`/`warm_cpu_limit` both `"4"`, pinned by
/// `djinn-k8s/src/bin_packing_fixture_tests.rs`) and a leased task invocation
/// is lifted to **4000m** by the launcher (`cpu_limit` `"4"`). They request
/// identically.
///
/// The intuition is not wrong, it is about the wrong axis: a warm compile runs
/// far LONGER than a typical task-run compile. But a concurrency semaphore
/// governs *rate*, not *duration* -- it answers "how many of these may run at
/// once", and while they run these two cost the node the same. Duration belongs
/// to scheduling and deadlines, not to the weight of a slot. So 1:1 is the
/// measured answer, and `weight_for_millicores` keeps it honest if the render
/// ever diverges.
///
/// # Zero is a real weight
///
/// [`BuildSlotWeight::REENTRANT`] is how the non-double-charge rule is
/// expressed: an invocation lease taken by a task-run that already holds a
/// dispatch slot occupies zero, because that capacity was already bought at
/// spawn. See [`Self::REENTRANT`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct BuildSlotWeight(i64);

impl BuildSlotWeight {
    /// A workload that buys no capacity.
    ///
    /// Two populations are zero-weight, for different reasons:
    ///
    /// * **Light dispatch.** A Light task-run is not pre-charged a slot,
    ///   because pre-charging one 100% of the time for a ~5% event collapses
    ///   throughput (see [`RoleResourceClass::Light`]). The ~5% that do compile
    ///   are charged later, at full weight, by their invocation lease.
    /// * **Re-entrant invocation.** A build-capable task-run already reserved a
    ///   slot at dispatch for exactly the compile it is now running. Charging
    ///   its invocation lease again would double-count one physical compile --
    ///   the same defect, one layer down, that unifying the two authorities
    ///   exists to fix. It still takes a fencing token and the quota lift; it
    ///   just does not pay twice.
    pub const REENTRANT: Self = Self(0);

    /// One full build slot.
    pub const FULL: Self = Self(1);

    /// Derive a weight from a rendered CPU request/limit and the slot size.
    ///
    /// Rounds UP: a workload asking for more than a slot must occupy more than
    /// a slot, or the cap would under-count the node's real commitment. A
    /// positive request always yields at least [`Self::FULL`], so no non-zero
    /// workload can be made free by rounding.
    #[must_use]
    pub fn for_millicores(millicores: u32, slot_millicores: u32) -> Self {
        if millicores == 0 {
            return Self::REENTRANT;
        }
        if slot_millicores == 0 {
            // A zero slot size is a misconfiguration, not a licence to admit
            // unboundedly. Charge a full slot and let the cap do its job.
            return Self::FULL;
        }
        let slots = millicores.div_ceil(slot_millicores);
        Self(i64::from(slots.max(1)))
    }

    /// The weight a task-run's DISPATCH reserves (layer 1).
    ///
    /// Build-capable work is certain enough to compile that it is pre-charged
    /// before the pod exists; Light work is not. This is the whole of layer 1's
    /// capacity contribution.
    #[must_use]
    pub fn for_dispatch(class: RoleResourceClass, slot_millicores: u32) -> Self {
        if class.gated_at_dispatch() {
            Self::for_millicores(slot_millicores, slot_millicores)
        } else {
            Self::REENTRANT
        }
    }

    /// The durable slot count.
    #[must_use]
    pub const fn slots(self) -> i64 {
        self.0
    }

    /// Whether this workload buys capacity at all.
    #[must_use]
    pub const fn occupies(self) -> bool {
        self.0 > 0
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
    /// Reviewer-only resume: a prior run's worker stage already completed and
    /// its commits are durable on the mirror task_branch, but the run died in
    /// (or before) the reviewer stage — typically a Job-deadline / pod kill of
    /// the reviewer session. The host resolves this from `ReviewResponse` ONLY
    /// when a cheap durability check confirms the worker output is present
    /// (task_branch exists + is ahead of base), so re-running the worker would
    /// redo identical work. The sequence is `[Reviewer]`; workspace setup still
    /// clones task_branch first (so the reviewer sees the worker's diff), and
    /// the reviewer's `task_review_start` pre-stage transition is valid because
    /// the task is `needs_task_review` on the resume. When the worker output is
    /// NOT durable the host keeps `ReviewResponse` (full worker redo).
    ReviewResume,
    /// Lead intervention: a single-stage flow that runs the Lead agent on a
    /// task parked in `needs_lead_intervention`. The Lead inspects the stuck
    /// task and ends with `submit_decision`, which the supervisor maps to the
    /// terminal board transition (approve / reopen / decompose / close /
    /// escalate). Without this flow, lead-intervention tasks fell through to
    /// `NewTask` and looped worker→reviewer forever (the dead-end that wedged
    /// 82g0/78y9).
    Lead,
    /// Proposal-refinement tribunal: a single-stage flow that runs one
    /// refinement role (advocate, adversary, or judge) on a proposal. The
    /// concrete agent type is resolved from `task.agent_type` at the
    /// role-overrides layer.
    Refinement,
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
            SupervisorFlow::ReviewResume => "review_resume",
            SupervisorFlow::Lead => "lead",
            SupervisorFlow::Refinement => "refinement",
        }
    }
}

/// Free-function form of [`SupervisorFlow::role_sequence`] — exposed at the
/// crate root so call sites that only need the sequence can avoid pulling in
/// the full `SupervisorFlow` enum scope (matches the `lib.rs` re-export).
pub fn role_sequence(flow: SupervisorFlow) -> &'static [RoleKind] {
    use RoleKind::*;
    match flow {
        // Worker-only. The run ENDS after the worker stage: the supervisor
        // fires `submit_task_review` (in_progress → needs_task_review). The
        // coordinator re-dispatches a reviewer-only `ReviewResume` run (the
        // worker output is durable on the mirror task_branch).
        //
        // The wave-planner already broke the work down upstream, so no
        // upfront Planner stage here.
        SupervisorFlow::NewTask => &[Worker],
        // ReviewResponse (reviewer rejected / human asked for more) and
        // ConflictRetry (merge-conflict fixup) both re-enter at the worker and
        // hand off to review, exactly like NewTask — so they are also
        // worker-only.
        SupervisorFlow::ReviewResponse | SupervisorFlow::ConflictRetry => &[Worker],
        // Reviewer-only resume: the worker stage already ran on a prior run and
        // its commits are durable on the mirror task_branch (the host verified
        // this before choosing the flow). The task already moved to
        // needs_task_review. Skip straight to the reviewer, which reviews the
        // diff cloned from task_branch.
        SupervisorFlow::ReviewResume => &[Reviewer],
        SupervisorFlow::Spike => &[Architect],
        SupervisorFlow::Planning => &[Planner],
        // Single-stage: the Lead is the only actor. Its `submit_decision`
        // drives the terminal board transition (handled in the supervisor
        // body); there is no follow-on worker/reviewer in the same task-run —
        // `reopen` returns the task to `open` and the coordinator starts a
        // clean subsequent run.
        SupervisorFlow::Lead => &[Lead],
        // Single-stage refinement: the tribunal role (advocate, adversary, or
        // judge) runs once. The concrete agent type is resolved from
        // `task.agent_type` in the role-overrides layer.
        SupervisorFlow::Refinement => &[Refinement],
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
    // Exact dispatch attempt identity; never inferred from mutable state.
    #[serde(default)]
    pub task_attempt_id: Option<String>,
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
    /// Immutable knowledge-packing configuration resolved by the host at startup.
    #[serde(default)]
    pub knowledge_injection: djinn_core::models::KnowledgeInjectionConfig,
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
    /// older specs or host/worker version skew during a rolling deploy — the
    /// supervisor falls back to the bot identity. `#[serde(default)]` keeps older
    /// specs deserializable.
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
    /// Passive resume-via-git lifecycle metadata selected by the coordinator
    /// at re-dispatch time. This is a serde-only mirror of the coordinator's
    /// `ResumeLifecycleMetadata` (deliberately duplicated so the runtime/spec
    /// crate does not gain a coordinator dependency). `None` for the
    /// default/off path: when resume selection is disabled, or the task was
    /// not a re-dispatch candidate, the field stays absent and the existing
    /// dispatch behavior is preserved byte-for-byte on the wire.
    ///
    /// The field carries the full selection output: chosen source kind,
    /// checkpoint SHA or submit/review id when applicable, target ref, prior
    /// session/lineage context, and machine-readable rejected-candidate skip
    /// reasons (in the `extra.skipped` array). Downstream prompt/model/merge
    /// work (siblings `48ru`, `twsk`, `sy0g`) consume this from the
    /// task-run lifecycle payload rather than re-querying the coordinator.
    #[serde(default)]
    pub resume_lifecycle_metadata: Option<ResumeLifecycleMetadata>,
    /// Whether this task-run is a linked refinement evidence spike that must
    /// run under the read-only evidence-spike tool profile.  The worker pod
    /// reads this at stage-execution time to select
    /// `tool_schemas_evidence_spike()` and to pass `is_evidence_spike` into
    /// the reply-loop dispatch gate.  `false` (the `#[serde(default)]`
    /// sentinel) means "use the normal role tool surface".
    ///
    /// Derived at dispatch from the task's labels (`refinement-evidence` +
    /// `read-only`) via `djinn_core::models::task::is_evidence_spike`.
    #[serde(default)]
    pub is_evidence_spike: bool,
}

/// Resume-via-git lifecycle metadata selected by the coordinator at
/// re-dispatch time. This is a serde-only mirror of the coordinator's
/// `ResumeLifecycleMetadata` (see `djinn_coordinator::worker_lifecycle`). The
/// field shapes are intentionally aligned so the coordinator can serialize its
/// selection directly into a [`TaskRunSpec`] without a translation layer, and
/// the worker can deserialize the same shape when reading the spec off the
/// bincode wire.
///
/// All fields are `#[serde(default)]` so the older worker pods (running before
/// this additive change rolled out) continue to deserialize new specs
/// without an `EOF` on bincode decode. The default `None`/`false` value on
/// `considered` is the "resume selection was not consulted" signal; the
/// presence of `selection_reason` is the "a selection was made" signal.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ResumeLifecycleMetadata {
    /// Additive exact identity supplied by the coordinator. Mixed-version
    /// launchers omit either/both values, which deliberately remains NULL.
    #[serde(default)]
    pub dispatch_owner_incarnation_id: Option<String>,
    #[serde(default)]
    pub dispatch_group_id: Option<String>,
    /// Whether resume selection was considered for this dispatch/session.
    #[serde(default)]
    pub considered: bool,
    /// Selected checkpoint identifier, if any.
    #[serde(default)]
    pub checkpoint_id: Option<String>,
    /// Commit SHA selected as the resume base.
    #[serde(default)]
    pub commit_sha: Option<String>,
    /// Outcome of resume selection.
    #[serde(default)]
    pub selection_reason: Option<ResumeSelectionReason>,
    /// Chosen source kind (mirror of `djinn_coordinator::dispatch::resume_source::ResumeSourceKind`).
    /// `None` when the selection reason is not tied to a single source class
    /// (e.g. `MergeConflict`).
    #[serde(default)]
    pub source_kind: Option<ResumeSourceKind>,
    /// Target ref the future integration should check out. The selector only
    /// records it; the worker pod does not mutate the worktree from this
    /// task — that is `twsk`'s responsibility.
    #[serde(default)]
    pub target_ref: Option<String>,
    /// Legacy submit/review correlation id. New checkpoint selections leave this unset.
    #[serde(default)]
    pub submit_or_review_id: Option<String>,
    /// Prior session or lineage identifier that produced the chosen source.
    /// Used by the resume-prompt context (`48ru`) and as a hint for
    /// observability + decision telemetry.
    #[serde(default)]
    pub prior_session_lineage: Option<String>,
    /// Machine-readable record of every rejected candidate and its skip
    /// reason. Typed (not `serde_json::Value`) so the bincode wire format
    /// used for the worker→host frame can serialize/deserialize it.
    #[serde(default)]
    pub skipped: Vec<RejectedResumeSourceWire>,
    /// Previous model used before the termination that triggered this
    /// resume. Populated by the coordinator from model-rotation metadata
    /// when available. Used by the resume-prompt note (`48ru`) and
    /// model-rotation logic.
    #[serde(default)]
    pub previous_model: Option<String>,
    /// New/current model selected after model failover. When the
    /// coordinator's model-rotation metadata indicates a rotation from
    /// `previous_model` to a different model, this field carries the
    /// target model. Used by the resume-prompt note so the worker knows
    /// which model it is running on after failover.
    #[serde(default)]
    pub new_model: Option<String>,
    /// Human-readable failover/termination reason supplied by the
    /// coordinator. Populated from model-rotation reason metadata when
    /// available. Used by the resume-prompt note so the worker knows
    /// why the prior session was terminated and a new model was chosen.
    #[serde(default)]
    pub failover_reason: Option<String>,
    /// Last durable-progress summary from the prior session, when
    /// available. Used by the resume-prompt note to give the worker
    /// context about what was accomplished before termination.
    #[serde(default)]
    pub last_durable_progress_summary: Option<String>,
    /// Suggested command from prior checkpoint metadata, when available. Used by the
    /// resume-prompt note so the worker can re-verify quickly.
    #[serde(default)]
    pub verification_command: Option<String>,
}

/// Rejected candidate + machine-readable skip reason. Typed mirror of
/// `djinn_coordinator::dispatch::resume_source::RejectedResumeSource`, sized
/// for the bincode worker→host wire frame (no `serde_json::Value` in the
/// fields). Snake-case serde names match the coordinator definition so the
/// two stay wire-compatible.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RejectedResumeSourceWire {
    #[serde(default)]
    pub kind: Option<ResumeSourceKind>,
    #[serde(default)]
    pub target_ref: Option<String>,
    #[serde(default)]
    pub checkpoint_sha: Option<String>,
    #[serde(default)]
    pub submit_or_review_id: Option<String>,
    /// Reuse [`ResumeSelectionReason`] for the bucket the candidate fell
    /// into (e.g. `CheckpointUnsafe`, `MergeConflict`, `CheckpointMissing`,
    /// `Disabled`). This is the closest stable enum the runtime surface
    /// already exposes; siblings `48ru`/`twsk` interpret the value the
    /// same way the coordinator does.
    #[serde(default)]
    pub reason: Option<ResumeSelectionReason>,
}

/// Machine-readable resume source kind chosen by the selector. Mirror of
/// `djinn_coordinator::dispatch::resume_source::ResumeSourceKind`; aligned
/// `snake_case` serde names so the two definitions are wire-compatible.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeSourceKind {
    /// A safety-scanned checkpoint commit on the task branch.
    TaskBranchCheckpoint,
    /// A safety-scanned checkpoint commit on an alternate checkpoint ref.
    AlternateCheckpointRef,
    /// Clean task branch fallback when no prior output can be resumed safely.
    CleanTaskBranch,
}

/// Machine-readable classification for resume checkpoint selection decisions.
/// Mirror of `djinn_coordinator::ResumeSelectionReason`; aligned `snake_case`
/// serde names so the two definitions are wire-compatible.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeSelectionReason {
    LatestSafeCheckpoint,
    AlternateCheckpointRef,
    CleanTaskBranchFallback,
    NewerTaskBranch,
    CheckpointMissing,
    CheckpointUnsafe,
    MergeConflict,
    Disabled,
}

/// Wire-capable classification of a stage failure that was caused by a typed
/// provider error.
///
/// The typed `djinn_provider::provider::error::ProviderError` lives only inside
/// the worker pod (the reply loop calls the provider directly there), and it is
/// NOT serde-serializable, so it cannot ride the bincode report frame back to
/// the host. The host owns the *persistent* model circuit-breaker
/// (`HealthTracker`); a fast provider rejection that produces no token stall —
/// bad credential, persistent malformed request, repeated 5xx — therefore never
/// reached the host breaker before this field existed, and dispatch kept
/// re-selecting a model that is structurally broken for that user.
///
/// `stage.rs` downcasts the reply-loop's terminal error to `ProviderError` and
/// folds it into one of these *breaker-relevant* classes; the host
/// (`supervisor_runner.rs`) maps the class back onto `record_failure` /
/// `record_stall` using the task creator's scope. Only the classes the breaker
/// actually acts on are represented — failures the breaker deliberately ignores
/// (ContextOverflow) and untyped/legacy errors carry `None` so they never trip
/// it. A hard `Transport` death folds into `Transient` (gentle
/// consecutive-failure breaker) so a model that dies instantly on every dispatch
/// auto-disables instead of being re-selected forever.
///
/// The class is ALSO the coordinator's only evidence about *who* a failed
/// session should be blamed on, so the split between `Failure` and `Transient`
/// matters beyond the breaker: `Failure` is the task-attributable class (a
/// poisoned resume transcript the provider 400s, output we cannot parse) and is
/// the only one that arms the third-strike planner-remediation escalation;
/// `Transient` and `Throttle` are provider-attributable and must decay on the
/// cooldown ladder instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderFailureClass {
    /// Persistent-invalid-request / invalid-output — a "quiet but broken"
    /// failure the REQUEST is responsible for (the poisoned resume transcript
    /// the provider 400s, a body we cannot parse). Feeds the gentler
    /// consecutive-failure breaker (`record_failure`): a one-off may be
    /// transient, so it only demotes the model after it repeats.
    ///
    /// This is the task-attributable class, and therefore the ONLY one that
    /// arms the coordinator's third-strike planner-remediation escalation.
    /// Provider-side faults that reproduce independently of the request body
    /// (5xx, transport death) belong to [`Self::Transient`].
    Failure,
    /// Rate-limit / quota (throttle). Feeds the immediate-failover breaker
    /// (`record_stall`), matching the coordinator's throttle→stall intent: a
    /// throttled credential should fail over to the next model at once with a
    /// cooldown that outlasts the task's redispatch ladder.
    ///
    /// `retry_after_ms` carries the provider-stated reset window (parsed from a
    /// `Retry-After` header / rate-limit-reset, see
    /// `ProviderError::retry_after_ms()`) when the provider supplied one. The
    /// coordinator uses it as a *floor* under the escalating redispatch cooldown
    /// (A6): a multi-hour quota window must not be probed on the fixed ~30-min
    /// ladder. `None` when the provider stated no reset, in which case the
    /// ordinary ladder applies. `#[serde(default)]` keeps it additive over the
    /// bincode wire (positional, non-self-describing): a version-matched worker
    /// is required for the field to decode, exactly as documented on the enum.
    Throttle {
        #[serde(default)]
        retry_after_ms: Option<u64>,
    },
    /// Auth / credential failure (401/403 — e.g. a revoked or invalid OAuth
    /// token). Deterministic, not transient: a retry with the same dead
    /// credential always fails. The host trips the breaker IMMEDIATELY (like a
    /// throttle, via `record_stall`) so dispatch fails over to the user's next
    /// model at once instead of probing the dead one three times, AND drives
    /// credential-revocation surfacing (mark the credential revoked + notify the
    /// owner to reconnect). Added last to preserve the bincode discriminants of
    /// `Failure`/`Throttle` on the worker→host report wire.
    AuthInvalid,
    /// Transient PROVIDER-side fault — a 5xx (`server_error` /
    /// `server_is_overloaded`, incl. the in-stream `response.failed` form) or a
    /// hard network/transport death. The provider is broken, not the task: the
    /// same transcript redispatched onto a healthy backend succeeds, so this
    /// class must never be read as evidence that the task itself is
    /// undispatchable.
    ///
    /// Breaker behaviour is deliberately IDENTICAL to [`Self::Failure`]: the
    /// host feeds it to the gentle consecutive-failure breaker
    /// (`record_failure`), so model-health/auto-disable is unchanged and a model
    /// that 5xx's on every dispatch still auto-disables. What changes is
    /// *task attribution* — the coordinator spares the two task-blaming counters
    /// (the planner-remediation `provider_failure_streak` and the terminal
    /// `dispatch_failure_streak`) for this class, exactly as it already does for
    /// [`Self::Throttle`], while the escalating redispatch cooldown and the
    /// per-`(scope, model)` failover still apply. Incident (task `2gq7`,
    /// 2026-07-29): three independent OpenAI 500s across three sessions armed
    /// the third-strike escalation and minted a "Planner remediation" task whose
    /// reason asserted a poisoned resume transcript that never existed.
    ///
    /// `retry_after_ms` carries a provider-stated reset window when one was
    /// supplied (rare on a 5xx; `None` otherwise) and floors the redispatch
    /// cooldown the same way a throttle's does. `#[serde(default)]` keeps the
    /// field additive over the wire, like `Throttle`'s.
    ///
    /// Appended LAST — after `AuthInvalid` — for exactly the reason
    /// `AuthInvalid` was: the report frame rides a positional,
    /// non-self-describing bincode wire, so inserting or reordering a variant
    /// would shift the discriminants of `Failure`/`Throttle`/`AuthInvalid` and
    /// mis-decode reports from a version-skewed worker mid-deploy. A worker that
    /// EMITS `Transient` therefore requires a version-matched host.
    Transient {
        #[serde(default)]
        retry_after_ms: Option<u64>,
    },
}

/// Terminal outcome of a task-run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TaskRunOutcome {
    PrOpened {
        url: String,
        sha: String,
    },
    /// Planner decided the task should not execute.
    Closed {
        reason: String,
    },
    /// Planner/architect surfaced a question that blocks automated execution
    /// (e.g. ambiguous scope, missing design decision).
    Escalated {
        reason: String,
    },
    Failed {
        stage: String,
        reason: String,
        /// Set when the failure was a typed provider error the host breaker
        /// should act on (see [`ProviderFailureClass`]). `None` for non-LLM
        /// failures (git push, PR open) and for provider errors the breaker
        /// deliberately ignores. `#[serde(default)]` keeps serde formats that can
        /// omit fields decoding this as `None`.
        #[serde(default)]
        provider_failure: Option<ProviderFailureClass>,
        /// Machine-readable class for structured tool/provider-write failures.
        ///
        /// Keep these additive fields serialized even when `None`: the
        /// worker→host report wire uses bincode, whose struct fields are
        /// positional rather than self-describing. `skip_serializing_if` would
        /// omit `None` bytes and make same-version bincode decoding hit EOF.
        #[serde(default)]
        error_class: Option<ErrorClass>,
        /// Actionable recovery hint for the agent/operator, when available.
        #[serde(default)]
        hint: Option<String>,
        /// Bounded upstream response/detail excerpt for compact rendering.
        #[serde(default)]
        body_excerpt: Option<String>,
    },
    Interrupted,
    /// The worker stage completed and the supervisor fired `submit_task_review`
    /// (in_progress → needs_task_review). The run ends here — no PR is opened.
    /// The coordinator's next dispatch pass picks up the `needs_task_review`
    /// task and runs the reviewer. This is the terminal outcome of every
    /// worker-only flow (NewTask / ReviewResponse / ConflictRetry); it maps to a
    /// `Completed` task-run status (the worker stage genuinely succeeded) so it
    /// feeds `record_success` and never trips the model breaker.
    ///
    /// Added LAST to preserve the bincode discriminants of the existing variants
    /// on the worker→host `TerminalReport` wire (bincode is positional/
    /// non-self-describing, so reordering would break cross-version decoding
    /// during a rolling deploy — same rationale as `ProviderFailureClass::AuthInvalid`).
    WorkerSubmitted,
    /// Reply-loop guard terminated a degenerate session before the task could
    /// make progress. Added LAST to preserve bincode discriminants of all
    /// existing worker→host terminal-report variants.
    LoopGuardTripped {
        kind: LoopGuardKind,
        offending_signature: String,
        threshold: u32,
        observed: u32,
        turn_span: (u32, u32),
        session_id: String,
    },
    /// The worker deliberately parked after budget wind-down. Added LAST to
    /// preserve bincode discriminants of all existing terminal-report variants.
    Parked {
        reason: String,
        wind_down_ignored: bool,
        session_id: String,
        tokens_in: i64,
        tokens_out: i64,
    },
    /// A blocking pre-task command failed, timed out, or was cancelled, or a
    /// required service readiness check failed before any agent session or work
    /// attempt was created.  The run is classified as an environmental
    /// non-attempt: no `TaskRunSupervisor::run` was invoked, no agent
    /// session/work attempt exists, and no quality strike, arbiter penalty, or
    /// park-rung penalty should be applied.
    ///
    /// `stages_completed` is always empty for this outcome.
    ///
    /// Added LAST to preserve bincode discriminants of all existing
    /// terminal-report variants.
    EnvironmentalNonAttempt {
        /// Machine-readable reason: `pre_task_failed`, `pre_task_timed_out`,
        /// `pre_task_cancelled`, or `service_readiness_failed`.
        reason: String,
    },
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
    fn new_task_flow_is_worker_only() {
        // Planner ran upstream as a Planning task; NewTask is the worker's
        // domain and doesn't re-plan. The reviewer leg no longer rides this
        // run: the worker submits to task review, and
        // a passing task review re-dispatches a reviewer-only ReviewResume.
        let seq = SupervisorFlow::NewTask.role_sequence();
        assert!(!seq.contains(&RoleKind::Planner));
        assert!(!seq.contains(&RoleKind::Reviewer));
        assert_eq!(seq, &[RoleKind::Worker]);
    }

    #[test]
    fn review_response_and_conflict_retry_are_worker_only() {
        // Both re-enter at the worker and must verify before the next review,
        // so neither carries a reviewer stage anymore.
        assert_eq!(
            SupervisorFlow::ReviewResponse.role_sequence(),
            &[RoleKind::Worker]
        );
        assert_eq!(
            SupervisorFlow::ConflictRetry.role_sequence(),
            &[RoleKind::Worker]
        );
    }

    #[test]
    fn review_resume_is_reviewer_only() {
        // The reviewer leg arrives exclusively via the ReviewResume path now;
        // ReviewResume stays reviewer-only.
        assert_eq!(
            SupervisorFlow::ReviewResume.role_sequence(),
            &[RoleKind::Reviewer]
        );
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
    fn worker_submitted_outcome_bincode_roundtrip() {
        // The terminal worker outcome (hand-off to review) must survive the
        // worker→host bincode frame.
        let report = TaskRunReport {
            task_run_id: "run-ws".to_string(),
            outcome: TaskRunOutcome::WorkerSubmitted,
            stages_completed: vec![RoleKind::Worker],
        };
        let bytes = bincode::serialize(&report).expect("serialize");
        let back: TaskRunReport = bincode::deserialize(&bytes).expect("deserialize");
        assert!(matches!(back.outcome, TaskRunOutcome::WorkerSubmitted));
        assert_eq!(back.stages_completed, vec![RoleKind::Worker]);
    }

    #[test]
    fn task_run_spec_bincode_roundtrip() {
        let mut per_role = HashMap::new();
        per_role.insert(RoleKind::Planner, "anthropic/claude-sonnet-4.5".to_string());
        per_role.insert(RoleKind::Worker, "anthropic/claude-opus-4.7".to_string());

        let spec = TaskRunSpec {
            task_run_id: "019e6a03-8aef-7201-9c9d-d7ba17613a0b".to_string(),
            task_attempt_id: None,
            task_id: "task-abc".to_string(),
            project_id: "proj-xyz".to_string(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".to_string(),
            task_branch: "djinn/task-abc".to_string(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: per_role,
            read_source_project_ids: vec!["proj-read-1".to_string()],
            knowledge_injection: djinn_core::models::KnowledgeInjectionConfig {
                knowledge_injection_budget_bytes: 4_096,
                knowledge_injection_line_cap_bytes: 256,
                knowledge_injection_limit: 3,
                injection_starvation_threshold_percent: 50,
                injection_starvation_query_floor: 20,
                retrieval_health_window_minutes: 1_440,
            },
            github_owner: None,
            github_install_token: None,
            commit_author_name: Some("Ada Lovelace".to_string()),
            commit_author_email: Some("1+ada@users.noreply.github.com".to_string()),
            resume_lifecycle_metadata: None,
            is_evidence_spike: false,
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
        assert_eq!(back.knowledge_injection, spec.knowledge_injection);
        assert_eq!(back.commit_author_name, spec.commit_author_name);
        assert_eq!(back.commit_author_email, spec.commit_author_email);
    }

    /// AC: "Selection metadata attached to the dispatch/session lifecycle path
    /// includes chosen source kind, checkpoint SHA or submit/review id when
    /// applicable, target ref, prior session/lineage context, and
    /// rejected-candidate skip reasons." A `TaskRunSpec` with a populated
    /// `resume_lifecycle_metadata` must round-trip through bincode (the
    /// worker→host wire format) and surface all selection fields so the
    /// downstream prompt/model/merge work in siblings `48ru`/`twsk`/`sy0g`
    /// can read the chosen source, the target ref, the checkpoint SHA (or
    /// submit/review id), the prior session lineage, and the rejected-
    /// candidate skip reasons — every field needed for the integration
    /// without dropping the metadata on the wire.
    #[test]
    fn task_run_spec_carries_full_resume_lifecycle_metadata_through_bincode() {
        let mut per_role = HashMap::new();
        per_role.insert(RoleKind::Worker, "anthropic/claude-opus-4.7".to_string());

        let spec = TaskRunSpec {
            task_run_id: "019f1a03-8aef-7201-9c9d-d7ba17613a0b".to_string(),
            task_attempt_id: None,
            task_id: "task-resume".to_string(),
            project_id: "proj-xyz".to_string(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".to_string(),
            task_branch: "djinn/task-resume".to_string(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: per_role,
            read_source_project_ids: vec![],
            knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: Some(ResumeLifecycleMetadata {
                dispatch_owner_incarnation_id: Some(
                    "00000000-0000-7000-8000-000000000001".to_string(),
                ),
                dispatch_group_id: Some("00000000-0000-7000-8000-000000000002".to_string()),
                considered: true,
                checkpoint_id: Some("ckpt-1".to_string()),
                commit_sha: Some("deadbeef".to_string()),
                selection_reason: Some(ResumeSelectionReason::LatestSafeCheckpoint),
                source_kind: Some(ResumeSourceKind::TaskBranchCheckpoint),
                target_ref: Some("refs/heads/task/resume-target".to_string()),
                submit_or_review_id: Some("review-7".to_string()),
                prior_session_lineage: Some("session-prev".to_string()),
                skipped: vec![RejectedResumeSourceWire {
                    kind: Some(ResumeSourceKind::CleanTaskBranch),
                    target_ref: Some("refs/heads/task/resume-target".to_string()),
                    checkpoint_sha: None,
                    submit_or_review_id: Some("review-7".to_string()),
                    reason: Some(ResumeSelectionReason::CheckpointUnsafe),
                }],
                previous_model: Some("anthropic/claude-opus-4.7".to_string()),
                new_model: Some("openai/gpt-4.1".to_string()),
                failover_reason: Some("no_durable_progress_streak".to_string()),
                last_durable_progress_summary: Some("Implemented core feature".to_string()),
                verification_command: Some("cargo test".to_string()),
            }),
            is_evidence_spike: false,
        };

        // Bincode round-trip (the worker→host wire format).
        let bytes = bincode::serialize(&spec).expect("serialize");
        let back: TaskRunSpec = bincode::deserialize(&bytes).expect("deserialize");

        let meta = back
            .resume_lifecycle_metadata
            .as_ref()
            .expect("resume_lifecycle_metadata must round-trip through bincode");
        assert!(meta.considered, "considered flag must survive the wire");
        assert_eq!(meta.checkpoint_id.as_deref(), Some("ckpt-1"));
        assert_eq!(meta.commit_sha.as_deref(), Some("deadbeef"));
        assert_eq!(
            meta.selection_reason,
            Some(ResumeSelectionReason::LatestSafeCheckpoint)
        );
        // Every machine-readable selection field the AC requires must
        // be present after the round-trip.
        assert_eq!(
            meta.source_kind,
            Some(ResumeSourceKind::TaskBranchCheckpoint),
            "chosen source kind must reach the spec"
        );
        assert_eq!(
            meta.target_ref.as_deref(),
            Some("refs/heads/task/resume-target"),
            "target ref must reach the spec"
        );
        assert_eq!(
            meta.submit_or_review_id.as_deref(),
            Some("review-7"),
            "submit/review id must reach the spec when applicable"
        );
        assert_eq!(
            meta.prior_session_lineage.as_deref(),
            Some("session-prev"),
            "prior session/lineage context must reach the spec"
        );
        assert_eq!(
            meta.skipped.len(),
            1,
            "rejected-candidate skip reasons must reach the spec"
        );
        assert_eq!(
            meta.skipped[0].kind,
            Some(ResumeSourceKind::CleanTaskBranch),
            "rejected-candidate kind must reach the spec"
        );
        assert_eq!(
            meta.skipped[0].reason,
            Some(ResumeSelectionReason::CheckpointUnsafe),
            "rejected-candidate skip reason must reach the spec"
        );
        assert_eq!(
            meta.new_model.as_deref(),
            Some("openai/gpt-4.1"),
            "failover target model must reach the spec"
        );
        assert_eq!(
            meta.failover_reason.as_deref(),
            Some("no_durable_progress_streak"),
            "failover reason must reach the spec"
        );
    }

    /// Disabled / no-resume path: when the coordinator did not select a
    /// resume source, the spec keeps `resume_lifecycle_metadata` as `None`
    /// and the legacy default/off dispatch behavior is preserved byte-for-
    /// byte on the bincode wire.
    #[test]
    fn task_run_spec_default_off_has_no_resume_lifecycle_metadata() {
        let spec = TaskRunSpec {
            task_run_id: "run-default".to_string(),
            task_attempt_id: None,
            task_id: "task-default".to_string(),
            project_id: "proj-1".to_string(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".to_string(),
            task_branch: "djinn/task-default".to_string(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: HashMap::new(),
            read_source_project_ids: vec![],
            knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: false,
        };

        let bytes = bincode::serialize(&spec).expect("serialize");
        let back: TaskRunSpec = bincode::deserialize(&bytes).expect("deserialize");

        assert!(
            back.resume_lifecycle_metadata.is_none(),
            "default/off dispatch must not inject a resume metadata payload"
        );
    }

    #[test]
    fn task_run_spec_evidence_spike_flag_roundtrips_through_bincode() {
        let mut spec = TaskRunSpec {
            task_run_id: "run-ev".to_string(),
            task_attempt_id: None,
            task_id: "task-ev".to_string(),
            project_id: "proj-1".to_string(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".to_string(),
            task_branch: "djinn/task-ev".to_string(),
            flow: SupervisorFlow::Spike,
            model_id_per_role: HashMap::new(),
            read_source_project_ids: vec![],
            knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: true,
        };

        let bytes = bincode::serialize(&spec).expect("serialize");
        let back: TaskRunSpec = bincode::deserialize(&bytes).expect("deserialize");
        assert!(
            back.is_evidence_spike,
            "is_evidence_spike must survive bincode round-trip"
        );

        // Default value is false for normal tasks.
        spec.is_evidence_spike = false;
        let bytes2 = bincode::serialize(&spec).expect("serialize");
        let back2: TaskRunSpec = bincode::deserialize(&bytes2).expect("deserialize");
        assert!(
            !back2.is_evidence_spike,
            "non-evidence-spike spec must round-trip as false"
        );
    }

    #[test]
    fn task_run_spec_from_demand_evidence_contract_selects_evidence_spike_profile() {
        // Simulate the TaskRunSpec built for a spike created by the
        // `proposal_refinement_demand_evidence` contract (labels include
        // "refinement-evidence" and "read-only"). The selector in
        // djinn-core::is_evidence_spike must set the flag, and the runtime
        // spec must carry it through serialization.
        let mut spec = TaskRunSpec {
            task_run_id: "run-evidence".to_string(),
            task_attempt_id: None,
            task_id: "task-evidence".to_string(),
            project_id: "proj-1".to_string(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".to_string(),
            task_branch: "djinn/task-evidence".to_string(),
            flow: SupervisorFlow::Spike,
            model_id_per_role: HashMap::new(),
            read_source_project_ids: vec![],
            knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: true,
        };

        let bytes = bincode::serialize(&spec).expect("serialize");
        let back: TaskRunSpec = bincode::deserialize(&bytes).expect("deserialize");
        assert!(
            back.is_evidence_spike,
            "demand-evidence contract spike must carry the evidence-spike profile through the runtime spec"
        );

        // A normal Architect spike without the read-only marker must round-trip
        // with the flag unset.
        spec.is_evidence_spike = false;
        let bytes2 = bincode::serialize(&spec).expect("serialize");
        let back2: TaskRunSpec = bincode::deserialize(&bytes2).expect("deserialize");
        assert!(
            !back2.is_evidence_spike,
            "ordinary Architect spike must not be downgraded to evidence-spike profile"
        );
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

    #[test]
    fn failed_outcome_throttle_with_retry_after_bincode_roundtrip() {
        // A6: the `Throttle { retry_after_ms }` shape must survive the bincode
        // wire so the host coordinator can floor the redispatch cooldown on a
        // provider-stated reset.
        let report = TaskRunReport {
            task_run_id: "run-2".to_string(),
            outcome: TaskRunOutcome::Failed {
                stage: "worker".to_string(),
                reason: "rate limited".to_string(),
                provider_failure: Some(ProviderFailureClass::Throttle {
                    retry_after_ms: Some(5 * 60 * 60 * 1000),
                }),
                error_class: None,
                hint: None,
                body_excerpt: None,
            },
            stages_completed: vec![RoleKind::Worker],
        };

        let bytes = bincode::serialize(&report).expect("serialize");
        let back: TaskRunReport = bincode::deserialize(&bytes).expect("deserialize");

        match back.outcome {
            TaskRunOutcome::Failed {
                stage,
                reason,
                provider_failure,
                ..
            } => {
                assert_eq!(stage, "worker");
                assert_eq!(reason, "rate limited");
                assert_eq!(
                    provider_failure,
                    Some(ProviderFailureClass::Throttle {
                        retry_after_ms: Some(5 * 60 * 60 * 1000)
                    })
                );
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn failed_outcome_throttle_without_retry_after_bincode_roundtrip() {
        // The provider may state no reset; the field is then `None` and the
        // ordinary escalating ladder applies.
        let report = TaskRunReport {
            task_run_id: "run-3".to_string(),
            outcome: TaskRunOutcome::Failed {
                stage: "worker".to_string(),
                reason: "rate limited".to_string(),
                provider_failure: Some(ProviderFailureClass::Throttle {
                    retry_after_ms: None,
                }),
                error_class: None,
                hint: None,
                body_excerpt: None,
            },
            stages_completed: vec![RoleKind::Worker],
        };

        let bytes = bincode::serialize(&report).expect("serialize");
        let back: TaskRunReport = bincode::deserialize(&bytes).expect("deserialize");

        match back.outcome {
            TaskRunOutcome::Failed {
                provider_failure, ..
            } => assert_eq!(
                provider_failure,
                Some(ProviderFailureClass::Throttle {
                    retry_after_ms: None
                })
            ),
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn failed_outcome_transient_bincode_roundtrip() {
        // The `Transient` class (provider-side 5xx / transport death) must
        // survive the worker→host report wire, including its optional
        // provider-stated reset window.
        for retry_after_ms in [None, Some(90_000u64)] {
            let report = TaskRunReport {
                task_run_id: "run-4".to_string(),
                outcome: TaskRunOutcome::Failed {
                    stage: "planner".to_string(),
                    reason: "server_is_overloaded: Our servers are currently overloaded"
                        .to_string(),
                    provider_failure: Some(ProviderFailureClass::Transient { retry_after_ms }),
                    error_class: None,
                    hint: None,
                    body_excerpt: None,
                },
                stages_completed: vec![RoleKind::Planner],
            };

            let bytes = bincode::serialize(&report).expect("serialize");
            let back: TaskRunReport = bincode::deserialize(&bytes).expect("deserialize");

            match back.outcome {
                TaskRunOutcome::Failed {
                    provider_failure, ..
                } => assert_eq!(
                    provider_failure,
                    Some(ProviderFailureClass::Transient { retry_after_ms }),
                    "the transient class must round-trip verbatim"
                ),
                other => panic!("unexpected outcome: {other:?}"),
            }
        }
    }

    /// Wire-compat guard for [`ProviderFailureClass`].
    ///
    /// bincode is positional and non-self-describing: an enum is encoded as its
    /// variant INDEX (u32 LE), so inserting or reordering a variant silently
    /// re-points every previously-encoded byte sequence at a different variant.
    /// A version-skewed worker mid-deploy would then have its `Throttle` decoded
    /// as `AuthInvalid` (or worse). `Transient` was therefore appended LAST,
    /// after `AuthInvalid`, exactly as `AuthInvalid` was appended after
    /// `Throttle`.
    ///
    /// This pins the pre-existing indices as literal bytes. If someone inserts a
    /// variant anywhere but the end, these assertions fail rather than shipping
    /// a mid-deploy mis-decode.
    #[test]
    fn provider_failure_class_wire_discriminants_are_append_only() {
        // Encoded form of each variant, exactly as an older worker emits it.
        const FAILURE: [u8; 4] = [0, 0, 0, 0];
        const THROTTLE_NONE: [u8; 5] = [1, 0, 0, 0, 0];
        const AUTH_INVALID: [u8; 4] = [2, 0, 0, 0];
        const TRANSIENT_NONE: [u8; 5] = [3, 0, 0, 0, 0];

        assert_eq!(
            bincode::serialize(&ProviderFailureClass::Failure).unwrap(),
            FAILURE.to_vec(),
            "Failure must stay variant index 0"
        );
        assert_eq!(
            bincode::serialize(&ProviderFailureClass::Throttle {
                retry_after_ms: None
            })
            .unwrap(),
            THROTTLE_NONE.to_vec(),
            "Throttle must stay variant index 1"
        );
        assert_eq!(
            bincode::serialize(&ProviderFailureClass::AuthInvalid).unwrap(),
            AUTH_INVALID.to_vec(),
            "AuthInvalid must stay variant index 2"
        );
        assert_eq!(
            bincode::serialize(&ProviderFailureClass::Transient {
                retry_after_ms: None
            })
            .unwrap(),
            TRANSIENT_NONE.to_vec(),
            "Transient is the newest variant and must be appended LAST, at index 3"
        );

        // And the decode direction: bytes minted by a worker that predates
        // `Transient` still land on the variant they were written as.
        assert_eq!(
            bincode::deserialize::<ProviderFailureClass>(&FAILURE).unwrap(),
            ProviderFailureClass::Failure
        );
        assert_eq!(
            bincode::deserialize::<ProviderFailureClass>(&THROTTLE_NONE).unwrap(),
            ProviderFailureClass::Throttle {
                retry_after_ms: None
            }
        );
        assert_eq!(
            bincode::deserialize::<ProviderFailureClass>(&AUTH_INVALID).unwrap(),
            ProviderFailureClass::AuthInvalid
        );
        assert_eq!(
            bincode::deserialize::<ProviderFailureClass>(&TRANSIENT_NONE).unwrap(),
            ProviderFailureClass::Transient {
                retry_after_ms: None
            }
        );
    }

    fn loop_guard_outcome(kind: LoopGuardKind) -> TaskRunOutcome {
        TaskRunOutcome::LoopGuardTripped {
            kind,
            offending_signature: "tool_failure:shell:{\"command\":\"cargo test\"}:error: denied"
                .to_string(),
            threshold: 3,
            observed: 3,
            turn_span: (4, 6),
            session_id: "session-1".to_string(),
        }
    }

    #[test]
    fn loop_guard_outcome_bincode_roundtrip_for_each_kind() {
        for kind in [
            LoopGuardKind::IdenticalToolFailure,
            LoopGuardKind::PermissionDenial,
            LoopGuardKind::IdenticalOutput,
            LoopGuardKind::ConsecutiveFailures,
        ] {
            let report = TaskRunReport {
                task_run_id: "run-loop-guard".to_string(),
                outcome: loop_guard_outcome(kind),
                stages_completed: vec![RoleKind::Worker],
            };

            let bytes = bincode::serialize(&report).expect("serialize");
            let back: TaskRunReport = bincode::deserialize(&bytes).expect("deserialize");

            match back.outcome {
                TaskRunOutcome::LoopGuardTripped {
                    kind: back_kind,
                    offending_signature,
                    threshold,
                    observed,
                    turn_span,
                    session_id,
                } => {
                    assert_eq!(back_kind, kind);
                    assert!(offending_signature.contains("tool_failure:shell"));
                    assert_eq!(threshold, 3);
                    assert_eq!(observed, 3);
                    assert_eq!(turn_span, (4, 6));
                    assert_eq!(session_id, "session-1");
                }
                other => panic!("unexpected outcome: {other:?}"),
            }
        }
    }

    #[test]
    fn task_run_outcome_bincode_discriminants_keep_existing_variants_stable() {
        let old_variants = [
            TaskRunOutcome::PrOpened {
                url: "https://example.test/pr/1".to_string(),
                sha: "abc123".to_string(),
            },
            TaskRunOutcome::Closed {
                reason: "done".to_string(),
            },
            TaskRunOutcome::Escalated {
                reason: "blocked".to_string(),
            },
            TaskRunOutcome::Failed {
                stage: "worker".to_string(),
                reason: "boom".to_string(),
                provider_failure: None,
                error_class: None,
                hint: None,
                body_excerpt: None,
            },
            TaskRunOutcome::Interrupted,
            TaskRunOutcome::WorkerSubmitted,
        ];

        for (expected_discriminant, outcome) in old_variants.into_iter().enumerate() {
            let bytes = bincode::serialize(&outcome).expect("serialize old variant");
            assert_eq!(
                &bytes[..4],
                &(expected_discriminant as u32).to_le_bytes(),
                "existing variant discriminant shifted for {outcome:?}"
            );
            let decoded: TaskRunOutcome = bincode::deserialize(&bytes).expect("decode old frame");
            assert_eq!(
                std::mem::discriminant(&decoded),
                std::mem::discriminant(&outcome)
            );
        }

        let new_bytes =
            bincode::serialize(&loop_guard_outcome(LoopGuardKind::IdenticalToolFailure))
                .expect("serialize new variant");
        assert_eq!(&new_bytes[..4], &6u32.to_le_bytes());
    }

    #[test]
    fn environmental_non_attempt_bincode_roundtrip() {
        let report = TaskRunReport {
            task_run_id: "run-env".to_string(),
            outcome: TaskRunOutcome::EnvironmentalNonAttempt {
                reason: "pre_task_failed".to_string(),
            },
            stages_completed: Vec::new(),
        };
        let bytes = bincode::serialize(&report).expect("serialize");
        let back: TaskRunReport = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(back.task_run_id, "run-env");
        assert!(back.stages_completed.is_empty());
        match back.outcome {
            TaskRunOutcome::EnvironmentalNonAttempt { reason } => {
                assert_eq!(reason, "pre_task_failed");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn environmental_non_attempt_discriminant_is_appended_after_parked() {
        // Parked is discriminant 7; EnvironmentalNonAttempt must be 8.
        let parked = TaskRunOutcome::Parked {
            reason: "budget".into(),
            wind_down_ignored: false,
            session_id: "s".into(),
            tokens_in: 0,
            tokens_out: 0,
        };
        let parked_bytes = bincode::serialize(&parked).expect("serialize parked");
        assert_eq!(&parked_bytes[..4], &7u32.to_le_bytes());

        let env = TaskRunOutcome::EnvironmentalNonAttempt {
            reason: "pre_task_failed".into(),
        };
        let env_bytes = bincode::serialize(&env).expect("serialize env");
        assert_eq!(&env_bytes[..4], &8u32.to_le_bytes());
    }
}
