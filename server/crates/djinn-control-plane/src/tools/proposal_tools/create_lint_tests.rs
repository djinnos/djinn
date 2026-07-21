use crate::server::DjinnMcpServer;
use crate::state::stubs::test_mcp_state;
use djinn_core::events::EventBus;
use djinn_db::{Database, ProposalRepository};
use serde_json::Value;

async fn test_server() -> (DjinnMcpServer, Database) {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    (DjinnMcpServer::new(test_mcp_state(db.clone())), db)
}

async fn lint_row_count(db: &Database, proposal_id: Option<&str>) -> i64 {
    let mut query = "SELECT COUNT(*) FROM proposal_revision_lint_results".to_string();
    if proposal_id.is_some() {
        query.push_str(" WHERE proposal_id = $1");
    }
    let mut statement = sqlx::query_scalar::<_, i64>(&query);
    if let Some(proposal_id) = proposal_id {
        statement = statement.bind(proposal_id);
    }
    statement.fetch_one(db.pool()).await.unwrap()
}

async fn revision_row_count(db: &Database) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM proposal_revisions")
        .fetch_one(db.pool())
        .await
        .unwrap()
}

async fn assert_response_has_exact_head_lint(
    repo: &ProposalRepository,
    response: &Value,
    expected_body: &str,
) -> String {
    assert!(
        response.get("error").is_none(),
        "mutation failed: {response:?}"
    );
    let id = response["id"].as_str().expect("proposal id").to_string();
    let seq = response["latest_revision_seq"]
        .as_i64()
        .expect("head sequence") as i32;
    let revisions = repo.revisions(&id).await.unwrap();
    let revision = revisions
        .iter()
        .find(|revision| revision.seq == seq)
        .expect("response head is a stored revision");
    assert_eq!(revision.body, expected_body, "committed head body");
    let expected_lint =
        serde_json::to_value(repo.lint_for_revision(revision).await.unwrap()).unwrap();
    assert_eq!(
        response["latest_lint"], expected_lint,
        "response must publish exact cached lint"
    );
    assert_eq!(
        response["latest_lint"]["body_sha256"],
        djinn_spec_lint::body_sha256(expected_body)
    );
    assert!(
        response["latest_lint"]["warnings"]
            .as_array()
            .is_some_and(|warnings| !warnings.is_empty()),
        "warning-only write must commit and return its warning result"
    );
    id
}

