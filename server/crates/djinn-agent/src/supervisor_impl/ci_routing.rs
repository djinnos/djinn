//! Supervisor-side validation of a Lead result on a CI route (proposal
//! `nafu`, wave 4).
//!
//! Wave 1 made the route durable and gave `resolve_tier2_lease` an atomic
//! current-identity guard. Wave 2 made the classification closed. This module
//! is the third fence: it decides *what a Lead result is allowed to mean*
//! before the guard is consulted, and *what may happen* once the guard has
//! answered.
//!
//! # The two halves, and why they are separate
//!
//! [`adjudicate`] is validation. It turns a Lead response — a delivered
//! `submit_decision` payload, a session that finalized through the wrong
//! tool, a session that never finalized, or a session that timed out — into
//! exactly one [`CiLeadPlan`]. It cannot fail: every invalid, unsupported, or
//! timed-out response degrades to a **diagnostic reopen**, because the
//! proposal makes that the single fallback and forbids a second Lead session
//! for the same evidence.
//!
//! [`board_effect`] is application. It takes the plan plus the boolean
//! `resolve_tier2_lease` returned and produces the effects the supervisor may
//! perform. A failed guard produces [`CiBoardEffect::None`] — no board
//! transition, no worker, no park, no supersede, no mutation of the route
//! that superseded it. The repository has already written
//! `superseded_before_apply` inside the same transaction; there is nothing
//! left for this side to do, and doing anything would be the bug.
//!
//! Keeping them apart is what makes "exactly one worker for a valid current
//! reopen" and "zero of everything for a stale one" countable rather than
//! asserted by name — see [`CiEffectCounts`].
//!
//! # What is deliberately absent
//!
//! There is no retry plan, no provider call, and no dependency-graph edge in
//! any type here. Lead cannot authorize `rerun_failed_jobs` or
//! `enable_auto_merge`, and `approve` has no representation at all: the mere
//! existence of a [`CiAdjudicationContext`] means the CI evidence was not
//! passing, and approve is legal only on the passing path where no context
//! exists. That is stronger than a boolean flag Lead could argue with.

use djinn_db::{
    CiDiagnosticReason, CiLane, CiOriginState, CiReopenMode, CiRouteOutcome, CiTier2Reason,
    CiTier2Resolution,
};
use djinn_supervisor::StageOutcome;

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

/// The evidence-scoped facts a Lead session was dispatched under.
///
/// Its presence is the switch: a Lead stage that carries one is adjudicating
/// non-passing CI evidence and is bound by this module; a Lead stage without
/// one keeps the pre-existing arbiter contract untouched. That is the
/// "feature disabled" row of the mixed-version matrix — an old coordinator
/// writes no `ci_route` block, so a new supervisor behaves exactly as before.
#[derive(Clone, Debug)]
pub(crate) struct CiAdjudicationContext {
    /// Which lane produced the evidence.
    pub lane: CiLane,
    /// The board state the route must reopen *from*. Read by
    /// [`board_effect`], which wave 3 wires.
    #[allow(dead_code)]
    pub origin_state: CiOriginState,
    /// Why the route reached Tier 2. It is the diagnostic boundary a fallback
    /// cites, and it is what makes park unavailable for the four cases the
    /// proposal says must reopen instead.
    pub tier2_reason: CiTier2Reason,
    /// The **only** corpus a repair's `verification_command` may be drawn
    /// from: commands already present in repository/task context, plus
    /// commands CI evidence exposed directly. A command absent from here was
    /// invented — most often from a job name — and downgrades the repair to a
    /// diagnosis.
    pub repository_commands: Vec<String>,
    /// Evidence handles from the bundle (run id, check names, head SHAs,
    /// dequeue id). A directive that cites none of them is not
    /// evidence-grounded, whatever it asserts about itself.
    pub evidence_references: Vec<String>,
}

