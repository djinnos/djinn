//! Role-scoped rendering of typed tribunal evidence into a prompt block.
//!
//! Until now the only evidence projection reaching any prompt was
//! `build_advocate_evidence_findings_context` in `djinn-coordinator`:
//! Advocate-only, sourced from a legacy debate row, and ending in a raw
//! `body_metadata` JSON dump. An Adversary raising a demand and a Judge folding
//! one both worked from prose.
//!
//! This module replaces it with a **pure** renderer over a **closed** record.
//!
//! ## Why the shape is closed
//!
//! [`TypedEvidenceRoleContextV1`] has a fixed field set, pinned by
//! [`TYPED_EVIDENCE_ROLE_CONTEXT_V1_FIELDS`] and asserted in the tests below.
//! None of its fields is a free-text question collection, so a prose or
//! QuestionForm inventory cannot be reintroduced through this path without
//! failing that assertion. That is the checkable form of the exclusion: rather
//! than trying to prove no inventory exists anywhere — a universal negative no
//! test run can establish — it makes the only channel into the prompt
//! structurally incapable of carrying one.
//!
//! ## Why the renderer is pure
//!
//! It takes a value and returns a `String`. It reads no database, no clock, and
//! no environment, so a snapshot of its output is a snapshot of the contract
//! rather than of a fixture's incidental state.

use crate::AgentType;
use serde::{Deserialize, Serialize};

/// The exact serialized field set of [`TypedEvidenceRoleContextV1`].
///
/// Adding a field without extending this list fails
/// `typed_evidence_context_shape_is_closed`. Keep it sorted.
pub const TYPED_EVIDENCE_ROLE_CONTEXT_V1_FIELDS: [&str; 12] = [
    "attempts",
    "claim",
    "demand",
    "demanded_revision_seq",
    "disposition",
    "evidence_outcome",
    "finding_id",
    "gaps",
    "lifecycle",
    "planned_checks",
    "retry",
    "version",
];

/// Section markers. Role scoping is asserted by the presence or absence of
/// these exact strings, so they are constants rather than inline literals.
pub const DEMAND_SECTION: &str = "## Demand thresholds";
pub const RETRY_SECTION: &str = "## Retry and conflict";
pub const DISPOSITION_SECTION: &str = "## Disposition";

/// Lifecycle of the finding, mirroring `typed_evidence_findings.lifecycle`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypedEvidenceContextLifecycle {
    Demanded,
    SpikeActive,
    EvidenceReceived,
    Failed,
    Resolved,
    Withdrawn,
}

impl TypedEvidenceContextLifecycle {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Demanded => "demanded",
            Self::SpikeActive => "spike_active",
            Self::EvidenceReceived => "evidence_received",
            Self::Failed => "failed",
            Self::Resolved => "resolved",
            Self::Withdrawn => "withdrawn",
        }
    }
}

/// Server-derived health of one immutable evidence anchor.
///
/// The health is a server fact, not an agent claim: it is what the ingress
/// validator concluded after dereferencing the anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypedEvidenceContextAnchorHealth {
    Healthy,
    Unusable,
    Unavailable,
}

impl TypedEvidenceContextAnchorHealth {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Unusable => "unusable",
            Self::Unavailable => "unavailable",
        }
    }
}

/// One planned check and, once a return has landed, how it resolved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedEvidencePlannedCheckContext {
    pub check_id: String,
    /// `code`, `graph`, or `command`.
    pub method: String,
    /// `passed`, `failed`, `not_run`, or absent before the return.
    pub status: Option<String>,
    /// Immutable provenance for the observation, e.g. `command:<invocation>`.
    pub anchor_locator: Option<String>,
    pub anchor_health: Option<TypedEvidenceContextAnchorHealth>,
}

/// One spike attempt against the finding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedEvidenceAttemptContext {
    pub sequence: i32,
    pub spike_task_id: String,
    /// `resolved`, `partial`, `unresolved`, or absent while in flight.
    pub outcome: Option<String>,
    /// Persisted failure text for this attempt.
    pub failure_detail: Option<String>,
}

