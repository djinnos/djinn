//! In-pod agent dispatch tests for proposal authoring tools.
//!
//! The Advocate runs in a worker pod and dispatches through
//! `djinn_mcp_extension`. `proposal_update` / `proposal_block_patch` were once
//! only wired in the server-side control-plane dispatch, so the Advocate's body
//! revisions failed with "unknown djinn frontend tool" and the tribunal could
//! never converge. These tests drive the full agent `call_tool` path and assert
//! the body is actually revised.

use super::*;

#[tokio::test]
async fn call_tool_dispatches_proposal_update_revises_body() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let project_path = crate::extension::tests::project_fs_path(&project)
        .to_string_lossy()
        .into_owned();
    let proposal_repo = djinn_db::ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = proposal_repo
        .create(djinn_db::ProposalCreateInput {
            title: "advocate revises me",
            body: "## Thesis\n\nThin body without DoR sections.",
            acceptance_criteria: Some("[]"),
            status: Some("building"),
            body_format: None,
        })
        .await
        .expect("create proposal");
    let state = agent_context_from_db(db.clone(), CancellationToken::new());

    let new_body = "## Problem\n\nThe gap.\n\n## Scope\n\nIn and out.\n\n## Objectives\n\nMeasurable outcomes.";
    let response = call_tool(
        &state,
        &crate::test_helpers::test_services(),
        "proposal_update",
        Some(
            serde_json::json!({ "id": proposal.short_id, "body": new_body })
                .as_object()
                .expect("proposal_update args object")
                .clone(),
        ),
        Path::new(&project_path),
        None,
        Some("advocate"),
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("proposal_update dispatch should succeed (was 'unknown djinn frontend tool')");

    assert_eq!(response.get("ok").and_then(|v| v.as_bool()), Some(true));
    let reloaded = proposal_repo
        .resolve(&proposal.id)
        .await
        .expect("resolve")
        .expect("proposal exists");
    assert_eq!(
        reloaded.body, new_body,
        "advocate body revision must persist"
    );
    assert!(
        reloaded.latest_revision_seq > proposal.latest_revision_seq,
        "a material body revision must bump latest_revision_seq"
    );
}

/// Companion for the optional progressive-enrichment path: the Advocate's
/// `proposal_block_patch` must also Handle in-pod instead of erroring.
#[tokio::test]
async fn call_tool_dispatches_proposal_block_patch_in_pod() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let project_path = crate::extension::tests::project_fs_path(&project)
        .to_string_lossy()
        .into_owned();
    let proposal_repo = djinn_db::ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = proposal_repo
        .create(djinn_db::ProposalCreateInput {
            title: "enrich me",
            body: "## Design\n\nReplace this paragraph with a block.",
            acceptance_criteria: Some("[]"),
            status: Some("building"),
            body_format: None,
        })
        .await
        .expect("create proposal");
    let state = agent_context_from_db(db.clone(), CancellationToken::new());

    let response = call_tool(
        &state,
        &crate::test_helpers::test_services(),
        "proposal_block_patch",
        Some(
            serde_json::json!({
                "id": proposal.short_id,
                "selector": { "exact_text": "Replace this paragraph with a block." },
                "operation": "replace",
                "block_mdx": "Grounded replacement text.",
            })
            .as_object()
            .expect("proposal_block_patch args object")
            .clone(),
        ),
        Path::new(&project_path),
        None,
        Some("advocate"),
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("proposal_block_patch dispatch should succeed (was 'unknown djinn frontend tool')");

    assert_eq!(response.get("ok").and_then(|v| v.as_bool()), Some(true));
    let reloaded = proposal_repo
        .resolve(&proposal.id)
        .await
        .expect("resolve")
        .expect("proposal exists");
    assert!(
        reloaded.body.contains("Grounded replacement text."),
        "block patch must land in the body"
    );
}

/// The Advocate discovers the MDX block vocabulary via `get_block_catalog`,
/// which must Handle in-pod (it failed with "unknown djinn frontend tool").
#[tokio::test]
async fn call_tool_dispatches_get_block_catalog_in_pod() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let project_path = crate::extension::tests::project_fs_path(&project)
        .to_string_lossy()
        .into_owned();
    let state = agent_context_from_db(db.clone(), CancellationToken::new());

    let response = call_tool(
        &state,
        &crate::test_helpers::test_services(),
        "get_block_catalog",
        Some(serde_json::Map::new()),
        Path::new(&project_path),
        None,
        Some("advocate"),
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("get_block_catalog dispatch should succeed (was 'unknown djinn frontend tool')");

    assert!(
        response
            .get("blocks")
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty()),
        "catalog must return a non-empty block vocabulary"
    );
}
