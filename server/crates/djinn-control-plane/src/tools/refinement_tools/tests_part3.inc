
    // ── Durable attribution on every admission source ────────────────────

    /// Every admission source must leave the run attributed, because
    /// `proposals.refinement_owner_user_id` is the only attribution the durable
    /// dispatcher accepts. Production run `019fa0bb-c31f` (proposal `8ixk`) sat
    /// on generation 5 with `run_state = active` and zero debate entries for
    /// twelve days: `proposal_refinement_demand_round` admitted generation after
    /// generation without ever writing an owner, so
    /// `drive_durable_refinement_intents` re-claimed its intent every tick and
    /// then bailed with "awaiting durable refinement attribution".
    ///
    /// Neutralization: reverting `admit_refinement_run` to pass the owner
    /// through only from `proposal_refinement_start` fails this on the demand
    /// assertion with `Some("attribution-author-019f")` != `None`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_admission_source_leaves_the_run_durably_attributed() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Attribution on demand",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        link_proposal_to_project(&db, &repo, &proposal.id).await;

        // `refinement_owner_user_id` is FK-constrained to `users`, so the
        // author fallback can only be observed against a real row.
        sqlx::query("INSERT INTO users (id, github_id, github_login) VALUES ($1, $2, $3)")
            .bind("attribution-author-019f")
            .bind(919_067_201i64)
            .bind("attribution-author-019f")
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE proposals SET author_user_id = $2 WHERE id = $1")
            .bind(&proposal.id)
            .bind("attribution-author-019f")
            .execute(db.pool())
            .await
            .unwrap();

        // A demanded round carries no explicit owner. It must still attribute
        // the run from the proposal author.
        let response = server
            .dispatch_tool(
                "proposal_refinement_demand_round",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "reason": "attribution regression",
                    "request_id": "demand-attribution-019f",
                }),
            )
            .await
            .unwrap();
        assert_eq!(response["accepted"], true, "{response}");
        assert_eq!(
            repo.get(&proposal.id)
                .await
                .unwrap()
                .unwrap()
                .refinement_owner_user_id
                .as_deref(),
            Some("attribution-author-019f"),
            "a demanded round must attribute the run it admits"
        );

        // A later start without an explicit owner must not clear or reassign
        // the attribution the run already has.
        let response = server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "request_id": "start-attribution-019f",
                }),
            )
            .await
            .unwrap();
        assert!(
            response["error"].as_str() != Some("Persistence"),
            "{response}"
        );
        assert_eq!(
            repo.get(&proposal.id)
                .await
                .unwrap()
                .unwrap()
                .refinement_owner_user_id
                .as_deref(),
            Some("attribution-author-019f"),
            "an implicit start must never clear a run's durable owner"
        );
    }

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
