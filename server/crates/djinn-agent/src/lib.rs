// Agent-domain code extracted from djinn-server.
// Covers: commands, agent context/roles/lifecycle, actors.
//
// The `tool_code_graph()` schema uses `serde_json::json!` via the local
// `object!` macro. After Phase 2 added seven new ops, the macro
// expansion outgrew the default 128 recursion budget.
#![recursion_limit = "256"]

pub(crate) mod commands;
pub mod dispatch_pause;
pub mod doctor;
pub(crate) mod events;
pub(crate) mod process;

// ─── Agent module (was src/agent/) ───────────────────────────────────────────

pub mod compaction;
pub mod context;
// Extension tools: `pub(crate)` internals, `chat_tools` re-exports the chat-safe subset.
pub mod chat_tools;
pub mod direct_services;
pub mod environment;
pub(crate) mod extension;
pub mod file_time;
pub(crate) mod github_error_render;
pub(crate) mod knowledge_promotion;
pub mod lsp;
pub mod mcp_client;
pub mod mcp_settings;
pub(crate) mod oauth;
pub(crate) mod output_parser;
pub mod output_stash;
pub(crate) mod patch;
pub mod prompts;
pub mod repo_access;
pub mod roles;
pub mod runtime_bridge;
pub(crate) mod sandbox;
pub mod skills;
pub mod skills_manifest;
pub mod supervisor;
pub(crate) mod supervisor_impl;
pub mod task_confidence;
pub mod task_merge;
pub(crate) mod truncate;
pub mod warmer;

// ─── Resource monitoring ─────────────────────────────────────────────────────

pub mod resource_monitor;

// ─── Actors (was src/actors/) ────────────────────────────────────────────────

pub mod actors;

pub use actors::coordinator::{
    BreakerDebugEntry, CoordinatorDebugSnapshot, DebugCooldown, DebugDispatchState,
    DebugFailureStreak, DebugInflightEntry, DebugSlot, DebugTotals, DispatchPauseView,
};

/// One-shot recovery sweep that backfills post-session knowledge extraction
/// over completed-but-unextracted task-runs. Triggered from the server boot
/// path behind an env flag — see `run_extraction_backfill` for the policy.
pub use actors::slot::session_extraction::run_extraction_backfill;

// ─── AgentType (re-exported from djinn-roles) ──────────────────────────────

pub use djinn_roles::AgentType;

/// Initialize the djinn-roles tool schema registry with the extension-provided
/// schema functions.
///
/// Must be called once at startup before any prompt rendering that requires
/// tool schemas.  Typically called from the server boot path.
pub fn init_tool_schema_registry() {
    use std::collections::HashMap;

    let mut schemas: HashMap<&'static str, fn() -> Vec<serde_json::Value>> = HashMap::new();
    schemas.insert("worker", extension::tool_schemas_worker);
    schemas.insert("reviewer", extension::tool_schemas_reviewer);
    schemas.insert("lead", extension::tool_schemas_lead);
    schemas.insert("planner", extension::tool_schemas_planner);
    schemas.insert("architect", extension::tool_schemas_architect);

    djinn_roles::register_tool_schemas(schemas);
}

#[cfg(test)]
pub(crate) mod test_helpers;

#[cfg(test)]
mod tests {
    use super::AgentType;
    use crate::roles;

    fn assert_equivalent_to_role_config(agent_type: AgentType) {
        let cfg = roles::config_for(agent_type);
        assert_eq!(agent_type.as_str(), cfg.name);
        assert_eq!(agent_type.dispatch_role(), cfg.dispatch_role);
    }

    #[test]
    fn role_config_equivalence_for_all_agent_types() {
        for agent_type in [
            AgentType::Worker,
            AgentType::Reviewer,
            AgentType::Lead,
            AgentType::Planner,
            AgentType::Architect,
        ] {
            assert_equivalent_to_role_config(agent_type);
        }
    }

    #[test]
    fn for_task_status_covers_all_expected_paths() {
        // Tasks with conflict context now route to Worker, not a dedicated conflict resolver
        assert_eq!(AgentType::for_task_status("open", false), AgentType::Worker);
        assert_eq!(AgentType::for_task_status("open", true), AgentType::Worker);
        assert_eq!(
            AgentType::for_task_status("needs_task_review", false),
            AgentType::Reviewer
        );
        assert_eq!(
            AgentType::for_task_status("in_task_review", false),
            AgentType::Reviewer
        );
        assert_eq!(
            AgentType::for_task_status("needs_lead_intervention", false),
            AgentType::Lead
        );
        assert_eq!(
            AgentType::for_task_status("in_lead_intervention", false),
            AgentType::Lead
        );
    }

    #[test]
    fn dispatch_role_for_all_variants() {
        assert_eq!(AgentType::Worker.dispatch_role(), "worker");
        assert_eq!(AgentType::Reviewer.dispatch_role(), "reviewer");
        assert_eq!(AgentType::Lead.dispatch_role(), "lead");
        assert_eq!(AgentType::Planner.dispatch_role(), "planner");
        assert_eq!(AgentType::Architect.dispatch_role(), "architect");
    }
}
