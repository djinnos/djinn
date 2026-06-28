pub mod agent;
pub mod credential;
pub mod dispatch_state;
pub mod epic;
pub mod git_settings;
pub mod org_ai_policy;
pub mod project;
pub mod proposal;
pub mod provider;
pub mod session;
pub mod session_message;
pub mod settings;
pub mod task;
pub mod task_run;
pub mod user_settings;

pub use agent::Agent;
pub use credential::Credential;
pub use dispatch_state::DispatchStateRecord;
pub use epic::Epic;
pub use git_settings::GitSettings;
pub use org_ai_policy::{LockLevel, OrgAiPolicy, OrgDefaultLanes};
pub use project::Project;
pub use proposal::{
    EvidenceFindings, NeedsEvidenceClaim, Proposal, ProposalDebateTrail, ProposalFeedback,
    ProposalRevision, ProposalSignoff, ProposalTarget,
};
pub use provider::{CustomProvider, Model, Pricing, Provider, SeedModel};
pub use session::{CostBasis, SessionRecord, SessionStatus};
pub use session_message::SessionMessage;
pub use settings::{DispatchPause, DispatchPauseScope, DispatchPauseState, DjinnSettings, Setting};
pub use task::{
    ActivityEntry, IssueType, PRIORITY_CRITICAL, Task, TaskStatus, TransitionAction,
    TransitionApply, compute_transition, compute_transition_for_issue_type,
};
pub use task_run::{TaskRunRecord, TaskRunStatus, TaskRunTrigger};
pub use user_settings::{ModelLane, ModelLanes, UserSettings};

/// Parse a JSON array string (e.g. '["a","b"]') into a `Vec<String>`.
/// Returns an empty vec on any parse failure.
pub fn parse_json_array(json: &str) -> Vec<String> {
    serde_json::from_str(json).unwrap_or_default()
}
