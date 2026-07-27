//! Versioned durable contracts for grounded refinement evidence.
//!
//! These types deliberately describe server-authored provenance rather than
//! caller-supplied command results.  Later lifecycle layers validate and render
//! the structured completion payload without changing this persisted history.

use serde::{Deserialize, Serialize};

/// A frozen, task/session-bound evidence investigation plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidencePlan {
    pub id: String,
    pub spike_task_id: String,
    pub session_id: String,
    pub captured_commit_sha: String,
    pub worktree_fingerprint: String,
    pub checks: Vec<EvidencePlanCheck>,
    pub created_at: String,
    pub updated_at: String,
}

/// One ordered, nonempty question in an [`EvidencePlan`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidencePlanCheck {
    pub plan_id: String,
    pub ordinal: i32,
    pub check_id: String,
    pub question: String,
    /// Stable method vocabulary: `code`, `graph`, or `command`.
    pub method: String,
}

/// Immutable, server-authored command provenance attached to one planned check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceCommandInvocation {
    pub id: String,
    pub plan_id: String,
    pub spike_task_id: String,
    pub session_id: String,
    pub captured_commit_sha: String,
    pub worktree_fingerprint: String,
    pub check_id: String,
    pub argv: Vec<String>,
    pub canonical_cwd: String,
    pub launch_state: String,
    pub process_state: String,
    pub launched_at: Option<String>,
    pub finished_at: Option<String>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub runner_failure: Option<String>,
    pub elapsed_millis: Option<i64>,
    pub timeout_millis: Option<i64>,
    pub timed_out: bool,
    pub stdout_digest: Option<String>,
    pub stdout_excerpt: Option<String>,
    pub stdout_truncated: bool,
    pub stderr_digest: Option<String>,
    pub stderr_excerpt: Option<String>,
    pub stderr_truncated: bool,
    pub created_at: String,
}

/// Closed, version-readable structured hand-off written exactly once per plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceFinalizedProjection {
    pub id: String,
    pub plan_id: String,
    pub version: i32,
    /// A structured JSON object. Its versioned schema is owned by the finalizer.
    pub payload: serde_json::Value,
    pub finalized_at: String,
}

/// Identity-scoped hydration used by finalizers and Judge projection readers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidencePlanHydration {
    pub plan: EvidencePlan,
    pub invocations: Vec<EvidenceCommandInvocation>,
    pub finalized_projection: Option<EvidenceFinalizedProjection>,
}
