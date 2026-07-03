
    // ── AC: AwaitingEvidence with full structured claim payload ───────────
    //
    // Covers acceptance criteria for the distinct AwaitingEvidence and
    // EvidenceFailed status payloads task (h2az). Tests verify:
    //   - Full structured claim fields including insufficient_in_session_research
    //     and expected_findings
    //   - EvidenceFailed with various failure reasons (spike_force_closed,
    //     malformed_findings) distinct from AwaitingEvidence
    //   - EvidenceFailed does not carry stale awaiting_review or converged copy
    //   - PausedOrFrozen precedence preserves evidence context including
    //     the new claim fields

    // ── AC#1: AwaitingEvidence structured claim includes all fields ─────

    /// When a structured claim is stored, the AwaitingEvidence payload
    /// includes `insufficient_in_session_research` and `expected_findings`
    /// alongside the existing claim fields.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn awaiting_evidence_full_structured_claim_payload() {
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
        assert_eq!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::AwaitingEvidence,
        );

        let ne = status.needs_evidence.as_ref()
            .expect("needs_evidence must be present");

        // Spike identification.
        assert_eq!(ne.spike_task_id, *spike_id);
        assert!(!ne.spike_short_id.is_empty());
        assert_eq!(ne.spike_status, "open");

        // Core structured claim fields.
        assert_eq!(ne.question.as_deref(), Some("Is module X load-bearing?"));
        assert_eq!(ne.target_subsystem.as_deref(), Some("auth"));
        assert_eq!(
            ne.spec_unknown_anchor.as_deref(),
            Some("Section 3.2 token refresh"),
        );
        assert_eq!(ne.round, Some(1));
        assert_eq!(ne.against_revision_seq, Some(2));
        assert_eq!(ne.created_by_task_id.as_deref(), Some("judge-task-001"));

        // The newly added structured claim fields.
        assert_eq!(
            ne.insufficient_in_session_research.as_deref(),
            Some("No auth tests found"),
        );
        assert_eq!(
            ne.expected_findings.as_deref(),
            Some("Evidence that token refresh is or is not required"),
        );

        // Evidence phase.
        assert_eq!(
            ne.evidence_phase,
            Some(crate::tools::proposal_ops::EvidenceLifecyclePhase::AwaitingEvidence),
        );
        assert!(ne.failure_reason.is_none());
    }

    /// Legacy plain-string claims have `None` for the new fields.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn legacy_claim_has_none_for_new_fields() {
        let (_server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo
            .create(ProposalCreateInput {
                title: "Legacy New Fields Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
            .await
            .unwrap();

        let proj = insert_project(&db, "svc-legacy-new").await;
        let epic = insert_epic(&db, &proj, "lg02").await;
        let spike_id = insert_task(&db, &proj, &epic, "spike-lg2").await;

        repo.set_needs_evidence_spike(&p.id, &spike_id, "X is load-bearing")
            .await
            .unwrap();

        let status = build_refinement_status(&repo, &p.id).await.unwrap();
        let ne = status.needs_evidence.as_ref()
            .expect("needs_evidence must be present");

        assert_eq!(ne.claim, "X is load-bearing");
        assert!(ne.question.is_none());
        assert!(ne.target_subsystem.is_none());
        assert!(ne.spec_unknown_anchor.is_none());
        assert!(ne.insufficient_in_session_research.is_none());
        assert!(ne.expected_findings.is_none());
    }

    // ── AC#2: EvidenceFailed with spike_force_closed ────────────────────

    /// EvidenceFailed with `spike_force_closed` reason is a distinct
    /// state, not an open AwaitingEvidence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn evidence_failed_spike_force_closed_distinct() {
        let (_server, db, p) = setup_structured_claim().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let spike_id = p.linked_spike_task_id.as_ref().unwrap();

        repo.record_awaiting_evidence_started(
            &p.id, spike_id, "judge-task-001", 1, 2,
        )
        .await
        .unwrap();
        repo.record_evidence_failed(
            &p.id, spike_id, "judge-task-001", 1, 2, "spike_force_closed",
        )
        .await
        .unwrap();

        let status = build_refinement_status(&repo, &p.id).await.unwrap();

        // Top-level state is EvidenceFailed.
        assert_eq!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::EvidenceFailed,
        );
        assert_ne!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::AwaitingEvidence,
        );
        assert_ne!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::Active,
        );

        // Inner payload has the failure reason.
        let ne = status.needs_evidence.as_ref().unwrap();
        assert_eq!(
            ne.evidence_phase,
            Some(crate::tools::proposal_ops::EvidenceLifecyclePhase::EvidenceFailed),
        );
        assert_eq!(ne.failure_reason.as_deref(), Some("spike_force_closed"));

        // Structured claim fields are still available (not cleared).
        assert!(ne.question.is_some());
        assert!(ne.target_subsystem.is_some());
        assert!(ne.insufficient_in_session_research.is_some());
        assert!(ne.expected_findings.is_some());
    }

    // ── AC#2: EvidenceFailed with malformed_findings ────────────────────

    /// EvidenceFailed with `malformed_findings` reason is distinct from
    /// AwaitingEvidence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn evidence_failed_malformed_findings_distinct() {
        let (_server, db, p) = setup_structured_claim().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let spike_id = p.linked_spike_task_id.as_ref().unwrap();

        repo.record_awaiting_evidence_started(
            &p.id, spike_id, "judge-task-001", 1, 2,
        )
        .await
        .unwrap();
        repo.record_evidence_failed(
            &p.id, spike_id, "judge-task-001", 1, 2, "malformed_findings",
        )
        .await
        .unwrap();

        let status = build_refinement_status(&repo, &p.id).await.unwrap();
        assert_eq!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::EvidenceFailed,
        );

        let ne = status.needs_evidence.as_ref().unwrap();
        assert_eq!(ne.failure_reason.as_deref(), Some("malformed_findings"));
    }

    // ── AC#3: EvidenceFailed does not carry stale review/converged copy ─

    /// EvidenceFailed does not set `awaiting_review` or stale stop reasons
    /// that would make the UI render "in progress" or "converged".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn evidence_failed_no_stale_review_or_converged_copy() {
        let (_server, db, p) = setup_structured_claim().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let spike_id = p.linked_spike_task_id.as_ref().unwrap();

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

        // EvidenceFailed must not be presented as "awaiting review" for
        // the human accept/reject flow.
        assert!(
            !status.awaiting_review,
            "EvidenceFailed must not set awaiting_review",
        );

        // The stop_reason must not contain converged or in-progress wording
        // that would confuse the UI.
        if let Some(ref reason) = status.stop_reason {
            assert!(
                !reason.contains("converged"),
                "stop_reason must not say 'converged': {reason}",
            );
        }

        // Evidence lifecycle state is explicitly EvidenceFailed, not any
        // state that maps to "in progress" or "awaiting review" in UI copy.
        assert_eq!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::EvidenceFailed,
        );
        assert_ne!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::AwaitingEvidence,
            "must not be represented as an open awaiting state",
        );
    }

    // ── AC#4: PausedOrFrozen preserves evidence context ─────────────────

    /// When frozen, the top-level state is PausedOrFrozen but the inner
    /// `needs_evidence` payload retains all structured claim fields
    /// (including the new ones) so the UI can show evidence context
    /// alongside the freeze message.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn paused_or_frozen_preserves_full_evidence_context() {
        let (_server, db, p) = setup_structured_claim().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let spike_id = p.linked_spike_task_id.as_ref().unwrap();

        // Record awaiting lifecycle.
        repo.record_awaiting_evidence_started(
            &p.id, spike_id, "judge-task-001", 1, 2,
        )
        .await
        .unwrap();

        // Freeze the proposal.
        repo.set_frozen(&p.id, true).await.unwrap();

        let status = build_refinement_status(&repo, &p.id).await.unwrap();
        assert_eq!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::PausedOrFrozen,
            "frozen must take precedence",
        );

        // Evidence context is preserved (not discarded).
        let ne = status.needs_evidence.as_ref()
            .expect("needs_evidence must be preserved when frozen");
        assert_eq!(
            ne.evidence_phase,
            Some(crate::tools::proposal_ops::EvidenceLifecyclePhase::AwaitingEvidence),
            "inner evidence phase is preserved",
        );

        // All structured claim fields are still populated.
        assert_eq!(ne.question.as_deref(), Some("Is module X load-bearing?"));
        assert_eq!(ne.target_subsystem.as_deref(), Some("auth"));
        assert_eq!(
            ne.spec_unknown_anchor.as_deref(),
            Some("Section 3.2 token refresh"),
        );
        assert_eq!(
            ne.insufficient_in_session_research.as_deref(),
            Some("No auth tests found"),
        );
        assert_eq!(
            ne.expected_findings.as_deref(),
            Some("Evidence that token refresh is or is not required"),
        );
        assert_eq!(ne.round, Some(1));
        assert_eq!(ne.against_revision_seq, Some(2));
        assert_eq!(ne.created_by_task_id.as_deref(), Some("judge-task-001"));
    }

    /// When frozen and evidence has failed, the top-level state is
    /// PausedOrFrozen but the inner failure context is preserved.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn paused_or_frozen_preserves_evidence_failed_context() {
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
            &p.id, spike_id, "judge-task-001", 1, 2, "spike_errored",
        )
        .await
        .unwrap();

        // Freeze the proposal.
        repo.set_frozen(&p.id, true).await.unwrap();

        let status = build_refinement_status(&repo, &p.id).await.unwrap();
        assert_eq!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::PausedOrFrozen,
            "frozen must take precedence",
        );

        // Inner failure context is preserved.
        let ne = status.needs_evidence.as_ref()
            .expect("needs_evidence must be preserved when frozen");
        assert_eq!(
            ne.evidence_phase,
            Some(crate::tools::proposal_ops::EvidenceLifecyclePhase::EvidenceFailed),
        );
        assert_eq!(ne.failure_reason.as_deref(), Some("spike_errored"));

        // Structured claim fields still available alongside failure.
        assert!(ne.question.is_some());
        assert!(ne.insufficient_in_session_research.is_some());
        assert!(ne.expected_findings.is_some());
    }

    // ── AC#5: EvidenceReceived does not produce contradictory fields ────

    /// EvidenceReceived state: evidence has been received but the proposal
    /// is not frozen. Status should not imply paused or failed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn evidence_received_no_contradictory_fields() {
        let (_server, db, p) = setup_structured_claim().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let spike_id = p.linked_spike_task_id.as_ref().unwrap();

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

        assert_eq!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::EvidenceReceived,
        );
        assert_ne!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::EvidenceFailed,
        );
        assert_ne!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::PausedOrFrozen,
        );
        assert!(!status.awaiting_review);

        let ne = status.needs_evidence.as_ref().unwrap();
        assert_eq!(
            ne.evidence_phase,
            Some(crate::tools::proposal_ops::EvidenceLifecyclePhase::EvidenceReceived),
        );
        assert!(ne.failure_reason.is_none());

        // Structured claim fields still available.
        assert!(ne.question.is_some());
        assert!(ne.insufficient_in_session_research.is_some());
        assert!(ne.expected_findings.is_some());
    }
