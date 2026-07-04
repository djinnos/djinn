// Tests for the CRUD/create concern in `proposal_tools/create.rs`.
//
// These tests are split out of `create.rs` so the production module stays under
// the size-guard threshold; behavior and expectations are unchanged.

#[cfg(test)]
mod list_summary_tests {
    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
    use djinn_core::events::EventBus;
    use djinn_db::{
        Database, ProjectRepository, ProposalCreateInput, ProposalDebateTrailCreateInput,
        ProposalRepository,
    };

    /// A well-formed body that passes all deterministic readiness checks.
    fn ready_body() -> &'static str {
        r#"
# Problem
Users cannot do X.

# Scope
In scope: Y. Out of scope: Z.

# Objectives
- Deliver A

## File map
```file-map
    src/main.rs
```

# Dependencies
Blocked by service C.

# Open Questions
What happens if D fails?
"#
    }

    async fn test_server() -> (DjinnMcpServer, Database) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        (DjinnMcpServer::new(test_mcp_state(db.clone())), db)
    }

    /// Pull the `list_summary` object for a given proposal id out of a
    /// `proposal_list` response.
    fn summary_for<'a>(
        list: &'a serde_json::Value,
        proposal_id: &str,
    ) -> Option<&'a serde_json::Value> {
        list.get("proposals")?
            .as_array()?
            .iter()
            .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(proposal_id))
            .and_then(|p| p.get("list_summary"))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_list_surfaces_tribunal_and_gate_summary() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = project_repo
            .create("svc-list-sum", "test", "svc-list-sum-repo")
            .await
            .unwrap();

        // Messy: empty body (fails DoR), no target, active refinement, one
        // blocking objection, and a judge needs-work verdict.
        let messy = repo
            .create(ProposalCreateInput {
                title: "Messy",
                body: "just some text",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        repo.record_refinement_lifecycle(&messy.id, "refinement_start", None)
            .await
            .unwrap();
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &messy.id,
            kind: "objection",
            body: "unbounded scope",
            blocking: true,
            agent_role: "adversary",
            author_kind: "agent",
            author_model: Some("m"),
            source_task_id: None,
            against_revision_seq: 1,
            round: 2,
            body_metadata: None,
        })
        .await
        .unwrap();
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &messy.id,
            kind: "verdict",
            body: "verdict: needs-work",
            blocking: true,
            agent_role: "judge",
            author_kind: "agent",
            author_model: Some("m"),
            source_task_id: None,
            against_revision_seq: 1,
            round: 2,
            body_metadata: None,
        })
        .await
        .unwrap();

        // Clean: DoR-passing body, a target, refinement converged awaiting
        // review, an approving verdict, no blocking objections.
        let clean = repo
            .create(ProposalCreateInput {
                title: "Clean",
                body: ready_body(),
                acceptance_criteria: Some(r#"[{"criterion":"API returns 200","met":false}]"#),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        repo.add_target(&clean.id, &project.id, "primary")
            .await
            .unwrap();
        repo.record_refinement_lifecycle(&clean.id, "refinement_start", None)
            .await
            .unwrap();
        repo.record_refinement_lifecycle(&clean.id, "refinement_awaiting_review", None)
            .await
            .unwrap();

        let list = server
            .dispatch_tool("proposal_list", serde_json::json!({ "limit": 50 }))
            .await
            .unwrap();
        assert!(
            list.get("error").is_none(),
            "proposal_list failed: {:?}",
            list.get("error")
        );

        let m = summary_for(&list, &messy.id).expect("messy has a list_summary");
        assert_eq!(m["refinement_active"], serde_json::json!(true));
        assert_eq!(m["awaiting_review"], serde_json::json!(false));
        assert_eq!(m["current_round"], serde_json::json!(2));
        assert_eq!(m["needs_evidence"], serde_json::json!(false));
        assert_eq!(m["dor_ready"], serde_json::json!(false));
        assert_eq!(m["gate_ready"], serde_json::json!(false));
        assert_eq!(
            m["unresolved_blocking_count"],
            serde_json::json!(1),
            "the judge verdict row must be excluded from the objection count"
        );

        let c = summary_for(&list, &clean.id).expect("clean has a list_summary");
        assert_eq!(c["refinement_active"], serde_json::json!(true));
        assert_eq!(c["awaiting_review"], serde_json::json!(true));
        assert_eq!(c["dor_ready"], serde_json::json!(true));
        assert_eq!(c["gate_ready"], serde_json::json!(true));
        assert_eq!(c["unresolved_blocking_count"], serde_json::json!(0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_list_omits_summary_for_terminal_proposals() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let done = repo
            .create(ProposalCreateInput {
                title: "Shipped",
                body: ready_body(),
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        repo.set_status(&done.id, "done").await.unwrap();

        let list = server
            .dispatch_tool("proposal_list", serde_json::json!({ "limit": 50 }))
            .await
            .unwrap();
        let entry = list
            .get("proposals")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(done.id.as_str()))
            })
            .expect("proposal present in list");
        assert!(
            entry.get("list_summary").is_none(),
            "terminal proposals must not carry a list_summary (chips hidden)"
        );
    }
}

// ── Body excerpt / include_bodies tests ───────────────────────────────────

#[cfg(test)]
mod body_excerpt_tests {
    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
    use djinn_core::events::EventBus;
    use djinn_db::{Database, ProposalCreateInput, ProposalRepository};

    async fn test_server() -> (DjinnMcpServer, Database) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        (DjinnMcpServer::new(test_mcp_state(db.clone())), db)
    }

    /// Excerpt helper: caps at exactly 512 Unicode scalars, no ellipsis.
    #[test]
    fn excerpt_caps_at_512_scalars() {
        use crate::tools::proposal_ops::body_excerpt;

        // Exactly 512 chars — not truncated.
        let body_512: String = "a".repeat(512);
        let (ex, truncated) = body_excerpt(&body_512);
        assert_eq!(ex.len(), 512);
        assert!(!truncated);
        assert_eq!(ex, body_512);

        // 513 chars — truncated to 512.
        let body_513: String = "b".repeat(513);
        let (ex, truncated) = body_excerpt(&body_513);
        assert_eq!(ex.chars().count(), 512);
        assert!(truncated);
        assert!(ex.starts_with(&"b".repeat(512)));

        // Empty body — not truncated.
        let (ex, truncated) = body_excerpt("");
        assert_eq!(ex, "");
        assert!(!truncated);
    }

    /// Excerpt caps on Unicode scalar values, not bytes or grapheme clusters.
    #[test]
    fn excerpt_respects_unicode_scalars() {
        use crate::tools::proposal_ops::body_excerpt;

        // Each emoji is 1 Unicode scalar but 4 UTF-8 bytes. 512 emojis = 512
        // scalars = 2048 bytes. Should NOT truncate.
        let emojis: String = "🦀".repeat(512);
        let (ex, truncated) = body_excerpt(&emojis);
        assert_eq!(ex.chars().count(), 512);
        assert!(!truncated);
        assert_eq!(ex, emojis);

        // 513 emojis → truncated at 512.
        let emojis_513: String = "🦀".repeat(513);
        let (ex, truncated) = body_excerpt(&emojis_513);
        assert_eq!(ex.chars().count(), 512);
        assert!(truncated);
    }

    /// Default `proposal_list` rows omit full body, include excerpt + truncated.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn default_list_omits_full_body() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let body = "This is a short test body for excerpt verification.";
        repo.create(ProposalCreateInput {
            title: "Excerpt Test",
            body,
            acceptance_criteria: None,
            status: None,
            body_format: None,
        })
        .await
        .unwrap();

        let list = server
            .dispatch_tool("proposal_list", serde_json::json!({ "limit": 10 }))
            .await
            .unwrap();
        assert!(list.get("error").is_none(), "list failed: {:?}", list.get("error"));

        let rows = list["proposals"].as_array().expect("proposals array");
        assert!(!rows.is_empty());
        let row = &rows[0];

        // Excerpt metadata present.
        assert_eq!(row["body_excerpt"].as_str().unwrap(), body);
        assert!(!row["body_truncated"].as_bool().unwrap());

        // Full body must NOT be serialized when include_bodies is omitted.
        assert!(
            row.get("body").is_none(),
            "full body must be absent on default list rows"
        );
    }

    /// `include_bodies: true` restores full body and retains excerpt metadata.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn include_bodies_true_restores_full_body() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let body = "Full body content that should be present when opted in.";
        repo.create(ProposalCreateInput {
            title: "Full Body Test",
            body,
            acceptance_criteria: None,
            status: None,
            body_format: None,
        })
        .await
        .unwrap();

        let list = server
            .dispatch_tool("proposal_list", serde_json::json!({ "limit": 10, "include_bodies": true }))
            .await
            .unwrap();
        assert!(list.get("error").is_none(), "list failed: {:?}", list.get("error"));

        let rows = list["proposals"].as_array().expect("proposals array");
        let row = &rows[0];

        // Full body present.
        assert_eq!(row["body"].as_str().unwrap(), body);
        // Excerpt metadata still present.
        assert_eq!(row["body_excerpt"].as_str().unwrap(), body);
        assert!(!row["body_truncated"].as_bool().unwrap());
    }

    /// `include_bodies: false` explicitly — same as omitted (no full body).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn include_bodies_false_explicit() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        repo.create(ProposalCreateInput {
            title: "Explicit False",
            body: "some body",
            acceptance_criteria: None,
            status: None,
            body_format: None,
        })
        .await
        .unwrap();

        let list = server
            .dispatch_tool("proposal_list", serde_json::json!({ "limit": 10, "include_bodies": false }))
            .await
            .unwrap();
        assert!(list.get("error").is_none());

        let rows = list["proposals"].as_array().expect("proposals array");
        assert!(
            rows[0].get("body").is_none(),
            "full body must be absent when include_bodies=false"
        );
    }

    /// Long body is properly truncated in excerpt but full body available.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn long_body_truncated_in_excerpt() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let long_body: String = "x".repeat(2000);
        repo.create(ProposalCreateInput {
            title: "Long Body",
            body: &long_body,
            acceptance_criteria: None,
            status: None,
            body_format: None,
        })
        .await
        .unwrap();

        // Default: excerpt truncated, no full body.
        let list = server
            .dispatch_tool("proposal_list", serde_json::json!({ "limit": 10 }))
            .await
            .unwrap();
        let row = &list["proposals"].as_array().unwrap()[0];
        assert_eq!(row["body_excerpt"].as_str().unwrap().chars().count(), 512);
        assert!(row["body_truncated"].as_bool().unwrap());
        assert!(row.get("body").is_none());

        // With include_bodies: true — full body available, excerpt still truncated.
        let list = server
            .dispatch_tool("proposal_list", serde_json::json!({ "limit": 10, "include_bodies": true }))
            .await
            .unwrap();
        let row = &list["proposals"].as_array().unwrap()[0];
        assert_eq!(row["body"].as_str().unwrap(), &long_body);
        assert_eq!(row["body_excerpt"].as_str().unwrap().chars().count(), 512);
        assert!(row["body_truncated"].as_bool().unwrap());
    }

    /// Pagination, filters, and list_summary still work with the new row model.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_preserves_pagination_and_summary() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());

        for i in 0..3 {
            repo.create(ProposalCreateInput {
                title: &format!("Proposal {i}"),
                body: &format!("Body content for proposal {i}"),
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        }

        // Pagination works.
        let list = server
            .dispatch_tool("proposal_list", serde_json::json!({ "limit": 2, "offset": 0 }))
            .await
            .unwrap();
        let meta = &list["meta"];
        assert_eq!(meta["limit"].as_i64().unwrap(), 2);
        assert_eq!(meta["total_count"].as_i64().unwrap(), 3);
        assert!(meta["has_more"].as_bool().unwrap());

        let rows = list["proposals"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        // Rows carry excerpt fields.
        assert!(rows[0].get("body_excerpt").is_some());
        assert!(rows[0].get("body_truncated").is_some());
    }
}

// ── Schema-lean regression tests ──────────────────────────────────────────
//
// Guard `ProposalCreateParams` and `ProposalUpdateParams` against accidental
// inlining of block vocabulary (tags, field schemas, catalog enums). Clients
// discover vocabulary via `get_block_catalog` / `proposal_blocks`, then
// submit proposal bodies through the existing `body` + `body_format` fields.

#[cfg(test)]
mod schema_lean_tests {
    use schemars::schema_for;
    use serde_json::Value;

    /// Recursively collect every string value reachable from `value`.
    fn collect_strings(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(s) => out.push(s.clone()),
            Value::Array(arr) => {
                for item in arr {
                    collect_strings(item, out);
                }
            }
            Value::Object(map) => {
                for v in map.values() {
                    collect_strings(v, out);
                }
            }
            _ => {}
        }
    }

    /// Assert that the serialized JSON schema does not mention any of the
    /// given forbidden terms.  A single traversal collects all string values
    /// (keys, enum entries, titles, descriptions, …) and a linear scan
    /// checks every one.
    fn assert_schema_excludes_terms(schema: &Value, forbidden: &[&str], context: &str) {
        let mut strings = Vec::new();
        collect_strings(schema, &mut strings);
        for term in forbidden {
            for s in &strings {
                assert!(
                    !s.contains(term),
                    "{context} schema unexpectedly contains forbidden term \
                     \"{term}\" in string value \"{s}\""
                );
            }
        }
    }

    /// Terms that must never appear in a proposal write-schema.  These
    /// cover: generic vocabulary field names, concrete MDX block tags, and
    /// block enum / field schema vocabulary.
    const FORBIDDEN_BLOCK_TERMS: &[&str] = &[
        // generic vocabulary surface
        "block_types",
        "catalog",
        "blocks",
        // concrete MDX block tags (must match proposal_block_catalog.json)
        "AnnotatedCode",
        "ApiEndpoint",
        "Callout",
        "Checklist",
        "Columns",
        "Decisions",
        "Diagram",
        "Diff",
        "FileTree",
        "JsonExplorer",
        "QuestionForm",
        "RichText",
        "Tabs",
        "Wireframe",
        // kebab-case type identifiers
        "annotated-code",
        "api-endpoint",
        "callout",
        "checklist",
        "columns",
        "decisions",
        "diagram",
        "diff",
        "file-tree",
        "json-explorer",
        "question-form",
        "rich-text",
        "tabs",
        "wireframe",
        // block enum / field schema vocabulary
        "BlockType",
        "ProposalBlock",
    ];

    /// Expected top-level properties for `ProposalCreateParams`.
    const CREATE_ALLOWED_PROPS: &[&str] = &[
        "title",
        "body",
        "acceptance_criteria",
        "target_projects",
        "status",
        "body_format",
    ];

    /// Expected top-level properties for `ProposalUpdateParams`.
    const UPDATE_ALLOWED_PROPS: &[&str] = &[
        "id",
        "title",
        "body",
        "acceptance_criteria",
        "status",
        "superseded_by",
        "body_format",
    ];

    #[test]
    fn proposal_create_params_schema_is_lean_and_excludes_block_vocabulary() {
        let schema = schema_for!(crate::tools::proposal_tools::ProposalCreateParams);
        let json: Value = serde_json::to_value(&schema).expect("schema serializes");

        // Verify allowed properties.
        let props = json["properties"]
            .as_object()
            .expect("ProposalCreateParams schema should have properties object");
        let prop_keys: Vec<&str> = props.keys().map(String::as_str).collect();
        assert_eq!(
            prop_keys, CREATE_ALLOWED_PROPS,
            "ProposalCreateParams properties drifted: got {prop_keys:?}, \
             expected {CREATE_ALLOWED_PROPS:?}"
        );

        assert_schema_excludes_terms(&json, FORBIDDEN_BLOCK_TERMS, "ProposalCreateParams");
    }

    #[test]
    fn proposal_update_params_schema_is_lean_and_excludes_block_vocabulary() {
        let schema = schema_for!(crate::tools::proposal_tools::ProposalUpdateParams);
        let json: Value = serde_json::to_value(&schema).expect("schema serializes");

        // Verify allowed properties.
        let props = json["properties"]
            .as_object()
            .expect("ProposalUpdateParams schema should have properties object");
        let prop_keys: Vec<&str> = props.keys().map(String::as_str).collect();
        assert_eq!(
            prop_keys, UPDATE_ALLOWED_PROPS,
            "ProposalUpdateParams properties drifted: got {prop_keys:?}, \
             expected {UPDATE_ALLOWED_PROPS:?}"
        );

        assert_schema_excludes_terms(&json, FORBIDDEN_BLOCK_TERMS, "ProposalUpdateParams");
    }
}
