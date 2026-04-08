use super::handlers::*;
use super::helpers::*;
use super::tool_defs::*;
use super::types::*;
use super::*;
use crate::AgentType;
use crate::test_helpers::create_test_db;
use crate::test_helpers::{
    agent_context_from_db, create_test_epic, create_test_project, create_test_task,
};
use djinn_core::events::EventBus;
use djinn_db::EpicRepository;
use djinn_db::NoteRepository;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

pub(crate) mod fuzzy_replace_tests {
    use super::super::fuzzy::{fuzzy_replace, reindent_replacement};
    use super::*;

    #[test]
    fn rebases_multiline_replacement_using_matched_indentation() {
        let content = "fn main() {\n    match value {\n        Some(x) => {\n            process(x);\n        }\n    }\n}\n";
        let old_text = "match value {\n    Some(x) => {\n        process(x);\n    }\n}";
        let new_text = "match value {\n    Some(x) => {\n        if ready {\n            process(x);\n        }\n    }\n}";

        let (updated, note) = fuzzy_replace(content, old_text, new_text, Path::new("test.rs"))
            .expect("fuzzy replace should succeed");

        assert_eq!(note.as_deref(), Some("(matched with flexible indentation)"));
        assert!(updated.contains(
            "    match value {\n        Some(x) => {\n            if ready {\n                process(x);\n            }\n        }\n    }"
        ));
    }

    #[test]
    fn preserves_later_nested_indent_when_first_replacement_line_is_less_indented() {
        let content = "impl Example {\n        if condition {\n            run();\n        }\n}\n";
        let old_text = "if condition {\n    run();\n}";
        let new_text =
            "if condition {\n    let nested = || {\n        run();\n    };\n    nested();\n}";

        let (updated, note) = fuzzy_replace(content, old_text, new_text, Path::new("test.rs"))
            .expect("fuzzy replace should succeed");

        assert_eq!(note.as_deref(), Some("(matched with flexible indentation)"));
        assert!(updated.contains(
            "        if condition {\n            let nested = || {\n                run();\n            };\n            nested();\n        }"
        ));
    }

    #[test]
    fn reindent_replacement_preserves_internal_relative_indentation() {
        let matched_block = "        if ready {\n            execute();\n        }";
        let replacement =
            "if ready {\n    let nested = || {\n        execute();\n    };\n    nested();\n}";

        assert_eq!(
            reindent_replacement(matched_block, replacement),
            "        if ready {\n            let nested = || {\n                execute();\n            };\n            nested();\n        }"
        );
    }
}

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

#[tokio::test]
async fn write_rejects_symlink_escape_outside_worktree() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-worktree-");
    let outside = crate::test_helpers::test_tempdir("djinn-ext-outside-");
    let link = worktree.path().join("escape-link");

    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), &link).expect("create symlink");
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(outside.path(), &link).expect("create symlink");

    let args = Some(
        serde_json::json!({"path":"escape-link/pwned.txt","content":"owned"})
            .as_object()
            .expect("obj")
            .clone(),
    );

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let result = call_write(&state, &args, worktree.path()).await;
    assert!(result.is_err());
    let err = result.err().unwrap_or_default();
    assert!(err.contains("outside worktree"));
    assert!(!outside.path().join("pwned.txt").exists());
}