impl CiAdjudicationContext {
    /// Read the context out of the arbitration row's structured directive.
    ///
    /// The coordinator writes `directive.ci_route` when it dispatches Lead
    /// under a Tier-2 lease (wave 3). Absence — an old coordinator, an
    /// ordinary non-CI intervention, or the feature disabled — yields `None`
    /// and leaves the legacy arbiter path in charge.
    ///
    /// Malformed or partial blocks also yield `None`: a context that cannot
    /// name its lane, origin, or Tier-2 reason cannot validate anything, and
    /// half-applying this contract is worse than not applying it.
    pub(crate) fn from_arbiter_directive(directive: Option<&serde_json::Value>) -> Option<Self> {
        let route = directive?.get("ci_route")?;
        let str_field = |key: &str| route.get(key).and_then(serde_json::Value::as_str);
        let string_list = |key: &str| -> Vec<String> {
            route
                .get(key)
                .and_then(serde_json::Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default()
        };
        Some(Self {
            lane: CiLane::parse(str_field("lane")?).ok()?,
            origin_state: CiOriginState::parse(str_field("origin_state")?).ok()?,
            tier2_reason: CiTier2Reason::parse(str_field("tier2_reason")?).ok()?,
            repository_commands: string_list("repository_commands"),
            evidence_references: string_list("evidence_references"),
        })
    }

    /// Whether `park` is available at all for this route.
    ///
    /// The proposal is explicit: "Uncertainty, exhausted budget, provider
    /// errors, and stale or colliding artifacts reopen rather than park."
    /// Those four map exactly onto the Tier-2 reason set minus
    /// [`CiTier2Reason::CausalFailure`], so the check is a closed match rather
    /// than a heuristic over the dossier's prose.
    fn park_is_available(&self) -> bool {
        match self.tier2_reason {
            CiTier2Reason::CausalFailure => true,
            CiTier2Reason::EvidenceUnknown
            | CiTier2Reason::ProviderActionFailed
            | CiTier2Reason::OutcomeUnknown
            | CiTier2Reason::RetryExhausted => false,
        }
    }

    /// The diagnostic reason that describes *this route's* unknown boundary.
    ///
    /// Used whenever a result degrades for a reason other than a missing or
    /// invented command (which has its own, more precise reason).
    fn boundary_reason(&self) -> CiDiagnosticReason {
        match self.tier2_reason {
            CiTier2Reason::EvidenceUnknown => CiDiagnosticReason::EvidenceIncomplete,
            CiTier2Reason::ProviderActionFailed | CiTier2Reason::OutcomeUnknown => {
                CiDiagnosticReason::ProviderActionFailed
            }
            CiTier2Reason::CausalFailure | CiTier2Reason::RetryExhausted => {
                CiDiagnosticReason::NoGroundedRemedy
            }
        }
    }

    /// True when `text` cites at least one handle from the evidence bundle.
    ///
    /// A context with no handles cannot demand a citation, so it accepts any
    /// non-empty text rather than making every reopen a diagnosis.
    fn is_grounded(&self, text: &str) -> bool {
        if self.evidence_references.is_empty() {
            return !text.trim().is_empty();
        }
        self.evidence_references
            .iter()
            .any(|reference| text.contains(reference.as_str()))
    }

    /// True when `command` was copied from repository/task context or exposed
    /// directly by CI evidence.
    ///
    /// Compared on whitespace-normalized text so re-indentation does not
    /// invalidate a genuinely copied command, and *only* by equality — a
    /// command that merely shares a prefix with a known one is a new command.
    fn command_is_repository_valid(&self, command: &str) -> bool {
        let normalize = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let candidate = normalize(command);
        !candidate.is_empty()
            && self
                .repository_commands
                .iter()
                .any(|known| normalize(known) == candidate)
    }
}

// ---------------------------------------------------------------------------
// Lead's response
// ---------------------------------------------------------------------------

/// How the Lead session ended, from the supervisor's point of view.
#[derive(Clone, Copy, Debug)]
pub(crate) enum LeadResponse<'a> {
    /// The session finalized through `submit_decision` with this payload.
    Submitted(&'a serde_json::Value),
    /// The session finalized through some other tool.
    Unsupported(&'a str),
    /// The session ended without calling any finalize tool.
    Missing,
    /// The session exceeded its deadline and produced no result.
    /// Constructed by the wave-3 dispatcher's timeout path.
    #[allow(dead_code)]
    TimedOut,
}

/// Why a delivered result was replaced by the diagnostic fallback.
///
/// Recorded so the reopen's directive can name what Lead actually did, rather
/// than presenting the platform's fallback as Lead's finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CiResultRejection {
    /// `approve` / `approve_conflict` on non-passing CI evidence.
    ApprovedNonPassingCi,
    /// A reopen naming both plans, or neither.
    ReopenPlanAmbiguous,
    /// A repair whose `verification_command` is missing or was not copied
    /// from repository/task context or CI evidence.
    VerificationCommandNotRepositoryValid,
    /// A reopen whose directive is empty or cites no evidence handle.
    DirectiveNotGrounded,
    /// A `diagnostic_reason` outside the closed set.
    DiagnosticReasonUnknown,
    /// A park on a route the proposal says must reopen.
    ParkUnavailableForRoute,
    /// A park whose dossier is missing, incomplete, or cites no evidence.
    ParkNotCited,
    /// A supersede with no replacement tasks.
    SupersedeWithoutReplacements,
    /// A decision string outside the five existing results.
    UnknownDecision,
    /// The session finalized through a tool that is not `submit_decision`.
    UnsupportedFinalizeTool,
    /// The session produced no result at all.
    NoResult,
    /// The session timed out.
    TimedOut,
}

// ---------------------------------------------------------------------------
// The validated plan
// ---------------------------------------------------------------------------

/// The only four things a Lead result may mean on a CI route.
///
/// Note what has no variant: retry, provider action, approve, and any form of
/// dependency-graph mutation. They are unrepresentable, not merely rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CiLeadPlan {
    /// A grounded remedy with a repository-valid command.
    RepairReopen {
        directive: String,
        verification_command: String,
        exclude_models: Vec<String>,
    },
    /// An explicit statement of what remains unknown, with no command.
    DiagnosticReopen {
        directive: String,
        reason: CiDiagnosticReason,
    },
    /// A cited infrastructure dead-end.
    Park { dossier_json: String },
    /// Existing arbiter supersede, unwidened.
    Supersede { replacement_task_ids: Vec<String> },
}

