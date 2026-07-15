use serde::{Deserialize, Serialize};

/// Top-level grouping entity with a simplified open→closed lifecycle.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct Epic {
    pub id: String,
    pub project_id: String,
    pub short_id: String,
    pub title: String,
    pub description: String,
    pub emoji: String,
    pub color: String,
    pub status: String,
    pub owner: String,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub memory_refs: String,
    /// ADR-051 Epic C — when false, epic creation does not auto-dispatch
    /// a breakdown Planner.  Default true to preserve existing behaviour.
    pub auto_breakdown: bool,
    /// ADR-051 Epic C — slug of the accepted ADR that spawned this epic.
    /// Threaded into the breakdown Planner's session context.
    pub originating_adr_id: Option<String>,
    /// Real user FK of the epic's creator (NULL for system/unowned epics).
    /// Tasks broken down from this epic inherit it; the board uses it to
    /// scope epics to the owner filter, mirroring `Task::created_by_user_id`.
    pub created_by_user_id: Option<String>,
    /// Originating proposal, denormalized from `proposal_epics` (unique per
    /// epic) at graduation link time. `None` for hand-created epics; cleared
    /// when the proposal unlinks or is deleted. The board groups swimlanes
    /// by it.
    #[serde(default)]
    pub proposal_id: Option<String>,
}

/// Payload for `epic.created` / `epic.updated` events: the epic row plus the
/// proposal swimlane labels the board needs to group live epics, mirroring
/// the `epic_list` enrichment. Emit through `EpicRepository::emit_created` /
/// `emit_updated`, which hydrate the labels; [`EpicEventPayload::bare`] is
/// for epics with no proposal linkage, lookup-failure fallbacks, and tests.
#[derive(Debug, serde::Serialize)]
pub struct EpicEventPayload<'a> {
    #[serde(flatten)]
    pub epic: &'a Epic,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal_short_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal_build_owner_user_id: Option<String>,
}

impl<'a> EpicEventPayload<'a> {
    /// Payload without proposal labels. Consumers treat missing labels as
    /// "linkage unknown", so only use this when the epic has no
    /// `proposal_id` or the label lookup failed.
    pub fn bare(epic: &'a Epic) -> Self {
        Self {
            epic,
            proposal_short_id: None,
            proposal_title: None,
            proposal_status: None,
            proposal_build_owner_user_id: None,
        }
    }
}