#[tokio::test]
async fn call_tool_dispatches_task_create_with_public_response_shape() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let mut state = agent_context_from_db(db.clone(), CancellationToken::new());
    state.task_ops_project_path_override = Some(project.path.clone().into());

    let response = call_tool(
        &state,
        "task_create",
        Some(
            serde_json::json!({
                "epic_id": epic.short_id,
                "title": "Dispatch-created task",
                "description": "Created through extension dispatch",
                "design": "Keep the response shape stable",
                "priority": 3,
                "owner": "planner",
                "acceptance_criteria": ["first criterion"],
                "memory_refs": ["decisions/adr-041-unified-tool-service-layer-in-djinn-mcp"],
                "agent_type": "rust-expert"
            })
            .as_object()
            .expect("task_create args object")
            .clone(),
        ),
        Path::new(&project.path),
        None,
        Some("planner"),
        None,
    )
    .await
    .expect("task_create dispatch should succeed");

    assert_eq!(
        response.get("title").and_then(|v| v.as_str()),
        Some("Dispatch-created task")
    );
    assert_eq!(
        response.get("description").and_then(|v| v.as_str()),
        Some("Created through extension dispatch")
    );
    assert_eq!(response.get("priority").and_then(|v| v.as_i64()), Some(3));
    assert_eq!(
        response.get("owner").and_then(|v| v.as_str()),
        Some("planner")
    );
    assert_eq!(
        response.get("status").and_then(|v| v.as_str()),
        Some("open")
    );
    assert_eq!(response.get("agent_type").and_then(|v| v.as_str()), None);
    assert_eq!(
        response
            .get("acceptance_criteria")
            .and_then(|v| v.as_array())
            .and_then(|items| items.first())
            .and_then(|item| item
                .as_str()
                .or_else(|| item.get("criterion").and_then(|v| v.as_str()))),
        Some("first criterion")
    );
    assert_eq!(
        response
            .get("memory_refs")
            .and_then(|v| v.as_array())
            .and_then(|items| items.first())
            .and_then(|v| v.as_str()),
        Some("decisions/adr-041-unified-tool-service-layer-in-djinn-mcp")
    );
}

#[tokio::test]
async fn call_tool_dispatches_task_update_with_public_response_shape() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let mut state = agent_context_from_db(db.clone(), CancellationToken::new());
    state.task_ops_project_path_override = Some(project.path.clone().into());

    let response = call_tool(
        &state,
        "task_update",
        Some(
            serde_json::json!({
                "id": task.short_id,
                "title": "Dispatch-updated task",
                "description": "Updated through extension dispatch",
                "design": "Keep the update response shape stable",
                "priority": 2,
                "owner": "planner",
                "labels_add": ["migration-test"],
                "acceptance_criteria": [{"criterion": "updated criterion", "met": false}],
                "memory_refs_add": ["decisions/adr-041-unified-tool-service-layer-in-djinn-mcp"]
            })
            .as_object()
            .expect("task_update args object")
            .clone(),
        ),
        Path::new(&project.path),
        Some(&task.id),
        Some("planner"),
        None,
    )
    .await
    .expect("task_update dispatch should succeed");

    assert_eq!(
        response.get("id").and_then(|v| v.as_str()),
        Some(task.id.as_str())
    );
    assert_eq!(
        response.get("short_id").and_then(|v| v.as_str()),
        Some(task.short_id.as_str())
    );
    assert_eq!(
        response.get("title").and_then(|v| v.as_str()),
        Some("Dispatch-updated task")
    );
    assert_eq!(
        response.get("description").and_then(|v| v.as_str()),
        Some("Updated through extension dispatch")
    );
    assert_eq!(
        response.get("design").and_then(|v| v.as_str()),
        Some("Keep the update response shape stable")
    );
    assert_eq!(response.get("priority").and_then(|v| v.as_i64()), Some(2));
    assert_eq!(
        response.get("owner").and_then(|v| v.as_str()),
        Some("planner")
    );
    assert_eq!(
        response
            .get("labels")
            .and_then(|v| v.as_array())
            .map(|labels| labels
                .iter()
                .filter_map(|value| value.as_str())
                .collect::<Vec<_>>()),
        Some(vec!["migration-test"])
    );
    assert_eq!(
        response
            .get("acceptance_criteria")
            .and_then(|v| v.as_array())
            .and_then(|items| items.first())
            .and_then(|item| item
                .as_str()
                .or_else(|| item.get("criterion").and_then(|v| v.as_str()))),
        Some("updated criterion")
    );
    assert_eq!(
        response
            .get("memory_refs")
            .and_then(|v| v.as_array())
            .and_then(|items| items.first())
            .and_then(|v| v.as_str()),
        Some("decisions/adr-041-unified-tool-service-layer-in-djinn-mcp")
    );
}