/// A validated plan plus, when the delivered result was replaced, why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CiAdjudication {
    pub plan: CiLeadPlan,
    pub rejection: Option<CiResultRejection>,
}

impl CiAdjudication {
    fn accepted(plan: CiLeadPlan) -> Self {
        Self {
            plan,
            rejection: None,
        }
    }

    /// The single fallback: one diagnostic reopen, never a second Lead
    /// session and never a silent drop.
    fn fallback(
        rejection: CiResultRejection,
        reason: CiDiagnosticReason,
        boundary: String,
    ) -> Self {
        Self {
            plan: CiLeadPlan::DiagnosticReopen {
                directive: boundary,
                reason,
            },
            rejection: Some(rejection),
        }
    }

    /// Convenience for the common fallback whose reason is the route's own
    /// unknown boundary.
    fn route_fallback(
        ctx: &CiAdjudicationContext,
        rejection: CiResultRejection,
        boundary: impl Into<String>,
    ) -> Self {
        let reason = ctx.boundary_reason();
        Self::fallback(rejection, reason, boundary.into())
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate a Lead response against the CI adjudication contract.
///
/// Always yields a plan. An invalid, unsupported, missing, or timed-out
/// response yields a diagnostic reopen — which reaches the board only if the
/// atomic guard in [`board_effect`] still succeeds.
pub(crate) fn adjudicate<'a>(
    ctx: &CiAdjudicationContext,
    response: LeadResponse<'a>,
) -> CiAdjudication {
    let payload = match response {
        LeadResponse::Submitted(payload) => payload,
        LeadResponse::Unsupported(tool) => {
            return CiAdjudication::route_fallback(
                ctx,
                CiResultRejection::UnsupportedFinalizeTool,
                format!(
                    "Lead finalized through `{tool}` instead of `submit_decision`, so no \
                     adjudication was produced for this CI failure. Establish the cause from the \
                     CI evidence on the route before changing code."
                ),
            );
        }
        LeadResponse::Missing => {
            return CiAdjudication::route_fallback(
                ctx,
                CiResultRejection::NoResult,
                "Lead ended without submitting a decision, so no adjudication was produced for \
                 this CI failure. Establish the cause from the CI evidence on the route before \
                 changing code."
                    .to_owned(),
            );
        }
        LeadResponse::TimedOut => {
            return CiAdjudication::route_fallback(
                ctx,
                CiResultRejection::TimedOut,
                "Lead adjudication timed out, so no remedy was established for this CI failure. \
                 Establish the cause from the CI evidence on the route before changing code."
                    .to_owned(),
            );
        }
    };

    let field = |key: &str| {
        payload
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };

    match payload
        .get("decision")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
    {
        // `approve` is legal only on the passing path, where this function is
        // never reached. Reaching it here means Lead approved non-passing CI.
        "approve" | "approve_conflict" => CiAdjudication::route_fallback(
            ctx,
            CiResultRejection::ApprovedNonPassingCi,
            "Lead approved a PR whose CI is not passing, which this route cannot apply. \
             Establish from the CI evidence whether the failure is caused by this branch."
                .to_owned(),
        ),
        "reopen" => adjudicate_reopen(
            ctx,
            payload,
            field("directive"),
            field("verification_command"),
        ),
        "park" => adjudicate_park(ctx, payload),
        "supersede" => {
            let replacements = string_array(payload, "created_tasks");
            if replacements.is_empty() {
                CiAdjudication::route_fallback(
                    ctx,
                    CiResultRejection::SupersedeWithoutReplacements,
                    "Lead superseded this task without naming replacement subtasks, so no work \
                     carries the CI failure forward. Establish the cause from the CI evidence on \
                     the route before changing code."
                        .to_owned(),
                )
            } else {
                CiAdjudication::accepted(CiLeadPlan::Supersede {
                    replacement_task_ids: replacements,
                })
            }
        }
        other => CiAdjudication::route_fallback(
            ctx,
            CiResultRejection::UnknownDecision,
            format!(
                "Lead submitted the unsupported decision `{other}`, so no adjudication was \
                 produced for this CI failure. Establish the cause from the CI evidence on the \
                 route before changing code."
            ),
        ),
    }
}

/// Decide which of the two mutually exclusive reopen plans a payload carries.
fn adjudicate_reopen(
    ctx: &CiAdjudicationContext,
    payload: &serde_json::Value,
    directive: Option<&str>,
    verification_command: Option<&str>,
) -> CiAdjudication {
    let raw_reason = payload
        .get("diagnostic_reason")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // Exclusivity first: a payload claiming both plans, or neither, states no
    // plan at all. Deciding which half to believe would be the fabrication
    // the two modes exist to prevent.
    let plan_signals =
        usize::from(verification_command.is_some()) + usize::from(raw_reason.is_some());
    if plan_signals != 1 {
        return CiAdjudication::route_fallback(
            ctx,
            CiResultRejection::ReopenPlanAmbiguous,
            if plan_signals == 0 {
                "Lead reopened without a verification command and without a diagnostic reason, so \
                 neither a repair nor a diagnosis was established. Establish the cause from the \
                 CI evidence on the route before changing code."
                    .to_owned()
            } else {
                "Lead reopened with both a verification command and a diagnostic reason, which \
                 are mutually exclusive, so no plan was established. Establish the cause from the \
                 CI evidence on the route before changing code."
                    .to_owned()
            },
        );
    }

    let Some(directive) = directive else {
        return CiAdjudication::route_fallback(
            ctx,
            CiResultRejection::DirectiveNotGrounded,
            "Lead reopened without a directive, so no instruction reached the next worker. \
             Establish the cause from the CI evidence on the route before changing code."
                .to_owned(),
        );
    };

    // Grounding is required by both plans, so it is checked once, before the
    // plans diverge.
    if !ctx.is_grounded(directive) && !cites_evidence(ctx, payload) {
        return CiAdjudication::route_fallback(
            ctx,
            CiResultRejection::DirectiveNotGrounded,
            format!(
                "Lead's directive cited no evidence from this route, so its remedy is not \
                 grounded. Establish the cause from the CI evidence on the route before changing \
                 code. Ungrounded directive was: {directive}"
            ),
        );
    }

    match (verification_command, raw_reason) {
        // ── Repair ─────────────────────────────────────────────────────────
        (Some(command), None) => {
            if ctx.command_is_repository_valid(command) {
                CiAdjudication::accepted(CiLeadPlan::RepairReopen {
                    directive: directive.to_owned(),
                    verification_command: command.to_owned(),
                    exclude_models: string_array(payload, "exclude_models"),
                })
            } else {
                // The proposal's exact rule: no repository-valid command means
                // the repair is invalid and the result must be a diagnosis.
                // The directive survives — it was grounded — so the worker
                // keeps Lead's finding and loses only the invented command.
                CiAdjudication::fallback(
                    CiResultRejection::VerificationCommandNotRepositoryValid,
                    CiDiagnosticReason::NoRepositoryCommand,
                    format!(
                        "No repository-valid verification command exists for this failure; Lead \
                         proposed `{command}`, which is not present in repository/task context \
                         and was not exposed by CI evidence. Establish the correct command from \
                         the repository before verifying. Lead's finding was: {directive}"
                    ),
                )
            }
        }
        // ── Diagnose ───────────────────────────────────────────────────────
        (None, Some(reason)) => match CiDiagnosticReason::parse(reason) {
            Ok(reason) => CiAdjudication::accepted(CiLeadPlan::DiagnosticReopen {
                directive: directive.to_owned(),
                reason,
            }),
            Err(_) => CiAdjudication::route_fallback(
                ctx,
                CiResultRejection::DiagnosticReasonUnknown,
                format!(
                    "Lead cited the diagnostic reason `{reason}`, which is outside the closed \
                     set, so the diagnosis was not accepted as submitted. Lead's finding was: \
                     {directive}"
                ),
            ),
        },
        // Excluded by the `plan_signals` check above.
        _ => unreachable!("plan exclusivity is decided before the plans diverge"),
    }
}

/// Validate a park: available for this route, and cited.
fn adjudicate_park(ctx: &CiAdjudicationContext, payload: &serde_json::Value) -> CiAdjudication {
    if !ctx.park_is_available() {
        return CiAdjudication::route_fallback(
            ctx,
            CiResultRejection::ParkUnavailableForRoute,
            format!(
                "Lead parked a route whose Tier-2 reason is `{}`; uncertainty, an exhausted \
                 budget, a provider error, and a stale or colliding artifact reopen rather than \
                 park. Establish the cause from the CI evidence on the route before changing \
                 code.",
                ctx.tier2_reason.as_str()
            ),
        );
    }

    let dossier = payload.get("park_dossier");
    let text = |key: &str| {
        dossier
            .and_then(|d| d.get(key))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let (Some(hold), Some(analysis)) = (text("hold_description"), text("failure_analysis")) else {
        return CiAdjudication::route_fallback(
            ctx,
            CiResultRejection::ParkNotCited,
            "Lead parked without a dossier stating the hold and the failure analysis, so no \
             infrastructure dead-end was cited. Establish the cause from the CI evidence on the \
             route before changing code."
                .to_owned(),
        );
    };

    // "Cited" means the dossier points at this route's evidence. Prose that
    // asserts a dead-end without naming one is the over-parking the proposal
    // lists as a risk.
    if !ctx.is_grounded(analysis) && !ctx.is_grounded(hold) {
        return CiAdjudication::route_fallback(
            ctx,
            CiResultRejection::ParkNotCited,
            format!(
                "Lead's park dossier cited no evidence from this route, so no infrastructure \
                 dead-end was established. Establish the cause from the CI evidence on the route \
                 before changing code. Uncited analysis was: {analysis}"
            ),
        );
    }

    let dossier_json = dossier
        .map(|d| serde_json::to_string(d).unwrap_or_else(|_| "{}".to_owned()))
        .unwrap_or_else(|| "{}".to_owned());
    CiAdjudication::accepted(CiLeadPlan::Park { dossier_json })
}

/// True when the payload's `evidence` object names a handle from the bundle.
fn cites_evidence(ctx: &CiAdjudicationContext, payload: &serde_json::Value) -> bool {
    let Some(evidence) = payload.get("evidence") else {
        return false;
    };
    ["summary", "reference_id", "source"]
        .iter()
        .filter_map(|key| evidence.get(*key).and_then(serde_json::Value::as_str))
        .any(|text| ctx.is_grounded(text))
}

fn string_array(payload: &serde_json::Value, key: &str) -> Vec<String> {
    payload
        .get(key)
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Durable resolution
// ---------------------------------------------------------------------------

/// The row wave 1 will write, derived from the plan.
///
/// This is the value handed to `CiRouteAttemptRepository::resolve_tier2_lease`
/// — which validates it again on the way in, and which evaluates the
/// identity guard in the same transaction that persists it.
#[allow(dead_code)]
pub(crate) fn durable_resolution(plan: &CiLeadPlan) -> CiTier2Resolution {
    match plan {
        CiLeadPlan::RepairReopen { .. } => CiTier2Resolution::repair(),
        CiLeadPlan::DiagnosticReopen { reason, .. } => CiTier2Resolution::diagnose(*reason),
        CiLeadPlan::Park { dossier_json } => CiTier2Resolution::park(dossier_json.clone()),
        CiLeadPlan::Supersede { .. } => CiTier2Resolution::plain(CiRouteOutcome::Superseded),
    }
}

/// Which reopen mode a plan is, for the durable record. `None` for the two
/// plans that are not reopens.
#[allow(dead_code)]
pub(crate) fn reopen_mode(plan: &CiLeadPlan) -> Option<CiReopenMode> {
    match plan {
        CiLeadPlan::RepairReopen { .. } => Some(CiReopenMode::Repair),
        CiLeadPlan::DiagnosticReopen { .. } => Some(CiReopenMode::Diagnose),
        CiLeadPlan::Park { .. } | CiLeadPlan::Supersede { .. } => None,
    }
}

// ---------------------------------------------------------------------------
// Application
// ---------------------------------------------------------------------------
//
// # Why this half carries `#[allow(dead_code)]`
//
// Validation (above) is live: `stage::lead_stage_outcome_routed` calls it on
// every Lead result. Application is not, and cannot be, until wave 3 owns the
// Tier-2 lease and wave 5 turns the route on — the guard call needs the route
// row's provider-action key and lease id, which the stage boundary does not
// have and must not invent.
//
// The alternative was to land these types with wave 3 or 5. The proposal
// forbids that: W4's whole landing rule is that "an older validator must never
// receive a new reopen payload", so the validator and the shape of what it
// authorizes have to arrive together, in one merge group, ahead of the code
// that calls them. Every item below is exercised by this module's tests.

/// What `CiRouteAttemptRepository::resolve_tier2_lease` answered.
///
/// The repository returns a bare `bool`; naming it here is what stops a call
/// site from reading it as "did the write happen" rather than "is the
/// evidence still current".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum CiGuardOutcome {
    /// Head, lane, dequeue identity all unchanged and no newer passing or
    /// merged observation. The resolution was persisted.
    Current,
    /// The compare-and-set lost. The route was closed
    /// `superseded_before_apply` in the same transaction.
    SupersededBeforeApply,
}

#[allow(dead_code)]
impl CiGuardOutcome {
    /// Interpret `resolve_tier2_lease`'s return value.
    pub(crate) fn from_resolve(applied: bool) -> Self {
        if applied {
            Self::Current
        } else {
            Self::SupersededBeforeApply
        }
    }
}

/// Everything the supervisor is permitted to do with an adjudication.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum CiBoardEffect {
    /// Apply `PrCiFailed` from the recorded origin to `open`, log the plan,
    /// and dispatch exactly one worker.
    Reopen {
        origin_state: CiOriginState,
        mode: CiReopenMode,
        directive: String,
        /// Present only for a repair. A diagnostic reopen carries no command
        /// by construction, which is why this is an `Option` and not an
        /// empty string that a downstream renderer would print.
        verification_command: Option<String>,
        exclude_models: Vec<String>,
    },
    /// Park with the cited dossier.
    Park { dossier_json: String },
    /// Force-close as superseded by the replacements.
    Supersede { replacement_task_ids: Vec<String> },
    /// The guard failed. Nothing happens — this is the whole point.
    None,
}

/// Countable side effects, so "exactly one worker" and "a no-op" are
/// assertions about behaviour rather than about a variant's name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct CiEffectCounts {
    pub board_transitions: usize,
    pub worker_dispatches: usize,
    /// Mutations of the *current* route — the one that superseded this
    /// obsolete attempt. Always zero: an obsolete route may never touch it.
    pub current_route_mutations: usize,
    /// Further Lead sessions for the same evidence. Always zero.
    pub lead_sessions: usize,
    /// Dependency-graph edges. Always zero — CI routing adds none.
    pub dependency_edges: usize,
    /// Provider calls. Always zero — there is no Lead-to-provider edge.
    pub provider_actions: usize,
}

