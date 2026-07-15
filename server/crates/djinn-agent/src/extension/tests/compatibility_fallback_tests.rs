//! Regression coverage for compatibility normalization at the agent-local fallback boundary.

use djinn_core::tool_call::{
    CompatibilityCode, ToolCallFailure, ToolCallOutcome, TrustedRemedyCode,
};
use djinn_mcp_extension::compatibility::{
    AtomicDeletionBundle, CompatibilityTrap, ReleaseNoteOwner, ReleaseNoteRef, RenamedToolTrap,
    ServerReleaseVersion, ToolForwardingSafety, TrapLifecycle,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::test_helpers::{agent_context_from_db, create_test_db, test_services, test_tempdir};

fn current_release() -> ServerReleaseVersion {
    ServerReleaseVersion {
        major: 0,
        minor: 1,
        patch: 0,
    }
}

fn safe_local_read_alias() -> CompatibilityTrap {
    CompatibilityTrap::RenamedTool(RenamedToolTrap {
        old_name: "compat_read",
        replacement_tool: "read",
        semantic_safety: ToolForwardingSafety::Exact,
        lifecycle: TrapLifecycle {
            id: "agent-local-read-alias",
            introduced_in: current_release(),
            remove_after: ServerReleaseVersion {
                major: 0,
                minor: 3,
                patch: 0,
            },
            release_note: ReleaseNoteRef {
                owner: ReleaseNoteOwner::McpApi,
                reference: "agent-local-read-alias",
            },
            deletion: AtomicDeletionBundle {
                change_id: "remove-agent-local-read-alias",
                trap_id: "agent-local-read-alias",
                fixture_case_ids: &["agent-local-read-alias"],
                release_note_reference: "agent-local-read-alias",
            },
        },
        remedy: TrustedRemedyCode::CallReplacementTool,
    })
}

#[tokio::test]
async fn safe_hidden_alias_reaches_local_handler_and_unauthorized_replacement_is_rejected() {
    let worktree = test_tempdir("compatibility-local-fallback-");
    let seed = worktree.path().join("seed.txt");
    tokio::fs::write(&seed, "normalized local fallback\n")
        .await
        .expect("seed file");
    let state = agent_context_from_db(create_test_db(), CancellationToken::new());
    let services = test_services();
    let registry = [safe_local_read_alias()];
    let release = current_release();
    let args = Some(
        json!({ "path": seed })
            .as_object()
            .cloned()
            .expect("arguments"),
    );

    // The hidden alias normalizes once to local `read` and retains its warning.
    let outcome = call_tool_with_compatibility(
        &state,
        &services,
        "compat_read",
        args.clone(),
        worktree.path(),
        None,
        Some("planner"),
        None,
        None,
        &ToolCancellation::never(),
        &registry,
        &release,
    )
    .await;
    match outcome {
        ToolCallOutcome::Success { value, warnings } => {
            assert!(
                value["content"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("normalized local fallback")
            );
            assert_eq!(warnings.len(), 1);
            assert_eq!(warnings[0].code, CompatibilityCode::DeprecatedSurface);
            assert_eq!(warnings[0].old_name, "compat_read");
            assert_eq!(warnings[0].tool, "read");
        }
        other => panic!("safe alias must reach the current local handler: {other:?}"),
    }

    // Authorization uses the normalized replacement, not the hidden alias.
    let stale_only_schema = vec![json!({ "name": "compat_read" })];
    let outcome = call_tool_with_compatibility(
        &state,
        &services,
        "compat_read",
        args,
        worktree.path(),
        None,
        Some("planner"),
        None,
        Some(&stale_only_schema),
        &ToolCancellation::never(),
        &registry,
        &release,
    )
    .await;
    match outcome {
        ToolCallOutcome::Failure(ToolCallFailure::Message(message)) => {
            assert_eq!(message, "tool `read` is not in the allowed schema list");
        }
        other => panic!("unauthorized replacement must not reach local handler: {other:?}"),
    }
}
