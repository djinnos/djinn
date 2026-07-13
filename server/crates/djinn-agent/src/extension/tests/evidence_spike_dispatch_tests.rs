//! Tests for evidence-spike allowlist enforcement through the agent-local
//! fallback dispatch.
//!
//! These tests verify that:
//! 1. Blocked mutation tools (`write`, `edit`, `apply_patch`, `shell`,
//!    `task_delete_branch`, `task_transition`, `request_lead`,
//!    `request_planner`) are rejected under the evidence-spike allowlist.
//! 2. Allowed read-only tools (`read`, `code_search`, `code_graph`,
//!    `skill_read`, `lsp`) remain routable.
//! 3. Dynamic MCP registry tools are rejected under the evidence-spike
//!    allowlist.
//! 4. Normal Architect/Worker call paths remain unchanged when no
//!    evidence-spike allowlist is provided.

use super::*;
use crate::extension::handlers::dispatch_tool_call;
use crate::mcp_client::McpToolRegistry;

/// The evidence-spike schemas used as `allowed_schemas` in dispatch.
fn evidence_spike_schemas() -> Vec<serde_json::Value> {
    crate::init_tool_schema_registry();
    tool_schemas_evidence_spike()
}

/// Build a synthetic `{"name": ..., "arguments": ...}` tool call for the
/// given tool name and optional arguments map.
fn make_tool_call(
    name: &str,
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
) -> serde_json::Value {
    serde_json::json!({ "name": name, "arguments": arguments })
}

// ── Blocked tools: evidence-spike allowlist must reject mutation/destructive
//    tools before the local fallback match arms execute. ─────────────────

