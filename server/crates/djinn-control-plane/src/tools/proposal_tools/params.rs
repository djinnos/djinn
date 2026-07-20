// MCP tool parameter structs for the proposal CRUD tools.
//
// Extracted from `create.rs` to keep that file under the Server Size Guard
// threshold. These are plain Deserialize + JsonSchema data types with no
// behaviour beyond serde field attributes.

use rmcp::schemars;
use serde::Deserialize;

use crate::tools::epic_ops::AcceptanceCriterionItem;

// ── Param structs ────────────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalCreateParams {
    pub title: String,
    /// Spec body (markdown or MDX depending on `body_format`).
    pub body: Option<String>,
    /// Acceptance criteria: plain strings or `{criterion, met}` objects.
    pub acceptance_criteria: Option<Vec<AcceptanceCriterionItem>>,
    /// Target projects (UUIDs or owner/repo slugs) this proposal touches.
    /// Editable later via proposal_add_target / proposal_remove_target.
    pub target_projects: Option<Vec<String>>,
    /// Initial status: `triage`, `draft` (default), or `in_review`. Proposer-
    /// role authors are always placed in `triage` regardless of this value.
    pub status: Option<String>,
    /// Body encoding: `markdown` (default) or `mdx` (block-aware).
    pub body_format: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalImportParams {
    /// Full portable proposal.mdx content, including optional YAML frontmatter.
    pub mdx: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalExportParams {
    /// Proposal UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalShowParams {
    /// Proposal UUID or short_id.
    pub id: String,
    /// Select which top-level sections to include in the response.
    /// Accepted values: `proposal`, `targets`, `feedback`, `signoffs`,
    /// `revisions`, `debate`, `epics`, `gate_status`.
    /// Default: all fields selected. Invalid values return a validation error.
    #[serde(default)]
    pub fields: Option<Vec<String>>,
    /// Controls revision body verbosity when `revisions` is selected.
    /// Accepted values: `excerpt` (default), `full`, `omit`.
    /// Ignored when `fields` omits `revisions`.
    pub revision_bodies: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalListParams {
    pub status: Option<String>,
    /// Filter by author user id.
    pub author: Option<String>,
    /// Filter to proposals targeting this project (UUID or owner/repo slug).
    pub target_project: Option<String>,
    /// Full-text search on title and body.
    pub text: Option<String>,
    /// Sort order: "created_desc" (default), "created", "updated", "updated_desc".
    pub sort: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// When `true`, include the full body. Omitted and `false` are equivalent;
    /// bodies imply excerpt metadata.
    pub include_bodies: Option<bool>,
    /// When `true`, include excerpt metadata. Omitted and `false` are equivalent.
    pub include_excerpts: Option<bool>,
    /// When `true`, include structured criteria. Omitted and `false` are equivalent.
    pub include_acceptance_criteria: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalTargetParams {
    /// Proposal UUID or short_id.
    pub id: String,
    /// Target project: UUID or owner/repo slug (must be registered).
    pub project: String,
    /// `primary` (a write-target, default) or `reference` (read-only context).
    pub role: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalUpdateParams {
    /// Proposal UUID or short_id.
    pub id: String,
    pub title: Option<String>,
    pub body: Option<String>,
    /// Acceptance criteria: plain strings or `{criterion, met}` objects.
    pub acceptance_criteria: Option<Vec<AcceptanceCriterionItem>>,
    /// draft | in_review | approved | building | done | rejected | archived | superseded.
    pub status: Option<String>,
    /// UUID or short_id of the proposal that supersedes this one.
    pub superseded_by: Option<String>,
    /// Body encoding: `markdown` (default) or `mdx` (block-aware).
    pub body_format: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalDeleteParams {
    /// Proposal UUID or short_id.
    pub id: String,
}
