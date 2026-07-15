use serde::{Deserialize, Serialize};

/// A structured needs-evidence claim that the Judge identifies as requiring
/// a read-only investigation spike. Persisted as JSON in the
/// `proposals.needs_evidence_claim` column alongside the linked spike task id.
///
/// Fields:
/// - `question`: the feasibility question the spike must answer.
/// - `target_subsystem`: the subsystem or module under investigation.
/// - `spec_unknown_anchor`: what in the spec is unknown/unverified.
/// - `insufficient_in_session_research`: why in-session research was not enough.
/// - `expected_findings`: what the spike should produce to resolve the claim.
/// - `round`: the debate round when the demand was issued.
/// - `against_revision_seq`: the proposal revision the demand targets.
/// - `created_by_task_id`: the Judge task id that issued the demand.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeedsEvidenceClaim {
    pub question: String,
    pub target_subsystem: String,
    pub spec_unknown_anchor: String,
    pub insufficient_in_session_research: String,
    pub expected_findings: String,
    pub round: i32,
    pub against_revision_seq: i32,
    pub created_by_task_id: String,
}

impl NeedsEvidenceClaim {
    /// Attempt to parse a stored `needs_evidence_claim` JSON string back into
    /// the typed struct. Returns `None` when the input is `None` or empty, and
    /// `Err` when the JSON is present but does not match the expected shape.
    pub fn parse_stored(json: Option<&str>) -> Result<Option<Self>, String> {
        match json {
            None | Some("") => Ok(None),
            Some(s) => serde_json::from_str(s)
                .map(Some)
                .map_err(|e| format!("invalid NeedsEvidenceClaim JSON: {e}")),
        }
    }
}

/// A global, project-independent proposal: the collaborative "why/what/scope"
/// artifact that precedes (and is decoupled from) the project-scoped epic/task
/// execution engine. Has NO `project_id` — it targets projects via
/// [`ProposalTarget`], and that target set is editable over time.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct Proposal {
    pub id: String,
    /// Globally unique short id (NOT per-project, unlike epics).
    pub short_id: String,
    pub title: String,
    /// Spec body (markdown or MDX depending on `body_format`).
    pub body: String,
    /// Body encoding: `markdown` (legacy default) or `mdx` (block-aware).
    pub body_format: String,
    /// JSON array of acceptance-criteria strings (stored as JSONB, surfaced as
    /// text — parse with [`crate::models::parse_json_array`]).
    pub acceptance_criteria: String,
    /// Lifecycle: `triage` | `draft` | `in_review` | `approved` | `building` |
    /// `done` | `rejected` | `archived` | `superseded`.
    pub status: String,
    /// Real user FK of the author (NULL for system/agent-authored proposals).
    pub author_user_id: Option<String>,
    /// When superseded, the id of the proposal that replaces this one.
    pub superseded_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    /// Head revision number; sign-offs anchored to an earlier seq are stale.
    pub latest_revision_seq: i32,
    /// Last proposal revision that the in-flight build has reconciled against.
    /// `None` means no build reconciliation has been stamped for this proposal.
    pub last_reconciled_revision_seq: Option<i32>,
    /// True when the latest proposal revision is newer than the revision the
    /// in-flight build has reconciled against.
    pub pending_reconcile: bool,
    /// Participant accountable for the build once graduated (also the epic
    /// creator, so commits attribute correctly).
    pub build_owner_user_id: Option<String>,
    /// When `true`, the build is frozen: the proposal stays `building` but its
    /// graduated epics' tasks are held out of dispatch. Cleared to resume.
    pub build_frozen: bool,
    /// The `epic_breakdown` task created at graduation (1:1 with a build
    /// generation). Set on graduate, cleared on stop. Lets a stop find and
    /// force-close the breakdown task even before it has produced epics.
    pub build_breakdown_task_id: Option<String>,
    /// Durable owner of the current/most recent refinement run.
    pub refinement_owner_user_id: Option<String>,
    /// When the Judge issues a `needs-evidence` verdict, this is the FK to the
    /// spike task that must close before refinement can resume. `None` when not
    /// parked. ON DELETE SET NULL keeps the proposal safe if the task is
    /// hard-deleted.
    pub linked_spike_task_id: Option<String>,
    /// The named load-bearing feasibility claim that the Judge identified as
    /// needing research. Set alongside `linked_spike_task_id`; cleared when
    /// the spike closes and refinement resumes.
    pub needs_evidence_claim: Option<String>,
}

