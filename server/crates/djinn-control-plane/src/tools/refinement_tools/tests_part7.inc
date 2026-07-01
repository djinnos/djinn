
    // ── Evidence lifecycle state surfacing tests ───────────────────────────
    //
    // Focused tests for `evidence_lifecycle_state` on
    // `ProposalRefinementStatusModel`.  Covers:
    //   - Active (no evidence parking)
    //   - AwaitingEvidence (open spike)
    //   - EvidenceFailed (persisted failure lifecycle)
    //   - PausedOrFrozen precedence (build_frozen overrides active)
    //   - Terminal (proposal status is terminal)
    //
    // All tests use in-memory DB + `build_refinement_status` — no live
    // runtime infrastructure.

    use crate::tools::proposal_ops::EvidenceLifecyclePhase;

    // ── AC: Active state when no evidence parking ──────────────────────

    /// When refinement is active and no linked spike exists,
    /// `evidence_lifecycle_state` is `Active`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_state_active_when_no_evidence_parking() {
        let (_server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo
            .create(ProposalCreateInput {
                title: "Active Test",
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

        let status = build_refinement_status(&repo, &p.id).await.unwrap();
        assert!(status.active, "refinement must be active");
        assert_eq!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::Active,
        );
        assert!(status.needs_evidence.is_none());
    }

    // ── AC: AwaitingEvidence when spike is linked ─────────────────────

    /// When a linked spike exists but no lifecycle event has been
    /// recorded yet, `evidence_lifecycle_state` is `AwaitingEvidence`.
    /// This covers the brief window between spike creation and the
    /// `refinement_awaiting_evidence_started` event write.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_state_awaiting_evidence_open_spike_no_event() {
        let (_server, db, p) = setup_structured_claim().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());

        let status = build_refinement_status(&repo, &p.id).await.unwrap();
        // The spike is linked but no lifecycle event has been written yet.
        assert_eq!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::AwaitingEvidence,
        );
        assert!(status.needs_evidence.is_some(), "needs_evidence must be populated");
    }

    /// When a linked spike exists and the
    /// `refinement_awaiting_evidence_started` lifecycle event has been
    /// recorded, `evidence_lifecycle_state` is `AwaitingEvidence`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_state_awaiting_evidence_with_lifecycle_event() {
        let (_server, db, p) = setup_structured_claim().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());

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
        // Inner phase matches too.
        let ne = status.needs_evidence.as_ref().unwrap();
        assert_eq!(
            ne.evidence_phase,
            Some(EvidenceLifecyclePhase::AwaitingEvidence),
        );
    }

    // ── AC: EvidenceFailed is distinct from Active/Awaiting/Terminal ──

    /// When the spike has been failed (lifecycle event recorded),
    /// `evidence_lifecycle_state` is `EvidenceFailed`, distinct from
    /// Active, AwaitingEvidence, and Terminal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_state_evidence_failed_distinct() {
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
        assert_eq!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::EvidenceFailed,
        );
        // It must NOT be any of these.
        assert_ne!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::Active,
        );
        assert_ne!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::AwaitingEvidence,
        );
        assert_ne!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::Terminal,
        );
        // Inner failure reason is populated.
        let ne = status.needs_evidence.as_ref().unwrap();
        assert_eq!(ne.failure_reason.as_deref(), Some("spike_cancelled"));
    }

    // ── AC: PausedOrFrozen precedence over active and evidence states ─

    /// When `build_frozen` is true, `evidence_lifecycle_state` is
    /// `PausedOrFrozen` — even if refinement is active and no evidence
    /// parking exists.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_state_paused_or_frozen_overrides_active() {
        let (_server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo
            .create(ProposalCreateInput {
                title: "Frozen Active Test",
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

        // Freeze the proposal.
        repo.set_frozen(&p.id, true).await.unwrap();

        let status = build_refinement_status(&repo, &p.id).await.unwrap();
        assert!(status.active, "refinement is technically still active");
        assert_eq!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::PausedOrFrozen,
            "frozen must override active",
        );
        assert_ne!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::Active,
        );
    }

    /// When `build_frozen` is true AND an evidence spike is linked,
    /// `evidence_lifecycle_state` is `PausedOrFrozen` — not
    /// `AwaitingEvidence`.  Freeze takes precedence over evidence
    /// sub-states.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_state_paused_or_frozen_overrides_awaiting_evidence() {
        let (_server, db, p) = setup_structured_claim().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());

        // Freeze the proposal.
        repo.set_frozen(&p.id, true).await.unwrap();

        let status = build_refinement_status(&repo, &p.id).await.unwrap();
        assert_eq!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::PausedOrFrozen,
            "frozen must override awaiting evidence",
        );
        // needs_evidence sub-field still populated (not cleared).
        assert!(status.needs_evidence.is_some());
    }

    /// When `build_frozen` is true AND evidence has failed,
    /// `evidence_lifecycle_state` is `PausedOrFrozen` — not
    /// `EvidenceFailed`.  Freeze takes precedence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_state_paused_or_frozen_overrides_evidence_failed() {
        let (_server, db, p) = setup_structured_claim().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let spike_id = p.linked_spike_task_id.as_ref().unwrap();

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
            "frozen must override evidence failed",
        );
        // Inner failure reason still populated (not cleared).
        let ne = status.needs_evidence.as_ref().unwrap();
        assert_eq!(ne.failure_reason.as_deref(), Some("spike_errored"));
    }

    // ── AC: Terminal state ────────────────────────────────────────────

    /// When the proposal status is terminal (`done`), the lifecycle
    /// state is `Terminal` regardless of refinement history.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_state_terminal_proposal() {
        let (_server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo
            .create(ProposalCreateInput {
                title: "Terminal Test",
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

        // Mark proposal as done.
        repo.set_status(&p.id, "done").await.unwrap();

        let status = build_refinement_status(&repo, &p.id).await.unwrap();
        assert_eq!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::Terminal,
        );
        assert_ne!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::Active,
        );
    }

    /// Terminal takes precedence over freeze: a done + frozen proposal
    /// is `Terminal`, not `PausedOrFrozen`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_state_terminal_takes_precedence_over_frozen() {
        let (_server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo
            .create(ProposalCreateInput {
                title: "Terminal Frozen Test",
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
        repo.set_frozen(&p.id, true).await.unwrap();
        repo.set_status(&p.id, "done").await.unwrap();

        let status = build_refinement_status(&repo, &p.id).await.unwrap();
        assert_eq!(
            status.evidence_lifecycle_state,
            EvidenceLifecycleState::Terminal,
            "terminal must take precedence over frozen",
        );
    }

    /// Various terminal statuses all produce `Terminal`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_state_terminal_all_terminal_statuses() {
        for terminal_status in &["done", "rejected", "archived", "superseded"] {
            let (_server, db) = test_server().await;
            let repo = ProposalRepository::new(db.clone(), EventBus::noop());
            let p = repo
                .create(ProposalCreateInput {
                    title: &format!("Terminal {terminal_status} Test"),
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
            repo.set_status(&p.id, terminal_status).await.unwrap();

            let status = build_refinement_status(&repo, &p.id).await.unwrap();
            assert_eq!(
                status.evidence_lifecycle_state,
                EvidenceLifecycleState::Terminal,
                "status '{terminal_status}' must be Terminal",
            );
        }
    }

    // ── AC: EvidenceReceived is surfaced ──────────────────────────────

    /// When evidence has been received (lifecycle event recorded),
    /// `evidence_lifecycle_state` is `EvidenceReceived`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_state_evidence_received() {
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
    }

    // ── AC: Unfreezing restores the correct evidence state ────────────

    /// After unfreezing a proposal that was in AwaitingEvidence, the
    /// lifecycle state reverts to `AwaitingEvidence`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_state_unfreeze_restores_awaiting_evidence() {
        let (_server, db, p) = setup_structured_claim().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let spike_id = p.linked_spike_task_id.as_ref().unwrap();

        repo.record_awaiting_evidence_started(
            &p.id, spike_id, "judge-task-001", 1, 2,
        )
        .await
        .unwrap();

        // Freeze → PausedOrFrozen.
        repo.set_frozen(&p.id, true).await.unwrap();
        let frozen_status = build_refinement_status(&repo, &p.id).await.unwrap();
        assert_eq!(
            frozen_status.evidence_lifecycle_state,
            EvidenceLifecycleState::PausedOrFrozen,
        );

        // Unfreeze → AwaitingEvidence.
        repo.set_frozen(&p.id, false).await.unwrap();
        let unfrozen_status = build_refinement_status(&repo, &p.id).await.unwrap();
        assert_eq!(
            unfrozen_status.evidence_lifecycle_state,
            EvidenceLifecycleState::AwaitingEvidence,
        );
    }
