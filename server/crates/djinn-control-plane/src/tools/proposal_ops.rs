// Response models for the global Proposals MCP tools. Mirrors the shape of
// `epic_ops.rs`: thin serializable views over the `djinn-core` models with
// JSON-array fields expanded to `Vec<String>`.

use crate::tools::epic_ops::{AcceptanceCriterionItem, parse_acceptance_criteria_array};
use djinn_core::models::{
    Proposal, ProposalFeedback, ProposalRevision, ProposalSignoff, ProposalTarget,
};
use serde::{Deserialize, Serialize};

// ── Revision metadata convention ─────────────────────────────────────────────
//
// Material proposal revisions persist structured metadata in
// `proposal_revisions.event_metadata` (a JSONB column already surfaced by
// `ProposalRevisionModel::event_metadata` and `ProposalRepository::revisions`).
// The convention below is the typed shape future targeted-patch callers use to
// attribute authoring revisions to the active native-skill version (and to
// record the targeted range they patched). Ordinary `proposal_update` calls
// leave the column `NULL`, preserving the pre-existing contract.
//
// Persistence is fully additive: callers build a `TargetedBlockPatchMetadata`
// (or any compatible JSON object), serialize it with [`serde_json::to_value`],
// and pass the resulting `serde_json::Value` to
// `ProposalRepository::update` via `ProposalUpdateInput { event_metadata, .. }`.

/// Change-kind tags persisted on `proposal_revisions.event_metadata`.
///
/// The string values are part of the public contract for downstream consumers
/// (UI attribution badges, audit queries, planner provenance). New tags are
/// additive; renaming an existing tag is a breaking change and must update any
/// consumers that filter on the literal.
pub mod revision_change_kind {
    /// A targeted MDX block-patch — one selected paragraph/section/list was
    /// wrapped or replaced with valid MDX block content while the rest of the
    /// body was preserved. Drives one proposal revision per patch.
    pub const TARGETED_BLOCK_PATCH: &str = "targeted_block_patch";

    /// A full-body rewrite of the proposal spec. Reserved for cases where the
    /// planner intentionally regenerates the whole spec (rare; the targeted
    /// patch primitive is preferred for refinement).
    pub const BODY_REWRITE: &str = "body_rewrite";
}

/// Auditable aggregate of the shared parent-terminal disposition matrix.
/// Both proposal abort and obsolete-epic reconciliation return this in preview
/// and mutation modes so callers can distinguish disposal from safe retention.
#[derive(Serialize, Deserialize, Clone, Default, schemars::JsonSchema)]
pub struct ProposalDispositionSummary {
    pub disposed: i64,
    pub parked: i64,
    pub retained_other_parent: i64,
    pub retained_external_dependent: i64,
    pub retained_already_terminal: i64,
}

/// One targeted block-patch attribution payload. Serialized into
/// `proposal_revisions.event_metadata` so the planner refinement loop and
/// downstream UI can render a per-revision provenance badge linking the
/// revision to the active native-skill version and the section it touched.
///
/// `serde_json::Value` is the on-disk shape — the structured form here is the
/// typed contract callers build before serializing. This keeps the metadata
/// type-safe at the call site (so the planner and the future patch primitive
/// can't drift on field names) while leaving the database and HTTP layers
/// schema-free.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TargetedBlockPatchMetadata {
    /// [`revision_change_kind::TARGETED_BLOCK_PATCH`] (or another declared kind).
    /// Always present — it discriminates which metadata fields downstream
    /// consumers can rely on.
    pub change_kind: String,
    /// Stable identifier of the block catalog entry that was inserted, when
    /// the patch installs a known block. `None` for free-form text edits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_id: Option<String>,
    /// Human-readable description of the targeted range (e.g. the matched
    /// paragraph, section heading, or fenced offset). `None` when the patch
    /// has no meaningful range (e.g. a body rewrite).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    /// Inclusive byte offset of the patched range, when known. Paired with
    /// [`Self::range_end_byte`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range_start_byte: Option<i64>,
    /// Exclusive byte offset of the patched range, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range_end_byte: Option<i64>,
    /// Name of the active native skill that produced the patch (e.g.
    /// `visual-spec`). Surfaces provenance for the per-revision badge.
    pub native_skill_name: String,
    /// Pinned version of the active native skill. Combined with
    /// `native_skill_name`, this fully identifies the authoring surface.
    pub native_skill_version: String,
    /// Optional free-form notes the caller wants persisted alongside the
    /// structured fields. Kept short; not queryable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ProposalModel {
    pub id: String,
    pub short_id: String,
    pub title: String,
    pub body: String,
    /// Body encoding: `markdown` (legacy default) or `mdx` (block-aware).
    pub body_format: String,
    /// Structured acceptance criteria (`{criterion, met}` or plain string),
    /// same shape as tasks. `met` means "agreed during scoping".
    pub acceptance_criteria: Vec<AcceptanceCriterionItem>,
    /// Lifecycle: draft | in_review | approved | building | done | rejected |
    /// archived | superseded.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<String>,
    /// Head revision number (sign-offs anchored earlier are stale).
    pub latest_revision_seq: i32,
    /// Last proposal revision that the in-flight build has reconciled against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_reconciled_revision_seq: Option<i32>,
    /// True when the in-flight build is behind the latest proposal revision.
    pub pending_reconcile: bool,
    /// Build owner once graduated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_owner_user_id: Option<String>,
    /// Count of unresolved feedback entries — drives the per-row badge in the
    /// proposals list. Only populated on `proposal_list`; `0` on show paths.
    #[serde(default)]
    pub unresolved_feedback_count: i64,
    /// When parked for needs-evidence: the linked spike task id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linked_spike_task_id: Option<String>,
    /// When parked for needs-evidence: the named feasibility claim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needs_evidence_claim: Option<String>,
    /// Compact tribunal/readiness summary — populated only on `proposal_list`
    /// (batched across the page) for non-terminal proposals, so list rows can
    /// render tribunal/gate chips without opening each proposal. `None` on show
    /// paths and for terminal proposals (done/rejected/archived/superseded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_summary: Option<ProposalListSummary>,
}

