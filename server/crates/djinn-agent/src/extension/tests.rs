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

mod code_graph_tests;
mod edit_dispatch_tests;
mod epic_extension_tests;
mod evidence_spike_dispatch_tests;
mod gate_guard_dispatch_tests;
mod lsp_dispatch_tests;
mod lsp_tool_boundary_tests;
mod memory_dispatch_tests;
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
