use super::*;

#[test]
fn floor_char_boundary_ascii() {
    assert_eq!(floor_char_boundary("hello", 3), 3);
}

#[test]
fn floor_char_boundary_multibyte_interior() {
    // '─' (U+2500) is 3 bytes: E2 94 80
    let s = "─";
    assert_eq!(floor_char_boundary(s, 1), 0);
    assert_eq!(floor_char_boundary(s, 2), 0);
    assert_eq!(floor_char_boundary(s, 3), 3);
}

#[test]
fn floor_char_boundary_emoji() {
    // '🔥' is 4 bytes
    let s = "🔥x";
    assert_eq!(floor_char_boundary(s, 1), 0);
    assert_eq!(floor_char_boundary(s, 2), 0);
    assert_eq!(floor_char_boundary(s, 3), 0);
    assert_eq!(floor_char_boundary(s, 4), 4);
    assert_eq!(floor_char_boundary(s, 5), 5);
}

#[test]
fn floor_char_boundary_beyond_len() {
    assert_eq!(floor_char_boundary("hi", 100), 2);
}

#[test]
fn floor_char_boundary_zero() {
    assert_eq!(floor_char_boundary("hello", 0), 0);
}

#[test]
fn tool_lsp_schema_exposes_symbol_filters() {
    let tool = tool_lsp();
    let schema = serde_json::to_value(&tool).unwrap();
    let input_schema = &schema["inputSchema"]["properties"];
    assert!(input_schema.get("symbol").is_some());
    assert!(input_schema.get("depth").is_some());
    assert!(input_schema.get("kind").is_some());
    assert!(input_schema.get("name_filter").is_some());
}

#[test]
fn validate_symbol_only_params_rejects_non_symbol_operations() {
    let params = LspParams {
        operation: "hover".to_string(),
        file_path: "src/lib.rs".to_string(),
        line: Some(1),
        character: Some(1),
        symbol: None,
        depth: Some(1),
        kind: Some("function".to_string()),
        name_filter: Some("foo".to_string()),
    };

    let error = validate_symbol_only_params("hover", &params).unwrap_err();
    assert!(error.contains("depth"));
    assert!(error.contains("kind"));
    assert!(error.contains("name_filter"));
}

#[test]
fn validate_symbol_only_params_allows_symbols_and_plain_hover() {
    let symbol_params = LspParams {
        operation: "symbols".to_string(),
        file_path: "src/lib.rs".to_string(),
        line: None,
        character: None,
        symbol: None,
        depth: Some(2),
        kind: Some("function".to_string()),
        name_filter: Some("foo".to_string()),
    };
    assert!(validate_symbol_only_params("symbols", &symbol_params).is_ok());

    let hover_params = LspParams {
        operation: "hover".to_string(),
        file_path: "src/lib.rs".to_string(),
        line: Some(1),
        character: Some(1),
        symbol: None,
        depth: None,
        kind: None,
        name_filter: None,
    };
    assert!(validate_symbol_only_params("hover", &hover_params).is_ok());
}

#[tokio::test]
async fn call_lsp_rejects_invalid_hover_target_combinations() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-lsp-hover-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let missing_both = call_lsp(
        &state,
        &Some(
            serde_json::json!({
                "operation": "hover",
                "file_path": "src/lib.rs"
            })
            .as_object()
            .expect("hover args object")
            .clone(),
        ),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        missing_both,
        "hover requires either symbol or line+character"
    );

    let incomplete_coords = call_lsp(
        &state,
        &Some(
            serde_json::json!({
                "operation": "hover",
                "file_path": "src/lib.rs",
                "line": 4
            })
            .as_object()
            .expect("hover args object")
            .clone(),
        ),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        incomplete_coords,
        "hover requires both line and character when symbol is omitted"
    );

    let mixed = call_lsp(
        &state,
        &Some(
            serde_json::json!({
                "operation": "hover",
                "file_path": "src/lib.rs",
                "line": 4,
                "character": 2,
                "symbol": "Thing/method"
            })
            .as_object()
            .expect("hover args object")
            .clone(),
        ),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        mixed,
        "hover accepts either symbol or line+character, but not both"
    );
}