/// Compact tribunal/readiness state for a single proposal row on the list.
///
/// Every field is a cheap, batched approximation of the richer per-proposal
/// gate/refinement status surfaced by `proposal_show` — enough to drive the
/// list-row chips (tribunal round / awaiting-review / evidence, and a gate
/// pass/fail dot with a "why blocked" tooltip) without the several-queries-per-
/// row those full builders cost.
#[derive(Serialize, Deserialize, Clone, Default, schemars::JsonSchema)]
pub struct ProposalListSummary {
    /// A refinement (tribunal) run is active — a round is in flight.
    pub refinement_active: bool,
    /// Refinement converged and is parked awaiting human review (the human is
    /// the bottleneck — the most important state to surface).
    pub awaiting_review: bool,
    /// Highest debate round reached (`0` when there is no debate trail yet).
    pub current_round: i32,
    /// Parked on an open needs-evidence spike.
    pub needs_evidence: bool,
    /// Deterministic Definition-of-Ready passes.
    pub dor_ready: bool,
    /// Composed-gate approximation passes: `dor_ready` AND the latest judge
    /// verdict is not needs-work AND there are no unresolved blocking objections
    /// AND the proposal is not parked on evidence. (Override lifecycle handling
    /// is intentionally omitted here; the full check lives in `proposal_show`.)
    pub gate_ready: bool,
    /// Count of unresolved blocking (non-verdict) debate objections.
    pub unresolved_blocking_count: i64,
}

/// An epic this proposal graduated into.
#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ProposalEpicModel {
    pub epic_id: String,
    pub epic_short_id: String,
    /// Epic title, for display alongside the short id.
    pub epic_title: String,
    /// Epic emoji, for display alongside the title.
    pub epic_emoji: String,
    pub project_path: String,
    pub status: String,
    pub reconciled_at_revision_seq: Option<i32>,
    pub needs_reconcile: bool,
}

impl From<&Proposal> for ProposalModel {
    fn from(p: &Proposal) -> Self {
        Self::from_with_count(p, 0)
    }
}

impl ProposalModel {
    /// Build a view stamping the unresolved-feedback count (the list path
    /// supplies it from a correlated subquery; show paths pass `0`).
    pub fn from_with_count(p: &Proposal, unresolved_feedback_count: i64) -> Self {
        Self {
            id: p.id.clone(),
            short_id: p.short_id.clone(),
            title: p.title.clone(),
            body: p.body.clone(),
            body_format: p.body_format.clone(),
            acceptance_criteria: parse_acceptance_criteria(&p.acceptance_criteria),
            status: p.status.clone(),
            author_user_id: p.author_user_id.clone(),
            superseded_by: p.superseded_by.clone(),
            created_at: p.created_at.clone(),
            updated_at: p.updated_at.clone(),
            closed_at: p.closed_at.clone(),
            latest_revision_seq: p.latest_revision_seq,
            last_reconciled_revision_seq: p.last_reconciled_revision_seq,
            pending_reconcile: p.pending_reconcile,
            build_owner_user_id: p.build_owner_user_id.clone(),
            unresolved_feedback_count,
            linked_spike_task_id: p.linked_spike_task_id.clone(),
            needs_evidence_claim: p.needs_evidence_claim.clone(),
            list_summary: None,
        }
    }

