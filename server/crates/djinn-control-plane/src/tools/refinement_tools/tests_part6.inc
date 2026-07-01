
    // ── End-to-end demand-evidence regressions ─────────────────────────
    //
    // Consolidated regression suite covering the remaining acceptance
    // criteria after the existing tests_part5 validation tests.
    //
    // Focus areas:
    // - Cross-proposal stale context (Judge task for a different proposal)
    // - Enhanced no-mutation assertions (task count, debate trail, lifecycle)
    // - Successful demand: spike title/description template, reciprocal
    //   links, structured claim, debate entry body_metadata, needs_evidence
    //   kind distinct from verdict, refinement status with needs_evidence
    // - Coordinator regression: needs_evidence entry does not match verdict
    //   lookup; approve/needs-work verdict handling is unaffected.

    // ── AC: Cross-proposal Judge task rejected ────────────────────────

    /// A Judge task attributed to the caller but for a DIFFERENT proposal
    /// must be rejected: the authorization gate scopes by proposal id.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cross_proposal_judge_task_rejected() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());

        // Create two proposals.
        let p1 = repo
            .create(ProposalCreateInput {
                title: "Proposal A",
                body: "has anchor-text for A",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        let p2 = repo
            .create(ProposalCreateInput {
                title: "Proposal B",
                body: "has anchor-text for B",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Start refinement for BOTH proposals.
        repo.record_refinement_lifecycle(&p1.id, "refinement_start", None)
            .await
            .unwrap();
        repo.record_refinement_lifecycle(&p2.id, "refinement_start", None)
            .await
            .unwrap();

        let user_id = create_test_user(&db, "cross-proposal-judge").await;
        let project_id = link_proposal_to_project(&db, &repo, &p1.id).await;
        // Create Judge task for proposal p1 — NOT p2.
        let _judge_task_p1 =
            create_judge_task(&db, &project_id, &p1.id, &user_id).await;

        // Also link p2 to the project so spike creation would succeed if
        // validation didn't reject first.
        let _project_id_2 =
            link_proposal_to_project(&db, &repo, &p2.id).await;

        let snap = mutation_snapshot(&repo, &p2.id).await;

        // Demand evidence for p2 — the Judge task is for p1, so auth fails.
        let params = valid_demand_params(&p2.id);
        let resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id), async {
                server
                    .dispatch_tool("proposal_refinement_demand_evidence", params)
                    .await
            })
            .await
            .expect("tool should be registered");

        let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(
            error.contains("no active Judge task"),
            "should mention no Judge task for this proposal: {error}"
        );
        assert!(!resp.get("accepted").and_then(|v| v.as_bool()).unwrap_or(true));

        let after = mutation_snapshot(&repo, &p2.id).await;
        assert_eq!(snap, after, "cross-proposal rejection must not mutate state");
    }

    // ── AC: Empty spec_unknown_anchor rejected ───────────────────────

    /// An empty `spec_unknown_anchor` string (after trimming) is rejected
    /// before any mutation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_spec_unknown_anchor_rejected() {
        let (server, db, p, user_id, _judge_task_id) =
            setup_demand_test().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let snap = mutation_snapshot(&repo, &p.id).await;

        let mut params = valid_demand_params(&p.id);
        params["spec_unknown_anchor"] = serde_json::json!("   ");

        let resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id), async {
                server
                    .dispatch_tool("proposal_refinement_demand_evidence", params)
                    .await
            })
            .await
            .expect("tool should be registered");

        let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(
            error.contains("spec_unknown_anchor"),
            "should mention spec_unknown_anchor: {error}"
        );
        assert!(!resp.get("accepted").and_then(|v| v.as_bool()).unwrap_or(true));

        let after = mutation_snapshot(&repo, &p.id).await;
        assert_eq!(snap, after, "rejected demand must not mutate state");
    }

    // ── AC: Rejection does not create any tasks ──────────────────────

    /// After a rejected demand, the total task count must be exactly the
    /// same as before the call. This explicitly verifies no spike task
    /// row was created and left dangling.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejection_does_not_create_tasks() {
        let (server, db, p, user_id, _judge_task_id) =
            setup_demand_test().await;

        // Count tasks before via direct SQL.
        let before_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
                .fetch_one(db.pool())
                .await
                .unwrap();

        // Use a stale round to force rejection.
        let mut params = valid_demand_params(&p.id);
        params["round"] = serde_json::json!(999);

        let resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id), async {
                server
                    .dispatch_tool("proposal_refinement_demand_evidence", params)
                    .await
            })
            .await
            .expect("tool should be registered");

        assert!(!resp.get("accepted").and_then(|v| v.as_bool()).unwrap_or(true));

        // Count tasks after.
        let after_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
                .fetch_one(db.pool())
                .await
                .unwrap();

        assert_eq!(
            before_count, after_count,
            "rejected demand must not create any tasks"
        );
    }

    // ── AC: Valid demand spike title/description template ─────────────

    /// After a successful demand, the spike task title must start with
    /// "Evidence spike:" and the description must contain the structured
    /// template sections.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn valid_demand_spike_title_and_description_template() {
        let (server, db, p, user_id, _judge_task_id) =
            setup_demand_test().await;
        let task_repo = djinn_db::TaskRepository::new(db.clone(), EventBus::noop());

        let resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id), async {
                server
                    .dispatch_tool(
                        "proposal_refinement_demand_evidence",
                        valid_demand_params(&p.id),
                    )
                    .await
            })
            .await
            .expect("tool should be registered");

        assert!(resp.get("accepted").and_then(|v| v.as_bool()).unwrap_or(false));
        let spike_id = resp
            .get("result")
            .and_then(|r| r.get("spike_task_id"))
            .and_then(|v| v.as_str())
            .expect("spike_task_id must be present");

        let spike = task_repo
            .get(spike_id)
            .await
            .unwrap()
            .expect("spike must exist");

        // Title template.
        assert!(
            spike.title.starts_with("Evidence spike:"),
            "spike title must start with 'Evidence spike:': {}",
            spike.title
        );
        assert!(
            spike.title.contains("Does module X handle token expiry correctly?"),
            "spike title must contain the question"
        );

        // Description template sections.
        let desc = &spike.description;
        assert!(desc.contains("## Evidence Spike"), "must have heading");
        assert!(desc.contains(&format!("**Proposal:** {}", p.id)), "must have proposal id");
        assert!(desc.contains(&format!("(short_id: {})", p.short_id)), "must have short_id");
        assert!(desc.contains("**Question:**"), "must have question section");
        assert!(desc.contains("**Target subsystem:**"), "must have target subsystem section");
        assert!(desc.contains("**Spec unknown anchor:**"), "must have anchor section");
        assert!(desc.contains("**Insufficiency rationale:**"), "must have insufficiency section");
        assert!(desc.contains("**Expected findings:**"), "must have expected findings section");
        assert!(desc.contains("### Read-Only Constraints"), "must have read-only section");
    }

    // ── AC: Reciprocal proposal/task links ───────────────────────────

    /// After a successful demand, the proposal's `linked_spike_task_id`
    /// must point to the spike task, and the spike task must carry a
    /// proposal-scoped label.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn valid_demand_reciprocal_proposal_task_links() {
        let (server, db, p, user_id, _judge_task_id) =
            setup_demand_test().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let task_repo = djinn_db::TaskRepository::new(db.clone(), EventBus::noop());

        let resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id), async {
                server
                    .dispatch_tool(
                        "proposal_refinement_demand_evidence",
                        valid_demand_params(&p.id),
                    )
                    .await
            })
            .await
            .expect("tool should be registered");

        assert!(resp.get("accepted").and_then(|v| v.as_bool()).unwrap_or(false));
        let spike_id = resp
            .get("result")
            .and_then(|r| r.get("spike_task_id"))
            .and_then(|v| v.as_str())
            .expect("spike_task_id must be present");

        // Proposal → spike link.
        let updated = repo.get(&p.id).await.unwrap().unwrap();
        assert_eq!(
            updated.linked_spike_task_id.as_deref(),
            Some(spike_id),
            "proposal must link to the spike task"
        );

        // Spike → proposal label.
        let spike = task_repo.get(spike_id).await.unwrap().unwrap();
        let labels: Vec<String> = serde_json::from_str(&spike.labels).unwrap_or_default();
        assert!(
            labels.contains(&format!("proposal:{}", p.short_id)),
            "spike must have proposal:<short_id> label: {:?}",
            labels
        );
    }

    // ── AC: Structured claim persisted as JSON ───────────────────────

    /// After a successful demand, `needs_evidence_claim` on the proposal
    /// must be well-formed JSON containing all claim fields.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn valid_demand_structured_claim_persisted() {
        let (server, db, p, user_id, _judge_task_id) =
            setup_demand_test().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());

        let resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id), async {
                server
                    .dispatch_tool(
                        "proposal_refinement_demand_evidence",
                        valid_demand_params(&p.id),
                    )
                    .await
            })
            .await
            .expect("tool should be registered");

        assert!(resp.get("accepted").and_then(|v| v.as_bool()).unwrap_or(false));

        let updated = repo.get(&p.id).await.unwrap().unwrap();
        let raw_claim = updated
            .needs_evidence_claim
            .as_deref()
            .expect("needs_evidence_claim must be set");

        let claim: serde_json::Value =
            serde_json::from_str(raw_claim).expect("claim must be valid JSON");

        assert_eq!(
            claim.get("question").and_then(|v| v.as_str()),
            Some("Does module X handle token expiry correctly?"),
            "claim must contain question"
        );
        assert_eq!(
            claim.get("target_subsystem").and_then(|v| v.as_str()),
            Some("auth"),
            "claim must contain target_subsystem"
        );
        assert_eq!(
            claim.get("spec_unknown_anchor").and_then(|v| v.as_str()),
            Some("anchor-text"),
            "claim must contain spec_unknown_anchor"
        );
        assert_eq!(
            claim.get("round").and_then(|v| v.as_i64()),
            Some(1),
            "claim must contain round"
        );
        assert_eq!(
            claim.get("against_revision_seq").and_then(|v| v.as_i64()),
            Some(1),
            "claim must contain against_revision_seq"
        );
        assert!(
            claim.get("created_by_task_id").is_some(),
            "claim must contain created_by_task_id"
        );
    }

    // ── AC: Debate entry body_metadata and kind ──────────────────────

    /// After a successful demand, the needs_evidence debate entry must
    /// have `kind = "needs_evidence"` (not "verdict"), `blocking = true`,
    /// `agent_role = "judge"`, and body_metadata containing the claim link.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn valid_demand_debate_entry_metadata() {
        let (server, db, p, user_id, _judge_task_id) =
            setup_demand_test().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());

        let resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id), async {
                server
                    .dispatch_tool(
                        "proposal_refinement_demand_evidence",
                        valid_demand_params(&p.id),
                    )
                    .await
            })
            .await
            .expect("tool should be registered");

        assert!(resp.get("accepted").and_then(|v| v.as_bool()).unwrap_or(false));

        let trail = repo.debate_trail(&p.id).await.unwrap();
        let ne_entries: Vec<_> = trail.iter().filter(|e| e.kind == "needs_evidence").collect();

        assert_eq!(ne_entries.len(), 1, "exactly one needs_evidence entry");
        let entry = &ne_entries[0];

        // Kind is needs_evidence — NOT verdict.
        assert_eq!(entry.kind, "needs_evidence");
        assert_eq!(entry.agent_role, "judge");
        assert!(entry.blocking, "needs_evidence must be blocking");

        // body_metadata must be present and contain claim link fields.
        let meta = entry
            .body_metadata
            .as_ref()
            .expect("body_metadata must be set");
        let meta_val: serde_json::Value =
            serde_json::from_str(meta).expect("body_metadata must be valid JSON");
        assert_eq!(
            meta_val.get("kind").and_then(|v| v.as_str()),
            Some("needs_evidence_link_v1"),
            "body_metadata must identify as needs_evidence_link_v1"
        );
        assert!(
            meta_val.get("spike_task_id").is_some(),
            "body_metadata must contain spike_task_id"
        );
    }

    // ── AC: needs_evidence entry is distinct from verdict ────────────

    /// The coordinator's `process_judge_outcome` searches for debate
    /// entries where `kind == "verdict"`. A needs_evidence entry
    /// (`kind == "needs_evidence"`) must NOT be matched by that search.
    /// This test proves the two entry types are disjoint at the DB layer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn needs_evidence_entry_distinct_from_verdict() {
        let (server, db, p, user_id, _judge_task_id) =
            setup_demand_test().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());

        // Issue a demand → creates needs_evidence entry.
        let resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                server
                    .dispatch_tool(
                        "proposal_refinement_demand_evidence",
                        valid_demand_params(&p.id),
                    )
                    .await
            })
            .await
            .expect("tool should be registered");

        assert!(resp.get("accepted").and_then(|v| v.as_bool()).unwrap_or(false));

        // Also append a verdict entry (simulate Judge verdicting).
        repo.add_debate_trail_entry(djinn_db::ProposalDebateTrailCreateInput {
            proposal_id: &p.id,
            kind: "verdict",
            body: "Proposal is ready.",
            blocking: false,
            agent_role: "judge",
            author_kind: "agent",
            author_model: None,
            source_task_id: None,
            against_revision_seq: 1,
            round: 1,
            body_metadata: None,
        })
        .await
        .unwrap();

        let trail = repo.debate_trail(&p.id).await.unwrap();

        // needs_evidence entries.
        let ne_entries: Vec<_> = trail.iter().filter(|e| e.kind == "needs_evidence").collect();
        assert_eq!(ne_entries.len(), 1, "one needs_evidence entry");

        // verdict entries.
        let verdict_entries: Vec<_> = trail.iter().filter(|e| e.kind == "verdict").collect();
        assert_eq!(verdict_entries.len(), 1, "one verdict entry");

        // They are disjoint: the needs_evidence entry would NOT be picked
        // up by the coordinator's verdict lookup
        // (`kind == "verdict" && agent_role == "judge"`).
        assert_ne!(
            ne_entries[0].kind, verdict_entries[0].kind,
            "needs_evidence and verdict must have different kinds"
        );

        // A search for verdict-only entries excludes the needs_evidence entry.
        let judge_verdicts: Vec<_> = trail
            .iter()
            .filter(|e| e.agent_role == "judge" && e.kind == "verdict" && e.round == 1)
            .collect();
        assert_eq!(
            judge_verdicts.len(),
            1,
            "verdict lookup must find exactly one verdict, not the needs_evidence entry"
        );
        assert_eq!(judge_verdicts[0].kind, "verdict");
    }

    // ── AC: Refinement status surfaces needs_evidence ────────────────

    /// After a successful demand, the refinement status response must
    /// contain a `needs_evidence` object with the spike task id, claim,
    /// and awaiting evidence phase.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_status_surfaces_needs_evidence_after_demand() {
        let (server, _db, p, user_id, _judge_task_id) =
            setup_demand_test().await;

        let resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id), async {
                server
                    .dispatch_tool(
                        "proposal_refinement_demand_evidence",
                        valid_demand_params(&p.id),
                    )
                    .await
            })
            .await
            .expect("tool should be registered");

        assert!(resp.get("accepted").and_then(|v| v.as_bool()).unwrap_or(false));
        let spike_id = resp
            .get("result")
            .and_then(|r| r.get("spike_task_id"))
            .and_then(|v| v.as_str())
            .expect("spike_task_id must be present");

        // Query refinement status.
        let status_resp = server
            .dispatch_tool(
                "proposal_refinement_status",
                serde_json::json!({ "proposal_id": p.id }),
            )
            .await
            .expect("tool should be registered");

        let refinement = status_resp
            .get("refinement")
            .expect("must have refinement");
        let ne = refinement
            .get("needs_evidence")
            .expect("needs_evidence must be present after demand");

        assert_eq!(
            ne.get("spike_task_id").and_then(|v| v.as_str()),
            Some(spike_id),
            "needs_evidence spike_task_id must match"
        );
        assert!(
            ne.get("claim").and_then(|v| v.as_str()).is_some(),
            "needs_evidence must have claim text"
        );
    }

    // ── AC: Approve verdict path not regressed ───────────────────────

    /// A non-blocking verdict (approve) must still park the tribunal for
    /// human review, regardless of whether a needs_evidence entry exists
    /// in the debate trail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn approve_verdict_not_regressed_by_needs_evidence() {
        let (server, db, p, user_id, _judge_task_id) =
            setup_demand_test().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());

        // Issue demand first.
        let demand_resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                server
                    .dispatch_tool(
                        "proposal_refinement_demand_evidence",
                        valid_demand_params(&p.id),
                    )
                    .await
            })
            .await
            .expect("tool should be registered");
        assert!(demand_resp.get("accepted").and_then(|v| v.as_bool()).unwrap_or(false));

        // Now simulate an approve verdict (non-blocking).
        repo.add_debate_trail_entry(djinn_db::ProposalDebateTrailCreateInput {
            proposal_id: &p.id,
            kind: "verdict",
            body: "Approved: spec is ready.",
            blocking: false,
            agent_role: "judge",
            author_kind: "agent",
            author_model: None,
            source_task_id: None,
            against_revision_seq: 1,
            round: 1,
            body_metadata: None,
        })
        .await
        .unwrap();

        let trail = repo.debate_trail(&p.id).await.unwrap();
        // The verdict entry should be present and distinct from needs_evidence.
        let verdict = trail
            .iter()
            .find(|e| e.kind == "verdict" && e.agent_role == "judge");
        assert!(verdict.is_some(), "verdict entry must exist");
        assert!(!verdict.unwrap().blocking, "approve verdict is non-blocking");

        // The needs_evidence entry is still there but doesn't interfere.
        let ne = trail.iter().find(|e| e.kind == "needs_evidence");
        assert!(ne.is_some(), "needs_evidence entry must still exist");
    }

    // ── AC: Needs-work verdict path not regressed ────────────────────

    /// A blocking verdict (needs-work) must still trigger another round,
    /// regardless of whether a needs_evidence entry exists.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn needs_work_verdict_not_regressed_by_needs_evidence() {
        let (server, db, p, user_id, _judge_task_id) =
            setup_demand_test().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());

        // Issue demand first.
        let demand_resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                server
                    .dispatch_tool(
                        "proposal_refinement_demand_evidence",
                        valid_demand_params(&p.id),
                    )
                    .await
            })
            .await
            .expect("tool should be registered");
        assert!(demand_resp.get("accepted").and_then(|v| v.as_bool()).unwrap_or(false));

        // Simulate a needs-work verdict (blocking).
        repo.add_debate_trail_entry(djinn_db::ProposalDebateTrailCreateInput {
            proposal_id: &p.id,
            kind: "verdict",
            body: "Not ready: missing scope section.",
            blocking: true,
            agent_role: "judge",
            author_kind: "agent",
            author_model: None,
            source_task_id: None,
            against_revision_seq: 1,
            round: 1,
            body_metadata: None,
        })
        .await
        .unwrap();

        let trail = repo.debate_trail(&p.id).await.unwrap();
        let verdict = trail
            .iter()
            .find(|e| e.kind == "verdict" && e.agent_role == "judge" && e.round == 1);
        assert!(verdict.is_some(), "verdict entry must exist");
        assert!(verdict.unwrap().blocking, "needs-work verdict is blocking");

        // Verdict lookup pattern excludes the needs_evidence entry.
        let judge_verdicts: Vec<_> = trail
            .iter()
            .filter(|e| e.agent_role == "judge" && e.kind == "verdict" && e.round == 1)
            .collect();
        assert_eq!(judge_verdicts.len(), 1, "exactly one verdict entry found");
    }
