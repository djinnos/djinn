use super::handlers::*;
use super::helpers::*;
use super::tool_defs::*;
use super::types::*;
use super::*;
use crate::AgentType;
use crate::test_helpers::create_test_db;
use crate::test_helpers::{
    agent_context_from_db, create_test_epic, create_test_project, create_test_task,
};
use djinn_core::events::EventBus;
use djinn_db::EpicRepository;
use djinn_db::NoteRepository;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// Test-only adapter for legacy assertions while production dispatch carries
/// typed outcomes to the slot renderer.
pub(crate) trait ToolCallOutcomeTestExt {
    fn into_test_result(self) -> Result<serde_json::Value, String>;

    fn expect(self, message: &str) -> serde_json::Value
    where
        Self: Sized,
    {
        self.into_test_result().expect(message)
    }

    fn expect_err(self, message: &str) -> String
    where
        Self: Sized,
    {
        self.into_test_result().expect_err(message)
    }
}

impl ToolCallOutcomeTestExt for djinn_core::tool_call::ToolCallOutcome {
    fn into_test_result(self) -> Result<serde_json::Value, String> {
        match self {
            djinn_core::tool_call::ToolCallOutcome::Success { value, .. } => Ok(value),
            djinn_core::tool_call::ToolCallOutcome::Failure(
                djinn_core::tool_call::ToolCallFailure::Message(message),
            ) => Err(message),
            djinn_core::tool_call::ToolCallOutcome::Failure(
                djinn_core::tool_call::ToolCallFailure::Structured {
                    code,
                    message,
                    data,
                },
            ) => Err(format!(
                "{code:?}: {message}: {}",
                serde_json::to_string(&data).expect("compatibility metadata serializes")
            )),
        }
    }
}

mod code_graph_tests;
mod compatibility_fallback_tests;
mod edit_dispatch_tests;
mod epic_extension_tests;
mod evidence_spike_dispatch_tests;
mod gate_guard_dispatch_tests;
mod jit_trace_tests;
mod lsp_dispatch_tests;
mod lsp_tool_boundary_tests;
mod memory_dispatch_tests;
mod memory_mutation_param_tests;
mod phase_1_surface_guard_tests;
mod planner_routing_tests;
mod proposal_dispatch_tests;
mod schema_snapshot_tests;
mod shell_dispatch_tests;
mod skill_read_tests;
mod task_kill_session_tests;
mod tool_dispatch_tests;

/// Filesystem path a test can use in place of the removed `Project.path`
/// field. Derives `{DJINN_HOME}/projects/{owner}/{repo}` from the project's
/// github coords — matches how production code locates clones.
fn project_fs_path(project: &djinn_core::models::Project) -> PathBuf {
    djinn_core::paths::project_dir(&project.github_owner, &project.github_repo)
}

fn tool_names(schemas: &[serde_json::Value]) -> Vec<&str> {
    schemas
        .iter()
        .filter_map(|v| v.get("name").and_then(|n| n.as_str()))
        .collect()
}