#[tokio::test]
async fn evidence_spike_blocks_write_in_local_fallback() {
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    let services = crate::test_helpers::test_services();
    let tmp = crate::test_helpers::test_tempdir("djinn-ev-fallback-write-");
    let schemas = evidence_spike_schemas();

    let result = dispatch_tool_call(
        &state,
        &services,
        &make_tool_call(
            "write",
            Some(
                serde_json::json!({"path": "x.txt", "content": "hi"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        ),
        tmp.path(),
        Some(&schemas),
        None,
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await;

    assert!(
        result.is_err(),
        "write must be rejected under evidence-spike allowlist, got: {result:?}"
    );
    let err = result.unwrap_err();
    assert!(
        err.contains("not in the allowed schema list"),
        "error should mention allowed schema list, got: {err}"
    );
}

#[tokio::test]
async fn evidence_spike_blocks_edit_in_local_fallback() {
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    let services = crate::test_helpers::test_services();
    let tmp = crate::test_helpers::test_tempdir("djinn-ev-fallback-edit-");
    let schemas = evidence_spike_schemas();

    let result = dispatch_tool_call(
        &state,
        &services,
        &make_tool_call(
            "edit",
            Some(
                serde_json::json!({"path": "x.txt", "old_text": "a", "new_text": "b"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        ),
        tmp.path(),
        Some(&schemas),
        None,
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await;

    assert!(
        result.is_err(),
        "edit must be rejected under evidence-spike allowlist, got: {result:?}"
    );
}

#[tokio::test]
async fn evidence_spike_blocks_apply_patch_in_local_fallback() {
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    let services = crate::test_helpers::test_services();
    let tmp = crate::test_helpers::test_tempdir("djinn-ev-fallback-patch-");
    let schemas = evidence_spike_schemas();

    let result = dispatch_tool_call(
        &state,
        &services,
        &make_tool_call(
            "apply_patch",
            Some(
                serde_json::json!({"patch": "*** Begin Patch\n*** End Patch"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        ),
        tmp.path(),
        Some(&schemas),
        None,
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await;

    assert!(
        result.is_err(),
        "apply_patch must be rejected under evidence-spike allowlist, got: {result:?}"
    );
}

#[tokio::test]
async fn evidence_spike_blocks_shell_in_local_fallback() {
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    let services = crate::test_helpers::test_services();
    let tmp = crate::test_helpers::test_tempdir("djinn-ev-fallback-shell-");
    let schemas = evidence_spike_schemas();

    let result = dispatch_tool_call(
        &state,
        &services,
        &make_tool_call(
            "shell",
            Some(
                serde_json::json!({"command": "echo hello"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        ),
        tmp.path(),
        Some(&schemas),
        None,
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await;

    assert!(
        result.is_err(),
        "shell must be rejected under evidence-spike allowlist, got: {result:?}"
    );
}

#[tokio::test]
async fn evidence_spike_blocks_task_delete_branch_in_local_fallback() {
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    let services = crate::test_helpers::test_services();
    let tmp = crate::test_helpers::test_tempdir("djinn-ev-fallback-del-");
    let schemas = evidence_spike_schemas();

    let result = dispatch_tool_call(
        &state,
        &services,
        &make_tool_call(
            "task_delete_branch",
            Some(
                serde_json::json!({"id": "fake-task"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        ),
        tmp.path(),
        Some(&schemas),
        None,
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await;

    assert!(
        result.is_err(),
        "task_delete_branch must be rejected under evidence-spike allowlist, got: {result:?}"
    );
}

#[tokio::test]
async fn evidence_spike_blocks_task_transition_in_local_fallback() {
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    let services = crate::test_helpers::test_services();
    let tmp = crate::test_helpers::test_tempdir("djinn-ev-fallback-trans-");
    let schemas = evidence_spike_schemas();

    let result = dispatch_tool_call(
        &state,
        &services,
        &make_tool_call(
            "task_transition",
            Some(
                serde_json::json!({"id": "fake-task", "status": "closed"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        ),
        tmp.path(),
        Some(&schemas),
        None,
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await;

    assert!(
        result.is_err(),
        "task_transition must be rejected under evidence-spike allowlist, got: {result:?}"
    );
}

#[tokio::test]
async fn evidence_spike_blocks_request_lead_in_local_fallback() {
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    let services = crate::test_helpers::test_services();
    let tmp = crate::test_helpers::test_tempdir("djinn-ev-fallback-lead-");
    let schemas = evidence_spike_schemas();

    let result = dispatch_tool_call(
        &state,
        &services,
        &make_tool_call(
            "request_lead",
            Some(
                serde_json::json!({"id": "fake-task", "reason": "stuck"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        ),
        tmp.path(),
        Some(&schemas),
        None,
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await;

    assert!(
        result.is_err(),
        "request_lead must be rejected under evidence-spike allowlist, got: {result:?}"
    );
}

#[tokio::test]
async fn evidence_spike_blocks_request_planner_in_local_fallback() {
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    let services = crate::test_helpers::test_services();
    let tmp = crate::test_helpers::test_tempdir("djinn-ev-fallback-plan-");
    let schemas = evidence_spike_schemas();

    let result = dispatch_tool_call(
        &state,
        &services,
        &make_tool_call(
            "request_planner",
            Some(
                serde_json::json!({"id": "fake-task", "reason": "stuck"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        ),
        tmp.path(),
        Some(&schemas),
        None,
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await;

    assert!(
        result.is_err(),
        "request_planner must be rejected under evidence-spike allowlist, got: {result:?}"
    );
}

// ── Allowed read-only tools: evidence-spike allowlist must NOT block
//    these tools from reaching the dispatch arms. ─────────────────────────

#[tokio::test]
async fn evidence_spike_allows_code_search_in_local_fallback() {
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    let services = crate::test_helpers::test_services();
    let tmp = crate::test_helpers::test_tempdir("djinn-ev-fallback-csearch-");
    let schemas = evidence_spike_schemas();

    // code_search should reach the dispatch arm (not be blocked by the
    // allowlist).  It may fail with a project-resolution error, which is
    // fine — the important thing is it's NOT rejected as "not in the
    // allowed schema list".
    let result = dispatch_tool_call(
        &state,
        &services,
        &make_tool_call(
            "code_search",
            Some(
                serde_json::json!({"query": "test"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        ),
        tmp.path(),
        Some(&schemas),
        None,
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await;

    // If it's an allowlist rejection, that's a failure.
    if let Err(ref e) = result {
        assert!(
            !e.contains("not in the allowed schema list"),
            "code_search must not be blocked by evidence-spike allowlist, got: {e}"
        );
        // Any other error (e.g. missing project) is acceptable — we're only
        // testing the allowlist gate, not the tool's full implementation.
    }
}

#[tokio::test]
async fn evidence_spike_allows_skill_read_in_local_fallback() {
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    let services = crate::test_helpers::test_services();
    let tmp = crate::test_helpers::test_tempdir("djinn-ev-fallback-skill-");
    let schemas = evidence_spike_schemas();

    // skill_read should reach the dispatch arm (not blocked by allowlist).
    // It will likely fail with "skill not found" which is fine.
    let result = dispatch_tool_call(
        &state,
        &services,
        &make_tool_call(
            "skill_read",
            Some(
                serde_json::json!({"name": "nonexistent"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        ),
        tmp.path(),
        Some(&schemas),
        None,
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await;

    if let Err(ref e) = result {
        assert!(
            !e.contains("not in the allowed schema list"),
            "skill_read must not be blocked by evidence-spike allowlist, got: {e}"
        );
    }
}

#[tokio::test]
async fn evidence_spike_allows_read_in_local_fallback() {
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    let services = crate::test_helpers::test_services();
    let tmp = crate::test_helpers::test_tempdir("djinn-ev-fallback-read-");
    let schemas = evidence_spike_schemas();

    let result = dispatch_tool_call(
        &state,
        &services,
        &make_tool_call(
            "read",
            Some(
                serde_json::json!({"file_path": "nonexistent.txt"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        ),
        tmp.path(),
        Some(&schemas),
        None,
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await;

    if let Err(ref e) = result {
        assert!(
            !e.contains("not in the allowed schema list"),
            "read must not be blocked by evidence-spike allowlist, got: {e}"
        );
    }
}

#[tokio::test]
async fn evidence_spike_allows_code_graph_in_local_fallback() {
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    let services = crate::test_helpers::test_services();
    let tmp = crate::test_helpers::test_tempdir("djinn-ev-fallback-graph-");
    let schemas = evidence_spike_schemas();

    // code_graph is Unhandled by the extension and falls through to local
    // fallback.  Should not be blocked by the allowlist.
    let result = dispatch_tool_call(
        &state,
        &services,
        &make_tool_call(
            "code_graph",
            Some(
                serde_json::json!({"symbol": "foo"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        ),
        tmp.path(),
        Some(&schemas),
        None,
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await;

    if let Err(ref e) = result {
        assert!(
            !e.contains("not in the allowed schema list"),
            "code_graph must not be blocked by evidence-spike allowlist, got: {e}"
        );
    }
}

// ── Final findings handoff: submit_work must not be blocked ──────────────
// AC#1 requires that the final findings handoff path still works for
// evidence-spike sessions. submit_work is the only allowed mutation-capable
// finalize tool, and it must reach the dispatch arm without being blocked
// by the evidence-spike allowlist gate.

#[tokio::test]
async fn evidence_spike_allows_submit_work_findings_handoff() {
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    let services = crate::test_helpers::test_services();
    let tmp = crate::test_helpers::test_tempdir("djinn-ev-fallback-submit-");
    let schemas = evidence_spike_schemas();

    // submit_work should reach the dispatch arm (not be blocked by the
    // allowlist).  It will likely fail with a task-resolution error because
    // no real task exists in the test DB, which is fine — the important
    // thing is it's NOT rejected as "not in the allowed schema list".
    let result = dispatch_tool_call(
        &state,
        &services,
        &make_tool_call(
            "submit_work",
            Some(
                serde_json::json!({"task_id": "fake-task", "commit_title": "findings", "summary": "evidence findings"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        ),
        tmp.path(),
        Some(&schemas),
        None,
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await;

    if let Err(ref e) = result {
        assert!(
            !e.contains("not in the allowed schema list"),
            "submit_work must not be blocked by evidence-spike allowlist (findings handoff path), got: {e}"
        );
    }
}

// ── Dynamic MCP registry tools: must be rejected under evidence-spike ────

#[tokio::test]
async fn evidence_spike_blocks_dynamic_mcp_registry_tool() {
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    let services = crate::test_helpers::test_services();
    let tmp = crate::test_helpers::test_tempdir("djinn-ev-fallback-mcp-");
    let schemas = evidence_spike_schemas();

    // Create a mock MCP registry with a "dangerous_tool".
    let mcp_registry = McpToolRegistry::with_dispatch(
        vec![("dangerous_tool".to_string(), "test-server".to_string())],
        vec![serde_json::json!({"name": "dangerous_tool"})],
        |_name, _args| Err("should not be called".to_string()),
    );

    let result = dispatch_tool_call(
        &state,
        &services,
        &make_tool_call(
            "dangerous_tool",
            Some(
                serde_json::json!({"key": "value"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        ),
        tmp.path(),
        Some(&schemas),
        None,
        None,
        Some(&mcp_registry),
        &crate::extension::ToolCancellation::never(),
    )
    .await;

    assert!(
        result.is_err(),
        "dynamic MCP registry tool must be rejected under evidence-spike, got: {result:?}"
    );
    let err = result.unwrap_err();
    // The tool is rejected — either by the generic schema gate ("not in the
    // allowed schema list") or by the MCP-specific guard ("dynamic MCP
    // registry tool … not permitted").  Both are correct; the important
    // thing is the call never reaches the MCP server.
    assert!(
        err.contains("not in the allowed schema list")
            || (err.contains("dynamic MCP registry tool") && err.contains("not permitted")),
        "error should indicate the tool is blocked under the restricted profile, got: {err}"
    );
}

// ── Normal paths: no allowlist means everything works as before ──────────

#[tokio::test]
async fn normal_dispatch_no_allowlist_allows_mcp_registry_tool() {
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    let services = crate::test_helpers::test_services();
    let tmp = crate::test_helpers::test_tempdir("djinn-ev-fallback-norm-mcp-");

    // Create a mock MCP registry with a tool that returns success.
    let mcp_registry = McpToolRegistry::with_dispatch(
        vec![("custom_tool".to_string(), "test-server".to_string())],
        vec![serde_json::json!({"name": "custom_tool"})],
        |_name, _args| Ok(serde_json::json!({"ok": true})),
    );

    // No allowed_schemas — normal dispatch path.
    let result = dispatch_tool_call(
        &state,
        &services,
        &make_tool_call("custom_tool", None),
        tmp.path(),
        None, // no allowlist
        None,
        None,
        Some(&mcp_registry),
        &crate::extension::ToolCancellation::never(),
    )
    .await;

    assert!(
        result.is_ok(),
        "MCP registry tool must succeed with no allowlist (normal path), got: {result:?}"
    );
}

#[tokio::test]
async fn normal_dispatch_no_allowlist_does_not_block_mutation_tools() {
    // With no allowed_schemas (normal dispatch), mutation tools should
    // reach the match arms without being blocked by the guard.
    // We can't fully execute them (need project, worktree, etc.) but we
    // verify they don't get the allowlist error.
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    let services = crate::test_helpers::test_services();
    let tmp = crate::test_helpers::test_tempdir("djinn-ev-fallback-norm-mut-");

    // shell with no allowlist should attempt dispatch (not be blocked).
    // It may fail for other reasons (sandbox, etc.) but must NOT get
    // "not in the allowed schema list".
    let result = dispatch_tool_call(
        &state,
        &services,
        &make_tool_call(
            "shell",
            Some(
                serde_json::json!({"command": "true"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        ),
        tmp.path(),
        None, // no allowlist
        None,
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await;

    if let Err(ref e) = result {
        assert!(
            !e.contains("not in the allowed schema list"),
            "shell must not be blocked without allowlist (normal path), got: {e}"
        );
    }
}

// ── Schema consistency: evidence_spike_tool_names covers all schemas ─────

#[test]
fn evidence_spike_tool_names_match_schema_set() {
    crate::init_tool_schema_registry();
    let schemas = tool_schemas_evidence_spike();
    let names = evidence_spike_tool_names();

    let schema_names: std::collections::BTreeSet<String> = schemas
        .iter()
        .filter_map(|s| s.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect();

    assert_eq!(
        names, schema_names,
        "evidence_spike_tool_names() must exactly match the names in tool_schemas_evidence_spike()"
    );
}

#[test]
fn evidence_spike_excludes_known_mutation_tools() {
    crate::init_tool_schema_registry();
    let names = evidence_spike_tool_names();

    let blocked = [
        "write",
        "edit",
        "apply_patch",
        "shell",
        "task_delete_branch",
        "task_kill_session",
        "task_transition",
        "request_lead",
        "request_planner",
        "task_create",
        "task_update",
        "epic_create",
        "epic_update",
        "epic_close",
        "memory_write",
        "memory_edit",
        "memory_move",
    ];

    for tool in &blocked {
        assert!(
            !names.contains(*tool),
            "evidence-spike allowlist must not include blocked tool `{tool}`"
        );
    }
}

#[test]
fn evidence_spike_includes_required_read_only_tools() {
    crate::init_tool_schema_registry();
    let names = evidence_spike_tool_names();

    let required = [
        "read",
        "code_search",
        "code_graph",
        "skill_read",
        "lsp",
        "ci_job_log",
        "github_search",
        "output_view",
        "output_grep",
        "memory_read",
        "memory_search",
        "memory_list",
        "memory_build_context",
        "epic_show",
        "epic_tasks",
        "submit_work",
    ];

    for tool in &required {
        assert!(
            names.contains(*tool),
            "evidence-spike allowlist must include required read-only tool `{tool}`"
        );
    }
}
