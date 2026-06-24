// Response models for the global Proposals MCP tools. Mirrors the shape of
// `epic_ops.rs`: thin serializable views over the `djinn-core` models with
// JSON-array fields expanded to `Vec<String>`.

use crate::tools::epic_ops::AcceptanceCriterionItem;
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
        }
    }
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ProposalRevisionModel {
    pub id: String,
    pub seq: i32,
    pub title: String,
    pub body: String,
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
            body: r.body.clone(),
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
    /// Target-epic tasks force-closed, or open target-epic tasks in preview.
    pub tasks_closed: i64,
    /// Running target-epic worker sessions killed, or live sessions in preview.
    pub sessions_killed: i64,
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
    /// Update authority mode: `checkpoint` (advocate revisions are proposed
    /// but not auto-applied) or `auto_accept` (revisions are applied as
    /// proposal updates).
    pub update_authority: String,
    /// When set, refinement has stopped for this reason.
    /// Values: `adversary_dry`, `round_cap`, `spawn_cap`, `repeated_objection`,
    /// `agent_failure`, or `null` (still running / not started).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
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

/// Parse the stored acceptance-criteria JSON array into structured items,
/// accepting both plain strings and `{criterion, met}` objects (same
/// tolerance as the task layer).
fn parse_acceptance_criteria(raw: &str) -> Vec<AcceptanceCriterionItem> {
    let parsed = serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    parsed
        .into_iter()
        .map(|item| {
            serde_json::from_value::<AcceptanceCriterionItem>(item.clone())
                .unwrap_or_else(|_| AcceptanceCriterionItem::Text(item.to_string()))
        })
        .collect()
}