    /// Attach the batched tribunal/readiness summary (list path only).
    pub fn with_list_summary(mut self, summary: ProposalListSummary) -> Self {
        self.list_summary = Some(summary);
        self
    }
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ProposalRevisionModel {
    pub id: String,
    pub seq: i32,
    pub title: String,
    /// Full revision body — present only when `revision_bodies = "full"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// First 512 Unicode scalar values of the revision body.
    /// Present when `revision_bodies` is `excerpt` or `full`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_excerpt: Option<String>,
    /// `true` when the original revision body exceeded the 512-scalar cap.
    /// Present when `revision_bodies` is `excerpt` or `full`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_truncated: Option<bool>,
    /// Body encoding: `markdown` (legacy default) or `mdx` (block-aware).
    pub body_format: String,
    pub acceptance_criteria: Vec<AcceptanceCriterionItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edited_by_user_id: Option<String>,
    /// `spec_revision` for material spec snapshots, `status_change` for
    /// lifecycle-only history rows.
    pub event_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_metadata: Option<String>,
    pub created_at: String,
}

impl From<&ProposalRevision> for ProposalRevisionModel {
    fn from(r: &ProposalRevision) -> Self {
        Self {
            id: r.id.clone(),
            seq: r.seq,
            title: r.title.clone(),
            body: Some(r.body.clone()),
            body_excerpt: None,
            body_truncated: None,
            body_format: r.body_format.clone(),
            acceptance_criteria: parse_acceptance_criteria(&r.acceptance_criteria),
            edited_by_user_id: r.edited_by_user_id.clone(),
            event_kind: r.event_kind.clone(),
            status_from: r.status_from.clone(),
            status_to: r.status_to.clone(),
            event_metadata: r.event_metadata.clone(),
            created_at: r.created_at.clone(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ProposalSignoffModel {
    /// `scoped` (product) or `technical` (engineering).
    pub kind: String,
    pub user_id: String,
    /// Revision this sign-off was given against.
    pub revision_seq: i32,
    /// True when the proposal advanced past `revision_seq` (needs re-approval).
    pub stale: bool,
    pub created_at: String,
}

impl ProposalSignoffModel {
    pub fn from_signoff(s: &ProposalSignoff, latest_revision_seq: i32) -> Self {
        Self {
            kind: s.kind.clone(),
            user_id: s.user_id.clone(),
            revision_seq: s.revision_seq,
            stale: s.revision_seq < latest_revision_seq,
            created_at: s.created_at.clone(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ProposalTargetModel {
    pub project_id: String,
    /// `primary` (a write-target) or `reference` (read-only context).
    pub role: String,
    pub created_at: String,
    /// `owner/repo` slug, resolved by the handler for display chips.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_path: Option<String>,
    /// Human-friendly project name, resolved by the handler.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
}

impl From<&ProposalTarget> for ProposalTargetModel {
    fn from(t: &ProposalTarget) -> Self {
        Self {
            project_id: t.project_id.clone(),
            role: t.role.clone(),
            created_at: t.created_at.clone(),
            project_path: None,
            project_name: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ProposalFeedbackModel {
    pub id: String,
    pub proposal_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// `user` or `ai`.
    pub author_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_model: Option<String>,
    pub body: String,
    /// When set, the feedback is resolved (addressed or dismissed) and collapsed
    /// out of the active thread.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,
    /// The revision that addressed this feedback (`null` when dismissed without
    /// a spec change).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_revision_seq: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_by_user_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<&ProposalFeedback> for ProposalFeedbackModel {
    fn from(f: &ProposalFeedback) -> Self {
        Self {
            id: f.id.clone(),
            proposal_id: f.proposal_id.clone(),
            parent_id: f.parent_id.clone(),
            author_kind: f.author_kind.clone(),
            author_user_id: f.author_user_id.clone(),
            author_model: f.author_model.clone(),
            body: f.body.clone(),
            resolved_at: f.resolved_at.clone(),
            resolved_revision_seq: f.resolved_revision_seq,
            resolved_by_user_id: f.resolved_by_user_id.clone(),
            created_at: f.created_at.clone(),
            updated_at: f.updated_at.clone(),
        }
    }
}

// ── Single-entity / show / mutation responses ────────────────────────────────

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ProposalSingleResponse {
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<ProposalModel>,
    /// Portable proposal.mdx representation (populated by `proposal_export`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mdx: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ProposalShowResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal: Option<ProposalModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub targets: Option<Vec<ProposalTargetModel>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback: Option<Vec<ProposalFeedbackModel>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revisions: Option<Vec<ProposalRevisionModel>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signoffs: Option<Vec<ProposalSignoffModel>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epics: Option<Vec<ProposalEpicModel>>,
    /// Memory notes linked to this proposal's graduated epics/tasks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memory_refs: Vec<ProposalMemoryRefModel>,
    /// Structured debate-trail rows (objections, rebuttals, verdicts).
    /// Kept separate from `feedback` (plain human discussion).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debate_trail: Option<Vec<ProposalDebateTrailModel>>,
    /// Refinement session status (active round, stop reason, update authority).
    /// `None` when refinement has not been started for this proposal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refinement: Option<ProposalRefinementStatusModel>,
    /// Composed gate status: deterministic DoR + tribunal conditions.
    /// Always present on a successful `proposal_show` response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate_status: Option<ProposalGateStatusModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A structured debate-trail row for the proposal tribunal. Separate from
/// [`ProposalFeedbackModel`] (human discussion): debate rows are typed
/// (objection, rebuttal, verdict), track blocking state, and carry
/// resolution/reopen lifecycle.
#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ProposalDebateTrailModel {
    pub id: String,
    pub proposal_id: String,
    /// `objection` | `rebuttal` | `verdict`.
    pub kind: String,
    pub body: String,
    /// When true, this entry blocks proposal readiness.
    pub blocking: bool,
    /// Agent role (e.g. "advocate", "adversary", "judge").
    pub agent_role: String,
    /// `agent` or `user`.
    pub author_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_task_id: Option<String>,
    /// The proposal revision this entry was written against.
    pub against_revision_seq: i32,
    /// Debate round (1-based).
    pub round: i32,
    /// When set, the entry has been resolved. `None` while open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_by_user_id: Option<String>,
    /// When set alongside `resolved_at`, the entry was reopened.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reopened_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reopened_by_user_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<&djinn_core::models::ProposalDebateTrail> for ProposalDebateTrailModel {
    fn from(d: &djinn_core::models::ProposalDebateTrail) -> Self {
        Self {
            id: d.id.clone(),
            proposal_id: d.proposal_id.clone(),
            kind: d.kind.clone(),
            body: d.body.clone(),
            blocking: d.blocking,
            agent_role: d.agent_role.clone(),
            author_kind: d.author_kind.clone(),
            author_user_id: d.author_user_id.clone(),
            author_model: d.author_model.clone(),
            source_task_id: d.source_task_id.clone(),
            against_revision_seq: d.against_revision_seq,
            round: d.round,
            resolved_at: d.resolved_at.clone(),
            resolved_by_user_id: d.resolved_by_user_id.clone(),
            reopened_at: d.reopened_at.clone(),
            reopened_by_user_id: d.reopened_by_user_id.clone(),
            created_at: d.created_at.clone(),
            updated_at: d.updated_at.clone(),
        }
    }
}

/// A memory note linked to a proposal via its graduated epics/tasks.
#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ProposalMemoryRefModel {
    pub permalink: String,
    pub title: String,
    pub note_type: String,
    /// The entity the note is attached to: `"epic"` or `"task"`.
    pub source_entity_type: String,
    /// Short ID of the epic or task that links the note.
    pub source_short_id: String,
}

impl From<djinn_db::ProposalMemoryRef> for ProposalMemoryRefModel {
    fn from(r: djinn_db::ProposalMemoryRef) -> Self {
        Self {
            permalink: r.permalink,
            title: r.title,
            note_type: r.note_type,
            source_entity_type: r.source_entity_type,
            source_short_id: r.source_short_id,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ProposalTargetsResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub targets: Option<Vec<ProposalTargetModel>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ProposalFeedbackResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback: Option<ProposalFeedbackModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ProposalReconcileObsoleteEpicResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epic_id: Option<String>,
    /// `true` when this was a dry-run that did not mutate anything.
    pub preview: bool,
    /// `true` when merged work in the target epic prevented mutation.
    pub blocked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_feedback_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_feedback_body: Option<String>,
    /// Task UUIDs in the target epic that have merged work and caused a block.
    #[serde(default)]
    pub merged_tasks: Vec<String>,
    /// Target epic closed, or that would be closed in preview.
    pub epics_closed: i64,
    /// Target-epic tasks disposed (closed), or that would be disposed in preview.
    pub tasks_closed: i64,
    /// Retained for wire compatibility. Proposal disposition never kills sessions
    /// directly; normal task status-change handling reconciles worker state.
    pub sessions_killed: i64,
    /// Child-disposition outcomes for the selected epic. Retained findings do
    /// not make the reconciliation fail.
    #[serde(default)]
    pub disposition: ProposalDispositionSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ProposalDeleteResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── Debate-trail responses ───────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ProposalDebateTrailResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<ProposalDebateTrailModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ProposalDebateTrailListResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entries: Option<Vec<ProposalDebateTrailModel>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── Refinement status models ────────────────────────────────────────────────

/// Refinement session state tracked on the proposal's event_metadata.
/// The coordinator refinement workflow populates these fields; the
/// control-plane surfaces them read-only to the UI.
#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ProposalRefinementStatusModel {
    /// Whether refinement has been started for this proposal.
    pub active: bool,
    /// Current debate round (1-based). `None` when refinement has not started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_round: Option<i32>,
    /// How many consecutive adversary dry rounds have been observed.
    pub dry_rounds: i32,
    /// Total debate-trail entries produced so far.
    pub total_entries: i32,
    /// When set, refinement has stopped for this reason.
    /// Values: `adversary_dry`, `round_cap`, `spawn_cap`, `repeated_objection`,
    /// `agent_failure`, or `null` (still running / not started).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// True when the autonomous tribunal has converged (or escalated) and is
    /// parked for the human's single accept/reject review of the refined spec.
    #[serde(default)]
    pub awaiting_review: bool,
    /// The judge's summary shown alongside the accept/reject review.
    /// `None` unless `awaiting_review` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judge_summary: Option<String>,
    /// The pre-refinement snapshot revision seq (the diff baseline) when
    /// `awaiting_review` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_revision_seq: Option<i32>,
    /// When the proposal is parked for a needs-evidence spike, this contains
    /// the claim and spike task reference. `None` when not parked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needs_evidence: Option<NeedsEvidenceStatus>,
    /// Top-level evidence lifecycle state derived from durable proposal
    /// fields, lifecycle events, and linked-spike task status. Lets
    /// downstream consumers distinguish Active, AwaitingEvidence,
    /// EvidenceFailed, PausedOrFrozen, and Terminal without inspecting
    /// individual sub-fields.
    pub evidence_lifecycle_state: EvidenceLifecycleState,
}

/// Evidence lifecycle phase for a needs-evidence parking.
///
/// This is the inner phase within a `NeedsEvidenceStatus` and describes
/// only the evidence-spike lifecycle. See [`EvidenceLifecycleState`] for
/// the top-level refinement status discriminator that includes Active,
/// PausedOrFrozen, and Terminal.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceLifecyclePhase {
    /// Spike is running, no findings yet.
    AwaitingEvidence,
    /// Spike returned structured findings; refinement can resume.
    EvidenceReceived,
    /// Spike failed (cancelled, errored, force-closed).
    EvidenceFailed,
}

/// Top-level evidence lifecycle state for the refinement status surface.
///
/// Derived from durable proposal fields, debate-trail lifecycle events,
/// linked-spike task status, and admin freeze state — **not** from
/// in-memory coordinator state. Downstream control-plane and UI consumers
/// use this discriminator to determine the refinement's effective state
/// without needing to inspect individual `needs_evidence` sub-fields.
///
/// Precedence (highest → lowest):
/// 1. `Terminal` — proposal status is done/rejected/archived/superseded.
/// 2. `PausedOrFrozen` — admin freeze is active (`build_frozen = true`).
///    This takes precedence over active resume wording.
/// 3. `EvidenceFailed` — persisted failure lifecycle event exists.
/// 4. `EvidenceReceived` — persisted receipt lifecycle event exists.
/// 5. `AwaitingEvidence` — open linked evidence spike.
/// 6. `Active` — refinement is active, no evidence parking.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceLifecycleState {
    /// Refinement is active (advocate/adversary/judge loop running).
    Active,
    /// Refinement is parked: an evidence spike is in flight and findings
    /// have not yet arrived.
    AwaitingEvidence,
    /// Evidence findings have been recorded; refinement may resume once
    /// downstream processing completes.
    EvidenceReceived,
    /// Evidence spike failed or was force-closed; refinement is blocked
    /// until the failure is addressed.
    EvidenceFailed,
    /// The proposal is administratively paused or frozen. Takes precedence
    /// over all evidence sub-states and active resume wording.
    PausedOrFrozen,
    /// Refinement has reached a terminal outcome (proposal done/rejected/
    /// archived/superseded).
    Terminal,
}

/// Needs-evidence parking state for a proposal.
#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct NeedsEvidenceStatus {
    /// The named feasibility claim that the Judge identified.
    /// For structured claims this is the `question` field; for legacy
    /// plain-string claims this is the raw string.
    pub claim: String,
    /// The spike task id (UUID).
    pub spike_task_id: String,
    /// The spike task short id (human-readable).
    pub spike_short_id: String,
    /// Current status of the spike task.
    pub spike_status: String,

    // ── Structured claim fields (None for legacy plain-string claims) ──────
    /// The feasibility question the spike must answer (from the structured
    /// claim). `None` when `needs_evidence_claim` is a legacy plain string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// The subsystem or module under investigation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_subsystem: Option<String>,
    /// What in the spec is unknown/unverified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec_unknown_anchor: Option<String>,
    /// Debate round when the demand was issued.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub round: Option<i32>,
    /// Proposal revision sequence the demand targets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub against_revision_seq: Option<i32>,
    /// The Judge task id that issued the demand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by_task_id: Option<String>,

    // ── Additional structured claim fields (None for legacy claims) ────────
    /// Why in-session research was insufficient to resolve the claim.
    /// From the structured claim's `insufficient_in_session_research`.
    /// `None` when `needs_evidence_claim` is a legacy plain string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub insufficient_in_session_research: Option<String>,
    /// What the evidence spike should produce to resolve the claim.
    /// From the structured claim's `expected_findings`.
    /// `None` when `needs_evidence_claim` is a legacy plain string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_findings: Option<String>,

    // ── Evidence lifecycle phase (from persisted lifecycle events) ─────────
    /// Current evidence lifecycle phase (awaiting, received, or failed).
    /// Derived from persisted `proposal_revisions` lifecycle events.
    /// `None` when no lifecycle event has been recorded yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_phase: Option<EvidenceLifecyclePhase>,
    /// For `evidence_failed`, the failure reason (`spike_cancelled`,
    /// `spike_errored`, `spike_force_closed`, `malformed_findings`, etc.).
    /// `None` for other phases.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
}

/// Response for `proposal_refinement_start`.
#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ProposalRefinementStartResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal_id: Option<String>,
    /// The initial refinement status after starting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refinement: Option<ProposalRefinementStatusModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Response for `proposal_refinement_status`.
#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ProposalRefinementStatusResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refinement: Option<ProposalRefinementStatusModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── Gate status models (task g11d) ────────────────────────────────────────

/// Composed gate status for a proposal: deterministic DoR + tribunal
/// conditions. Returned by `proposal_show` so the UI can render readiness
/// without recomputing it client-side.
#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ProposalGateStatusModel {
    /// Whether the composed gate passes (DoR ready + tribunal conditions met).
    pub ready: bool,
    /// Whether the deterministic DoR checks pass.
    pub dor_ready: bool,
    /// Specific DoR failures (empty when `dor_ready` is true).
    pub dor_failures: Vec<GateFailureModel>,
    /// Latest judge verdict body text, when a judge has issued a verdict.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judge_verdict_body: Option<String>,
    /// Latest judge verdict entry id, when a judge has issued a verdict.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judge_verdict_id: Option<String>,
    /// Whether the latest judge verdict contains "needs-work".
    pub judge_needs_work: bool,
    /// Consecutive adversary dry rounds at the end of the trail.
    pub adversary_dry_count: i32,
    /// Count of unresolved blocking debate-trail entries.
    pub unresolved_blocking_count: i32,
    /// IDs of unresolved blocking debate-trail entries.
    pub unresolved_blocking_ids: Vec<String>,
    /// Needs-evidence spike parking state. `None` when not parked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needs_evidence: Option<NeedsEvidenceStatus>,
    /// Whether a current human override exists for this revision.
    pub human_override_active: bool,
    /// Human-readable explanations of all gate failures, each naming the
    /// exact blocking condition. Empty when `ready` is true.
    pub blocked_explanations: Vec<String>,
}

/// One DoR failure in the gate status.
#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct GateFailureModel {
    /// Which high-level check failed (e.g. `problem_coverage`, `vague_acceptance_criteria`).
    pub check: String,
    /// Human-readable failure message.
    pub message: String,
}

// ── Body excerpt helpers ───────────────────────────────────────────────────

/// Maximum number of Unicode scalar values retained in a body excerpt.
pub const BODY_EXCERPT_MAX_SCALARS: usize = 512;

/// Deterministic body excerpt: caps the input at exactly
/// [`BODY_EXCERPT_MAX_SCALARS`] Unicode scalar values (Rust `char`s).
///
/// Returns `(excerpt, truncated)` where `truncated` is `true` when the
/// original body exceeded the cap.  No ellipsis is appended — the caller
/// can decide whether to surface the truncation flag.
pub fn body_excerpt(body: &str) -> (String, bool) {
    if body.chars().count() <= BODY_EXCERPT_MAX_SCALARS {
        return (body.to_owned(), false);
    }
    let excerpt: String = body.chars().take(BODY_EXCERPT_MAX_SCALARS).collect();
    (excerpt, true)
}

// ── proposal_show field selection & revision body modes ────────────────────

/// Accepted field names for `proposal_show` `fields` parameter.
pub const SHOW_FIELDS_ACCEPTED: &[&str] = &[
    "proposal",
    "targets",
    "feedback",
    "signoffs",
    "revisions",
    "debate",
    "epics",
    "gate_status",
];

/// Accepted values for `proposal_show` `revision_bodies` parameter.
pub const REVISION_BODIES_ACCEPTED: &[&str] = &["excerpt", "full", "omit"];

/// Validate field names passed to `proposal_show`. Returns `Ok(())` when
/// every entry is in [`SHOW_FIELDS_ACCEPTED`], or an error naming the first
/// invalid value and listing all accepted values.
pub fn validate_show_fields(fields: &[String]) -> Result<(), String> {
    for f in fields {
        if !SHOW_FIELDS_ACCEPTED.contains(&f.as_str()) {
            return Err(format!(
                "invalid field: {f:?} (accepted: {})",
                SHOW_FIELDS_ACCEPTED.join(", ")
            ));
        }
    }
    Ok(())
}

/// Validate the `revision_bodies` enum value. Returns `Ok(())` when
/// the value is in [`REVISION_BODIES_ACCEPTED`], or an error naming the
/// accepted values.
pub fn validate_revision_bodies_value(s: &str) -> Result<(), String> {
    if !REVISION_BODIES_ACCEPTED.contains(&s) {
        return Err(format!(
            "invalid revision_bodies: {s:?} (accepted: {})",
            REVISION_BODIES_ACCEPTED.join(", ")
        ));
    }
    Ok(())
}

/// Apply the revision body mode to a list of [`ProposalRevisionModel`]s.
///
/// * `"full"` — `body` is populated alongside `body_excerpt` / `body_truncated`.
/// * `"excerpt"` (default) — `body` is `None`; `body_excerpt` and
///   `body_truncated` are populated.
/// * `"omit"` — `body`, `body_excerpt`, and `body_truncated` are all `None`.
pub fn apply_revision_body_mode(revisions: &mut [ProposalRevisionModel], mode: &str) {
    for rev in revisions.iter_mut() {
        // We need the original body to compute excerpt/truncated.
        // In "full" mode, body is already set. In other modes, we
        // extract it from body (which was set by From) and then clear it.
        let original_body: Option<String> = rev.body.take();
        match mode {
            "full" => {
                if let Some(ref b) = original_body {
                    let (excerpt, truncated) = body_excerpt(b);
                    rev.body = original_body;
                    rev.body_excerpt = Some(excerpt);
                    rev.body_truncated = Some(truncated);
                }
            }
            "omit" => {
                // Everything stays None.
            }
            _ => {
                // "excerpt" — default mode
                if let Some(ref b) = original_body {
                    let (excerpt, truncated) = body_excerpt(b);
                    rev.body_excerpt = Some(excerpt);
                    rev.body_truncated = Some(truncated);
                }
            }
        }
    }
}

/// List-specific proposal row model.
///
/// The default wire shape is a bounded summary. Body data and criteria are
/// opt-in; callers needing complete proposal detail use `proposal_show`.
#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ProposalListRow {
    pub id: String,
    pub short_id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_excerpt: Option<String>,
    /// `true` when the original body exceeded the 512-scalar cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_truncated: Option<bool>,
    /// Full proposal body — **only serialized when `include_bodies = true`**.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Structured acceptance criteria (`{criterion, met}` or plain string),
    /// included only when requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acceptance_criteria: Option<Vec<AcceptanceCriterionItem>>,
    /// Lifecycle: draft | in_review | approved | building | done | rejected |
    /// archived | superseded.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_user_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// True when the in-flight build is behind the latest proposal revision.
    pub pending_reconcile: bool,
    /// Build owner once graduated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_owner_user_id: Option<String>,
    /// Count of unresolved feedback entries — drives the per-row badge in the
    /// proposals list.
    #[serde(default)]
    pub unresolved_feedback_count: i64,
    /// Total criteria, including legacy strings.
    pub ac_total: i64,
    /// Criteria explicitly marked `{ met: true }`.
    pub ac_met: i64,
    /// Compact tribunal/readiness summary — populated only on `proposal_list`
    /// (batched across the page) for non-terminal proposals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_summary: Option<ProposalListSummary>,
}

impl ProposalListRow {
    /// Build a list row from a proposal and an unresolved-feedback count.
    ///
    /// `include_bodies` implies excerpt metadata for compatibility.
    pub fn from_proposal(
        p: &Proposal,
        unresolved_feedback_count: i64,
        include_bodies: bool,
        include_excerpts: bool,
        include_acceptance_criteria: bool,
    ) -> Self {
        let include_excerpt_metadata = include_bodies || include_excerpts;
        let (body_excerpt, body_truncated) = if include_excerpt_metadata {
            let (excerpt, truncated) = body_excerpt(&p.body);
            (Some(excerpt), Some(truncated))
        } else {
            (None, None)
        };
        let (ac_total, ac_met) = acceptance_criteria_counts(&p.acceptance_criteria);
        Self {
            id: p.id.clone(),
            short_id: p.short_id.clone(),
            title: p.title.clone(),
            body_excerpt,
            body_truncated,
            body: include_bodies.then(|| p.body.clone()),
            acceptance_criteria: include_acceptance_criteria
                .then(|| parse_acceptance_criteria(&p.acceptance_criteria)),
            status: p.status.clone(),
            author_user_id: p.author_user_id.clone(),
            created_at: p.created_at.clone(),
            updated_at: p.updated_at.clone(),
            pending_reconcile: p.pending_reconcile,
            build_owner_user_id: p.build_owner_user_id.clone(),
            unresolved_feedback_count,
            ac_total,
            ac_met,
            list_summary: None,
        }
    }

    /// Attach the batched tribunal/readiness summary (list path only).
    pub fn with_list_summary(mut self, summary: ProposalListSummary) -> Self {
        self.list_summary = Some(summary);
        self
    }
}

/// Parse the stored acceptance-criteria JSON array into structured items,
/// accepting both plain strings and `{criterion, met}` objects (same
/// tolerance as the task layer).
fn parse_acceptance_criteria(raw: &str) -> Vec<AcceptanceCriterionItem> {
    parse_acceptance_criteria_array(raw)
}

fn acceptance_criteria_counts(raw: &str) -> (i64, i64) {
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(raw) else {
        return (0, 0);
    };
    let total = items.len() as i64;
    let met = items
        .iter()
        .filter(|item| item.get("met").and_then(serde_json::Value::as_bool) == Some(true))
        .count() as i64;
    (total, met)
}

// ── Human authority control responses ──────────────────────────────────────

/// Response for `proposal_refinement_demand_round`.
#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct DemandRoundResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal_id: Option<String>,
    /// True when the demand was accepted and a new round started.
    pub accepted: bool,
    /// Refinement status after the demand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refinement: Option<ProposalRefinementStatusModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Response for `proposal_refinement_resolve`.
#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ResolveReviewResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal_id: Option<String>,
    /// True when the human's accept/reject was applied.
    pub resolved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Response for `proposal_verdict_override`.
#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct VerdictOverrideResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal_id: Option<String>,
    /// True when the override was recorded.
    pub overridden: bool,
    /// The revision seq the override is scoped to. Later revisions make
    /// this override stale.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub override_on_revision_seq: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── Needs-evidence demand responses ──────────────────────────────────────────

/// Accepted demand result for `proposal_refinement_demand_evidence`.
///
/// Returned inside [`NeedsEvidenceDemandResponse`] when the Judge's demand
/// passes validation. The accepted-demand mutation atomically creates the
/// evidence spike task, links it to the proposal, writes a `needs_evidence`
/// debate entry, and records a `refinement_awaiting_evidence_started`
/// lifecycle event.
#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct NeedsEvidenceDemandResult {
    /// The recorded needs-evidence claim question.
    pub claim: String,
    /// The spike task id (UUID) created for this demand. Present when the
    /// demand was accepted and the spike was successfully linked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spike_task_id: Option<String>,
    /// The pre-refinement snapshot revision seq the demand targets.
    pub against_revision_seq: i32,
    /// The debate round when the demand was issued.
    pub round: i32,
}

/// Response for `proposal_refinement_demand_evidence`.
///
/// Two flavours: accepted (claim recorded, proposal parked) or rejected
/// (validation failed, `error` populated). Exactly one of `result` or
/// `error` is present depending on `accepted`.
#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct NeedsEvidenceDemandResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal_id: Option<String>,
    /// True when the demand was accepted and recorded.
    pub accepted: bool,
    /// Details for an accepted demand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<NeedsEvidenceDemandResult>,
    /// Error message for a rejected demand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
