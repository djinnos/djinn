//! Lifecycle stage helpers reused by the supervisor-driven dispatch path.
//!
//! Task #8 deleted the legacy `run_task_lifecycle` entry point and its
//! worktree orchestration.  What remains are the pure per-stage helpers
//! (setup resolution, model + credential resolution, MCP + skills
//! resolution, role-level override resolution, prompt-context assembly,
//! post-session teardown, and the transition retry utility) which
//! [`crate::supervisor_impl::stage::execute_stage`] composes for each role in
//! a task-run.

pub mod mcp_resolve;
pub mod model_resolution;
pub mod prompt_context;
pub mod retry;
pub mod role_overrides;
pub mod setup;
pub mod task_classifier;
pub mod teardown;