#[tokio::test]
async fn call_tool_dispatches_comment_and_transition_flows() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let mut state = agent_context_from_db(db.clone(), CancellationToken::new());
    state.task_ops_project_path_override = Some(project.path.clone().into());

    let comment = call_tool(
        &state,
        "task_comment_add",
        Some(
            serde_json::json!({
                "id": task.short_id,
                "body": "Dispatch-level architect note"
            })
            .as_object()
            .expect("task_comment_add args object")
            .clone(),
        ),
        Path::new(&project.path),
        Some(&task.id),
        Some("architect"),
        None,
    )
    .await
    .expect("task_comment_add dispatch should succeed");

    assert_eq!(
        comment.get("task_id").and_then(|v| v.as_str()),
        Some(task.id.as_str())
    );
    assert_eq!(
        comment.get("actor_id").and_then(|v| v.as_str()),
        Some("architect")
    );
    assert_eq!(
        comment.get("actor_role").and_then(|v| v.as_str()),
        Some("architect")
    );
    assert_eq!(
        comment.get("event_type").and_then(|v| v.as_str()),
        Some("comment")
    );
    assert_eq!(
        comment
            .get("payload")
            .and_then(|v| v.get("body"))
            .and_then(|v| v.as_str()),
        Some("Dispatch-level architect note")
    );

    let transitioned = call_tool(
        &state,
        "task_transition",
        Some(
            serde_json::json!({
                "id": task.short_id,
                "action": "start"
            })
            .as_object()
            .expect("task_transition args object")
            .clone(),
        ),
        Path::new(&project.path),
        Some(&task.id),
        Some("lead"),
        None,
    )
    .await
    .expect("task_transition dispatch should succeed");

    assert_eq!(
        transitioned.get("id").and_then(|v| v.as_str()),
        Some(task.id.as_str())
    );
    assert_eq!(
        transitioned.get("short_id").and_then(|v| v.as_str()),
        Some(task.short_id.as_str())
    );
    assert_eq!(
        transitioned.get("status").and_then(|v| v.as_str()),
        Some("in_progress")
    );
    assert_eq!(
        transitioned.get("title").and_then(|v| v.as_str()),
        Some(task.title.as_str())
    );
}

#[tokio::test]
async fn call_tool_dispatches_agent_ops_through_shared_agent_seam() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let state = agent_context_from_db(db.clone(), CancellationToken::new());

    let create_response = call_tool(
        &state,
        "agent_create",
        Some(
            serde_json::json!({
                "project": project.path,
                "name": "Rust specialist",
                "base_role": "worker",
                "description": "Handles Rust-heavy tasks",
                "system_prompt_extensions": "Focus on Rust diagnostics",
                "model_preference": "gpt-5"
            })
            .as_object()
            .expect("agent_create args object")
            .clone(),
        ),
        Path::new(&project.path),
        None,
        Some("architect"),
        None,
    )
    .await
    .expect("agent_create dispatch should succeed");

    assert_eq!(
        create_response
            .get("agent_name")
            .and_then(|value| value.as_str()),
        Some("Rust specialist")
    );
    assert_eq!(
        create_response
            .get("base_role")
            .and_then(|value| value.as_str()),
        Some("worker")
    );
    assert_eq!(
        create_response
            .get("created")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    let created_agent_id = create_response
        .get("agent_id")
        .and_then(|value| value.as_str())
        .expect("agent id in create response")
        .to_string();

    let metrics_response = call_tool(
        &state,
        "agent_metrics",
        Some(
            serde_json::json!({
                "project": project.path,
                "agent_id": created_agent_id,
                "window_days": 14
            })
            .as_object()
            .expect("agent_metrics args object")
            .clone(),
        ),
        Path::new(&project.path),
        None,
        Some("architect"),
        None,
    )
    .await
    .expect("agent_metrics dispatch should succeed");

    assert_eq!(
        metrics_response
            .get("window_days")
            .and_then(|value| value.as_i64()),
        Some(14)
    );
    let roles = metrics_response
        .get("roles")
        .and_then(|value| value.as_array())
        .expect("roles array in metrics response");
    assert_eq!(roles.len(), 1);
    assert_eq!(
        roles[0].get("agent_name").and_then(|value| value.as_str()),
        Some("Rust specialist")
    );
    assert_eq!(
        roles[0].get("base_role").and_then(|value| value.as_str()),
        Some("worker")
    );
    assert!(roles[0].get("learned_prompt").is_some());
    let extraction_quality = roles[0]
        .get("extraction_quality")
        .and_then(|value| value.as_object())
        .expect("extraction_quality object");
    assert_eq!(
        extraction_quality
            .get("extracted")
            .and_then(|value| value.as_i64()),
        Some(0)
    );
}

