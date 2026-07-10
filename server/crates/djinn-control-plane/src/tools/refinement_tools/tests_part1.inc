
    async fn test_server() -> (DjinnMcpServer, Database) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        (DjinnMcpServer::new(test_mcp_state(db.clone())), db)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_start_creates_lifecycle_event() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Refinement Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None, // defaults to draft
                body_format: None,
            })
            .await
            .unwrap();
        link_proposal_to_project(&db, &repo, &proposal.id).await;

        let resp = server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .expect("tool should be registered");

        assert!(
            resp.get("error").and_then(|v| v.as_str()).is_none(),
            "expected no error, got: {:?}",
            resp.get("error")
        );
        let refinement = resp.get("refinement").expect("should have refinement");
        assert_eq!(
            refinement.get("active").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            refinement.get("current_round").and_then(|v| v.as_i64()),
            Some(1)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_start_rejects_building_proposal() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Building Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: Some("building"),
                body_format: None,
            })
            .await
            .unwrap();

        let resp = server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .expect("tool should be registered");

        let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(
            error.contains("does not support refinement"),
            "should reject building status: {error}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_status_returns_inactive_when_not_started() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "No Refinement",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let resp = server
            .dispatch_tool(
                "proposal_refinement_status",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .expect("tool should be registered");

        let refinement = resp.get("refinement").expect("should have refinement");
        assert_eq!(
            refinement.get("active").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            refinement.get("current_round").and_then(|v| v.as_i64()),
            None
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_status_reflects_debate_trail_rounds() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Debate Round Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: Some("in_review"),
                body_format: None,
            })
            .await
            .unwrap();
        link_proposal_to_project(&db, &repo, &proposal.id).await;

        // Start refinement.
        server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .unwrap();

        // Add a non-blocking objection in round 1.
        server
            .dispatch_tool(
                "proposal_debate_append",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "kind": "objection",
                    "body": "Minor issue",
                    "blocking": false,
                    "agent_role": "adversary",
                    "author_kind": "agent",
                    "against_revision_seq": 1,
                    "round": 1,
                }),
            )
            .await
            .unwrap();

        // Add a non-blocking objection in round 2.
        server
            .dispatch_tool(
                "proposal_debate_append",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "kind": "objection",
                    "body": "Another minor issue",
                    "blocking": false,
                    "agent_role": "adversary",
                    "author_kind": "agent",
                    "against_revision_seq": 1,
                    "round": 2,
                }),
            )
            .await
            .unwrap();

        let resp = server
            .dispatch_tool(
                "proposal_refinement_status",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .unwrap();

        let refinement = resp.get("refinement").expect("should have refinement");
        assert_eq!(
            refinement.get("active").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            refinement.get("current_round").and_then(|v| v.as_i64()),
            Some(2)
        );
        assert_eq!(
            refinement.get("total_entries").and_then(|v| v.as_i64()),
            Some(2)
        );
        // Both rounds are non-blocking → 2 dry rounds.
        assert_eq!(
            refinement.get("dry_rounds").and_then(|v| v.as_i64()),
            Some(2)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_start_rejects_nonexistent_proposal() {
        let (server, _db) = test_server().await;

        let resp = server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({ "proposal_id": "nonexistent" }),
            )
            .await
            .expect("tool should be registered");

        let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(
            error.contains("proposal not found"),
            "should mention proposal not found: {error}"
        );
    }

    /// Test that when the coordinator rejects the refinement start (e.g.
    /// duplicate active run), the error is propagated through the response
    /// and a `refinement_stop` lifecycle entry is recorded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_start_propagates_coordinator_error_and_records_stop() {
        use crate::bridge::CoordinatorOps;
        use async_trait::async_trait;

        /// Coordinator stub that always rejects refinement starts.
        struct RejectingCoordinator;
        #[async_trait]
        impl CoordinatorOps for RejectingCoordinator {
            fn get_status(&self) -> Result<crate::bridge::CoordinatorStatus, String> {
                Err("not initialized".into())
            }
            async fn trigger_dispatch_for_project(&self, _: &str) -> Result<(), String> {
                Err("not initialized".into())
            }
            async fn start_proposal_refinement(
                &self,
                _: crate::bridge::ProposalRefinementStartRequest,
            ) -> Result<(), String> {
                Err("duplicate refinement active".to_string())
            }
            async fn demand_proposal_refinement_round(
                &self,
                _: crate::bridge::ProposalRefinementStartRequest,
            ) -> Result<(), String> {
                Err("refinement still active".to_string())
            }
            async fn resolve_refinement_review(
                &self,
                _: String,
                _: bool,
                _: Option<String>,
            ) -> Result<(), String> {
                Err("no refinement awaiting review".to_string())
            }
            async fn record_supervisor_rework_reopen(
                &self,
                _: &str,
                _: &djinn_core::models::TransitionAction,
                _: Option<&str>,
            ) {
            }
        }

        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let state = crate::state::stubs::test_mcp_state(db.clone());
        // Override the coordinator to one that rejects.
        let state = crate::state::McpState::with_enrichment(
            db.clone(),
            state.event_bus(),
            state.catalog().clone(),
            state.health_tracker().clone(),
            Some(Arc::new(RejectingCoordinator) as Arc<dyn CoordinatorOps>),
            None,
            None,
            None,
            state.lsp().clone(),
            Arc::new(crate::state::stubs::StubRuntimeOps),
            Arc::new(crate::state::stubs::StubGitOps),
            Arc::new(crate::state::stubs::StubRepoGraphOps),
            None,
        );
        let server = DjinnMcpServer::new(state);

        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Reject Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        link_proposal_to_project(&db, &repo, &proposal.id).await;

        let resp = server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .expect("tool should be registered");

        let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(
            error.contains("coordinator rejected refinement start"),
            "should mention coordinator rejection: {error}"
        );
        assert!(
            error.contains("duplicate refinement active"),
            "should include the coordinator error message: {error}"
        );

        // Verify a refinement_stop lifecycle entry was recorded.
        let revisions = repo.revisions(&proposal.id).await.unwrap();
        let starts: Vec<_> = revisions
            .iter()
            .filter(|r| r.event_kind == "refinement_start")
            .collect();
        let stops: Vec<_> = revisions
            .iter()
            .filter(|r| r.event_kind == "refinement_stop")
            .collect();
        assert_eq!(starts.len(), 1, "expected one refinement_start");
        assert_eq!(stops.len(), 1, "expected one refinement_stop");
        // The stop metadata should include the error reason.
        let stop_meta: serde_json::Value =
            serde_json::from_str(stops[0].event_metadata.as_deref().unwrap_or("{}")).unwrap();
        assert_eq!(
            stop_meta.get("source").and_then(|v| v.as_str()),
            Some("coordinator_start_failure")
        );
    }

    /// Test that when the coordinator is not wired (None), the tool records
    /// a refinement_stop and returns an error rather than leaving a dangling
    /// lifecycle entry.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_start_records_stop_when_coordinator_unavailable() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        // Build McpState with no coordinator (None).
        let state = crate::state::McpState::new(
            db.clone(),
            EventBus::noop(),
            crate::state::stubs::test_mcp_state(db.clone())
                .catalog()
                .clone(),
            crate::state::stubs::test_mcp_state(db.clone())
                .health_tracker()
                .clone(),
            None, // no coordinator
            None,
            None,
            None,
            Arc::new(crate::state::stubs::StubLspOps),
            Arc::new(crate::state::stubs::StubRuntimeOps),
            Arc::new(crate::state::stubs::StubGitOps),
            Arc::new(crate::state::stubs::StubRepoGraphOps),
        );
        let server = DjinnMcpServer::new(state);

        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "No Coordinator Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        link_proposal_to_project(&db, &repo, &proposal.id).await;

        let resp = server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .expect("tool should be registered");

        let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(
            error.contains("coordinator not available"),
            "should mention coordinator unavailable: {error}"
        );

        // Verify the lifecycle was cleaned up.
        let revisions = repo.revisions(&proposal.id).await.unwrap();
        let stops: Vec<_> = revisions
            .iter()
            .filter(|r| r.event_kind == "refinement_stop")
            .collect();
        assert_eq!(stops.len(), 1, "expected one refinement_stop");
    }

    // ── Full refinement happy path ────────────────────────────────────────────

    /// End-to-end happy path: start refinement → adversary blocking objection
    /// (round 1) → adversary dry (round 2) → adversary dry (round 3) →
    /// judge verdict → status reports stopped with adversary_dry.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_happy_path_start_to_stop() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Happy Path Test",
                body: "## Problem\nThe problem is X.\n## Solution\nWe do Y.",
                acceptance_criteria: Some(r#"["AC1: done", "AC2: done"]"#),
                status: Some("in_review"),
                body_format: None,
            })
            .await
            .unwrap();
        link_proposal_to_project(&db, &repo, &proposal.id).await;

        // 1. Start refinement (checkpoint mode).
        let start_resp = server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .unwrap();
        assert!(start_resp.get("error").and_then(|v| v.as_str()).is_none());
        let refinement = start_resp.get("refinement").unwrap();
        assert_eq!(
            refinement.get("active").and_then(|v| v.as_bool()),
            Some(true)
        );

        // 2. Round 1: adversary raises a blocking objection.
        server
            .dispatch_tool(
                "proposal_debate_append",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "kind": "objection",
                    "body": "Missing risk assessment section",
                    "blocking": true,
                    "agent_role": "adversary",
                    "author_kind": "agent",
                    "author_model": "openai/gpt-4o",
                    "against_revision_seq": 0,
                    "round": 1,
                }),
            )
            .await
            .unwrap();

        // 3. Round 2: adversary finds no blocking objections (dry).
        server
            .dispatch_tool(
                "proposal_debate_append",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "kind": "objection",
                    "body": "Minor formatting concern",
                    "blocking": false,
                    "agent_role": "adversary",
                    "author_kind": "agent",
                    "against_revision_seq": 1,
                    "round": 2,
                }),
            )
            .await
            .unwrap();

        // 4. Round 3: adversary is dry again.
        // (No debate entries for round 3 = explicit dry.)

        // 5. Judge verdict.
        server
            .dispatch_tool(
                "proposal_debate_append",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "kind": "verdict",
                    "body": "Proposal meets readiness criteria.",
                    "blocking": false,
                    "agent_role": "judge",
                    "author_kind": "agent",
                    "author_model": "anthropic/claude-sonnet-4-20250514",
                    "against_revision_seq": 1,
                    "round": 3,
                }),
            )
            .await
            .unwrap();

        // 6. Record refinement_stop lifecycle to simulate coordinator stop.
        let stop_metadata = serde_json::json!({
            "source": "refinement_loop",
            "event": "refinement_stop",
            "reason_tag": "adversary_dry",
        });
        repo.record_refinement_lifecycle(&proposal.id, "refinement_stop", Some(&stop_metadata))
            .await
            .unwrap();

        // 7. Verify refinement status shows stopped with adversary_dry.
        let status_resp = server
            .dispatch_tool(
                "proposal_refinement_status",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .unwrap();
        let refinement = status_resp.get("refinement").unwrap();
        assert_eq!(
            refinement.get("active").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            refinement.get("stop_reason").and_then(|v| v.as_str()),
            Some("adversary_dry")
        );
        // Round should reflect the max round in the debate trail.
        assert_eq!(
            refinement.get("current_round").and_then(|v| v.as_i64()),
            Some(3)
        );
        // Total debate entries: 1 objection + 1 non-blocking + 1 verdict = 3.
        assert_eq!(
            refinement.get("total_entries").and_then(|v| v.as_i64()),
            Some(3)
        );
        // Only round 2 has no blocking adversary objection (round 1 had one).
        // Dry rounds count consecutive from the end: round 3 had no adversary
        // objection at all (verdict only), round 2 had non-blocking → 2 dry.
        let dry = refinement
            .get("dry_rounds")
            .and_then(|v| v.as_i64())
            .unwrap();
        assert!(dry >= 1, "should have at least 1 dry round, got {dry}");
    }

    // ── Stop reason: round_cap ────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_status_shows_round_cap_stop_reason() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Round Cap Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        link_proposal_to_project(&db, &repo, &proposal.id).await;

        // Start refinement.
        server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .unwrap();

        // Simulate coordinator recording a round_cap stop.
        let stop_metadata = serde_json::json!({
            "source": "refinement_loop",
            "event": "refinement_stop",
            "reason_tag": "round_cap",
            "reason_detail": "RoundCap",
        });
        repo.record_refinement_lifecycle(&proposal.id, "refinement_stop", Some(&stop_metadata))
            .await
            .unwrap();

        let status_resp = server
            .dispatch_tool(
                "proposal_refinement_status",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .unwrap();
        let refinement = status_resp.get("refinement").unwrap();
        assert_eq!(
            refinement.get("active").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            refinement.get("stop_reason").and_then(|v| v.as_str()),
            Some("round_cap")
        );
    }

    // ── Stop reason: spawn_cap ─────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_status_shows_spawn_cap_stop_reason() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Spawn Cap Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        link_proposal_to_project(&db, &repo, &proposal.id).await;

        server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .unwrap();

        let stop_metadata = serde_json::json!({
            "source": "refinement_loop",
            "event": "refinement_stop",
            "reason_tag": "spawn_cap",
        });
        repo.record_refinement_lifecycle(&proposal.id, "refinement_stop", Some(&stop_metadata))
            .await
            .unwrap();

        let status_resp = server
            .dispatch_tool(
                "proposal_refinement_status",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .unwrap();
        let refinement = status_resp.get("refinement").unwrap();
        assert_eq!(
            refinement.get("stop_reason").and_then(|v| v.as_str()),
            Some("spawn_cap")
        );
    }

    // ── Stop reason: repeated_objection ────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_status_shows_repeated_objection_stop_reason() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Repeated Objection Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        link_proposal_to_project(&db, &repo, &proposal.id).await;

        server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .unwrap();

        let stop_metadata = serde_json::json!({
            "source": "refinement_loop",
            "event": "refinement_stop",
            "reason_tag": "repeated_objection",
        });
        repo.record_refinement_lifecycle(&proposal.id, "refinement_stop", Some(&stop_metadata))
            .await
            .unwrap();

        let status_resp = server
            .dispatch_tool(
                "proposal_refinement_status",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .unwrap();
        let refinement = status_resp.get("refinement").unwrap();
        assert_eq!(
            refinement.get("stop_reason").and_then(|v| v.as_str()),
            Some("repeated_objection")
        );
    }

    // ── Stop reason: agent_failure ─────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_status_shows_agent_failure_stop_reason() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Agent Failure Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        link_proposal_to_project(&db, &repo, &proposal.id).await;

        server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .unwrap();

        let stop_metadata = serde_json::json!({
            "source": "refinement_loop",
            "event": "refinement_stop",
            "reason_tag": "agent_failure",
        });
        repo.record_refinement_lifecycle(&proposal.id, "refinement_stop", Some(&stop_metadata))
            .await
            .unwrap();

        let status_resp = server
            .dispatch_tool(
                "proposal_refinement_status",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .unwrap();
        let refinement = status_resp.get("refinement").unwrap();
        assert_eq!(
            refinement.get("stop_reason").and_then(|v| v.as_str()),
            Some("agent_failure")
        );
    }

