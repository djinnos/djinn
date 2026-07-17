//! Negative guard for project-local Djinn teaching on agent-facing surfaces.
//!
//! The only accepted Djinn directories are server-home namespaces: a path
//! rooted at `$DJINN_HOME` or immediately rooted at `~`. Migration fixtures
//! and historical database audit records are intentionally *not* included in
//! this surface list; their paths are explicitly anchored in their own tests.
//! Fix the producer when this guard fails rather than weakening the guard.

use djinn_mcp_extension::tool_defs::*;
use std::path::{Path, PathBuf};

fn all_role_schemas() -> Vec<(&'static str, Vec<serde_json::Value>)> {
    vec![
        ("worker", tool_schemas_worker()),
        ("reviewer", tool_schemas_reviewer()),
        ("lead", tool_schemas_lead()),
        ("planner", tool_schemas_planner()),
        ("architect", tool_schemas_architect()),
        ("advocate", tool_schemas_advocate()),
        ("adversary", tool_schemas_adversary()),
        ("judge", tool_schemas_judge()),
    ]
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .expect("could not determine workspace root from CARGO_MANIFEST_DIR")
        .to_path_buf()
}

/// Detect a project-relative (or project-absolute) Djinn namespace. The
/// comparison is case-insensitive because prompts and JSON schemas are
/// consumed as text, not normalized filesystem paths.
fn contains_project_local_djinn_path(text: &str) -> bool {
    let lower = text.to_lowercase();
    let mut start = 0;
    while let Some(offset) = lower[start..].find(".djinn/") {
        let index = start + offset;
        let prefix = &lower[..index];
        if !prefix.ends_with("$djinn_home/") && !prefix.ends_with("~/") {
            return true;
        }
        start = index + ".djinn/".len();
    }
    false
}

fn assert_clean(label: &str, text: &str) {
    assert!(
        !contains_project_local_djinn_path(text),
        "{label} contains project-local Djinn path teaching"
    );
}

fn assert_clean_file(path: &Path) {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    assert_clean(&path.display().to_string(), &content);
}

fn assert_clean_tree(path: &Path) {
    for entry in
        std::fs::read_dir(path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
    {
        let entry = entry.expect("read directory entry");
        let entry_path = entry.path();
        if entry_path.is_dir() {
            assert_clean_tree(&entry_path);
        } else {
            assert_clean_file(&entry_path);
        }
    }
}

fn assert_clean_json(value: &serde_json::Value, label: &str) {
    match value {
        serde_json::Value::String(value) => assert_clean(label, value),
        serde_json::Value::Array(values) => {
            for value in values {
                assert_clean_json(value, label);
            }
        }
        serde_json::Value::Object(values) => {
            for (key, value) in values {
                assert_clean(key, key);
                assert_clean_json(value, label);
            }
        }
        _ => {}
    }
}

#[test]
fn no_project_local_djinn_path_in_rendered_tool_schemas() {
    for (role, schemas) in all_role_schemas() {
        for schema in schemas {
            // Recursion covers names, descriptions, and every input-schema
            // field rather than merely top-level tool descriptions.
            assert_clean_json(&schema, role);
        }
    }
}

#[test]
fn no_project_local_djinn_path_in_canonical_and_generated_surfaces() {
    let workspace_root = workspace_root();
    let files = [
        "server/crates/djinn-roles/src/prompts/dev.md",
        "server/crates/djinn-roles/src/prompts/chat.md",
        "server/crates/djinn-roles/src/prompts/architect.md",
        "server/crates/djinn-roles/src/prompts/planner.md",
        "server/crates/djinn-agent/src/actors/slot/lifecycle/prompt_context.rs",
        "server/crates/djinn-coordinator/src/wave.rs",
        "server/crates/djinn-agent/src/extension/handlers/workspace.rs",
        "server/crates/djinn-agent/src/native_skills.rs",
        "server/crates/djinn-agent/src/native_assets/visual-spec/SKILL.md",
        "server/crates/djinn-control-plane/src/tools/memory_tools/types.rs",
        "server/crates/djinn-control-plane/src/tools/memory_tools/writes.rs",
        "server/crates/djinn-mcp-extension/src/shared_schemas.rs",
        "server/src/server/chat/prompt/codebase_header.rs",
        "server/crates/djinn-mcp-extension/tests/fixtures/tool_surface_baseline.json",
        "server/crates/djinn-mcp-extension/src/tests/snapshots/djinn_mcp_extension__tests__schema_tests__worker_tool_schemas.snap",
        "server/src/server/tests/snapshots/djinn_server__server__tests__tool_schemas__mcp_tools_schema.snap",
    ];
    for file in files {
        assert_clean_file(&workspace_root.join(file));
    }

    // These are generated projections and role snapshots; recurse so newly
    // added roles or fixture files cannot bypass the same guard.
    for directory in [
        "server/crates/djinn-agent/src/extension/tests/snapshots",
        "server/crates/djinn-provider/tests/fixtures/tool_schema_projection/builtin",
    ] {
        assert_clean_tree(&workspace_root.join(directory));
    }
}

#[test]
fn server_home_djinn_namespaces_remain_valid() {
    assert!(!contains_project_local_djinn_path(
        "$DJINN_HOME/.djinn/server-state.sqlite"
    ));
    assert!(!contains_project_local_djinn_path(
        "~/.djinn/server-state.sqlite"
    ));
}

#[test]
fn relative_and_project_root_djinn_namespaces_are_rejected_case_insensitively() {
    for value in [
        ".DJINN/MEMORY/pitfalls/example.md",
        "project/.DjInN/decisions/proposed/example.md",
        "/workspace/project/.djinn/skills/example.md",
    ] {
        assert!(
            contains_project_local_djinn_path(value),
            "must reject {value}"
        );
    }
}
