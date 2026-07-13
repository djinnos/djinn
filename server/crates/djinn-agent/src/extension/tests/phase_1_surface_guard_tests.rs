use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::test_helpers::{agent_context_from_db, create_test_db, test_services, test_tempdir};

/// Phase 1 regression guard: the five default model-facing tools (`edit`,
/// `apply_patch`, `read`, `write`, `shell`) must continue to be dispatched
/// through the agent-local fallback handlers (`extension::handlers`) rather than
/// rerouted to new implementations. The schema snapshot alone cannot catch a
/// dispatch reroute that keeps the same JSON schema, so this test exercises
/// the real two-phase dispatch (`djinn_mcp_extension::dispatch_tool_call`
/// returns `Unhandled`, then the agent fallback handles the tool) and asserts
/// the characteristic behavior of each handler.
#[tokio::test]
async fn phase_1_default_model_facing_tools_dispatch_through_agent_fallback() {
    let worktree = test_tempdir("phase1_surface_guard");
    let seed = worktree.path().join("seed.txt");
    tokio::fs::write(&seed, "hello world\n")
        .await
        .expect("seed file");

    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    let services = test_services();

    let as_args = |v: serde_json::Value| Some(v.as_object().cloned().unwrap());

    // 1. `read` routes to the workspace read handler.
    let result = call_tool(
        &state,
        &services,
        "read",
        as_args(json!({ "path": seed.to_str().unwrap() })),
        worktree.path(),
        None,
        Some("planner"),
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("read should be handled by the agent fallback");
    assert!(
        result["content"]
            .as_str()
            .expect("read content")
            .contains("hello world"),
        "read handler did not return the seeded file content"
    );

    // 2. `shell` routes to the workspace shell handler.
    let result = call_tool(
        &state,
        &services,
        "shell",
        as_args(json!({ "command": "echo phase1_surface_guard" })),
        worktree.path(),
        None,
        Some("planner"),
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("shell should be handled by the agent fallback");
    assert!(
        result["stdout"]
            .as_str()
            .expect("shell stdout")
            .contains("phase1_surface_guard"),
        "shell handler did not run the requested command"
    );

    // 3. `write` routes to the workspace write handler.
    let new_file = worktree.path().join("written.txt");
    let result = call_tool(
        &state,
        &services,
        "write",
        as_args(json!({
            "path": new_file.to_str().unwrap(),
            "content": "phase1 write body"
        })),
        worktree.path(),
        None,
        Some("planner"),
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("write should be handled by the agent fallback");
    assert_eq!(
        result["ok"].as_bool(),
        Some(true),
        "write handler did not succeed"
    );
    let written = tokio::fs::read_to_string(&new_file)
        .await
        .expect("written file");
    assert!(written.contains("phase1 write body"));

    // 4. `edit` routes to the workspace edit handler. It requires a read record
    //    in the same session, so re-read the seed file before editing.
    let _ = call_tool(
        &state,
        &services,
        "read",
        as_args(json!({ "path": seed.to_str().unwrap() })),
        worktree.path(),
        None,
        Some("planner"),
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("re-read before edit");
    let result = call_tool(
        &state,
        &services,
        "edit",
        as_args(json!({
            "path": seed.to_str().unwrap(),
            "old_text": "hello world",
            "new_text": "hello phase1"
        })),
        worktree.path(),
        None,
        Some("planner"),
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("edit should be handled by the agent fallback");
    assert_eq!(
        result["ok"].as_bool(),
        Some(true),
        "edit handler did not succeed"
    );

    // 5. `apply_patch` routes to the workspace apply_patch handler. Add a new
    //    file so no read-record is required.
    let patch_file = "patched.txt";
    let result = call_tool(
        &state,
        &services,
        "apply_patch",
        as_args(json!({
            "patch": format!(
                "*** Begin Patch\n*** Add File: {patch_file}\n+phase1 patched body\n*** End Patch"
            )
        })),
        worktree.path(),
        None,
        Some("planner"),
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("apply_patch should be handled by the agent fallback");
    assert_eq!(
        result["ok"].as_bool(),
        Some(true),
        "apply_patch handler did not succeed"
    );
    let files = result["files"].as_array().expect("patch files");
    assert!(
        files.iter().any(|f| {
            f["path"]
                .as_str()
                .map(|p| p.ends_with(patch_file))
                .unwrap_or(false)
        }),
        "apply_patch result did not list the expected file"
    );
    let patched = tokio::fs::read_to_string(worktree.path().join(patch_file))
        .await
        .expect("patched file");
    assert!(patched.contains("phase1 patched body"));
}
