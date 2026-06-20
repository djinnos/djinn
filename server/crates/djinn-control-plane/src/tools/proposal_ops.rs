// Response models for the global Proposals MCP tools. Mirrors the shape of
// `epic_ops.rs`: thin serializable views over the `djinn-core` models with
// JSON-array fields expanded to `Vec<String>`.

use crate::tools::epic_ops::AcceptanceCriterionItem;
use djinn_core::models::{
    Proposal, ProposalFeedback, ProposalRevision, ProposalSignoff, ProposalTarget,
};
use serde::{Deserialize, Serialize};

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_section: Option<String>,
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
            target_section: f.target_section.clone(),
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
    /// Portable `proposal.mdx` export string returned by `proposal_export`.
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