#[allow(dead_code)]
impl CiBoardEffect {
    pub(crate) fn counts(&self) -> CiEffectCounts {
        let (board_transitions, worker_dispatches) = match self {
            // A reopen is one transition and exactly one worker.
            Self::Reopen { .. } => (1, 1),
            // Park and supersede transition the board but dispatch no worker
            // for this task.
            Self::Park { .. } | Self::Supersede { .. } => (1, 0),
            Self::None => (0, 0),
        };
        CiEffectCounts {
            board_transitions,
            worker_dispatches,
            ..CiEffectCounts::default()
        }
    }

    /// True when nothing at all happens.
    pub(crate) fn is_noop(&self) -> bool {
        matches!(self, Self::None)
    }
}

/// Combine a validated plan with the guard's answer.
///
/// Every plan — repair, diagnose, park, supersede, and each of the fallbacks
/// — collapses to [`CiBoardEffect::None`] when the guard failed. There is no
/// plan that survives a stale identity.
#[allow(dead_code)]
pub(crate) fn board_effect(
    ctx: &CiAdjudicationContext,
    plan: &CiLeadPlan,
    guard: CiGuardOutcome,
) -> CiBoardEffect {
    if guard == CiGuardOutcome::SupersededBeforeApply {
        return CiBoardEffect::None;
    }
    match plan {
        CiLeadPlan::RepairReopen {
            directive,
            verification_command,
            exclude_models,
        } => CiBoardEffect::Reopen {
            origin_state: ctx.origin_state,
            mode: CiReopenMode::Repair,
            directive: directive.clone(),
            verification_command: Some(verification_command.clone()),
            exclude_models: exclude_models.clone(),
        },
        CiLeadPlan::DiagnosticReopen { directive, .. } => CiBoardEffect::Reopen {
            origin_state: ctx.origin_state,
            mode: CiReopenMode::Diagnose,
            directive: directive.clone(),
            verification_command: None,
            exclude_models: Vec::new(),
        },
        CiLeadPlan::Park { dossier_json } => CiBoardEffect::Park {
            dossier_json: dossier_json.clone(),
        },
        CiLeadPlan::Supersede {
            replacement_task_ids,
        } => CiBoardEffect::Supersede {
            replacement_task_ids: replacement_task_ids.clone(),
        },
    }
}

