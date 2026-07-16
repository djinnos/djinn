// Agent-domain code extracted from djinn-server.
// Covers: commands, agent context/roles/lifecycle, actors.
//
// The `tool_code_graph()` schema (now in `djinn-mcp-extension`) uses
// `serde_json::json!` via the `object!` macro. After Phase 2 added seven
// new ops, the macro expansion outgrew the default 128 recursion budget.
// Kept here for safety since `djinn-agent` still re-exports the schema.
#![recursion_limit = "256"]

pub(crate) mod commands;
pub mod dispatch_pause;
pub mod doctor;
pub(crate) mod events;
pub(crate) mod process;
pub(crate) mod rollout;

// ─── Agent module (was src/agent/) ───────────────────────────────────────────

pub mod compaction;
pub mod context;
// Extension tools: `pub(crate)` internals, `chat_tools` re-exports the chat-safe subset.
pub mod chat_tools;
pub mod direct_services;
pub mod environment;
pub(crate) mod extension;
pub(crate) mod extension_diagnostics;
pub mod extension_diagnostics_probe;
pub mod file_time;
pub(crate) mod github_error_render;
pub(crate) mod knowledge_promotion;
pub mod lsp;
pub mod mcp_client;
pub mod mcp_settings;
pub mod native_skills;
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

// ─── djinn-slot re-exports ─────────────────────────────────────────────────
//
// Phase 5: the canonical slot code now lives in `djinn-slot`.  The
// `djinn_agent::actors::slot::*` facade paths are preserved above; these
// additional re-exports expose the new host-context types so downstream
// crates can construct a `SlotContext` and wire `SlotHostCallbacks`.

pub use djinn_slot::SlotEvent;
pub use djinn_slot::host::{KnowledgeBranchTarget, SlotContext, SlotHostCallbacks};

// ─── AgentType (re-exported from djinn-roles) ──────────────────────────────

pub use djinn_roles::AgentType;

/// Initialize the djinn-roles tool schema registry with the extension-provided
/// schema functions.
///
/// Idempotent: safe to call multiple times — the registry is only populated
/// on the first call.  Called automatically from [`roles::RoleRegistry::new()`]
/// so production boot paths get tool schemas without a separate init step.
pub fn init_tool_schema_registry() {
    use std::sync::Once;

    static INIT: Once = Once::new();
    INIT.call_once(|| {
        use std::collections::HashMap;

        let mut schemas: HashMap<&'static str, fn() -> Vec<serde_json::Value>> = HashMap::new();
        schemas.insert("worker", extension::tool_schemas_worker);
        schemas.insert("reviewer", extension::tool_schemas_reviewer);
        schemas.insert("lead", extension::tool_schemas_lead);
        schemas.insert("planner", extension::tool_schemas_planner);
        schemas.insert("architect", extension::tool_schemas_architect);
        // Tribunal refinement roles (k9zw).
        schemas.insert("advocate", extension::tool_schemas_advocate);
        schemas.insert("adversary", extension::tool_schemas_adversary);
        schemas.insert("judge", extension::tool_schemas_judge);
        // Evidence-spike profile: read-only investigation tools for
        // tasks created by the Judge demand-evidence path (epic 6tjy).
        schemas.insert("evidence_spike", extension::tool_schemas_evidence_spike);

        djinn_roles::register_tool_schemas(schemas);
    });
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_helpers;

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
            AgentType::Advocate,
            AgentType::Adversary,
            AgentType::Judge,
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
        assert_eq!(AgentType::Advocate.dispatch_role(), "advocate");
        assert_eq!(AgentType::Adversary.dispatch_role(), "adversary");
        assert_eq!(AgentType::Judge.dispatch_role(), "judge");
    }
}

/// Regression: verify the djinn-agent compatibility facade for djinn-roles
/// imports remains available.
///
/// After Phase 3 extraction, `AgentType`, `RoleConfig`, `config_for`,
/// `TaskContext`, and prompt templates are owned by `djinn-roles` but
/// re-exported under the original `djinn_agent::*` paths.  These compile-time
/// assertions catch accidental removal of the re-exports.
#[cfg(test)]
mod djinn_roles_facade_regression {
    // ── AgentType re-export ────────────────────────────────────────────
    #[test]
    fn agent_type_available_through_djinn_agent() {
        // The type must be accessible via the crate root.
        let _agent_type: super::AgentType = super::AgentType::Worker;
    }

    // ── RoleConfig and config_for re-export via roles module ──────────
    #[test]
    fn role_config_and_config_for_available_through_djinn_agent_roles() {
        let cfg: &crate::roles::RoleConfig = crate::roles::config_for(super::AgentType::Worker);
        assert_eq!(cfg.name, "worker");
    }

    // ── TaskContext and render fn accessible via prompts module ────────
    // We don't construct TaskContext (many fields), but verify the type
    // alias / re-export resolves and a function accepting it is reachable.
    #[test]
    fn task_context_and_render_accessible_through_djinn_agent_prompts() {
        // Verify the type is visible.
        fn _assert_type_exists(_ctx: &crate::prompts::TaskContext) {}
        // Verify the facade re-exported render function compiles.
        let _render_fn: fn(
            &crate::roles::RoleConfig,
            &djinn_core::models::Task,
            &crate::prompts::TaskContext,
        ) -> String = crate::prompts::render_prompt_for_role;
    }

    // ── Prompt template re-exports ────────────────────────────────────
    #[test]
    fn prompt_templates_available_through_djinn_agent_prompts() {
        // All public templates should be non-empty static strings accessible
        // through the djinn_agent::prompts facade.
        assert!(!crate::prompts::BASE_TEMPLATE.is_empty());
        assert!(!crate::prompts::DEV_TEMPLATE.is_empty());
        assert!(!crate::prompts::REVIEWER_TEMPLATE.is_empty());
        assert!(!crate::prompts::LEAD_TEMPLATE.is_empty());
        assert!(!crate::prompts::PLANNER_TEMPLATE.is_empty());
        assert!(!crate::prompts::ARCHITECT_TEMPLATE.is_empty());
        assert!(!crate::prompts::CLUSTER_DOC_TEMPLATE.is_empty());
        assert!(!crate::prompts::ADVOCATE_TEMPLATE.is_empty());
        assert!(!crate::prompts::ADVERSARY_TEMPLATE.is_empty());
        assert!(!crate::prompts::JUDGE_TEMPLATE.is_empty());
    }
}