/// Demand-threshold guidance. Adversary and Judge only: it is the standard the
/// demand had to clear, which the Advocate is not the audience for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedEvidenceDemandContext {
    /// `feasibility`, `safety`, `compatibility`, `rollout`, or a control
    /// category for a question that is not load-bearing.
    pub category: String,
    /// Whether the question is load-bearing enough to justify a spike.
    pub load_bearing: bool,
    /// The threshold sentence the demand had to clear.
    pub threshold: String,
    /// Concrete checks a spike is expected to run. Empty for a control.
    pub expected_checks: Vec<String>,
}

/// Retry and conflict detail. Advocate and Judge only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedEvidenceRetryContext {
    pub attempts_used: i32,
    pub attempts_allowed: i32,
    /// Reason the previous attempt failed, when one did.
    pub last_failure: Option<String>,
    /// Set when a competing demand held the finding's only retry slot.
    pub conflict_code: Option<String>,
}

/// Terminal Judge decision. Judge only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedEvidenceDispositionContext {
    /// `resolved` or `withdrawn`.
    pub disposition: String,
    /// Committed spec revision the evidence was folded into.
    pub folding_revision: i32,
    pub rationale: String,
    /// A withdrawal must assert the uncertainty was not load-bearing.
    pub withdrawal_is_non_load_bearing: bool,
}

/// Everything any tribunal role may be told about one typed evidence finding.
///
/// The field set is closed on purpose — see the module docs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedEvidenceRoleContextV1 {
    /// Schema marker; always `TypedEvidenceRoleContextV1`.
    pub version: String,
    pub finding_id: String,
    pub lifecycle: TypedEvidenceContextLifecycle,
    /// The claim under demand. A single question, never a collection.
    pub claim: String,
    /// Revision the demand was raised against. Provenance, not a filter.
    pub demanded_revision_seq: i32,
    pub planned_checks: Vec<TypedEvidencePlannedCheckContext>,
    pub attempts: Vec<TypedEvidenceAttemptContext>,
    /// Validated outcome of the latest return.
    pub evidence_outcome: Option<String>,
    /// Normalized failures and gaps from the latest return.
    pub gaps: Vec<String>,
    pub demand: Option<TypedEvidenceDemandContext>,
    pub retry: Option<TypedEvidenceRetryContext>,
    pub disposition: Option<TypedEvidenceDispositionContext>,
}

impl TypedEvidenceRoleContextV1 {
    pub const VERSION: &'static str = "TypedEvidenceRoleContextV1";

    /// A minimal context for a freshly raised demand.
    pub fn new(
        finding_id: impl Into<String>,
        lifecycle: TypedEvidenceContextLifecycle,
        claim: impl Into<String>,
        demanded_revision_seq: i32,
    ) -> Self {
        Self {
            version: Self::VERSION.to_owned(),
            finding_id: finding_id.into(),
            lifecycle,
            claim: claim.into(),
            demanded_revision_seq,
            planned_checks: Vec::new(),
            attempts: Vec::new(),
            evidence_outcome: None,
            gaps: Vec::new(),
            demand: None,
            retry: None,
            disposition: None,
        }
    }
}

/// Whether a role is shown the demand-threshold surface.
///
/// The Adversary raises demands and the Judge adjudicates them. The Advocate
/// answers the spec, and showing it the threshold invites writing to the test.
pub const fn shows_demand(role: AgentType) -> bool {
    matches!(role, AgentType::Adversary | AgentType::Judge)
}

/// Whether a role is shown the retry and conflict surface.
///
/// The Advocate needs to know an attempt is being re-run before revising, and
/// the Judge needs it to adjudicate. The Adversary's objection does not change
/// with the retry budget.
pub const fn shows_retry(role: AgentType) -> bool {
    matches!(role, AgentType::Advocate | AgentType::Judge)
}

/// Whether a role is shown the terminal disposition.
///
/// Only the Judge issues one, and only the Judge may act on one.
pub const fn shows_disposition(role: AgentType) -> bool {
    matches!(role, AgentType::Judge)
}