#[tokio::test]
async fn call_tool_dispatches_memory_ops_through_shared_memory_seam() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let mut state = agent_context_from_db(db.clone(), CancellationToken::new());
    state.task_ops_project_path_override = Some(project.path.clone().into());

    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let seed = note_repo
        .create(
            &project.id,
            Path::new(&project.path),
            "Shared Memory Seed",
            "Architecture guidance with [[Shared Memory Related]] references.",
            "adr",
            "[]",
        )
        .await
        .expect("create seed note");
    note_repo
        .create(
            &project.id,
            Path::new(&project.path),
            "Shared Memory Related",
            "Related architecture context.",
            "reference",
            "[]",
        )
        .await
        .expect("create related note");

    let search_response = call_tool(
        &state,
        "memory_search",
        Some(
            serde_json::json!({
                "project": project.path,
                "query": "architecture",
                "limit": 5
            })
            .as_object()
            .expect("memory_search args object")
            .clone(),
        ),
        Path::new(&project.path),
        Some(&task.id),
        Some("architect"),
        None,
    )
    .await
    .expect("memory_search dispatch should succeed");
    assert!(
        search_response.get("error").is_none()
            || search_response
                .get("error")
                .is_some_and(|value| value.is_null())
    );
    assert!(
        search_response
            .get("results")
            .and_then(|value| value.as_array())
            .is_some_and(|results| !results.is_empty())
    );

    let read_response = call_tool(
        &state,
        "memory_read",
        Some(
            serde_json::json!({
                "project": project.path,
                "identifier": seed.permalink
            })
            .as_object()
            .expect("memory_read args object")
            .clone(),
        ),
        Path::new(&project.path),
        Some(&task.id),
        Some("architect"),
        None,
    )
    .await
    .expect("memory_read dispatch should succeed");
    assert!(
        read_response.get("error").is_none()
            || read_response
                .get("error")
                .is_some_and(|value| value.is_null())
    );
    assert_eq!(
        read_response
            .get("permalink")
            .and_then(|value| value.as_str()),
        Some(seed.permalink.as_str())
    );

    let list_response = call_tool(
        &state,
        "memory_list",
        Some(
            serde_json::json!({
                "project": project.path,
                "folder": "decisions",
                "depth": 1
            })
            .as_object()
            .expect("memory_list args object")
            .clone(),
        ),
        Path::new(&project.path),
        Some(&task.id),
        Some("architect"),
        None,
    )
    .await
    .expect("memory_list dispatch should succeed");
    assert!(
        list_response.get("error").is_none()
            || list_response
                .get("error")
                .is_some_and(|value| value.is_null())
    );
    assert!(
        list_response
            .get("notes")
            .and_then(|value| value.as_array())
            .is_some_and(|notes| !notes.is_empty())
    );

    let context_response = call_tool(
        &state,
        "memory_build_context",
        Some(
            serde_json::json!({
                "project": project.path,
                "url": format!("memory://{}", seed.permalink),
                "budget": 512,
                "max_related": 5
            })
            .as_object()
            .expect("memory_build_context args object")
            .clone(),
        ),
        Path::new(&project.path),
        Some(&task.id),
        Some("architect"),
        None,
    )
    .await
    .expect("memory_build_context dispatch should succeed");
    assert!(
        context_response.get("error").is_none()
            || context_response
                .get("error")
                .is_some_and(|value| value.is_null())
    );
    assert_eq!(
        context_response
            .get("primary")
            .and_then(|value| value.as_array())
            .map(|items| items.len()),
        Some(1)
    );
}

