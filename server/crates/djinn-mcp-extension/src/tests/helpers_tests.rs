//! Tests for shared helper functions.
//!
//! Moved from `djinn-agent::extension::tests` during the Phase 4 extraction
//! — these test `crate::helpers` directly.

use crate::helpers::*;
use crate::types::LspParams;

// ── ensure_path_within_worktree ──────────────────────────────────────────

fn test_tempdir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("create tempdir")
}

#[test]
fn ensure_path_within_worktree_accepts_in_tree_and_rejects_traversal() {
    let worktree = test_tempdir("djinn-ext-worktree-");
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
    let worktree = test_tempdir("djinn-ext-worktree-");
    let outside = test_tempdir("djinn-ext-outside-");
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

// ── is_tool_allowed_for_schemas ──────────────────────────────────────────

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

// ── resolve_path ─────────────────────────────────────────────────────────

#[test]
fn resolve_path_handles_relative_absolute_and_normalization() {
    let worktree = test_tempdir("djinn-ext-resolve-");
    let base = worktree.path();

    let relative = resolve_path("src/main.rs", base);
    assert_eq!(relative, base.join("src/main.rs"));

    let absolute = resolve_path("/etc/hosts", base);
    assert_eq!(absolute, std::path::PathBuf::from("/etc/hosts"));

    let normalized = resolve_path("./src/../Cargo.toml", base);
    assert_eq!(normalized, base.join("Cargo.toml"));
}

// ── validate_symbol_only_params ──────────────────────────────────────────

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