/// Render the typed evidence block for one role.
///
/// Pure: no database, no clock, no environment. The shared sections come first
/// and are byte-identical across roles, so the committed per-role snapshots
/// differ only in the role-scoped sections.
pub fn render_for_role(role: AgentType, context: &TypedEvidenceRoleContextV1) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str("# Typed evidence\n\n");
    out.push_str(&format!(
        "Finding `{}` is `{}`, demanded against revision {}.\n",
        context.finding_id,
        context.lifecycle.as_str(),
        context.demanded_revision_seq,
    ));
    out.push_str(&format!("Claim under demand: {}\n", context.claim));

    if let Some(outcome) = context.evidence_outcome.as_deref() {
        out.push_str(&format!("Validated evidence outcome: {outcome}\n"));
    }

    if !context.planned_checks.is_empty() {
        out.push_str("\n## Planned checks\n\n");
        for check in &context.planned_checks {
            out.push_str(&format!("- `{}` ({})", check.check_id, check.method));
            if let Some(status) = check.status.as_deref() {
                out.push_str(&format!(" — {status}"));
            }
            if let Some(locator) = check.anchor_locator.as_deref() {
                out.push_str(&format!("; anchor `{locator}`"));
            }
            if let Some(health) = check.anchor_health {
                out.push_str(&format!(" (server-derived health: {})", health.as_str()));
            }
            out.push('\n');
        }
    }

    if !context.attempts.is_empty() {
        out.push_str("\n## Attempts\n\n");
        for attempt in &context.attempts {
            out.push_str(&format!(
                "- attempt {} on spike `{}`",
                attempt.sequence, attempt.spike_task_id
            ));
            if let Some(outcome) = attempt.outcome.as_deref() {
                out.push_str(&format!(" — {outcome}"));
            }
            if let Some(detail) = attempt.failure_detail.as_deref() {
                out.push_str(&format!("; {detail}"));
            }
            out.push('\n');
        }
    }

    if !context.gaps.is_empty() {
        out.push_str("\n## Failures and gaps\n\n");
        for gap in &context.gaps {
            out.push_str(&format!("- {gap}\n"));
        }
    }

    if shows_demand(role)
        && let Some(demand) = context.demand.as_ref()
    {
        out.push_str(&format!("\n{DEMAND_SECTION}\n\n"));
        out.push_str(&format!("Category: {}\n", demand.category));
        out.push_str(&format!(
            "Load-bearing: {}\n",
            if demand.load_bearing { "yes" } else { "no" }
        ));
        out.push_str(&format!("Threshold: {}\n", demand.threshold));
        if demand.expected_checks.is_empty() {
            out.push_str(
                "Expected checks: none — this remains an ordinary objection rather than \
                 an evidence demand.\n",
            );
        } else {
            out.push_str("Expected checks:\n");
            for check in &demand.expected_checks {
                out.push_str(&format!("- {check}\n"));
            }
        }
    }

    if shows_retry(role)
        && let Some(retry) = context.retry.as_ref()
    {
        out.push_str(&format!("\n{RETRY_SECTION}\n\n"));
        out.push_str(&format!(
            "Attempts used: {} of {}\n",
            retry.attempts_used, retry.attempts_allowed
        ));
        if let Some(failure) = retry.last_failure.as_deref() {
            out.push_str(&format!("Last failure: {failure}\n"));
        }
        if let Some(conflict) = retry.conflict_code.as_deref() {
            out.push_str(&format!("Conflict: {conflict}\n"));
        }
    }

    if shows_disposition(role)
        && let Some(disposition) = context.disposition.as_ref()
    {
        out.push_str(&format!("\n{DISPOSITION_SECTION}\n\n"));
        out.push_str(&format!("Decision: {}\n", disposition.disposition));
        out.push_str(&format!(
            "Folded into revision: {}\n",
            disposition.folding_revision
        ));
        out.push_str(&format!("Rationale: {}\n", disposition.rationale));
        out.push_str(&format!(
            "Withdrawal asserted non-load-bearing: {}\n",
            if disposition.withdrawal_is_non_load_bearing {
                "yes"
            } else {
                "no"
            }
        ));
    }

    out
}
