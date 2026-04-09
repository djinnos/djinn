use super::*;

// -----------------------------------------------------------------------
// Phase 2 NamePath wiring: end-to-end `lsp` tests through call_tool
// -----------------------------------------------------------------------

/// Helper to invoke the `lsp` tool through the public `call_tool` boundary.
async fn lsp_tool(
    state: &AgentContext,
    args: serde_json::Value,
    worktree: &Path,
) -> Result<serde_json::Value, String> {
    call_tool(
        state,
        "lsp",
        args.as_object()
            .expect("lsp args must be an object")
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
async fn lsp_tool_boundary_routes_hover_with_symbol() {
    let worktree = crate::test_helpers::test_tempdir("djinn-lsp-e2e-hover-sym-");
    let src = worktree.path().join("src/lib.txt");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(&src, "fn example() {}\n").unwrap();

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    // Symbol-based hover through the full dispatch boundary.
    let err = lsp_tool(
        &state,
        serde_json::json!({
            "operation": "hover",
            "file_path": "src/lib.txt",
            "symbol": "example"
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    // Reaches LspManager and fails because .txt has no LSP server.
    assert!(
        err.contains("no LSP server configured for"),
        "expected LSP routing error, got: {err}"
    );
}

#[tokio::test]
async fn lsp_tool_boundary_routes_definition_with_symbol() {
    let worktree = crate::test_helpers::test_tempdir("djinn-lsp-e2e-def-sym-");
    let src = worktree.path().join("src/lib.txt");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(&src, "fn example() {}\n").unwrap();

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = lsp_tool(
        &state,
        serde_json::json!({
            "operation": "definition",
            "file_path": "src/lib.txt",
            "symbol": "example"
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("no LSP server configured for"),
        "expected LSP routing error, got: {err}"
    );
}

#[tokio::test]
async fn lsp_tool_boundary_routes_references_with_symbol() {
    let worktree = crate::test_helpers::test_tempdir("djinn-lsp-e2e-ref-sym-");
    let src = worktree.path().join("src/lib.txt");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(&src, "fn example() {}\n").unwrap();

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = lsp_tool(
        &state,
        serde_json::json!({
            "operation": "references",
            "file_path": "src/lib.txt",
            "symbol": "example"
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("no LSP server configured for"),
        "expected LSP routing error, got: {err}"
    );
}

#[tokio::test]
async fn lsp_tool_boundary_preserves_legacy_coordinate_hover() {
    let worktree = crate::test_helpers::test_tempdir("djinn-lsp-e2e-coord-hover-");
    let src = worktree.path().join("src/lib.txt");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(&src, "fn legacy() {}\n").unwrap();

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    // Legacy line+character (no symbol) through dispatch boundary.
    let err = lsp_tool(
        &state,
        serde_json::json!({
            "operation": "hover",
            "file_path": "src/lib.txt",
            "line": 1,
            "character": 4
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("no LSP server configured for"),
        "legacy coordinate hover should reach LspManager, got: {err}"
    );
}

#[tokio::test]
async fn lsp_tool_boundary_preserves_legacy_coordinate_definition() {
    let worktree = crate::test_helpers::test_tempdir("djinn-lsp-e2e-coord-def-");
    let src = worktree.path().join("src/lib.txt");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(&src, "fn legacy() {}\n").unwrap();

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = lsp_tool(
        &state,
        serde_json::json!({
            "operation": "definition",
            "file_path": "src/lib.txt",
            "line": 1,
            "character": 4
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("no LSP server configured for"),
        "legacy coordinate definition should reach LspManager, got: {err}"
    );
}

#[tokio::test]
async fn lsp_tool_boundary_preserves_legacy_coordinate_references() {
    let worktree = crate::test_helpers::test_tempdir("djinn-lsp-e2e-coord-ref-");
    let src = worktree.path().join("src/lib.txt");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(&src, "fn legacy() {}\n").unwrap();

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = lsp_tool(
        &state,
        serde_json::json!({
            "operation": "references",
            "file_path": "src/lib.txt",
            "line": 1,
            "character": 4
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("no LSP server configured for"),
        "legacy coordinate references should reach LspManager, got: {err}"
    );
}

#[tokio::test]
async fn lsp_tool_boundary_rejects_mixed_symbol_and_coords() {
    let worktree = crate::test_helpers::test_tempdir("djinn-lsp-e2e-mixed-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    for operation in &["hover", "definition", "references"] {
        let err = lsp_tool(
            &state,
            serde_json::json!({
                "operation": operation,
                "file_path": "src/lib.rs",
                "symbol": "Foo/bar",
                "line": 10,
                "character": 5
            }),
            worktree.path(),
        )
        .await
        .unwrap_err();

        assert!(
            err.contains("but not both"),
            "{operation}: expected mutual-exclusion error, got: {err}"
        );
    }
}

#[tokio::test]
async fn lsp_tool_boundary_rejects_missing_target() {
    let worktree = crate::test_helpers::test_tempdir("djinn-lsp-e2e-missing-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    for operation in &["hover", "definition", "references"] {
        let err = lsp_tool(
            &state,
            serde_json::json!({
                "operation": operation,
                "file_path": "src/lib.rs"
            }),
            worktree.path(),
        )
        .await
        .unwrap_err();

        assert!(
            err.contains("requires either symbol or line+character"),
            "{operation}: expected requires-target error, got: {err}"
        );
    }
}

#[tokio::test]
async fn lsp_tool_boundary_rejects_incomplete_coords() {
    let worktree = crate::test_helpers::test_tempdir("djinn-lsp-e2e-incomplete-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    for operation in &["hover", "definition", "references"] {
        // line without character
        let err = lsp_tool(
            &state,
            serde_json::json!({
                "operation": operation,
                "file_path": "src/lib.rs",
                "line": 5
            }),
            worktree.path(),
        )
        .await
        .unwrap_err();

        assert!(
            err.contains("requires both line and character when symbol is omitted"),
            "{operation} (line only): expected incomplete-coords error, got: {err}"
        );

        // character without line
        let err = lsp_tool(
            &state,
            serde_json::json!({
                "operation": operation,
                "file_path": "src/lib.rs",
                "character": 3
            }),
            worktree.path(),
        )
        .await
        .unwrap_err();

        assert!(
            err.contains("requires both line and character when symbol is omitted"),
            "{operation} (character only): expected incomplete-coords error, got: {err}"
        );
    }
}

#[tokio::test]
async fn lsp_tool_boundary_rejects_symbol_only_params_on_non_symbols_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-lsp-e2e-symonly-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    for operation in &["hover", "definition", "references"] {
        let err = lsp_tool(
            &state,
            serde_json::json!({
                "operation": operation,
                "file_path": "src/lib.rs",
                "line": 1,
                "character": 1,
                "depth": 2,
                "kind": "function"
            }),
            worktree.path(),
        )
        .await
        .unwrap_err();

        assert!(
            err.contains("only supported for operation='symbols'"),
            "{operation}: expected symbols-only param error, got: {err}"
        );
    }
}

#[tokio::test]
async fn lsp_tool_boundary_symbols_operation_coexists_with_symbol_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-lsp-e2e-symbols-coexist-");
    let src = worktree.path().join("src/lib.txt");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(&src, "fn coexist() {}\n").unwrap();

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    // `symbols` operation with depth/kind/name_filter should reach LspManager
    // (and fail because .txt has no LSP server).
    let err = lsp_tool(
        &state,
        serde_json::json!({
            "operation": "symbols",
            "file_path": "src/lib.txt",
            "depth": 1,
            "kind": "function",
            "name_filter": "coexist"
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("no LSP server configured for"),
        "symbols operation should reach LspManager, got: {err}"
    );

    // Meanwhile, symbol-based hover on same file should also reach LspManager
    // (different code path: symbol dispatch vs symbols operation).
    let err = lsp_tool(
        &state,
        serde_json::json!({
            "operation": "hover",
            "file_path": "src/lib.txt",
            "symbol": "coexist"
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("no LSP server configured for"),
        "symbol-based hover should coexist with symbols operation, got: {err}"
    );
}

#[tokio::test]
async fn lsp_tool_boundary_unknown_operation_rejected() {
    let worktree = crate::test_helpers::test_tempdir("djinn-lsp-e2e-unknown-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = lsp_tool(
        &state,
        serde_json::json!({
            "operation": "rename",
            "file_path": "src/lib.rs",
            "line": 1,
            "character": 1
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("unknown LSP operation: rename"),
        "expected unknown-operation error, got: {err}"
    );
}
