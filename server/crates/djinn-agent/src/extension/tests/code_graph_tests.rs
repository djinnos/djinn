use super::*;

// -----------------------------------------------------------------------
// code_graph dispatch tests
// -----------------------------------------------------------------------

/// Helper to invoke the `code_graph` tool through the public `call_tool` boundary.
async fn code_graph_tool(
    state: &AgentContext,
    args: serde_json::Value,
    worktree: &Path,
) -> Result<serde_json::Value, String> {
    call_tool(
        state,
        "code_graph",
        args.as_object()
            .expect("code_graph args must be an object")
            .clone()
            .into(),
        worktree,
        None,
        None,
        None,
    )
    .await
}

#[tokio::test]
async fn code_graph_dispatch_neighbors_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-neighbors-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "neighbors",
            "project_path": worktree.path().to_string_lossy(),
            "key": "src/lib.rs",
            "direction": "outgoing"
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    // The agent bridge stub rejects with a known message.
    assert!(
        err.contains("code_graph not available"),
        "neighbors should reach graph ops layer, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_ranked_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-ranked-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "ranked",
            "project_path": worktree.path().to_string_lossy(),
            "kind_filter": "file",
            "limit": 10
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("code_graph not available"),
        "ranked should reach graph ops layer, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_impact_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-impact-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "impact",
            "project_path": worktree.path().to_string_lossy(),
            "key": "rust-analyzer cargo . MyStruct#",
            "limit": 5
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("code_graph not available"),
        "impact should reach graph ops layer, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_implementations_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-impls-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "implementations",
            "project_path": worktree.path().to_string_lossy(),
            "key": "rust-analyzer cargo . MyTrait#"
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("code_graph not available"),
        "implementations should reach graph ops layer, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_rejects_unknown_operation() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-unknown-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "shortest_path",
            "project_path": worktree.path().to_string_lossy(),
            "key": "src/lib.rs"
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("unknown code_graph operation 'shortest_path'"),
        "expected unknown-operation error, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_neighbors_requires_key() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-no-key-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "neighbors",
            "project_path": worktree.path().to_string_lossy()
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("'key' is required"),
        "neighbors without key should fail, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_impact_requires_key() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-impact-no-key-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "impact",
            "project_path": worktree.path().to_string_lossy()
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("'key' is required"),
        "impact without key should fail, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_implementations_requires_key() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-impls-no-key-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "implementations",
            "project_path": worktree.path().to_string_lossy()
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("'key' is required"),
        "implementations without key should fail, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_search_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-search-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "search",
            "project_path": worktree.path().to_string_lossy(),
            "query": "AgentSession",
            "limit": 5,
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "search should reach graph ops layer, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_search_requires_query() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-search-no-query-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "search",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("'query' is required"),
        "search without query should fail, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_cycles_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-cycles-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "cycles",
            "project_path": worktree.path().to_string_lossy(),
            "min_size": 2,
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "cycles should reach graph ops layer, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_orphans_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-orphans-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "orphans",
            "project_path": worktree.path().to_string_lossy(),
            "visibility": "private",
            "limit": 10,
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "orphans should reach graph ops layer, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_path_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-path-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "path",
            "project_path": worktree.path().to_string_lossy(),
            "from": "src/a.rs",
            "to": "src/b.rs",
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "path should reach graph ops layer, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_path_requires_from_and_to() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-path-missing-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "path",
            "project_path": worktree.path().to_string_lossy(),
            "from": "src/a.rs",
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("'to' is required"),
        "path without 'to' should fail, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_edges_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-edges-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "edges",
            "project_path": worktree.path().to_string_lossy(),
            "from_glob": "server/src/**",
            "to_glob": "server/crates/**",
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "edges should reach graph ops layer, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_edges_requires_globs() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-edges-missing-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "edges",
            "project_path": worktree.path().to_string_lossy(),
            "from_glob": "server/src/**",
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("'to_glob' is required"),
        "edges without to_glob should fail, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_describe_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-describe-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "describe",
            "project_path": worktree.path().to_string_lossy(),
            "key": "scip-rust . . . AgentSession#",
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "describe should reach graph ops layer, got: {err}"
    );
}

