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
pub mod task_attempt;
pub mod task_run;
pub mod user_settings;
pub mod verify_run;

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
    ActivityEntry, CiStatus, IssueType, MergeQueueLane, PRIORITY_CRITICAL, ReopenClass,
    ReopenLedgerEntry, Task, TaskPrCiSnapshot, TaskPrCiSnapshotInput, TaskPrCiSnapshotMqLaneInput,
    TaskStatus, TransitionAction, TransitionApply, compute_transition,
    compute_transition_for_issue_type,
};
pub use task_attempt::{
    GuardDecision, GuardReason, LogTailMeta, TASK_ATTEMPT_DISPATCH_KEY_MAX_LEN,
    TASK_ATTEMPT_LOG_TAIL_MAX_LEN, TASK_ATTEMPT_SUMMARY_MAX_LEN, TaskAttempt,
    TaskAttemptHistoryRow, TaskAttemptLedgerRow, TaskAttemptOutcome, TaskAttemptPromptSummary,
};
pub use task_run::{TaskRunRecord, TaskRunStatus, TaskRunTrigger};
pub use user_settings::{LaneMaxSessions, ModelLane, ModelLanes, UserSettings};
pub use verify_run::{
    AutoSubmitReviewRecord, AutoSubmitTriggerReason, RejectedVerdictKind,
    TaskRejectedSubmissionIntegrityRecord, VerifyResult, VerifyRunRecord, VerifySource,
};

/// Parse a JSON array string (e.g. '["a","b"]') into a `Vec<String>`.
/// Returns an empty vec on any parse failure.
pub fn parse_json_array(json: &str) -> Vec<String> {
    serde_json::from_str(json).unwrap_or_default()
}
