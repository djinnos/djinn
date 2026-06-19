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
    /// Body format discriminator: `markdown` (legacy default) or `mdx` (block-aware).
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
    /// Body format discriminator: `markdown` (legacy default) or `mdx` (block-aware).
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
    /// Optional pointer to the part of the spec this entry is about.
    pub target_section: Option<String>,
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
