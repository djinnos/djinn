//! Lifecycle stage helpers reused by the supervisor-driven dispatch path.
//!
//! Task #8 deleted the legacy `run_task_lifecycle` entry point and its
//! worktree orchestration.  What remains are the pure per-stage helpers
//! (setup/verification resolution, model + credential resolution, MCP + skills
//! resolution, prompt-context assembly, post-session teardown, and the
//! transition retry utility) which [`crate::supervisor::stage::execute_stage`]
//! composes for each role in a task-run.

pub(crate) mod mcp_resolve;
pub(crate) mod model_resolution;
pub(crate) mod prompt_context;
pub(crate) mod retry;
pub(crate) mod role_overrides;
pub(crate) mod setup;
pub(crate) mod teardown;
