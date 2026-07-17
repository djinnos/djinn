//! One negative guard contract for every agent-facing prompt/schema surface.
//!
//! Migration fixtures and historical database audit records are deliberately
//! outside this inventory and are covered by their own path-anchored tests.
//! Fix a producer rather than adding exclusions when this guard fails.

use std::path::{Path, PathBuf};

use djinn_core::events::EventBus;
use djinn_db::Database;
use djinn_mcp_extension::tool_defs::*;

use crate::AgentType;
use crate::prompts::{TaskContext, render_prompt_for_role};
use crate::roles::LeadRole;

use super::test_support::{assemble_for_role, create_project_epic_task};

const AGENT_TYPES: [AgentType; 8] = [
    AgentType::Worker,
    AgentType::Reviewer,
    AgentType::Lead,
    AgentType::Planner,
    AgentType::Architect,
    AgentType::Advocate,
    AgentType::Adversary,
    AgentType::Judge,
];

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("determine workspace root")
        .to_path_buf()
}

fn is_path_character(character: char) -> bool {
    character.is_ascii_alphanumeric()
        || matches!(character, '/' | '\\' | '.' | '_' | '-' | '$' | '~')
}

fn has_lexical_root_boundary(text: &str, root_start: usize) -> bool {
    text[..root_start]
        .chars()
        .next_back()
        .is_none_or(is_non_path_delimiter)
}

fn is_non_path_delimiter(character: char) -> bool {
    !is_path_character(character)
}

/// Detect project-relative/project-absolute `.djinn/` namespaces while
/// preserving only lexically rooted server-home path tokens.
fn contains_project_local_djinn_path(text: &str) -> bool {
    let lower = text.to_lowercase();
    let mut search_start = 0;
    while let Some(offset) = lower[search_start..].find(".djinn/") {
        let index = search_start + offset;
        let tilde_root = index
            .checked_sub(2)
            .filter(|&start| &lower[start..index] == "~/")
            .filter(|&start| has_lexical_root_boundary(&lower, start));
        let env_root = index
            .checked_sub("$djinn_home/".len())
            .filter(|&start| &lower[start..index] == "$djinn_home/")
            .filter(|&start| has_lexical_root_boundary(&lower, start));
        if tilde_root.is_none() && env_root.is_none() {
            return true;
        }
        search_start = index + ".djinn/".len();
    }
    false
}

fn assert_clean(label: &str, text: &str) {
    assert!(
        !contains_project_local_djinn_path(text),
        "{label} contains project-local Djinn path teaching"
    );
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
                assert_clean(label, key);
                assert_clean_json(value, label);
            }
        }
        _ => {}
    }
}

fn assert_clean_file(path: &Path) {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    assert_clean(&path.display().to_string(), &content);
}

fn assert_clean_tree(path: &Path) {
    for entry in
        std::fs::read_dir(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
    {
        let path = entry.expect("read directory entry").path();
        if path.is_dir() {
            assert_clean_tree(&path);
        } else {
            assert_clean_file(&path);
        }
    }
}

fn all_role_schemas() -> Vec<(&'static str, Vec<serde_json::Value>)> {
    vec![
        ("worker", tool_schemas_worker()),
        ("evidence_spike", tool_schemas_evidence_spike()),
        ("reviewer", tool_schemas_reviewer()),
        ("lead", tool_schemas_lead()),
        ("planner", tool_schemas_planner()),
        ("architect", tool_schemas_architect()),
        ("advocate", tool_schemas_advocate()),
        ("adversary", tool_schemas_adversary()),
        ("judge", tool_schemas_judge()),
    ]
}

#[tokio::test]
async fn runtime_and_serialized_surfaces_reject_project_local_djinn_paths() {
    // Namespace classifier regressions are part of this contract, so its
    // exception cannot silently broaden independently of the surface scan.
    for prose in [
        "Server state is stored at ~/.djinn/state.sqlite.",
        "Use `$DJINN_HOME/.djinn/cache/` for the server cache.",
        "Paths (~/.DjInN/state.sqlite) remain home-rooted.",
    ] {
        assert!(
            !contains_project_local_djinn_path(prose),
            "must allow {prose}"
        );
    }
    for path in [
        ".DJINN/MEMORY/example.md",
        "project/.DjInN/decisions/example.md",
        "/workspace/project/.djinn/skills/example.md",
        "project/~/.djinn/memory/example.md",
        "/workspace/~/.DjInN/memory/example.md",
        "project/$DJINN_HOME/.djinn/memory/example.md",
        "project/$DJINN_HOME/.DjInN/memory/example.md",
    ] {
        assert!(
            contains_project_local_djinn_path(path),
            "must reject {path}"
        );
    }

    crate::init_tool_schema_registry();
    let db = Database::ephemeral()
        .await
        .expect("create ephemeral database");
    let events = EventBus::noop();
    let task = create_project_epic_task(
        &db,
        &events,
        "Runtime guard epic title",
        "Runtime guard task title",
    )
    .await;
    let task_context = TaskContext {
        project_path: "/workspace/runtime-guard-project".into(),
        workspace_path: "/workspace/runtime-guard-worktree".into(),
        diff: None,
        commits: None,
        start_commit: None,
        end_commit: None,
        conflict_files: None,
        merge_base_branch: None,
        merge_target_branch: None,
        merge_failure_context: None,
        setup_commands: Some("cargo test -p focused-crate".into()),
        activity: Some("Representative task activity.".into()),
        worker_summary: Some("Representative worker summary.".into()),
        worker_concerns: None,
        epic_context: Some("Epic: Runtime guard epic title\nTask: Runtime guard task title".into()),
        knowledge_context: Some("Database-backed knowledge context.".into()),
        code_graph_context: None,
        reviewer_diff_context: None,
        ci_blocking_directive: None,
        worker_resume_note: None,
        arbiter_directive: None,
    };
    for agent_type in AGENT_TYPES {
        let rendered = render_prompt_for_role(agent_type.role_config(), &task, &task_context);
        assert_clean(agent_type.as_str(), &rendered);
    }

    // Exercise the production wrapper with a real persisted task+epic. Lead
    // requires epic context, proving both interpolation and final assembly are
    // inspected rather than treating template files as a runtime proxy.
    let assembled = assemble_for_role(db, &task, &LeadRole, None, "", &[], &[]).await;
    assert_clean("assembled base prompt", &assembled.base_system_prompt);
    assert_clean("assembled final prompt", &assembled.system_prompt);
    assert_clean(
        "assembled epic context",
        assembled
            .epic_context
            .as_deref()
            .expect("lead assembly populates epic context"),
    );

    // Recursive traversal covers MCP names, descriptions, and every nested
    // input-schema string/key for every registered agent role.
    for (role, schemas) in all_role_schemas() {
        for schema in schemas {
            assert_clean_json(&schema, role);
        }
    }

    let root = workspace_root();
    for file in [
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
        "server/src/server/tests/snapshots/djinn_server__server__tests__tool_schemas__mcp_tools_schema.snap",
        "ui/src/api/generated/mcp-tools.gen.ts",
    ] {
        assert_clean_file(&root.join(file));
    }
    for directory in [
        "server/crates/djinn-mcp-extension/src/tests/snapshots",
        "server/crates/djinn-mcp-extension/tests/fixtures",
        "server/crates/djinn-agent/src/extension/tests/snapshots",
        "server/crates/djinn-provider/tests/fixtures/tool_schema_projection/builtin",
    ] {
        assert_clean_tree(&root.join(directory));
    }
}
