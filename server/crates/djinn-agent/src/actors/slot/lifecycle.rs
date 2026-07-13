//! Lifecycle stage helpers reused by the supervisor-driven dispatch path.
//!
//! Task #8 deleted the legacy `run_task_lifecycle` entry point and its
//! worktree orchestration.  What remains are the pure per-stage helpers
//! (setup resolution, model + credential resolution, MCP + skills
//! resolution, role-level override resolution, prompt-context assembly,
//! post-session teardown, and the transition retry utility) which
//! [`crate::supervisor_impl::stage::execute_stage`] composes for each role in
//! a task-run.

pub(crate) mod attempt_context;
pub(crate) mod mcp_resolve;
#[allow(dead_code)] // Contract is wired into the session-start integration task.
pub(crate) mod memory_intent_planner;
pub(crate) mod model_resolution;
pub(crate) mod prompt_context;
pub(crate) mod role_overrides;
pub(crate) mod setup;
pub(crate) mod task_classifier;
pub(crate) mod teardown;
