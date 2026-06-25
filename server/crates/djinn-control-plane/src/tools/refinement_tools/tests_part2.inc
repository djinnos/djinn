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

    // ── Checkpoint vs auto-accept authority in status ─────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_checkpoint_authority_preserved_in_status() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Checkpoint Authority",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Start with checkpoint mode.
        server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "update_authority": "checkpoint",
                }),
            )
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
            refinement.get("update_authority").and_then(|v| v.as_str()),
            Some("checkpoint"),
            "checkpoint authority must be preserved"
        );
        assert_eq!(
            refinement.get("active").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    // ── Lifecycle attribution metadata ────────────────────────────────────────

    /// Refinement-start lifecycle entries carry the update_authority in
    /// event_metadata (always `checkpoint`); refinement-stop entries carry the
    /// stop_reason tag.
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

        // Start metadata carries the authority mode.
        let start_meta: serde_json::Value =
            serde_json::from_str(starts[0].event_metadata.as_deref().unwrap_or("{}")).unwrap();
        assert_eq!(
            start_meta.get("update_authority").and_then(|v| v.as_str()),
            Some("checkpoint"),
            "start metadata must include update_authority"
        );

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

    // ── Checkpoint list/approve/reject tests ────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn checkpoint_list_returns_empty_for_no_pending() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Empty Checkpoint List",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let resp = server
            .dispatch_tool(
                "proposal_refinement_checkpoint_list",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .expect("tool should be registered");

        assert!(
            resp.get("error").and_then(|v| v.as_str()).is_none(),
            "expected no error, got: {:?}",
            resp.get("error")
        );
        let pending = resp.get("pending").and_then(|v| v.as_array()).unwrap();
        assert!(pending.is_empty(), "expected no pending revisions");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn checkpoint_list_returns_pending_revisions() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Pending Checkpoint",
                body: "original body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Simulate a pending checkpoint revision by updating the proposal
        // (which creates a spec_revision) and then marking it as pending.
        repo.update(
            &proposal.id,
            djinn_db::ProposalUpdateInput {
                title: "Advocate Revised Title",
                body: "advocate revised body content",
                acceptance_criteria: "[]",
                status: "draft",
                superseded_by: None,
                body_format: None,
                event_metadata: None,
            },
        )
        .await
        .unwrap();

        // Mark the latest revision as checkpoint_pending.
        let updated = repo.get(&proposal.id).await.unwrap().unwrap();
        let meta = serde_json::json!({
            "source": "refinement_loop",
            "role": "advocate",
            "round": 1,
            "authority": "checkpoint",
            "checkpoint_status": "pending",
        });
        repo.set_latest_revision_event_metadata(&proposal.id, updated.latest_revision_seq, &meta)
            .await
            .unwrap();

        // Revert the live body to simulate the coordinator's checkpoint revert.
        // (In production the coordinator does this; here we do it manually.)

        let resp = server
            .dispatch_tool(
                "proposal_refinement_checkpoint_list",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .expect("tool should be registered");

        let pending = resp.get("pending").and_then(|v| v.as_array()).unwrap();
        assert_eq!(pending.len(), 1, "expected one pending revision");
        let first = &pending[0];
        assert_eq!(
            first.get("title").and_then(|v| v.as_str()),
            Some("Advocate Revised Title")
        );
        assert_eq!(first.get("role").and_then(|v| v.as_str()), Some("advocate"));
        assert_eq!(first.get("round").and_then(|v| v.as_i64()), Some(1));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn checkpoint_approve_applies_pending_revision() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Original Title",
                body: "original body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Simulate advocate revision + pending marking.
        repo.update(
            &proposal.id,
            djinn_db::ProposalUpdateInput {
                title: "Advocate Title",
                body: "advocate body",
                acceptance_criteria: "[]",
                status: "draft",
                superseded_by: None,
                body_format: None,
                event_metadata: None,
            },
        )
        .await
        .unwrap();

        let updated = repo.get(&proposal.id).await.unwrap().unwrap();
        let pending_seq = updated.latest_revision_seq;
        let meta = serde_json::json!({
            "source": "refinement_loop",
            "role": "advocate",
            "round": 1,
            "authority": "checkpoint",
            "checkpoint_status": "pending",
        });
        repo.set_latest_revision_event_metadata(&proposal.id, pending_seq, &meta)
            .await
            .unwrap();

        // Approve the pending revision.
        let resp = server
            .dispatch_tool(
                "proposal_refinement_checkpoint_approve",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "revision_seq": pending_seq,
                }),
            )
            .await
            .expect("tool should be registered");

        assert!(
            resp.get("error").and_then(|v| v.as_str()).is_none(),
            "expected no error, got: {:?}",
            resp.get("error")
        );
        assert_eq!(resp.get("approved").and_then(|v| v.as_bool()), Some(true));

        // The live proposal should now have the advocate's body.
        let final_proposal = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(final_proposal.title, "Advocate Title");
        assert_eq!(final_proposal.body, "advocate body");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn checkpoint_reject_leaves_live_body_unchanged() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Original Title",
                body: "original body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Simulate advocate revision + pending marking + revert.
        repo.update(
            &proposal.id,
            djinn_db::ProposalUpdateInput {
                title: "Advocate Title",
                body: "advocate body",
                acceptance_criteria: "[]",
                status: "draft",
                superseded_by: None,
                body_format: None,
                event_metadata: None,
            },
        )
        .await
        .unwrap();

        let updated = repo.get(&proposal.id).await.unwrap().unwrap();
        let pending_seq = updated.latest_revision_seq;
        let meta = serde_json::json!({
            "source": "refinement_loop",
            "role": "advocate",
            "round": 1,
            "authority": "checkpoint",
            "checkpoint_status": "pending",
        });
        repo.set_latest_revision_event_metadata(&proposal.id, pending_seq, &meta)
            .await
            .unwrap();

        // Reject the pending revision.
        let resp = server
            .dispatch_tool(
                "proposal_refinement_checkpoint_reject",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "revision_seq": pending_seq,
                }),
            )
            .await
            .expect("tool should be registered");

        assert!(
            resp.get("error").and_then(|v| v.as_str()).is_none(),
            "expected no error, got: {:?}",
            resp.get("error")
        );
        assert_eq!(resp.get("rejected").and_then(|v| v.as_bool()), Some(true));

        // The live proposal body is unchanged (reject doesn't modify it).
        let final_proposal = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(final_proposal.body, "advocate body");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn checkpoint_approve_is_idempotent() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Idempotent Approve",
                body: "original body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Create a pending revision.
        repo.update(
            &proposal.id,
            djinn_db::ProposalUpdateInput {
                title: "Pending Title",
                body: "pending body",
                acceptance_criteria: "[]",
                status: "draft",
                superseded_by: None,
                body_format: None,
                event_metadata: None,
            },
        )
        .await
        .unwrap();

        let updated = repo.get(&proposal.id).await.unwrap().unwrap();
        let seq = updated.latest_revision_seq;
        let meta = serde_json::json!({
            "checkpoint_status": "pending",
            "role": "advocate",
            "round": 1,
            "authority": "checkpoint",
        });
        repo.set_latest_revision_event_metadata(&proposal.id, seq, &meta)
            .await
            .unwrap();

        // First approve.
        let resp1 = server
            .dispatch_tool(
                "proposal_refinement_checkpoint_approve",
                serde_json::json!({ "proposal_id": proposal.id, "revision_seq": seq }),
            )
            .await
            .unwrap();
        assert_eq!(resp1.get("approved").and_then(|v| v.as_bool()), Some(true));

        // Second approve (idempotent — the revision is no longer pending).
        let resp2 = server
            .dispatch_tool(
                "proposal_refinement_checkpoint_approve",
                serde_json::json!({ "proposal_id": proposal.id, "revision_seq": seq }),
            )
            .await
            .unwrap();
        assert_eq!(resp2.get("approved").and_then(|v| v.as_bool()), Some(true));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn checkpoint_reject_is_idempotent() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Idempotent Reject",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        repo.update(
            &proposal.id,
            djinn_db::ProposalUpdateInput {
                title: "Advocate Title",
                body: "advocate body",
                acceptance_criteria: "[]",
                status: "draft",
                superseded_by: None,
                body_format: None,
                event_metadata: None,
            },
        )
        .await
        .unwrap();

        let updated = repo.get(&proposal.id).await.unwrap().unwrap();
        let seq = updated.latest_revision_seq;
        let meta = serde_json::json!({
            "checkpoint_status": "pending",
            "role": "advocate",
            "round": 1,
            "authority": "checkpoint",
        });
        repo.set_latest_revision_event_metadata(&proposal.id, seq, &meta)
            .await
            .unwrap();

        // First reject.
        let resp1 = server
            .dispatch_tool(
                "proposal_refinement_checkpoint_reject",
                serde_json::json!({ "proposal_id": proposal.id, "revision_seq": seq }),
            )
            .await
            .unwrap();
        assert_eq!(resp1.get("rejected").and_then(|v| v.as_bool()), Some(true));

        // Second reject (idempotent — no longer pending).
        let resp2 = server
            .dispatch_tool(
                "proposal_refinement_checkpoint_reject",
                serde_json::json!({ "proposal_id": proposal.id, "revision_seq": seq }),
            )
            .await
            .unwrap();
        assert_eq!(resp2.get("rejected").and_then(|v| v.as_bool()), Some(true));
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
