// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
use std::collections::{HashMap, HashSet};

use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::{
    EvidenceFindings, NeedsEvidenceClaim, Proposal, ProposalDebateTrail, ProposalFeedback,
    ProposalRevision, ProposalSignoff, ProposalTarget,
};

use crate::database::Database;
use crate::repositories::epic::EpicRepository;
use crate::repositories::note::NoteRepository;
use crate::repositories::note::{LexicalSearchBackend, sanitize_postgres_tsquery};
use crate::{Error, Result};

use djinn_memory::ProposalSearchResult;
use sqlx::{Postgres, Row, Transaction};

// Global proposals layer (Phase 0). A `proposal` is project-independent; it
// targets projects via `proposal_targets` (editable M:N) and carries unified
// discussion+suggestion `proposal_feedback`. This repository mirrors
// `epic.rs` conventions: `query_as!` with inlined SELECT projections, `$N`
// params, and an event emitted after every mutation.

// ── Query / result types ─────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum SqlParam {
    Text(String),
}

/// Memory note reached through a proposal's graduated epics and their tasks.
///
/// This is a read-time projection, not a database model: the permalink/source
/// are read from `epics.memory_refs` / `tasks.memory_refs`, while `title` and
/// `note_type` are resolved from `notes`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProposalMemoryRef {
    pub permalink: String,
    pub title: String,
    pub note_type: String,
    pub source_entity_type: String,
    pub source_short_id: String,
}

/// Filters and pagination for [`ProposalRepository::list_filtered`].
pub struct ProposalListQuery {
    pub status: Option<String>,
    pub text: Option<String>,
    pub author_user_id: Option<String>,
    /// Restrict to proposals that target this project (UUID).
    pub target_project_id: Option<String>,
    pub sort: String,
    pub limit: i64,
    pub offset: i64,
}

impl Default for ProposalListQuery {
    fn default() -> Self {
        Self {
            status: None,
            text: None,
            author_user_id: None,
            target_project_id: None,
            sort: "created_desc".to_owned(),
            limit: 25,
            offset: 0,
        }
    }
}

pub struct ProposalListResult {
    /// Each proposal paired with its unresolved-feedback count (drives the
    /// per-row badge in the proposals list).
    pub proposals: Vec<(Proposal, i64)>,
    pub total_count: i64,
}

/// List-only row: the `Proposal` columns plus the correlated unresolved-feedback
/// count. Kept separate from `Proposal` (which maps 1:1 to columns via the
/// `query_as!` macro paths) so the list's extra aggregate doesn't leak into
/// every get/resolve projection.
#[derive(sqlx::FromRow)]
struct ProposalListRow {
    #[sqlx(flatten)]
    proposal: Proposal,
    unresolved_feedback_count: i64,
}

/// Batched tribunal/readiness raw facts for one proposal on the list path.
///
/// Populated by [`ProposalRepository::list_summaries`], which runs one grouped
/// (or window) query per fact across the whole listed page — so rendering the
/// list's tribunal/gate chips costs a handful of queries total instead of the
/// several-per-row that `build_gate_status` / `build_refinement_status` issue.
/// Callers derive the deterministic DoR and composed-gate booleans upstream
/// (they live in the control-plane readiness evaluator).
#[derive(Debug, Default, Clone)]
pub struct ProposalListSummaryRow {
    /// Unresolved blocking, non-verdict debate objections outstanding.
    pub unresolved_blocking_count: i64,
    /// Body of the latest judge verdict, if any (for the needs-work heuristic
    /// applied upstream — kept out of the DB layer so the heuristic stays
    /// single-sourced with `build_gate_status`).
    pub latest_judge_verdict_body: Option<String>,
    /// Highest debate round reached (`0` when there is no debate trail yet).
    pub current_round: i32,
    /// A refinement (tribunal) run is active — started with no later stop.
    pub refinement_active: bool,
    /// Refinement converged and is parked awaiting human review.
    pub awaiting_review: bool,
    /// Number of target projects attached (feeds the DoR target-count check).
    pub target_count: i64,
}

pub struct ProposalCreateInput<'a> {
    pub title: &'a str,
    pub body: &'a str,
    /// JSON array string of acceptance-criteria; `None` defaults to `[]`.
    pub acceptance_criteria: Option<&'a str>,
    /// Initial status; `None` defaults to `draft`.
    pub status: Option<&'a str>,
    /// Body encoding: `markdown` (default) or `mdx` (block-aware).
    pub body_format: Option<&'a str>,
}

pub struct ProposalUpdateInput<'a> {
    pub title: &'a str,
    pub body: &'a str,
    /// JSON array string of acceptance-criteria.
    pub acceptance_criteria: &'a str,
    pub status: &'a str,
    pub superseded_by: Option<&'a str>,
    /// Body encoding: `markdown` (default) or `mdx`.
    pub body_format: Option<&'a str>,
    /// Optional structured metadata persisted to `proposal_revisions.event_metadata`
    /// when the update triggers a material spec revision. When `None`, the
    /// revision row's `event_metadata` stays `NULL` (preserves the pre-existing
    /// behavior for ordinary `proposal_update` callers). Used by the planner
    /// refinement loop to attribute authoring revisions to the active native-skill
    /// version and to record targeted block-patch context (selector, range, etc.).
    pub event_metadata: Option<&'a serde_json::Value>,
}

pub struct ProposalFeedbackCreateInput<'a> {
    pub proposal_id: &'a str,
    pub parent_id: Option<&'a str>,
    /// `user` (default) or `ai`.
    pub author_kind: &'a str,
    pub author_model: Option<&'a str>,
    pub body: &'a str,
}

pub struct ProposalDebateTrailCreateInput<'a> {
    pub proposal_id: &'a str,
    /// `objection` | `rebuttal` | `verdict` | `needs_evidence` | `evidence_findings`.
    pub kind: &'a str,
    pub body: &'a str,
    /// When true, this entry blocks proposal readiness.
    pub blocking: bool,
    /// Agent role (e.g. "advocate", "adversary", "judge", "spike").
    pub agent_role: &'a str,
    /// `agent` (default) or `user`.
    pub author_kind: &'a str,
    pub author_model: Option<&'a str>,
    /// Optional source task attribution.
    pub source_task_id: Option<&'a str>,
    /// The proposal revision this entry is written against.
    pub against_revision_seq: i32,
    /// Debate round (1-based).
    pub round: i32,
    /// Optional structured metadata persisted as JSONB in
    /// `proposal_debate_trail.body_metadata`. Required for `needs_evidence`
    /// (linkage to proposal/Judge task/spike task/round/revision) and for
    /// `evidence_findings` (the structured findings payload). Optional and
    /// ignored for `objection`/`rebuttal`/`verdict`, which only need the
    /// `body` text.
    pub body_metadata: Option<&'a serde_json::Value>,
}

/// A Planner-authored acceptance-criteria spec amendment. Unlike
/// [`ProposalRepository::set_acceptance_criteria`], these operations are real
/// spec edits: they bump the proposal revision and write an audit trail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProposalAcceptanceCriteriaAmendment<'a> {
    Rewrite { index: usize, criterion: &'a str },
    Drop { index: usize },
    Waive { index: usize },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
struct ProposalAcceptanceCriteriaAuditEntry {
    operation: &'static str,
    index: usize,
    old_criterion: serde_json::Value,
    new_criterion: serde_json::Value,
}

struct ProposalRevisionSnapshot<'a> {
    proposal_id: &'a str,
    seq: i32,
    title: &'a str,
    body: &'a str,
    body_format: &'a str,
    acceptance_criteria: &'a serde_json::Value,
    edited_by: Option<&'a str>,
    /// Optional structured metadata to persist into the revision row's
    /// `event_metadata` JSONB column. `None` writes SQL `NULL` (the historical
    /// default for ordinary `proposal_update` revisions). Set by callers that
    /// need to attribute the revision to a specific source (e.g. a planner
    /// targeted block-patch attached to the active native-skill version).
    event_metadata: Option<&'a serde_json::Value>,
    // Retained-body status history is a checked snapshot too, but has a
    // distinct event annotation from a material spec edit.
    event_kind: &'a str,
    status_from: Option<&'a str>,
    status_to: Option<&'a str>,
}

/// Linkage payload attached to a `needs_evidence` debate-trail entry's
/// `body_metadata`. Persists the structured linkage so the Judge/spike/
/// coordinator can recover the demand's identity from a debate row alone,
/// without needing to read `proposals.needs_evidence_claim`.
///
/// Fields are intentionally the same identifiers the proposal substrate
/// already uses (proposal id, Judge task id, spike task id, round, revision
/// sequence) plus a marker `kind = "needs_evidence_link_v1"` so future
/// schema versions can be distinguished.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct NeedsEvidenceClaimLink {
    /// Schema marker; `needs_evidence_link_v1`.
    pub kind: String,
    /// Proposal id (UUID) the demand was issued against. Required.
    pub proposal_id: String,
    /// Judge task id (UUID) that issued the demand. Required.
    pub judge_task_id: String,
    /// Spike task id (UUID) created (or to be created) to satisfy the
    /// demand. Required.
    pub spike_task_id: String,
    /// Refinement debate round when the demand was issued. Required.
    pub round: i32,
    /// Proposal revision sequence the demand targets. Required.
    pub against_revision_seq: i32,
}

impl NeedsEvidenceClaimLink {
    pub const KIND_MARKER: &'static str = "needs_evidence_link_v1";

    /// Build the structured payload from a `NeedsEvidenceClaim` (the typed
    /// claim produced by the Judge demand tool) plus the proposal id and
    /// spike task id the substrate already knows about.
    pub fn from_claim(proposal_id: &str, spike_task_id: &str, claim: &NeedsEvidenceClaim) -> Self {
        Self {
            kind: Self::KIND_MARKER.to_owned(),
            proposal_id: proposal_id.to_owned(),
            judge_task_id: claim.created_by_task_id.clone(),
            spike_task_id: spike_task_id.to_owned(),
            round: claim.round,
            against_revision_seq: claim.against_revision_seq,
        }
    }

    /// Serialize this link to JSON for storage in `body_metadata`.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("NeedsEvidenceClaimLink is always serializable")
    }

    /// Parse + validate the linkage payload out of a `body_metadata` JSONB
    /// value. Returns `Err(String)` on schema mismatch or missing required
    /// fields so callers can surface a clear error message.
    pub fn from_metadata(meta: &serde_json::Value) -> std::result::Result<Self, String> {
        // First reject obvious shape problems up front.
        let obj = meta
            .as_object()
            .ok_or_else(|| "needs_evidence body_metadata must be a JSON object".to_owned())?;
        let kind = obj.get("kind").and_then(|v| v.as_str()).ok_or_else(|| {
            "needs_evidence body_metadata missing required field \"kind\"".to_owned()
        })?;
        if kind != Self::KIND_MARKER {
            return Err(format!(
                "needs_evidence body_metadata kind mismatch: expected {:?}, got {:?}",
                Self::KIND_MARKER,
                kind
            ));
        }
        // Then serde-deserialize to enforce the typed schema (this is what
        // catches missing required string/int fields).
        let link: Self = serde_json::from_value(meta.clone()).map_err(|e| {
            format!("needs_evidence body_metadata failed to parse NeedsEvidenceClaimLink: {e}")
        })?;
        if link.proposal_id.trim().is_empty() {
            return Err("needs_evidence body_metadata.proposal_id must be non-empty".to_owned());
        }
        if link.judge_task_id.trim().is_empty() {
            return Err("needs_evidence body_metadata.judge_task_id must be non-empty".to_owned());
        }
        if link.spike_task_id.trim().is_empty() {
            return Err("needs_evidence body_metadata.spike_task_id must be non-empty".to_owned());
        }
        if link.round <= 0 {
            return Err("needs_evidence body_metadata.round must be >= 1".to_owned());
        }
        if link.against_revision_seq <= 0 {
            return Err(
                "needs_evidence body_metadata.against_revision_seq must be >= 1".to_owned(),
            );
        }
        Ok(link)
    }
}

/// Lifecycle event kinds for the evidence pipeline that build on
/// `record_refinement_lifecycle`. Centralized so sibling code can pattern-
/// match a single source of truth instead of stringly-typing the kinds at
/// every call site.
pub mod evidence_lifecycle_kind {
    /// Refinement has been parked waiting for an evidence spike to close.
    pub const AWAITING_EVIDENCE_STARTED: &str = "refinement_awaiting_evidence_started";
    /// The spike returned structured findings and the substrate has accepted
    /// the result; refinement resumes.
    pub const EVIDENCE_RECEIVED: &str = "refinement_evidence_received";
    /// The spike failed (cancelled, errored, or force-closed without
    /// findings). Refinement resumes without the spike answer.
    pub const EVIDENCE_FAILED: &str = "refinement_evidence_failed";
}

/// Builder for the structured `event_metadata` JSON used by the three
/// evidence lifecycle events. Field names match the convention the
/// `record_refinement_lifecycle` row already documents (round, revision,
/// source task ids, etc.) and stay stable across sibling tasks.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct EvidenceLifecycleMetadata {
    /// Discriminant field — values: `"awaiting_started"`, `"received"`,
    /// `"failed"`. Stored under `event_metadata.phase` so downstream
    /// readers don't need to know the lifecycle `event_kind` to interpret
    /// the metadata.
    pub phase: String,
    /// Proposal id the lifecycle event applies to. Required.
    pub proposal_id: String,
    /// Spike task id this event is about (the spike that produced or
    /// failed to produce evidence). Required.
    pub spike_task_id: String,
    /// Judge task id that originally issued the needs-evidence demand.
    /// Required for `awaiting_started` and `received`; may be omitted for
    /// `failed` if the Judge task was hard-deleted before this event.
    pub judge_task_id: String,
    /// Refinement round this event belongs to. Required.
    pub round: i32,
    /// Proposal revision sequence the spike was working against. Required.
    pub against_revision_seq: i32,
    /// For `evidence_failed`, the reason (`spike_cancelled`,
    /// `spike_errored`, `spike_force_closed`, `malformed_findings`, etc.).
    /// `None` for the other phases.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub failure_reason: Option<String>,
    /// `proposal_debate_trail.id` of the valid `evidence_findings` row that
    /// satisfied the linked spike. Present only for `received`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub findings_debate_entry_id: Option<String>,
    /// Stored structured findings payload from the valid `evidence_findings`
    /// row. Present only for `received` so restart/resume code can find the
    /// exact handoff row without repeating fuzzy lookup.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub findings_metadata_json: Option<String>,
}

impl EvidenceLifecycleMetadata {
    /// Build the metadata for `refinement_awaiting_evidence_started`.
    pub fn awaiting_started(
        proposal_id: &str,
        spike_task_id: &str,
        judge_task_id: &str,
        round: i32,
        against_revision_seq: i32,
    ) -> Self {
        Self {
            phase: "awaiting_started".to_owned(),
            proposal_id: proposal_id.to_owned(),
            spike_task_id: spike_task_id.to_owned(),
            judge_task_id: judge_task_id.to_owned(),
            round,
            against_revision_seq,
            failure_reason: None,
            findings_debate_entry_id: None,
            findings_metadata_json: None,
        }
    }

    /// Build the metadata for `refinement_evidence_received`.
    pub fn received(
        proposal_id: &str,
        spike_task_id: &str,
        judge_task_id: &str,
        round: i32,
        against_revision_seq: i32,
    ) -> Self {
        Self::received_with_findings(
            proposal_id,
            spike_task_id,
            judge_task_id,
            round,
            against_revision_seq,
            None,
            None,
        )
    }

    /// Build the metadata for `refinement_evidence_received`, including the
    /// exact valid findings row that caused receipt classification.
    pub fn received_with_findings(
        proposal_id: &str,
        spike_task_id: &str,
        judge_task_id: &str,
        round: i32,
        against_revision_seq: i32,
        findings_debate_entry_id: Option<&str>,
        findings_metadata_json: Option<&str>,
    ) -> Self {
        Self {
            phase: "received".to_owned(),
            proposal_id: proposal_id.to_owned(),
            spike_task_id: spike_task_id.to_owned(),
            judge_task_id: judge_task_id.to_owned(),
            round,
            against_revision_seq,
            failure_reason: None,
            findings_debate_entry_id: findings_debate_entry_id.map(str::to_owned),
            findings_metadata_json: findings_metadata_json.map(str::to_owned),
        }
    }

    /// Build the metadata for `refinement_evidence_failed`.
    pub fn failed(
        proposal_id: &str,
        spike_task_id: &str,
        judge_task_id: &str,
        round: i32,
        against_revision_seq: i32,
        failure_reason: &str,
    ) -> Self {
        Self {
            phase: "failed".to_owned(),
            proposal_id: proposal_id.to_owned(),
            spike_task_id: spike_task_id.to_owned(),
            judge_task_id: judge_task_id.to_owned(),
            round,
            against_revision_seq,
            failure_reason: Some(failure_reason.to_owned()),
            findings_debate_entry_id: None,
            findings_metadata_json: None,
        }
    }

    /// Serialize to the `serde_json::Value` shape `record_refinement_lifecycle`
    /// expects, wrapped under the documented `metadata` key so reviewers can
    /// see at a glance what the row carries.
    pub fn to_event_metadata(&self) -> serde_json::Value {
        serde_json::json!({"metadata": self})
    }

    /// Parse the `event_metadata` JSON back out of a stored lifecycle row.
    /// Accepts both the legacy unwrapped shape (when `metadata` is the
    /// object itself) and the new wrapped shape; returns the inner metadata.
    pub fn parse_event_metadata(raw: Option<&str>) -> std::result::Result<Option<Self>, String> {
        match raw {
            None | Some("") => Ok(None),
            Some(s) => {
                let v: serde_json::Value = serde_json::from_str(s)
                    .map_err(|e| format!("invalid lifecycle metadata JSON: {e}"))?;
                // Try wrapped form first: `{"metadata": {...}}`.
                if let Some(inner) = v.get("metadata") {
                    return serde_json::from_value::<Self>(inner.clone())
                        .map(Some)
                        .map_err(|e| format!("invalid EvidenceLifecycleMetadata (wrapped): {e}"));
                }
                // Legacy unwrapped form: the metadata IS the object.
                serde_json::from_value::<Self>(v)
                    .map(Some)
                    .map_err(|e| format!("invalid EvidenceLifecycleMetadata (unwrapped): {e}"))
            }
        }
    }
}

/// The current structured `evidence_findings` debate entry for a linked
/// needs-evidence spike, ready for lifecycle/coordinator callers to consume.
#[derive(Clone, Debug, PartialEq)]
pub struct CurrentEvidenceFindings {
    pub proposal_id: String,
    pub spike_task_id: String,
    pub round: i32,
    pub against_revision_seq: i32,
    pub debate_entry_id: String,
    pub debate_entry_body: String,
    pub findings_metadata_json: String,
    pub findings: EvidenceFindings,
}

/// Read-only recovery candidate for proposals still parked on a linked
/// needs-evidence spike. The coordinator restart path uses this persisted view
/// to decide whether each linked spike is still open or has reached a terminal
/// task outcome; lifecycle classification and mutation remain owned by
/// [`ProposalRepository::persist_terminal_linked_spike_evidence_lifecycle`].
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct LinkedEvidenceSpikeRecoveryCandidate {
    pub proposal_id: String,
    pub linked_spike_task_id: String,
    pub linked_spike_task_status: String,
    pub linked_spike_task_close_reason: Option<String>,
}

/// Result of classifying and persisting a terminal linked evidence spike.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalLinkedEvidenceSpikeOutcome {
    /// The spike completed successfully and valid current findings were found;
    /// a `refinement_evidence_received` lifecycle event was written.
    EvidenceReceived,
    /// The spike terminated unsuccessfully, or completed without valid current
    /// findings; a `refinement_evidence_failed` lifecycle event was written.
    EvidenceFailed { reason: String },
    /// A matching receipt/failure event already existed, so no duplicate row
    /// was written.
    AlreadyRecorded { event_kind: String },
    /// The proposal is no longer linked to the supplied spike.
    NotLinked,
    /// The supplied task status is not terminal; callers should wait.
    NotTerminal,
}

/// Cap status for needs-evidence demands in the current refinement run.
///
/// The count is derived entirely from persisted debate/lifecycle rows, so
/// it survives restart-style reconstruction without in-memory state.
/// Completed, failed, cancelled, and force-closed spikes still count
/// because the accepted `needs_evidence` debate entry remains persisted
/// regardless of later spike outcome.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NeedsEvidenceCapStatus {
    /// Number of accepted `needs_evidence` debate entries after the
    /// latest `refinement_start` for this proposal.
    pub count: u32,
    /// The Phase 1 cap value (always 2 for Phase 1).
    pub cap: u32,
    /// `true` when `count >= cap` — a third accepted demand must be
    /// rejected before any spike/link write occurs.
    pub cap_exceeded: bool,
    /// `true` when no `refinement_start` lifecycle event exists for this
    /// proposal, meaning cap accounting has no run boundary. Callers
    /// should treat this as "not in refinement, cap not applicable".
    pub no_refinement_run: bool,
}

/// Durable reconstruction of a refinement parked awaiting the human's single
/// accept/reject review.
///
/// When the tribunal converges (or is escalated to the human), the coordinator
/// writes a `refinement_awaiting_review` lifecycle row into `proposal_revisions`
/// carrying this metadata. Because the row is durable, the parked state can be
/// rebuilt after a server restart instead of being wiped as "interrupted".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AwaitingReviewPark {
    /// The judge's convergence summary text persisted at park time.
    pub judge_summary: Option<String>,
    /// The pre-refinement snapshot revision seq — the revert target if the
    /// human rejects the refined result.
    pub snapshot_revision_seq: Option<i32>,
    /// The refined revision seq the tribunal converged on and parked against.
    pub refined_revision_seq: Option<i32>,
    /// The parked stop-reason tag, if the park was an escalation (e.g.
    /// `round_cap`, `repeated_objection`). `None` for a clean judge-ready
    /// convergence.
    pub stop_reason: Option<String>,
}

/// Minimal proposal reference for labelling (board swimlanes, links).
#[derive(Debug, sqlx::FromRow)]
pub struct ProposalRef {
    pub id: String,
    pub short_id: String,
    pub title: String,
    pub status: String,
    /// Participant accountable for the build (whose credentials the epics'
    /// tasks burn). The board shows their avatar on the proposal swimlane.
    pub build_owner_user_id: Option<String>,
}

pub struct ProposalRepository {
    db: Database,
    events: EventBus,
}

