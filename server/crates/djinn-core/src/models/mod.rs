pub mod agent;
pub mod consolidation;
pub mod consolidation_query;
pub mod credential;
pub mod epic;
pub mod git_settings;
pub mod note;
pub mod note_association;
pub mod project;
pub mod provider;
pub mod session;
pub mod session_message;
pub mod settings;
pub mod task;

pub use agent::Agent;
pub use consolidation::{ConsolidatedNoteProvenance, ConsolidationRunMetric};
pub use consolidation_query::{
    ConsolidationCandidateEdge, ConsolidationCluster, ConsolidationNote, DbNoteGroup,
};
pub use credential::Credential;
pub use epic::Epic;
pub use git_settings::GitSettings;
pub use note::{
    BrokenLink, BuildContextResponse, ContradictionCandidate, GitLogEntry, GraphEdge, GraphNode,
    GraphResponse, HealthReport, Note, NoteAbstract, NoteCompact, NoteDedupCandidate, NoteOverview,
    NoteSearchResult, OrphanNote, ReindexSummary, StaleFolder, TypeRisk,
};
pub use note_association::{NoteAssociation, canonical_pair};
pub use project::Project;
pub use provider::{CustomProvider, Model, Pricing, Provider, SeedModel};
pub use session::{SessionRecord, SessionStatus};
pub use session_message::SessionMessage;
pub use settings::{DjinnSettings, Setting};
pub use task::{
    ActivityEntry, IssueType, PRIORITY_CRITICAL, Task, TaskStatus, TransitionAction,
    TransitionApply, compute_transition, compute_transition_for_issue_type,
};

/// Parse a JSON array string (e.g. '["a","b"]') into a `Vec<String>`.
/// Returns an empty vec on any parse failure.
pub fn parse_json_array(json: &str) -> Vec<String> {
    serde_json::from_str(json).unwrap_or_default()
}
