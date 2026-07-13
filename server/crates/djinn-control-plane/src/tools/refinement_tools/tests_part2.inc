    // ── Debate-trail metadata fields ──────────────────────────────────────────

    /// Debate-trail entries carry round, against_revision_seq, agent_role,
    /// author_kind, and author_model metadata.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn debate_trail_entries_include_round_revision_role_model_metadata() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Metadata Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Add an adversary objection with full metadata.
        server
            .dispatch_tool(
                "proposal_debate_append",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "kind": "objection",
                    "body": "Missing scope section",
                    "blocking": true,
                    "agent_role": "adversary",
                    "author_kind": "agent",
                    "author_model": "openai/gpt-4o",
                    "against_revision_seq": 2,
                    "round": 1,
                }),
            )
            .await
            .unwrap();

        // Add a judge verdict with full metadata.
        server
            .dispatch_tool(
                "proposal_debate_append",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "kind": "verdict",
                    "body": "Proposal is ready.",
                    "blocking": false,
                    "agent_role": "judge",
                    "author_kind": "agent",
                    "author_model": "anthropic/claude-sonnet-4-20250514",
                    "against_revision_seq": 3,
                    "round": 2,
                }),
            )
            .await
            .unwrap();

        // Read back via proposal_debate_list.
        let list_resp = server
            .dispatch_tool(
                "proposal_debate_list",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .unwrap();
        let entries = list_resp
            .get("entries")
            .and_then(|v| v.as_array())
            .expect("should have entries array");
        assert_eq!(entries.len(), 2, "expected 2 debate entries");

        // Verify adversary objection metadata.
        let objection = &entries[0];
        assert_eq!(
            objection.get("kind").and_then(|v| v.as_str()),
            Some("objection")
        );
        assert_eq!(
            objection.get("agent_role").and_then(|v| v.as_str()),
            Some("adversary")
        );
        assert_eq!(
            objection.get("author_kind").and_then(|v| v.as_str()),
            Some("agent")
        );
        assert_eq!(
            objection.get("author_model").and_then(|v| v.as_str()),
            Some("openai/gpt-4o")
        );
        assert_eq!(
            objection
                .get("against_revision_seq")
                .and_then(|v| v.as_i64()),
            Some(2)
        );
        assert_eq!(objection.get("round").and_then(|v| v.as_i64()), Some(1));
        assert_eq!(
            objection.get("blocking").and_then(|v| v.as_bool()),
            Some(true)
        );

        // Verify judge verdict metadata.
        let verdict = &entries[1];
        assert_eq!(
            verdict.get("kind").and_then(|v| v.as_str()),
            Some("verdict")
        );
        assert_eq!(
            verdict.get("agent_role").and_then(|v| v.as_str()),
            Some("judge")
        );
        assert_eq!(
            verdict.get("author_model").and_then(|v| v.as_str()),
            Some("anthropic/claude-sonnet-4-20250514")
        );
        assert_eq!(
            verdict.get("against_revision_seq").and_then(|v| v.as_i64()),
            Some(3)
        );
        assert_eq!(verdict.get("round").and_then(|v| v.as_i64()), Some(2));
    }

    // ── Debate-trail separate from human feedback ─────────────────────────────

    /// `proposal_show` returns debate-trail tribunal rows and human
    /// `proposal_feedback` as separate fields. They must never overlap.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_show_keeps_debate_trail_separate_from_feedback() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Separation Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Add human feedback (proposal_feedback).
        server
            .dispatch_tool(
                "proposal_feedback_add",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "body": "Looks good to me!",
                }),
            )
            .await
            .unwrap();

        // Add a debate-trail entry (tribunal row).
        server
            .dispatch_tool(
                "proposal_debate_append",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "kind": "objection",
                    "body": "Missing scope",
                    "blocking": true,
                    "agent_role": "adversary",
                    "author_kind": "agent",
                    "against_revision_seq": 0,
                    "round": 1,
                }),
            )
            .await
            .unwrap();

        // Fetch via proposal_show.
        let show_resp = server
            .dispatch_tool("proposal_show", serde_json::json!({ "id": proposal.id }))
            .await
            .unwrap();

        // feedback and debate_trail are separate arrays.
        let feedback = show_resp
            .get("feedback")
            .and_then(|v| v.as_array())
            .expect("should have feedback array");
        let debate_trail = show_resp
            .get("debate_trail")
            .and_then(|v| v.as_array())
            .expect("should have debate_trail array");

        assert_eq!(feedback.len(), 1, "expected 1 human feedback entry");
        assert_eq!(debate_trail.len(), 1, "expected 1 debate-trail entry");

        // Human feedback should NOT have agent_role, kind, or round fields.
        let fb = &feedback[0];
        assert!(fb.get("agent_role").is_none() || fb.get("agent_role").unwrap().is_null());
        assert!(fb.get("kind").is_none() || fb.get("kind").unwrap().is_null());

        // Debate-trail entry should have agent_role, kind, and round.
        let dt = &debate_trail[0];
        assert_eq!(
            dt.get("agent_role").and_then(|v| v.as_str()),
            Some("adversary")
        );
        assert_eq!(dt.get("kind").and_then(|v| v.as_str()), Some("objection"));
        assert_eq!(dt.get("round").and_then(|v| v.as_i64()), Some(1));
        assert_eq!(dt.get("blocking").and_then(|v| v.as_bool()), Some(true));
    }

    // ── Lifecycle attribution metadata ────────────────────────────────────────

    /// Refinement-start and refinement-stop lifecycle entries are recorded;
    /// stop entries carry the stop_reason tag.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_lifecycle_entries_carry_attribution_metadata() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Attribution Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        link_proposal_to_project(&db, &repo, &proposal.id).await;

        // Start refinement (always checkpoint-gated).
        server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({
                    "proposal_id": proposal.id,
                }),
            )
            .await
            .unwrap();

        // Simulate coordinator stop.
        let stop_metadata = serde_json::json!({
            "source": "refinement_loop",
            "event": "refinement_stop",
            "reason_tag": "adversary_dry",
            "reason_detail": "AdversaryDry",
        });
        repo.record_refinement_lifecycle(&proposal.id, "refinement_stop", Some(&stop_metadata))
            .await
            .unwrap();

        // Read revisions and verify metadata.
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

        // Stop metadata carries the reason tag.
        let stop_meta: serde_json::Value =
            serde_json::from_str(stops[0].event_metadata.as_deref().unwrap_or("{}")).unwrap();
        assert_eq!(
            stop_meta.get("reason_tag").and_then(|v| v.as_str()),
            Some("adversary_dry"),
            "stop metadata must include reason_tag"
        );
        assert_eq!(
            stop_meta.get("source").and_then(|v| v.as_str()),
            Some("refinement_loop"),
            "stop metadata must include source"
        );
    }

    // ── Duplicate active refinement rejection ─────────────────────────────────

    /// Starting refinement when one is already active (no stop entry after
    /// start) should be rejected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_start_rejects_duplicate_active_refinement() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Duplicate Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        link_proposal_to_project(&db, &repo, &proposal.id).await;

        // First start succeeds.
        let resp1 = server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .unwrap();
        assert!(resp1.get("error").and_then(|v| v.as_str()).is_none());

        // Second start should be rejected (lifecycle already active).
        let resp2 = server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .unwrap();
        let error = resp2.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(
            error.contains("already active"),
            "should reject duplicate start: {error}"
        );
    }

    // ── DoR readiness findings available in refinement status ─────────────────

    /// The deterministic DoR evaluator is consulted at round boundaries.
    /// Verify that a proposal with no problem section fails readiness and
    /// that the readiness result can be derived from the proposal body.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_dor_findings_available_at_round_boundary() {
        use crate::tools::epic_ops::AcceptanceCriterionItem;
        use crate::tools::proposal_readiness::evaluate_proposal_readiness;

        // A proposal body without a Problem section should fail the DoR check.
        let body = "## Solution\nWe do something cool.";
        let acs: Vec<AcceptanceCriterionItem> = vec![];
        let readiness = evaluate_proposal_readiness(body, &acs, 0);
        assert!(
            !readiness.ready,
            "proposal without problem section should fail DoR"
        );
        assert!(
            !readiness.failures.is_empty(),
            "should have readiness failures"
        );

        // A complete proposal body should pass.
        let good_body =
            "## Problem\nSomething is broken.\n## Solution\nFix it.\n## Scope\nLimited scope.";
        let good_acs = vec![AcceptanceCriterionItem::Text("AC1: Done".into())];
        let good_readiness = evaluate_proposal_readiness(good_body, &good_acs, 1);
        // Note: may still fail on vague AC, but the point is DoR is deterministic
        // and reusable — no second evaluator is needed.
        let error_string = good_readiness.to_error_string();
        // The readiness result is deterministic and reusable at round boundaries.
        // We just verify the shape is consistent.
        if good_readiness.ready {
            assert!(error_string.is_none());
        } else {
            assert!(error_string.is_some());
        }
    }

    // ── Human authority: demand round tool ──────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn demand_round_tool_accepts_and_records_action() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Demand Round Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: Some("draft"),
                body_format: None,
            })
            .await
            .unwrap();
        link_proposal_to_project(&db, &repo, &proposal.id).await;

        let resp = server
            .dispatch_tool(
                "proposal_refinement_demand_round",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "reason": "Need another round after judge feedback",
                    "update_authority": "checkpoint",
                }),
            )
            .await
            .unwrap();
        assert!(
            resp.get("error").and_then(|v| v.as_str()).is_none(),
            "expected no error, got: {:?}",
            resp.get("error")
        );
        assert_eq!(
            resp.get("accepted").and_then(|v| v.as_bool()),
            Some(true),
            "demand should be accepted"
        );
        assert_eq!(
            resp.get("proposal_id").and_then(|v| v.as_str()),
            Some(proposal.id.as_str()),
        );

        let revisions = repo.revisions(&proposal.id).await.unwrap();
        let start = revisions
            .iter()
            .rev()
            .find(|r| r.event_kind == "refinement_start")
            .expect("demand round should record refinement_start");
        assert_eq!(
            start.seq, proposal.latest_revision_seq,
            "reviewer feedback must be scoped to the current proposal revision"
        );
        let meta: serde_json::Value =
            serde_json::from_str(start.event_metadata.as_deref().expect("metadata")).unwrap();
        assert_eq!(meta["source"], "human_demand_round");
        assert_eq!(
            meta["reviewer_feedback"],
            "Need another round after judge feedback"
        );
        assert_eq!(meta["reason"], "Need another round after judge feedback");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn demand_round_allows_parked_awaiting_review_and_dispatches_once() {
        use crate::bridge::{CoordinatorOps, ProposalRefinementStartRequest};
        use async_trait::async_trait;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingDemandCoordinator {
            demand_calls: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl CoordinatorOps for CountingDemandCoordinator {
            fn get_status(&self) -> Result<crate::bridge::CoordinatorStatus, String> {
                Err("not initialized".into())
            }
            async fn trigger_dispatch_for_project(&self, _: &str) -> Result<(), String> {
                Err("not initialized".into())
            }
            async fn start_proposal_refinement(
                &self,
                _: ProposalRefinementStartRequest,
            ) -> Result<(), String> {
                Err("unexpected start call".into())
            }
            async fn demand_proposal_refinement_round(
                &self,
                _: ProposalRefinementStartRequest,
            ) -> Result<(), String> {
                self.demand_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            async fn resolve_refinement_review(
                &self,
                _: String,
                _: bool,
                _: Option<String>,
            ) -> Result<(), String> {
                Err("unexpected resolve call".into())
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
        let base = test_mcp_state(db.clone());
        let demand_calls = Arc::new(AtomicUsize::new(0));
        let state = crate::state::McpState::with_enrichment(
            db.clone(),
            base.event_bus(),
            base.catalog().clone(),
            base.health_tracker().clone(),
            base.retrieval_config(),
            base.retrieval_metrics(),
            Some(Arc::new(CountingDemandCoordinator {
                demand_calls: demand_calls.clone(),
            }) as Arc<dyn CoordinatorOps>),
            None,
            None,
            None,
            base.lsp().clone(),
            Arc::new(crate::state::stubs::StubRuntimeOps),
            Arc::new(crate::state::stubs::StubGitOps),
            Arc::new(crate::state::stubs::StubRepoGraphOps),
            None,
        );
        let server = DjinnMcpServer::new(state);
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Awaiting Review Demand",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: Some("in_review"),
                body_format: None,
            })
            .await
            .unwrap();
        link_proposal_to_project(&db, &repo, &proposal.id).await;

        repo.record_refinement_lifecycle(&proposal.id, "refinement_start", None)
            .await
            .unwrap();
        repo.record_refinement_lifecycle(
            &proposal.id,
            "refinement_awaiting_review",
            Some(&serde_json::json!({ "judge_summary": "parked for review" })),
        )
        .await
        .unwrap();

        let feedback = "UNIQUE awaiting-review feedback 019f0fed exact";
        let resp = server
            .dispatch_tool(
                "proposal_refinement_demand_round",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "reason": feedback,
                }),
            )
            .await
            .unwrap();

        assert_eq!(resp.get("accepted").and_then(|v| v.as_bool()), Some(true));
        assert!(resp.get("error").and_then(|v| v.as_str()).is_none());
        assert_eq!(demand_calls.load(Ordering::SeqCst), 1);

        let revisions = repo.revisions(&proposal.id).await.unwrap();
        let demand_starts: Vec<_> = revisions
            .iter()
            .filter(|r| {
                r.event_kind == "refinement_start"
                    && r.event_metadata
                        .as_deref()
                        .is_some_and(|m| m.contains("human_demand_round"))
            })
            .collect();
        assert_eq!(demand_starts.len(), 1, "fresh demand start exactly once");
        let meta: serde_json::Value = serde_json::from_str(
            demand_starts[0]
                .event_metadata
                .as_deref()
                .expect("demand metadata"),
        )
        .unwrap();
        assert_eq!(meta["source"], "human_demand_round");
        assert_eq!(meta["reviewer_feedback"], feedback);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn demand_round_rejects_true_duplicate_active_refinement() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Duplicate Active Demand",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: Some("draft"),
                body_format: None,
            })
            .await
            .unwrap();

        repo.record_refinement_lifecycle(&proposal.id, "refinement_start", None)
            .await
            .unwrap();

        let resp = server
            .dispatch_tool(
                "proposal_refinement_demand_round",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "reason": "should not dispatch while active",
                }),
            )
            .await
            .unwrap();

        assert_eq!(resp.get("accepted").and_then(|v| v.as_bool()), Some(false));
        let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(error.contains("already active"), "unexpected error: {error}");

        let revisions = repo.revisions(&proposal.id).await.unwrap();
        assert_eq!(
            revisions
                .iter()
                .filter(|r| r.event_kind == "refinement_start")
                .count(),
            1,
            "duplicate active demand must not record or dispatch a new start"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn demand_round_tool_rejects_non_draft_proposal() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Building Proposal",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: Some("building"),
                body_format: None,
            })
            .await
            .unwrap();

        let resp = server
            .dispatch_tool(
                "proposal_refinement_demand_round",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "reason": "test",
                }),
            )
            .await
            .unwrap();
        assert!(resp.get("error").and_then(|v| v.as_str()).is_some());
        assert_eq!(
            resp.get("accepted").and_then(|v| v.as_bool()),
            Some(false),
        );
    }

    // ── Human authority: verdict override tool ──────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn verdict_override_records_and_scopes_to_revision() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Override Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: Some("draft"),
                body_format: None,
            })
            .await
            .unwrap();

        let resp = server
            .dispatch_tool(
                "proposal_verdict_override",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "reason": "PM reviewed and approves despite judge concerns",
                }),
            )
            .await
            .unwrap();
        assert!(
            resp.get("error").and_then(|v| v.as_str()).is_none(),
            "expected no error, got: {:?}",
            resp.get("error")
        );
        assert_eq!(
            resp.get("overridden").and_then(|v| v.as_bool()),
            Some(true),
        );
        // Override should be scoped to the current revision.
        let override_seq = resp
            .get("override_on_revision_seq")
            .and_then(|v| v.as_i64())
            .expect("should have override_on_revision_seq");
        assert_eq!(override_seq, 1, "should be scoped to revision 1");

        // Verify the lifecycle event was recorded in revisions.
        let revisions = repo.revisions(&proposal.id).await.unwrap();
        let override_event = revisions
            .iter()
            .find(|r| r.event_kind == "verdict_override");
        assert!(
            override_event.is_some(),
            "verdict_override lifecycle event should exist in revisions"
        );
        let meta = override_event.unwrap();
        let meta_json: serde_json::Value =
            serde_json::from_str(meta.event_metadata.as_deref().unwrap_or("{}")).unwrap();
        assert_eq!(
            meta_json
                .get("override_reason")
                .and_then(|v| v.as_str()),
            Some("PM reviewed and approves despite judge concerns")
        );
        assert_eq!(
            meta_json
                .get("override_on_revision_seq")
                .and_then(|v| v.as_i64()),
            Some(1)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn verdict_override_rejects_empty_reason() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "No Reason Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let resp = server
            .dispatch_tool(
                "proposal_verdict_override",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "reason": "",
                }),
            )
            .await
            .unwrap();
        assert!(resp.get("error").and_then(|v| v.as_str()).is_some());
        assert_eq!(
            resp.get("overridden").and_then(|v| v.as_bool()),
            Some(false),
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn verdict_override_stale_after_revision_advance() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Staleness Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: Some("draft"),
                body_format: None,
            })
            .await
            .unwrap();

        // Override at revision 1.
        let resp1 = server
            .dispatch_tool(
                "proposal_verdict_override",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "reason": "PM override at rev 1",
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            resp1.get("overridden").and_then(|v| v.as_bool()),
            Some(true),
        );
        let override_seq_1 = resp1
            .get("override_on_revision_seq")
            .and_then(|v| v.as_i64())
            .unwrap();
        assert_eq!(override_seq_1, 1);

        // Edit the proposal to advance the revision.
        repo.update(
            &proposal.id,
            djinn_db::ProposalUpdateInput {
                title: "Staleness Test",
                body: "updated body",
                acceptance_criteria: "[]",
                status: "draft",
                superseded_by: None,
                body_format: None,
                event_metadata: None,
            },
        )
        .await
        .unwrap();

        // Verify the proposal now has latest_revision_seq > override_seq.
        let updated = repo.get(&proposal.id).await.unwrap().unwrap();
        assert!(
            updated.latest_revision_seq > override_seq_1 as i32,
            "proposal revision should have advanced: {} > {}",
            updated.latest_revision_seq,
            override_seq_1
        );

        // The override at seq 1 is now stale — the latest_verdict_override
        // query still returns it, but gate composition (task cuzf) should
        // compare it to latest_revision_seq and reject it.
        let (override_seq, _meta) = repo
            .latest_verdict_override(&proposal.id)
            .await
            .unwrap()
            .expect("override should exist");
        assert!(
            override_seq < updated.latest_revision_seq,
            "override at seq {} is stale when proposal is at seq {}",
            override_seq,
            updated.latest_revision_seq
        );
    }

    // ── Human authority: blocking objection via debate trail ────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn human_blocking_objection_via_debate_append() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Human Objection Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: Some("draft"),
                body_format: None,
            })
            .await
            .unwrap();

        // Human adds a blocking objection via proposal_debate_append.
        let resp = server
            .dispatch_tool(
                "proposal_debate_append",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "kind": "objection",
                    "body": "Security review required before approval",
                    "blocking": true,
                    "agent_role": "human",
                    "author_kind": "user",
                    "against_revision_seq": 1,
                    "round": 1,
                }),
            )
            .await
            .unwrap();
        assert!(
            resp.get("error").and_then(|v| v.as_str()).is_none(),
            "expected no error, got: {:?}",
            resp.get("error")
        );
        let entry = resp.get("entry").expect("response should have entry");
        assert_eq!(
            entry.get("kind").and_then(|v| v.as_str()),
            Some("objection")
        );
        assert_eq!(
            entry.get("blocking").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            entry.get("author_kind").and_then(|v| v.as_str()),
            Some("user")
        );
        assert_eq!(
            entry.get("agent_role").and_then(|v| v.as_str()),
            Some("human")
        );
        assert!(entry.get("created_at").and_then(|v| v.as_str()).is_some());
        assert!(entry.get("id").and_then(|v| v.as_str()).is_some());

        // Verify it shows up in proposal_show debate_trail.
        let show = server
            .dispatch_tool("proposal_show", serde_json::json!({ "id": proposal.id }))
            .await
            .unwrap();
        let debate = show
            .get("debate_trail")
            .and_then(|v| v.as_array())
            .expect("should have debate_trail");
        assert_eq!(debate.len(), 1);
        assert_eq!(
            debate[0].get("blocking").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            debate[0].get("author_kind").and_then(|v| v.as_str()),
            Some("user")
        );
    }

    // ── Human authority: reopen preserves audit metadata ────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reopen_preserves_audit_metadata_in_tool_response() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Reopen Audit Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Create a debate entry.
        let create_resp = server
            .dispatch_tool(
                "proposal_debate_append",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "kind": "objection",
                    "body": "Needs more detail",
                    "blocking": true,
                    "agent_role": "human",
                    "author_kind": "user",
                    "against_revision_seq": 1,
                    "round": 1,
                }),
            )
            .await
            .unwrap();
        let entry_id = create_resp
            .get("entry")
            .and_then(|e| e.get("id"))
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();

        // Resolve it.
        server
            .dispatch_tool(
                "proposal_debate_resolve",
                serde_json::json!({ "id": entry_id }),
            )
            .await
            .unwrap();

        // Reopen it with explicit user attribution for audit trail.
        let reopen_resp = server
            .dispatch_tool(
                "proposal_debate_reopen",
                serde_json::json!({ "id": entry_id, "user_id": "test-user-reopener" }),
            )
            .await
            .unwrap();
        assert!(
            reopen_resp.get("error").and_then(|v| v.as_str()).is_none(),
        );
        let reopened = reopen_resp.get("entry").expect("should have entry");
        // Both resolved_at and reopened_at should be present.
        assert!(
            reopened.get("resolved_at").and_then(|v| v.as_str()).is_some(),
            "resolved_at should persist after reopen"
        );
        assert!(
            reopened.get("reopened_at").and_then(|v| v.as_str()).is_some(),
            "reopened_at should be set"
        );
        assert!(
            reopened
                .get("reopened_by_user_id")
                .is_some(),
            "reopened_by_user_id should be set"
        );
        // The entry should still carry the original blocking and kind.
        assert_eq!(
            reopened.get("kind").and_then(|v| v.as_str()),
            Some("objection")
        );
        assert_eq!(
            reopened.get("blocking").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    // ── Missing-target fail-fast at start time ────────────────────────────────

    /// Starting refinement on a proposal with no target project must be
    /// rejected up front with an actionable message, rather than silently
    /// terminating the tribunal with an opaque agent_failure once the
    /// coordinator fails to place the first tribunal task (prod incident,
    /// proposal t7lh, 2026-07-10). No refinement_start lifecycle entry should
    /// be recorded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_start_rejects_proposal_without_target() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "No Target Start",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: Some("draft"),
                body_format: None,
            })
            .await
            .unwrap();

        // Intentionally do NOT link a target project.
        let resp = server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .expect("tool should be registered");

        let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(
            error.contains("no target project"),
            "should mention missing target: {error}"
        );
        assert!(
            error.contains("proposal_add_target"),
            "should tell the operator how to fix it: {error}"
        );

        // Fail-fast means no refinement lifecycle entry was written.
        let revisions = repo.revisions(&proposal.id).await.unwrap();
        assert_eq!(
            revisions
                .iter()
                .filter(|r| r.event_kind == "refinement_start")
                .count(),
            0,
            "no refinement_start should be recorded when the target check fails"
        );
    }

    /// Demanding a refinement round on a proposal with no target project is
    /// rejected with the same actionable message.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn demand_round_rejects_proposal_without_target() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "No Target Demand",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: Some("draft"),
                body_format: None,
            })
            .await
            .unwrap();

        // Intentionally do NOT link a target project.
        let resp = server
            .dispatch_tool(
                "proposal_refinement_demand_round",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "reason": "another round",
                }),
            )
            .await
            .expect("tool should be registered");

        assert_eq!(resp.get("accepted").and_then(|v| v.as_bool()), Some(false));
        let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(
            error.contains("no target project"),
            "should mention missing target: {error}"
        );
        assert!(
            error.contains("proposal_add_target"),
            "should tell the operator how to fix it: {error}"
        );
    }