/// An immutable proposal-history row. Spec revisions are appended on every
/// material edit; status/audit events may also be appended at the current spec
/// sequence without advancing `proposals.latest_revision_seq`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct ProposalRevision {
    pub id: String,
    pub proposal_id: String,
    pub seq: i32,
    pub title: String,
    pub body: String,
    /// Body encoding: `markdown` (legacy default) or `mdx` (block-aware).
    pub body_format: String,
    /// JSON array text (parse with [`crate::models::parse_json_array`] or the
    /// structured AC parser).
    pub acceptance_criteria: String,
    pub edited_by_user_id: Option<String>,
    /// `spec_revision` for material proposal snapshots, or an audit event kind
    /// such as `status_change` for non-spec lifecycle history.
    pub event_kind: String,
    pub status_from: Option<String>,
    pub status_to: Option<String>,
    /// Optional JSON metadata for non-spec history rows, serialized as text.
    pub event_metadata: Option<String>,
    pub created_at: String,
}

/// A review sign-off on a proposal. `kind` is `scoped` (product) or
/// `technical` (engineering). `revision_seq` is the head revision it was given
/// against — when the proposal advances past it, the sign-off is stale.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct ProposalSignoff {
    pub proposal_id: String,
    pub kind: String,
    pub user_id: String,
    pub revision_seq: i32,
    pub created_at: String,
}

/// A project this proposal targets. `role` is `primary` (a write-target) or
/// `reference` (read-only context). Editable — this M:N link is the re-target
/// capability that lets a proposal move between projects without losing
/// identity.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct ProposalTarget {
    pub proposal_id: String,
    pub project_id: String,
    pub role: String,
    pub created_at: String,
}