#[tokio::test]
async fn call_tool_dispatches_registered_mcp_tool_success() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let state = agent_context_from_db(db, CancellationToken::new());
    let registry = crate::mcp_client::McpToolRegistry::with_dispatch(
        [("web_search".to_string(), "search-server".to_string())],
        vec![serde_json::json!({"name": "web_search"})],
        move |tool_name, arguments| {
            assert_eq!(tool_name, "web_search");
            assert_eq!(
                arguments.as_ref().and_then(|args| args.get("query")),
                Some(&serde_json::json!("djinn"))
            );
            Ok(serde_json::json!({
                "items": [{"title": "Djinn", "url": "https://example.com/djinn"}]
            }))
        },
    );

    let response = call_tool(
        &state,
        "web_search",
        Some(
            serde_json::json!({
                "query": "djinn"
            })
            .as_object()
            .expect("mcp args object")
            .clone(),
        ),
        Path::new(&project.path),
        None,
        Some("worker"),
        Some(&registry),
    )
    .await
    .expect("registered MCP tool should dispatch");

    assert_eq!(
        response
            .get("items")
            .and_then(|value| value.as_array())
            .and_then(|items| items.first())
            .and_then(|item| item.get("title"))
            .and_then(|value| value.as_str()),
        Some("Djinn")
    );
}

#[tokio::test]
async fn call_tool_dispatches_registered_mcp_tool_error() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let state = agent_context_from_db(db, CancellationToken::new());
    let registry = crate::mcp_client::McpToolRegistry::with_dispatch(
        [("web_fetch".to_string(), "fetch-server".to_string())],
        vec![serde_json::json!({"name": "web_fetch"})],
        move |tool_name, arguments| {
            assert_eq!(tool_name, "web_fetch");
            assert_eq!(
                arguments.as_ref().and_then(|args| args.get("url")),
                Some(&serde_json::json!("https://example.com/fail"))
            );
            Err("upstream MCP error".to_string())
        },
    );

    let error = call_tool(
        &state,
        "web_fetch",
        Some(
            serde_json::json!({
                "url": "https://example.com/fail"
            })
            .as_object()
            .expect("mcp args object")
            .clone(),
        ),
        Path::new(&project.path),
        None,
        Some("worker"),
        Some(&registry),
    )
    .await
    .expect_err("MCP errors should flow through the normal tool error path");

    assert!(error.contains("upstream MCP error"));
}

#[test]
fn worker_cannot_use_lead_only_tool() {
    // submit_decision is lead-only (ADR-036: finalize tools are role-specific).
    assert!(!is_tool_allowed_for_agent(
        AgentType::Worker,
        "submit_decision"
    ));
    assert!(is_tool_allowed_for_agent(
        AgentType::Lead,
        "submit_decision"
    ));
    // task_transition is not in the lead tool set (removed by ADR-036).
    assert!(!is_tool_allowed_for_agent(
        AgentType::Lead,
        "task_transition"
    ));
}

#[test]
fn shell_timeout_defaults_and_minimum() {
    fn resolve_timeout(t: Option<u64>) -> u64 {
        t.unwrap_or(120_000).max(1000)
    }
    assert_eq!(resolve_timeout(None), 120_000);
    assert_eq!(resolve_timeout(Some(0)), 1000);
}

