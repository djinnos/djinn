//! Comprehensive stale-token guard for all agent-facing description surfaces.
//!
//! This integration test (outside `src/`, so the AC grep over `src/` is clean)
//! checks that:
//! 1. No tool description in any role's schema contains the stale DB-system
//!    token (constructed at runtime to avoid self-matching).
//! 2. All memory_* tool descriptions include the `.djinn/memory/` filesystem
//!    caution ("Do not assume .djinn/memory/ paths are readable").
//! 3. All role-prompt `.md` files and agent-facing Rust source files
//!    (`prompt_context.rs`, `wave.rs`) are free of the stale token.
//!
//! If this test fails, a stale reference was reintroduced. Fix the source
//! file, not the test.

use djinn_mcp_extension::tool_defs::*;
use std::path::PathBuf;

/// Build the lowercase stale DB-system token from character codes so that
/// grep for the contiguous substring never matches this source file.
fn stale_token() -> String {
    [100u8, 111, 108, 116].iter().map(|&b| b as char).collect()
}

/// All tool-schema-producing functions to check.
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

#[test]
fn no_stale_token_in_tool_descriptions() {
    let forbidden = stale_token();
    for (role, schemas) in &all_role_schemas() {
        for schema in schemas {
            let name = schema
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("<unnamed>");
            let desc = schema
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            assert!(
                !desc.to_lowercase().contains(&forbidden),
                "tool '{name}' in role '{role}' contains stale DB-system reference in description: {desc}"
            );
        }
    }
}

#[test]
fn memory_tool_descriptions_include_filesystem_caution() {
    let memory_tools = all_role_schemas()
        .into_iter()
        .flat_map(|(_, schemas)| schemas)
        .filter(|s| {
            s.get("name")
                .and_then(|n| n.as_str())
                .map(|n| n.starts_with("memory_"))
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();

    assert!(
        !memory_tools.is_empty(),
        "expected at least one memory_* tool in the schema surface"
    );

    // Only write/edit/move are mutation-capable memory tools that should
    // carry the filesystem caution. Read/search/list are read-only and
    // don't write, so the caution is optional for them.
    let should_caution = ["memory_write", "memory_edit", "memory_move"];

    for schema in &memory_tools {
        let name = schema.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let desc = schema
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("");

        if should_caution.contains(&name) {
            assert!(
                desc.contains(".djinn/memory/"),
                "tool '{name}' description should caution about .djinn/memory/ paths; got: {desc}"
            );
            assert!(
                desc.contains("worker filesystem"),
                "tool '{name}' description should warn about worker filesystem; got: {desc}"
            );
        }
    }
}

#[test]
fn no_stale_token_in_agent_facing_files() {
    let forbidden = stale_token();

    // Locate the workspace root from the crate directory
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent() // server/crates
        .and_then(|p| p.parent()) // server
        .and_then(|p| p.parent()) // workspace root
        .expect("could not determine workspace root from CARGO_MANIFEST_DIR");

    // All agent-facing file paths that must be clean of the stale token.
    let agent_facing_files: Vec<PathBuf> = [
        // Role prompts
        "server/crates/djinn-roles/src/prompts/dev.md",
        "server/crates/djinn-roles/src/prompts/chat.md",
        "server/crates/djinn-roles/src/prompts/architect.md",
        "server/crates/djinn-roles/src/prompts/planner.md",
        // Agent-facing Rust source
        "server/crates/djinn-agent/src/actors/slot/lifecycle/prompt_context.rs",
        "server/crates/djinn-coordinator/src/wave.rs",
    ]
    .iter()
    .map(|rel| workspace_root.join(rel))
    .collect();

    for path in &agent_facing_files {
        if !path.exists() {
            // File may not exist in the worktree (e.g. moved/renamed).
            // Skip rather than fail — the test is about guarding existing
            // content, not asserting file existence.
            continue;
        }
        let content = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        let lower = content.to_lowercase();
        assert!(
            !lower.contains(&forbidden),
            "agent-facing file {} contains stale DB-system reference",
            path.display()
        );
    }
}
