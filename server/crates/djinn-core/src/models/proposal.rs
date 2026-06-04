use serde::{Deserialize, Serialize};

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
    /// Markdown spec body.
    pub body: String,
    /// JSON array of acceptance-criteria strings (stored as JSONB, surfaced as
    /// text — parse with [`crate::models::parse_json_array`]).
    pub acceptance_criteria: String,
    /// Lifecycle: `draft` | `shared` | `ready` | `archived` | `superseded`.
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
}

/// An immutable snapshot of a proposal's spec at a point in time. Appended on
/// every material edit; diffs between revisions drive the "changes since your
/// approval" review.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct ProposalRevision {
    pub id: String,
    pub proposal_id: String,
    pub seq: i32,
    pub title: String,
    pub body: String,
    /// JSON array text (parse with [`crate::models::parse_json_array`] or the
    /// structured AC parser).
    pub acceptance_criteria: String,
    pub edited_by_user_id: Option<String>,
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

/// A unified feedback entry on a proposal — discussion AND suggestions in one
/// primitive. `status` is `None` for plain discussion (a "comment") and
/// `open`/`accepted`/`rejected` for a trackable suggestion. `author_kind`
/// distinguishes human (`user`) from AI (`ai`, with `author_model` set).
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
    /// Optional pointer to the part of the spec this entry is about.
    pub target_section: Option<String>,
    /// `None` = discussion; `open` | `accepted` | `rejected` = suggestion.
    pub status: Option<String>,
    /// For an "edit suggestion", the proposed new spec body. `None` for a
    /// plain discussion/comment.
    pub proposed_body: Option<String>,
    /// Revision the proposed change landed in once accepted.
    pub applied_revision_seq: Option<i32>,
    pub created_at: String,
    pub updated_at: String,
}