/// A single feedback entry on a proposal — plain discussion that cannot be
/// applied to the spec directly. Changes flow through djinn in chat, which
/// rewrites the spec via `proposal_update` (appending a revision) and marks the
/// feedback resolved. `author_kind` distinguishes human (`user`) from AI (`ai`,
/// with `author_model` set).
///
/// Resolution:
///   `resolved_at == None`                        → unresolved (shown; counts)
///   `resolved_at` + `resolved_revision_seq`      → addressed in that revision
///   `resolved_at` + `resolved_revision_seq` None → dismissed (no spec change)
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct ProposalFeedback {
    pub id: String,
    pub proposal_id: String,
    /// Parent feedback id for threaded replies (`None` for a top-level entry).
    pub parent_id: Option<String>,
    /// `user` or `ai`.
    pub author_kind: String,
    pub author_user_id: Option<String>,
    /// Model id when `author_kind == "ai"`.
    pub author_model: Option<String>,
    pub body: String,
    /// When set, the feedback has been resolved (addressed or dismissed) and is
    /// collapsed out of the active thread. `None` while unresolved.
    pub resolved_at: Option<String>,
    /// The proposal revision that addressed this feedback. `None` when the
    /// feedback was dismissed without a spec change.
    pub resolved_revision_seq: Option<i32>,
    /// User who resolved it (`None` for system/agent resolution).
    pub resolved_by_user_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// A structured debate-trail entry for the proposal tribunal. Distinct from
/// [`ProposalFeedback`] (human discussion): debate rows are typed (objection,
/// rebuttal, verdict), track blocking state, and carry resolution/reopen
/// lifecycle so the adversarial refinement workflow can stand up, resolve, and
/// re-open individual positions.
///
/// Resolution state machine:
///   `resolved_at == None`                                  → open
///   `resolved_at` set, `reopened_at == None`               → resolved
///   `resolved_at` set, `reopened_at` set                   → reopened (effectively open)
///   Re-resolving clears `reopened_at`/`reopened_by_user_id`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct ProposalDebateTrail {
    pub id: String,
    pub proposal_id: String,
    /// `objection` | `rebuttal` | `verdict` | `needs_evidence` | `evidence_findings`.
    pub kind: String,
    pub body: String,
    /// When true, this entry blocks proposal readiness.
    pub blocking: bool,
    /// Agent role (e.g. "advocate", "adversary", "judge", "spike").
    pub agent_role: String,
    /// `agent` or `user`.
    pub author_kind: String,
    pub author_user_id: Option<String>,
    /// Model id when `author_kind == "agent"`.
    pub author_model: Option<String>,
    /// Optional source task that produced this entry.
    pub source_task_id: Option<String>,
    /// The proposal revision this entry was written against.
    pub against_revision_seq: i32,
    /// Debate round (1-based).
    pub round: i32,
    /// Optional structured JSONB metadata. Required for `needs_evidence`
    /// (linkage payload) and `evidence_findings` (structured findings);
    /// `None` or ignored for `objection`/`rebuttal`/`verdict`.
    pub body_metadata: Option<String>,
    /// When set, the entry has been resolved. `None` while open.
    pub resolved_at: Option<String>,
    /// User who resolved it (`None` for system/agent resolution).
    pub resolved_by_user_id: Option<String>,
    /// When set alongside `resolved_at`, the entry was reopened.
    pub reopened_at: Option<String>,
    /// User who reopened it.
    pub reopened_by_user_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Structured payload for an `evidence_findings` debate-trail entry written
/// by the spike (read-only investigation) when it closes. The schema is
/// fixed so the advocate/coordinator can rely on every field being present
/// on a well-formed findings entry.
///
/// Fields:
/// - `answer`: a one-paragraph answer to the Judge's feasibility question.
/// - `evidence`: list of concrete findings (commands run, file paths,
///   measurements, links) backing the answer.
/// - `code_paths_inspected`: explicit list of repo-relative code paths the
///   spike actually read or scanned while gathering evidence.
/// - `confidence`: self-rated confidence in the answer, in `[0.0, 1.0]`.
/// - `residual_risks`: known unresolved risks the advocate should weigh
///   before treating the spike as definitive.
/// - `recommendation_for_advocate`: short suggestion for how the advocate
///   should incorporate or caveat the findings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvidenceFindings {
    pub answer: String,
    pub evidence: Vec<String>,
    pub code_paths_inspected: Vec<String>,
    /// Self-rated confidence in `[0.0, 1.0]`. Constructed via
    /// [`EvidenceFindings::validate`] which clamps out-of-range values and
    /// returns a typed error.
    pub confidence: f64,
    pub residual_risks: Vec<String>,
    pub recommendation_for_advocate: String,
}

impl EvidenceFindings {
    /// Validate that every required field is well-formed. Returns
    /// `Err(String)` describing the first failure so callers can surface a
    /// clear message instead of a serde panic. Specifically:
    /// - `answer`, `recommendation_for_advocate` must be non-empty after
    ///   trimming.
    /// - `confidence` must be a finite number in `[0.0, 1.0]`.
    ///
    /// Lists (`evidence`, `code_paths_inspected`, `residual_risks`) are
    /// allowed to be empty (the spike may have found nothing to cite for
    /// some of them); serde still enforces the array shape.
    pub fn validate(&self) -> Result<(), String> {
        if self.answer.trim().is_empty() {
            return Err("evidence_findings.answer must be non-empty".to_owned());
        }
        if self.recommendation_for_advocate.trim().is_empty() {
            return Err(
                "evidence_findings.recommendation_for_advocate must be non-empty".to_owned(),
            );
        }
        if !self.confidence.is_finite() || !(0.0..=1.0).contains(&self.confidence) {
            return Err(format!(
                "evidence_findings.confidence must be finite and in [0.0, 1.0], got {}",
                self.confidence
            ));
        }
        Ok(())
    }

    /// Parse a stored JSON string (e.g. from a debate-trail `body_metadata`
    /// blob) back into the typed struct, with the same `None`/`Err`
    /// convention as [`NeedsEvidenceClaim::parse_stored`].
    pub fn parse_stored(json: Option<&str>) -> Result<Option<Self>, String> {
        match json {
            None | Some("") => Ok(None),
            Some(s) => serde_json::from_str(s)
                .map(Some)
                .map_err(|e| format!("invalid EvidenceFindings JSON: {e}")),
        }
    }
}
