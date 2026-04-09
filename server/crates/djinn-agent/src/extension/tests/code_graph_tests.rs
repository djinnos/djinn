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
async fn code_graph_dispatch_diff_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-diff-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "diff",
            "project_path": worktree.path().to_string_lossy(),
            "since": "previous",
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "diff should reach graph ops layer, got: {err}"
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

#[tokio::test]
async fn lsp_tool_boundary_symbols_with_depth_only() {
    let worktree = crate::test_helpers::test_tempdir("djinn-lsp-e2e-sym-depth-");
    let src = worktree.path().join("src/lib.txt");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(&src, "fn top() {}\n").unwrap();

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = lsp_tool(
        &state,
        serde_json::json!({
            "operation": "symbols",
            "file_path": "src/lib.txt",
            "depth": 0
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("no LSP server configured for"),
        "symbols with depth-only should reach LspManager, got: {err}"
    );
}

#[tokio::test]
async fn lsp_tool_boundary_symbols_with_kind_only() {
    let worktree = crate::test_helpers::test_tempdir("djinn-lsp-e2e-sym-kind-");
    let src = worktree.path().join("src/lib.txt");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(&src, "fn top() {}\n").unwrap();

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = lsp_tool(
        &state,
        serde_json::json!({
            "operation": "symbols",
            "file_path": "src/lib.txt",
            "kind": "function,struct"
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("no LSP server configured for"),
        "symbols with kind-only should reach LspManager, got: {err}"
    );
}

#[tokio::test]
async fn lsp_tool_boundary_symbols_with_name_filter_only() {
    let worktree = crate::test_helpers::test_tempdir("djinn-lsp-e2e-sym-name-");
    let src = worktree.path().join("src/lib.txt");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(&src, "fn top() {}\n").unwrap();

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = lsp_tool(
        &state,
        serde_json::json!({
            "operation": "symbols",
            "file_path": "src/lib.txt",
            "name_filter": "top"
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("no LSP server configured for"),
        "symbols with name_filter-only should reach LspManager, got: {err}"
    );
}

#[tokio::test]
async fn lsp_tool_boundary_symbols_bare_no_filters() {
    let worktree = crate::test_helpers::test_tempdir("djinn-lsp-e2e-sym-bare-");
    let src = worktree.path().join("src/lib.txt");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(&src, "fn bare() {}\n").unwrap();

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = lsp_tool(
        &state,
        serde_json::json!({
            "operation": "symbols",
            "file_path": "src/lib.txt"
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("no LSP server configured for"),
        "symbols with no filters should reach LspManager, got: {err}"
    );
}
