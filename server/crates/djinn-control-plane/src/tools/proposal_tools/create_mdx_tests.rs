// MDX auto-upgrade and block-validation tests included by `create_tests.rs`.

    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
    use djinn_core::events::EventBus;
    use djinn_db::{Database, ProposalCreateInput, ProposalRepository};

    async fn test_server() -> (DjinnMcpServer, Database) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        (DjinnMcpServer::new(test_mcp_state(db.clone())), db)
    }

    /// A valid block body (children-form blocks + a trailing question-form).
    const VALID_BLOCK_BODY: &str = "# Proposal\n\n<Callout id=\"note\" tone=\"info\">\nImportant context.\n</Callout>\n\n<QuestionForm id=\"q\" title=\"Open Questions\">\nAny concerns?\n</QuestionForm>\n";

    /// create with omitted body_format + a body containing known block tags →
    /// stored body_format is upgraded to "mdx".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_omitted_format_with_blocks_stores_mdx() {
        let (server, _db) = test_server().await;
        let resp = server
            .dispatch_tool(
                "proposal_create",
                serde_json::json!({ "title": "Blocks", "body": VALID_BLOCK_BODY }),
            )
            .await
            .unwrap();
        assert!(
            resp.get("error").is_none(),
            "create should succeed: {:?}",
            resp.get("error")
        );
        assert_eq!(
            resp.get("body_format").and_then(|v| v.as_str()),
            Some("mdx"),
            "a markdown body carrying block tags must be stored as mdx"
        );
    }

    /// create with body_format="markdown" + an UNKNOWN block tag is rejected
    /// (this passed silently before the cutover).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_markdown_format_with_unknown_block_rejected() {
        let (server, _db) = test_server().await;
        let resp = server
            .dispatch_tool(
                "proposal_create",
                serde_json::json!({
                    "title": "Bad",
                    "body": "# P\n\n<FancyUnknown id=\"x\" />\n",
                    "body_format": "markdown",
                }),
            )
            .await
            .unwrap();
        let err = resp.get("error").and_then(|v| v.as_str()).expect("error");
        assert!(err.contains("FancyUnknown"), "error was: {err}");
    }

    /// The exact production failure: `<Decisions id="x" decisions={[…]} />` in a
    /// markdown body is rejected with an actionable error telling the author to
    /// write children markdown with `###` headings.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_self_closing_decisions_attr_form_rejected() {
        let (server, _db) = test_server().await;
        let body = "# P\n\n<Decisions id=\"choice\" decisions={[{\"decision\":\"JWT\"}]} />\n\n<QuestionForm id=\"q\" title=\"Q\">\nq?\n</QuestionForm>\n";
        let resp = server
            .dispatch_tool(
                "proposal_create",
                serde_json::json!({ "title": "Decisions attr form", "body": body }),
            )
            .await
            .unwrap();
        let err = resp.get("error").and_then(|v| v.as_str()).expect("error");
        assert!(err.contains("Decisions block"), "error was: {err}");
        assert!(err.contains("`choice`"), "error was: {err}");
        assert!(err.contains("###"), "error was: {err}");
    }

    /// Self-closing file-tree and checklist blocks are likewise rejected on the
    /// create path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_self_closing_children_blocks_rejected() {
        let (server, _db) = test_server().await;
        for (id, tag_body) in [
            ("layout", "<FileTree id=\"layout\" root=\"src\" />"),
            ("acc", "<Checklist id=\"acc\" />"),
        ] {
            let body = format!(
                "# P\n\n{tag_body}\n\n<QuestionForm id=\"q\" title=\"Q\">\nq?\n</QuestionForm>\n"
            );
            let resp = server
                .dispatch_tool(
                    "proposal_create",
                    serde_json::json!({ "title": "empty block", "body": body }),
                )
                .await
                .unwrap();
            let err = resp
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("expected error for id {id}"));
            assert!(err.contains(&format!("`{id}`")), "error was: {err}");
        }
    }

    /// A valid children-form Decisions block plus an annotated-code attribute
    /// form are accepted, and the proposal is stored as mdx.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_valid_children_and_attr_forms_accepted() {
        let (server, _db) = test_server().await;
        let body = "# P\n\n<Decisions id=\"d\">\n### Use JWT for stateless auth\nStatus: accepted\n\nWe scale horizontally.\n</Decisions>\n\n<AnnotatedCode id=\"code\" language=\"rust\" code={`fn main() {}`} />\n\n<QuestionForm id=\"q\" title=\"Q\">\nq?\n</QuestionForm>\n";
        let resp = server
            .dispatch_tool(
                "proposal_create",
                serde_json::json!({ "title": "Valid blocks", "body": body }),
            )
            .await
            .unwrap();
        assert!(
            resp.get("error").is_none(),
            "create should succeed: {:?}",
            resp.get("error")
        );
        assert_eq!(resp.get("body_format").and_then(|v| v.as_str()), Some("mdx"));
    }

    /// update path: a markdown proposal updated with a body carrying block tags
    /// is upgraded to mdx; an unknown block tag on update is rejected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_upgrades_and_validates_blocks() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let existing = repo
            .create(ProposalCreateInput {
                title: "Plain",
                body: "plain markdown",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: Some("markdown"),
            })
            .await
            .unwrap();

        // Upgrade: update the body with block tags → stored as mdx.
        let ok = server
            .dispatch_tool(
                "proposal_update",
                serde_json::json!({ "id": existing.id, "body": VALID_BLOCK_BODY }),
            )
            .await
            .unwrap();
        assert!(
            ok.get("error").is_none(),
            "update should succeed: {:?}",
            ok.get("error")
        );
        assert_eq!(ok.get("body_format").and_then(|v| v.as_str()), Some("mdx"));
        let stored = repo.get(&existing.id).await.unwrap().unwrap();
        assert_eq!(stored.body_format, "mdx");

        // Reject: update with an unknown block tag.
        let bad = server
            .dispatch_tool(
                "proposal_update",
                serde_json::json!({
                    "id": existing.id,
                    "body": "# P\n\n<TotallyUnknown id=\"z\" />\n",
                }),
            )
            .await
            .unwrap();
        let err = bad.get("error").and_then(|v| v.as_str()).expect("error");
        assert!(err.contains("TotallyUnknown"), "error was: {err}");
    }

    /// A QuestionForm that is not the last block is rejected on the create
    /// path with the established user-facing error string.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_rejects_question_form_not_last() {
        let (server, _db) = test_server().await;
        let body = "# P\n\n<QuestionForm id=\"q\" title=\"Q\">\nAny concerns?\n</QuestionForm>\n\n<Callout id=\"note\" tone=\"info\">\nTrailing content after the question form.\n</Callout>\n";
        let resp = server
            .dispatch_tool(
                "proposal_create",
                serde_json::json!({ "title": "QF not last", "body": body }),
            )
            .await
            .unwrap();
        let err = resp.get("error").and_then(|v| v.as_str()).expect("error");
        assert!(
            err.contains("question-form block must be the last block"),
            "error was: {err}"
        );
    }

    /// A QuestionForm that is not the last block is rejected on the update
    /// path as well.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_rejects_question_form_not_last() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let existing = repo
            .create(ProposalCreateInput {
                title: "Plain",
                body: "plain markdown",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: Some("markdown"),
            })
            .await
            .unwrap();

        let body = "# P\n\n<QuestionForm id=\"q\" title=\"Q\">\nAny concerns?\n</QuestionForm>\n\n<Callout id=\"note\" tone=\"info\">\nTrailing content after the question form.\n</Callout>\n";
        let resp = server
            .dispatch_tool(
                "proposal_update",
                serde_json::json!({ "id": existing.id, "body": body }),
            )
            .await
            .unwrap();
        let err = resp.get("error").and_then(|v| v.as_str()).expect("error");
        assert!(
            err.contains("question-form block must be the last block"),
            "error was: {err}"
        );
    }
