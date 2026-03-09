pub mod compaction;
pub mod config;
pub mod extension;
pub mod message;
pub mod oauth;
pub mod output_parser;
pub mod prompts;
pub mod provider;
pub mod sandbox;

// ─── Goose session re-exports ─────────────────────────────────────────────────

pub use goose::session::{SessionManager, SessionType};

/// Create a Goose SessionManager backed by the given directory.
pub fn init_session_manager(sessions_dir: std::path::PathBuf) -> std::sync::Arc<SessionManager> {
    std::sync::Arc::new(SessionManager::new(sessions_dir))
}

// ─── Agent type ───────────────────────────────────────────────────────────────

/// Role a Goose agent is playing within Djinn.
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
