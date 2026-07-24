//! Rendered-surface guards for the configuration-aware `run_verification`
//! surface and the unconditional `prepare_build_cache` worker tool (epic 1bnj).
//!
//! Both the configured (non-empty final-verification plan) and empty-plan cases
//! are covered for Worker and Reviewer, at the schema-surface level and in the
//! rendered prompt prose.

use djinn_core::events::EventBus;
use djinn_db::Database;
use djinn_mcp_extension::tool_defs::{tool_schemas_reviewer, tool_schemas_worker};
use djinn_roles::prompts::retain_conditional_verification_tools;

use super::test_support::create_project_epic_task;
use crate::AgentType;
use crate::prompts::{TaskContext, render_prompt_for_role};

fn tool_names(schemas: &[serde_json::Value]) -> Vec<String> {
    schemas
        .iter()
        .filter_map(|s| s.get("name").and_then(|n| n.as_str()).map(str::to_string))
        .collect()
}

/// Resolve the live per-project tool surface for a role the way the supervisor
/// stage does: canonical schema set with the conditional verification tool
/// stripped when the plan is empty.
fn live_surface(schemas: Vec<serde_json::Value>, configured: bool) -> Vec<String> {
    let mut schemas = schemas;
    retain_conditional_verification_tools(&mut schemas, configured);
    tool_names(&schemas)
}

#[test]
fn worker_surface_gates_run_verification_but_always_has_prepare_build_cache() {
    let configured = live_surface(tool_schemas_worker(), true);
    assert!(
        configured.iter().any(|n| n == "run_verification"),
        "configured worker must advertise run_verification"
    );
    assert!(
        configured.iter().any(|n| n == "prepare_build_cache"),
        "configured worker must advertise prepare_build_cache"
    );

    let empty = live_surface(tool_schemas_worker(), false);
    assert!(
        !empty.iter().any(|n| n == "run_verification"),
        "empty-plan worker must NOT advertise run_verification"
    );
    assert!(
        empty.iter().any(|n| n == "prepare_build_cache"),
        "prepare_build_cache renders for the worker regardless of gate configuration"
    );
}

#[test]
fn reviewer_surface_gates_run_verification_and_never_has_prepare_build_cache() {
    let configured = live_surface(tool_schemas_reviewer(), true);
    assert!(
        configured.iter().any(|n| n == "run_verification"),
        "configured reviewer must advertise run_verification"
    );
    assert!(
        !configured.iter().any(|n| n == "prepare_build_cache"),
        "prepare_build_cache is a worker-only tool"
    );

    let empty = live_surface(tool_schemas_reviewer(), false);
    assert!(
        !empty.iter().any(|n| n == "run_verification"),
        "empty-plan reviewer must NOT advertise run_verification"
    );
}

async fn make_task() -> djinn_core::models::Task {
    let db = Database::ephemeral().await.expect("ephemeral database");
    let events = EventBus::noop();
    create_project_epic_task(&db, &events, "1bnj epic", "Conditional surface").await
}

fn ctx(final_verification_configured: bool) -> TaskContext {
    TaskContext {
        project_path: "proj".into(),
        workspace_path: "/workspace/wt".into(),
        diff: None,
        commits: None,
        start_commit: None,
        end_commit: None,
        conflict_files: None,
        merge_base_branch: None,
        merge_target_branch: None,
        merge_failure_context: None,
        setup_commands: None,
        activity: None,
        worker_summary: None,
        worker_concerns: None,
        epic_context: None,
        knowledge_context: None,
        code_graph_context: None,
        reviewer_diff_context: None,
        ci_blocking_directive: None,
        worker_resume_note: None,
        arbiter_directive: None,
        final_verification_configured,
    }
}

#[tokio::test]
async fn worker_prompt_prose_is_configuration_aware() {
    crate::init_tool_schema_registry();
    let task = make_task().await;

    let configured = render_prompt_for_role(AgentType::Worker.role_config(), &task, &ctx(true));
    assert!(
        configured.contains("## Final Verification"),
        "configured worker prompt must recommend iterating with run_verification"
    );
    assert!(
        configured.contains("not credited"),
        "configured worker prompt must explain shell builds are permitted but not credited"
    );
    assert!(
        configured.contains("- `run_verification()`"),
        "configured worker tools section must list run_verification"
    );

    let empty = render_prompt_for_role(AgentType::Worker.role_config(), &task, &ctx(false));
    assert!(
        !empty.contains("## Final Verification"),
        "empty-plan worker prompt must render legacy language with no gate claim"
    );
    assert!(
        !empty.contains("- `run_verification()`"),
        "empty-plan worker tools section must omit run_verification"
    );
    // prepare_build_cache is present in both cases.
    assert!(empty.contains("- `prepare_build_cache()`"));
    assert!(configured.contains("- `prepare_build_cache()`"));
}

#[tokio::test]
async fn reviewer_prompt_prose_is_configuration_aware() {
    crate::init_tool_schema_registry();
    let task = make_task().await;

    let configured = render_prompt_for_role(AgentType::Reviewer.role_config(), &task, &ctx(true));
    assert!(
        configured.contains("## Independent Verification"),
        "configured reviewer prompt must expose independent rerun guidance"
    );
    assert!(configured.contains("- `run_verification()`"));

    let empty = render_prompt_for_role(AgentType::Reviewer.role_config(), &task, &ctx(false));
    assert!(
        !empty.contains("## Independent Verification"),
        "empty-plan reviewer prompt must render legacy language with no gate claim"
    );
    assert!(!empty.contains("- `run_verification()`"));
}