/// v8 cochange op: routes through `RepoGraphOps::coupling`. Agent stub
/// returns "code_graph not available" — same pattern as every other
/// dispatch test. Verifies wiring rather than empty-state semantics.
#[tokio::test]
async fn code_graph_dispatch_cochange_with_key_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-cochange-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "cochange",
            "project_path": worktree.path().to_string_lossy(),
            "key": "file:internal/worker/page_worker.go",
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "cochange-with-key should reach graph ops layer, got: {err}"
    );
}

/// v8 cochange without key routes through `RepoGraphOps::coupling_hotspots`.
#[tokio::test]
async fn code_graph_dispatch_cochange_without_key_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-cochange-pairs-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "cochange",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "cochange-without-key should reach graph ops layer, got: {err}"
    );
}

/// v8 churn op: routes through `RepoGraphOps::churn`. Same dispatch
/// test pattern.
#[tokio::test]
async fn code_graph_dispatch_churn_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-churn-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "churn",
            "project_path": worktree.path().to_string_lossy(),
            "limit": 10,
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "churn should reach graph ops layer, got: {err}"
    );
}

/// v8 hotspots op: short-circuits cleanly when graph isn't warmed —
/// the underlying ranked() call hits the same "code_graph not available"
/// stub. Asserts the dispatch is wired even though the empty-state
/// behavior depends on warm + churn data.
#[tokio::test]
async fn code_graph_dispatch_hotspots_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-hotspots-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "hotspots",
            "project_path": worktree.path().to_string_lossy(),
            "limit": 5,
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "hotspots should reach the bridge stub, got: {err}"
    );
}

/// Iter 28 complexity op: dispatches through
/// `RepoGraphOps::complexity` and surfaces the unavailability error
/// when the agent stub is in play. Confirms the new arm is wired.
#[tokio::test]
async fn code_graph_dispatch_complexity_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-complexity-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "complexity",
            "project_path": worktree.path().to_string_lossy(),
            "target": "functions",
            "sort_by": "cognitive",
            "limit": 5,
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("complexity not available"),
        "complexity should reach the bridge stub, got: {err}"
    );
}

/// v8 final batch: 5 trait-delegation ops (status / snapshot /
/// symbols_at / diff_touches / detect_changes). Same pattern.
#[tokio::test]
async fn code_graph_dispatch_status_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-status-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "status",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(err.contains("code_graph not available"), "got: {err}");
}

#[tokio::test]
async fn code_graph_dispatch_snapshot_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-snapshot-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "snapshot",
            "project_path": worktree.path().to_string_lossy(),
            "node_cap": 1000,
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(err.contains("code_graph not available"), "got: {err}");
}

#[tokio::test]
async fn code_graph_dispatch_symbols_at_validates_inputs() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-symat-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "symbols_at",
            "project_path": worktree.path().to_string_lossy(),
            // Missing key + min_size — should hit arg validation.
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    // iter-21: error message updated to mention both new + legacy field names.
    assert!(
        err.contains("'file_path'") && err.contains("legacy 'key'"),
        "got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_diff_touches_validates_inputs() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-diff-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "diff_touches",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(err.contains("'changed_ranges' is required"), "got: {err}");
}

#[tokio::test]
async fn code_graph_dispatch_detect_changes_validates_inputs() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-dc-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "detect_changes",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("detect_changes requires"),
        "got: {err}"
    );
}

/// v8 batch: 6 trait-delegation ops (api_surface / metrics_at /
/// dead_symbols / deprecated_callers / touches_hot_path /
/// coupling_hubs) all reach the agent bridge stub. One test per op
/// — deliberately uniform so adding the next trait op only needs a
/// tiny copy-paste here.
#[tokio::test]
async fn code_graph_dispatch_api_surface_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-api-surface-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "api_surface",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(err.contains("code_graph not available"), "got: {err}");
}

#[tokio::test]
async fn code_graph_dispatch_metrics_at_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-metrics-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "metrics_at",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(err.contains("code_graph not available"), "got: {err}");
}

#[tokio::test]
async fn code_graph_dispatch_dead_symbols_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-dead-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "dead_symbols",
            "project_path": worktree.path().to_string_lossy(),
            "kind_filter": "high",
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(err.contains("code_graph not available"), "got: {err}");
}

