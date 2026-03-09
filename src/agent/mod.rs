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
}

impl AgentType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::ConflictResolver => "conflict_resolver",
            Self::TaskReviewer => "task_reviewer",
            Self::PM => "pm",
        }
    }
}
