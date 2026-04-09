use std::path::Path;

use djinn_db::ProjectRepository;
use djinn_mcp::tools::memory_tools::*;
use serde_json::json;

use crate::events::EventBus;
use crate::test_helpers::{
    create_test_app, create_test_app_with_db, create_test_db, create_test_epic,
    create_test_project_with_dir, initialize_mcp_session, initialize_mcp_session_with_headers,
    mcp_call_tool, mcp_call_tool_with_headers,
};
use djinn_db::NoteRepository;

include!("memory_tools/contract_tests.rs");

include!("memory_tools/param_deserialization.rs");