#[tokio::test]
async fn code_graph_dispatch_deprecated_callers_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-deprecated-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "deprecated_callers",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(err.contains("code_graph not available"), "got: {err}");
}

#[tokio::test]
async fn code_graph_dispatch_touches_hot_path_validates_inputs() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-hotpath-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "touches_hot_path",
            "project_path": worktree.path().to_string_lossy(),
            // Missing the required from_glob/to_glob/query — should
            // fail with arg-validation message before reaching the
            // bridge stub.
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("touches_hot_path requires"),
        "should fail with arg-validation, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_coupling_hubs_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-hubs-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "coupling_hubs",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(err.contains("code_graph not available"), "got: {err}");
}

/// v8 boundary_check op: reaches the bridge layer (which short-circuits
/// in agent-side stub mode). Asserts the dispatch wire is hooked up
/// AND that the rules-required validation fires before the bridge.
#[tokio::test]
async fn code_graph_dispatch_boundary_check_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-boundary-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "boundary_check",
            "project_path": worktree.path().to_string_lossy(),
            "rules": [
                {
                    "name": "domain-must-not-depend-on-transport",
                    "from_glob": "internal/domain/**",
                    "forbid_to": ["internal/api/**", "internal/transport/**"]
                }
            ]
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "boundary_check should reach graph ops layer, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_boundary_check_requires_rules() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-boundary-no-rules-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "boundary_check",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("'rules' is required"),
        "boundary_check without rules should fail with arg-validation error, got: {err}"
    );
}

/// v8 blast_radius op: aggregates `neighbors(incoming, group_by=file)`
/// + `impact(group_by=file)`, categorises each file path into
/// runtime/tests/e2e_tests buckets. The agent bridge stub still short-
/// circuits before reaching graph_ops, so this test asserts the op is
/// wired (reaches the bridge) rather than the categorizer logic — the
/// path-classification logic is exercised by direct unit tests in
/// `code_intel.rs`.
#[tokio::test]
async fn code_graph_dispatch_blast_radius_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-blast-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "blast_radius",
            "project_path": worktree.path().to_string_lossy(),
            "key": "file:internal/worker/page_worker.go",
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "blast_radius should reach graph ops layer, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_blast_radius_requires_key() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-blast-no-key-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "blast_radius",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("'key' is required"),
        "blast_radius without key should fail with arg-validation error, got: {err}"
    );
}

/// v8 capability introspection: returns metadata about what's actually
/// wired in this binary — does NOT load the canonical graph, so it
/// works against a fresh tempdir with no warm cache. Asserts the
/// payload shape so client agents can rely on the keys being present.
#[tokio::test]
async fn code_graph_dispatch_capabilities_returns_introspection_payload() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-capabilities-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let result = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "capabilities",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .expect("capabilities should not error");

    // Top-level keys clients depend on:
    let obj = result.as_object().expect("payload must be a JSON object");
    assert!(obj.contains_key("operations"), "missing operations: {result}");
    assert!(obj.contains_key("default_search_mode"), "missing default_search_mode");
    assert!(obj.contains_key("available_search_modes"), "missing available_search_modes");
    assert!(obj.contains_key("env_features"), "missing env_features");
    assert!(obj.contains_key("access_classifier_languages"), "missing access_classifier_languages");
    assert!(obj.contains_key("repo_graph_artifact_version"), "missing repo_graph_artifact_version");
    assert!(obj.contains_key("filter_tiers"), "missing filter_tiers");
    assert!(obj.contains_key("default_filters"), "missing default_filters");

    // capabilities itself must list itself, otherwise clients can't
    // discover the op via probing.
    let ops = obj["operations"].as_array().expect("operations must be array");
    assert!(
        ops.iter().any(|o| o.as_str() == Some("capabilities")),
        "capabilities op must list itself in `operations`"
    );

    // Artifact version stamp matches the v8 bump.
    assert_eq!(obj["repo_graph_artifact_version"], 8);

    // Languages we ship a tree-sitter classifier for.
    let langs = obj["access_classifier_languages"]
        .as_array()
        .expect("languages must be array");
    for required in ["rust", "go", "python", "typescript", "javascript"] {
        assert!(
            langs.iter().any(|l| l.as_str() == Some(required)),
            "missing language {required} in access_classifier_languages"
        );
    }
}