const WARNING_BODY: &str = "A [dangling local reference](#missing-anchor).";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warning_only_authoring_mutations_commit_and_return_exact_head_lint() {
    let (server, db) = test_server().await;
    let repo = ProposalRepository::new(db, EventBus::noop());

    let created = server
        .dispatch_tool(
            "proposal_create",
            serde_json::json!({
                "title": "Warning create", "body": WARNING_BODY,
            }),
        )
        .await
        .unwrap();
    let create_id = assert_response_has_exact_head_lint(&repo, &created, WARNING_BODY).await;

    let updated_body = "Updated [dangling local reference](#still-missing).";
    let updated = server
        .dispatch_tool(
            "proposal_update",
            serde_json::json!({
                "id": create_id, "body": updated_body,
            }),
        )
        .await
        .unwrap();
    let update_id = assert_response_has_exact_head_lint(&repo, &updated, updated_body).await;

    let imported_body = "Imported [dangling local reference](#absent).";
    let imported = server
        .dispatch_tool("proposal_import", serde_json::json!({
            "mdx": format!("---\ntitle: Warning import\nbody_format: markdown\n---\n{imported_body}"),
        }))
        .await
        .unwrap();
    let import_id = assert_response_has_exact_head_lint(&repo, &imported, imported_body).await;

    let imported_update_body = "Imported update [dangling local reference](#gone).";
    let imported_update = server
        .dispatch_tool("proposal_import", serde_json::json!({
            "mdx": format!("---\nid: {import_id}\ntitle: Warning import updated\nbody_format: markdown\n---\n{imported_update_body}"),
        }))
        .await
        .unwrap();
    assert_response_has_exact_head_lint(&repo, &imported_update, imported_update_body).await;

    let patch_source = "Patch this paragraph.";
    let patch_seed = server
        .dispatch_tool(
            "proposal_create",
            serde_json::json!({
                "title": "Warning patch", "body": patch_source,
            }),
        )
        .await
        .unwrap();
    let patch_id = patch_seed["id"].as_str().unwrap();
    let patched_body = "Patched [dangling local reference](#missing-after-patch).";
    let patched = server
        .dispatch_tool(
            "proposal_block_patch",
            serde_json::json!({
                "id": patch_id,
                "selector": { "exact_text": patch_source },
                "operation": "replace",
                "block_mdx": patched_body,
            }),
        )
        .await
        .unwrap();
    assert_response_has_exact_head_lint(&repo, &patched, patched_body).await;

    // Ensure the update result stayed on its original proposal rather than
    // accidentally taking the import-create branch.
    assert_eq!(updated["id"], update_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lint_errors_are_structured_and_rollback_every_update_family_write() {
    let (server, db) = test_server().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let rejected_body = concat!(
        "<Callout id=\"duplicate\">one</Callout>\n",
        "<Callout id=\"duplicate\">two</Callout>\n",
        "<Callout id=\"duplicate\">three</Callout>"
    );

    let before_create_revisions = revision_row_count(&db).await;
    let before_create_lints = lint_row_count(&db, None).await;
    let rejected_create = server
        .dispatch_tool(
            "proposal_create",
            serde_json::json!({
                "title": "Rejected create", "body": rejected_body, "body_format": "mdx",
            }),
        )
        .await
        .unwrap();
    assert_eq!(rejected_create["error"], "SPEC_LINT_REJECTED");
    assert_eq!(rejected_create["code"], "SPEC_LINT_REJECTED");
    let violations = rejected_create["violations"]
        .as_array()
        .expect("structured violations");
    assert_eq!(violations.len(), 2);
    for violation in violations {
        assert_eq!(violation["code"], "DUPLICATE_BLOCK_ID");
        assert_eq!(violation["severity"], "error");
        assert!(violation["message"].is_string());
        assert!(violation["span"]["start_byte"].is_u64());
        assert!(violation["span"]["end_byte"].is_u64());
    }
    assert!(violations.windows(2).all(|pair| {
        (
            pair[0]["span"]["start_byte"].as_u64(),
            pair[0]["span"]["end_byte"].as_u64(),
        ) <= (
            pair[1]["span"]["start_byte"].as_u64(),
            pair[1]["span"]["end_byte"].as_u64(),
        )
    }));
    assert_eq!(
        revision_row_count(&db).await,
        before_create_revisions,
        "rejected create leaves no revision"
    );
    assert_eq!(
        lint_row_count(&db, None).await,
        before_create_lints,
        "rejected create leaves no lint row"
    );

    let seed = server
        .dispatch_tool(
            "proposal_create",
            serde_json::json!({
                "title": "Rollback seed", "body": "Original paragraph.",
            }),
        )
        .await
        .unwrap();
    let id = seed["id"].as_str().unwrap().to_string();
    let before = repo.get(&id).await.unwrap().unwrap();
    let before_lints = lint_row_count(&db, Some(&id)).await;

    for response in [
        server.dispatch_tool("proposal_update", serde_json::json!({
            "id": id, "body": rejected_body, "body_format": "mdx",
        })).await.unwrap(),
        server.dispatch_tool("proposal_import", serde_json::json!({
            "mdx": format!("---\nid: {id}\ntitle: Rollback seed\nbody_format: mdx\n---\n{rejected_body}"),
        })).await.unwrap(),
        server.dispatch_tool("proposal_block_patch", serde_json::json!({
            "id": id,
            "selector": { "exact_text": "Original paragraph." },
            "operation": "replace",
            "block_mdx": rejected_body,
        })).await.unwrap(),
    ] {
        assert_eq!(response["error"], "SPEC_LINT_REJECTED", "{response:?}");
        assert_eq!(response["code"], "SPEC_LINT_REJECTED", "{response:?}");
        let after = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(after.latest_revision_seq, before.latest_revision_seq, "rejected mutation increments no sequence");
        assert_eq!(lint_row_count(&db, Some(&id)).await, before_lints, "rejected mutation leaves no lint row");
    }
}
