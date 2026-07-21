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

    /// Multibyte Unicode scalars are not clipped at byte boundaries.
    #[test]
    fn excerpt_preserves_multibyte_scalars() {
        use crate::tools::proposal_ops::body_excerpt;

        // 511 emojis + 1 trailing scalar = 512 total; should not truncate.
        let prefix: String = "🦀".repeat(511);
        let body = format!("{prefix}α");
        assert_eq!(body.chars().count(), 512);
        let (ex, truncated) = body_excerpt(&body);
        assert_eq!(ex.chars().count(), 512);
        assert_eq!(ex, body);
        assert!(!truncated);

        // 512 emojis + 1 trailing scalar = 513; should truncate at 512.
        let prefix: String = "🦀".repeat(512);
        let body = format!("{prefix}α");
        assert_eq!(body.chars().count(), 513);
        let (ex, truncated) = body_excerpt(&body);
        assert_eq!(ex.chars().count(), 512);
        assert!(truncated);
        // The last scalar must be the emoji (4 bytes), not a split fragment.
        assert!(ex.ends_with("🦀"));
    }

    /// Exact boundary at 512 scalars: not truncated; 513 is truncated.
    #[test]
    fn excerpt_boundary_exact_512_vs_513() {
        use crate::tools::proposal_ops::body_excerpt;

        let body_512 = "★".repeat(512);
        let (ex, truncated) = body_excerpt(&body_512);
        assert_eq!(ex, body_512);
        assert!(!truncated);

        let body_513 = "★".repeat(513);
        let (ex, truncated) = body_excerpt(&body_513);
        assert_eq!(ex, "★".repeat(512));
        assert!(truncated);
    }

    /// Default `proposal_list` rows are bounded summaries: omit body, excerpt
    /// metadata, and criteria; always carry `ac_total`/`ac_met`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn default_list_omits_full_body_and_excerpt_metadata() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let body = "This is a short test body for excerpt verification.";
        repo.create(ProposalCreateInput {
            title: "Excerpt Test",
            body,
            acceptance_criteria: Some(r#"[{"criterion":"one","met":true},{"criterion":"two"}]"#),
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

        // Default rows omit body, excerpt metadata, and criteria.
        assert!(row.get("body").is_none(), "body must be absent on default rows");
        assert!(row.get("body_excerpt").is_none(), "body_excerpt must be absent");
        assert!(row.get("body_truncated").is_none(), "body_truncated must be absent");
        assert!(row.get("acceptance_criteria").is_none(), "criteria must be absent");
        // Always-present integer counts.
        assert_eq!(row["ac_total"].as_i64(), Some(2));
        assert_eq!(row["ac_met"].as_i64(), Some(1));
    }

    /// `include_bodies: true` restores full body and implies excerpt metadata.
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
        // Excerpt metadata implied by include_bodies.
        assert_eq!(row["body_excerpt"].as_str().unwrap(), body);
        assert!(!row["body_truncated"].as_bool().unwrap());
    }

    /// `include_bodies: false` explicitly — same as omitted (no full body or excerpt).
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
        assert!(
            rows[0].get("body_excerpt").is_none(),
            "excerpt metadata must be absent when include_bodies=false"
        );
    }

    /// Long body is properly truncated in excerpt (when requested) and full body
    /// is available via `include_bodies`.
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

        // Default: no excerpt, no body.
        let list = server
            .dispatch_tool("proposal_list", serde_json::json!({ "limit": 10 }))
            .await
            .unwrap();
        let row = &list["proposals"].as_array().unwrap()[0];
        assert!(row.get("body_excerpt").is_none());
        assert!(row.get("body").is_none());

        // With include_excerpts: true — excerpt present, truncated, no full body.
        let list = server
            .dispatch_tool("proposal_list", serde_json::json!({ "limit": 10, "include_excerpts": true }))
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

    /// Pagination, filters, and counts work with the new lean row model.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_preserves_pagination_and_counts() {
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

        // Pagination works. Metadata fields are serialized at the top level
        // (not nested under a "meta" key) per `serialize_named_list_response`.
        let list = server
            .dispatch_tool("proposal_list", serde_json::json!({ "limit": 2, "offset": 0 }))
            .await
            .unwrap();
        assert_eq!(list["limit"].as_i64().unwrap(), 2);
        assert_eq!(list["total_count"].as_i64().unwrap(), 3);
        assert!(list["has_more"].as_bool().unwrap());

        let rows = list["proposals"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        // Rows carry counts.
        assert!(rows[0].get("ac_total").is_some());
        assert!(rows[0].get("ac_met").is_some());
    }

    // ── Flag truth table and tolerant count tests ──────────────────────────────

    /// Exercise all eight include_bodies/include_excerpts/include_acceptance_criteria
    /// combinations and assert the correct optional fields appear/are omitted in
    /// each mode, while counts are always present.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_flag_truth_table_all_eight_combinations() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        // Two criteria: one met-true object, one legacy string.
        const CRITERIA: &str =
            r#"[{"criterion":"met one","met":true},"legacy string"]"#;
        let body = "truth table body content for testing";
        repo.create(ProposalCreateInput {
            title: "Flag Table",
            body,
            acceptance_criteria: Some(CRITERIA),
            status: None,
            body_format: None,
        })
        .await
        .unwrap();

        for (bodies, excerpts, criteria) in [
            (false, false, false),
            (false, false, true),
            (false, true, false),
            (false, true, true),
            (true, false, false),
            (true, false, true),
            (true, true, false),
            (true, true, true),
        ] {
            let req = serde_json::json!({
                "limit": 10,
                "include_bodies": bodies,
                "include_excerpts": excerpts,
                "include_acceptance_criteria": criteria,
            });
            let list = server
                .dispatch_tool("proposal_list", req)
                .await
                .unwrap();
            assert!(list.get("error").is_none(), "flags b={bodies} e={excerpts} c={criteria} failed");
            let row = &list["proposals"].as_array().unwrap()[0];

            // Counts are always present.
            assert_eq!(row["ac_total"].as_i64(), Some(2), "b={bodies} e={excerpts} c={criteria}");
            assert_eq!(row["ac_met"].as_i64(), Some(1), "b={bodies} e={excerpts} c={criteria}");

            // Body appears only when include_bodies is true.
            let expect_body = bodies;
            assert_eq!(
                row.get("body").is_some(),
                expect_body,
                "b={bodies} e={excerpts} c={criteria}: body presence mismatch"
            );
            if expect_body {
                assert_eq!(row["body"].as_str(), Some(body));
            }

            // Excerpt metadata appears when bodies or excerpts is true.
            let expect_excerpt = bodies || excerpts;
            assert_eq!(
                row.get("body_excerpt").is_some(),
                expect_excerpt,
                "b={bodies} e={excerpts} c={criteria}: body_excerpt presence mismatch"
            );
            assert_eq!(
                row.get("body_truncated").is_some(),
                expect_excerpt,
                "b={bodies} e={excerpts} c={criteria}: body_truncated presence mismatch"
            );

            // Criteria appear only when include_acceptance_criteria is true.
            assert_eq!(
                row.get("acceptance_criteria").is_some(),
                criteria,
                "b={bodies} e={excerpts} c={criteria}: criteria presence mismatch"
            );
        }
    }

    /// Omitted flags and explicit `false` produce identical row shapes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_omitted_equals_explicit_false() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        repo.create(ProposalCreateInput {
            title: "Equivalence",
            body: "equivalence body",
            acceptance_criteria: Some(r#"[{"criterion":"x","met":true}]"#),
            status: None,
            body_format: None,
        })
        .await
        .unwrap();

        let omitted = server
            .dispatch_tool("proposal_list", serde_json::json!({ "limit": 10 }))
            .await
            .unwrap();
        let explicit_false = server
            .dispatch_tool("proposal_list", serde_json::json!({
                "limit": 10,
                "include_bodies": false,
                "include_excerpts": false,
                "include_acceptance_criteria": false,
            }))
            .await
            .unwrap();

        let r1 = &omitted["proposals"].as_array().unwrap()[0];
        let r2 = &explicit_false["proposals"].as_array().unwrap()[0];

        // Both omit the same optional fields.
        for field in ["body", "body_excerpt", "body_truncated", "acceptance_criteria"] {
            assert_eq!(
                r1.get(field).is_none(),
                r2.get(field).is_none(),
                "field {field} differs between omitted and explicit-false"
            );
        }
        // Both carry the same counts.
        assert_eq!(r1["ac_total"], r2["ac_total"]);
        assert_eq!(r1["ac_met"], r2["ac_met"]);
    }

    /// Independent flags: requesting criteria does not add excerpts, and vice
    /// versa. Bodies imply excerpts but not criteria.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_flags_are_independent() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        repo.create(ProposalCreateInput {
            title: "Independent",
            body: "independent body content here",
            acceptance_criteria: Some(r#"[{"criterion":"a","met":false}]"#),
            status: None,
            body_format: None,
        })
        .await
        .unwrap();

        // Criteria only — no excerpt, no body.
        let list = server
            .dispatch_tool("proposal_list", serde_json::json!({
                "limit": 10,
                "include_acceptance_criteria": true,
            }))
            .await
            .unwrap();
        let row = &list["proposals"].as_array().unwrap()[0];
        assert!(row.get("acceptance_criteria").is_some());
        assert!(row.get("body_excerpt").is_none());
        assert!(row.get("body").is_none());

        // Excerpts only — no criteria, no body.
        let list = server
            .dispatch_tool("proposal_list", serde_json::json!({
                "limit": 10,
                "include_excerpts": true,
            }))
            .await
            .unwrap();
        let row = &list["proposals"].as_array().unwrap()[0];
        assert!(row.get("body_excerpt").is_some());
        assert!(row.get("body_truncated").is_some());
        assert!(row.get("acceptance_criteria").is_none());
        assert!(row.get("body").is_none());

        // Bodies — implies excerpt metadata but not criteria.
        let list = server
            .dispatch_tool("proposal_list", serde_json::json!({
                "limit": 10,
                "include_bodies": true,
            }))
            .await
            .unwrap();
        let row = &list["proposals"].as_array().unwrap()[0];
        assert!(row.get("body").is_some());
        assert!(row.get("body_excerpt").is_some());
        assert!(row.get("body_truncated").is_some());
        assert!(row.get("acceptance_criteria").is_none());
    }

    /// Tolerant count semantics: legacy strings count as total/not-met; object
    /// met:true counts as met; false/missing met counts as not-met; empty/absent/
    /// malformed non-array storage yields 0/0.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_tolerant_counts() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());

        // Legacy strings: two items, zero met.
        let p_strings = repo
            .create(ProposalCreateInput {
                title: "Legacy strings",
                body: "body",
                acceptance_criteria: Some(r#"["first string","second string"]"#),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        // Objects: two met:true, one met:false, one missing met.
        let p_objects = repo
            .create(ProposalCreateInput {
                title: "Objects mixed",
                body: "body",
                acceptance_criteria: Some(
                    r#"[
                        {"criterion":"m1","met":true},
                        {"criterion":"m2","met":true},
                        {"criterion":"n1","met":false},
                        {"criterion":"n2"}
                    ]"#,
                ),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        // Empty array.
        let p_empty = repo
            .create(ProposalCreateInput {
                title: "Empty criteria",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        // Absent criteria (null in storage).
        let p_absent = repo
            .create(ProposalCreateInput {
                title: "Absent criteria",
                body: "body",
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        // Malformed non-array (a plain JSON object, not an array).
        let p_malformed = repo
            .create(ProposalCreateInput {
                title: "Malformed object",
                body: "body",
                acceptance_criteria: Some(r#"{"not":"an array"}"#),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let list = server
            .dispatch_tool("proposal_list", serde_json::json!({ "limit": 10 }))
            .await
            .unwrap();
        let rows = list["proposals"].as_array().unwrap();
        let find = |pid: &str| -> &serde_json::Value {
            rows.iter()
                .find(|r| r["id"].as_str() == Some(pid))
                .unwrap_or_else(|| panic!("row {pid} not found"))
        };

        // Legacy strings: total=2, met=0.
        let r = find(&p_strings.id);
        assert_eq!(r["ac_total"].as_i64(), Some(2));
        assert_eq!(r["ac_met"].as_i64(), Some(0));

        // Objects: total=4, met=2.
        let r = find(&p_objects.id);
        assert_eq!(r["ac_total"].as_i64(), Some(4));
        assert_eq!(r["ac_met"].as_i64(), Some(2));

        // Empty: total=0, met=0.
        let r = find(&p_empty.id);
        assert_eq!(r["ac_total"].as_i64(), Some(0));
        assert_eq!(r["ac_met"].as_i64(), Some(0));

        // Absent: total=0, met=0.
        let r = find(&p_absent.id);
        assert_eq!(r["ac_total"].as_i64(), Some(0));
        assert_eq!(r["ac_met"].as_i64(), Some(0));

        // Malformed non-array: total=0, met=0 (fail closed).
        let r = find(&p_malformed.id);
        assert_eq!(r["ac_total"].as_i64(), Some(0));
        assert_eq!(r["ac_met"].as_i64(), Some(0));
    }

    // ── proposal_show field / revision body mode tests ───────────────────────

    /// `proposal_show` default revision output uses excerpts.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn show_default_revision_excerpt() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let body = "a".repeat(2000);
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Revision Excerpt",
                body: &body,
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let show = server
            .dispatch_tool(
                "proposal_show",
                serde_json::json!({
                    "id": proposal.id,
                    "fields": ["revisions"],
                }),
            )
            .await
            .unwrap();
        assert!(show.get("error").is_none(), "show failed: {:?}", show.get("error"));
        let revs = show["revisions"].as_array().expect("revisions present");
        assert!(!revs.is_empty());
        let rev = &revs[0];
        assert!(rev.get("body").is_none(), "default revision body should be omitted");
        assert!(rev["body_excerpt"].is_string());
        assert!(rev["body_truncated"].as_bool().unwrap());
        assert_eq!(rev["body_excerpt"].as_str().unwrap().chars().count(), 512);
    }

    /// `revision_bodies: "full"` restores full revision bodies.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn show_revision_bodies_full() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let body = "b".repeat(2000);
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Revision Full",
                body: &body,
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let show = server
            .dispatch_tool(
                "proposal_show",
                serde_json::json!({
                    "id": proposal.id,
                    "fields": ["revisions"],
                    "revision_bodies": "full",
                }),
            )
            .await
            .unwrap();
        let revs = show["revisions"].as_array().expect("revisions present");
        let rev = &revs[0];
        assert_eq!(rev["body"].as_str().unwrap(), &body);
        assert_eq!(rev["body_excerpt"].as_str().unwrap().chars().count(), 512);
        assert!(rev["body_truncated"].as_bool().unwrap());
    }

    /// `revision_bodies: "omit"` removes all revision body text.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn show_revision_bodies_omit() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Revision Omit",
                body: "some body content",
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let show = server
            .dispatch_tool(
                "proposal_show",
                serde_json::json!({
                    "id": proposal.id,
                    "fields": ["revisions"],
                    "revision_bodies": "omit",
                }),
            )
            .await
            .unwrap();
        let revs = show["revisions"].as_array().expect("revisions present");
        let rev = &revs[0];
        assert!(rev.get("body").is_none());
        assert!(rev.get("body_excerpt").is_none());
        assert!(rev.get("body_truncated").is_none());
    }

    /// Invalid `fields` returns an error naming the invalid value and accepted values.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn show_invalid_fields_error() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Invalid Fields",
                body: "body",
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let resp = server
            .dispatch_tool(
                "proposal_show",
                serde_json::json!({
                    "id": proposal.id,
                    "fields": ["unknown_field"],
                }),
            )
            .await
            .unwrap();
        let err = resp["error"].as_str().expect("response should contain error field");
        assert!(err.contains("invalid field: \"unknown_field\""), "err: {err}");
        assert!(err.contains("accepted: proposal, targets, feedback, signoffs, revisions, debate, epics, gate_status"), "err: {err}");
    }

    /// Invalid `revision_bodies` returns an error naming the invalid value and accepted values.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn show_invalid_revision_bodies_error() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Invalid Revision Bodies",
                body: "body",
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let resp = server
            .dispatch_tool(
                "proposal_show",
                serde_json::json!({
                    "id": proposal.id,
                    "fields": ["revisions"],
                    "revision_bodies": "compressed",
                }),
            )
            .await
            .unwrap();
        let err = resp["error"].as_str().expect("response should contain error field");
        assert!(err.contains("invalid revision_bodies: \"compressed\""), "err: {err}");
        assert!(err.contains("accepted: excerpt, full, omit"), "err: {err}");
    }

    /// `revision_bodies` is ignored when `fields` omits `revisions`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn show_revision_bodies_ignored_without_revisions_field() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Ignored Revision Bodies",
                body: "body",
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let show = server
            .dispatch_tool(
                "proposal_show",
                serde_json::json!({
                    "id": proposal.id,
                    "fields": ["proposal"],
                    "revision_bodies": "full",
                }),
            )
            .await
            .unwrap();
        assert!(show.get("error").is_none(), "show failed: {:?}", show.get("error"));
        assert!(show.get("revisions").is_none(), "revisions should not be present");
    }

    // ── Payload budget tests ─────────────────────────────────────────────────

    /// Exactly 50 deterministic rows with 4,096-byte bodies and multiple criteria
    /// stay <= 32,768 UTF-8 bytes as the complete serialized envelope, omitting
    /// body/excerpt/criteria by default, while always carrying ac_total/ac_met.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_envelope_compact_50_rows() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        const MAX_ENVELOPE_BYTES: usize = 32_768;
        const BODY_LEN: usize = 4_096;
        const SERIALIZED_ID_BYTES: usize = 38; // 36 characters plus JSON quotes
        const SERIALIZED_TIMESTAMP_BYTES: usize = 26; // 24 characters plus JSON quotes
        // Mix of legacy strings and objects, including met-true, met-false,
        // and bare criterion objects. Criteria are omitted by default but the
        // count fields prove they were counted from the stored data.
        const CRITERIA_JSON: &str = r#"[
            {"criterion":"Must handle a thing","met":true},
            {"criterion":"Should handle another","met":false},
            "Legacy plain string criterion",
            {"criterion":"No met field at all"}
        ]"#;

        let large_body = "B".repeat(BODY_LEN);
        for i in 0..50 {
            let title = format!("Proposal {i:02} summary row budget fixture");
            repo.create(ProposalCreateInput {
                title: &title,
                body: &large_body,
                acceptance_criteria: Some(CRITERIA_JSON),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        }

        let response = server
            .dispatch_tool("proposal_list", serde_json::json!({ "limit": 50, "offset": 0 }))
            .await
            .unwrap();

        // Assert complete envelope metadata before measuring.
        assert_eq!(response["total_count"].as_i64(), Some(50));
        assert_eq!(response["limit"].as_i64(), Some(50));
        assert_eq!(response["offset"].as_i64(), Some(0));
        assert_eq!(response["has_more"].as_bool(), Some(false));

        let rows = response["proposals"].as_array().expect("proposals array");
        assert_eq!(rows.len(), 50, "expected exactly 50 rows");

        // Every default row omits body, excerpt metadata, and criteria.
        for (i, row) in rows.iter().enumerate() {
            assert!(row.get("body").is_none(), "row {i}: body must be absent");
            assert!(row.get("body_excerpt").is_none(), "row {i}: body_excerpt must be absent");
            assert!(row.get("body_truncated").is_none(), "row {i}: body_truncated must be absent");
            assert!(row.get("acceptance_criteria").is_none(), "row {i}: criteria must be absent");
            // Values vary, but JSON widths are fixed for repository UUIDs and timestamps.
            assert_eq!(serde_json::to_vec(&row["id"]).unwrap().len(), SERIALIZED_ID_BYTES);
            for field in ["created_at", "updated_at"] {
                assert_eq!(
                    serde_json::to_vec(&row[field]).unwrap().len(),
                    SERIALIZED_TIMESTAMP_BYTES,
                    "row {i}: serialized {field} width"
                );
            }
            // Always-present integer counts from the mixed criteria fixture.
            assert_eq!(row["ac_total"].as_i64(), Some(4), "row {i}: ac_total");
            assert_eq!(row["ac_met"].as_i64(), Some(1), "row {i}: ac_met (only met:true counts)");
        }

        let envelope = serde_json::to_vec(&response).expect("response serializes");
        assert!(
            envelope.len() <= MAX_ENVELOPE_BYTES,
            "50-row default list envelope is {} bytes, exceeds {} byte ceiling",
            envelope.len(),
            MAX_ENVELOPE_BYTES
        );
    }

    /// proposal_show with 25 revisions of 4,096-char bodies is <= 64 KiB by default,
    /// and `revision_bodies: \"full\"` exposes all full revision bodies.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn show_payload_budget_default() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        const BUDGET_KIB: usize = 64;
        const BUDGET_BYTES: usize = BUDGET_KIB * 1024;
        const BODY_LEN: usize = 4096;

        let body = "s".repeat(BODY_LEN);
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Revision Budget",
                body: &body,
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Create 24 additional material revisions so the total is 25.
        for i in 0..24 {
            let updated = format!("{body}\nrev {i}");
            repo.update(
                &proposal.id,
                djinn_db::ProposalUpdateInput {
                    title: &proposal.title,
                    body: &updated,
                    acceptance_criteria: &proposal.acceptance_criteria,
                    status: &proposal.status,
                    superseded_by: proposal.superseded_by.as_deref(),
                    body_format: Some(&proposal.body_format),
                    event_metadata: None,
                },
            )
            .await
            .unwrap();
        }

        let show = server
            .dispatch_tool(
                "proposal_show",
                serde_json::json!({
                    "id": proposal.id,
                    "fields": ["revisions"],
                }),
            )
            .await
            .unwrap();
        let json = serde_json::to_string(&show).expect("show serializes");
        assert!(
            json.len() <= BUDGET_BYTES,
            "default show payload {} bytes exceeds {} KiB budget",
            json.len(),
            BUDGET_KIB
        );

        let full = server
            .dispatch_tool(
                "proposal_show",
                serde_json::json!({
                    "id": proposal.id,
                    "fields": ["revisions"],
                    "revision_bodies": "full",
                }),
            )
            .await
            .unwrap();
        let revs = full["revisions"].as_array().expect("revisions present");
        assert_eq!(revs.len(), 25);
        assert!(
            revs.iter().all(|r| r["body"].as_str().map(|b| b.len() >= BODY_LEN).unwrap_or(false)),
            "full revision bodies should be available"
        );
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

// ── MDX auto-upgrade + block validation on the create/update write paths ─────
//
// A proposal body can carry MDX block tags while its declared/omitted
// body_format is "markdown"; before the cutover that skipped ALL block
// validation and stored the tags as raw markdown (rendered as literal text in
// the UI). These tests pin the cutover: any full-body write with block tags is
// upgraded to "mdx" and validated identically to a declared-mdx body.

#[cfg(test)]
mod mdx_upgrade_and_validation_tests {
    include!("create_mdx_tests.rs");
}

// ── Repository lint result mutation contract ───────────────────────────────
// These tool-level integrations pin the lint-cache response and rollback
// contract for every server authoring surface.
#[cfg(test)]
mod lint_mutation_contract_tests {
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

    async fn assert_response_has_exact_head_lint(
        repo: &ProposalRepository,
        response: &Value,
        expected_body: &str,
    ) -> String {
        assert!(response.get("error").is_none(), "mutation failed: {response:?}");
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
        let expected_lint = serde_json::to_value(repo.lint_for_revision(revision).await.unwrap()).unwrap();
        assert_eq!(response["latest_lint"], expected_lint, "response must publish exact cached lint");
        assert_eq!(response["latest_lint"]["body_sha256"], djinn_spec_lint::body_sha256(expected_body));
        assert!(
            response["latest_lint"]["warnings"].as_array().is_some_and(|warnings| !warnings.is_empty()),
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
            .dispatch_tool("proposal_create", serde_json::json!({
                "title": "Warning create", "body": WARNING_BODY,
            }))
            .await
            .unwrap();
        let create_id = assert_response_has_exact_head_lint(&repo, &created, WARNING_BODY).await;

        let updated_body = "Updated [dangling local reference](#still-missing).";
        let updated = server
            .dispatch_tool("proposal_update", serde_json::json!({
                "id": create_id, "body": updated_body,
            }))
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
            .dispatch_tool("proposal_create", serde_json::json!({
                "title": "Warning patch", "body": patch_source,
            }))
            .await
            .unwrap();
        let patch_id = patch_seed["id"].as_str().unwrap();
        let patched_body = "Patched [dangling local reference](#missing-after-patch).";
        let patched = server
            .dispatch_tool("proposal_block_patch", serde_json::json!({
                "id": patch_id,
                "selector": { "exact_text": patch_source },
                "operation": "replace",
                "block_mdx": patched_body,
            }))
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

        let before_create_lints = lint_row_count(&db, None).await;
        let rejected_create = server
            .dispatch_tool("proposal_create", serde_json::json!({
                "title": "Rejected create", "body": rejected_body, "body_format": "mdx",
            }))
            .await
            .unwrap();
        assert_eq!(rejected_create["error"], "SPEC_LINT_REJECTED");
        assert_eq!(rejected_create["code"], "SPEC_LINT_REJECTED");
        let violations = rejected_create["violations"].as_array().expect("structured violations");
        assert_eq!(violations.len(), 2);
        for violation in violations {
            assert_eq!(violation["code"], "DUPLICATE_BLOCK_ID");
            assert_eq!(violation["severity"], "error");
            assert!(violation["message"].is_string());
            assert!(violation["span"]["start_byte"].is_u64());
            assert!(violation["span"]["end_byte"].is_u64());
        }
        assert!(violations.windows(2).all(|pair| {
            (pair[0]["span"]["start_byte"].as_u64(), pair[0]["span"]["end_byte"].as_u64())
                <= (pair[1]["span"]["start_byte"].as_u64(), pair[1]["span"]["end_byte"].as_u64())
        }));
        assert_eq!(lint_row_count(&db, None).await, before_create_lints, "rejected create leaves no lint row");

        let seed = server
            .dispatch_tool("proposal_create", serde_json::json!({
                "title": "Rollback seed", "body": "Original paragraph.",
            }))
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
}
