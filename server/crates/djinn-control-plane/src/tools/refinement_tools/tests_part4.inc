
    // ── Structured claim and evidence lifecycle status tests ──────────────

    use djinn_core::models::{EvidenceFindings, NeedsEvidenceClaim};

    /// Helper: insert a project row and return its id.
    async fn insert_project(db: &Database, name: &str) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
        )
        .bind(&id)
        .bind(name)
        .bind(name)
        .bind(format!("repo-{}", &id.replace('-', "")[..31]))
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    /// Helper: insert an epic row and return its id.
    async fn insert_epic(db: &Database, project_id: &str, short_id: &str) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, status, owner, memory_refs, auto_breakdown) VALUES ($1, $2, $3, 'T', '', '', '', 'open', '', '[]'::jsonb, true)",
        )
        .bind(&id)
        .bind(project_id)
        .bind(short_id)
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    /// Helper: insert a task row and return its id.
    async fn insert_task(db: &Database, project_id: &str, epic_id: &str, short_id: &str) -> String {
        let creator_uuid = uuid::Uuid::now_v7();
        let creator_id = creator_uuid.to_string();
        let github_id = (creator_uuid.as_u128() & i64::MAX as u128) as i64;
        sqlx::query("INSERT INTO users (id, github_id, github_login) VALUES ($1, $2, $3)")
            .bind(&creator_id)
            .bind(github_id)
            .bind(format!("fixture-{creator_id}"))
            .execute(db.pool())
            .await
            .unwrap();

        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design, issue_type, priority, owner, status, continuation_count, labels, acceptance_criteria, memory_refs, created_by_user_id) VALUES ($1, $2, $3, $4, 'T', '', '', 'task', 0, '', 'open', 0, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $5)",
        )
        .bind(&id)
        .bind(project_id)
        .bind(short_id)
        .bind(epic_id)
        .bind(&creator_id)
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    /// Helper: create a proposal, start refinement, set a structured
    /// needs-evidence spike with a real task, and return (server, db, proposal).
    async fn setup_structured_claim() -> (DjinnMcpServer, Database, djinn_core::models::Proposal) {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo
            .create(ProposalCreateInput {
                title: "Structured Claim Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Start refinement.
        repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
            .await
            .unwrap();

        // Create a real task to satisfy the FK constraint on linked_spike_task_id.
        let proj = insert_project(&db, "svc-test").await;
        let epic = insert_epic(&db, &proj, "sp01").await;
        let spike_id = insert_task(&db, &proj, &epic, "spike-01").await;

        let claim = NeedsEvidenceClaim {
            question: "Is module X load-bearing?".to_owned(),
            target_subsystem: "auth".to_owned(),
            spec_unknown_anchor: "Section 3.2 token refresh".to_owned(),
            insufficient_in_session_research: "No auth tests found".to_owned(),
            expected_findings: "Evidence that token refresh is or is not required".to_owned(),
            round: 1,
            against_revision_seq: 2,
            created_by_task_id: "judge-task-001".to_owned(),
        };
        repo.set_structured_needs_evidence_spike(&p.id, &spike_id, &claim)
            .await
            .unwrap();

        let updated = repo.get(&p.id).await.unwrap().unwrap();
        (server, db, updated)
    }

    // ── AC#1: Structured claim fields are exposed ────────────────────────

    /// `build_refinement_status` surfaces structured claim fields (question,
    /// target_subsystem, round, revision, created_by_task_id) when the
    /// persisted `needs_evidence_claim` is well-formed JSON.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn structured_claim_fields_exposed_in_status() {
        let (_server, db, p) = setup_structured_claim().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let status = build_refinement_status(&repo, &p.id).await.unwrap();

        let ne = status.needs_evidence.as_ref()
            .expect("needs_evidence must be present");
        // claim is the question from the structured claim.
        assert_eq!(ne.claim, "Is module X load-bearing?");
        // Structured fields are populated.
        assert_eq!(ne.question.as_deref(), Some("Is module X load-bearing?"));
        assert_eq!(ne.target_subsystem.as_deref(), Some("auth"));
        assert_eq!(ne.spec_unknown_anchor.as_deref(), Some("Section 3.2 token refresh"));
        assert_eq!(ne.round, Some(1));
        assert_eq!(ne.against_revision_seq, Some(2));
        assert_eq!(ne.created_by_task_id.as_deref(), Some("judge-task-001"));
        // Spike task id is propagated.
        assert!(!ne.spike_task_id.is_empty());
    }

    // ── AC#2: Legacy plain-string claim is tolerated ─────────────────────

    /// `build_refinement_status` does not panic on a legacy plain-string
    /// `needs_evidence_claim` and surfaces it as the `claim` text.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn legacy_plain_string_claim_tolerated() {
        let (_server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo
            .create(ProposalCreateInput {
                title: "Legacy Claim Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Start refinement.
        repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
            .await
            .unwrap();

        // Create a real task to satisfy the FK constraint.
        let proj = insert_project(&db, "svc-legacy").await;
        let epic = insert_epic(&db, &proj, "lg01").await;
        let spike_id = insert_task(&db, &proj, &epic, "spike-lg").await;

        // Use the legacy plain-string setter.
        repo.set_needs_evidence_spike(&p.id, &spike_id, "X is load-bearing")
            .await
            .unwrap();

        let status = build_refinement_status(&repo, &p.id).await.unwrap();
        let ne = status.needs_evidence.as_ref()
            .expect("needs_evidence must be present");
        // The raw string is preserved as the claim.
        assert_eq!(ne.claim, "X is load-bearing");
        // Structured fields are None for a legacy claim.
        assert!(ne.question.is_none());
        assert!(ne.target_subsystem.is_none());
        assert!(ne.spec_unknown_anchor.is_none());
        assert!(ne.round.is_none());
        assert!(ne.against_revision_seq.is_none());
        assert!(ne.created_by_task_id.is_none());
    }

    // ── AC#3a: Awaiting-evidence lifecycle phase ─────────────────────────

    /// After `record_awaiting_evidence_started`, the status shows
    /// `evidence_phase = AwaitingEvidence`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn awaiting_evidence_lifecycle_phase() {
        let (_server, db, p) = setup_structured_claim().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());

        // Record the awaiting lifecycle event.
        let spike_id = p.linked_spike_task_id.as_ref().unwrap();
        repo.record_awaiting_evidence_started(
            &p.id, spike_id, "judge-task-001", 1, 2,
        )
        .await
        .unwrap();

        let status = build_refinement_status(&repo, &p.id).await.unwrap();
        let ne = status.needs_evidence.as_ref().unwrap();
        assert_eq!(
            ne.evidence_phase,
            Some(crate::tools::proposal_ops::EvidenceLifecyclePhase::AwaitingEvidence)
        );
        assert!(ne.failure_reason.is_none());
    }

    // ── AC#3b: Evidence-received lifecycle phase ─────────────────────────

    /// After `record_evidence_received`, the status shows
    /// `evidence_phase = EvidenceReceived`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn evidence_received_lifecycle_phase() {
        let (_server, db, p) = setup_structured_claim().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let spike_id = p.linked_spike_task_id.as_ref().unwrap();

        // Record awaiting → received lifecycle.
        repo.record_awaiting_evidence_started(
            &p.id, spike_id, "judge-task-001", 1, 2,
        )
        .await
        .unwrap();
        repo.record_evidence_received(
            &p.id, spike_id, "judge-task-001", 1, 2,
        )
        .await
        .unwrap();

        let status = build_refinement_status(&repo, &p.id).await.unwrap();
        let ne = status.needs_evidence.as_ref().unwrap();
        assert_eq!(
            ne.evidence_phase,
            Some(crate::tools::proposal_ops::EvidenceLifecyclePhase::EvidenceReceived)
        );
        assert!(ne.failure_reason.is_none());
    }

    // ── AC#3c: Evidence-failed lifecycle phase ───────────────────────────

    /// After `record_evidence_failed`, the status shows
    /// `evidence_phase = EvidenceFailed` and the failure reason.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn evidence_failed_lifecycle_phase() {
        let (_server, db, p) = setup_structured_claim().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let spike_id = p.linked_spike_task_id.as_ref().unwrap();

        // Record awaiting → failed lifecycle.
        repo.record_awaiting_evidence_started(
            &p.id, spike_id, "judge-task-001", 1, 2,
        )
        .await
        .unwrap();
        repo.record_evidence_failed(
            &p.id, spike_id, "judge-task-001", 1, 2, "spike_cancelled",
        )
        .await
        .unwrap();

        let status = build_refinement_status(&repo, &p.id).await.unwrap();
        let ne = status.needs_evidence.as_ref().unwrap();
        assert_eq!(
            ne.evidence_phase,
            Some(crate::tools::proposal_ops::EvidenceLifecyclePhase::EvidenceFailed)
        );
        assert_eq!(ne.failure_reason.as_deref(), Some("spike_cancelled"));
    }

    // ── AC#3d: Latest lifecycle event wins ───────────────────────────────

    /// When multiple lifecycle events exist, the latest one determines the
    /// phase (evidence_failed after evidence_received → failed).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn latest_lifecycle_event_wins() {
        let (_server, db, p) = setup_structured_claim().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let spike_id = p.linked_spike_task_id.as_ref().unwrap();

        // Full lifecycle: awaiting → received → failed.
        repo.record_awaiting_evidence_started(
            &p.id, spike_id, "judge-task-001", 1, 2,
        )
        .await
        .unwrap();
        repo.record_evidence_received(
            &p.id, spike_id, "judge-task-001", 1, 2,
        )
        .await
        .unwrap();
        repo.record_evidence_failed(
            &p.id, spike_id, "judge-task-001", 1, 2, "spike_errored",
        )
        .await
        .unwrap();

        let status = build_refinement_status(&repo, &p.id).await.unwrap();
        let ne = status.needs_evidence.as_ref().unwrap();
        assert_eq!(
            ne.evidence_phase,
            Some(crate::tools::proposal_ops::EvidenceLifecyclePhase::EvidenceFailed)
        );
        assert_eq!(ne.failure_reason.as_deref(), Some("spike_errored"));
    }

    // ── AC#4a: evidence_findings schema from control-plane boundary ──────

    /// The `EvidenceFindings` schema (from djinn-core) is usable from the
    /// control-plane layer: it validates, serializes, and round-trips through
    /// a persisted debate entry.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn evidence_findings_schema_usable_from_control_plane() {
        let (_server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo
            .create(ProposalCreateInput {
                title: "Findings Schema Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let findings = EvidenceFindings {
            answer: "Module X is load-bearing; removing it breaks auth".to_owned(),
            evidence: vec![
                "grep -r 'token_refresh' auth/ shows 3 call sites".to_owned(),
            ],
            code_paths_inspected: vec![
                "auth/src/token.rs".to_owned(),
                "auth/src/refresh.rs".to_owned(),
            ],
            confidence: 0.95,
            residual_risks: vec!["Untested edge case in token expiry".to_owned()],
            recommendation_for_advocate: "Add integration test for token expiry edge case".to_owned(),
        };
        findings.validate().expect("findings must validate");

        let meta = serde_json::to_value(&findings).unwrap();
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &p.id,
            kind: "evidence_findings",
            body: &findings.answer,
            blocking: false,
            agent_role: "spike",
            author_kind: "agent",
            author_model: None,
            source_task_id: None,
            against_revision_seq: 1,
            round: 1,
            body_metadata: Some(&meta),
        })
        .await
        .expect("evidence_findings entry must be accepted");

        // Verify it persisted and can be read back.
        let trail = repo.debate_trail(&p.id).await.unwrap();
        assert_eq!(trail.len(), 1);
        assert_eq!(trail[0].kind, "evidence_findings");
        let stored_meta = trail[0].body_metadata.as_ref().unwrap();
        let round_tripped = EvidenceFindings::parse_stored(Some(stored_meta))
            .unwrap()
            .expect("must parse back");
        assert_eq!(round_tripped.answer, findings.answer);
        assert_eq!(round_tripped.confidence, 0.95);
        assert_eq!(round_tripped.code_paths_inspected.len(), 2);
    }

    // ── AC#4b: Cap accounting from control-plane layer ───────────────────

    /// The `check_needs_evidence_cap` control-plane helper returns correct
    /// cap status from persisted debate rows. This proves the cap-accounting
    /// is usable from the control-plane boundary.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cap_accounting_usable_from_control_plane() {
        let (_server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo
            .create(ProposalCreateInput {
                title: "Cap Boundary Test",
                body: "",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Start refinement.
        repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
            .await
            .unwrap();

        // One demand: under cap.
        let link = NeedsEvidenceClaimLink {
            kind: NeedsEvidenceClaimLink::KIND_MARKER.to_owned(),
            proposal_id: p.id.clone(),
            judge_task_id: "judge-1".to_owned(),
            spike_task_id: uuid::Uuid::now_v7().to_string(),
            round: 1,
            against_revision_seq: 1,
        };
        let meta_value = link.to_value();
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &p.id,
            kind: "needs_evidence",
            body: "demand 1",
            blocking: true,
            agent_role: "judge",
            author_kind: "agent",
            author_model: None,
            source_task_id: None,
            against_revision_seq: 1,
            round: 1,
            body_metadata: Some(&meta_value),
        })
        .await
        .unwrap();

        // Control-plane helper: under cap.
        let cap = check_needs_evidence_cap(&repo, &p.id).await.unwrap();
        assert_eq!(cap.count, 1);
        assert!(!cap.cap_exceeded);

        // Second demand: at cap.
        let link2 = NeedsEvidenceClaimLink {
            kind: NeedsEvidenceClaimLink::KIND_MARKER.to_owned(),
            proposal_id: p.id.clone(),
            judge_task_id: "judge-2".to_owned(),
            spike_task_id: uuid::Uuid::now_v7().to_string(),
            round: 2,
            against_revision_seq: 1,
        };
        let meta_value2 = link2.to_value();
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &p.id,
            kind: "needs_evidence",
            body: "demand 2",
            blocking: true,
            agent_role: "judge",
            author_kind: "agent",
            author_model: None,
            source_task_id: None,
            against_revision_seq: 1,
            round: 2,
            body_metadata: Some(&meta_value2),
        })
        .await
        .unwrap();

        let cap2 = check_needs_evidence_cap(&repo, &p.id).await.unwrap();
        assert_eq!(cap2.count, 2);
        assert!(cap2.cap_exceeded, "cap must be exceeded after 2 demands");
    }

    // ── AC#4c: Cap survives restart-style reconstruction ─────────────────

    /// After persisting debate rows, the cap count survives a fresh
    /// `ProposalRepository` construction (simulating restart with no
    /// in-memory state). This is the key restart-safety property.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cap_survives_restart_style_reconstruction() {
        let (_server, db) = test_server().await;

        // First "process": create proposal, start refinement, add demands.
        let proposal_id;
        {
            let repo = ProposalRepository::new(db.clone(), EventBus::noop());
            let p = repo
                .create(ProposalCreateInput {
                    title: "Restart Safety Test",
                    body: "",
                    acceptance_criteria: Some("[]"),
                    status: None,
                    body_format: None,
                })
                .await
                .unwrap();
            proposal_id = p.id.clone();

            repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
                .await
                .unwrap();

            for i in 1..=2 {
                let link = NeedsEvidenceClaimLink {
                    kind: NeedsEvidenceClaimLink::KIND_MARKER.to_owned(),
                    proposal_id: p.id.clone(),
                    judge_task_id: format!("judge-{i}"),
                    spike_task_id: uuid::Uuid::now_v7().to_string(),
                    round: i,
                    against_revision_seq: 1,
                };
                let meta_value = link.to_value();
                repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
                    proposal_id: &p.id,
                    kind: "needs_evidence",
                    body: &format!("demand {i}"),
                    blocking: true,
                    agent_role: "judge",
                    author_kind: "agent",
                    author_model: None,
                    source_task_id: None,
                    against_revision_seq: 1,
                    round: i,
                    body_metadata: Some(&meta_value),
                })
                .await
                .unwrap();
            }
        }
        // repo dropped — simulating process exit.

        // Second "process": fresh repo from persisted rows.
        {
            let repo = ProposalRepository::new(db.clone(), EventBus::noop());
            let cap = check_needs_evidence_cap(&repo, &proposal_id)
                .await
                .unwrap();
            assert_eq!(cap.count, 2, "cap count must survive restart");
            assert!(cap.cap_exceeded, "cap_exceeded must survive restart");
            assert!(!cap.no_refinement_run, "refinement run must be detectable");
        }
    }

    // ── AC#4d: Malformed rejected demands do not count ───────────────────

    /// A `needs_evidence` debate entry without `body_metadata` is rejected
    /// by `add_debate_trail_entry`, so it never persists and never counts
    /// toward the cap.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn malformed_demand_rejected_and_does_not_count() {
        let (_server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo
            .create(ProposalCreateInput {
                title: "Malformed Demand Test",
                body: "",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
            .await
            .unwrap();

        // Attempt a needs_evidence entry without body_metadata — should fail.
        let result = repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &p.id,
            kind: "needs_evidence",
            body: "malformed demand",
            blocking: true,
            agent_role: "judge",
            author_kind: "agent",
            author_model: None,
            source_task_id: None,
            against_revision_seq: 1,
            round: 1,
            body_metadata: None,
        })
        .await;
        assert!(result.is_err(), "needs_evidence without body_metadata must be rejected");

        // Cap is still 0 because the malformed demand was never persisted.
        let cap = check_needs_evidence_cap(&repo, &p.id).await.unwrap();
        assert_eq!(cap.count, 0);
        assert!(!cap.cap_exceeded);
    }