impl ProposalRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self { db, events }
    }

    /// Access the underlying database for constructing sibling repositories.
    pub fn db(&self) -> &Database {
        &self.db
    }

    /// Access the underlying event bus for constructing sibling repositories.
    pub fn events(&self) -> &EventBus {
        &self.events
    }

    pub async fn get(&self, id: &str) -> Result<Option<Proposal>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Proposal,
            r#"SELECT id, short_id, title, body, body_format,
                    acceptance_criteria::text AS "acceptance_criteria!",
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, last_reconciled_revision_seq, pending_reconcile, build_owner_user_id, refinement_owner_user_id, build_frozen, build_breakdown_task_id, linked_spike_task_id, needs_evidence_claim
             FROM proposals WHERE id = $1"#,
            id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn get_by_short_id(&self, short_id: &str) -> Result<Option<Proposal>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Proposal,
            r#"SELECT id, short_id, title, body, body_format,
                    acceptance_criteria::text AS "acceptance_criteria!",
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, last_reconciled_revision_seq, pending_reconcile, build_owner_user_id, refinement_owner_user_id, build_frozen, build_breakdown_task_id, linked_spike_task_id, needs_evidence_claim
             FROM proposals WHERE short_id = $1"#,
            short_id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Resolve a proposal by UUID or short_id.
    pub async fn resolve(&self, id_or_short: &str) -> Result<Option<Proposal>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Proposal,
            r#"SELECT id, short_id, title, body, body_format,
                    acceptance_criteria::text AS "acceptance_criteria!",
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, last_reconciled_revision_seq, pending_reconcile, build_owner_user_id, refinement_owner_user_id, build_frozen, build_breakdown_task_id, linked_spike_task_id, needs_evidence_claim
             FROM proposals WHERE id = $1 OR short_id = $2"#,
            id_or_short,
            id_or_short
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn create(&self, input: ProposalCreateInput<'_>) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        let short_id = self.generate_short_id(&id).await?;
        let status = input.status.unwrap_or("draft");
        let body_format = input.body_format.unwrap_or("markdown");
        let ac_str = input.acceptance_criteria.unwrap_or("[]");
        let acceptance_criteria: serde_json::Value = serde_json::from_str(ac_str).map_err(|e| {
            Error::InvalidData(format!(
                "invalid json for proposals.acceptance_criteria: {e}"
            ))
        })?;
        // Author is the authenticated MCP caller, mirroring how epics stamp
        // `created_by_user_id`. `None` when no user context is in scope.
        let author_user_id = djinn_core::auth_context::current_user_id();
        let mut tx = self.db.pool().begin().await?;
        sqlx::query!(
            "INSERT INTO proposals (id, short_id, title, body, body_format, acceptance_criteria, status, author_user_id, latest_revision_seq)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 1)",
            id,
            short_id,
            input.title,
            input.body,
            body_format,
            acceptance_criteria,
            status,
            author_user_id
        )
        .execute(&mut *tx)
        .await?;
        // Seed revision 1 with the initial spec so every proposal has a head to
        // diff against. The seed carries no authoring metadata — the proposal
        // is brand-new, so the block-patch / native-skill attribution contract
        // does not apply.
        self.insert_revision_checked(
            &mut tx,
            ProposalRevisionSnapshot {
                proposal_id: &id,
                seq: 1,
                title: input.title,
                body: input.body,
                body_format,
                acceptance_criteria: &acceptance_criteria,
                edited_by: author_user_id.as_deref(),
                event_metadata: None,
                event_kind: "spec_revision",
                status_from: None,
                status_to: None,
            },
        )
        .await?;
        tx.commit().await?;
        let proposal = self.get_required(&id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_created(&proposal));
        Ok(proposal)
    }

    pub async fn update(&self, id: &str, input: ProposalUpdateInput<'_>) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        let acceptance_criteria: serde_json::Value =
            serde_json::from_str(input.acceptance_criteria).map_err(|e| {
                Error::InvalidData(format!(
                    "invalid json for proposals.acceptance_criteria: {e}"
                ))
            })?;
        let current = self
            .get(id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("proposal not found: {id}")))?;
        let body_format = input.body_format.unwrap_or(&current.body_format);
        let current_ac: serde_json::Value =
            serde_json::from_str(&current.acceptance_criteria).unwrap_or(serde_json::json!([]));
        // A "material" edit changes the spec (title/body/AC), not just status.
        // Only material edits append a revision and disturb sign-offs.
        let content_changed = input.title != current.title
            || input.body != current.body
            || body_format != current.body_format
            || acceptance_criteria != current_ac;

        // Stale/hard rule: editing the spec of an *approved* proposal reverts it
        // to in_review and clears its sign-offs (you changed an approved spec).
        // While in_review, edits leave sign-offs in place — they go stale
        // automatically because the head revision advances past them.
        let demote = content_changed && current.status == "approved";
        let building_amend = content_changed && current.status == "building";
        let effective_status = if building_amend {
            "building"
        } else if demote && input.status == "approved" {
            "in_review"
        } else {
            input.status
        };
        let next_seq = if content_changed {
            current.latest_revision_seq + 1
        } else {
            current.latest_revision_seq
        };
        let status_changed = current.status != effective_status;
        let record_done_status_event =
            !content_changed && status_changed && effective_status == "done";

        let mut tx = self.db.pool().begin().await?;
        sqlx::query!(
            r#"UPDATE proposals SET title = $1, body = $2, body_format = $10, acceptance_criteria = $3, status = $4,
                    superseded_by = $5, latest_revision_seq = $8,
                    pending_reconcile = CASE WHEN $9 THEN true ELSE pending_reconcile END,
                    closed_at = CASE WHEN $6 IN ('done', 'rejected', 'archived', 'superseded')
                        THEN COALESCE(closed_at, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
                        ELSE NULL END,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $7"#,
            input.title,
            input.body,
            acceptance_criteria,
            effective_status,
            input.superseded_by,
            effective_status,
            id,
            next_seq,
            building_amend,
            body_format,
        )
        .execute(&mut *tx)
        .await?;

        if content_changed {
            let editor = djinn_core::auth_context::current_user_id();
            self.insert_revision_checked(
                &mut tx,
                ProposalRevisionSnapshot {
                    proposal_id: id,
                    seq: next_seq,
                    title: input.title,
                    body: input.body,
                    body_format,
                    acceptance_criteria: &acceptance_criteria,
                    edited_by: editor.as_deref(),
                    event_metadata: input.event_metadata,
                    event_kind: "spec_revision",
                    status_from: None,
                    status_to: None,
                },
            )
            .await?;
        } else if record_done_status_event {
            let editor = djinn_core::auth_context::current_user_id();
            self.insert_revision_checked(
                &mut tx,
                ProposalRevisionSnapshot {
                    proposal_id: id,
                    seq: next_seq,
                    title: input.title,
                    body: input.body,
                    body_format,
                    acceptance_criteria: &acceptance_criteria,
                    edited_by: editor.as_deref(),
                    event_metadata: None,
                    event_kind: "status_change",
                    status_from: Some(&current.status),
                    status_to: Some(effective_status),
                },
            )
            .await?;
        }
        if demote {
            sqlx::query!("DELETE FROM proposal_signoffs WHERE proposal_id = $1", id)
                .execute(&mut *tx)
                .await?;
        }

        // Re-evaluate the approval gate after any status/spec change. Sign-offs
        // can already be present when a proposal *enters* in_review (e.g. signed
        // while in draft, or promoted via the status dropdown); add_signoff only
        // reconciles at sign-off time, so without this the gate would never fire.
        if current.status != "building" {
            self.reconcile_approval_in_tx(&mut tx, id).await?;
        }
        tx.commit().await?;
        let proposal = self.get_required(id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&proposal));
        Ok(proposal)
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!("DELETE FROM proposals WHERE id = $1", id)
            .execute(self.db.pool())
            .await?;
        self.events.send(DjinnEventEnvelope::proposal_deleted(id));
        Ok(())
    }

    // ── Targets (editable M:N to projects) ───────────────────────────────────

    pub async fn targets(&self, proposal_id: &str) -> Result<Vec<ProposalTarget>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            ProposalTarget,
            r#"SELECT proposal_id, project_id, role, created_at
             FROM proposal_targets WHERE proposal_id = $1 ORDER BY created_at"#,
            proposal_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Add (or re-role) a target project. Idempotent on `(proposal_id,
    /// project_id)`; re-adding updates the role. The `project_id` FK must
    /// reference a registered project.
    pub async fn add_target(&self, proposal_id: &str, project_id: &str, role: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "INSERT INTO proposal_targets (proposal_id, project_id, role) VALUES ($1, $2, $3)
             ON CONFLICT (proposal_id, project_id) DO UPDATE SET role = EXCLUDED.role",
            proposal_id,
            project_id,
            role
        )
        .execute(self.db.pool())
        .await?;
        if let Some(proposal) = self.get(proposal_id).await? {
            self.events
                .send(DjinnEventEnvelope::proposal_updated(&proposal));
        }
        Ok(())
    }

    /// Remove a target project. No-op if absent.
    pub async fn remove_target(&self, proposal_id: &str, project_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "DELETE FROM proposal_targets WHERE proposal_id = $1 AND project_id = $2",
            proposal_id,
            project_id
        )
        .execute(self.db.pool())
        .await?;
        if let Some(proposal) = self.get(proposal_id).await? {
            self.events
                .send(DjinnEventEnvelope::proposal_updated(&proposal));
        }
        Ok(())
    }

    // ── Feedback (discussion; resolved through djinn, not applied directly) ──

    pub async fn feedback(&self, proposal_id: &str) -> Result<Vec<ProposalFeedback>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            ProposalFeedback,
            r#"SELECT id, proposal_id, parent_id, author_kind, author_user_id, author_model,
                    body, resolved_at, resolved_revision_seq, resolved_by_user_id, created_at, updated_at
             FROM proposal_feedback WHERE proposal_id = $1 ORDER BY created_at"#,
            proposal_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn get_feedback(&self, feedback_id: &str) -> Result<Option<ProposalFeedback>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            ProposalFeedback,
            r#"SELECT id, proposal_id, parent_id, author_kind, author_user_id, author_model,
                    body, resolved_at, resolved_revision_seq, resolved_by_user_id, created_at, updated_at
             FROM proposal_feedback WHERE id = $1"#,
            feedback_id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn add_feedback(
        &self,
        input: ProposalFeedbackCreateInput<'_>,
    ) -> Result<ProposalFeedback> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        let author_user_id = djinn_core::auth_context::current_user_id();
        sqlx::query!(
            "INSERT INTO proposal_feedback
                (id, proposal_id, parent_id, author_kind, author_user_id, author_model, body)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            id,
            input.proposal_id,
            input.parent_id,
            input.author_kind,
            author_user_id,
            input.author_model,
            input.body
        )
        .execute(self.db.pool())
        .await?;
        let feedback = self.get_feedback_required(&id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_feedback_created(
                input.proposal_id,
                &feedback,
            ));
        Ok(feedback)
    }

    /// Resolve a feedback entry: collapse it out of the active thread. Pass the
    /// revision that addressed it (when djinn applied a spec change) or `None`
    /// for a plain dismissal. Stamps the resolving user via `current_user_id()`.
    /// Idempotent — re-resolving just refreshes the resolution.
    pub async fn set_feedback_resolved(
        &self,
        feedback_id: &str,
        resolved_revision_seq: Option<i32>,
    ) -> Result<ProposalFeedback> {
        self.db.ensure_initialized().await?;
        let resolved_by = djinn_core::auth_context::current_user_id();
        sqlx::query!(
            r#"UPDATE proposal_feedback SET
                    resolved_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                    resolved_revision_seq = $1,
                    resolved_by_user_id = $2,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $3"#,
            resolved_revision_seq,
            resolved_by,
            feedback_id
        )
        .execute(self.db.pool())
        .await?;
        let feedback = self.get_feedback_required(feedback_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_feedback_created(
                &feedback.proposal_id,
                &feedback,
            ));
        Ok(feedback)
    }

    // ── Debate trail (structured objections/rebuttals/verdicts) ──────────────

    /// List debate-trail entries for a proposal, ordered by round then creation.
    pub async fn debate_trail(&self, proposal_id: &str) -> Result<Vec<ProposalDebateTrail>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            ProposalDebateTrail,
            r#"SELECT id, proposal_id, kind, body, blocking, agent_role, author_kind,
                    author_user_id, author_model, source_task_id,
                    against_revision_seq, round,
                    body_metadata::text AS body_metadata,
                    resolved_at, resolved_by_user_id,
                    reopened_at, reopened_by_user_id,
                    created_at, updated_at
             FROM proposal_debate_trail
             WHERE proposal_id = $1
             ORDER BY round, created_at"#,
            proposal_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Get a single debate-trail entry by id.
    pub async fn get_debate_trail_entry(
        &self,
        entry_id: &str,
    ) -> Result<Option<ProposalDebateTrail>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            ProposalDebateTrail,
            r#"SELECT id, proposal_id, kind, body, blocking, agent_role, author_kind,
                    author_user_id, author_model, source_task_id,
                    against_revision_seq, round,
                    body_metadata::text AS body_metadata,
                    resolved_at, resolved_by_user_id,
                    reopened_at, reopened_by_user_id,
                    created_at, updated_at
             FROM proposal_debate_trail
             WHERE id = $1"#,
            entry_id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Append a debate-trail entry. Validates that the proposal exists and that
    /// `kind` is one of the allowed values. Emits a `proposal_debate_trail_created` event.
    ///
    /// Allowed kinds:
    /// - `objection` | `rebuttal` | `verdict` — human-readable debate rows
    ///   with `body` text; `body_metadata` is optional and ignored when set.
    /// - `needs_evidence` — Judge's structured demand that refinement be
    ///   parked for a spike. Requires `agent_role = "judge"`, `blocking =
    ///   true` (so it participates in the open-blocking partial index used
    ///   by readiness queries), and `body_metadata` containing the
    ///   [`NeedsEvidenceClaimLink`] linkage payload (proposal id, Judge
    ///   task id, spike task id, round, revision). The `body` field holds
    ///   the human-readable question summary.
    /// - `evidence_findings` — Spike's structured answer. Requires
    ///   `agent_role = "spike"`, `blocking = false`, and `body_metadata`
    ///   containing a well-formed [`EvidenceFindings`] payload (per the
    ///   schema in the proposal acceptance criteria). Malformed findings
    ///   are rejected with a clear error.
    pub async fn add_debate_trail_entry(
        &self,
        input: ProposalDebateTrailCreateInput<'_>,
    ) -> Result<ProposalDebateTrail> {
        self.db.ensure_initialized().await?;
        // Validate kind + per-kind invariants.
        match input.kind {
            "objection" | "rebuttal" | "verdict" => {
                // No metadata required for the legacy kinds; `body_metadata`
                // is silently ignored.
            }
            "needs_evidence" => {
                if input.agent_role != "judge" {
                    return Err(Error::InvalidData(format!(
                        "needs_evidence debate entry requires agent_role = \"judge\", got {:?}",
                        input.agent_role
                    )));
                }
                if !input.blocking {
                    return Err(Error::InvalidData(
                        "needs_evidence debate entry must be blocking = true (it gates \
                         refinement until the spike closes)"
                            .to_owned(),
                    ));
                }
                let meta = input.body_metadata.ok_or_else(|| {
                    Error::InvalidData(
                        "needs_evidence debate entry requires body_metadata with linkage \
                         (proposal id, Judge task id, spike task id, round, revision)"
                            .to_owned(),
                    )
                })?;
                NeedsEvidenceClaimLink::from_metadata(meta).map_err(Error::InvalidData)?;
            }
            "evidence_findings" => {
                if input.agent_role != "spike" {
                    return Err(Error::InvalidData(format!(
                        "evidence_findings debate entry requires agent_role = \"spike\", got {:?}",
                        input.agent_role
                    )));
                }
                if input.blocking {
                    return Err(Error::InvalidData(
                        "evidence_findings debate entry must be blocking = false (the spike \
                         answer resolves the demand rather than blocking readiness)"
                            .to_owned(),
                    ));
                }
                let meta = input.body_metadata.ok_or_else(|| {
                    Error::InvalidData(
                        "evidence_findings debate entry requires body_metadata containing the \
                         structured findings payload"
                            .to_owned(),
                    )
                })?;
                let findings = EvidenceFindings::parse_stored(Some(&meta.to_string()))
                    .map_err(|e| {
                        Error::InvalidData(format!(
                            "evidence_findings body_metadata must contain structured findings: {e}"
                        ))
                    })?
                    .ok_or_else(|| {
                        Error::InvalidData(
                            "evidence_findings body_metadata must contain a non-empty \
                                 structured findings payload"
                                .to_owned(),
                        )
                    })?;
                findings.validate().map_err(|e| {
                    Error::InvalidData(format!(
                        "evidence_findings body_metadata must contain structured findings: {e}"
                    ))
                })?;
            }
            other => {
                return Err(Error::InvalidData(format!(
                    "invalid debate trail kind: {other:?}; expected objection, rebuttal, verdict, needs_evidence, or evidence_findings"
                )));
            }
        }
        // Validate author_kind.
        match input.author_kind {
            "agent" | "user" => {}
            other => {
                return Err(Error::InvalidData(format!(
                    "invalid author_kind: {other:?}; expected agent or user"
                )));
            }
        }
        // Validate proposal exists.
        if self.get(input.proposal_id).await?.is_none() {
            return Err(Error::InvalidData(format!(
                "proposal not found: {}",
                input.proposal_id
            )));
        }
        let id = uuid::Uuid::now_v7().to_string();
        let author_user_id: Option<String> = if input.author_kind == "user" {
            djinn_core::auth_context::current_user_id()
        } else {
            None
        };
        sqlx::query!(
            "INSERT INTO proposal_debate_trail
                (id, proposal_id, kind, body, blocking, agent_role, author_kind,
                 author_user_id, author_model, source_task_id,
                 against_revision_seq, round, body_metadata)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
            id,
            input.proposal_id,
            input.kind,
            input.body,
            input.blocking,
            input.agent_role,
            input.author_kind,
            author_user_id,
            input.author_model,
            input.source_task_id,
            input.against_revision_seq,
            input.round,
            input.body_metadata,
        )
        .execute(self.db.pool())
        .await?;
        let entry = self.get_debate_trail_entry_required(&id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_debate_trail_created(
                input.proposal_id,
                &entry,
            ));
        Ok(entry)
    }

    /// Resolve a debate-trail entry. Stamps the resolving user via
    /// `current_user_id()`. Clears any prior reopen state. Idempotent.
    pub async fn resolve_debate_trail_entry(&self, entry_id: &str) -> Result<ProposalDebateTrail> {
        self.db.ensure_initialized().await?;
        let resolved_by = djinn_core::auth_context::current_user_id();
        sqlx::query!(
            r#"UPDATE proposal_debate_trail SET
                    resolved_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                    resolved_by_user_id = $1,
                    reopened_at = NULL,
                    reopened_by_user_id = NULL,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $2"#,
            resolved_by,
            entry_id
        )
        .execute(self.db.pool())
        .await?;
        let entry = self.get_debate_trail_entry_required(entry_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_debate_trail_updated(
                &entry.proposal_id,
                &entry,
            ));
        Ok(entry)
    }

    /// Reopen a previously resolved debate-trail entry. Stamps the reopening
    /// user via `current_user_id()`. No-op (idempotent) if already open.
    pub async fn reopen_debate_trail_entry(&self, entry_id: &str) -> Result<ProposalDebateTrail> {
        self.reopen_debate_trail_entry_with_user(entry_id, None)
            .await
    }

    /// Reopen a previously resolved debate-trail entry with an explicit user
    /// attribution. When `user_id` is `None`, falls back to
    /// `current_user_id()`. No-op (idempotent) if already open.
    pub async fn reopen_debate_trail_entry_with_user(
        &self,
        entry_id: &str,
        user_id: Option<&str>,
    ) -> Result<ProposalDebateTrail> {
        self.db.ensure_initialized().await?;
        let reopened_by = user_id
            .map(|s| Some(s.to_string()))
            .unwrap_or_else(djinn_core::auth_context::current_user_id);
        sqlx::query!(
            r#"UPDATE proposal_debate_trail SET
                    reopened_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                    reopened_by_user_id = $1,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $2 AND resolved_at IS NOT NULL"#,
            reopened_by,
            entry_id
        )
        .execute(self.db.pool())
        .await?;
        let entry = self.get_debate_trail_entry_required(entry_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_debate_trail_updated(
                &entry.proposal_id,
                &entry,
            ));
        Ok(entry)
    }

    /// Return the unresolved blocking debate-trail entries for a proposal,
    /// ordered by creation time. An entry is "unresolved blocking" when
    /// `blocking = true` AND `resolved_at IS NULL`. Used by readiness gates
    /// and the per-run needs-evidence cap (sibling task `g442`) to count
    /// pending demands without scanning the full trail.
    pub async fn unresolved_blocking_entries(
        &self,
        proposal_id: &str,
    ) -> Result<Vec<ProposalDebateTrail>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            ProposalDebateTrail,
            r#"SELECT id, proposal_id, kind, body, blocking, agent_role, author_kind,
                    author_user_id, author_model, source_task_id,
                    against_revision_seq, round,
                    body_metadata::text AS body_metadata,
                    resolved_at, resolved_by_user_id,
                    reopened_at, reopened_by_user_id,
                    created_at, updated_at
             FROM proposal_debate_trail
             WHERE proposal_id = $1
               AND blocking = true
               AND resolved_at IS NULL
             ORDER BY created_at, id"#,
            proposal_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Return the `needs_evidence` debate-trail entries for a proposal
    /// (resolved or not), ordered by creation time. Used by the per-run cap
    /// accounting to count accepted Judge demands; malformed/rejected
    /// demands never reach this list because `add_debate_trail_entry`
    /// refuses to persist them.
    pub async fn needs_evidence_entries(
        &self,
        proposal_id: &str,
    ) -> Result<Vec<ProposalDebateTrail>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            ProposalDebateTrail,
            r#"SELECT id, proposal_id, kind, body, blocking, agent_role, author_kind,
                    author_user_id, author_model, source_task_id,
                    against_revision_seq, round,
                    body_metadata::text AS body_metadata,
                    resolved_at, resolved_by_user_id,
                    reopened_at, reopened_by_user_id,
                    created_at, updated_at
             FROM proposal_debate_trail
             WHERE proposal_id = $1
               AND kind = 'needs_evidence'
             ORDER BY created_at, id"#,
            proposal_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    // ── Needs-evidence cap accounting ──────────────────────────────────────

    /// Phase 1 cap: the maximum number of accepted `needs_evidence` debate
    /// entries allowed per refinement run. The Judge demand tool should call
    /// [`Self::needs_evidence_cap_status_for_current_run`] before creating a
    /// new demand and reject if `cap_exceeded` is true.
    pub const NEEDS_EVIDENCE_PHASE1_CAP: u32 = 2;

    /// Count accepted `needs_evidence` debate entries that were created after
    /// the latest `refinement_start` lifecycle event for the given proposal.
    ///
    /// Returns `None` when no `refinement_start` exists (no refinement run
    /// boundary). Malformed/rejected demands never count because
    /// `add_debate_trail_entry` refuses to persist them.
    ///
    /// The timestamp comparison uses `created_at` from the debate row versus
    /// the `created_at` of the latest `refinement_start` revision row. This
    /// is deterministic and survives restart because both are persisted.
    pub async fn needs_evidence_count_for_current_run(
        &self,
        proposal_id: &str,
    ) -> Result<Option<u32>> {
        self.db.ensure_initialized().await?;

        // Find the latest refinement_start lifecycle event's created_at.
        let revisions = self.revisions(proposal_id).await?;
        let latest_start = revisions
            .iter()
            .rev()
            .find(|r| r.event_kind == "refinement_start");

        let Some(start) = latest_start else {
            return Ok(None); // No refinement run — cap not applicable.
        };

        // Count needs_evidence debate entries created after the start.
        let count: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) AS "n!: i64"
               FROM proposal_debate_trail
               WHERE proposal_id = $1
                 AND kind = 'needs_evidence'
                 AND created_at > $2"#,
        )
        .bind(proposal_id)
        .bind(&start.created_at)
        .fetch_one(self.db.pool())
        .await?;

        Ok(Some(count as u32))
    }

    /// Return the full cap status for needs-evidence demands in the current
    /// refinement run. This is the primary helper that the Judge demand tool
    /// should call before creating/linking a spike.
    ///
    /// When `no_refinement_run` is true, the caller should not issue demands
    /// (there is no active refinement to park). When `cap_exceeded` is true,
    /// the caller must reject the demand before any spike/link write occurs.
    pub async fn needs_evidence_cap_status_for_current_run(
        &self,
        proposal_id: &str,
    ) -> Result<NeedsEvidenceCapStatus> {
        let count = self
            .needs_evidence_count_for_current_run(proposal_id)
            .await?;
        match count {
            None => Ok(NeedsEvidenceCapStatus {
                count: 0,
                cap: Self::NEEDS_EVIDENCE_PHASE1_CAP,
                cap_exceeded: false,
                no_refinement_run: true,
            }),
            Some(count) => Ok(NeedsEvidenceCapStatus {
                count,
                cap: Self::NEEDS_EVIDENCE_PHASE1_CAP,
                cap_exceeded: count >= Self::NEEDS_EVIDENCE_PHASE1_CAP,
                no_refinement_run: false,
            }),
        }
    }

    // ── Listing ──────────────────────────────────────────────────────────────

    pub async fn list_filtered(&self, query: ProposalListQuery) -> Result<ProposalListResult> {
        self.db.ensure_initialized().await?;
        let (where_sql, params) = proposal_build_where(
            &query.status,
            &query.text,
            &query.author_user_id,
            &query.target_project_id,
        );
        let order_sql = proposal_sort_to_sql(&query.sort);

        // NOTE: dynamic SQL (WHERE clause built from optional filters) — compile-time check not possible
        let total_sql = format!("SELECT COUNT(*) FROM proposals WHERE {where_sql}");
        let mut total_q = sqlx::query_scalar::<_, i64>(&total_sql);
        for p in &params {
            let SqlParam::Text(s) = p;
            total_q = total_q.bind(s.clone());
        }
        let total = total_q.fetch_one(self.db.pool()).await?;

        let limit_ph = format!("${}", params.len() + 1);
        let offset_ph = format!("${}", params.len() + 2);
        // NOTE: dynamic SQL (WHERE + ORDER built from optional filters) — compile-time check not possible.
        // The correlated subquery counts unresolved feedback per row (cheap via
        // the `proposal_feedback_unresolved` partial index) for the list badge.
        let sql = format!(
            r#"SELECT id, short_id, title, body, body_format, acceptance_criteria::text AS acceptance_criteria,
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, last_reconciled_revision_seq, pending_reconcile, build_owner_user_id, refinement_owner_user_id, build_frozen, build_breakdown_task_id, linked_spike_task_id, needs_evidence_claim,
                    (SELECT COUNT(*) FROM proposal_feedback pf
                       WHERE pf.proposal_id = proposals.id AND pf.resolved_at IS NULL) AS unresolved_feedback_count
             FROM proposals WHERE {where_sql} ORDER BY {order_sql} LIMIT {limit_ph} OFFSET {offset_ph}"#
        );
        let mut q = sqlx::query_as::<_, ProposalListRow>(&sql);
        for p in &params {
            let SqlParam::Text(s) = p;
            q = q.bind(s.clone());
        }
        let proposals = q
            .bind(query.limit)
            .bind(query.offset)
            .fetch_all(self.db.pool())
            .await?
            .into_iter()
            .map(|row| (row.proposal, row.unresolved_feedback_count))
            .collect();

        Ok(ProposalListResult {
            proposals,
            total_count: total,
        })
    }

    /// The sole checked retained-body revision-snapshot insertion path.
    async fn insert_revision_checked(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        revision: ProposalRevisionSnapshot<'_>,
    ) -> Result<()> {
        let format = match revision.body_format {
            "markdown" => djinn_spec_lint::BodyFormat::Markdown,
            "mdx" => djinn_spec_lint::BodyFormat::Mdx,
            other => {
                return Err(Error::InvalidData(format!(
                    "invalid proposal body_format for spec lint: {other}"
                )));
            }
        };
        let checked_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let mut result = djinn_spec_lint::lint(revision.body, format, checked_at);
        result.sort_violations();
        result
            .validate_for_body(revision.body)
            .map_err(|e| Error::Internal(format!("invalid spec lint result: {e}")))?;
        if !result.errors.is_empty() {
            return Err(Error::SpecLintRejected(crate::SpecLintRejected {
                code: "SPEC_LINT_REJECTED".into(),
                violations: result
                    .errors
                    .iter()
                    .map(|violation| crate::SpecLintViolation {
                        code: violation.code.clone(),
                        message: violation.message.clone(),
                        span_start: violation.span.start,
                        span_end: violation.span.end,
                    })
                    .collect(),
            }));
        }
        let result_json = serde_json::to_value(&result)?;
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO proposal_revisions (id, proposal_id, seq, title, body, body_format, acceptance_criteria, edited_by_user_id, event_kind, event_metadata, status_from, status_to) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)")
            .bind(&id)
            .bind(revision.proposal_id)
            .bind(revision.seq)
            .bind(revision.title)
            .bind(revision.body)
            .bind(revision.body_format)
            .bind(revision.acceptance_criteria)
            .bind(revision.edited_by)
            .bind(revision.event_kind)
            .bind(revision.event_metadata.cloned())
            .bind(revision.status_from)
            .bind(revision.status_to)
            .execute(&mut **tx)
            .await?;
        // Status snapshots retain the current head's sequence. They must still
        // be linted above, but that head already owns this cache key from its
        // material revision. Keep that valid result rather than conflicting on
        // the per-(proposal, sequence, linter) cache primary key.
        sqlx::query("INSERT INTO proposal_revision_lint_results (proposal_id, revision_seq, linter_version, revision_id, body_sha256, result_json) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (proposal_id, revision_seq, linter_version) DO NOTHING")
            .bind(revision.proposal_id)
            .bind(revision.seq)
            .bind(&result.linter_version)
            .bind(&id)
            .bind(&result.body_sha256)
            .bind(result_json)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    /// Insert an empty-body history row for refinement/evidence lifecycle only.
    /// This deliberately bypasses revision linting because it is not a retained
    /// proposal body snapshot. Callers hold the mutation transaction so durable
    /// lifecycle state commits with its associated proposal mutation.
    async fn insert_lightweight_lifecycle_event_in_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        proposal_id: &str,
        seq: i32,
        event_kind: &str,
        event_metadata: Option<&serde_json::Value>,
    ) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO proposal_revisions
                (id, proposal_id, seq, title, body, body_format, acceptance_criteria, edited_by_user_id, event_kind, event_metadata)
               VALUES ($1, $2, $3, '', '', 'markdown', '[]', NULL, $4, $5)"#,
        )
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(proposal_id)
        .bind(seq)
        .bind(event_kind)
        .bind(event_metadata)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Revisions/history events of a proposal, oldest first.
    pub async fn revisions(&self, proposal_id: &str) -> Result<Vec<ProposalRevision>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, ProposalRevision>(
            r#"SELECT id, proposal_id, seq, title, body, body_format,
                    acceptance_criteria::text AS acceptance_criteria,
                    edited_by_user_id, event_kind, status_from, status_to,
                    event_metadata::text AS event_metadata, created_at
             FROM proposal_revisions
             WHERE proposal_id = $1
             ORDER BY created_at, id"#,
        )
        .bind(proposal_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Return lint results for one immutable stored revision.
    ///
    /// The lint table is a cache, not an authority: its version, revision
    /// identity, body hash, serialized contract, format, and diagnostic spans
    /// must all agree with the stored revision before it can be returned.
    pub async fn lint_for_revision(
        &self,
        revision: &ProposalRevision,
    ) -> Result<djinn_spec_lint::SpecLintResultV1> {
        self.db.ensure_initialized().await?;

        // Re-read the immutable snapshot rather than trusting a caller-provided
        // body. This keeps recomputation bound to the exact persisted revision
        // identified by all three parts of the lint table's foreign key.
        let revision = sqlx::query_as::<_, ProposalRevision>(
            r#"SELECT id, proposal_id, seq, title, body, body_format,
                    acceptance_criteria::text AS acceptance_criteria,
                    edited_by_user_id, event_kind, status_from, status_to,
                    event_metadata::text AS event_metadata, created_at
             FROM proposal_revisions
             WHERE id = $1 AND proposal_id = $2 AND seq = $3"#,
        )
        .bind(&revision.id)
        .bind(&revision.proposal_id)
        .bind(revision.seq)
        .fetch_optional(self.db.pool())
        .await?
        .ok_or_else(|| {
            Error::InvalidData(format!(
                "proposal revision not found: {}/{}/{}",
                revision.proposal_id, revision.seq, revision.id
            ))
        })?;

        let format = match revision.body_format.as_str() {
            "markdown" => djinn_spec_lint::BodyFormat::Markdown,
            "mdx" => djinn_spec_lint::BodyFormat::Mdx,
            other => {
                return Err(Error::InvalidData(format!(
                    "invalid proposal body_format for spec lint: {other}"
                )));
            }
        };
        let expected_hash = djinn_spec_lint::body_sha256(&revision.body);

        let cached = sqlx::query(
            r#"SELECT linter_version, revision_id, body_sha256, result_json
                 FROM proposal_revision_lint_results
                 WHERE proposal_id = $1 AND revision_seq = $2
                   AND linter_version = $3"#,
        )
        .bind(&revision.proposal_id)
        .bind(revision.seq)
        .bind(djinn_spec_lint::SpecLintResultV1::LINTER_VERSION)
        .fetch_optional(self.db.pool())
        .await?;

        if let Some(cached) = cached {
            let cached_version: String = cached.try_get("linter_version")?;
            let cached_revision_id: String = cached.try_get("revision_id")?;
            let cached_hash: String = cached.try_get("body_sha256")?;
            let cached_json: serde_json::Value = cached.try_get("result_json")?;
            if cached_version == djinn_spec_lint::SpecLintResultV1::LINTER_VERSION
                && cached_revision_id == revision.id
                && cached_hash == expected_hash
                && let Ok(result) =
                    serde_json::from_value::<djinn_spec_lint::SpecLintResultV1>(cached_json)
                && result.linter_version == djinn_spec_lint::SpecLintResultV1::LINTER_VERSION
                && result.body_sha256 == expected_hash
                && result.body_format == format
                && result.validate_for_body(&revision.body).is_ok()
            {
                return Ok(result);
            }
        }

        // Repository reads must be reproducible even when repairing legacy or
        // corrupt cache rows. This timestamp intentionally does not consult a
        // clock; persisted creation-time results retain their own provenance.
        let mut result = djinn_spec_lint::lint(&revision.body, format, "1970-01-01T00:00:00.000Z");
        result.sort_violations();
        result
            .validate_for_body(&revision.body)
            .map_err(|e| Error::Internal(format!("invalid spec lint result: {e}")))?;
        Ok(result)
    }

    /// Return the `created_at` of the latest `refinement_start` lifecycle event
    /// for this proposal — the boundary that separates the current refinement
    /// run's debate-trail entries from any prior (interrupted) run's entries.
    ///
    /// Returns `None` when no `refinement_start` exists (no refinement run
    /// boundary recorded). Callers use this to scope debate-trail reads to the
    /// current run so a restarted run's round numbers do not collide with a
    /// prior run's entries (which reuse the same 1-based round counter).
    pub async fn latest_refinement_start_at(&self, proposal_id: &str) -> Result<Option<String>> {
        let revisions = self.revisions(proposal_id).await?;
        Ok(revisions
            .iter()
            .rev()
            .find(|r| r.event_kind == "refinement_start")
            .map(|r| r.created_at.clone()))
    }

    /// Record a refinement lifecycle event (`refinement_start` or
    /// `refinement_stop`) as a lightweight `proposal_revisions` row. These
    /// events carry `event_metadata` with structured JSON (e.g.
    /// `{ "update_authority": "checkpoint" }` or
    /// `{ "stop_reason": "adversary_dry" }`) but no spec snapshot — `title`,
    /// `body`, etc. are empty. The row's `seq` is set to the proposal's current
    /// head revision so ordering stays correct.
    pub async fn record_refinement_lifecycle(
        &self,
        proposal_id: &str,
        event_kind: &str,
        event_metadata: Option<&serde_json::Value>,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        let proposal = self
            .get(proposal_id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("proposal not found: {proposal_id}")))?;
        let mut tx = self.db.pool().begin().await?;
        self.insert_lightweight_lifecycle_event_in_tx(
            &mut tx,
            proposal_id,
            proposal.latest_revision_seq,
            event_kind,
            event_metadata,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Atomically persist a refinement run's durable owner and start boundary.
    pub async fn start_refinement_with_owner(
        &self,
        proposal_id: &str,
        owner_user_id: Option<&str>,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let revision_seq = sqlx::query_scalar::<_, i32>(
            "SELECT latest_revision_seq FROM proposals WHERE id = $1 FOR UPDATE",
        )
        .bind(proposal_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| Error::InvalidData(format!("proposal not found: {proposal_id}")))?;
        sqlx::query("UPDATE proposals SET refinement_owner_user_id = $2 WHERE id = $1")
            .bind(proposal_id)
            .bind(owner_user_id)
            .execute(&mut *tx)
            .await?;
        let event_metadata = serde_json::json!({ "refinement_owner_user_id": owner_user_id });
        self.insert_lightweight_lifecycle_event_in_tx(
            &mut tx,
            proposal_id,
            revision_seq,
            "refinement_start",
            Some(&event_metadata),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Convenience wrapper for `record_refinement_lifecycle` that writes a
    /// `refinement_awaiting_evidence_started` row with the structured
    /// `EvidenceLifecycleMetadata` already shaped. Returns the JSON that was
    /// persisted so callers can echo it in tool responses / logs.
    pub async fn record_awaiting_evidence_started(
        &self,
        proposal_id: &str,
        spike_task_id: &str,
        judge_task_id: &str,
        round: i32,
        against_revision_seq: i32,
    ) -> Result<serde_json::Value> {
        let meta = EvidenceLifecycleMetadata::awaiting_started(
            proposal_id,
            spike_task_id,
            judge_task_id,
            round,
            against_revision_seq,
        );
        let value = meta.to_event_metadata();
        self.record_refinement_lifecycle(
            proposal_id,
            evidence_lifecycle_kind::AWAITING_EVIDENCE_STARTED,
            Some(&value),
        )
        .await?;
        Ok(value)
    }

    /// Convenience wrapper for `record_refinement_lifecycle` that writes a
    /// `refinement_evidence_received` row with the structured metadata.
    pub async fn record_evidence_received(
        &self,
        proposal_id: &str,
        spike_task_id: &str,
        judge_task_id: &str,
        round: i32,
        against_revision_seq: i32,
    ) -> Result<serde_json::Value> {
        let meta = EvidenceLifecycleMetadata::received(
            proposal_id,
            spike_task_id,
            judge_task_id,
            round,
            against_revision_seq,
        );
        let value = meta.to_event_metadata();
        self.record_refinement_lifecycle(
            proposal_id,
            evidence_lifecycle_kind::EVIDENCE_RECEIVED,
            Some(&value),
        )
        .await?;
        Ok(value)
    }

    /// Convenience wrapper for `record_refinement_lifecycle` that writes a
    /// `refinement_evidence_received` row carrying the exact valid findings
    /// debate-entry reference returned by `current_evidence_findings_for_linked_spike`.
    pub async fn record_evidence_received_with_findings(
        &self,
        judge_task_id: &str,
        findings: &CurrentEvidenceFindings,
    ) -> Result<serde_json::Value> {
        let meta = EvidenceLifecycleMetadata::received_with_findings(
            &findings.proposal_id,
            &findings.spike_task_id,
            judge_task_id,
            findings.round,
            findings.against_revision_seq,
            Some(&findings.debate_entry_id),
            Some(&findings.findings_metadata_json),
        );
        let value = meta.to_event_metadata();
        self.record_refinement_lifecycle(
            &findings.proposal_id,
            evidence_lifecycle_kind::EVIDENCE_RECEIVED,
            Some(&value),
        )
        .await?;
        Ok(value)
    }

    /// Convenience wrapper for `record_refinement_lifecycle` that writes a
    /// `refinement_evidence_failed` row with the structured metadata,
    /// including the failure reason.
    pub async fn record_evidence_failed(
        &self,
        proposal_id: &str,
        spike_task_id: &str,
        judge_task_id: &str,
        round: i32,
        against_revision_seq: i32,
        failure_reason: &str,
    ) -> Result<serde_json::Value> {
        let meta = EvidenceLifecycleMetadata::failed(
            proposal_id,
            spike_task_id,
            judge_task_id,
            round,
            against_revision_seq,
            failure_reason,
        );
        let value = meta.to_event_metadata();
        self.record_refinement_lifecycle(
            proposal_id,
            evidence_lifecycle_kind::EVIDENCE_FAILED,
            Some(&value),
        )
        .await?;
        Ok(value)
    }

    /// Return the proposal IDs whose refinement is currently dangling — i.e.
    /// they have more `refinement_start` lifecycle events than `refinement_stop`
    /// events, so a refinement was started but never recorded as stopped.
    ///
    /// On a clean run this is exactly the set the coordinator is actively
    /// driving in memory. After a server restart the in-memory loops are lost
    /// but these DB rows remain, leaving "zombie" refinements that report
    /// `active` yet make no progress. Startup recovery uses this to reconcile
    /// them.
    pub async fn dangling_refinement_proposal_ids(&self) -> Result<Vec<String>> {
        self.db.ensure_initialized().await?;
        let ids = sqlx::query_scalar::<_, String>(
            r#"SELECT proposal_id
               FROM proposal_revisions
               WHERE event_kind IN ('refinement_start', 'refinement_stop')
               GROUP BY proposal_id
               HAVING SUM(CASE WHEN event_kind = 'refinement_start' THEN 1 ELSE 0 END)
                    > SUM(CASE WHEN event_kind = 'refinement_stop' THEN 1 ELSE 0 END)"#,
        )
        .fetch_all(self.db.pool())
        .await?;
        Ok(ids)
    }

    /// If the proposal's current refinement run is parked awaiting the human's
    /// single accept/reject review, return the reconstructed park metadata.
    ///
    /// A run is parked awaiting review when the latest `refinement_awaiting_review`
    /// lifecycle row comes at or after the latest `refinement_start` and is not
    /// superseded by a later `refinement_stop` — the exact same predicate
    /// `build_refinement_status` uses to surface the `awaiting_review` flag.
    ///
    /// Startup recovery uses this to distinguish a legitimately-converged park
    /// (which must be restored so the human can still accept/reject it) from a
    /// refinement genuinely interrupted mid-tribunal (which is stamped
    /// `refinement_stop` with `Interrupted`). Returns `None` when the proposal
    /// is mid-tribunal or not in refinement at all.
    pub async fn parked_awaiting_review(
        &self,
        proposal_id: &str,
    ) -> Result<Option<AwaitingReviewPark>> {
        let revisions = self.revisions(proposal_id).await?;

        let Some(latest_start) = revisions
            .iter()
            .rev()
            .find(|r| r.event_kind == "refinement_start")
        else {
            return Ok(None);
        };
        let latest_awaiting = revisions
            .iter()
            .rev()
            .find(|r| r.event_kind == "refinement_awaiting_review");
        let latest_stop = revisions
            .iter()
            .rev()
            .find(|r| r.event_kind == "refinement_stop");

        // Mirror `build_refinement_status`'s awaiting-review predicate: the park
        // must belong to the current run (at/after the latest start) and must
        // not be superseded by a later stop.
        let is_parked = match (&latest_awaiting, &latest_stop) {
            (Some(aw), Some(stop)) => {
                latest_start.created_at <= aw.created_at && stop.created_at < aw.created_at
            }
            (Some(aw), None) => latest_start.created_at <= aw.created_at,
            _ => false,
        };
        if !is_parked {
            return Ok(None);
        }

        let meta = latest_awaiting
            .and_then(|r| r.event_metadata.as_ref())
            .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok());

        let judge_summary = meta
            .as_ref()
            .and_then(|v| v.get("judge_summary")?.as_str().map(String::from));
        let snapshot_revision_seq = meta
            .as_ref()
            .and_then(|v| v.get("snapshot_revision_seq")?.as_i64())
            .map(|n| n as i32);
        let refined_revision_seq = meta
            .as_ref()
            .and_then(|v| v.get("refined_revision_seq")?.as_i64())
            .map(|n| n as i32);
        let stop_reason = meta
            .as_ref()
            .and_then(|v| v.get("stop_reason")?.as_str().map(String::from));

        Ok(Some(AwaitingReviewPark {
            judge_summary,
            snapshot_revision_seq,
            refined_revision_seq,
            stop_reason,
        }))
    }

    /// Find the latest verdict override for a proposal. Returns
    /// `Some((override_on_revision_seq, override_metadata_json))` when an
    /// active override exists, or `None` when no override has been recorded.
    ///
    /// Gate composition (task cuzf) uses this to check whether a human
    /// override supersedes a judge `needs-work` verdict: the override is
    /// active when its `override_on_revision_seq` equals the proposal's
    /// current `latest_revision_seq`.
    pub async fn latest_verdict_override(
        &self,
        proposal_id: &str,
    ) -> Result<Option<(i32, String)>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query_scalar::<_, Option<String>>(
            r#"SELECT event_metadata::text FROM proposal_revisions
               WHERE proposal_id = $1
                 AND event_kind = 'verdict_override'
               ORDER BY created_at DESC, id DESC
               LIMIT 1"#,
        )
        .bind(proposal_id)
        .fetch_optional(self.db.pool())
        .await?;
        if let Some(Some(meta_str)) = row {
            // Extract override_on_revision_seq from the JSON.
            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&meta_str)
                && let Some(seq) = meta
                    .get("override_on_revision_seq")
                    .and_then(|v| v.as_i64())
            {
                return Ok(Some((seq as i32, meta_str)));
            }
        }
        Ok(None)
    }

    /// Return the latest human demand-round reviewer feedback for the proposal's
    /// current head revision.
    ///
    /// Demand-round feedback is recorded on `refinement_start` lifecycle rows.
    /// Because those rows carry `seq = proposals.latest_revision_seq`, this
    /// helper deliberately filters to the current seq so feedback from a prior
    /// proposal revision is not reused by a later tribunal round.
    pub async fn latest_current_revision_reviewer_feedback(
        &self,
        proposal_id: &str,
    ) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;
        let proposal = self
            .get(proposal_id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("proposal not found: {proposal_id}")))?;

        let rows = sqlx::query_scalar::<_, Option<String>>(
            r#"SELECT event_metadata::text FROM proposal_revisions
               WHERE proposal_id = $1
                 AND seq = $2
                 AND event_kind = 'refinement_start'
                 AND event_metadata IS NOT NULL
               ORDER BY created_at DESC, id DESC"#,
        )
        .bind(proposal_id)
        .bind(proposal.latest_revision_seq)
        .fetch_all(self.db.pool())
        .await?;

        for meta_str in rows.into_iter().flatten() {
            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&meta_str)
                && meta.get("source").and_then(|v| v.as_str()) == Some("human_demand_round")
                && let Some(feedback) = meta.get("reviewer_feedback").and_then(|v| v.as_str())
            {
                return Ok(Some(feedback.to_string()));
            }
        }
        Ok(None)
    }

    /// Patch the `event_metadata` column on the latest `spec_revision` row for
    /// `proposal_id`.  Used by the refinement coordinator to retroactively
    /// attribute an advocate-authored revision after the agent session completes
    /// (the agent's `proposal_update` tool call doesn't carry refinement
    /// context, so the metadata is set post-hoc).
    ///
    /// When no `spec_revision` row exists for the given `seq`, this is a
    /// no-op — the revision was created by a non-spec source (lifecycle event,
    /// status change, etc.) and doesn't need attribution.
    pub async fn set_latest_revision_event_metadata(
        &self,
        proposal_id: &str,
        seq: i32,
        event_metadata: &serde_json::Value,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        let metadata: Option<serde_json::Value> = Some(event_metadata.clone());
        sqlx::query(
            r#"UPDATE proposal_revisions
               SET event_metadata = $3
             WHERE proposal_id = $1 AND seq = $2 AND event_kind = 'spec_revision'"#,
        )
        .bind(proposal_id)
        .bind(seq)
        .bind(metadata)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Stamp `event_metadata` on every `spec_revision` row in the open seq range
    /// `(from_seq_exclusive, to_seq_inclusive]`. Used by the refinement loop so
    /// that ALL revisions an advocate session produces in a round (e.g. a body
    /// edit plus an acceptance-criteria edit) carry the `source =
    /// "refinement_loop"` attribution — letting the history UI collapse the
    /// whole tribunal run into a single entry.
    pub async fn set_spec_revisions_event_metadata_range(
        &self,
        proposal_id: &str,
        from_seq_exclusive: i32,
        to_seq_inclusive: i32,
        event_metadata: &serde_json::Value,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        let metadata: Option<serde_json::Value> = Some(event_metadata.clone());
        sqlx::query(
            r#"UPDATE proposal_revisions
               SET event_metadata = $4
             WHERE proposal_id = $1
               AND event_kind = 'spec_revision'
               AND seq > $2 AND seq <= $3"#,
        )
        .bind(proposal_id)
        .bind(from_seq_exclusive)
        .bind(to_seq_inclusive)
        .bind(metadata)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn signoffs(&self, proposal_id: &str) -> Result<Vec<ProposalSignoff>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            ProposalSignoff,
            r#"SELECT proposal_id, kind, user_id, revision_seq, created_at
             FROM proposal_signoffs WHERE proposal_id = $1 ORDER BY created_at"#,
            proposal_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Record (or refresh) a `kind` sign-off by `user_id`, anchored to the head
    /// revision. Idempotent per (proposal, kind, user). Reconciles approval.
    pub async fn add_signoff(
        &self,
        proposal_id: &str,
        kind: &str,
        user_id: &str,
    ) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        let proposal = self
            .get(proposal_id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("proposal not found: {proposal_id}")))?;
        sqlx::query!(
            r#"INSERT INTO proposal_signoffs (proposal_id, kind, user_id, revision_seq)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (proposal_id, kind, user_id) DO UPDATE
                 SET revision_seq = EXCLUDED.revision_seq,
                     created_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')"#,
            proposal_id,
            kind,
            user_id,
            proposal.latest_revision_seq
        )
        .execute(self.db.pool())
        .await?;
        if proposal.status != "building" {
            self.reconcile_approval(proposal_id).await?;
        }
        let updated = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&updated));
        Ok(updated)
    }

    /// Withdraw `user_id`'s `kind` sign-off. Reconciles approval (may demote
    /// `approved → in_review` if the gate is no longer met).
    pub async fn clear_signoff(
        &self,
        proposal_id: &str,
        kind: &str,
        user_id: &str,
    ) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "DELETE FROM proposal_signoffs WHERE proposal_id = $1 AND kind = $2 AND user_id = $3",
            proposal_id,
            kind,
            user_id
        )
        .execute(self.db.pool())
        .await?;
        let proposal = self
            .get(proposal_id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("proposal not found: {proposal_id}")))?;
        if proposal.status != "building" {
            self.reconcile_approval(proposal_id).await?;
        }
        let updated = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&updated));
        Ok(updated)
    }

    /// Drive the status off the sign-off state. A `draft` auto-advances to
    /// `in_review` on its first fresh sign-off (the act of signing *is* the
    /// request for review), and any state reaches `approved` once both a scoped
    /// and a technical sign-off are fresh at the head revision. An `approved`
    /// proposal auto-demotes back to `in_review` when that's no longer true.
    async fn reconcile_approval(&self, proposal_id: &str) -> Result<()> {
        let mut tx = self.db.pool().begin().await?;
        self.reconcile_approval_in_tx(&mut tx, proposal_id).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Reconcile sign-off-derived status using the caller's transaction. This
    /// keeps material edits, sign-off invalidation, and their status effects
    /// indivisible.
    async fn reconcile_approval_in_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        proposal_id: &str,
    ) -> Result<()> {
        let proposal = match sqlx::query_as::<_, (String, i32)>(
            "SELECT status, latest_revision_seq FROM proposals WHERE id = $1",
        )
        .bind(proposal_id)
        .fetch_optional(&mut **tx)
        .await?
        {
            Some(proposal) => proposal,
            None => return Ok(()),
        };
        let fresh: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(DISTINCT kind) AS "n!: i64" FROM proposal_signoffs
             WHERE proposal_id = $1 AND revision_seq = $2 AND kind IN ('scoped', 'technical')"#,
            proposal_id,
            proposal.1
        )
        .fetch_one(&mut **tx)
        .await?;
        let both = fresh == 2;
        let any = fresh >= 1;
        let new_status = match proposal.0.as_str() {
            "draft" if both => Some("approved"),
            "draft" if any => Some("in_review"),
            "in_review" if both => Some("approved"),
            "approved" if !both => Some("in_review"),
            "building" => None,
            _ => None,
        };
        if let Some(status) = new_status {
            sqlx::query!(
                r#"UPDATE proposals SET status = $1,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                 WHERE id = $2"#,
                status,
                proposal_id
            )
            .execute(&mut **tx)
            .await?;
        }
        Ok(())
    }

    // ── Graduation ───────────────────────────────────────────────────────────

    /// Distinct participants accountable for the proposal: its author plus
    /// everyone who has signed off. The build owner must be one of these.
    pub async fn participants(&self, proposal_id: &str) -> Result<Vec<String>> {
        self.db.ensure_initialized().await?;
        let mut ids: Vec<String> = sqlx::query_scalar!(
            "SELECT DISTINCT user_id FROM proposal_signoffs WHERE proposal_id = $1",
            proposal_id
        )
        .fetch_all(self.db.pool())
        .await?;
        if let Some(p) = self.get(proposal_id).await?
            && let Some(author) = p.author_user_id
            && !ids.contains(&author)
        {
            ids.push(author);
        }
        Ok(ids)
    }

    /// Link a graduated epic to the proposal. Idempotent.
    pub async fn link_epic(
        &self,
        proposal_id: &str,
        epic_id: &str,
        project_id: &str,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        let had_graduated_epics = !self.graduated_epics(proposal_id).await?.is_empty();
        sqlx::query!(
            "INSERT INTO proposal_epics (proposal_id, epic_id, project_id) VALUES ($1, $2, $3)
             ON CONFLICT (proposal_id, epic_id) DO NOTHING",
            proposal_id,
            epic_id,
            project_id
        )
        .execute(self.db.pool())
        .await?;
        self.set_epic_proposal_link(epic_id, Some(proposal_id))
            .await?;
        if let Some(proposal) = self.get(proposal_id).await?
            && proposal.status == "building"
        {
            let seq = if had_graduated_epics {
                proposal
                    .last_reconciled_revision_seq
                    .unwrap_or(proposal.latest_revision_seq)
            } else {
                proposal.latest_revision_seq
            };
            self.record_epic_reconciliation(proposal_id, epic_id, seq)
                .await?;
            if !had_graduated_epics {
                self.mark_reconciled(proposal_id).await?;
            }
        }
        Ok(())
    }

    /// `(epic_id, project_id)` pairs this proposal graduated into.
    pub async fn graduated_epics(&self, proposal_id: &str) -> Result<Vec<(String, String)>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query!(
            "SELECT epic_id, project_id FROM proposal_epics WHERE proposal_id = $1 ORDER BY created_at",
            proposal_id
        )
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.epic_id, r.project_id))
            .collect())
    }

    /// Memory notes attached to this proposal's graduated epics or their tasks.
    ///
    /// Walks `proposal_epics -> epics.memory_refs` and then each graduated
    /// epic's `tasks.memory_refs`, resolving note metadata from the `notes`
    /// table. Duplicate permalinks are returned once, keeping the first source
    /// encountered in graduation/task order.
    pub async fn memory_refs_for_proposal(
        &self,
        proposal_id: &str,
    ) -> Result<Vec<ProposalMemoryRef>> {
        self.db.ensure_initialized().await?;
        let note_repo = NoteRepository::new(self.db.clone(), self.events.clone());
        let mut seen = HashSet::new();
        let mut refs = Vec::new();

        for (epic_id, project_id) in self.graduated_epics(proposal_id).await? {
            let epic = sqlx::query_as::<_, (String, String)>(
                r#"SELECT short_id, memory_refs::text
                   FROM epics
                   WHERE id = $1"#,
            )
            .bind(&epic_id)
            .fetch_optional(self.db.pool())
            .await?;

            if let Some((epic_short_id, epic_memory_refs)) = epic {
                for permalink in parse_memory_refs_json(&epic_memory_refs)? {
                    if let Some(note) = note_repo.get_by_permalink(&project_id, &permalink).await?
                        && seen.insert(permalink.clone())
                    {
                        refs.push(ProposalMemoryRef {
                            permalink,
                            title: note.title,
                            note_type: note.note_type,
                            source_entity_type: "epic".to_owned(),
                            source_short_id: epic_short_id.clone(),
                        });
                    }
                }

                let task_rows = sqlx::query_as::<_, (String, String)>(
                    r#"SELECT short_id, memory_refs::text
                       FROM tasks
                       WHERE epic_id = $1
                       ORDER BY created_at, id"#,
                )
                .bind(&epic_id)
                .fetch_all(self.db.pool())
                .await?;

                for (task_short_id, task_memory_refs) in task_rows {
                    for permalink in parse_memory_refs_json(&task_memory_refs)? {
                        if let Some(note) =
                            note_repo.get_by_permalink(&project_id, &permalink).await?
                            && seen.insert(permalink.clone())
                        {
                            refs.push(ProposalMemoryRef {
                                permalink,
                                title: note.title,
                                note_type: note.note_type,
                                source_entity_type: "task".to_owned(),
                                source_short_id: task_short_id.clone(),
                            });
                        }
                    }
                }
            }
        }

        Ok(refs)
    }

    /// Stamp that one graduated epic has been reconciled against a proposal
    /// revision. Idempotent for repeated reconcile runs of the same revision.
    pub async fn record_epic_reconciliation(
        &self,
        proposal_id: &str,
        epic_id: &str,
        revision_seq: i32,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            r#"INSERT INTO proposal_reconciliations (proposal_id, epic_id, revision_seq)
               VALUES ($1, $2, $3)
               ON CONFLICT (proposal_id, epic_id, revision_seq) DO NOTHING"#,
        )
        .bind(proposal_id)
        .bind(epic_id)
        .bind(revision_seq)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Latest reconciled proposal revision per graduated epic for a proposal.
    pub async fn latest_epic_reconciliations(
        &self,
        proposal_id: &str,
    ) -> Result<HashMap<String, i32>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query_as::<_, (String, Option<i32>)>(
            r#"SELECT pe.epic_id, MAX(pr.revision_seq) AS revision_seq
               FROM proposal_epics pe
               LEFT JOIN proposal_reconciliations pr
                 ON pr.proposal_id = pe.proposal_id
                AND pr.epic_id = pe.epic_id
               WHERE pe.proposal_id = $1
               GROUP BY pe.epic_id"#,
        )
        .bind(proposal_id)
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(epic_id, revision_seq)| revision_seq.map(|seq| (epic_id, seq)))
            .collect())
    }

    /// Lightweight `(id, short_id, title)` lookup for a set of proposal ids.
    /// Used by `epic_list` to label proposal swimlanes on the board without
    /// hydrating full proposals.
    pub async fn refs_by_ids(&self, ids: &[String]) -> Result<Vec<ProposalRef>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, ProposalRef>(
            "SELECT id, short_id, title, status, build_owner_user_id FROM proposals WHERE id = ANY($1)",
        )
        .bind(ids)
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Mirror a `proposal_epics` link change onto the denormalized
    /// `epics.proposal_id` column and re-emit the epic so live boards regroup
    /// the swimlane immediately.
    async fn set_epic_proposal_link(&self, epic_id: &str, proposal_id: Option<&str>) -> Result<()> {
        sqlx::query!(
            "UPDATE epics SET proposal_id = $1 WHERE id = $2",
            proposal_id,
            epic_id
        )
        .execute(self.db.pool())
        .await?;
        let epics = EpicRepository::new(self.db.clone(), self.events.clone());
        if let Some(epic) = epics.get(epic_id).await? {
            epics.emit_updated(&epic).await;
        }
        Ok(())
    }

    /// Drop every graduated-epic link for a proposal. The missing counterpart
    /// to [`Self::link_epic`] (which only ever inserts): an aborted build must
    /// unlink its epics so a later re-graduation starts from a clean set
    /// instead of accumulating closed epics from prior generations.
    pub async fn unlink_epics(&self, proposal_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        let linked = self.graduated_epics(proposal_id).await?;
        sqlx::query!(
            "DELETE FROM proposal_epics WHERE proposal_id = $1",
            proposal_id
        )
        .execute(self.db.pool())
        .await?;
        for (epic_id, _) in linked {
            self.set_epic_proposal_link(&epic_id, None).await?;
        }
        Ok(())
    }

    pub async fn unlink_epics_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        proposal_id: &str,
    ) -> Result<()> {
        sqlx::query("DELETE FROM proposal_epics WHERE proposal_id = $1")
            .bind(proposal_id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("UPDATE epics SET proposal_id = NULL WHERE proposal_id = $1")
            .bind(proposal_id)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    /// Drop one graduated-epic link for a proposal. Idempotent.
    ///
    /// This is the scoped counterpart to [`Self::unlink_epics`], used by
    /// proposal reconcile when retiring one obsolete epic subtree while leaving
    /// unrelated graduated epics attached to the still-building proposal.
    pub async fn unlink_epic(&self, proposal_id: &str, epic_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("DELETE FROM proposal_epics WHERE proposal_id = $1 AND epic_id = $2")
            .bind(proposal_id)
            .bind(epic_id)
            .execute(self.db.pool())
            .await?;
        self.set_epic_proposal_link(epic_id, None).await?;
        Ok(())
    }

    pub async fn unlink_epic_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        proposal_id: &str,
        epic_id: &str,
    ) -> Result<()> {
        sqlx::query("DELETE FROM proposal_epics WHERE proposal_id = $1 AND epic_id = $2")
            .bind(proposal_id)
            .bind(epic_id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("UPDATE epics SET proposal_id = NULL WHERE id = $1")
            .bind(epic_id)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    /// Record the `epic_breakdown` task created at graduation.
    pub async fn set_breakdown_task(&self, proposal_id: &str, task_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "UPDATE proposals SET build_breakdown_task_id = $1 WHERE id = $2",
            task_id,
            proposal_id
        )
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Park the proposal for a needs-evidence spike: move status back to
    /// `draft`, link the spike task, and record the named feasibility claim.
    /// Emits a `proposal_updated` event.
    pub async fn set_needs_evidence_spike(
        &self,
        proposal_id: &str,
        spike_task_id: &str,
        claim: &str,
    ) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            r#"UPDATE proposals SET
                    status = 'draft',
                    linked_spike_task_id = $1,
                    needs_evidence_claim = $2,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $3"#,
        )
        .bind(spike_task_id)
        .bind(claim)
        .bind(proposal_id)
        .execute(self.db.pool())
        .await?;
        let proposal = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&proposal));
        Ok(proposal)
    }

    /// Structured counterpart of [`Self::set_needs_evidence_spike`]: persists
    /// a typed [`NeedsEvidenceClaim`] as JSON in `needs_evidence_claim` and
    /// sets `linked_spike_task_id` in the same atomic UPDATE. The proposal
    /// status is moved back to `draft` and a `proposal_updated` event fires.
    ///
    /// Callers that only have an opaque string claim should keep using
    /// [`Self::set_needs_evidence_spike`]; both methods write the same columns
    /// and emit the same event.
    pub async fn set_structured_needs_evidence_spike(
        &self,
        proposal_id: &str,
        spike_task_id: &str,
        claim: &NeedsEvidenceClaim,
    ) -> Result<Proposal> {
        let json = serde_json::to_string(claim).map_err(|e| {
            Error::InvalidData(format!("failed to serialize NeedsEvidenceClaim: {e}"))
        })?;
        self.set_needs_evidence_spike(proposal_id, spike_task_id, &json)
            .await
    }

    /// Try to atomically link a spike to the proposal only when no existing
    /// spike is linked (`linked_spike_task_id IS NULL`).
    ///
    /// Returns `Some(proposal)` when the link succeeded, or `None` when
    /// the proposal already has an existing linked spike (the row is
    /// unchanged). This is the race-safe path for concurrent demand
    /// attempts: at most one caller can win the IS NULL guard.
    pub async fn try_set_structured_needs_evidence_spike(
        &self,
        proposal_id: &str,
        spike_task_id: &str,
        claim: &NeedsEvidenceClaim,
    ) -> Result<Option<Proposal>> {
        self.db.ensure_initialized().await?;
        let json = serde_json::to_string(claim).map_err(|e| {
            Error::InvalidData(format!("failed to serialize NeedsEvidenceClaim: {e}"))
        })?;
        let result = sqlx::query(
            r#"UPDATE proposals SET
                    status = 'draft',
                    linked_spike_task_id = $1,
                    needs_evidence_claim = $2,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $3
               AND linked_spike_task_id IS NULL"#,
        )
        .bind(spike_task_id)
        .bind(&json)
        .bind(proposal_id)
        .execute(self.db.pool())
        .await?;

        if result.rows_affected() == 0 {
            // Either the proposal doesn't exist or it already has a linked
            // spike. Read the proposal to distinguish and to return the
            // current state.
            let proposal = self.get(proposal_id).await?;
            return Ok(proposal); // Some with existing spike, or None if deleted
        }

        let proposal = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&proposal));
        Ok(Some(proposal))
    }

    /// Clear the needs-evidence spike linkage after the spike closes and
    /// refinement resumes. Emits a `proposal_updated` event.
    pub async fn clear_needs_evidence_spike(&self, proposal_id: &str) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            r#"UPDATE proposals SET
                    linked_spike_task_id = NULL,
                    needs_evidence_claim = NULL,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $1"#,
        )
        .bind(proposal_id)
        .execute(self.db.pool())
        .await?;
        let proposal = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&proposal));
        Ok(proposal)
    }

    /// Find a proposal that is parked on the given spike task (reverse lookup
    /// from spike task id to proposal). Returns `None` when no proposal is
    /// parked on this spike.
    pub async fn find_by_linked_spike(&self, spike_task_id: &str) -> Result<Option<Proposal>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Proposal,
            r#"SELECT id, short_id, title, body, body_format,
                    acceptance_criteria::text AS "acceptance_criteria!",
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, last_reconciled_revision_seq, pending_reconcile, build_owner_user_id, refinement_owner_user_id, build_frozen, build_breakdown_task_id, linked_spike_task_id, needs_evidence_claim
             FROM proposals WHERE linked_spike_task_id = $1"#,
            spike_task_id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// List proposals that are still parked on a linked needs-evidence spike,
    /// along with the current persisted task status/close_reason for that
    /// linked spike.
    ///
    /// This is a read-only substrate for coordinator recovery. It deliberately
    /// performs no evidence-findings validation and writes no lifecycle rows;
    /// callers that identify a terminal linked spike must hand the returned
    /// fields to `persist_terminal_linked_spike_evidence_lifecycle`.
    pub async fn list_linked_evidence_spike_recovery_candidates(
        &self,
    ) -> Result<Vec<LinkedEvidenceSpikeRecoveryCandidate>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, LinkedEvidenceSpikeRecoveryCandidate>(
            r#"SELECT p.id AS proposal_id,
                      p.linked_spike_task_id AS linked_spike_task_id,
                      t.status AS linked_spike_task_status,
                      t.close_reason AS linked_spike_task_close_reason
               FROM proposals p
               JOIN tasks t ON t.id = p.linked_spike_task_id
               WHERE p.linked_spike_task_id IS NOT NULL
               ORDER BY p.created_at, p.id"#,
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Return the current valid structured findings for a proposal's linked
    /// evidence spike, if the proposal is still parked on that exact spike and
    /// the newest matching `evidence_findings` row matches the stored claim's
    /// round/revision.
    ///
    /// Invalid or stale states are not exceptional for lifecycle callers:
    /// unlinked proposals, wrong spikes, malformed legacy claims, no matching
    /// row, missing metadata, or malformed findings all return `Ok(None)`.
    pub async fn current_evidence_findings_for_linked_spike(
        &self,
        proposal_id: &str,
        spike_task_id: &str,
    ) -> Result<Option<CurrentEvidenceFindings>> {
        self.db.ensure_initialized().await?;

        let Some(proposal) = self.get(proposal_id).await? else {
            return Ok(None);
        };
        if proposal.linked_spike_task_id.as_deref() != Some(spike_task_id) {
            return Ok(None);
        }

        let claim = match NeedsEvidenceClaim::parse_stored(proposal.needs_evidence_claim.as_deref())
        {
            Ok(Some(claim)) => claim,
            Ok(None) | Err(_) => return Ok(None),
        };

        let row = sqlx::query(
            r#"SELECT id, body, body_metadata::text AS body_metadata
               FROM proposal_debate_trail
               WHERE proposal_id = $1
                 AND kind = 'evidence_findings'
                 AND source_task_id = $2
                 AND round = $3
                 AND against_revision_seq = $4
               ORDER BY created_at DESC, updated_at DESC, id DESC
               LIMIT 1"#,
        )
        .bind(proposal_id)
        .bind(spike_task_id)
        .bind(claim.round)
        .bind(claim.against_revision_seq)
        .fetch_optional(self.db.pool())
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };
        let debate_entry_id: String = row.try_get("id")?;
        let debate_entry_body: String = row.try_get("body")?;
        let Some(findings_metadata_json) = row.try_get::<Option<String>, _>("body_metadata")?
        else {
            return Ok(None);
        };

        let findings = match EvidenceFindings::parse_stored(Some(&findings_metadata_json)) {
            Ok(Some(findings)) => findings,
            Ok(None) | Err(_) => return Ok(None),
        };
        if findings.validate().is_err() {
            return Ok(None);
        }

        Ok(Some(CurrentEvidenceFindings {
            proposal_id: proposal_id.to_owned(),
            spike_task_id: spike_task_id.to_owned(),
            round: claim.round,
            against_revision_seq: claim.against_revision_seq,
            debate_entry_id,
            debate_entry_body,
            findings_metadata_json,
            findings,
        }))
    }

    /// Classify a terminal linked evidence spike and persist exactly one receipt
    /// or failure lifecycle event for the current evidence cycle.
    ///
    /// Success is intentionally narrow: the linked spike task must be terminal
    /// with `status = "closed"` and `close_reason = "completed"`, and the
    /// canonical current-findings lookup must return valid findings for the
    /// proposal's current linked spike/round/revision. Every other terminal
    /// state, including completed-without-findings, records failure and leaves
    /// `linked_spike_task_id` / `needs_evidence_claim` untouched for downstream
    /// recovery/resolution code.
    pub async fn persist_terminal_linked_spike_evidence_lifecycle(
        &self,
        proposal_id: &str,
        spike_task_id: &str,
        spike_task_status: &str,
        spike_task_close_reason: Option<&str>,
    ) -> Result<TerminalLinkedEvidenceSpikeOutcome> {
        self.db.ensure_initialized().await?;

        let Some(proposal) = self.get(proposal_id).await? else {
            return Ok(TerminalLinkedEvidenceSpikeOutcome::NotLinked);
        };
        if proposal.linked_spike_task_id.as_deref() != Some(spike_task_id) {
            return Ok(TerminalLinkedEvidenceSpikeOutcome::NotLinked);
        }

        let (judge_task_id, round, against_revision_seq) =
            match NeedsEvidenceClaim::parse_stored(proposal.needs_evidence_claim.as_deref()) {
                Ok(Some(claim)) => (
                    claim.created_by_task_id,
                    claim.round,
                    claim.against_revision_seq,
                ),
                Ok(None) | Err(_) => (String::new(), 0, 0),
            };

        if let Some(existing) = self
            .existing_evidence_terminal_lifecycle_event(proposal_id, spike_task_id)
            .await?
        {
            return Ok(TerminalLinkedEvidenceSpikeOutcome::AlreadyRecorded {
                event_kind: existing,
            });
        }

        if !evidence_spike_task_is_terminal(spike_task_status) {
            return Ok(TerminalLinkedEvidenceSpikeOutcome::NotTerminal);
        }

        if evidence_spike_task_completed_successfully(spike_task_status, spike_task_close_reason) {
            if let Some(findings) = self
                .current_evidence_findings_for_linked_spike(proposal_id, spike_task_id)
                .await?
            {
                self.record_evidence_received_with_findings(&judge_task_id, &findings)
                    .await?;
                return Ok(TerminalLinkedEvidenceSpikeOutcome::EvidenceReceived);
            }

            let reason = "missing_valid_findings".to_owned();
            self.record_evidence_failed(
                proposal_id,
                spike_task_id,
                &judge_task_id,
                round,
                against_revision_seq,
                &reason,
            )
            .await?;
            return Ok(TerminalLinkedEvidenceSpikeOutcome::EvidenceFailed { reason });
        }

        let reason = evidence_spike_failure_reason(spike_task_status, spike_task_close_reason);
        self.record_evidence_failed(
            proposal_id,
            spike_task_id,
            &judge_task_id,
            round,
            against_revision_seq,
            &reason,
        )
        .await?;
        Ok(TerminalLinkedEvidenceSpikeOutcome::EvidenceFailed { reason })
    }

    async fn existing_evidence_terminal_lifecycle_event(
        &self,
        proposal_id: &str,
        spike_task_id: &str,
    ) -> Result<Option<String>> {
        for revision in self.revisions(proposal_id).await?.iter().rev() {
            if revision.event_kind != evidence_lifecycle_kind::EVIDENCE_RECEIVED
                && revision.event_kind != evidence_lifecycle_kind::EVIDENCE_FAILED
            {
                continue;
            }
            if let Ok(Some(meta)) =
                EvidenceLifecycleMetadata::parse_event_metadata(revision.event_metadata.as_deref())
                && meta.proposal_id == proposal_id
                && meta.spike_task_id == spike_task_id
            {
                return Ok(Some(revision.event_kind.clone()));
            }
        }
        Ok(None)
    }

    /// Freeze or un-freeze a build. Frozen builds stay `building` but their
    /// epics' tasks are held out of dispatch (see `build_ready_where`).
    pub async fn set_frozen(&self, proposal_id: &str, frozen: bool) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            r#"UPDATE proposals SET build_frozen = $1,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $2"#,
            frozen,
            proposal_id
        )
        .execute(self.db.pool())
        .await?;
        let proposal = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&proposal));
        Ok(proposal)
    }

    /// Explicit inverse of [`Self::set_building`]: revert an aborted build back
    /// to `approved` so it is immediately re-graduate-able. Clears the build
    /// owner, the breakdown-task link, and any freeze. (Epics are unlinked
    /// separately via [`Self::unlink_epics`].)
    pub async fn revert_to_approved(&self, proposal_id: &str) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            r#"UPDATE proposals SET status = 'approved', build_owner_user_id = NULL,
                    build_breakdown_task_id = NULL, build_frozen = false,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $1"#,
            proposal_id
        )
        .execute(self.db.pool())
        .await?;
        let proposal = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&proposal));
        Ok(proposal)
    }

    pub async fn revert_to_approved_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        proposal_id: &str,
    ) -> Result<()> {
        sqlx::query("UPDATE proposals SET status = 'approved', build_owner_user_id = NULL, build_breakdown_task_id = NULL, build_frozen = false WHERE id = $1").bind(proposal_id).execute(&mut **tx).await?;
        Ok(())
    }

    /// Mark a proposal as building, recording the build owner.
    pub async fn set_building(&self, proposal_id: &str, owner_user_id: &str) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            r#"UPDATE proposals SET status = 'building', build_owner_user_id = $1,
                    last_reconciled_revision_seq = latest_revision_seq,
                    pending_reconcile = false,
                    reconciled_at = now(),
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $2"#,
            owner_user_id,
            proposal_id
        )
        .execute(self.db.pool())
        .await?;
        let proposal = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&proposal));
        Ok(proposal)
    }

    /// Mark the current build as reconciled to the proposal's head revision and
    /// stamp each graduated epic. This is the successful reconcile write site;
    /// callers that apply a reconcile should use this instead of updating
    /// `last_reconciled_revision_seq` directly so per-epic badges stay in sync.
    pub async fn mark_reconciled(&self, proposal_id: &str) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        let proposal = self.get_required(proposal_id).await?;
        let revision_seq = proposal.latest_revision_seq;
        let epics = self.graduated_epics(proposal_id).await?;
        for (epic_id, _) in epics {
            self.record_epic_reconciliation(proposal_id, &epic_id, revision_seq)
                .await?;
        }
        sqlx::query(
            r#"UPDATE proposals SET last_reconciled_revision_seq = $1,
                    pending_reconcile = false,
                    reconciled_at = now(),
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
               WHERE id = $2"#,
        )
        .bind(revision_seq)
        .bind(proposal_id)
        .execute(self.db.pool())
        .await?;
        let proposal = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&proposal));
        Ok(proposal)
    }

    /// The proposal a graduated epic belongs to, if any. Reverse of
    /// [`Self::link_epic`] — used by the coordinator to decide whether closing
    /// an epic completes its parent proposal.
    pub async fn proposal_for_epic(&self, epic_id: &str) -> Result<Option<Proposal>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query!(
            "SELECT proposal_id FROM proposal_epics WHERE epic_id = $1 LIMIT 1",
            epic_id
        )
        .fetch_optional(self.db.pool())
        .await?;
        match row {
            Some(r) => self.get(&r.proposal_id).await,
            None => Ok(None),
        }
    }

    /// The proposal whose graduation/breakdown Planner task is `task_id`, if any.
    ///
    /// Initial proposal-decomposition sessions run on the proposal's
    /// `build_breakdown_task_id` before any child epic exists, so they cannot be
    /// reached through [`Self::proposal_for_epic`]. This reverse lookup lets
    /// session extraction attach planner-read provenance notes to the proposal
    /// as soon as that local task/session data is available.
    pub async fn proposal_for_breakdown_task(&self, task_id: &str) -> Result<Option<Proposal>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query_as::<_, (String,)>(
            "SELECT id FROM proposals WHERE build_breakdown_task_id = $1 LIMIT 1",
        )
        .bind(task_id)
        .fetch_optional(self.db.pool())
        .await?;
        match row {
            Some((id,)) => self.get(&id).await,
            None => Ok(None),
        }
    }

    /// `true` when the proposal has graduated at least one epic AND every
    /// graduated epic is closed. `false` for a proposal with no graduated
    /// epics (nothing has been built yet, so there is nothing to complete).
    pub async fn all_graduated_epics_closed(&self, proposal_id: &str) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query!(
            r#"SELECT
                    COUNT(*) AS "total!: i64",
                    COUNT(*) FILTER (WHERE e.status <> 'closed') AS "open!: i64"
               FROM proposal_epics pe
               JOIN epics e ON e.id = pe.epic_id
               WHERE pe.proposal_id = $1"#,
            proposal_id
        )
        .fetch_one(self.db.pool())
        .await?;
        Ok(row.total > 0 && row.open == 0)
    }

    /// Mark a proposal `done` (terminal). Stamps `closed_at` if not already set.
    /// Used by the Planner's `proposal_complete` tool after reviewing the
    /// finished build. Completing is also a successful reconcile: stamp every
    /// graduated epic at the proposal head and clear proposal-level drift before
    /// moving to the terminal state.
    /// Force a proposal's `status` directly, bypassing the revision/audit
    /// path. Intended for tests and migrations that need to simulate a given
    /// lifecycle state (e.g. legacy `approved` data) without going through the
    /// full `update` contract.
    pub async fn set_status(&self, proposal_id: &str, status: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            r#"UPDATE proposals SET status = $2,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $1"#,
        )
        .bind(proposal_id)
        .bind(status)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Advance a `draft` proposal to `in_review` when the tribunal converges
    /// and parks it awaiting human review. Idempotent and status-scoped: only
    /// `draft → in_review` transitions; any other status is left untouched.
    ///
    /// Emits a `status_change` revision event (matching the sign-off-driven
    /// promotion path) so the transition appears in revision history, and
    /// publishes a `proposal_updated` event. Returns `true` when a transition
    /// occurred.
    pub async fn advance_draft_to_in_review(&self, proposal_id: &str) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let Some(proposal) = self.get(proposal_id).await? else {
            return Ok(false);
        };
        if proposal.status != "draft" {
            return Ok(false);
        }
        let acceptance_criteria: serde_json::Value =
            serde_json::from_str(&proposal.acceptance_criteria).unwrap_or(serde_json::json!([]));
        let mut tx = self.db.pool().begin().await?;
        let changed = sqlx::query(
            r#"UPDATE proposals SET status = 'in_review',
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $1 AND status = 'draft'"#,
        )
        .bind(proposal_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if changed == 0 {
            return Ok(false);
        }
        self.insert_revision_checked(
            &mut tx,
            ProposalRevisionSnapshot {
                proposal_id,
                seq: proposal.latest_revision_seq,
                title: &proposal.title,
                body: &proposal.body,
                body_format: &proposal.body_format,
                acceptance_criteria: &acceptance_criteria,
                edited_by: None,
                event_metadata: None,
                event_kind: "status_change",
                status_from: Some("draft"),
                status_to: Some("in_review"),
            },
        )
        .await?;
        tx.commit().await?;
        let updated = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&updated));
        Ok(true)
    }

    pub async fn set_done(&self, proposal_id: &str) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        let proposal = self.get_required(proposal_id).await?;
        let revision_seq = proposal.latest_revision_seq;
        let epics = self.graduated_epics(proposal_id).await?;
        for (epic_id, _) in epics {
            self.record_epic_reconciliation(proposal_id, &epic_id, revision_seq)
                .await?;
        }
        sqlx::query(
            r#"UPDATE proposals SET status = 'done',
                    last_reconciled_revision_seq = latest_revision_seq,
                    pending_reconcile = false,
                    reconciled_at = now(),
                    closed_at = COALESCE(closed_at, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')),
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $1"#,
        )
        .bind(proposal_id)
        .execute(self.db.pool())
        .await?;
        let proposal = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&proposal));
        Ok(proposal)
    }

    /// Overwrite the acceptance-criteria JSON in place — a lightweight status
    /// annotation (the Planner ticking `met` flags as epics land), NOT a spec
    /// edit. Unlike [`Self::update`], this does NOT bump `latest_revision_seq`
    /// or clear sign-offs; `ac_json` must be a JSON array string of
    /// `{criterion, met}` objects (callers merge against the current criteria
    /// to preserve the `criterion` text).
    pub async fn set_acceptance_criteria(
        &self,
        proposal_id: &str,
        ac_json: &str,
    ) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        let acceptance_criteria: serde_json::Value =
            serde_json::from_str(ac_json).map_err(|e| {
                Error::InvalidData(format!(
                    "invalid json for proposals.acceptance_criteria: {e}"
                ))
            })?;
        sqlx::query!(
            r#"UPDATE proposals SET acceptance_criteria = $1,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $2"#,
            acceptance_criteria,
            proposal_id
        )
        .execute(self.db.pool())
        .await?;
        let proposal = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&proposal));
        Ok(proposal)
    }

    /// Apply Planner-authored acceptance-criteria amendments as real spec edits.
    ///
    /// Unlike [`Self::set_acceptance_criteria`], this bumps the proposal head
    /// revision and stamps the structured change list on the new revision's
    /// `event_metadata` (`kind = "ac_amendment"`) so it surfaces in the History
    /// stream. Unlike [`Self::update`], it intentionally retains existing
    /// sign-offs and does not demote approved proposals; the history audit entry
    /// is the mechanism for humans to object.
    pub async fn amend_acceptance_criteria(
        &self,
        proposal_id: &str,
        amendments: &[ProposalAcceptanceCriteriaAmendment<'_>],
        reason: &str,
    ) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(Error::InvalidData(
                "acceptance-criteria amendment reason is required".to_owned(),
            ));
        }
        if amendments.is_empty() {
            return Err(Error::InvalidData(
                "at least one acceptance-criteria amendment is required".to_owned(),
            ));
        }

        let current = self
            .get(proposal_id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("proposal not found: {proposal_id}")))?;
        let old_revision_seq = current.latest_revision_seq;
        let next_revision_seq = old_revision_seq + 1;
        let mut criteria = serde_json::from_str::<serde_json::Value>(&current.acceptance_criteria)
            .map_err(|e| {
                Error::InvalidData(format!(
                    "invalid json in proposals.acceptance_criteria: {e}"
                ))
            })?
            .as_array()
            .cloned()
            .ok_or_else(|| {
                Error::InvalidData("proposals.acceptance_criteria must be a JSON array".to_owned())
            })?;

        let mut audit_entries = Vec::with_capacity(amendments.len());
        for amendment in amendments {
            match amendment {
                ProposalAcceptanceCriteriaAmendment::Rewrite { index, criterion } => {
                    let criterion = criterion.trim();
                    if criterion.is_empty() {
                        return Err(Error::InvalidData(
                            "rewrite acceptance-criteria text is required".to_owned(),
                        ));
                    }
                    let old = criteria.get(*index).cloned().ok_or_else(|| {
                        Error::InvalidData(format!(
                            "acceptance-criteria index {index} out of range"
                        ))
                    })?;
                    let mut new = old.clone();
                    match &mut new {
                        serde_json::Value::Object(obj) => {
                            obj.insert(
                                "criterion".to_owned(),
                                serde_json::Value::String(criterion.to_owned()),
                            );
                        }
                        _ => new = serde_json::Value::String(criterion.to_owned()),
                    }
                    criteria[*index] = new.clone();
                    audit_entries.push(ProposalAcceptanceCriteriaAuditEntry {
                        operation: "rewrite",
                        index: *index,
                        old_criterion: old,
                        new_criterion: new,
                    });
                }
                ProposalAcceptanceCriteriaAmendment::Drop { index } => {
                    if *index >= criteria.len() {
                        return Err(Error::InvalidData(format!(
                            "acceptance-criteria index {index} out of range"
                        )));
                    }
                    let old = criteria.remove(*index);
                    audit_entries.push(ProposalAcceptanceCriteriaAuditEntry {
                        operation: "drop",
                        index: *index,
                        old_criterion: old,
                        new_criterion: serde_json::json!({"dropped": true}),
                    });
                }
                ProposalAcceptanceCriteriaAmendment::Waive { index } => {
                    let old = criteria.get(*index).cloned().ok_or_else(|| {
                        Error::InvalidData(format!(
                            "acceptance-criteria index {index} out of range"
                        ))
                    })?;
                    let mut new = old.clone();
                    match &mut new {
                        serde_json::Value::Object(obj) => {
                            obj.insert("waived".to_owned(), serde_json::Value::Bool(true));
                        }
                        _ => {
                            new = serde_json::json!({
                                "criterion": old,
                                "waived": true
                            });
                        }
                    }
                    criteria[*index] = new.clone();
                    audit_entries.push(ProposalAcceptanceCriteriaAuditEntry {
                        operation: "waive",
                        index: *index,
                        old_criterion: old,
                        new_criterion: new,
                    });
                }
            }
        }

        let acceptance_criteria = serde_json::Value::Array(criteria);
        let mut tx = self.db.pool().begin().await?;
        sqlx::query(
            r#"UPDATE proposals SET acceptance_criteria = $1, latest_revision_seq = $2,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $3"#,
        )
        .bind(&acceptance_criteria)
        .bind(next_revision_seq)
        .bind(proposal_id)
        .execute(&mut *tx)
        .await?;
        let editor = djinn_core::auth_context::current_user_id();
        // The structured change list rides on the spec revision itself, under
        // `event_metadata.kind = "ac_amendment"`. This keeps the amendment audit
        // in the History stream (where it belongs) rather than the reviewer
        // FEEDBACK pane, while the revision's `event_kind` stays `spec_revision`
        // so existing seq/spec-revision lookups keep working.
        let amendments_json = serde_json::to_value(&audit_entries)
            .map_err(|e| Error::InvalidData(format!("failed to encode amendment audit: {e}")))?;
        let event_metadata = serde_json::json!({
            "kind": "ac_amendment",
            "reason": reason,
            "amendments": amendments_json,
        });

        self.insert_revision_checked(
            &mut tx,
            ProposalRevisionSnapshot {
                proposal_id,
                seq: next_revision_seq,
                title: &current.title,
                body: &current.body,
                body_format: &current.body_format,
                acceptance_criteria: &acceptance_criteria,
                edited_by: editor.as_deref(),
                event_metadata: Some(&event_metadata),
                event_kind: "spec_revision",
                status_from: None,
                status_to: None,
            },
        )
        .await?;
        tx.commit().await?;

        let proposal = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&proposal));
        Ok(proposal)
    }

    /// `building` proposals whose build has fully drained — at least one
    /// graduated epic, and every graduated epic closed — for the coordinator's
    /// backfill sweep. Catches proposals whose epics closed before the review
    /// rule existed (or whose `epic.updated` event was missed), which would
    /// otherwise sit in `building` forever with no closeout review.
    pub async fn drained_building_proposals(&self) -> Result<Vec<Proposal>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Proposal,
            r#"SELECT id, short_id, title, body, body_format,
                    acceptance_criteria::text AS "acceptance_criteria!",
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, last_reconciled_revision_seq, pending_reconcile, build_owner_user_id, refinement_owner_user_id, build_frozen, build_breakdown_task_id, linked_spike_task_id, needs_evidence_claim
             FROM proposals p
             WHERE p.status = 'building'
               AND EXISTS (SELECT 1 FROM proposal_epics pe WHERE pe.proposal_id = p.id)
               AND NOT EXISTS (
                   SELECT 1 FROM proposal_epics pe
                   JOIN epics e ON e.id = pe.epic_id
                   WHERE pe.proposal_id = p.id AND e.status <> 'closed'
               )
             ORDER BY p.updated_at"#
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// `building` proposals whose proposal head has drifted ahead of the
    /// revision stamped as reconciled into the graduated build. Used by the
    /// coordinator's reconcile backstop sweep to recover missed
    /// `proposal.updated` events.
    pub async fn drift_building_proposals(&self) -> Result<Vec<Proposal>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Proposal>(
            r#"SELECT id, short_id, title, body, body_format,
                    acceptance_criteria::text AS acceptance_criteria,
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, last_reconciled_revision_seq, pending_reconcile, build_owner_user_id, refinement_owner_user_id, build_frozen, build_breakdown_task_id, linked_spike_task_id, needs_evidence_claim
             FROM proposals p
             WHERE p.status = 'building'
               AND (
                   p.pending_reconcile = true
                   OR p.latest_revision_seq > COALESCE(p.last_reconciled_revision_seq, 0)
               )
             ORDER BY p.updated_at"#
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    // ── Internal helpers ─────────────────────────────────────────────────────

    async fn get_required(&self, id: &str) -> Result<Proposal> {
        self.get(id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("proposal not found after write: {id}")))
    }

    async fn get_feedback_required(&self, id: &str) -> Result<ProposalFeedback> {
        self.get_feedback(id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("feedback not found after write: {id}")))
    }

    async fn get_debate_trail_entry_required(&self, id: &str) -> Result<ProposalDebateTrail> {
        self.get_debate_trail_entry(id).await?.ok_or_else(|| {
            Error::InvalidData(format!("debate trail entry not found after write: {id}"))
        })
    }

    /// Generate a globally-unique 4-char base36 short id for proposals.
    async fn generate_short_id(&self, seed_id: &str) -> Result<String> {
        self.db.ensure_initialized().await?;
        let seed = uuid::Uuid::parse_str(seed_id).map_err(|e| Error::InvalidData(e.to_string()))?;
        let candidate = short_id_from_uuid(&seed);
        if !short_id_exists(self.db.pool(), &candidate).await? {
            return Ok(candidate);
        }
        for _ in 0..16 {
            let candidate = short_id_from_uuid(&uuid::Uuid::now_v7());
            if !short_id_exists(self.db.pool(), &candidate).await? {
                return Ok(candidate);
            }
        }
        Err(Error::InvalidData(
            "short_id collision after 16 retries".into(),
        ))
    }

    /// Full-text search across proposals using the `search_vector` tsvector
    /// column (Postgres) or a LIKE fallback (SQLite).
    ///
    /// Returns proposals ranked by BM25/ts_rank, filtered to exclude archived
    /// and rejected proposals. Each result includes an HTML snippet with
    /// `<b>...</b>` highlights around matched terms.
    pub async fn search_proposals(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ProposalSearchResult>> {
        self.db.ensure_initialized().await?;

        let backend = match self.db.backend_capabilities().lexical_search {
            crate::database::NoteSearchBackend::SqliteFts5 => LexicalSearchBackend::SqliteFts5,
            crate::database::NoteSearchBackend::PostgresTsvector => {
                LexicalSearchBackend::PostgresTsvector
            }
        };

        match backend {
            LexicalSearchBackend::PostgresTsvector => {
                self.search_proposals_postgres(query, limit).await
            }
            LexicalSearchBackend::SqliteFts5 => self.search_proposals_sqlite(query, limit).await,
        }
    }

    /// Postgres path: tsvector GIN index with ts_rank + ts_headline.
    async fn search_proposals_postgres(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ProposalSearchResult>> {
        let sanitized = match sanitize_postgres_tsquery(query) {
            Some(q) => q,
            None => return Ok(vec![]),
        };

        // NOTE: dynamic SQL (backend-specific FTS query) — compile-time check not possible
        let sql = r#"SELECT id, short_id, title, status,
                ts_headline('english', body, to_tsquery('english', $1),
                            'StartSel=<b>, StopSel=</b>, MaxFragments=2, MaxWords=40, MinWords=20')
                    AS snippet,
                ts_rank(search_vector, to_tsquery('english', $1))::float8 AS score
             FROM proposals
             WHERE search_vector @@ to_tsquery('english', $1)
               AND status NOT IN ('archived', 'rejected')
             ORDER BY score DESC, id ASC
             LIMIT $2"#;

        let rows =
            sqlx::query_as::<sqlx::Postgres, (String, String, String, String, String, f64)>(sql)
                .bind(&sanitized)
                .bind(limit as i64)
                .fetch_all(self.db.pool())
                .await?;

        Ok(rows
            .into_iter()
            .map(
                |(id, short_id, title, status, snippet, score)| ProposalSearchResult {
                    id,
                    short_id,
                    title,
                    status,
                    snippet,
                    score,
                },
            )
            .collect())
    }

    /// SQLite fallback: LIKE queries against title + body + acceptance_criteria.
    async fn search_proposals_sqlite(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ProposalSearchResult>> {
        let tokens: Vec<&str> = query
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|t| !t.is_empty())
            .take(12)
            .collect();

        if tokens.is_empty() {
            return Ok(vec![]);
        }

        // Build LIKE conditions — each token must appear somewhere in the
        // concatenated searchable text.
        let mut conditions = Vec::new();
        for i in 0..tokens.len() {
            conditions.push(format!("(title || ' ' || body || ' ' || COALESCE(acceptance_criteria::text, '')) ILIKE ${}", i + 3));
        }
        let where_clause = conditions.join(" AND ");

        let sql = format!(
            r#"SELECT id, short_id, title, status,
                    substr(body, 1, 200) AS snippet,
                    1.0 AS score
             FROM proposals
             WHERE {}
               AND status NOT IN ('archived', 'rejected')
             ORDER BY updated_at DESC
             LIMIT $2"#,
            where_clause
        );

        let mut q =
            sqlx::query_as::<sqlx::Postgres, (String, String, String, String, String, f64)>(&sql);
        for token in &tokens {
            let pattern = format!("%{}%", token);
            q = q.bind(pattern);
        }
        q = q.bind(limit as i64);

        let rows = q.fetch_all(self.db.pool()).await?;

        Ok(rows
            .into_iter()
            .map(
                |(id, short_id, title, status, snippet, score)| ProposalSearchResult {
                    id,
                    short_id,
                    title,
                    status,
                    snippet,
                    score,
                },
            )
            .collect())
    }

    // ── Composed gate helpers (task cuzf) ─────────────────────────────

    /// List unresolved blocking debate-trail entries for a proposal.
    ///
    /// An entry is "unresolved" when `resolved_at IS NULL` OR
    /// (`resolved_at IS NOT NULL AND reopened_at IS NOT NULL`).
    /// Only `blocking = true` entries are returned.
    ///
    /// Judge `kind = 'verdict'` rows are excluded: verdicts gate through their
    /// own channel (`latest_judge_verdict` → `judge_needs_work`), where a later
    /// approve verdict supersedes an earlier reject. Nothing ever resolves the
    /// stale reject-verdict rows, so counting them here double-counts a signal
    /// that can never be cleared (see gate-verdict-supersession fix).
    pub async fn list_unresolved_blocking_debate_entries(
        &self,
        proposal_id: &str,
    ) -> Result<Vec<ProposalDebateTrail>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, ProposalDebateTrail>(
            r#"SELECT id, proposal_id, kind, body, blocking, agent_role, author_kind,
                    author_user_id, author_model, source_task_id,
                    against_revision_seq, round,
                    body_metadata::text AS body_metadata,
                    resolved_at, resolved_by_user_id,
                    reopened_at, reopened_by_user_id,
                    created_at, updated_at
             FROM proposal_debate_trail
             WHERE proposal_id = $1
               AND blocking = true
               AND kind <> 'verdict'
               AND (resolved_at IS NULL
                    OR (resolved_at IS NOT NULL AND reopened_at IS NOT NULL))
             ORDER BY round, created_at"#,
        )
        .bind(proposal_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Return the latest judge verdict entry for a proposal.
    ///
    /// Looks for debate-trail entries with `kind = 'verdict'` and
    /// `agent_role = 'judge'`, ordered newest-first.
    pub async fn latest_judge_verdict(
        &self,
        proposal_id: &str,
    ) -> Result<Option<ProposalDebateTrail>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, ProposalDebateTrail>(
            r#"SELECT id, proposal_id, kind, body, blocking, agent_role, author_kind,
                    author_user_id, author_model, source_task_id,
                    against_revision_seq, round,
                    body_metadata::text AS body_metadata,
                    resolved_at, resolved_by_user_id,
                    reopened_at, reopened_by_user_id,
                    created_at, updated_at
             FROM proposal_debate_trail
             WHERE proposal_id = $1
               AND kind = 'verdict'
               AND agent_role = 'judge'
             ORDER BY created_at DESC, id DESC
             LIMIT 1"#,
        )
        .bind(proposal_id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Check whether a proposal is currently parked on an open
    /// needs-evidence spike.
    pub async fn has_open_needs_evidence_spike(&self, proposal_id: &str) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let count: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) AS "n!: i64" FROM proposals
             WHERE id = $1
               AND linked_spike_task_id IS NOT NULL"#,
        )
        .bind(proposal_id)
        .fetch_one(self.db.pool())
        .await?;
        Ok(count > 0)
    }

    /// Batched tribunal/readiness raw facts for a page of proposals.
    ///
    /// One grouped/window query each for: unresolved blocking non-verdict debate
    /// entries, latest judge verdict, highest debate round, refinement lifecycle
    /// events, and target counts — keyed by proposal id. This deliberately does
    /// NOT call the per-proposal `build_gate_status` / `build_refinement_status`
    /// helpers (each of which issues several queries); it runs a fixed handful of
    /// queries for the whole page so the proposals list can render tribunal/gate
    /// chips cheaply. Callers derive `dor_ready` / `gate_ready` upstream.
    ///
    /// Proposals with no rows in a given table simply get the default (`0` /
    /// `false` / `None`) for that fact. Every id in `ids` is present in the map.
    pub async fn list_summaries(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, ProposalListSummaryRow>> {
        self.db.ensure_initialized().await?;
        let mut out: HashMap<String, ProposalListSummaryRow> = HashMap::new();
        if ids.is_empty() {
            return Ok(out);
        }
        for id in ids {
            out.entry(id.clone()).or_default();
        }

        // 1. Unresolved blocking non-verdict debate objections per proposal.
        //    Mirrors `list_unresolved_blocking_debate_entries` (blocking +
        //    unresolved-or-reopened) but excludes judge verdicts
        //    (`kind <> 'verdict'`) so the count reflects only outstanding
        //    objections, not the verdict row itself.
        let blocking_rows = sqlx::query_as::<_, (String, i64)>(
            r#"SELECT proposal_id, COUNT(*) AS n
               FROM proposal_debate_trail
               WHERE proposal_id = ANY($1)
                 AND blocking = true
                 AND kind <> 'verdict'
                 AND (resolved_at IS NULL
                      OR (resolved_at IS NOT NULL AND reopened_at IS NOT NULL))
               GROUP BY proposal_id"#,
        )
        .bind(ids)
        .fetch_all(self.db.pool())
        .await?;
        for (pid, n) in blocking_rows {
            if let Some(row) = out.get_mut(&pid) {
                row.unresolved_blocking_count = n;
            }
        }

        // 2. Latest judge verdict body per proposal (window via DISTINCT ON).
        let verdict_rows = sqlx::query_as::<_, (String, String)>(
            r#"SELECT DISTINCT ON (proposal_id) proposal_id, body
               FROM proposal_debate_trail
               WHERE proposal_id = ANY($1)
                 AND kind = 'verdict'
                 AND agent_role = 'judge'
               ORDER BY proposal_id, created_at DESC, id DESC"#,
        )
        .bind(ids)
        .fetch_all(self.db.pool())
        .await?;
        for (pid, body) in verdict_rows {
            if let Some(row) = out.get_mut(&pid) {
                row.latest_judge_verdict_body = Some(body);
            }
        }

        // 3. Highest debate round per proposal.
        let round_rows = sqlx::query_as::<_, (String, Option<i32>)>(
            r#"SELECT proposal_id, MAX(round) AS max_round
               FROM proposal_debate_trail
               WHERE proposal_id = ANY($1)
               GROUP BY proposal_id"#,
        )
        .bind(ids)
        .fetch_all(self.db.pool())
        .await?;
        for (pid, max_round) in round_rows {
            if let Some(row) = out.get_mut(&pid) {
                row.current_round = max_round.unwrap_or(0);
            }
        }

        // 4. Refinement lifecycle events → active / awaiting_review. Mirrors the
        //    created_at ordering logic in `build_refinement_status`: the latest
        //    refinement_start defines the active run; a stop after it ends it; an
        //    awaiting_review after the start (and after any stop) parks it.
        let refine_rows = sqlx::query_as::<_, (String, String, String)>(
            r#"SELECT proposal_id, event_kind, created_at
               FROM proposal_revisions
               WHERE proposal_id = ANY($1)
                 AND event_kind IN
                   ('refinement_start', 'refinement_stop', 'refinement_awaiting_review')
               ORDER BY proposal_id, created_at, id"#,
        )
        .bind(ids)
        .fetch_all(self.db.pool())
        .await?;
        // Rows ascend by created_at, so the last write per (proposal, kind) is
        // the latest occurrence — matching `revisions().iter().rev().find(..)`.
        let mut latest_start: HashMap<String, String> = HashMap::new();
        let mut latest_stop: HashMap<String, String> = HashMap::new();
        let mut latest_awaiting: HashMap<String, String> = HashMap::new();
        for (pid, kind, created_at) in refine_rows {
            match kind.as_str() {
                "refinement_start" => {
                    latest_start.insert(pid, created_at);
                }
                "refinement_stop" => {
                    latest_stop.insert(pid, created_at);
                }
                "refinement_awaiting_review" => {
                    latest_awaiting.insert(pid, created_at);
                }
                _ => {}
            }
        }
        for (pid, row) in out.iter_mut() {
            let Some(start) = latest_start.get(pid) else {
                continue;
            };
            let stop = latest_stop.get(pid);
            let awaiting = latest_awaiting.get(pid);
            // Active when no stop landed after the latest start.
            row.refinement_active = match stop {
                Some(stop) => stop <= start,
                None => true,
            };
            row.awaiting_review = match (awaiting, stop) {
                (Some(aw), Some(stop)) => start <= aw && stop < aw,
                (Some(aw), None) => start <= aw,
                _ => false,
            };
        }

        // 5. Target counts per proposal (feeds the DoR target-count check).
        let target_rows = sqlx::query_as::<_, (String, i64)>(
            r#"SELECT proposal_id, COUNT(*) AS n
               FROM proposal_targets
               WHERE proposal_id = ANY($1)
               GROUP BY proposal_id"#,
        )
        .bind(ids)
        .fetch_all(self.db.pool())
        .await?;
        for (pid, n) in target_rows {
            if let Some(row) = out.get_mut(&pid) {
                row.target_count = n;
            }
        }

        Ok(out)
    }
}

pub fn evidence_spike_task_is_terminal(status: &str) -> bool {
    matches!(
        status,
        "closed" | "failed" | "cancelled" | "canceled" | "done" | "rejected" | "archived"
    )
}

fn evidence_spike_task_completed_successfully(status: &str, close_reason: Option<&str>) -> bool {
    status == "closed" && close_reason == Some(djinn_core::models::task::CLOSE_REASON_COMPLETED)
}

fn evidence_spike_failure_reason(status: &str, close_reason: Option<&str>) -> String {
    let raw = close_reason.unwrap_or(status).trim();
    match raw {
        "force_closed" | "force_close" => "spike_force_closed".to_owned(),
        "cancelled" | "canceled" => "spike_cancelled".to_owned(),
        "failed" | "error" | "errored" => "spike_errored".to_owned(),
        "" => "spike_unsuccessful".to_owned(),
        other => format!(
            "spike_unsuccessful_{}",
            sanitize_evidence_failure_reason(other)
        ),
    }
}

fn sanitize_evidence_failure_reason(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').to_owned()
}

// ── Short ID helpers ─────────────────────────────────────────────────────────

fn short_id_from_uuid(id: &uuid::Uuid) -> String {
    let bytes = id.as_bytes();
    let n = u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    encode_base36(n % 1_679_616) // 36^4
}

fn encode_base36(mut n: u32) -> String {
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = [b'0'; 4];
    for i in (0..4).rev() {
        buf[i] = CHARS[(n % 36) as usize];
        n /= 36;
    }
    String::from_utf8(buf.to_vec()).unwrap()
}

/// Global uniqueness check against the `proposals` table only (short_ids are
/// NOT per-project for proposals).
async fn short_id_exists(pool: &sqlx::PgPool, short_id: &str) -> Result<bool> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM proposals WHERE short_id = $1) AS "exists!: bool""#,
        short_id
    )
    .fetch_one(pool)
    .await?)
}