#[tokio::test]
async fn call_lsp_rejects_invalid_definition_target_combinations() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-lsp-definition-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let missing_both = call_lsp(
        &state,
        &Some(
            serde_json::json!({
                "operation": "definition",
                "file_path": "src/lib.rs"
            })
            .as_object()
            .expect("definition args object")
            .clone(),
        ),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        missing_both,
        "definition requires either symbol or line+character"
    );

    let incomplete_coords = call_lsp(
        &state,
        &Some(
            serde_json::json!({
                "operation": "definition",
                "file_path": "src/lib.rs",
                "line": 4
            })
            .as_object()
            .expect("definition args object")
            .clone(),
        ),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        incomplete_coords,
        "definition requires both line and character when symbol is omitted"
    );

    let mixed = call_lsp(
        &state,
        &Some(
            serde_json::json!({
                "operation": "definition",
                "file_path": "src/lib.rs",
                "line": 4,
                "character": 2,
                "symbol": "Thing/method"
            })
            .as_object()
            .expect("definition args object")
            .clone(),
        ),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        mixed,
        "definition accepts either symbol or line+character, but not both"
    );
}

#[tokio::test]
async fn call_lsp_rejects_invalid_references_target_combinations() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-lsp-references-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let missing_both = call_lsp(
        &state,
        &Some(
            serde_json::json!({
                "operation": "references",
                "file_path": "src/lib.rs"
            })
            .as_object()
            .expect("references args object")
            .clone(),
        ),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        missing_both,
        "references requires either symbol or line+character"
    );

    let incomplete_coords = call_lsp(
        &state,
        &Some(
            serde_json::json!({
                "operation": "references",
                "file_path": "src/lib.rs",
                "line": 4
            })
            .as_object()
            .expect("references args object")
            .clone(),
        ),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        incomplete_coords,
        "references requires both line and character when symbol is omitted"
    );

    let mixed = call_lsp(
        &state,
        &Some(
            serde_json::json!({
                "operation": "references",
                "file_path": "src/lib.rs",
                "line": 4,
                "character": 2,
                "symbol": "Thing/method"
            })
            .as_object()
            .expect("references args object")
            .clone(),
        ),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        mixed,
        "references accepts either symbol or line+character, but not both"
    );
}

#[tokio::test]
async fn call_lsp_uses_coordinate_dispatch_for_hover_definition_and_references() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-lsp-coords-");
    let file_path = worktree.path().join("src/lib.txt");
    std::fs::create_dir_all(file_path.parent().expect("parent dir")).expect("create src dir");
    std::fs::write(&file_path, "pub fn sample() {}\n").expect("write file");

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let hover = call_lsp(
        &state,
        &Some(
            serde_json::json!({
                "operation": "hover",
                "file_path": "src/lib.txt",
                "line": 1,
                "character": 1
            })
            .as_object()
            .expect("hover args object")
            .clone(),
        ),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(hover.contains("no LSP server configured for"));
    assert!(hover.contains("src/lib.txt"));

    let definition = call_lsp(
        &state,
        &Some(
            serde_json::json!({
                "operation": "definition",
                "file_path": "src/lib.txt",
                "line": 1,
                "character": 1
            })
            .as_object()
            .expect("definition args object")
            .clone(),
        ),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(definition.contains("no LSP server configured for"));
    assert!(definition.contains("src/lib.txt"));

    let references = call_lsp(
        &state,
        &Some(
            serde_json::json!({
                "operation": "references",
                "file_path": "src/lib.txt",
                "line": 1,
                "character": 1
            })
            .as_object()
            .expect("references args object")
            .clone(),
        ),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(references.contains("no LSP server configured for"));
    assert!(references.contains("src/lib.txt"));
}

#[tokio::test]
async fn call_lsp_uses_symbol_dispatch_for_hover_definition_and_references() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-lsp-symbol-");
    let file_path = worktree.path().join("src/lib.txt");
    std::fs::create_dir_all(file_path.parent().expect("parent dir")).expect("create src dir");
    std::fs::write(&file_path, "pub fn sample() {}\n").expect("write file");

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let hover = call_lsp(
        &state,
        &Some(
            serde_json::json!({
                "operation": "hover",
                "file_path": "src/lib.txt",
                "symbol": "sample"
            })
            .as_object()
            .expect("hover args object")
            .clone(),
        ),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(hover.contains("no LSP server configured for"));
    assert!(hover.contains("src/lib.txt"));

    let definition = call_lsp(
        &state,
        &Some(
            serde_json::json!({
                "operation": "definition",
                "file_path": "src/lib.txt",
                "symbol": "sample"
            })
            .as_object()
            .expect("definition args object")
            .clone(),
        ),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(definition.contains("no LSP server configured for"));
    assert!(definition.contains("src/lib.txt"));

    let references = call_lsp(
        &state,
        &Some(
            serde_json::json!({
                "operation": "references",
                "file_path": "src/lib.txt",
                "symbol": "sample"
            })
            .as_object()
            .expect("references args object")
            .clone(),
        ),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(references.contains("no LSP server configured for"));
    assert!(references.contains("src/lib.txt"));
}
