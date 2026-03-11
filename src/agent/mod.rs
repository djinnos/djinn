pub mod compaction;
pub mod config;
pub mod extension;
pub mod message;
pub mod oauth;
pub mod output_parser;
pub mod prompts;
pub mod provider;
pub mod sandbox;
// ─── Agent type ───────────────────────────────────────────────────────────────

/// Role an agent is playing within Djinn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentType {
    /// Background worker that implements a task (writes code, etc.).
    Worker,
    /// Resolves a merge conflict after reviewer-approved merge failed.
    ConflictResolver,
    /// Reviews a single task's diff and approves or rejects it.
    TaskReviewer,
    /// PM agent that grooms backlog and handles intervention for stuck tasks.
    PM,
    /// Groomer agent for backlog grooming workflows.
    Groomer,
}

impl AgentType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::ConflictResolver => "conflict_resolver",
            Self::TaskReviewer => "task_reviewer",
            Self::PM => "pm",
            Self::Groomer => "groomer",
        }
    }

    /// Determine the agent type from a task's status string.
    ///
    /// When `has_conflict_context` is true and the status is `"open"`, returns
    /// `ConflictResolver` instead of `Worker`.
    pub fn for_task_status(status: &str, has_conflict_context: bool) -> Self {
        match status {
            "needs_task_review" | "in_task_review" => Self::TaskReviewer,
            "needs_pm_intervention" | "in_pm_intervention" => Self::PM,
            "open" if has_conflict_context => Self::ConflictResolver,
            _ => Self::Worker,
        }
    }

    /// The dispatch role used for model pool / settings lookup.
    ///
    /// `ConflictResolver` shares the `"worker"` model pool, so both map to
    /// `"worker"`.
    pub fn dispatch_role(&self) -> &'static str {
        match self {
            Self::Worker | Self::ConflictResolver => "worker",
            Self::TaskReviewer => "task_reviewer",
            Self::PM => "pm",
            Self::Groomer => "groomer",
        }
    }

    /// The transition action to claim/start a task for this agent type, given
    /// the task's current status.  Returns `None` if the task is already in the
    /// active state (e.g. `in_progress` for a worker).
    pub fn start_action(&self, task_status: &str) -> Option<crate::models::task::TransitionAction> {
        use crate::models::task::TransitionAction;
        match (self, task_status) {
            (Self::Worker | Self::ConflictResolver, "open") => Some(TransitionAction::Start),
            (Self::TaskReviewer, "needs_task_review") => Some(TransitionAction::TaskReviewStart),
            (Self::PM, "needs_pm_intervention") => Some(TransitionAction::PmInterventionStart),
            (Self::Groomer, _) => None,
            _ => None,
        }
    }

    /// The transition action to release/interrupt a task held by this agent type.
    pub fn release_action(&self) -> crate::models::task::TransitionAction {
        use crate::models::task::TransitionAction;
        match self {
            Self::Worker | Self::ConflictResolver => TransitionAction::Release,
            Self::TaskReviewer => TransitionAction::ReleaseTaskReview,
            Self::PM => TransitionAction::PmInterventionRelease,
            Self::Groomer => TransitionAction::Release,
        }
    }
}

impl std::str::FromStr for AgentType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "worker" => Ok(Self::Worker),
            "conflict_resolver" => Ok(Self::ConflictResolver),
            "task_reviewer" => Ok(Self::TaskReviewer),
            "pm" => Ok(Self::PM),
            "groomer" => Ok(Self::Groomer),
            _ => Err(format!("unknown agent type: {s}")),
        }
    }
}

impl serde::Serialize for AgentType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for AgentType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = <String as serde::Deserialize>::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}