#[test]
fn tool_schemas_include_role_specific_tools() {
    fn schema_names(schemas: Vec<serde_json::Value>) -> Vec<String> {
        schemas
            .into_iter()
            .filter_map(|v| {
                v.get("name")
                    .and_then(|n| n.as_str())
                    .map(ToString::to_string)
            })
            .collect()
    }

    let worker = schema_names(tool_schemas_worker());
    assert!(worker.iter().any(|n| n == "shell"));
    assert!(worker.iter().any(|n| n == "write"));
    assert!(worker.iter().any(|n| n == "edit"));
    assert!(worker.iter().any(|n| n == "memory_write"));
    assert!(worker.iter().any(|n| n == "memory_edit"));
    assert!(worker.iter().any(|n| n == "submit_work"));
    assert!(!worker.iter().any(|n| n == "task_comment_add"));

    let reviewer = schema_names(tool_schemas_reviewer());
    assert!(reviewer.iter().any(|n| n == "submit_review"));
    assert!(!reviewer.iter().any(|n| n == "task_update_ac"));
    assert!(!reviewer.iter().any(|n| n == "task_comment_add"));

    let lead = schema_names(tool_schemas_lead());
    assert!(lead.iter().any(|n| n == "task_create"));
    assert!(lead.iter().any(|n| n == "submit_decision"));
    assert!(!lead.iter().any(|n| n == "task_transition"));
    assert!(!lead.iter().any(|n| n == "task_comment_add"));

    let planner = schema_names(tool_schemas_planner());
    assert!(planner.iter().any(|n| n == "task_create"));
    assert!(planner.iter().any(|n| n == "task_transition"));
    assert!(planner.iter().any(|n| n == "submit_grooming"));
    assert!(planner.iter().any(|n| n == "memory_write"));
    assert!(planner.iter().any(|n| n == "memory_edit"));
    // Per ADR-051 §1 the Planner now runs patrol mode, which needs to leave
    // diagnostic comments on stuck tasks and mutate learned_prompts for
    // specialist agents during the effectiveness review.
    assert!(planner.iter().any(|n| n == "task_comment_add"));
    assert!(planner.iter().any(|n| n == "memory_health"));
    assert!(planner.iter().any(|n| n == "memory_broken_links"));
    assert!(planner.iter().any(|n| n == "memory_orphans"));
    assert!(planner.iter().any(|n| n == "memory_build_context"));
    assert!(planner.iter().any(|n| n == "agent_metrics"));
    assert!(planner.iter().any(|n| n == "agent_create"));
    assert!(planner.iter().any(|n| n == "agent_amend_prompt"));

    let architect = schema_names(tool_schemas_architect());
    assert!(architect.iter().any(|n| n == "shell"));
    assert!(architect.iter().any(|n| n == "read"));
    assert!(architect.iter().any(|n| n == "task_create"));
    assert!(architect.iter().any(|n| n == "task_comment_add"));
    assert!(architect.iter().any(|n| n == "task_transition"));
    assert!(architect.iter().any(|n| n == "task_kill_session"));
    assert!(architect.iter().any(|n| n == "memory_write"));
    assert!(architect.iter().any(|n| n == "memory_edit"));
    assert!(architect.iter().any(|n| n == "submit_work"));
    // Architect must NOT have code-writing tools.
    assert!(!architect.iter().any(|n| n == "write"));
    assert!(!architect.iter().any(|n| n == "edit"));
    assert!(!architect.iter().any(|n| n == "apply_patch"));
    // Per ADR-051 §1 `agent_amend_prompt` moved from Architect to Planner
    // (agent-effectiveness review is a patrol action, not a consultant action).
    assert!(!architect.iter().any(|n| n == "agent_amend_prompt"));
}

#[test]
fn ensure_path_within_worktree_accepts_in_tree_and_rejects_traversal() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-worktree-");
    let nested = worktree.path().join("nested");
    std::fs::create_dir_all(&nested).expect("create nested");
    let in_tree = nested.join("file.txt");
    ensure_path_within_worktree(&in_tree, worktree.path()).expect("in-tree path should pass");

    let traversal = worktree.path().join("..").join("..").join("escape.txt");
    let err = ensure_path_within_worktree(&traversal, worktree.path())
        .expect_err("traversal should be rejected");
    assert!(err.contains("outside worktree"));
}

