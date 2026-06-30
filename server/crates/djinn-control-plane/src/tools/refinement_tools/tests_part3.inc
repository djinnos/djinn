
    // ── Needs-evidence cap accounting tests ──────────────────────────────

    use djinn_db::{NeedsEvidenceCapStatus, NeedsEvidenceClaimLink, ProposalDebateTrailCreateInput};

    /// `check_needs_evidence_cap` returns the cap status for a proposal
    /// with no refinement run (no_refinement_run = true, count = 0).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn check_needs_evidence_cap_no_run() {
        let (_server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo
            .create(ProposalCreateInput {
                title: "Cap No Run",
                body: "",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let status = check_needs_evidence_cap(&repo, &p.id).await.unwrap();
        assert!(status.no_refinement_run);
        assert_eq!(status.count, 0);
        assert!(!status.cap_exceeded);
    }

    /// `check_needs_evidence_cap` returns cap_exceeded after two accepted
    /// demands in a refinement run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn check_needs_evidence_cap_two_demands_exceeded() {
        let (_server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo
            .create(ProposalCreateInput {
                title: "Cap Exceeded",
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

        // Two demands.
        for i in 1..=2 {
            let judge_task_id = uuid::Uuid::now_v7().to_string();
            let spike_task_id = uuid::Uuid::now_v7().to_string();
            let link = NeedsEvidenceClaimLink {
                kind: NeedsEvidenceClaimLink::KIND_MARKER.to_owned(),
                proposal_id: p.id.clone(),
                judge_task_id,
                spike_task_id,
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

        let status = check_needs_evidence_cap(&repo, &p.id).await.unwrap();
        assert!(!status.no_refinement_run);
        assert_eq!(status.count, 2);
        assert!(status.cap_exceeded);
    }