// ---------------------------------------------------------------------------
// Stage-outcome projection
// ---------------------------------------------------------------------------

/// Project a validated plan onto the existing [`StageOutcome`] surface.
///
/// No new variant is introduced: the proposal forbids a new top-level Lead
/// result, and the supervisor's existing reopen/park/supersede handling is
/// what applies it. A diagnostic reopen carries an empty verification
/// command, which the arbitration row stores and the worker prompt already
/// skips when blank.
pub(crate) fn stage_outcome(plan: &CiLeadPlan) -> StageOutcome {
    match plan {
        CiLeadPlan::RepairReopen {
            directive,
            verification_command,
            exclude_models,
        } => StageOutcome::LeadReopen {
            reason: "lead reopened task with a grounded CI repair plan".to_owned(),
            directive: directive.clone(),
            verification_command: verification_command.clone(),
            exclude_models: exclude_models.clone(),
        },
        CiLeadPlan::DiagnosticReopen { directive, reason } => StageOutcome::LeadReopen {
            reason: format!(
                "lead reopened task with a CI diagnostic plan ({})",
                reason.as_str()
            ),
            directive: directive.clone(),
            verification_command: String::new(),
            exclude_models: Vec::new(),
        },
        CiLeadPlan::Park { dossier_json } => StageOutcome::LeadParked {
            park_dossier_json: dossier_json.clone(),
        },
        CiLeadPlan::Supersede {
            replacement_task_ids,
        } => StageOutcome::LeadSuperseded {
            reason: "arbiter superseded task with replacement subtasks".to_owned(),
            replacement_task_ids: replacement_task_ids.clone(),
        },
    }
}

#[cfg(test)]
mod tests;
