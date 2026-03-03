// Agent module — Goose-based in-process agent session management.
//
// Foundational types for the AgentSupervisor (d9s4). Wraps the Goose library
// session lifecycle so the rest of the server stays decoupled from Goose internals.

pub mod config;
pub mod extension;
pub mod output_parser;
pub mod prompts;

use std::{path::PathBuf, sync::Arc, time::Instant};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub use goose::agents::AgentEvent;
pub use goose::session::session_manager::{Session, SessionManager, SessionType};

// ─── Handle ───────────────────────────────────────────────────────────────────

/// Tracks a running in-process Goose agent session.
pub struct GooseSessionHandle {
    /// Tokio task running the agent's reply loop.
    pub join: JoinHandle<anyhow::Result<()>>,
    /// Token used to request cooperative cancellation of the session.
    pub cancel: CancellationToken,
    /// Goose session ID (nanoid string stored in sessions.db).
    pub session_id: String,
    /// Djinn task UUID that this session is working on.
    pub task_id: String,
    /// Optional isolated git worktree used by this session.
    pub worktree_path: Option<PathBuf>,
    /// Monotonic launch timestamp used for runtime duration reporting.
    pub started_at: Instant,
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
    /// Reviews a completed phase's aggregate diff.
    PhaseReviewer,
}

impl AgentType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::ConflictResolver => "conflict_resolver",
            Self::TaskReviewer => "task_reviewer",
            Self::PhaseReviewer => "phase_reviewer",
        }
    }
}

// ─── SessionManager init ───────────────────────────────────────────────────────

/// Create a `SessionManager` rooted at `data_dir`.
///
/// Goose's `SessionManager` manages its own SQLite database (`sessions.db`)
/// inside `data_dir`. For Djinn this should be `~/.djinn/sessions/`.
pub fn init_session_manager(data_dir: PathBuf) -> Arc<SessionManager> {
    Arc::new(SessionManager::new(data_dir))
}