fn parse_memory_refs_json(memory_refs_json: &str) -> Result<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(memory_refs_json).map_err(|e| {
        Error::InvalidData(format!("invalid json for proposal memory_refs walk: {e}"))
    })?;
    Ok(value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default())
}

// ── Dynamic query helpers ────────────────────────────────────────────────────

fn proposal_build_where(
    status: &Option<String>,
    text: &Option<String>,
    author_user_id: &Option<String>,
    target_project_id: &Option<String>,
) -> (String, Vec<SqlParam>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<SqlParam> = Vec::new();

    if let Some(s) = status {
        let ph = format!("${}", params.len() + 1);
        clauses.push(format!("status = {ph}"));
        params.push(SqlParam::Text(s.clone()));
    }
    if let Some(a) = author_user_id {
        let ph = format!("${}", params.len() + 1);
        clauses.push(format!("author_user_id = {ph}"));
        params.push(SqlParam::Text(a.clone()));
    }
    if let Some(proj) = target_project_id {
        let ph = format!("${}", params.len() + 1);
        clauses.push(format!(
            "EXISTS (SELECT 1 FROM proposal_targets pt WHERE pt.proposal_id = proposals.id AND pt.project_id = {ph})"
        ));
        params.push(SqlParam::Text(proj.clone()));
    }
    if let Some(t) = text {
        let ph_a = format!("${}", params.len() + 1);
        let ph_b = format!("${}", params.len() + 2);
        let ph_c = format!("${}", params.len() + 3);
        clauses.push(format!(
            "(title LIKE {ph_a} OR body LIKE {ph_b} OR short_id LIKE {ph_c})"
        ));
        let pattern = format!("%{t}%");
        params.push(SqlParam::Text(pattern.clone()));
        params.push(SqlParam::Text(pattern.clone()));
        params.push(SqlParam::Text(pattern));
    }

    let where_sql = if clauses.is_empty() {
        "1=1".to_owned()
    } else {
        clauses.join(" AND ")
    };
    (where_sql, params)
}

