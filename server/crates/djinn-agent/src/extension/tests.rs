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
mod epic_extension_tests;
pub(crate) mod fuzzy_replace_tests;
mod lsp_dispatch_tests;
mod lsp_tool_boundary_tests;
mod memory_dispatch_tests;
mod schema_snapshot_tests;
mod tool_dispatch_tests;

fn tool_names(schemas: &[serde_json::Value]) -> Vec<&str> {
    schemas
        .iter()
        .filter_map(|v| v.get("name").and_then(|n| n.as_str()))
        .collect()
}

fn tool_schema<'a>(schemas: &'a [serde_json::Value], name: &str) -> &'a serde_json::Value {
    schemas
        .iter()
        .find(|schema| schema.get("name").and_then(|n| n.as_str()) == Some(name))
        .expect("tool schema present")
}