#[test]
fn ensure_path_within_worktree_rejects_symlink_escape() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-worktree-");
    let outside = crate::test_helpers::test_tempdir("djinn-ext-outside-");
    let link = worktree.path().join("escape-link");

    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), &link).expect("create symlink");
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(outside.path(), &link).expect("create symlink");

    let escaped = link.join("leak.txt");
    let err = ensure_path_within_worktree(&escaped, worktree.path())
        .expect_err("symlink escape should be rejected");
    assert!(err.contains("outside worktree"));
}

#[test]
fn is_tool_allowed_for_schemas_handles_empty_and_invalid_entries() {
    assert!(!is_tool_allowed_for_schemas(&[], "shell"));

    let schemas = vec![
        serde_json::json!({}),
        serde_json::json!({"name": null}),
        serde_json::json!({"name": 42}),
        serde_json::json!({"name": "shell"}),
    ];
    assert!(is_tool_allowed_for_schemas(&schemas, "shell"));
    assert!(!is_tool_allowed_for_schemas(&schemas, "read"));
}

#[test]
fn resolve_path_handles_relative_absolute_and_normalization() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-resolve-");
    let base = worktree.path();

    let relative = resolve_path("src/main.rs", base);
    assert_eq!(relative, base.join("src/main.rs"));

    let absolute = resolve_path("/etc/hosts", base);
    assert_eq!(absolute, PathBuf::from("/etc/hosts"));

    let normalized = resolve_path("./src/../Cargo.toml", base);
    assert_eq!(normalized, base.join("Cargo.toml"));
}

fn tool_names(schemas: &[serde_json::Value]) -> Vec<&str> {
    schemas
        .iter()
        .filter_map(|v| v.get("name").and_then(|n| n.as_str()))
        .collect()
}

fn tool_schema<'a>(schemas: &'a [serde_json::Value], name: &str) -> &'a serde_json::Value {
    schemas
        .iter()
        .find(|schema| schema.get("name").and_then(|n| n.as_str()) == Some(name))
        .expect("tool schema present")
}

#[test]
fn tool_schemas_include_concurrency_metadata() {
    let worker = tool_schemas_worker();
    assert_eq!(tool_schema(&worker, "task_show")["concurrent_safe"], true);
    assert_eq!(tool_schema(&worker, "read")["concurrent_safe"], true);
    assert_eq!(
        tool_schema(&worker, "github_search")["concurrent_safe"],
        true
    );
    assert_eq!(tool_schema(&worker, "shell")["concurrent_safe"], false);
    assert_eq!(tool_schema(&worker, "write")["concurrent_safe"], false);

    let architect = tool_schemas_architect();
    assert_eq!(
        tool_schema(&architect, "code_graph")["concurrent_safe"],
        true
    );
    assert_eq!(
        tool_schema(&architect, "memory_build_context")["concurrent_safe"],
        true
    );
    assert_eq!(
        tool_schema(&architect, "task_comment_add")["concurrent_safe"],
        false
    );
}

#[test]
fn snapshot_worker_tool_names() {
    let schemas = tool_schemas_worker();
    let names = tool_names(&schemas);
    insta::assert_json_snapshot!("worker_tool_names", names);
}

#[test]
fn snapshot_worker_tool_schemas() {
    insta::assert_json_snapshot!("worker_tool_schemas", tool_schemas_worker());
}

#[test]
fn snapshot_reviewer_tool_names() {
    let schemas = tool_schemas_reviewer();
    let names = tool_names(&schemas);
    insta::assert_json_snapshot!("reviewer_tool_names", names);
}

#[test]
fn snapshot_reviewer_tool_schemas() {
    insta::assert_json_snapshot!("reviewer_tool_schemas", tool_schemas_reviewer());
}

#[test]
fn snapshot_lead_tool_names() {
    let schemas = tool_schemas_lead();
    let names = tool_names(&schemas);
    insta::assert_json_snapshot!("lead_tool_names", names);
}

#[test]
fn snapshot_lead_tool_schemas() {
    insta::assert_json_snapshot!("lead_tool_schemas", tool_schemas_lead());
}