fn proposal_sort_to_sql(sort: &str) -> &'static str {
    match sort {
        "created" => "created_at ASC",
        "created_desc" => "created_at DESC",
        "updated" => "updated_at ASC",
        "updated_desc" => "updated_at DESC",
        _ => "created_at DESC",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use djinn_core::events::{DjinnEventEnvelope, EventBus};

    use super::*;

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    fn capturing_bus() -> (EventBus, Arc<Mutex<Vec<DjinnEventEnvelope>>>) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let bus = EventBus::new({
            let captured = captured.clone();
            move |ev| captured.lock().unwrap().push(ev)
        });
        (bus, captured)
    }

    async fn insert_project(db: &Database, owner: &str) -> String {
        // The raw insert bypasses the repository's `ensure_initialized`, so
        // clone the per-test DB from the template explicitly before using it
        // (matters when this helper is the first DB op in a test).
        db.ensure_initialized().await.unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
            id,
            owner,
            owner,
            format!("repo-{}", &id.replace('-', "")[..31])
        )
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    fn create_input<'a>(title: &'a str) -> ProposalCreateInput<'a> {
        ProposalCreateInput {
            title,
            body: "",
            acceptance_criteria: None,
            status: None,
            body_format: None,
        }
    }

    fn create_input_with_ac<'a>(
        title: &'a str,
        body: &'a str,
        ac: &'a str,
    ) -> ProposalCreateInput<'a> {
        ProposalCreateInput {
            title,
            body,
            acceptance_criteria: Some(ac),
            status: None,
            body_format: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_defaults_and_short_id() {
        let (bus, captured) = capturing_bus();
        let repo = ProposalRepository::new(test_db(), bus);
        let p = repo.create(create_input("My Proposal")).await.unwrap();
        assert_eq!(p.title, "My Proposal");
        assert_eq!(p.status, "draft");
        assert_eq!(p.short_id.len(), 4);
        assert_eq!(p.acceptance_criteria, "[]");

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "proposal");
        assert_eq!(events[0].action, "created");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lint_for_revision_returns_the_complete_valid_mdx_cache_entry() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "MDX lint cache",
                body: "<Callout id=\"note\">content</Callout>",
                acceptance_criteria: None,
                status: None,
                body_format: Some("mdx"),
            })
            .await
            .unwrap();
        let revision = repo.revisions(&proposal.id).await.unwrap().remove(0);
        let cached_json: serde_json::Value = sqlx::query_scalar(
            "SELECT result_json FROM proposal_revision_lint_results WHERE proposal_id = $1 AND revision_seq = $2 AND linter_version = $3",
        )
        .bind(&revision.proposal_id)
        .bind(revision.seq)
        .bind(djinn_spec_lint::SpecLintResultV1::LINTER_VERSION)
        .fetch_one(db.pool())
        .await
        .unwrap();
        let cached: djinn_spec_lint::SpecLintResultV1 =
            serde_json::from_value(cached_json).unwrap();

        let result = repo.lint_for_revision(&revision).await.unwrap();
        assert_eq!(
            result, cached,
            "a valid cache entry must be returned intact"
        );
        assert_eq!(result.body_format, djinn_spec_lint::BodyFormat::Mdx);
        assert!(result.skipped_tiers.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lint_for_revision_recomputes_all_invalid_cache_cases_deterministically() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let expected = djinn_spec_lint::lint(
            "",
            djinn_spec_lint::BodyFormat::Markdown,
            "1970-01-01T00:00:00.000Z",
        );

        for (title, mutation) in [
            (
                "missing",
                "DELETE FROM proposal_revision_lint_results WHERE proposal_id = $1 AND revision_seq = $2",
            ),
            (
                "old version",
                "UPDATE proposal_revision_lint_results SET linter_version = 'v0' WHERE proposal_id = $1 AND revision_seq = $2",
            ),
            (
                "future version",
                "UPDATE proposal_revision_lint_results SET linter_version = 'v999' WHERE proposal_id = $1 AND revision_seq = $2",
            ),
            (
                "hash mismatch",
                "UPDATE proposal_revision_lint_results SET body_sha256 = 'not-the-body-hash' WHERE proposal_id = $1 AND revision_seq = $2",
            ),
        ] {
            let proposal = repo.create(create_input(title)).await.unwrap();
            let revision = repo.revisions(&proposal.id).await.unwrap().remove(0);
            sqlx::query(mutation)
                .bind(&revision.proposal_id)
                .bind(revision.seq)
                .execute(db.pool())
                .await
                .unwrap();
            assert_eq!(
                repo.lint_for_revision(&revision).await.unwrap(),
                expected,
                "{title} cache row must recompute"
            );
        }

        let proposal = repo.create(create_input("malformed json")).await.unwrap();
        let revision = repo.revisions(&proposal.id).await.unwrap().remove(0);
        sqlx::query(
            "UPDATE proposal_revision_lint_results SET result_json = $1 WHERE proposal_id = $2 AND revision_seq = $3",
        )
        .bind(serde_json::json!({"not": "a SpecLintResultV1"}))
        .bind(&revision.proposal_id)
        .bind(revision.seq)
        .execute(db.pool())
        .await
        .unwrap();
        assert_eq!(repo.lint_for_revision(&revision).await.unwrap(), expected);

        let proposal = repo.create(create_input("invalid span")).await.unwrap();
        let revision = repo.revisions(&proposal.id).await.unwrap().remove(0);
        sqlx::query(
            "UPDATE proposal_revision_lint_results SET result_json = $1 WHERE proposal_id = $2 AND revision_seq = $3",
        )
        .bind(serde_json::json!({
            "linter_version": "v1",
            "body_sha256": djinn_spec_lint::body_sha256(""),
            "body_format": "markdown",
            "checked_at": "cached",
            "errors": [{"code": "BAD", "severity": "error", "message": "bad", "span": {"start": 0, "end": 1}}],
            "warnings": [],
            "skipped_tiers": [{"tier": "mdx_structure", "reason": "BODY_FORMAT_MARKDOWN"}],
        }))
        .bind(&revision.proposal_id)
        .bind(revision.seq)
        .execute(db.pool())
        .await
        .unwrap();
        assert_eq!(repo.lint_for_revision(&revision).await.unwrap(), expected);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lint_for_revision_rejects_unknown_stored_body_format() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo.create(create_input("unknown format")).await.unwrap();
        let revision = repo.revisions(&proposal.id).await.unwrap().remove(0);
        sqlx::query("UPDATE proposal_revisions SET body_format = 'rst' WHERE id = $1")
            .bind(&revision.id)
            .execute(db.pool())
            .await
            .unwrap();

        let error = repo.lint_for_revision(&revision).await.unwrap_err();
        assert!(format!("{error}").contains("invalid proposal body_format for spec lint: rst"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_by_id_and_short_id() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Resolve")).await.unwrap();
        assert_eq!(repo.resolve(&p.id).await.unwrap().unwrap().id, p.id);
        assert_eq!(repo.resolve(&p.short_id).await.unwrap().unwrap().id, p.id);
        assert!(repo.resolve("nope").await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_sets_closed_at_on_archive() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Up")).await.unwrap();
        let updated = repo
            .update(
                &p.id,
                ProposalUpdateInput {
                    title: "Up2",
                    body: "new body",
                    acceptance_criteria: "[\"ac1\"]",
                    status: "archived",
                    superseded_by: None,
                    body_format: None,
                    event_metadata: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.title, "Up2");
        assert_eq!(updated.status, "archived");
        assert_eq!(updated.acceptance_criteria, "[\"ac1\"]");
        assert!(updated.closed_at.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn targets_add_rerole_remove() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo.create(create_input("Targeted")).await.unwrap();
        let proj = insert_project(&db, "svc-a").await;

        assert!(repo.targets(&p.id).await.unwrap().is_empty());
        repo.add_target(&p.id, &proj, "primary").await.unwrap();
        // Re-add updates role (idempotent on the PK).
        repo.add_target(&p.id, &proj, "reference").await.unwrap();
        let targets = repo.targets(&p.id).await.unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].role, "reference");

        repo.remove_target(&p.id, &proj).await.unwrap();
        assert!(repo.targets(&p.id).await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn feedback_add_and_resolve() {
        let (bus, captured) = capturing_bus();
        let repo = ProposalRepository::new(test_db(), bus);
        let p = repo.create(create_input("Feedback")).await.unwrap();
        captured.lock().unwrap().clear();

        // A human comment (arrives unresolved).
        let comment = repo
            .add_feedback(ProposalFeedbackCreateInput {
                proposal_id: &p.id,
                parent_id: None,
                author_kind: "user",
                author_model: None,
                body: "what about X?",
            })
            .await
            .unwrap();
        assert!(comment.resolved_at.is_none());

        // An AI-authored entry.
        let ai = repo
            .add_feedback(ProposalFeedbackCreateInput {
                proposal_id: &p.id,
                parent_id: None,
                author_kind: "ai",
                author_model: Some("claude-opus-4-8"),
                body: "enforce in svc-invoice not the gateway",
            })
            .await
            .unwrap();
        assert_eq!(ai.author_kind, "ai");
        assert!(ai.resolved_at.is_none());

        // Resolve the comment as addressed in revision 2.
        let resolved = repo
            .set_feedback_resolved(&comment.id, Some(2))
            .await
            .unwrap();
        assert!(resolved.resolved_at.is_some());
        assert_eq!(resolved.resolved_revision_seq, Some(2));

        // Dismiss the AI entry (no spec change).
        let dismissed = repo.set_feedback_resolved(&ai.id, None).await.unwrap();
        assert!(dismissed.resolved_at.is_some());
        assert!(dismissed.resolved_revision_seq.is_none());

        assert_eq!(repo.feedback(&p.id).await.unwrap().len(), 2);
        let events = captured.lock().unwrap();
        // two adds + two resolves = four feedback events
        assert_eq!(events.len(), 4);
        assert!(events.iter().all(|e| e.entity_type == "proposal_feedback"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_filters_by_status_and_target() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-b").await;

        let a = repo.create(create_input("Alpha")).await.unwrap();
        repo.create(create_input("Beta")).await.unwrap();
        repo.add_target(&a.id, &proj, "primary").await.unwrap();

        let all = repo
            .list_filtered(ProposalListQuery::default())
            .await
            .unwrap();
        assert_eq!(all.total_count, 2);

        let targeted = repo
            .list_filtered(ProposalListQuery {
                target_project_id: Some(proj.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(targeted.total_count, 1);
        assert_eq!(targeted.proposals[0].0.id, a.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_reports_unresolved_feedback_count() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Counted")).await.unwrap();
        let mk = |body: &'static str| ProposalFeedbackCreateInput {
            proposal_id: &p.id,
            parent_id: None,
            author_kind: "user",
            author_model: None,
            body,
        };
        let f1 = repo.add_feedback(mk("one")).await.unwrap();
        repo.add_feedback(mk("two")).await.unwrap();

        let listed = repo
            .list_filtered(ProposalListQuery::default())
            .await
            .unwrap();
        assert_eq!(listed.proposals[0].1, 2);

        // Resolving one drops the count.
        repo.set_feedback_resolved(&f1.id, Some(2)).await.unwrap();
        let listed = repo
            .list_filtered(ProposalListQuery::default())
            .await
            .unwrap();
        assert_eq!(listed.proposals[0].1, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_summaries_batches_tribunal_facts() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-sum").await;

        // Messy proposal: active refinement, one blocking objection, a judge
        // needs-work verdict, no target.
        let messy = repo.create(create_input("Messy")).await.unwrap();
        repo.record_refinement_lifecycle(&messy.id, "refinement_start", None)
            .await
            .unwrap();
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &messy.id,
            kind: "objection",
            body: "this scope is unbounded",
            blocking: true,
            agent_role: "adversary",
            author_kind: "agent",
            author_model: Some("m"),
            source_task_id: None,
            against_revision_seq: 1,
            round: 2,
            body_metadata: None,
        })
        .await
        .unwrap();
        // A judge verdict — must NOT count toward the blocking objection total
        // (kind <> 'verdict'), but its body feeds the needs-work heuristic.
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &messy.id,
            kind: "verdict",
            body: "verdict: needs-work — tighten the scope",
            blocking: true,
            agent_role: "judge",
            author_kind: "agent",
            author_model: Some("m"),
            source_task_id: None,
            against_revision_seq: 1,
            round: 2,
            body_metadata: None,
        })
        .await
        .unwrap();

        // Clean proposal: refinement converged and parked awaiting review, a
        // non-needs-work verdict, a target attached, no blocking objections.
        let clean = repo.create(create_input("Clean")).await.unwrap();
        repo.add_target(&clean.id, &proj, "primary").await.unwrap();
        repo.record_refinement_lifecycle(&clean.id, "refinement_start", None)
            .await
            .unwrap();
        repo.record_refinement_lifecycle(&clean.id, "refinement_awaiting_review", None)
            .await
            .unwrap();
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &clean.id,
            kind: "verdict",
            body: "verdict: approve — ready to graduate",
            blocking: false,
            agent_role: "judge",
            author_kind: "agent",
            author_model: Some("m"),
            source_task_id: None,
            against_revision_seq: 1,
            round: 3,
            body_metadata: None,
        })
        .await
        .unwrap();

        let ids = vec![messy.id.clone(), clean.id.clone()];
        let summaries = repo.list_summaries(&ids).await.unwrap();
        assert_eq!(summaries.len(), 2, "every id present in the map");

        let m = summaries.get(&messy.id).unwrap();
        assert!(m.refinement_active, "messy has an active refinement run");
        assert!(!m.awaiting_review);
        assert_eq!(m.current_round, 2);
        assert_eq!(
            m.unresolved_blocking_count, 1,
            "the verdict row must be excluded from the objection count"
        );
        assert!(m.target_count == 0);
        assert!(
            m.latest_judge_verdict_body
                .as_deref()
                .is_some_and(|b| b.contains("needs-work"))
        );

        let c = summaries.get(&clean.id).unwrap();
        assert!(c.refinement_active, "no stop after start → still active");
        assert!(c.awaiting_review, "awaiting_review after start parks it");
        assert_eq!(c.current_round, 3);
        assert_eq!(c.unresolved_blocking_count, 0);
        assert_eq!(c.target_count, 1);
        assert!(
            c.latest_judge_verdict_body
                .as_deref()
                .is_some_and(|b| b.contains("approve"))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_summaries_empty_ids_is_empty() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let summaries = repo.list_summaries(&[]).await.unwrap();
        assert!(summaries.is_empty());
    }

    fn update_input<'a>(
        title: &'a str,
        body: &'a str,
        ac: &'a str,
        status: &'a str,
    ) -> ProposalUpdateInput<'a> {
        ProposalUpdateInput {
            title,
            body,
            acceptance_criteria: ac,
            status,
            superseded_by: None,
            body_format: None,
            event_metadata: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn signoffs_gate_approval_revisions_and_staleness() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Gate")).await.unwrap();
        assert_eq!(p.latest_revision_seq, 1);
        // create seeds revision 1.
        assert_eq!(repo.revisions(&p.id).await.unwrap().len(), 1);

        // Move to in_review (status-only → no new revision).
        repo.update(&p.id, update_input("Gate", "", "[]", "in_review"))
            .await
            .unwrap();
        assert_eq!(repo.revisions(&p.id).await.unwrap().len(), 1);

        // One sign-off is not enough.
        let after_scoped = repo.add_signoff(&p.id, "scoped", "user-a").await.unwrap();
        assert_eq!(after_scoped.status, "in_review");
        // Both fresh sign-offs auto-advance to approved.
        let after_tech = repo
            .add_signoff(&p.id, "technical", "user-b")
            .await
            .unwrap();
        assert_eq!(after_tech.status, "approved");

        // Editing an approved spec demotes to in_review, bumps the revision, and
        // clears sign-offs.
        let edited = repo
            .update(&p.id, update_input("Gate v2", "", "[]", "approved"))
            .await
            .unwrap();
        assert_eq!(edited.status, "in_review");
        assert_eq!(edited.latest_revision_seq, 2);
        assert_eq!(repo.revisions(&p.id).await.unwrap().len(), 2);
        assert!(repo.signoffs(&p.id).await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn status_only_done_appends_history_without_revision_or_signoff_staleness() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Manual done")).await.unwrap();
        repo.add_signoff(&p.id, "scoped", "user-a").await.unwrap();
        let before = repo.get(&p.id).await.unwrap().unwrap();
        assert_eq!(before.status, "in_review");
        assert_eq!(before.latest_revision_seq, 1);
        let signoffs_before = repo.signoffs(&p.id).await.unwrap();
        assert_eq!(signoffs_before.len(), 1);

        let done = djinn_core::auth_context::SESSION_USER_ID
            .scope(
                Some("actor-user".to_owned()),
                repo.update(&p.id, update_input("Manual done", "", "[]", "done")),
            )
            .await
            .unwrap();

        assert_eq!(done.status, "done");
        assert!(done.closed_at.is_some());
        assert_eq!(done.latest_revision_seq, before.latest_revision_seq);

        let revisions = repo.revisions(&p.id).await.unwrap();
        assert_eq!(revisions.len(), 2);
        assert_eq!(revisions[0].event_kind, "spec_revision");
        let event = &revisions[1];
        assert_eq!(event.seq, before.latest_revision_seq);
        assert_eq!(event.event_kind, "status_change");
        assert_eq!(event.status_from.as_deref(), Some("in_review"));
        assert_eq!(event.status_to.as_deref(), Some("done"));
        assert_eq!(event.edited_by_user_id.as_deref(), Some("actor-user"));
        assert!(!event.created_at.is_empty());

        let signoffs_after = repo.signoffs(&p.id).await.unwrap();
        assert_eq!(signoffs_after.len(), signoffs_before.len());
        assert_eq!(
            signoffs_after[0].revision_seq,
            signoffs_before[0].revision_seq
        );
        assert_eq!(signoffs_after[0].revision_seq, done.latest_revision_seq);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn signing_off_advances_a_draft_without_a_manual_status_change() {
        // Regression: a draft used to ignore sign-offs entirely — the gate only
        // fired from in_review, so a draft with both fresh sign-offs sat in
        // draft until someone manually bumped it. Signing the scope now requests
        // review, and both fresh sign-offs approve straight from draft.
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Draft gate")).await.unwrap();
        assert_eq!(p.status, "draft");

        let after_scoped = repo.add_signoff(&p.id, "scoped", "user-a").await.unwrap();
        assert_eq!(after_scoped.status, "in_review");

        let after_tech = repo
            .add_signoff(&p.id, "technical", "user-b")
            .await
            .unwrap();
        assert_eq!(after_tech.status, "approved");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clearing_signoff_demotes_from_approved() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Demote")).await.unwrap();
        repo.update(&p.id, update_input("Demote", "", "[]", "in_review"))
            .await
            .unwrap();
        repo.add_signoff(&p.id, "scoped", "u1").await.unwrap();
        let approved = repo.add_signoff(&p.id, "technical", "u2").await.unwrap();
        assert_eq!(approved.status, "approved");
        let demoted = repo.clear_signoff(&p.id, "technical", "u2").await.unwrap();
        assert_eq!(demoted.status, "in_review");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn addressing_feedback_edits_spec_then_resolves_at_revision() {
        // Models the chat flow: djinn rewrites the spec via `update` (which
        // appends a revision), then marks the feedback resolved at that seq.
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Edit")).await.unwrap();
        let f = repo
            .add_feedback(ProposalFeedbackCreateInput {
                proposal_id: &p.id,
                parent_id: None,
                author_kind: "user",
                author_model: None,
                body: "tweak the spec",
            })
            .await
            .unwrap();
        let updated = repo
            .update(&p.id, update_input("Edit", "New spec body.", "[]", "draft"))
            .await
            .unwrap();
        assert_eq!(updated.body, "New spec body.");
        assert_eq!(updated.latest_revision_seq, 2);

        let resolved = repo
            .set_feedback_resolved(&f.id, Some(updated.latest_revision_seq))
            .await
            .unwrap();
        assert_eq!(resolved.resolved_revision_seq, Some(2));
        assert!(resolved.resolved_at.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn participants_and_graduation_linking() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-grad").await;
        let p = repo.create(create_input("Grad")).await.unwrap();

        repo.add_signoff(&p.id, "scoped", "user-x").await.unwrap();
        let parts = repo.participants(&p.id).await.unwrap();
        assert!(parts.contains(&"user-x".to_string()));

        // Simulate graduation linking an epic (insert an epic row directly).
        let epic_id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, status, owner, memory_refs, auto_breakdown)
             VALUES ($1, $2, 'gep1', 'T', '', '', '', 'open', '', '[]'::jsonb, true)",
            epic_id,
            proj
        )
        .execute(db.pool())
        .await
        .unwrap();
        repo.link_epic(&p.id, &epic_id, &proj).await.unwrap();
        repo.set_building(&p.id, "user-x").await.unwrap();

        let graduated = repo.graduated_epics(&p.id).await.unwrap();
        assert_eq!(graduated, vec![(epic_id, proj)]);
        let built = repo.get(&p.id).await.unwrap().unwrap();
        assert_eq!(built.status, "building");
        assert_eq!(built.build_owner_user_id.as_deref(), Some("user-x"));
    }

    /// Helper: insert an open epic row and return its id.
    async fn insert_epic(db: &Database, project_id: &str, short_id: &str) -> String {
        let epic_id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, status, owner, memory_refs, auto_breakdown)
             VALUES ($1, $2, $3, 'T', '', '', '', 'open', '', '[]'::jsonb, true)",
            epic_id,
            project_id,
            short_id
        )
        .execute(db.pool())
        .await
        .unwrap();
        epic_id
    }

    async fn close_epic(db: &Database, epic_id: &str) {
        sqlx::query!("UPDATE epics SET status = 'closed' WHERE id = $1", epic_id)
            .execute(db.pool())
            .await
            .unwrap();
    }

    async fn reconciliation_count(db: &Database, proposal_id: &str, epic_id: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM proposal_reconciliations WHERE proposal_id = $1 AND epic_id = $2",
        )
        .bind(proposal_id)
        .bind(epic_id)
        .fetch_one(db.pool())
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_completion_lifecycle() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-done").await;
        let p = repo.create(create_input("Closeout")).await.unwrap();

        let e1 = insert_epic(&db, &proj, "ce01").await;
        let e2 = insert_epic(&db, &proj, "ce02").await;
        repo.link_epic(&p.id, &e1, &proj).await.unwrap();
        repo.link_epic(&p.id, &e2, &proj).await.unwrap();
        repo.set_building(&p.id, "user-x").await.unwrap();

        // Reverse lookup resolves the parent proposal.
        assert_eq!(repo.proposal_for_epic(&e1).await.unwrap().unwrap().id, p.id);
        assert!(
            repo.proposal_for_epic("no-such-epic")
                .await
                .unwrap()
                .is_none()
        );

        // Not complete while any graduated epic is open.
        assert!(!repo.all_graduated_epics_closed(&p.id).await.unwrap());
        close_epic(&db, &e1).await;
        assert!(!repo.all_graduated_epics_closed(&p.id).await.unwrap());
        close_epic(&db, &e2).await;
        assert!(repo.all_graduated_epics_closed(&p.id).await.unwrap());

        // set_done is terminal and stamps closed_at.
        let done = repo.set_done(&p.id).await.unwrap();
        assert_eq!(done.status, "done");
        assert!(done.closed_at.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn all_graduated_epics_closed_is_false_without_epics() {
        // A proposal that has graduated nothing yet is not "complete".
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("No epics")).await.unwrap();
        assert!(!repo.all_graduated_epics_closed(&p.id).await.unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_acceptance_criteria_is_a_status_annotation_not_a_spec_edit() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo
            .create(ProposalCreateInput {
                title: "AC",
                body: "",
                acceptance_criteria: Some(
                    r#"[{"criterion":"a","met":false},{"criterion":"b","met":false}]"#,
                ),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        // A sign-off anchored to the head revision.
        repo.add_signoff(&p.id, "scoped", "u1").await.unwrap();
        let seq_before = repo.get(&p.id).await.unwrap().unwrap().latest_revision_seq;

        // Mark the first criterion met.
        let updated = repo
            .set_acceptance_criteria(
                &p.id,
                r#"[{"criterion":"a","met":true},{"criterion":"b","met":false}]"#,
            )
            .await
            .unwrap();

        // Unlike update(): no new revision and the sign-off survives.
        assert_eq!(updated.latest_revision_seq, seq_before);
        assert_eq!(repo.signoffs(&p.id).await.unwrap().len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&updated.acceptance_criteria).unwrap();
        assert_eq!(parsed[0]["met"], serde_json::json!(true));
        assert_eq!(parsed[1]["met"], serde_json::json!(false));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn amend_acceptance_criteria_rewrites_drops_waives_and_audits() {
        let (bus, captured) = capturing_bus();
        let repo = ProposalRepository::new(test_db(), bus);
        let p = repo
            .create(ProposalCreateInput {
                title: "AC amend",
                body: "body",
                acceptance_criteria: Some(
                    r#"[{"criterion":"rewrite me","met":false},{"criterion":"drop me","met":false},{"criterion":"waive me","met":false}]"#,
                ),
                status: Some("in_review"),
                body_format: None,
            })
            .await
            .unwrap();
        repo.add_signoff(&p.id, "scoped", "u1").await.unwrap();
        let signoffs_before = repo.signoffs(&p.id).await.unwrap();
        captured.lock().unwrap().clear();

        let updated = repo
            .amend_acceptance_criteria(
                &p.id,
                &[
                    ProposalAcceptanceCriteriaAmendment::Rewrite {
                        index: 0,
                        criterion: "rewritten criterion",
                    },
                    ProposalAcceptanceCriteriaAmendment::Drop { index: 1 },
                    ProposalAcceptanceCriteriaAmendment::Waive { index: 1 },
                ],
                "criterion 2 cannot be verified by agents",
            )
            .await
            .unwrap();

        assert_eq!(updated.latest_revision_seq, p.latest_revision_seq + 1);
        assert_eq!(repo.revisions(&p.id).await.unwrap().len(), 2);
        let signoffs_after = repo.signoffs(&p.id).await.unwrap();
        assert_eq!(signoffs_after.len(), signoffs_before.len());
        assert_eq!(
            signoffs_after[0].proposal_id,
            signoffs_before[0].proposal_id
        );
        assert_eq!(signoffs_after[0].kind, signoffs_before[0].kind);
        assert_eq!(signoffs_after[0].user_id, signoffs_before[0].user_id);
        assert_eq!(
            signoffs_after[0].revision_seq,
            signoffs_before[0].revision_seq
        );
        let parsed: serde_json::Value = serde_json::from_str(&updated.acceptance_criteria).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 2);
        assert_eq!(
            parsed[0]["criterion"],
            serde_json::json!("rewritten criterion")
        );
        assert_eq!(parsed[0]["met"], serde_json::json!(false));
        assert_eq!(parsed[1]["criterion"], serde_json::json!("waive me"));
        assert_eq!(parsed[1]["waived"], serde_json::json!(true));

        // No feedback row is written on amendment anymore — the audit rides on
        // the revision's `event_metadata` instead.
        let feedback = repo.feedback(&p.id).await.unwrap();
        assert!(feedback.is_empty());

        // The bumped spec revision (seq 2) carries the structured amendment audit.
        let revisions = repo.revisions(&p.id).await.unwrap();
        let amend_rev = revisions
            .iter()
            .find(|r| r.seq == 2 && r.event_kind == "spec_revision")
            .expect("amendment spec revision at seq 2");
        let meta: serde_json::Value =
            serde_json::from_str(amend_rev.event_metadata.as_deref().unwrap()).unwrap();
        assert_eq!(meta["kind"], serde_json::json!("ac_amendment"));
        assert_eq!(
            meta["reason"],
            serde_json::json!("criterion 2 cannot be verified by agents")
        );
        let audit_entries = meta["amendments"].as_array().unwrap();
        assert_eq!(audit_entries.len(), 3);
        assert_eq!(audit_entries[0]["operation"], serde_json::json!("rewrite"));
        assert_eq!(audit_entries[1]["operation"], serde_json::json!("drop"));
        assert_eq!(audit_entries[2]["operation"], serde_json::json!("waive"));
        assert_eq!(
            audit_entries[1]["old_criterion"],
            serde_json::json!({"criterion": "drop me", "met": false})
        );
        assert_eq!(
            audit_entries[1]["new_criterion"],
            serde_json::json!({"dropped": true})
        );

        // Only the `proposal updated` event fires — no `proposal_feedback created`.
        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "proposal");
        assert_eq!(events[0].action, "updated");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn amend_acceptance_criteria_validates_without_mutating() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo
            .create(ProposalCreateInput {
                title: "AC invalid",
                body: "body",
                acceptance_criteria: Some(r#"[{"criterion":"keep","met":false}]"#),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        let before = repo.get(&p.id).await.unwrap().unwrap();

        let empty_reason = repo
            .amend_acceptance_criteria(
                &p.id,
                &[ProposalAcceptanceCriteriaAmendment::Rewrite {
                    index: 0,
                    criterion: "changed",
                }],
                "   ",
            )
            .await;
        assert!(empty_reason.is_err());
        let after_empty_reason = repo.get(&p.id).await.unwrap().unwrap();
        assert_eq!(
            after_empty_reason.acceptance_criteria,
            before.acceptance_criteria
        );
        assert_eq!(
            after_empty_reason.latest_revision_seq,
            before.latest_revision_seq
        );

        let bad_index = repo
            .amend_acceptance_criteria(
                &p.id,
                &[ProposalAcceptanceCriteriaAmendment::Drop { index: 7 }],
                "bad index",
            )
            .await;
        assert!(bad_index.is_err());
        let after_bad_index = repo.get(&p.id).await.unwrap().unwrap();
        assert_eq!(
            after_bad_index.acceptance_criteria,
            before.acceptance_criteria
        );
        assert_eq!(
            after_bad_index.latest_revision_seq,
            before.latest_revision_seq
        );
        assert!(repo.feedback(&p.id).await.unwrap().is_empty());
        assert_eq!(repo.revisions(&p.id).await.unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drained_building_proposals_only_returns_fully_closed_builds() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-drain").await;

        // p1: building, one epic still OPEN → not drained.
        let p1 = repo.create(create_input("p1")).await.unwrap();
        let e1 = insert_epic(&db, &proj, "dr01").await;
        repo.link_epic(&p1.id, &e1, &proj).await.unwrap();
        repo.set_building(&p1.id, "u").await.unwrap();

        // p2: building, every epic CLOSED → drained.
        let p2 = repo.create(create_input("p2")).await.unwrap();
        let e2 = insert_epic(&db, &proj, "dr02").await;
        repo.link_epic(&p2.id, &e2, &proj).await.unwrap();
        repo.set_building(&p2.id, "u").await.unwrap();
        close_epic(&db, &e2).await;

        // p3: building, no graduated epics → not drained.
        let p3 = repo.create(create_input("p3")).await.unwrap();
        repo.set_building(&p3.id, "u").await.unwrap();

        // p4: NOT building (draft) with a closed epic → not drained.
        let p4 = repo.create(create_input("p4")).await.unwrap();
        let e4 = insert_epic(&db, &proj, "dr04").await;
        repo.link_epic(&p4.id, &e4, &proj).await.unwrap();
        close_epic(&db, &e4).await;

        let ids: Vec<String> = repo
            .drained_building_proposals()
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert!(
            ids.contains(&p2.id),
            "building + all epics closed is drained"
        );
        assert!(!ids.contains(&p1.id), "an open epic means not drained");
        assert!(
            !ids.contains(&p3.id),
            "no graduated epics means not drained"
        );
        assert!(!ids.contains(&p4.id), "non-building is never drained");
    }

    /// Helper: insert an open `task` row under an epic and return its id.
    async fn insert_task(db: &Database, project_id: &str, epic_id: &str, short_id: &str) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                                issue_type, priority, owner, status, continuation_count, labels, acceptance_criteria, memory_refs)
             VALUES ($1, $2, $3, $4, 'T', '', '', 'task', 0, '', 'open', 0, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb)",
            id,
            project_id,
            short_id,
            epic_id
        )
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    async fn set_task_status(
        db: &Database,
        task_id: &str,
        status: &str,
        close_reason: Option<&str>,
    ) {
        sqlx::query("UPDATE tasks SET status = $1, close_reason = $2 WHERE id = $3")
            .bind(status)
            .bind(close_reason)
            .bind(task_id)
            .execute(db.pool())
            .await
            .unwrap();
    }

    async fn set_epic_memory_refs(db: &Database, epic_id: &str, refs: Vec<String>) {
        let memory_refs =
            serde_json::Value::Array(refs.into_iter().map(serde_json::Value::String).collect());
        sqlx::query("UPDATE epics SET memory_refs = $1 WHERE id = $2")
            .bind(memory_refs)
            .bind(epic_id)
            .execute(db.pool())
            .await
            .unwrap();
    }

    async fn set_task_memory_refs(db: &Database, task_id: &str, refs: Vec<String>) {
        let memory_refs =
            serde_json::Value::Array(refs.into_iter().map(serde_json::Value::String).collect());
        sqlx::query("UPDATE tasks SET memory_refs = $1 WHERE id = $2")
            .bind(memory_refs)
            .bind(task_id)
            .execute(db.pool())
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_refs_for_proposal_walks_epics_and_tasks_deduping() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-memory-walk").await;
        let proposal = repo.create(create_input("Memory walk")).await.unwrap();

        let epic_one_ref = note_repo
            .create(&proj, "Epic One Ref", "epic one", "case", "[]")
            .await
            .unwrap();
        let epic_two_ref = note_repo
            .create(&proj, "Epic Two Ref", "epic two", "pattern", "[]")
            .await
            .unwrap();
        let task_one_ref = note_repo
            .create(&proj, "Task One Ref", "task one", "pitfall", "[]")
            .await
            .unwrap();
        let task_two_ref = note_repo
            .create(&proj, "Task Two Ref", "task two", "adr", "[]")
            .await
            .unwrap();

        let epic_one = insert_epic(&db, &proj, "mr01").await;
        let epic_two = insert_epic(&db, &proj, "mr02").await;
        repo.link_epic(&proposal.id, &epic_one, &proj)
            .await
            .unwrap();
        repo.link_epic(&proposal.id, &epic_two, &proj)
            .await
            .unwrap();

        set_epic_memory_refs(
            &db,
            &epic_one,
            vec![
                epic_one_ref.permalink.clone(),
                task_one_ref.permalink.clone(),
            ],
        )
        .await;
        set_epic_memory_refs(
            &db,
            &epic_two,
            vec![
                epic_two_ref.permalink.clone(),
                epic_one_ref.permalink.clone(),
            ],
        )
        .await;

        let task_one = insert_task(&db, &proj, &epic_one, "mt01").await;
        let task_two = insert_task(&db, &proj, &epic_one, "mt02").await;
        let task_three = insert_task(&db, &proj, &epic_two, "mt03").await;
        let task_four = insert_task(&db, &proj, &epic_two, "mt04").await;
        set_task_memory_refs(
            &db,
            &task_one,
            vec![
                task_one_ref.permalink.clone(),
                task_two_ref.permalink.clone(),
            ],
        )
        .await;
        set_task_memory_refs(&db, &task_two, vec![epic_one_ref.permalink.clone()]).await;
        set_task_memory_refs(&db, &task_three, vec![task_two_ref.permalink.clone()]).await;
        set_task_memory_refs(&db, &task_four, vec![epic_two_ref.permalink.clone()]).await;

        let refs = repo.memory_refs_for_proposal(&proposal.id).await.unwrap();
        assert_eq!(refs.len(), 4);

        let by_permalink: HashMap<_, _> = refs
            .into_iter()
            .map(|memory_ref| (memory_ref.permalink.clone(), memory_ref))
            .collect();

        assert_eq!(
            by_permalink.get(&epic_one_ref.permalink).unwrap(),
            &ProposalMemoryRef {
                permalink: epic_one_ref.permalink.clone(),
                title: "Epic One Ref".to_owned(),
                note_type: "case".to_owned(),
                source_entity_type: "epic".to_owned(),
                source_short_id: "mr01".to_owned(),
            }
        );
        assert_eq!(
            by_permalink.get(&task_one_ref.permalink).unwrap(),
            &ProposalMemoryRef {
                permalink: task_one_ref.permalink.clone(),
                title: "Task One Ref".to_owned(),
                note_type: "pitfall".to_owned(),
                source_entity_type: "epic".to_owned(),
                source_short_id: "mr01".to_owned(),
            }
        );
        assert_eq!(
            by_permalink.get(&task_two_ref.permalink).unwrap(),
            &ProposalMemoryRef {
                permalink: task_two_ref.permalink.clone(),
                title: "Task Two Ref".to_owned(),
                note_type: "adr".to_owned(),
                source_entity_type: "task".to_owned(),
                source_short_id: "mt01".to_owned(),
            }
        );
        assert_eq!(
            by_permalink.get(&epic_two_ref.permalink).unwrap(),
            &ProposalMemoryRef {
                permalink: epic_two_ref.permalink.clone(),
                title: "Epic Two Ref".to_owned(),
                note_type: "pattern".to_owned(),
                source_entity_type: "epic".to_owned(),
                source_short_id: "mr02".to_owned(),
            }
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_allows_spec_edit_while_building_and_marks_drift() {
        // Amend-while-building: a material edit to a `building` proposal is
        // allowed and only stamps drift — it does NOT touch the status. This
        // is the positive replacement for the old "spec edit while building is
        // rejected" regression; status-only updates remain allowed too.
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-editguard").await;
        let p = repo
            .create(create_input_with_ac(
                "Guarded",
                "",
                r#"[{"criterion":"do X","met":false}]"#,
            ))
            .await
            .unwrap();
        let epic = insert_epic(&db, &proj, "eg01").await;
        repo.link_epic(&p.id, &epic, &proj).await.unwrap();
        let building = repo.set_building(&p.id, "user-x").await.unwrap();
        assert_eq!(building.last_reconciled_revision_seq, Some(1));
        assert!(!building.pending_reconcile);
        assert!(
            repo.latest_epic_reconciliations(&p.id)
                .await
                .unwrap()
                .is_empty()
        );
        let linked_while_building = insert_epic(&db, &proj, "eg02").await;
        repo.link_epic(&p.id, &linked_while_building, &proj)
            .await
            .unwrap();
        let latest_by_epic = repo.latest_epic_reconciliations(&p.id).await.unwrap();
        assert_eq!(latest_by_epic.get(&linked_while_building), Some(&1));

        // Status-only update (no spec change) remains allowed: status stays
        // `building`, no new revision, no new drift.
        let status_only = repo
            .update(
                &p.id,
                update_input(
                    "Guarded",
                    "",
                    r#"[{"criterion":"do X","met":false}]"#,
                    "building",
                ),
            )
            .await
            .unwrap();
        assert_eq!(status_only.status, "building");
        assert_eq!(status_only.latest_revision_seq, 1);
        assert!(!status_only.pending_reconcile);
        assert_eq!(repo.revisions(&p.id).await.unwrap().len(), 1);

        // Material edit: title + body + AC all change. The build stays
        // `building`, a new revision lands, `pending_reconcile` flips true,
        // and `last_reconciled_revision_seq` does NOT advance (the build is
        // still against rev 1).
        let updated = repo
            .update(
                &p.id,
                update_input(
                    "Guarded v2",
                    "new body",
                    r#"[{"criterion":"do X better","met":false}]"#,
                    "approved",
                ),
            )
            .await
            .unwrap();
        assert_eq!(updated.title, "Guarded v2");
        assert_eq!(updated.body, "new body");
        let ac: serde_json::Value = serde_json::from_str(&updated.acceptance_criteria).unwrap();
        assert_eq!(
            ac,
            serde_json::json!([{"criterion": "do X better", "met": false}])
        );
        assert_eq!(updated.status, "building");
        assert_eq!(updated.latest_revision_seq, 2);
        assert_eq!(updated.last_reconciled_revision_seq, Some(1));
        assert!(updated.pending_reconcile);
        assert_eq!(repo.revisions(&p.id).await.unwrap().len(), 2);

        let reconciled = repo.mark_reconciled(&p.id).await.unwrap();
        assert_eq!(reconciled.last_reconciled_revision_seq, Some(2));
        assert!(!reconciled.pending_reconcile);
        let latest_by_epic = repo.latest_epic_reconciliations(&p.id).await.unwrap();
        assert_eq!(latest_by_epic.get(&epic), Some(&2));
        repo.record_epic_reconciliation(&p.id, &epic, 1)
            .await
            .unwrap();
        let latest_by_epic = repo.latest_epic_reconciliations(&p.id).await.unwrap();
        assert_eq!(latest_by_epic.get(&epic), Some(&2));

        // A closeout is itself a successful reconcile: it stamps current
        // per-epic metadata and clears proposal-level drift before moving to
        // the terminal state.
        let done = repo.set_done(&p.id).await.unwrap();
        assert_eq!(done.status, "done");
        assert!(!done.pending_reconcile);
        assert_eq!(done.last_reconciled_revision_seq, Some(2));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn add_signoff_while_building_does_not_reconcile_status() {
        // `reconcile_approval` short-circuits on `building` so a sign-off
        // (which would otherwise be enough to flip a draft → in_review and
        // a fresh pair → approved) never demotes a build back to the review
        // gate. The sign-off is still recorded, the status stays `building`.
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-build-signoff").await;
        let p = repo.create(create_input("Build signoff")).await.unwrap();
        let epic = insert_epic(&db, &proj, "bs01").await;
        repo.link_epic(&p.id, &epic, &proj).await.unwrap();
        repo.set_building(&p.id, "user-x").await.unwrap();

        let updated = repo.add_signoff(&p.id, "scoped", "user-y").await.unwrap();
        assert_eq!(updated.status, "building");
        assert_eq!(repo.signoffs(&p.id).await.unwrap().len(), 1);

        // clear_signoff is the symmetric guard: it must also skip
        // reconcile_approval on a building proposal, so withdrawing a sign-off
        // can never yank the build back to in_review.
        let cleared = repo.clear_signoff(&p.id, "scoped", "user-y").await.unwrap();
        assert_eq!(cleared.status, "building");
        assert!(repo.signoffs(&p.id).await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unlink_epics_clears_only_the_target_proposal() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-unlink").await;
        let p1 = repo.create(create_input("P1")).await.unwrap();
        let p2 = repo.create(create_input("P2")).await.unwrap();
        let e1 = insert_epic(&db, &proj, "ul01").await;
        let e2 = insert_epic(&db, &proj, "ul02").await;
        repo.link_epic(&p1.id, &e1, &proj).await.unwrap();
        repo.link_epic(&p2.id, &e2, &proj).await.unwrap();

        repo.unlink_epics(&p1.id).await.unwrap();

        assert!(repo.graduated_epics(&p1.id).await.unwrap().is_empty());
        // p2's link is untouched.
        assert_eq!(
            repo.graduated_epics(&p2.id).await.unwrap(),
            vec![(e2, proj)]
        );
        // Idempotent: a second unlink on an already-empty proposal is a no-op.
        repo.unlink_epics(&p1.id).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unlink_epic_removes_only_requested_link_and_cascades_reconciliation() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-unlink-one").await;
        let p1 = repo.create(create_input("P1 selective")).await.unwrap();
        let p2 = repo.create(create_input("P2 untouched")).await.unwrap();
        let e1 = insert_epic(&db, &proj, "uo01").await;
        let e2 = insert_epic(&db, &proj, "uo02").await;
        let e3 = insert_epic(&db, &proj, "uo03").await;

        repo.link_epic(&p1.id, &e1, &proj).await.unwrap();
        repo.link_epic(&p1.id, &e2, &proj).await.unwrap();
        // Link the same epic from another proposal too: the selective unlink is
        // keyed by both proposal_id and epic_id, not by epic_id alone.
        repo.link_epic(&p2.id, &e1, &proj).await.unwrap();
        repo.link_epic(&p2.id, &e3, &proj).await.unwrap();

        repo.record_epic_reconciliation(&p1.id, &e1, 1)
            .await
            .unwrap();
        repo.record_epic_reconciliation(&p1.id, &e2, 2)
            .await
            .unwrap();
        repo.record_epic_reconciliation(&p2.id, &e1, 3)
            .await
            .unwrap();
        repo.record_epic_reconciliation(&p2.id, &e3, 4)
            .await
            .unwrap();

        repo.unlink_epic(&p1.id, &e1).await.unwrap();

        assert_eq!(
            repo.graduated_epics(&p1.id).await.unwrap(),
            vec![(e2.clone(), proj.clone())]
        );
        let mut p2_links = repo.graduated_epics(&p2.id).await.unwrap();
        p2_links.sort();
        let mut expected_p2_links = vec![(e1.clone(), proj.clone()), (e3.clone(), proj.clone())];
        expected_p2_links.sort();
        assert_eq!(p2_links, expected_p2_links);

        assert_eq!(reconciliation_count(&db, &p1.id, &e1).await, 0);
        assert_eq!(reconciliation_count(&db, &p1.id, &e2).await, 1);
        assert_eq!(reconciliation_count(&db, &p2.id, &e1).await, 1);
        assert_eq!(reconciliation_count(&db, &p2.id, &e3).await, 1);

        let p1_reconciliations = repo.latest_epic_reconciliations(&p1.id).await.unwrap();
        assert_eq!(p1_reconciliations.get(&e1), None);
        assert_eq!(p1_reconciliations.get(&e2), Some(&2));
        let p2_reconciliations = repo.latest_epic_reconciliations(&p2.id).await.unwrap();
        assert_eq!(p2_reconciliations.get(&e1), Some(&3));
        assert_eq!(p2_reconciliations.get(&e3), Some(&4));

        // Idempotent: unlinking the already-removed pair again is a no-op.
        repo.unlink_epic(&p1.id, &e1).await.unwrap();
        assert_eq!(repo.graduated_epics(&p1.id).await.unwrap().len(), 1);
        assert_eq!(reconciliation_count(&db, &p1.id, &e1).await, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_frozen_round_trips() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Freeze")).await.unwrap();
        assert!(!p.build_frozen);
        let frozen = repo.set_frozen(&p.id, true).await.unwrap();
        assert!(frozen.build_frozen);
        assert!(repo.get(&p.id).await.unwrap().unwrap().build_frozen);
        let thawed = repo.set_frozen(&p.id, false).await.unwrap();
        assert!(!thawed.build_frozen);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn revert_to_approved_clears_all_build_state() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-revert").await;
        let p = repo.create(create_input("Revert")).await.unwrap();
        let epic = insert_epic(&db, &proj, "rv01").await;
        let task = insert_task(&db, &proj, &epic, "rv01t").await;

        repo.link_epic(&p.id, &epic, &proj).await.unwrap();
        repo.set_building(&p.id, "user-x").await.unwrap();
        repo.set_breakdown_task(&p.id, &task).await.unwrap();
        repo.set_frozen(&p.id, true).await.unwrap();
        let mid = repo.get(&p.id).await.unwrap().unwrap();
        assert_eq!(mid.status, "building");
        assert_eq!(mid.build_breakdown_task_id.as_deref(), Some(task.as_str()));
        assert!(mid.build_frozen);

        let reverted = repo.revert_to_approved(&p.id).await.unwrap();
        assert_eq!(reverted.status, "approved");
        assert!(reverted.build_owner_user_id.is_none());
        assert!(reverted.build_breakdown_task_id.is_none());
        assert!(!reverted.build_frozen);
        // Epics are unlinked separately, not by revert.
        assert_eq!(repo.graduated_epics(&p.id).await.unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn breakdown_task_link_survives_task_delete_as_null() {
        // ON DELETE SET NULL: hard-deleting the breakdown task nulls the link
        // rather than orphaning a dangling id or blocking the delete.
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-bd").await;
        let p = repo.create(create_input("Breakdown")).await.unwrap();
        let epic = insert_epic(&db, &proj, "bd01").await;
        let task = insert_task(&db, &proj, &epic, "bd01t").await;
        repo.set_breakdown_task(&p.id, &task).await.unwrap();
        assert_eq!(
            repo.get(&p.id)
                .await
                .unwrap()
                .unwrap()
                .build_breakdown_task_id
                .as_deref(),
            Some(task.as_str())
        );

        sqlx::query!("DELETE FROM tasks WHERE id = $1", task)
            .execute(db.pool())
            .await
            .unwrap();
        assert!(
            repo.get(&p.id)
                .await
                .unwrap()
                .unwrap()
                .build_breakdown_task_id
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn search_proposals_returns_matching_proposal_with_nonzero_score() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        // Create a proposal with a unique sentinel word in its body.
        let sentinel = format!("xyzzy{}searchtest", uuid::Uuid::now_v7().as_simple());
        let p = repo
            .create(ProposalCreateInput {
                title: "Search Target Proposal",
                body: &format!(
                    "This proposal describes a {} integration pattern for the platform.",
                    sentinel
                ),
                acceptance_criteria: Some(r#"[{"criterion":"works","met":false}]"#),
                status: Some("draft"),
                body_format: None,
            })
            .await
            .unwrap();

        let results = repo.search_proposals(&sentinel, 10).await.unwrap();

        assert!(
            !results.is_empty(),
            "search_proposals should return at least one result for a unique sentinel word"
        );
        let hit = results
            .iter()
            .find(|r| r.short_id == p.short_id)
            .expect("the created proposal should appear in search results");
        assert!(
            hit.score > 0.0,
            "the ts_rank score for an exact match should be positive"
        );
        assert!(
            !hit.snippet.is_empty(),
            "the snippet should not be empty for a matching proposal"
        );
        assert_eq!(hit.title, "Search Target Proposal");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn search_proposals_excludes_archived_and_rejected() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let sentinel = format!("zyxxy{}excludetest", uuid::Uuid::now_v7().as_simple());
        let p = repo
            .create(ProposalCreateInput {
                title: "Excluded Proposal",
                body: &format!("Body with {} keyword.", sentinel),
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Verify it appears when active (draft).
        let results = repo.search_proposals(&sentinel, 10).await.unwrap();
        assert!(results.iter().any(|r| r.short_id == p.short_id));

        // Archive it.
        repo.update(
            &p.id,
            ProposalUpdateInput {
                title: "Excluded Proposal",
                body: &format!("Body with {} keyword.", sentinel),
                acceptance_criteria: "[]",
                status: "archived",
                superseded_by: None,
                body_format: None,
                event_metadata: None,
            },
        )
        .await
        .unwrap();

        let results = repo.search_proposals(&sentinel, 10).await.unwrap();
        assert!(
            !results.iter().any(|r| r.short_id == p.short_id),
            "archived proposals should not appear in search results"
        );
    }

    // ── Material revision metadata (event_metadata) plumbing ──────────────────
    //
    // The block-patch primitive and the planner refinement loop depend on the
    // repository persisting structured metadata on the spec revision row so
    // each targeted patch (and the native-skill version that produced it) is
    // attributable after the fact. These tests pin the contract:
    //
    //   * The create seed revision and ordinary `proposal_update` calls write
    //     `event_metadata = NULL` (backward compatible — no schema change, no
    //     contract drift for existing callers).
    //   * When a caller passes a `serde_json::Value` through
    //     `ProposalUpdateInput { event_metadata, .. }`, the same value is
    //     round-tripped through `ProposalRepository::revisions`.
    //   * Status-only updates and the `status_change` audit events keep
    //     `event_metadata = NULL` (audit history stays metadata-free).
    //   * A `proposal_create` followed by an `update` with metadata produces
    //     a head revision whose metadata survives the read path unchanged.

    /// The seed revision written by `ProposalRepository::create` must keep
    /// `event_metadata` NULL. The seed is not an authoring operation — the
    /// block-patch / native-skill attribution contract does not apply.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_seed_revision_has_null_event_metadata() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Seed Meta")).await.unwrap();
        let revisions = repo.revisions(&p.id).await.unwrap();
        assert_eq!(revisions.len(), 1, "create must seed exactly one revision");
        let seed = &revisions[0];
        assert_eq!(seed.seq, 1);
        assert_eq!(seed.event_kind, "spec_revision");
        assert!(
            seed.event_metadata.is_none(),
            "create seed revision must leave event_metadata NULL, got {:?}",
            seed.event_metadata
        );
    }

    /// Ordinary `proposal_update` (no `event_metadata` payload) must keep the
    /// `event_metadata` column NULL. This is the backward-compatibility
    /// contract the task description pins for every existing caller
    /// (`proposal_update`, `proposal_create`, `proposal_import`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ordinary_update_writes_null_event_metadata() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Backward Compat")).await.unwrap();
        repo.update(
            &p.id,
            ProposalUpdateInput {
                title: "Backward Compat v2",
                body: "v2 body",
                acceptance_criteria: "[]",
                status: "draft",
                superseded_by: None,
                body_format: None,
                event_metadata: None,
            },
        )
        .await
        .unwrap();
        let revisions = repo.revisions(&p.id).await.unwrap();
        assert_eq!(revisions.len(), 2);
        let head = revisions.last().expect("head revision");
        assert_eq!(head.seq, 2);
        assert!(
            head.event_metadata.is_none(),
            "ordinary proposal_update must keep event_metadata NULL, got {:?}",
            head.event_metadata
        );
    }

    /// When the caller supplies structured metadata through
    /// `ProposalUpdateInput { event_metadata, .. }`, the repository must
    /// persist it into the `proposal_revisions.event_metadata` JSONB column
    /// unchanged (stable JSON shape) and the read path must surface the same
    /// text. The metadata is the typed contract that future targeted-patch
    /// calls will build.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_with_event_metadata_round_trips_to_revision() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Patchy")).await.unwrap();
        let metadata = serde_json::json!({
            "change_kind": "targeted_block_patch",
            "block_id": "callout-tip",
            "selector": "paragraph: 'lifecycle: draft'",
            "range_start_byte": 12,
            "range_end_byte": 48,
            "native_skill_name": "visual-spec",
            "native_skill_version": "0.1.0",
            "note": "replace markdown tip prose with <Callout />"
        });
        repo.update(
            &p.id,
            ProposalUpdateInput {
                title: "Patchy",
                body: "lifecycle: <Callout>draft</Callout>",
                acceptance_criteria: "[]",
                status: "draft",
                superseded_by: None,
                body_format: Some("mdx"),
                event_metadata: Some(&metadata),
            },
        )
        .await
        .unwrap();
        let revisions = repo.revisions(&p.id).await.unwrap();
        let head = revisions.last().expect("head revision");
        assert_eq!(head.seq, 2);
        let stored = head
            .event_metadata
            .as_deref()
            .expect("head revision must carry event_metadata for a targeted patch");
        let parsed: serde_json::Value = serde_json::from_str(stored)
            .expect("event_metadata must be a valid JSON document on the read path");
        assert_eq!(parsed, metadata);
        // Stable field-by-field contract: every key the design promises is
        // present and round-trips byte-for-byte. This is what the
        // `proposal_show`/revision model surfaces to UI consumers.
        assert_eq!(parsed["change_kind"], "targeted_block_patch");
        assert_eq!(parsed["block_id"], "callout-tip");
        assert_eq!(parsed["native_skill_name"], "visual-spec");
        assert_eq!(parsed["native_skill_version"], "0.1.0");
        assert_eq!(parsed["range_start_byte"], 12);
        assert_eq!(parsed["range_end_byte"], 48);
    }

    /// Two successive material updates — each carrying distinct metadata —
    /// must each land on their own revision row, with `latest_revision_seq`
    /// advancing once per patch. This is the per-patch attribution contract
    /// the design pins (one revision per targeted block-patch, not a
    /// monolithic body rewrite).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_targeted_patches_persist_two_distinct_revisions() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Multi Patch")).await.unwrap();

        let first = serde_json::json!({
            "change_kind": "targeted_block_patch",
            "block_id": "callout-tip",
            "native_skill_name": "visual-spec",
            "native_skill_version": "0.1.0",
        });
        repo.update(
            &p.id,
            ProposalUpdateInput {
                title: "Multi Patch",
                body: "patch-1 body",
                acceptance_criteria: "[]",
                status: "draft",
                superseded_by: None,
                body_format: Some("mdx"),
                event_metadata: Some(&first),
            },
        )
        .await
        .unwrap();

        let second = serde_json::json!({
            "change_kind": "targeted_block_patch",
            "block_id": "metric-tile",
            "selector": "section: '## Metrics'",
            "native_skill_name": "visual-spec",
            "native_skill_version": "0.1.0",
        });
        repo.update(
            &p.id,
            ProposalUpdateInput {
                title: "Multi Patch",
                body: "patch-1 + patch-2 body",
                acceptance_criteria: "[]",
                status: "draft",
                superseded_by: None,
                body_format: Some("mdx"),
                event_metadata: Some(&second),
            },
        )
        .await
        .unwrap();

        let updated = repo.get(&p.id).await.unwrap().expect("proposal row");
        assert_eq!(updated.latest_revision_seq, 3, "seed + two patches = 3");

        let revisions = repo.revisions(&p.id).await.unwrap();
        assert_eq!(revisions.len(), 3);
        assert!(revisions[0].event_metadata.is_none(), "seed stays NULL");
        let r1: serde_json::Value = serde_json::from_str(
            revisions[1]
                .event_metadata
                .as_deref()
                .expect("first patch metadata"),
        )
        .unwrap();
        assert_eq!(r1["block_id"], "callout-tip");
        let r2: serde_json::Value = serde_json::from_str(
            revisions[2]
                .event_metadata
                .as_deref()
                .expect("second patch metadata"),
        )
        .unwrap();
        assert_eq!(r2["block_id"], "metric-tile");
        assert_eq!(r2["selector"], "section: '## Metrics'");
    }

    /// `dangling_refinement_proposal_ids` reports a proposal exactly while it
    /// has more `refinement_start` than `refinement_stop` lifecycle rows — the
    /// signal startup recovery uses to reconcile refinements lost across a
    /// restart.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dangling_refinement_ids_track_unmatched_start() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo
            .create(create_input("Dangling Refinement"))
            .await
            .unwrap();

        // No refinement lifecycle yet → not dangling.
        assert!(
            repo.dangling_refinement_proposal_ids()
                .await
                .unwrap()
                .is_empty()
        );

        // A start with no matching stop → dangling.
        repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
            .await
            .unwrap();
        assert_eq!(
            repo.dangling_refinement_proposal_ids().await.unwrap(),
            vec![p.id.clone()]
        );

        // An awaiting_review event does not balance the start → still dangling.
        repo.record_refinement_lifecycle(&p.id, "refinement_awaiting_review", None)
            .await
            .unwrap();
        assert_eq!(
            repo.dangling_refinement_proposal_ids().await.unwrap(),
            vec![p.id.clone()]
        );

        // A matching stop → balanced → no longer dangling.
        repo.record_refinement_lifecycle(&p.id, "refinement_stop", None)
            .await
            .unwrap();
        assert!(
            repo.dangling_refinement_proposal_ids()
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reviewer_feedback_helper_ignores_stale_revision_feedback() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo
            .create(create_input("Reviewer Feedback"))
            .await
            .unwrap();
        let stale_feedback = "stale feedback from previous revision 019f0fed";
        let current_feedback = "current feedback for latest revision 019f0fed";

        repo.record_refinement_lifecycle(
            &p.id,
            "refinement_start",
            Some(&serde_json::json!({
                "source": "human_demand_round",
                "reviewer_feedback": stale_feedback,
                "reason": stale_feedback,
            })),
        )
        .await
        .unwrap();
        assert_eq!(
            repo.latest_current_revision_reviewer_feedback(&p.id)
                .await
                .unwrap()
                .as_deref(),
            Some(stale_feedback)
        );

        repo.update(
            &p.id,
            ProposalUpdateInput {
                title: &p.title,
                body: "new body advances revision",
                acceptance_criteria: &p.acceptance_criteria,
                status: "draft",
                superseded_by: None,
                body_format: None,
                event_metadata: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            repo.latest_current_revision_reviewer_feedback(&p.id)
                .await
                .unwrap(),
            None,
            "feedback from an older seq must not be considered current"
        );

        repo.record_refinement_lifecycle(
            &p.id,
            "refinement_start",
            Some(&serde_json::json!({
                "source": "human_demand_round",
                "reviewer_feedback": current_feedback,
                "reason": current_feedback,
            })),
        )
        .await
        .unwrap();
        assert_eq!(
            repo.latest_current_revision_reviewer_feedback(&p.id)
                .await
                .unwrap()
                .as_deref(),
            Some(current_feedback)
        );
    }

    /// Status-only updates and the `status_change` audit events they emit
    /// must keep `event_metadata` NULL. The audit trail of lifecycle
    /// transitions is not authoring metadata and should not be conflated
    /// with the targeted-patch contract.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn status_only_event_keeps_event_metadata_null() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Status Only")).await.unwrap();
        // Move the proposal to `done` (status-only path that triggers a
        // `status_change` audit row in addition to the create seed).
        repo.update(
            &p.id,
            ProposalUpdateInput {
                title: &p.title,
                body: &p.body,
                acceptance_criteria: &p.acceptance_criteria,
                status: "done",
                superseded_by: None,
                body_format: None,
                event_metadata: None,
            },
        )
        .await
        .unwrap();
        let revisions = repo.revisions(&p.id).await.unwrap();
        // seed (spec_revision) + status_change audit
        assert_eq!(revisions.len(), 2);
        assert_eq!(revisions[1].event_kind, "status_change");
        assert!(
            revisions[1].event_metadata.is_none(),
            "status_change rows must leave event_metadata NULL"
        );
    }

    // ── Debate trail tests ──────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn debate_trail_append_and_list_ordered() {
        let (bus, captured) = capturing_bus();
        let repo = ProposalRepository::new(test_db(), bus);
        let p = repo.create(create_input("Trail")).await.unwrap();
        captured.lock().unwrap().clear();

        // Append three entries in mixed order; list should return round then created_at.
        let obj = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "objection",
                body: "too broad",
                blocking: true,
                agent_role: "adversary",
                author_kind: "agent",
                author_model: Some("claude-opus-4-8"),
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: None,
            })
            .await
            .unwrap();
        assert_eq!(obj.kind, "objection");
        assert!(obj.blocking);
        assert_eq!(obj.round, 1);
        assert!(obj.resolved_at.is_none());
        assert!(obj.reopened_at.is_none());

        let reb = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "rebuttal",
                body: "scope is fine because...",
                blocking: false,
                agent_role: "advocate",
                author_kind: "agent",
                author_model: Some("claude-opus-4-8"),
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: None,
            })
            .await
            .unwrap();
        assert_eq!(reb.kind, "rebuttal");

        let verdict = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "verdict",
                body: "narrow scope to X",
                blocking: false,
                agent_role: "judge",
                author_kind: "agent",
                author_model: Some("claude-opus-4-8"),
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: None,
            })
            .await
            .unwrap();

        let trail = repo.debate_trail(&p.id).await.unwrap();
        assert_eq!(trail.len(), 3);
        // Ordered by round then created_at; ids are UUIDv7 so created_at ordering
        // is deterministic within the same millisecond.
        assert_eq!(trail[0].id, obj.id);
        assert_eq!(trail[1].id, reb.id);
        assert_eq!(trail[2].id, verdict.id);

        // get by id works
        let fetched = repo.get_debate_trail_entry(&obj.id).await.unwrap().unwrap();
        assert_eq!(fetched.id, obj.id);

        // events fired for each append
        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 3);
        assert!(
            events
                .iter()
                .all(|e| e.entity_type == "proposal_debate_trail")
        );
        assert!(events.iter().all(|e| e.action == "created"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn debate_trail_isolation_by_proposal() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p1 = repo.create(create_input("One")).await.unwrap();
        let p2 = repo.create(create_input("Two")).await.unwrap();

        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &p1.id,
            kind: "objection",
            body: "obj-1",
            blocking: false,
            agent_role: "adversary",
            author_kind: "agent",
            author_model: None,
            source_task_id: None,
            against_revision_seq: 1,
            round: 1,
            body_metadata: None,
        })
        .await
        .unwrap();

        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &p2.id,
            kind: "rebuttal",
            body: "reb-2",
            blocking: false,
            agent_role: "advocate",
            author_kind: "agent",
            author_model: None,
            source_task_id: None,
            against_revision_seq: 1,
            round: 1,
            body_metadata: None,
        })
        .await
        .unwrap();

        let trail1 = repo.debate_trail(&p1.id).await.unwrap();
        let trail2 = repo.debate_trail(&p2.id).await.unwrap();
        assert_eq!(trail1.len(), 1);
        assert_eq!(trail1[0].body, "obj-1");
        assert_eq!(trail2.len(), 1);
        assert_eq!(trail2[0].body, "reb-2");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn debate_trail_resolve_and_reopen() {
        let (bus, captured) = capturing_bus();
        let repo = ProposalRepository::new(test_db(), bus);
        let p = repo.create(create_input("Resolve")).await.unwrap();
        captured.lock().unwrap().clear();

        let entry = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "objection",
                body: "blocking issue",
                blocking: true,
                agent_role: "adversary",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: None,
            })
            .await
            .unwrap();

        // Resolve it.
        let resolved = repo.resolve_debate_trail_entry(&entry.id).await.unwrap();
        assert!(resolved.resolved_at.is_some());
        assert!(resolved.reopened_at.is_none());

        // Reopen it.
        let reopened = repo.reopen_debate_trail_entry(&entry.id).await.unwrap();
        assert!(reopened.resolved_at.is_some());
        assert!(reopened.reopened_at.is_some());

        // Re-resolve clears reopen state.
        let re_resolved = repo.resolve_debate_trail_entry(&entry.id).await.unwrap();
        assert!(re_resolved.resolved_at.is_some());
        assert!(re_resolved.reopened_at.is_none());
        assert!(re_resolved.reopened_by_user_id.is_none());

        let events = captured.lock().unwrap();
        // 1 created + 3 updates = 4 events
        assert_eq!(events.len(), 4);
        assert!(
            events
                .iter()
                .all(|e| e.entity_type == "proposal_debate_trail")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn judge_verdicts_never_count_as_unresolved_blocking() {
        // Regression (gate-verdict-supersession): a judge's blocking reject
        // verdict is never "resolved" — nothing clears it when a later approve
        // verdict supersedes it. Counting verdict rows as unresolved blocking
        // double-counts a signal that already gates through `latest_judge_verdict`
        // and would block the gate forever. Verdict rows must be excluded from
        // the unresolved-blocking set regardless of resolution/reopen state.
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo
            .create(create_input("Verdict supersession"))
            .await
            .unwrap();

        for (round, body) in [
            (1, "needs-work: unclear on X"),
            (2, "needs-work: still unclear"),
            (3, "needs-work: one more round"),
        ] {
            repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "verdict",
                body,
                blocking: true,
                agent_role: "judge",
                author_kind: "agent",
                author_model: Some("test-judge"),
                source_task_id: None,
                against_revision_seq: 1,
                round,
                body_metadata: None,
            })
            .await
            .unwrap();
        }

        // Even with three blocking reject verdicts and no resolution, the
        // unresolved-blocking set excludes verdict rows entirely.
        let unresolved = repo
            .list_unresolved_blocking_debate_entries(&p.id)
            .await
            .unwrap();
        assert!(
            unresolved.is_empty(),
            "judge verdict rows must not appear in unresolved-blocking set: {unresolved:?}"
        );

        // A later approve verdict is the latest verdict, superseding the rejects.
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &p.id,
            kind: "verdict",
            body: "Ready",
            blocking: false,
            agent_role: "judge",
            author_kind: "agent",
            author_model: Some("test-judge"),
            source_task_id: None,
            against_revision_seq: 1,
            round: 4,
            body_metadata: None,
        })
        .await
        .unwrap();

        let latest = repo.latest_judge_verdict(&p.id).await.unwrap().unwrap();
        assert_eq!(latest.body, "Ready");

        // A non-verdict blocking objection still counts (verdict exclusion is
        // narrow to `kind = 'verdict'`).
        let objection = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "objection",
                body: "Missing error handling",
                blocking: true,
                agent_role: "adversary",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 4,
                body_metadata: None,
            })
            .await
            .unwrap();
        let unresolved = repo
            .list_unresolved_blocking_debate_entries(&p.id)
            .await
            .unwrap();
        assert_eq!(unresolved.len(), 1, "objection must still count");
        assert_eq!(unresolved[0].id, objection.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn advance_draft_to_in_review_is_idempotent_and_status_scoped() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Park draft")).await.unwrap();
        assert_eq!(p.status, "draft");

        // First call transitions draft → in_review and records a status_change.
        let changed = repo.advance_draft_to_in_review(&p.id).await.unwrap();
        assert!(changed, "draft should advance to in_review");
        let after = repo.get(&p.id).await.unwrap().unwrap();
        assert_eq!(after.status, "in_review");

        let revisions = repo.revisions(&p.id).await.unwrap();
        let status_event = revisions
            .iter()
            .find(|r| r.event_kind == "status_change")
            .expect("a status_change revision event should be recorded");
        assert_eq!(status_event.status_from.as_deref(), Some("draft"));
        assert_eq!(status_event.status_to.as_deref(), Some("in_review"));

        // Second call is a no-op (idempotent): no transition, no extra event.
        let changed_again = repo.advance_draft_to_in_review(&p.id).await.unwrap();
        assert!(!changed_again, "already in_review — no transition");
        let status_events = repo
            .revisions(&p.id)
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.event_kind == "status_change")
            .count();
        assert_eq!(status_events, 1, "no duplicate status_change event");

        // A non-draft proposal is left untouched (status-scoped).
        let other = repo.create(create_input("Approved already")).await.unwrap();
        repo.set_status(&other.id, "approved").await.unwrap();
        let changed_other = repo.advance_draft_to_in_review(&other.id).await.unwrap();
        assert!(!changed_other, "approved proposal must not be touched");
        assert_eq!(
            repo.get(&other.id).await.unwrap().unwrap().status,
            "approved"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn debate_trail_invalid_kind_rejected() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Invalid")).await.unwrap();

        let err = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "comment",
                body: "nope",
                blocking: false,
                agent_role: "advocate",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: None,
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("invalid debate trail kind"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn debate_trail_proposal_must_exist() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());

        let err = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: "nonexistent-id",
                kind: "objection",
                body: "nope",
                blocking: false,
                agent_role: "adversary",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: None,
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("proposal not found"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn debate_trail_multiround_ordering() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Rounds")).await.unwrap();

        // Round 1
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &p.id,
            kind: "objection",
            body: "r1-obj",
            blocking: true,
            agent_role: "adversary",
            author_kind: "agent",
            author_model: None,
            source_task_id: None,
            against_revision_seq: 1,
            round: 1,
            body_metadata: None,
        })
        .await
        .unwrap();

        // Round 2
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &p.id,
            kind: "rebuttal",
            body: "r2-reb",
            blocking: false,
            agent_role: "advocate",
            author_kind: "agent",
            author_model: None,
            source_task_id: None,
            against_revision_seq: 2,
            round: 2,
            body_metadata: None,
        })
        .await
        .unwrap();

        let trail = repo.debate_trail(&p.id).await.unwrap();
        assert_eq!(trail.len(), 2);
        assert_eq!(trail[0].round, 1);
        assert_eq!(trail[0].body, "r1-obj");
        assert_eq!(trail[1].round, 2);
        assert_eq!(trail[1].body, "r2-reb");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn existing_feedback_crud_unaffected_by_debate_trail() {
        // Verify that adding debate-trail entries does not interfere with
        // existing proposal_feedback operations.
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Both")).await.unwrap();

        // Add a feedback entry.
        let fb = repo
            .add_feedback(ProposalFeedbackCreateInput {
                proposal_id: &p.id,
                parent_id: None,
                author_kind: "user",
                author_model: None,
                body: "human comment",
            })
            .await
            .unwrap();
        assert!(fb.resolved_at.is_none());

        // Add a debate trail entry.
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &p.id,
            kind: "objection",
            body: "ai objection",
            blocking: true,
            agent_role: "adversary",
            author_kind: "agent",
            author_model: None,
            source_task_id: None,
            against_revision_seq: 1,
            round: 1,
            body_metadata: None,
        })
        .await
        .unwrap();

        // Feedback still works independently.
        let feedbacks = repo.feedback(&p.id).await.unwrap();
        assert_eq!(feedbacks.len(), 1);
        assert_eq!(feedbacks[0].body, "human comment");

        let resolved = repo.set_feedback_resolved(&fb.id, Some(2)).await.unwrap();
        assert!(resolved.resolved_at.is_some());
        assert_eq!(resolved.resolved_revision_seq, Some(2));

        // Debate trail is still separate.
        let trail = repo.debate_trail(&p.id).await.unwrap();
        assert_eq!(trail.len(), 1);
        assert_eq!(trail[0].body, "ai objection");
    }

    // ── Needs-evidence structured claim tests ──────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_structured_needs_evidence_spike_round_trips_claim_json() {
        let (bus, captured) = capturing_bus();
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), bus);
        let p = repo.create(create_input("Structured Claim")).await.unwrap();
        captured.lock().unwrap().clear();

        let proj = insert_project(&db, "svc-spike").await;
        let epic = insert_epic(&db, &proj, "sp01").await;
        let spike_id = insert_task(&db, &proj, &epic, "spike-01").await;

        let claim = NeedsEvidenceClaim {
            question: "Can X handle 10k rps?".to_owned(),
            target_subsystem: "svc-payment".to_owned(),
            spec_unknown_anchor: "section 3.2 throughput".to_owned(),
            insufficient_in_session_research: "only checked config, not runtime".to_owned(),
            expected_findings: "load test results or queue depth proof".to_owned(),
            round: 2,
            against_revision_seq: 3,
            created_by_task_id: uuid::Uuid::now_v7().to_string(),
        };

        let updated = repo
            .set_structured_needs_evidence_spike(&p.id, &spike_id, &claim)
            .await
            .unwrap();

        // Status moved to draft and spike is linked.
        assert_eq!(updated.status, "draft");
        assert_eq!(
            updated.linked_spike_task_id.as_deref(),
            Some(spike_id.as_str())
        );

        // The stored JSON round-trips back to the typed struct.
        let stored_json = updated
            .needs_evidence_claim
            .as_deref()
            .expect("needs_evidence_claim must be set");
        let parsed: NeedsEvidenceClaim =
            serde_json::from_str(stored_json).expect("stored claim must be valid JSON");
        assert_eq!(parsed, claim);

        // The parse_stored helper also works.
        let via_helper = NeedsEvidenceClaim::parse_stored(Some(stored_json))
            .expect("parse_stored must succeed")
            .expect("parse_stored must return Some");
        assert_eq!(via_helper, claim);

        // find_by_linked_spike resolves.
        let found = repo
            .find_by_linked_spike(&spike_id)
            .await
            .unwrap()
            .expect("must find by linked spike");
        assert_eq!(found.id, p.id);

        // has_open_needs_evidence_spike returns true.
        assert!(repo.has_open_needs_evidence_spike(&p.id).await.unwrap());

        // Events fired.
        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "proposal");
        assert_eq!(events[0].action, "updated");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_structured_needs_evidence_spike_clear_and_reparse() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo.create(create_input("Clear Claim")).await.unwrap();
        let proj = insert_project(&db, "svc-clear").await;
        let epic = insert_epic(&db, &proj, "cl01").await;
        let spike_id = insert_task(&db, &proj, &epic, "clear-01").await;

        let claim = NeedsEvidenceClaim {
            question: "Is Y thread-safe?".to_owned(),
            target_subsystem: "core-cache".to_owned(),
            spec_unknown_anchor: "section 5.1 concurrency".to_owned(),
            insufficient_in_session_research: "grep found no mutex usage".to_owned(),
            expected_findings: "lock-ordering analysis".to_owned(),
            round: 1,
            against_revision_seq: 1,
            created_by_task_id: uuid::Uuid::now_v7().to_string(),
        };

        repo.set_structured_needs_evidence_spike(&p.id, &spike_id, &claim)
            .await
            .unwrap();

        // Confirm claim is populated.
        let stored = repo.get(&p.id).await.unwrap().unwrap();
        assert!(stored.needs_evidence_claim.is_some());
        assert!(stored.linked_spike_task_id.is_some());

        // Clear it.
        let cleared = repo.clear_needs_evidence_spike(&p.id).await.unwrap();
        assert!(cleared.needs_evidence_claim.is_none());
        assert!(cleared.linked_spike_task_id.is_none());
        assert!(!repo.has_open_needs_evidence_spike(&p.id).await.unwrap());

        // parse_stored on None returns None.
        assert!(NeedsEvidenceClaim::parse_stored(None).unwrap().is_none());
        assert!(
            NeedsEvidenceClaim::parse_stored(Some(""))
                .unwrap()
                .is_none()
        );

        // parse_stored on invalid JSON returns Err.
        assert!(NeedsEvidenceClaim::parse_stored(Some("{bad")).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn legacy_string_set_needs_evidence_spike_still_works() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo.create(create_input("Legacy")).await.unwrap();
        let proj = insert_project(&db, "svc-legacy").await;
        let epic = insert_epic(&db, &proj, "lg01").await;
        let spike_id = insert_task(&db, &proj, &epic, "legacy-01").await;

        // The opaque-string path still works.
        let updated = repo
            .set_needs_evidence_spike(&p.id, &spike_id, "X is load-bearing")
            .await
            .unwrap();
        assert_eq!(updated.status, "draft");
        assert_eq!(
            updated.needs_evidence_claim.as_deref(),
            Some("X is load-bearing")
        );
        assert_eq!(
            updated.linked_spike_task_id.as_deref(),
            Some(spike_id.as_str())
        );

        // It's not parseable as a NeedsEvidenceClaim (it's a plain string),
        // but parse_stored returns an Err (not a panic).
        let result = NeedsEvidenceClaim::parse_stored(updated.needs_evidence_claim.as_deref());
        assert!(result.is_err(), "opaque string must fail structured parse");
    }

    // ── Needs-evidence debate entry tests ────────────────────────────────

    /// A `needs_evidence` debate entry with valid linkage metadata is
    /// accepted, persisted, and the body_metadata round-trips through the
    /// stored row.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn needs_evidence_debate_entry_accepted_with_valid_metadata() {
        let (bus, captured) = capturing_bus();
        let repo = ProposalRepository::new(test_db(), bus);
        let p = repo.create(create_input("NE Entry")).await.unwrap();
        captured.lock().unwrap().clear();

        let judge_task_id = uuid::Uuid::now_v7().to_string();
        let spike_task_id = uuid::Uuid::now_v7().to_string();

        let link = NeedsEvidenceClaimLink {
            kind: NeedsEvidenceClaimLink::KIND_MARKER.to_owned(),
            proposal_id: p.id.clone(),
            judge_task_id: judge_task_id.clone(),
            spike_task_id: spike_task_id.clone(),
            round: 2,
            against_revision_seq: 3,
        };
        let meta_value = link.to_value();

        let entry = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "needs_evidence",
                body: "Need to verify throughput claim",
                blocking: true,
                agent_role: "judge",
                author_kind: "agent",
                author_model: Some("claude-opus-4-8"),
                source_task_id: Some(&judge_task_id),
                against_revision_seq: 3,
                round: 2,
                body_metadata: Some(&meta_value),
            })
            .await
            .unwrap();

        // Verify the entry persisted correctly.
        assert_eq!(entry.kind, "needs_evidence");
        assert!(entry.blocking);
        assert_eq!(entry.agent_role, "judge");
        assert_eq!(entry.round, 2);
        assert_eq!(entry.against_revision_seq, 3);
        assert_eq!(
            entry.source_task_id.as_deref(),
            Some(judge_task_id.as_str())
        );
        assert!(entry.resolved_at.is_none());

        // Verify body_metadata round-trips.
        let stored_meta_str = entry
            .body_metadata
            .as_ref()
            .expect("body_metadata must be set on needs_evidence entry");
        let stored_meta: serde_json::Value =
            serde_json::from_str(stored_meta_str).expect("body_metadata must be valid JSON");
        let parsed_link = NeedsEvidenceClaimLink::from_metadata(&stored_meta)
            .expect("stored body_metadata must parse back to NeedsEvidenceClaimLink");
        assert_eq!(parsed_link.proposal_id, p.id);
        assert_eq!(parsed_link.judge_task_id, judge_task_id);
        assert_eq!(parsed_link.spike_task_id, spike_task_id);
        assert_eq!(parsed_link.round, 2);
        assert_eq!(parsed_link.against_revision_seq, 3);
        assert_eq!(parsed_link.kind, NeedsEvidenceClaimLink::KIND_MARKER);

        // Verify it appears in unresolved_blocking_entries (blocking=true, resolved_at IS NULL).
        let unresolved = repo.unresolved_blocking_entries(&p.id).await.unwrap();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].id, entry.id);

        // Verify it appears in needs_evidence_entries.
        let ne_entries = repo.needs_evidence_entries(&p.id).await.unwrap();
        assert_eq!(ne_entries.len(), 1);
        assert_eq!(ne_entries[0].id, entry.id);

        // Events fired.
        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "proposal_debate_trail");
        assert_eq!(events[0].action, "created");
    }

    /// A `needs_evidence` entry with `agent_role != "judge"` is rejected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn needs_evidence_rejects_wrong_agent_role() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Role Reject")).await.unwrap();

        let link = NeedsEvidenceClaimLink {
            kind: NeedsEvidenceClaimLink::KIND_MARKER.to_owned(),
            proposal_id: p.id.clone(),
            judge_task_id: uuid::Uuid::now_v7().to_string(),
            spike_task_id: uuid::Uuid::now_v7().to_string(),
            round: 1,
            against_revision_seq: 1,
        };
        let meta_value = link.to_value();

        let err = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "needs_evidence",
                body: "test",
                blocking: true,
                agent_role: "advocate",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: Some(&meta_value),
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("agent_role = \"judge\""));
    }

    /// A `needs_evidence` entry with `blocking = false` is rejected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn needs_evidence_rejects_non_blocking() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Block Reject")).await.unwrap();

        let link = NeedsEvidenceClaimLink {
            kind: NeedsEvidenceClaimLink::KIND_MARKER.to_owned(),
            proposal_id: p.id.clone(),
            judge_task_id: uuid::Uuid::now_v7().to_string(),
            spike_task_id: uuid::Uuid::now_v7().to_string(),
            round: 1,
            against_revision_seq: 1,
        };
        let meta_value = link.to_value();

        let err = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "needs_evidence",
                body: "test",
                blocking: false,
                agent_role: "judge",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: Some(&meta_value),
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("blocking = true"));
    }

    /// A `needs_evidence` entry without `body_metadata` is rejected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn needs_evidence_rejects_missing_metadata() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("No Meta")).await.unwrap();

        let err = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "needs_evidence",
                body: "test",
                blocking: true,
                agent_role: "judge",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: None,
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("body_metadata"));
    }

    /// A `needs_evidence` entry with malformed metadata (wrong kind marker,
    /// missing fields, empty strings) is rejected with clear errors.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn needs_evidence_rejects_malformed_metadata() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Malformed Meta")).await.unwrap();

        // Wrong kind marker.
        let bad_kind = serde_json::json!({
            "kind": "wrong_kind",
            "proposal_id": p.id,
            "judge_task_id": "j1",
            "spike_task_id": "s1",
            "round": 1,
            "against_revision_seq": 1,
        });
        let err = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "needs_evidence",
                body: "test",
                blocking: true,
                agent_role: "judge",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: Some(&bad_kind),
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("kind mismatch"));

        // Missing required field (no spike_task_id).
        let missing_field = serde_json::json!({
            "kind": NeedsEvidenceClaimLink::KIND_MARKER,
            "proposal_id": p.id,
            "judge_task_id": "j1",
            "round": 1,
            "against_revision_seq": 1,
        });
        let err = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "needs_evidence",
                body: "test",
                blocking: true,
                agent_role: "judge",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: Some(&missing_field),
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("spike_task_id"));

        // Empty proposal_id.
        let empty_pid = serde_json::json!({
            "kind": NeedsEvidenceClaimLink::KIND_MARKER,
            "proposal_id": "  ",
            "judge_task_id": "j1",
            "spike_task_id": "s1",
            "round": 1,
            "against_revision_seq": 1,
        });
        let err = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "needs_evidence",
                body: "test",
                blocking: true,
                agent_role: "judge",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: Some(&empty_pid),
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("non-empty"));

        // round <= 0.
        let bad_round = serde_json::json!({
            "kind": NeedsEvidenceClaimLink::KIND_MARKER,
            "proposal_id": p.id,
            "judge_task_id": "j1",
            "spike_task_id": "s1",
            "round": 0,
            "against_revision_seq": 1,
        });
        let err = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "needs_evidence",
                body: "test",
                blocking: true,
                agent_role: "judge",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: Some(&bad_round),
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("round must be >= 1"));
    }

    // ── Evidence findings debate entry tests ──────────────────────────────

    /// An `evidence_findings` debate entry with valid structured findings is
    /// accepted and the body_metadata round-trips through the stored row.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn evidence_findings_debate_entry_accepted_with_valid_payload() {
        let (bus, captured) = capturing_bus();
        let repo = ProposalRepository::new(test_db(), bus);
        let p = repo.create(create_input("Findings")).await.unwrap();
        captured.lock().unwrap().clear();

        let spike_task_id = uuid::Uuid::now_v7().to_string();

        let findings = EvidenceFindings {
            answer: "The throughput requirement is achievable with the current architecture."
                .to_owned(),
            evidence: vec![
                "Ran load test with k6: sustained 12k rps".to_owned(),
                "Queue depth peaked at 200, well within limits".to_owned(),
            ],
            code_paths_inspected: vec![
                "src/payment/handler.rs".to_owned(),
                "src/queue/processor.rs".to_owned(),
            ],
            confidence: 0.85,
            residual_risks: vec!["Untested under memory pressure".to_owned()],
            recommendation_for_advocate:
                "Cite the k6 results in the spec; note memory pressure gap.".to_owned(),
        };
        let findings_value = serde_json::to_value(&findings).unwrap();

        let entry = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "evidence_findings",
                body: "Spike completed: throughput verified",
                blocking: false,
                agent_role: "spike",
                author_kind: "agent",
                author_model: Some("claude-opus-4-8"),
                source_task_id: Some(&spike_task_id),
                against_revision_seq: 3,
                round: 2,
                body_metadata: Some(&findings_value),
            })
            .await
            .unwrap();

        // Verify entry properties.
        assert_eq!(entry.kind, "evidence_findings");
        assert!(!entry.blocking);
        assert_eq!(entry.agent_role, "spike");
        assert_eq!(entry.round, 2);
        assert_eq!(entry.against_revision_seq, 3);
        assert_eq!(
            entry.source_task_id.as_deref(),
            Some(spike_task_id.as_str())
        );
        assert!(entry.resolved_at.is_none());

        // Verify body_metadata round-trips to the typed findings struct.
        let stored_meta_str = entry
            .body_metadata
            .as_ref()
            .expect("body_metadata must be set on evidence_findings entry");
        let parsed = EvidenceFindings::parse_stored(Some(stored_meta_str))
            .expect("body_metadata must parse as EvidenceFindings")
            .expect("must return Some");
        assert_eq!(parsed, findings);
        assert_eq!(parsed.confidence, 0.85);
        assert_eq!(parsed.code_paths_inspected.len(), 2);
        assert_eq!(parsed.evidence.len(), 2);

        // Verify it does NOT appear in unresolved_blocking_entries (blocking=false).
        let unresolved = repo.unresolved_blocking_entries(&p.id).await.unwrap();
        assert!(
            unresolved.is_empty(),
            "evidence_findings is non-blocking, must not appear in unresolved blocking"
        );

        // Events fired.
        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "proposal_debate_trail");
        assert_eq!(events[0].action, "created");
    }

    /// An `evidence_findings` entry with `agent_role != "spike"` is rejected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn evidence_findings_rejects_wrong_agent_role() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Spike Role")).await.unwrap();

        let findings = serde_json::json!({
            "answer": "yes",
            "evidence": [],
            "code_paths_inspected": [],
            "confidence": 0.9,
            "residual_risks": [],
            "recommendation_for_advocate": "proceed",
        });

        let err = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "evidence_findings",
                body: "test",
                blocking: false,
                agent_role: "judge",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: Some(&findings),
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("agent_role = \"spike\""));
    }

    /// An `evidence_findings` entry with `blocking = true` is rejected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn evidence_findings_rejects_blocking() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Block Spike")).await.unwrap();

        let findings = serde_json::json!({
            "answer": "yes",
            "evidence": [],
            "code_paths_inspected": [],
            "confidence": 0.9,
            "residual_risks": [],
            "recommendation_for_advocate": "proceed",
        });

        let err = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "evidence_findings",
                body: "test",
                blocking: true,
                agent_role: "spike",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: Some(&findings),
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("blocking = false"));
    }

    /// An `evidence_findings` entry without `body_metadata` is rejected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn evidence_findings_rejects_missing_body_metadata() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("No Findings Meta")).await.unwrap();

        let err = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "evidence_findings",
                body: "test",
                blocking: false,
                agent_role: "spike",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: None,
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("body_metadata"));
    }

    /// An `evidence_findings` entry with malformed findings is rejected.
    /// Tests empty answer, missing fields, and out-of-range confidence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn evidence_findings_rejects_malformed_payload() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Bad Findings")).await.unwrap();

        // Empty answer.
        let empty_answer = serde_json::json!({
            "answer": "  ",
            "evidence": [],
            "code_paths_inspected": [],
            "confidence": 0.5,
            "residual_risks": [],
            "recommendation_for_advocate": "proceed",
        });
        let err = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "evidence_findings",
                body: "test",
                blocking: false,
                agent_role: "spike",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: Some(&empty_answer),
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("answer must be non-empty"));

        // Out-of-range confidence (> 1.0).
        let bad_confidence = serde_json::json!({
            "answer": "valid answer",
            "evidence": [],
            "code_paths_inspected": [],
            "confidence": 1.5,
            "residual_risks": [],
            "recommendation_for_advocate": "proceed",
        });
        let err = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "evidence_findings",
                body: "test",
                blocking: false,
                agent_role: "spike",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: Some(&bad_confidence),
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("confidence"));

        // Negative confidence.
        let neg_confidence = serde_json::json!({
            "answer": "valid answer",
            "evidence": [],
            "code_paths_inspected": [],
            "confidence": -0.1,
            "residual_risks": [],
            "recommendation_for_advocate": "proceed",
        });
        let err = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "evidence_findings",
                body: "test",
                blocking: false,
                agent_role: "spike",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: Some(&neg_confidence),
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("confidence"));

        // Empty recommendation_for_advocate.
        let empty_rec = serde_json::json!({
            "answer": "valid answer",
            "evidence": [],
            "code_paths_inspected": [],
            "confidence": 0.8,
            "residual_risks": [],
            "recommendation_for_advocate": "   ",
        });
        let err = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "evidence_findings",
                body: "test",
                blocking: false,
                agent_role: "spike",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: Some(&empty_rec),
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("recommendation_for_advocate must be non-empty"));

        // Missing required field entirely (no answer field).
        let missing_field = serde_json::json!({
            "evidence": [],
            "code_paths_inspected": [],
            "confidence": 0.8,
            "residual_risks": [],
            "recommendation_for_advocate": "proceed",
        });
        let err = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "evidence_findings",
                body: "test",
                blocking: false,
                agent_role: "spike",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: Some(&missing_field),
            })
            .await
            .unwrap_err();
        // serde deserialization should fail because `answer` is missing.
        assert!(format!("{err}").contains("structured findings"));
    }

    /// The `evidence_findings` required schema has all six fields:
    /// `answer`, `evidence`, `code_paths_inspected`, `confidence`,
    /// `residual_risks`, `recommendation_for_advocate`.
    #[test]
    fn evidence_findings_schema_has_all_required_fields() {
        // Build a minimal valid payload and confirm every field is present.
        let findings = EvidenceFindings {
            answer: "The claim is verified.".to_owned(),
            evidence: vec!["grep output shows X".to_owned()],
            code_paths_inspected: vec!["src/main.rs".to_owned()],
            confidence: 0.95,
            residual_risks: vec!["Edge case Y not tested".to_owned()],
            recommendation_for_advocate: "Add caveat Z to spec.".to_owned(),
        };
        findings
            .validate()
            .expect("minimal valid findings must pass validation");

        // Round-trip through serde to verify the JSON shape has all six fields.
        let json = serde_json::to_value(&findings).unwrap();
        assert!(json.get("answer").is_some());
        assert!(json.get("evidence").is_some());
        assert!(json.get("code_paths_inspected").is_some());
        assert!(json.get("confidence").is_some());
        assert!(json.get("residual_risks").is_some());
        assert!(json.get("recommendation_for_advocate").is_some());
    }

    /// Invalid kinds beyond the five recognized ones are still rejected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_debate_kind_still_rejected_after_ne_evidence_extension() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Unknown Kind")).await.unwrap();

        let err = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "observation",
                body: "test",
                blocking: false,
                agent_role: "advocate",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: None,
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("invalid debate trail kind"));
        assert!(format!("{err}").contains("observation"));
    }

    fn sample_evidence_findings(answer: &str) -> EvidenceFindings {
        EvidenceFindings {
            answer: answer.to_owned(),
            evidence: vec!["verified by repository test fixture".to_owned()],
            code_paths_inspected: vec![
                "server/crates/djinn-db/src/repositories/proposal.rs".to_owned(),
            ],
            confidence: 0.9,
            residual_risks: vec!["fixture only".to_owned()],
            recommendation_for_advocate: "incorporate the finding".to_owned(),
        }
    }

    fn sample_needs_evidence_claim(round: i32, against_revision_seq: i32) -> NeedsEvidenceClaim {
        NeedsEvidenceClaim {
            question: "Can the linked spike answer the claim?".to_owned(),
            target_subsystem: "proposal repository".to_owned(),
            spec_unknown_anchor: "evidence handoff".to_owned(),
            insufficient_in_session_research: "requires spike completion".to_owned(),
            expected_findings: "structured evidence_findings".to_owned(),
            round,
            against_revision_seq,
            created_by_task_id: uuid::Uuid::now_v7().to_string(),
        }
    }

    async fn insert_raw_evidence_findings_entry(
        db: &Database,
        proposal_id: &str,
        spike_task_id: &str,
        round: i32,
        against_revision_seq: i32,
        body_metadata: Option<&serde_json::Value>,
    ) -> String {
        db.ensure_initialized().await.unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO proposal_debate_trail
                (id, proposal_id, kind, body, blocking, agent_role, author_kind,
                 author_user_id, author_model, source_task_id,
                 against_revision_seq, round, body_metadata)
             VALUES ($1, $2, 'evidence_findings', 'raw fixture', false, 'spike', 'agent',
                     NULL, NULL, $3, $4, $5, $6)",
        )
        .bind(&id)
        .bind(proposal_id)
        .bind(spike_task_id)
        .bind(against_revision_seq)
        .bind(round)
        .bind(body_metadata)
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn current_evidence_findings_lookup_returns_valid_current_findings() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo.create(create_input("Lookup Valid")).await.unwrap();
        let proj = insert_project(&db, "svc-lookup-valid").await;
        let epic = insert_epic(&db, &proj, "lv01").await;
        let spike_task_id = insert_task(&db, &proj, &epic, "lv-task").await;
        let claim = sample_needs_evidence_claim(2, 3);
        repo.set_structured_needs_evidence_spike(&p.id, &spike_task_id, &claim)
            .await
            .unwrap();

        let findings = sample_evidence_findings("the current spike answer");
        let findings_value = serde_json::to_value(&findings).unwrap();
        let entry = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "evidence_findings",
                body: "current findings body",
                blocking: false,
                agent_role: "spike",
                author_kind: "agent",
                author_model: None,
                source_task_id: Some(&spike_task_id),
                against_revision_seq: claim.against_revision_seq,
                round: claim.round,
                body_metadata: Some(&findings_value),
            })
            .await
            .unwrap();

        let current = repo
            .current_evidence_findings_for_linked_spike(&p.id, &spike_task_id)
            .await
            .unwrap()
            .expect("valid linked findings must be returned");
        assert_eq!(current.proposal_id, p.id);
        assert_eq!(current.spike_task_id, spike_task_id);
        assert_eq!(current.round, claim.round);
        assert_eq!(current.against_revision_seq, claim.against_revision_seq);
        assert_eq!(current.debate_entry_id, entry.id);
        assert_eq!(current.debate_entry_body, "current findings body");
        assert_eq!(current.findings_metadata_json, entry.body_metadata.unwrap());
        assert_eq!(current.findings, findings);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn current_evidence_findings_lookup_returns_none_when_missing() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo.create(create_input("Lookup Missing")).await.unwrap();
        let proj = insert_project(&db, "svc-lookup-missing").await;
        let epic = insert_epic(&db, &proj, "lm01").await;
        let spike_task_id = insert_task(&db, &proj, &epic, "lm-task").await;
        let claim = sample_needs_evidence_claim(2, 3);
        repo.set_structured_needs_evidence_spike(&p.id, &spike_task_id, &claim)
            .await
            .unwrap();

        assert!(
            repo.current_evidence_findings_for_linked_spike(&p.id, &spike_task_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn current_evidence_findings_lookup_returns_none_for_malformed_findings() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo.create(create_input("Lookup Malformed")).await.unwrap();
        let proj = insert_project(&db, "svc-lookup-malformed").await;
        let epic = insert_epic(&db, &proj, "mf01").await;
        let spike_task_id = insert_task(&db, &proj, &epic, "mf-task").await;
        let claim = sample_needs_evidence_claim(2, 3);
        repo.set_structured_needs_evidence_spike(&p.id, &spike_task_id, &claim)
            .await
            .unwrap();
        let malformed = serde_json::json!({
            "answer": "",
            "evidence": [],
            "code_paths_inspected": [],
            "confidence": 0.5,
            "residual_risks": [],
            "recommendation_for_advocate": "proceed"
        });
        insert_raw_evidence_findings_entry(
            &db,
            &p.id,
            &spike_task_id,
            claim.round,
            claim.against_revision_seq,
            Some(&malformed),
        )
        .await;

        assert!(
            repo.current_evidence_findings_for_linked_spike(&p.id, &spike_task_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn current_evidence_findings_lookup_returns_none_for_missing_metadata() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo
            .create(create_input("Lookup Missing Metadata"))
            .await
            .unwrap();
        let proj = insert_project(&db, "svc-lookup-nometa").await;
        let epic = insert_epic(&db, &proj, "nm01").await;
        let spike_task_id = insert_task(&db, &proj, &epic, "nm-task").await;
        let claim = sample_needs_evidence_claim(2, 3);
        repo.set_structured_needs_evidence_spike(&p.id, &spike_task_id, &claim)
            .await
            .unwrap();
        insert_raw_evidence_findings_entry(
            &db,
            &p.id,
            &spike_task_id,
            claim.round,
            claim.against_revision_seq,
            None,
        )
        .await;

        assert!(
            repo.current_evidence_findings_for_linked_spike(&p.id, &spike_task_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn current_evidence_findings_lookup_returns_none_for_wrong_spike() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo
            .create(create_input("Lookup Wrong Spike"))
            .await
            .unwrap();
        let proj = insert_project(&db, "svc-lookup-wspike").await;
        let epic = insert_epic(&db, &proj, "ws01").await;
        let linked_spike_id = insert_task(&db, &proj, &epic, "ws-task").await;
        let other_spike_id = uuid::Uuid::now_v7().to_string();
        let claim = sample_needs_evidence_claim(2, 3);
        repo.set_structured_needs_evidence_spike(&p.id, &linked_spike_id, &claim)
            .await
            .unwrap();

        assert!(
            repo.current_evidence_findings_for_linked_spike(&p.id, &other_spike_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn current_evidence_findings_lookup_returns_none_for_wrong_round() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo
            .create(create_input("Lookup Wrong Round"))
            .await
            .unwrap();
        let proj = insert_project(&db, "svc-lookup-wround").await;
        let epic = insert_epic(&db, &proj, "wr01").await;
        let spike_task_id = insert_task(&db, &proj, &epic, "wr-task").await;
        let claim = sample_needs_evidence_claim(2, 3);
        repo.set_structured_needs_evidence_spike(&p.id, &spike_task_id, &claim)
            .await
            .unwrap();
        let findings = sample_evidence_findings("wrong round");
        let findings_value = serde_json::to_value(&findings).unwrap();
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &p.id,
            kind: "evidence_findings",
            body: "wrong round",
            blocking: false,
            agent_role: "spike",
            author_kind: "agent",
            author_model: None,
            source_task_id: Some(&spike_task_id),
            against_revision_seq: claim.against_revision_seq,
            round: claim.round + 1,
            body_metadata: Some(&findings_value),
        })
        .await
        .unwrap();

        assert!(
            repo.current_evidence_findings_for_linked_spike(&p.id, &spike_task_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn current_evidence_findings_lookup_returns_none_for_wrong_revision() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo
            .create(create_input("Lookup Wrong Revision"))
            .await
            .unwrap();
        let proj = insert_project(&db, "svc-lookup-wrev").await;
        let epic = insert_epic(&db, &proj, "wv01").await;
        let spike_task_id = insert_task(&db, &proj, &epic, "wv-task").await;
        let claim = sample_needs_evidence_claim(2, 3);
        repo.set_structured_needs_evidence_spike(&p.id, &spike_task_id, &claim)
            .await
            .unwrap();
        let findings = sample_evidence_findings("wrong revision");
        let findings_value = serde_json::to_value(&findings).unwrap();
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &p.id,
            kind: "evidence_findings",
            body: "wrong revision",
            blocking: false,
            agent_role: "spike",
            author_kind: "agent",
            author_model: None,
            source_task_id: Some(&spike_task_id),
            against_revision_seq: claim.against_revision_seq + 1,
            round: claim.round,
            body_metadata: Some(&findings_value),
        })
        .await
        .unwrap();

        assert!(
            repo.current_evidence_findings_for_linked_spike(&p.id, &spike_task_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn current_evidence_findings_lookup_returns_none_for_unlinked_proposal() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Lookup Unlinked")).await.unwrap();
        let spike_task_id = uuid::Uuid::now_v7().to_string();

        assert!(
            repo.current_evidence_findings_for_linked_spike(&p.id, &spike_task_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn current_evidence_findings_lookup_returns_none_for_wrongly_linked_proposal() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo
            .create(create_input("Lookup Wrongly Linked"))
            .await
            .unwrap();
        let proj = insert_project(&db, "svc-lookup-wlink").await;
        let epic = insert_epic(&db, &proj, "wl01").await;
        let linked_spike_id = insert_task(&db, &proj, &epic, "wl-task").await;
        let requested_spike_id = uuid::Uuid::now_v7().to_string();
        let claim = sample_needs_evidence_claim(2, 3);
        repo.set_structured_needs_evidence_spike(&p.id, &linked_spike_id, &claim)
            .await
            .unwrap();
        let findings = sample_evidence_findings("requested but not linked");
        let findings_value = serde_json::to_value(&findings).unwrap();
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &p.id,
            kind: "evidence_findings",
            body: "requested spike findings",
            blocking: false,
            agent_role: "spike",
            author_kind: "agent",
            author_model: None,
            source_task_id: Some(&requested_spike_id),
            against_revision_seq: claim.against_revision_seq,
            round: claim.round,
            body_metadata: Some(&findings_value),
        })
        .await
        .unwrap();

        assert!(
            repo.current_evidence_findings_for_linked_spike(&p.id, &requested_spike_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn linked_evidence_spike_recovery_candidates_return_open_and_terminal_task_data() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project = insert_project(&db, &format!("svc-redrive-{}", uuid::Uuid::now_v7())).await;
        let epic = insert_epic(&db, &project, "rd01").await;

        let unlinked = repo
            .create(create_input("Unlinked recovery candidate"))
            .await
            .unwrap();

        let open = repo
            .create(create_input("Open recovery candidate"))
            .await
            .unwrap();
        let open_task = insert_task(&db, &project, &epic, "rd-open").await;
        repo.set_structured_needs_evidence_spike(
            &open.id,
            &open_task,
            &sample_needs_evidence_claim(1, 1),
        )
        .await
        .unwrap();

        let running = repo
            .create(create_input("Running recovery candidate"))
            .await
            .unwrap();
        let running_task = insert_task(&db, &project, &epic, "rd-run").await;
        set_task_status(&db, &running_task, "in_progress", None).await;
        repo.set_structured_needs_evidence_spike(
            &running.id,
            &running_task,
            &sample_needs_evidence_claim(1, 1),
        )
        .await
        .unwrap();

        let completed = repo
            .create(create_input("Completed recovery candidate"))
            .await
            .unwrap();
        let completed_task = insert_task(&db, &project, &epic, "rd-done").await;
        set_task_status(&db, &completed_task, "closed", Some("completed")).await;
        repo.set_structured_needs_evidence_spike(
            &completed.id,
            &completed_task,
            &sample_needs_evidence_claim(1, 1),
        )
        .await
        .unwrap();

        let candidates = repo
            .list_linked_evidence_spike_recovery_candidates()
            .await
            .unwrap();

        assert!(
            !candidates
                .iter()
                .any(|candidate| candidate.proposal_id == unlinked.id),
            "proposals without linked spikes must be excluded"
        );
        let open_candidate = candidates
            .iter()
            .find(|candidate| candidate.proposal_id == open.id)
            .expect("open linked spike candidate must be returned");
        assert_eq!(open_candidate.linked_spike_task_id, open_task);
        assert_eq!(open_candidate.linked_spike_task_status, "open");
        assert_eq!(open_candidate.linked_spike_task_close_reason, None);

        let running_candidate = candidates
            .iter()
            .find(|candidate| candidate.proposal_id == running.id)
            .expect("running linked spike candidate must be returned");
        assert_eq!(running_candidate.linked_spike_task_id, running_task);
        assert_eq!(running_candidate.linked_spike_task_status, "in_progress");
        assert_eq!(running_candidate.linked_spike_task_close_reason, None);

        let completed_candidate = candidates
            .iter()
            .find(|candidate| candidate.proposal_id == completed.id)
            .expect("completed linked spike candidate must be returned");
        assert_eq!(completed_candidate.linked_spike_task_id, completed_task);
        assert_eq!(completed_candidate.linked_spike_task_status, "closed");
        assert_eq!(
            completed_candidate
                .linked_spike_task_close_reason
                .as_deref(),
            Some("completed")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn linked_evidence_spike_recovery_candidates_return_failed_cancelled_and_force_closed_stably()
     {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project = insert_project(
            &db,
            &format!("svc-redrive-terminal-{}", uuid::Uuid::now_v7()),
        )
        .await;
        let epic = insert_epic(&db, &project, "rt01").await;

        let mut expected = Vec::new();
        for (title, short_id, status, close_reason) in [
            (
                "Failed recovery candidate",
                "rt-fail",
                "failed",
                Some("failed"),
            ),
            (
                "Force closed recovery candidate",
                "rt-force",
                "closed",
                Some("force_closed"),
            ),
            (
                "Cancelled recovery candidate",
                "rt-cancel",
                "cancelled",
                Some("cancelled"),
            ),
        ] {
            let proposal = repo.create(create_input(title)).await.unwrap();
            let task_id = insert_task(&db, &project, &epic, short_id).await;
            set_task_status(&db, &task_id, status, close_reason).await;
            repo.set_structured_needs_evidence_spike(
                &proposal.id,
                &task_id,
                &sample_needs_evidence_claim(1, 1),
            )
            .await
            .unwrap();
            expected.push((
                proposal.id,
                task_id,
                status.to_owned(),
                close_reason.map(str::to_owned),
            ));
        }

        let before_proposal_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM proposals")
            .fetch_one(db.pool())
            .await
            .unwrap();
        let before_task_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
            .fetch_one(db.pool())
            .await
            .unwrap();
        let first = repo
            .list_linked_evidence_spike_recovery_candidates()
            .await
            .unwrap();
        let second = repo
            .list_linked_evidence_spike_recovery_candidates()
            .await
            .unwrap();
        assert_eq!(
            first, second,
            "candidate lookup must be stable and read-only"
        );
        let after_proposal_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM proposals")
            .fetch_one(db.pool())
            .await
            .unwrap();
        let after_task_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(before_proposal_count, after_proposal_count);
        assert_eq!(before_task_count, after_task_count);

        for (proposal_id, task_id, status, close_reason) in expected {
            let candidate = first
                .iter()
                .find(|candidate| candidate.proposal_id == proposal_id)
                .expect("terminal-ish linked spike candidate must be returned");
            assert_eq!(candidate.linked_spike_task_id, task_id);
            assert_eq!(candidate.linked_spike_task_status, status);
            assert_eq!(candidate.linked_spike_task_close_reason, close_reason);
        }
    }

    fn count_evidence_lifecycle_events(revisions: &[ProposalRevision], kind: &str) -> usize {
        revisions
            .iter()
            .filter(|rev| rev.event_kind == kind)
            .count()
    }

    async fn setup_linked_evidence_spike(
        db: &Database,
        repo: &ProposalRepository,
        title: &str,
    ) -> (Proposal, String, NeedsEvidenceClaim) {
        let p = repo.create(create_input(title)).await.unwrap();
        let proj = insert_project(db, &format!("svc-terminal-{}", uuid::Uuid::now_v7())).await;
        let epic = insert_epic(db, &proj, "te01").await;
        let spike_task_id = insert_task(db, &proj, &epic, "te-task").await;
        let claim = sample_needs_evidence_claim(2, 3);
        repo.set_structured_needs_evidence_spike(&p.id, &spike_task_id, &claim)
            .await
            .unwrap();
        (
            repo.get(&p.id).await.unwrap().unwrap(),
            spike_task_id,
            claim,
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_linked_spike_success_with_findings_records_received_once() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let (p, spike_task_id, claim) =
            setup_linked_evidence_spike(&db, &repo, "Terminal Success").await;
        let findings = sample_evidence_findings("terminal success");
        let findings_value = serde_json::to_value(&findings).unwrap();
        let entry = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "evidence_findings",
                body: "valid terminal findings",
                blocking: false,
                agent_role: "spike",
                author_kind: "agent",
                author_model: None,
                source_task_id: Some(&spike_task_id),
                against_revision_seq: claim.against_revision_seq,
                round: claim.round,
                body_metadata: Some(&findings_value),
            })
            .await
            .unwrap();

        let outcome = repo
            .persist_terminal_linked_spike_evidence_lifecycle(
                &p.id,
                &spike_task_id,
                "closed",
                Some("completed"),
            )
            .await
            .unwrap();
        assert_eq!(
            outcome,
            TerminalLinkedEvidenceSpikeOutcome::EvidenceReceived
        );

        let repeated = repo
            .persist_terminal_linked_spike_evidence_lifecycle(
                &p.id,
                &spike_task_id,
                "closed",
                Some("completed"),
            )
            .await
            .unwrap();
        assert_eq!(
            repeated,
            TerminalLinkedEvidenceSpikeOutcome::AlreadyRecorded {
                event_kind: evidence_lifecycle_kind::EVIDENCE_RECEIVED.to_owned()
            }
        );

        let revisions = repo.revisions(&p.id).await.unwrap();
        assert_eq!(
            count_evidence_lifecycle_events(&revisions, evidence_lifecycle_kind::EVIDENCE_RECEIVED),
            1
        );
        assert_eq!(
            count_evidence_lifecycle_events(&revisions, evidence_lifecycle_kind::EVIDENCE_FAILED),
            0
        );
        let received = revisions
            .iter()
            .find(|rev| rev.event_kind == evidence_lifecycle_kind::EVIDENCE_RECEIVED)
            .unwrap();
        let meta =
            EvidenceLifecycleMetadata::parse_event_metadata(received.event_metadata.as_deref())
                .unwrap()
                .unwrap();
        assert_eq!(meta.proposal_id, p.id);
        assert_eq!(meta.spike_task_id, spike_task_id);
        assert_eq!(meta.round, claim.round);
        assert_eq!(meta.against_revision_seq, claim.against_revision_seq);
        assert_eq!(
            meta.findings_debate_entry_id.as_deref(),
            Some(entry.id.as_str())
        );
        assert_eq!(
            meta.findings_metadata_json.as_deref(),
            entry.body_metadata.as_deref()
        );
        let still_linked = repo.get(&p.id).await.unwrap().unwrap();
        assert_eq!(
            still_linked.linked_spike_task_id.as_deref(),
            Some(spike_task_id.as_str())
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_linked_spike_failed_records_failed_and_keeps_link() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let (p, spike_task_id, _claim) =
            setup_linked_evidence_spike(&db, &repo, "Terminal Failed").await;

        let outcome = repo
            .persist_terminal_linked_spike_evidence_lifecycle(
                &p.id,
                &spike_task_id,
                "closed",
                Some("failed"),
            )
            .await
            .unwrap();
        assert_eq!(
            outcome,
            TerminalLinkedEvidenceSpikeOutcome::EvidenceFailed {
                reason: "spike_errored".to_owned()
            }
        );
        let revisions = repo.revisions(&p.id).await.unwrap();
        assert_eq!(
            count_evidence_lifecycle_events(&revisions, evidence_lifecycle_kind::EVIDENCE_FAILED),
            1
        );
        let still_linked = repo.get(&p.id).await.unwrap().unwrap();
        assert_eq!(
            still_linked.linked_spike_task_id.as_deref(),
            Some(spike_task_id.as_str())
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_linked_spike_cancelled_or_force_closed_records_failed_once() {
        for (status, close_reason, expected) in [
            ("cancelled", Some("cancelled"), "spike_cancelled"),
            ("closed", Some("force_closed"), "spike_force_closed"),
        ] {
            let db = test_db();
            let repo = ProposalRepository::new(db.clone(), EventBus::noop());
            let (p, spike_task_id, _claim) =
                setup_linked_evidence_spike(&db, &repo, "Terminal Cancel").await;

            let outcome = repo
                .persist_terminal_linked_spike_evidence_lifecycle(
                    &p.id,
                    &spike_task_id,
                    status,
                    close_reason,
                )
                .await
                .unwrap();
            assert_eq!(
                outcome,
                TerminalLinkedEvidenceSpikeOutcome::EvidenceFailed {
                    reason: expected.to_owned()
                }
            );
            let repeated = repo
                .persist_terminal_linked_spike_evidence_lifecycle(
                    &p.id,
                    &spike_task_id,
                    status,
                    close_reason,
                )
                .await
                .unwrap();
            assert!(matches!(
                repeated,
                TerminalLinkedEvidenceSpikeOutcome::AlreadyRecorded { .. }
            ));
            let revisions = repo.revisions(&p.id).await.unwrap();
            assert_eq!(
                count_evidence_lifecycle_events(
                    &revisions,
                    evidence_lifecycle_kind::EVIDENCE_FAILED
                ),
                1
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_linked_spike_completed_without_findings_records_failed() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let (p, spike_task_id, _claim) =
            setup_linked_evidence_spike(&db, &repo, "No Findings").await;

        let outcome = repo
            .persist_terminal_linked_spike_evidence_lifecycle(
                &p.id,
                &spike_task_id,
                "closed",
                Some("completed"),
            )
            .await
            .unwrap();
        assert_eq!(
            outcome,
            TerminalLinkedEvidenceSpikeOutcome::EvidenceFailed {
                reason: "missing_valid_findings".to_owned()
            }
        );
        let revisions = repo.revisions(&p.id).await.unwrap();
        assert_eq!(
            count_evidence_lifecycle_events(&revisions, evidence_lifecycle_kind::EVIDENCE_FAILED),
            1
        );
        assert_eq!(
            count_evidence_lifecycle_events(&revisions, evidence_lifecycle_kind::EVIDENCE_RECEIVED),
            0
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_linked_spike_completed_with_malformed_findings_records_failed() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let (p, spike_task_id, claim) =
            setup_linked_evidence_spike(&db, &repo, "Malformed Findings").await;
        let malformed = serde_json::json!({
            "answer":"",
            "evidence":[],
            "code_paths_inspected":[],
            "confidence":0.7,
            "residual_risks":[],
            "recommendation_for_advocate":""
        });
        insert_raw_evidence_findings_entry(
            &db,
            &p.id,
            &spike_task_id,
            claim.round,
            claim.against_revision_seq,
            Some(&malformed),
        )
        .await;

        let outcome = repo
            .persist_terminal_linked_spike_evidence_lifecycle(
                &p.id,
                &spike_task_id,
                "closed",
                Some("completed"),
            )
            .await
            .unwrap();
        assert_eq!(
            outcome,
            TerminalLinkedEvidenceSpikeOutcome::EvidenceFailed {
                reason: "missing_valid_findings".to_owned()
            }
        );
    }

    // ── Evidence lifecycle metadata tests ─────────────────────────────────

    /// `EvidenceLifecycleMetadata::awaiting_started` serializes correctly
    /// and round-trips through `to_event_metadata` / `parse_event_metadata`.
    #[test]
    fn evidence_lifecycle_metadata_awaiting_started_round_trips() {
        let meta = EvidenceLifecycleMetadata::awaiting_started(
            "proposal-123",
            "spike-456",
            "judge-789",
            2,
            3,
        );
        assert_eq!(meta.phase, "awaiting_started");
        assert_eq!(meta.proposal_id, "proposal-123");
        assert_eq!(meta.spike_task_id, "spike-456");
        assert_eq!(meta.judge_task_id, "judge-789");
        assert_eq!(meta.round, 2);
        assert_eq!(meta.against_revision_seq, 3);
        assert!(meta.failure_reason.is_none());

        // Serialize to event_metadata shape.
        let value = meta.to_event_metadata();
        assert!(value.get("metadata").is_some());

        // Parse back.
        let raw = serde_json::to_string(&value).unwrap();
        let parsed = EvidenceLifecycleMetadata::parse_event_metadata(Some(&raw))
            .expect("parse must succeed")
            .expect("must return Some");
        assert_eq!(parsed, meta);
    }

    /// `EvidenceLifecycleMetadata::received` serializes correctly.
    #[test]
    fn evidence_lifecycle_metadata_received_round_trips() {
        let meta = EvidenceLifecycleMetadata::received("p1", "s1", "j1", 1, 1);
        assert_eq!(meta.phase, "received");
        assert!(meta.failure_reason.is_none());

        let value = meta.to_event_metadata();
        let raw = serde_json::to_string(&value).unwrap();
        let parsed = EvidenceLifecycleMetadata::parse_event_metadata(Some(&raw))
            .unwrap()
            .unwrap();
        assert_eq!(parsed, meta);
    }

    /// `EvidenceLifecycleMetadata::failed` serializes with failure_reason.
    #[test]
    fn evidence_lifecycle_metadata_failed_round_trips_with_reason() {
        let meta = EvidenceLifecycleMetadata::failed("p1", "s1", "j1", 1, 1, "spike_cancelled");
        assert_eq!(meta.phase, "failed");
        assert_eq!(meta.failure_reason.as_deref(), Some("spike_cancelled"));

        let value = meta.to_event_metadata();
        let raw = serde_json::to_string(&value).unwrap();
        let parsed = EvidenceLifecycleMetadata::parse_event_metadata(Some(&raw))
            .unwrap()
            .unwrap();
        assert_eq!(parsed, meta);
        assert_eq!(parsed.failure_reason.as_deref(), Some("spike_cancelled"));
    }

    /// `EvidenceLifecycleMetadata::parse_event_metadata` accepts both
    /// the wrapped `{ "metadata": {...} }` shape and the legacy unwrapped shape.
    #[test]
    fn evidence_lifecycle_metadata_accepts_both_wrapped_and_unwrapped() {
        let meta = EvidenceLifecycleMetadata::awaiting_started("p", "s", "j", 1, 1);

        // Wrapped form.
        let wrapped = serde_json::json!({"metadata": meta});
        let raw = serde_json::to_string(&wrapped).unwrap();
        let parsed = EvidenceLifecycleMetadata::parse_event_metadata(Some(&raw))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.phase, "awaiting_started");

        // Unwrapped (legacy) form — the metadata IS the object.
        let unwrapped = serde_json::to_value(&meta).unwrap();
        let raw = serde_json::to_string(&unwrapped).unwrap();
        let parsed = EvidenceLifecycleMetadata::parse_event_metadata(Some(&raw))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.phase, "awaiting_started");
    }

    /// `EvidenceLifecycleMetadata::parse_event_metadata` returns `Ok(None)`
    /// on `None` or empty input, and `Err` on malformed JSON.
    #[test]
    fn evidence_lifecycle_metadata_parse_edge_cases() {
        assert!(
            EvidenceLifecycleMetadata::parse_event_metadata(None)
                .unwrap()
                .is_none()
        );
        assert!(
            EvidenceLifecycleMetadata::parse_event_metadata(Some(""))
                .unwrap()
                .is_none()
        );
        assert!(EvidenceLifecycleMetadata::parse_event_metadata(Some("{bad")).is_err());
        // Valid JSON but wrong shape (array instead of object).
        assert!(EvidenceLifecycleMetadata::parse_event_metadata(Some("[1,2]")).is_err());
    }

    /// Lifecycle convenience wrappers (`record_awaiting_evidence_started`,
    /// `record_evidence_received`, `record_evidence_failed`) persist
    /// `proposal_revisions` rows with the correct event_kind and metadata,
    /// without regressing existing refinement lifecycle events.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn evidence_lifecycle_convenience_wrappers_persist_correctly() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Lifecycle")).await.unwrap();

        // Start a refinement so lifecycle events are valid.
        repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
            .await
            .unwrap();

        // 1. Awaiting evidence started.
        let awaiting_meta = repo
            .record_awaiting_evidence_started(&p.id, "spike-1", "judge-1", 1, 1)
            .await
            .unwrap();
        let inner = awaiting_meta
            .get("metadata")
            .expect("wrapped shape must have metadata key");
        assert_eq!(inner["phase"], "awaiting_started");
        assert_eq!(inner["proposal_id"], p.id);
        assert_eq!(inner["spike_task_id"], "spike-1");
        assert_eq!(inner["judge_task_id"], "judge-1");
        assert_eq!(inner["round"], 1);
        assert!(inner["failure_reason"].is_null());

        // 2. Evidence received.
        let received_meta = repo
            .record_evidence_received(&p.id, "spike-1", "judge-1", 1, 1)
            .await
            .unwrap();
        let inner = received_meta.get("metadata").unwrap();
        assert_eq!(inner["phase"], "received");
        assert!(inner["failure_reason"].is_null());

        // 3. Evidence failed.
        let failed_meta = repo
            .record_evidence_failed(&p.id, "spike-1", "judge-1", 1, 1, "spike_errored")
            .await
            .unwrap();
        let inner = failed_meta.get("metadata").unwrap();
        assert_eq!(inner["phase"], "failed");
        assert_eq!(inner["failure_reason"], "spike_errored");

        // Verify the revision rows persisted with the correct event_kind.
        let revisions = repo.revisions(&p.id).await.unwrap();
        // seed (seq 1) + refinement_start + awaiting + received + failed = 5 rows.
        assert_eq!(revisions.len(), 5);

        let awaiting_row = &revisions[2];
        assert_eq!(
            awaiting_row.event_kind,
            evidence_lifecycle_kind::AWAITING_EVIDENCE_STARTED
        );
        assert!(awaiting_row.event_metadata.is_some());

        let received_row = &revisions[3];
        assert_eq!(
            received_row.event_kind,
            evidence_lifecycle_kind::EVIDENCE_RECEIVED
        );
        assert!(received_row.event_metadata.is_some());

        let failed_row = &revisions[4];
        assert_eq!(
            failed_row.event_kind,
            evidence_lifecycle_kind::EVIDENCE_FAILED
        );
        assert!(failed_row.event_metadata.is_some());

        // The existing refinement_start row is untouched.
        assert_eq!(revisions[1].event_kind, "refinement_start");
    }

    /// `evidence_lifecycle_kind` constants match the expected string values.
    #[test]
    fn evidence_lifecycle_kind_constants_are_correct() {
        assert_eq!(
            evidence_lifecycle_kind::AWAITING_EVIDENCE_STARTED,
            "refinement_awaiting_evidence_started"
        );
        assert_eq!(
            evidence_lifecycle_kind::EVIDENCE_RECEIVED,
            "refinement_evidence_received"
        );
        assert_eq!(
            evidence_lifecycle_kind::EVIDENCE_FAILED,
            "refinement_evidence_failed"
        );
    }

    // ── Needs-evidence cap accounting tests ──────────────────────────────

    /// Helper: add a `needs_evidence` debate entry with valid linkage metadata.
    async fn add_needs_evidence_entry(
        repo: &ProposalRepository,
        proposal_id: &str,
        round: i32,
        against_revision_seq: i32,
        body: &str,
    ) -> ProposalDebateTrail {
        let judge_task_id = uuid::Uuid::now_v7().to_string();
        let spike_task_id = uuid::Uuid::now_v7().to_string();
        let link = NeedsEvidenceClaimLink {
            kind: NeedsEvidenceClaimLink::KIND_MARKER.to_owned(),
            proposal_id: proposal_id.to_owned(),
            judge_task_id,
            spike_task_id,
            round,
            against_revision_seq,
        };
        let meta_value = link.to_value();
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id,
            kind: "needs_evidence",
            body,
            blocking: true,
            agent_role: "judge",
            author_kind: "agent",
            author_model: Some("claude-opus-4-8"),
            source_task_id: None,
            against_revision_seq,
            round,
            body_metadata: Some(&meta_value),
        })
        .await
        .unwrap()
    }

    /// No `refinement_start` → `needs_evidence_count_for_current_run` returns
    /// `None` and cap status has `no_refinement_run = true`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cap_no_refinement_run_returns_none() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("No Refinement")).await.unwrap();

        let count = repo
            .needs_evidence_count_for_current_run(&p.id)
            .await
            .unwrap();
        assert!(count.is_none(), "no refinement start → count is None");

        let status = repo
            .needs_evidence_cap_status_for_current_run(&p.id)
            .await
            .unwrap();
        assert!(status.no_refinement_run);
        assert_eq!(status.count, 0);
        assert_eq!(status.cap, ProposalRepository::NEEDS_EVIDENCE_PHASE1_CAP);
        assert!(!status.cap_exceeded);
    }

    /// After `refinement_start`, zero demands → count = 0, not exceeded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cap_zero_demands_after_start() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Zero Demands")).await.unwrap();

        repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
            .await
            .unwrap();

        let count = repo
            .needs_evidence_count_for_current_run(&p.id)
            .await
            .unwrap();
        assert_eq!(count, Some(0));

        let status = repo
            .needs_evidence_cap_status_for_current_run(&p.id)
            .await
            .unwrap();
        assert!(!status.no_refinement_run);
        assert_eq!(status.count, 0);
        assert!(!status.cap_exceeded);
    }

    /// One accepted demand → count = 1, not exceeded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cap_one_demand_not_exceeded() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("One Demand")).await.unwrap();

        repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
            .await
            .unwrap();

        add_needs_evidence_entry(&repo, &p.id, 1, 1, "verify X").await;

        let status = repo
            .needs_evidence_cap_status_for_current_run(&p.id)
            .await
            .unwrap();
        assert_eq!(status.count, 1);
        assert!(!status.cap_exceeded);
    }

    /// Two accepted demands → count = 2 = cap, cap_exceeded = true.
    /// This is the Phase 1 boundary: a third demand must be rejected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cap_two_demands_cap_exceeded() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Two Demands")).await.unwrap();

        repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
            .await
            .unwrap();

        add_needs_evidence_entry(&repo, &p.id, 1, 1, "verify X").await;
        add_needs_evidence_entry(&repo, &p.id, 2, 1, "verify Y").await;

        let status = repo
            .needs_evidence_cap_status_for_current_run(&p.id)
            .await
            .unwrap();
        assert_eq!(status.count, 2);
        assert!(
            status.cap_exceeded,
            "at count == cap, cap_exceeded must be true"
        );
        assert_eq!(status.cap, 2);
    }

    /// The count is reconstructed from persisted rows — simulating a restart
    /// by creating a new `ProposalRepository` instance on the same database.
    /// The count must survive the "restart" because it queries the DB.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cap_count_survives_restart_style_reconstruction() {
        let db = test_db();
        let proposal_id;
        // Phase 1: create proposal, start refinement, add 2 demands.
        {
            let repo = ProposalRepository::new(db.clone(), EventBus::noop());
            let p = repo.create(create_input("Restart")).await.unwrap();
            proposal_id = p.id.clone();

            repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
                .await
                .unwrap();

            add_needs_evidence_entry(&repo, &p.id, 1, 1, "first").await;
            add_needs_evidence_entry(&repo, &p.id, 2, 1, "second").await;
        }
        // Phase 2: new repository instance (simulating restart).
        {
            let repo2 = ProposalRepository::new(db.clone(), EventBus::noop());
            let status = repo2
                .needs_evidence_cap_status_for_current_run(&proposal_id)
                .await
                .unwrap();
            assert_eq!(
                status.count, 2,
                "count must survive restart-style reconstruction"
            );
            assert!(status.cap_exceeded);
            assert!(!status.no_refinement_run);
        }
    }

    /// After `refinement_stop` + new `refinement_start`, the count resets
    /// to zero for the new run. Only entries after the latest start count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cap_resets_after_new_refinement_start() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Reset")).await.unwrap();

        // First run: start → 2 demands → stop.
        repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
            .await
            .unwrap();
        add_needs_evidence_entry(&repo, &p.id, 1, 1, "run1-1").await;
        add_needs_evidence_entry(&repo, &p.id, 2, 1, "run1-2").await;

        let status = repo
            .needs_evidence_cap_status_for_current_run(&p.id)
            .await
            .unwrap();
        assert_eq!(status.count, 2);
        assert!(status.cap_exceeded);

        // Stop and start new run.
        repo.record_refinement_lifecycle(&p.id, "refinement_stop", None)
            .await
            .unwrap();
        // Small delay so created_at advances (lifecycle rows use now()).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
            .await
            .unwrap();

        let status = repo
            .needs_evidence_cap_status_for_current_run(&p.id)
            .await
            .unwrap();
        assert_eq!(status.count, 0, "new run must reset count to zero");
        assert!(!status.cap_exceeded);

        // Add one demand in the new run.
        add_needs_evidence_entry(&repo, &p.id, 1, 2, "run2-1").await;

        let status = repo
            .needs_evidence_cap_status_for_current_run(&p.id)
            .await
            .unwrap();
        assert_eq!(status.count, 1);
        assert!(!status.cap_exceeded);
    }

    /// `latest_refinement_start_at` returns `None` before any run boundary and
    /// then tracks the LATEST `refinement_start` across an interrupted-and-
    /// restarted run, so debate-trail reads can be scoped to the current run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn latest_refinement_start_at_tracks_current_run_boundary() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("StartBoundary")).await.unwrap();

        // No refinement run yet → no boundary.
        assert_eq!(
            repo.latest_refinement_start_at(&p.id).await.unwrap(),
            None,
            "no refinement_start → None"
        );

        // Run #1 start.
        repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
            .await
            .unwrap();
        let start1 = repo
            .latest_refinement_start_at(&p.id)
            .await
            .unwrap()
            .expect("run #1 boundary");

        // Interrupt + restart (run #2). Delay so created_at advances.
        repo.record_refinement_lifecycle(&p.id, "refinement_stop", None)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
            .await
            .unwrap();
        let start2 = repo
            .latest_refinement_start_at(&p.id)
            .await
            .unwrap()
            .expect("run #2 boundary");

        assert!(
            start2 > start1,
            "latest boundary must be the newest refinement_start ({start2} > {start1})"
        );
    }

    // ── parked_awaiting_review ───────────────────────────────────────────

    fn awaiting_review_meta(
        judge_summary: &str,
        snapshot_seq: i32,
        refined_seq: i32,
        stop_reason: Option<&str>,
    ) -> serde_json::Value {
        serde_json::json!({
            "source": "refinement_loop",
            "event": "refinement_awaiting_review",
            "judge_summary": judge_summary,
            "snapshot_revision_seq": snapshot_seq,
            "refined_revision_seq": refined_seq,
            "stop_reason": stop_reason,
        })
    }

    /// A converged tribunal (start → awaiting_review, no stop) is reported as
    /// parked, with the snapshot/refined seqs and judge summary reconstructed
    /// from the durable lifecycle row.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parked_awaiting_review_returns_metadata_when_converged() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Parked")).await.unwrap();

        repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let meta = awaiting_review_meta("Converged: spec is ready.", 1, 3, None);
        repo.record_refinement_lifecycle(&p.id, "refinement_awaiting_review", Some(&meta))
            .await
            .unwrap();

        let park = repo
            .parked_awaiting_review(&p.id)
            .await
            .unwrap()
            .expect("converged run must report parked awaiting review");
        assert_eq!(
            park.judge_summary.as_deref(),
            Some("Converged: spec is ready.")
        );
        assert_eq!(park.snapshot_revision_seq, Some(1));
        assert_eq!(park.refined_revision_seq, Some(3));
        assert_eq!(park.stop_reason, None);
    }

    /// An escalation park carries the persisted `stop_reason` tag through.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parked_awaiting_review_carries_escalation_stop_reason() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Escalated")).await.unwrap();

        repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let meta = awaiting_review_meta("Round cap reached.", 2, 2, Some("round_cap"));
        repo.record_refinement_lifecycle(&p.id, "refinement_awaiting_review", Some(&meta))
            .await
            .unwrap();

        let park = repo.parked_awaiting_review(&p.id).await.unwrap().unwrap();
        assert_eq!(park.stop_reason.as_deref(), Some("round_cap"));
        assert_eq!(park.snapshot_revision_seq, Some(2));
        assert_eq!(park.refined_revision_seq, Some(2));
    }

    /// A refinement still mid-tribunal (started, no awaiting_review row yet) is
    /// NOT parked — recovery must stamp it interrupted, not restore it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parked_awaiting_review_none_mid_tribunal() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("MidTribunal")).await.unwrap();

        repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
            .await
            .unwrap();

        assert!(
            repo.parked_awaiting_review(&p.id).await.unwrap().is_none(),
            "a mid-tribunal run has no awaiting-review park"
        );
    }

    /// Once the human resolves the park (a `refinement_stop` lands after the
    /// awaiting_review row), the run is no longer parked.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parked_awaiting_review_none_after_human_resolved() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Resolved")).await.unwrap();

        repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let meta = awaiting_review_meta("Ready.", 1, 2, None);
        repo.record_refinement_lifecycle(&p.id, "refinement_awaiting_review", Some(&meta))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        repo.record_refinement_lifecycle(&p.id, "refinement_stop", None)
            .await
            .unwrap();

        assert!(
            repo.parked_awaiting_review(&p.id).await.unwrap().is_none(),
            "a stop after the awaiting-review row clears the park"
        );
    }

    /// A proposal never entered into refinement is not parked.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parked_awaiting_review_none_when_no_refinement() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("NoRefinement")).await.unwrap();
        assert!(repo.parked_awaiting_review(&p.id).await.unwrap().is_none());
    }

    /// Malformed/rejected demands that fail validation in
    /// `add_debate_trail_entry` do NOT count because no debate entry is
    /// persisted. Only accepted entries count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cap_rejected_demands_do_not_count() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Rejected")).await.unwrap();

        repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
            .await
            .unwrap();

        // Attempt a malformed demand (wrong agent_role) — should fail.
        let link = NeedsEvidenceClaimLink {
            kind: NeedsEvidenceClaimLink::KIND_MARKER.to_owned(),
            proposal_id: p.id.clone(),
            judge_task_id: uuid::Uuid::now_v7().to_string(),
            spike_task_id: uuid::Uuid::now_v7().to_string(),
            round: 1,
            against_revision_seq: 1,
        };
        let meta_value = link.to_value();
        let err = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "needs_evidence",
                body: "malformed",
                blocking: true,
                agent_role: "advocate", // wrong role
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: Some(&meta_value),
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("judge"));

        // Attempt without metadata — should fail.
        let err = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "needs_evidence",
                body: "no meta",
                blocking: true,
                agent_role: "judge",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: None,
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("body_metadata"));

        // Count must be 0 — no entries were persisted.
        let status = repo
            .needs_evidence_cap_status_for_current_run(&p.id)
            .await
            .unwrap();
        assert_eq!(status.count, 0, "rejected demands must not count");
        assert!(!status.cap_exceeded);

        // Now accept a valid demand.
        add_needs_evidence_entry(&repo, &p.id, 1, 1, "valid").await;

        let status = repo
            .needs_evidence_cap_status_for_current_run(&p.id)
            .await
            .unwrap();
        assert_eq!(status.count, 1, "accepted demand must count");
        assert!(!status.cap_exceeded);
    }

    /// Accepted entries continue to count regardless of later spike
    /// completion/failure/cancellation. The count is from the debate entry
    /// row, not from spike task status.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cap_entries_count_regardless_of_spike_outcome() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Spike Outcome")).await.unwrap();

        repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
            .await
            .unwrap();

        // Two accepted demands — cap reached.
        add_needs_evidence_entry(&repo, &p.id, 1, 1, "spike-1").await;
        add_needs_evidence_entry(&repo, &p.id, 2, 1, "spike-2").await;

        let status = repo
            .needs_evidence_cap_status_for_current_run(&p.id)
            .await
            .unwrap();
        assert_eq!(status.count, 2);
        assert!(status.cap_exceeded);

        // Simulate spike outcomes: record lifecycle events for completion
        // and failure. These should NOT affect the cap count because the
        // count is from debate entries, not lifecycle events.
        repo.record_evidence_received(&p.id, "spike-1", "judge-1", 1, 1)
            .await
            .unwrap();
        repo.record_evidence_failed(&p.id, "spike-2", "judge-2", 2, 1, "spike_cancelled")
            .await
            .unwrap();

        // Count must still be 2.
        let status = repo
            .needs_evidence_cap_status_for_current_run(&p.id)
            .await
            .unwrap();
        assert_eq!(status.count, 2, "spike outcomes must not change cap count");
        assert!(
            status.cap_exceeded,
            "cap must remain exceeded after spike outcomes"
        );
    }

    /// Non-`needs_evidence` debate entries (objection, rebuttal, verdict)
    /// do not count toward the cap.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cap_non_needs_evidence_entries_do_not_count() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Non NE")).await.unwrap();

        repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
            .await
            .unwrap();

        // Add objection, rebuttal, verdict — none should count.
        for kind in &["objection", "rebuttal", "verdict"] {
            repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind,
                body: "test entry",
                blocking: *kind == "objection",
                agent_role: "adversary",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
                body_metadata: None,
            })
            .await
            .unwrap();
        }

        let status = repo
            .needs_evidence_cap_status_for_current_run(&p.id)
            .await
            .unwrap();
        assert_eq!(status.count, 0, "non-needs_evidence entries must not count");
        assert!(!status.cap_exceeded);

        // Add one accepted needs_evidence entry — now count = 1.
        add_needs_evidence_entry(&repo, &p.id, 2, 1, "actual demand").await;

        let status = repo
            .needs_evidence_cap_status_for_current_run(&p.id)
            .await
            .unwrap();
        assert_eq!(status.count, 1);
    }

    /// `NEEDS_EVIDENCE_PHASE1_CAP` constant equals 2.
    #[test]
    fn needs_evidence_phase1_cap_is_two() {
        assert_eq!(ProposalRepository::NEEDS_EVIDENCE_PHASE1_CAP, 2);
    }

    /// `NeedsEvidenceCapStatus` fields are consistent.
    #[test]
    fn needs_evidence_cap_status_serializes() {
        let status = NeedsEvidenceCapStatus {
            count: 2,
            cap: 2,
            cap_exceeded: true,
            no_refinement_run: false,
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["count"], 2);
        assert_eq!(json["cap"], 2);
        assert_eq!(json["cap_exceeded"], true);
        assert_eq!(json["no_refinement_run"], false);
    }

    const CORRUPT_SPEC_FIXTURE: &str = include_str!(
        "../../../djinn-spec-lint/tests/fixtures/v1/synthetic/delimiter_failures/body.md"
    );
    const WARNING_SPEC_FIXTURE: &str = include_str!(
        "../../../djinn-spec-lint/tests/fixtures/v1/synthetic/unresolved_reference/body.md"
    );

    fn expected_spec_lint_rejection() -> Vec<crate::SpecLintViolation> {
        let mut result = djinn_spec_lint::lint(
            CORRUPT_SPEC_FIXTURE,
            djinn_spec_lint::BodyFormat::Markdown,
            "1970-01-01T00:00:00.000Z",
        );
        result.sort_violations();
        result
            .errors
            .into_iter()
            .map(|violation| crate::SpecLintViolation {
                code: violation.code,
                message: violation.message,
                span_start: violation.span.start,
                span_end: violation.span.end,
            })
            .collect()
    }

    fn runtime_revision_insert_bypasses(source: &str) -> Vec<usize> {
        // Deliberately exclude this test module (and migration files): this
        // invariant is solely about production repository runtime SQL.
        let source = source.split("#[cfg(test)]\nmod tests {").next().unwrap();
        let ranges = [
            "async fn insert_revision_checked",
            "async fn insert_lightweight_lifecycle_event_in_tx",
        ]
        .into_iter()
        .map(|name| {
            let start = source.find(name).expect("named insertion primitive");
            let open = start + source[start..].find('{').unwrap();
            let mut nesting = 0;
            let end = source[open..]
                .char_indices()
                .find_map(|(offset, character)| match character {
                    '{' => {
                        nesting += 1;
                        None
                    }
                    '}' => {
                        nesting -= 1;
                        (nesting == 0).then_some(open + offset + 1)
                    }
                    _ => None,
                })
                .expect("closed insertion primitive");
            start..end
        })
        .collect::<Vec<_>>();
        source
            .match_indices("INSERT INTO proposal_revisions")
            .filter_map(|(offset, _)| {
                (!ranges.iter().any(|range| range.contains(&offset))).then_some(offset)
            })
            .collect()
    }

    async fn seed_corrupt_legacy_head(
        db: &Database,
        repo: &ProposalRepository,
        title: &str,
    ) -> Proposal {
        let proposal = repo
            .create(create_input_with_ac(title, "", "[\"original\"]"))
            .await
            .unwrap();
        // Direct SQL is test-only legacy setup, not a repository write path.
        for statement in [
            "UPDATE proposals SET body = $1 WHERE id = $2",
            "UPDATE proposal_revisions SET body = $1 WHERE proposal_id = $2 AND seq = 1",
        ] {
            sqlx::query(statement)
                .bind(CORRUPT_SPEC_FIXTURE)
                .bind(&proposal.id)
                .execute(db.pool())
                .await
                .unwrap();
        }
        repo.get(&proposal.id).await.unwrap().unwrap()
    }

    async fn rejected_snapshot(
        db: &Database,
        repo: &ProposalRepository,
        id: &str,
    ) -> (String, usize, i64, usize) {
        let proposal = repo.get(id).await.unwrap().unwrap();
        let lint_rows = sqlx::query_scalar(
            "SELECT COUNT(*) FROM proposal_revision_lint_results WHERE proposal_id = $1",
        )
        .bind(id)
        .fetch_one(db.pool())
        .await
        .unwrap();
        (
            format!("{proposal:?}"),
            repo.revisions(id).await.unwrap().len(),
            lint_rows,
            repo.signoffs(id).await.unwrap().len(),
        )
    }

    fn assert_rejected(error: Error) {
        match error {
            Error::SpecLintRejected(rejection) => {
                assert_eq!(rejection.code, "SPEC_LINT_REJECTED");
                assert_eq!(rejection.violations, expected_spec_lint_rejection());
            }
            other => panic!("expected SPEC_LINT_REJECTED, got {other:?}"),
        }
    }

    #[test]
    fn runtime_revision_inserts_are_confined_to_checked_or_lightweight_primitives() {
        let source = include_str!("proposal.rs");
        assert!(runtime_revision_insert_bypasses(source).is_empty());
        let injected = source.replacen(
            "#[cfg(test)]\nmod tests {",
            "async fn bypass() { sqlx::query(\"INSERT INTO proposal_revisions\"); }\n#[cfg(test)]\nmod tests {",
            1,
        );
        assert_eq!(runtime_revision_insert_bypasses(&injected).len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn corrupt_fixture_rejects_create_without_persistence_or_notification() {
        let db = test_db();
        let (bus, events) = capturing_bus();
        let repo = ProposalRepository::new(db.clone(), bus);
        // The count queries intentionally run before a repository write, so
        // initialize this lazily-cloned test database before using its raw
        // pool. Repository methods do this themselves, but raw fixture
        // assertions must not rely on a later write to create the clone.
        db.ensure_initialized().await.unwrap();
        let before: (i64, i64, i64) = (
            sqlx::query_scalar("SELECT COUNT(*) FROM proposals")
                .fetch_one(db.pool())
                .await
                .unwrap(),
            sqlx::query_scalar("SELECT COUNT(*) FROM proposal_revisions")
                .fetch_one(db.pool())
                .await
                .unwrap(),
            sqlx::query_scalar("SELECT COUNT(*) FROM proposal_revision_lint_results")
                .fetch_one(db.pool())
                .await
                .unwrap(),
        );
        assert_rejected(
            repo.create(ProposalCreateInput {
                title: "bad",
                body: CORRUPT_SPEC_FIXTURE,
                acceptance_criteria: None,
                status: None,
                body_format: Some("markdown"),
            })
            .await
            .unwrap_err(),
        );
        let after: (i64, i64, i64) = (
            sqlx::query_scalar("SELECT COUNT(*) FROM proposals")
                .fetch_one(db.pool())
                .await
                .unwrap(),
            sqlx::query_scalar("SELECT COUNT(*) FROM proposal_revisions")
                .fetch_one(db.pool())
                .await
                .unwrap(),
            sqlx::query_scalar("SELECT COUNT(*) FROM proposal_revision_lint_results")
                .fetch_one(db.pool())
                .await
                .unwrap(),
        );
        assert_eq!(after, before);
        assert!(events.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn corrupt_legacy_head_rejects_all_checked_paths_and_clean_repair_advances() {
        for operation in ["material", "rewrite", "drop", "waive", "done", "review"] {
            let db = test_db();
            let (bus, events) = capturing_bus();
            let repo = ProposalRepository::new(db.clone(), bus);
            let proposal = seed_corrupt_legacy_head(&db, &repo, operation).await;
            events.lock().unwrap().clear();
            let before = rejected_snapshot(&db, &repo, &proposal.id).await;
            let result: Result<()> = match operation {
                "material" => repo
                    .update(
                        &proposal.id,
                        ProposalUpdateInput {
                            title: "changed",
                            body: CORRUPT_SPEC_FIXTURE,
                            acceptance_criteria: "[\"original\"]",
                            status: "draft",
                            superseded_by: None,
                            body_format: Some("markdown"),
                            event_metadata: None,
                        },
                    )
                    .await
                    .map(|_| ()),
                "rewrite" => repo
                    .amend_acceptance_criteria(
                        &proposal.id,
                        &[ProposalAcceptanceCriteriaAmendment::Rewrite {
                            index: 0,
                            criterion: "changed",
                        }],
                        "test",
                    )
                    .await
                    .map(|_| ()),
                "drop" => repo
                    .amend_acceptance_criteria(
                        &proposal.id,
                        &[ProposalAcceptanceCriteriaAmendment::Drop { index: 0 }],
                        "test",
                    )
                    .await
                    .map(|_| ()),
                "waive" => repo
                    .amend_acceptance_criteria(
                        &proposal.id,
                        &[ProposalAcceptanceCriteriaAmendment::Waive { index: 0 }],
                        "test",
                    )
                    .await
                    .map(|_| ()),
                "done" => repo
                    .update(
                        &proposal.id,
                        ProposalUpdateInput {
                            title: &proposal.title,
                            body: &proposal.body,
                            acceptance_criteria: &proposal.acceptance_criteria,
                            status: "done",
                            superseded_by: None,
                            body_format: Some(&proposal.body_format),
                            event_metadata: None,
                        },
                    )
                    .await
                    .map(|_| ()),
                "review" => repo
                    .advance_draft_to_in_review(&proposal.id)
                    .await
                    .map(|_| ()),
                _ => unreachable!(),
            };
            assert_rejected(result.unwrap_err());
            assert_eq!(
                rejected_snapshot(&db, &repo, &proposal.id).await,
                before,
                "{operation} rollback"
            );
            assert!(
                events.lock().unwrap().is_empty(),
                "{operation} notification residue"
            );
        }

        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let legacy = seed_corrupt_legacy_head(&db, &repo, "repair").await;
        let repaired = repo
            .update(
                &legacy.id,
                ProposalUpdateInput {
                    title: "repair",
                    body: "clean body",
                    acceptance_criteria: "[\"original\"]",
                    status: "draft",
                    superseded_by: None,
                    body_format: Some("markdown"),
                    event_metadata: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(repaired.latest_revision_seq, 2);
        assert_eq!(repo.revisions(&legacy.id).await.unwrap().len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn warning_fixture_persists_ordered_result_and_markdown_skipped_tier() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "warning",
                body: WARNING_SPEC_FIXTURE,
                acceptance_criteria: None,
                status: None,
                body_format: Some("markdown"),
            })
            .await
            .unwrap();
        let revision = repo.revisions(&proposal.id).await.unwrap().remove(0);
        let (linter_version, revision_id, body_sha256, result_json) =
            sqlx::query_as::<_, (String, String, String, serde_json::Value)>(
                "SELECT linter_version, revision_id, body_sha256, result_json \
             FROM proposal_revision_lint_results \
             WHERE proposal_id = $1 AND revision_seq = $2",
            )
            .bind(&proposal.id)
            .bind(revision.seq)
            .fetch_one(db.pool())
            .await
            .unwrap();

        // Inspect the durable row directly rather than `lint_for_revision`: that
        // read boundary deliberately recomputes stale or malformed cache rows.
        assert_eq!(
            linter_version,
            djinn_spec_lint::SpecLintResultV1::LINTER_VERSION
        );
        assert_eq!(revision_id, revision.id);
        assert_eq!(body_sha256, djinn_spec_lint::body_sha256(&revision.body));
        let persisted: djinn_spec_lint::SpecLintResultV1 =
            serde_json::from_value(result_json).unwrap();
        let mut expected = djinn_spec_lint::lint(
            &revision.body,
            djinn_spec_lint::BodyFormat::Markdown,
            persisted.checked_at.clone(),
        );
        expected.sort_violations();
        assert_eq!(persisted, expected);
        assert_eq!(
            persisted
                .warnings
                .iter()
                .map(|warning| warning.code.as_str())
                .collect::<Vec<_>>(),
            ["UNRESOLVED_LOCAL_REFERENCE"]
        );
        assert_eq!(persisted.skipped_tiers[0].tier, "mdx_structure");
        assert_eq!(persisted.skipped_tiers[0].reason, "BODY_FORMAT_MARKDOWN");
    }
}
