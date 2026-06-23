use super::*;

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