#[test]
fn snapshot_planner_tool_names() {
    let schemas = tool_schemas_planner();
    let names = tool_names(&schemas);
    insta::assert_json_snapshot!("planner_tool_names", names);
}

#[test]
fn snapshot_planner_tool_schemas() {
    insta::assert_json_snapshot!("planner_tool_schemas", tool_schemas_planner());
}

#[test]
fn snapshot_architect_tool_names() {
    let schemas = tool_schemas_architect();
    let names = tool_names(&schemas);
    insta::assert_json_snapshot!("architect_tool_names", names);
}

#[test]
fn snapshot_architect_tool_schemas() {
    insta::assert_json_snapshot!("architect_tool_schemas", tool_schemas_architect());
}

#[test]
fn snapshot_lsp_tool_schema() {
    insta::assert_json_snapshot!("lsp_tool_schema", serde_json::to_value(tool_lsp()).unwrap());
}

#[test]
fn snapshot_code_graph_tool_schema() {
    insta::assert_json_snapshot!(
        "code_graph_tool_schema",
        serde_json::to_value(tool_code_graph()).unwrap()
    );
}

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

#[tokio::test]
async fn epic_extension_handlers_match_shared_epic_ops_behavior() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic_repo = EpicRepository::new(db.clone(), EventBus::noop());
    let epic = epic_repo
        .update(
            &create_test_epic(&db, &project.id).await.id,
            djinn_db::EpicUpdateInput {
                title: "test-epic",
                description: "test epic description",
                emoji: "🧪",
                color: "#0000ff",
                owner: "test-owner",
                memory_refs: Some("[]"),
                status: None,
                auto_breakdown: None,
                originating_adr_id: None,
            },
        )
        .await
        .expect("normalize test epic color");
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let state = agent_context_from_db(db, CancellationToken::new());

    let show_args = Some(
        serde_json::json!({
            "project": project.path,
            "id": epic.short_id,
        })
        .as_object()
        .expect("show args object")
        .clone(),
    );
    let show_value = call_epic_show(&state, &show_args, None)
        .await
        .expect("epic_show succeeds");
    assert_eq!(show_value["id"], epic.id);
    assert_eq!(show_value["task_count"], serde_json::json!(1));
    assert!(show_value.get("error").is_none());

    let update_args = Some(
        serde_json::json!({
            "project": project.path,
            "id": epic.short_id,
            "title": "updated epic title",
            "description": "updated epic description",
            "status": "open",
            "memory_refs_add": ["notes/adr-041"],
        })
        .as_object()
        .expect("update args object")
        .clone(),
    );
    let update_value = call_epic_update(&state, &update_args, None)
        .await
        .expect("epic_update succeeds");
    let epic_model: djinn_mcp::tools::epic_ops::EpicSingleResponse =
        serde_json::from_value(update_value.clone()).expect("parse epic update response");
    let epic_model = epic_model.epic.expect("updated epic payload");
    assert_eq!(epic_model.title, "updated epic title");
    assert_eq!(epic_model.description, "updated epic description");
    assert_eq!(epic_model.memory_refs, vec!["notes/adr-041".to_string()]);
    assert!(update_value.get("error").is_none());

    let tasks_args = Some(
        serde_json::json!({
            "project": project.path,
            "id": epic.short_id,
            "limit": 10,
            "offset": 0,
        })
        .as_object()
        .expect("tasks args object")
        .clone(),
    );
    let tasks_value = call_epic_tasks(&state, &tasks_args, None)
        .await
        .expect("epic_tasks succeeds");
    assert_eq!(tasks_value["total"], serde_json::json!(1));
    assert_eq!(tasks_value["limit"], serde_json::json!(10));
    assert_eq!(tasks_value["offset"], serde_json::json!(0));
    assert_eq!(tasks_value["has_more"], serde_json::json!(false));
    assert_eq!(tasks_value["tasks"][0]["id"], task.id);
    assert!(tasks_value.get("total_count").is_none());
    assert!(tasks_value.get("error").is_none());
}
