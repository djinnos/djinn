// Response models for the global Proposals MCP tools. Mirrors the shape of
// `epic_ops.rs`: thin serializable views over the `djinn-core` models with
// JSON-array fields expanded to `Vec<String>`.

use djinn_core::models::{Proposal, ProposalFeedback, ProposalTarget};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct ProposalModel {
    pub id: String,
    pub short_id: String,
    pub title: String,
    pub body: String,
    pub acceptance_criteria: Vec<String>,
    /// Lifecycle: `draft` | `shared` | `ready` | `archived` | `superseded`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<String>,
}

impl From<&Proposal> for ProposalModel {
    fn from(p: &Proposal) -> Self {
        Self {
            id: p.id.clone(),
            short_id: p.short_id.clone(),
            title: p.title.clone(),
            body: p.body.clone(),
            acceptance_criteria: parse_string_array(&p.acceptance_criteria),
            status: p.status.clone(),
            author_user_id: p.author_user_id.clone(),
            superseded_by: p.superseded_by.clone(),
            created_at: p.created_at.clone(),
            updated_at: p.updated_at.clone(),
            closed_at: p.closed_at.clone(),
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
    /// `null` = discussion; `open` | `accepted` | `rejected` = suggestion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
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
            status: f.status.clone(),
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
pub struct ProposalDeleteResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub(crate) fn parse_string_array(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_default()
}
