    // ── Exact-run status snapshot regressions ─────────────────────────────

    use djinn_core::refinement_liveness::{
        DbTimestamp, RefinementLivenessEvidence, RefinementLivenessResult,
    };

    #[test]
    fn exact_status_serializes_every_shared_liveness_evidence_class() {
        let cases = [
            (RefinementLivenessEvidence::AwaitingReviewPark, "awaiting_review_park"),
            (RefinementLivenessEvidence::AwaitingEvidencePark, "awaiting_evidence_park"),
            (RefinementLivenessEvidence::PendingIntent { intent_id: "i".into() }, "pending_intent"),
            (RefinementLivenessEvidence::ClaimedIntent { intent_id: "i".into() }, "claimed_intent"),
            (RefinementLivenessEvidence::OpenTask { task_id: "t".into() }, "open_task"),
            (RefinementLivenessEvidence::QueuedTask { task_id: "t".into() }, "queued_task"),
            (RefinementLivenessEvidence::RunningTask { task_id: "t".into() }, "running_task"),
            (RefinementLivenessEvidence::PoolPausedTask { task_id: "t".into() }, "pool_paused_task"),
            (RefinementLivenessEvidence::LiveSession { session_id: "s".into(), task_id: "t".into() }, "live_session"),
            (RefinementLivenessEvidence::BetweenPhase { intent_id: "i".into() }, "between_phase"),
            (RefinementLivenessEvidence::FreshHeartbeat { heartbeat_at: DbTimestamp(1) }, "fresh_heartbeat"),
        ];
        for (evidence, expected) in cases {
            let (state, serialized) = crate::tools::refinement_helpers::liveness_fields(
                &RefinementLivenessResult::Live { evidence },
            );
            assert_eq!(state, "live");
            assert_eq!(serialized.as_deref(), Some(expected));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exact_run_status_is_read_only_for_phantom_parked_and_terminal_runs() {
        let (_server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo.create(ProposalCreateInput {
            title: "Exact status snapshot", body: "body", acceptance_criteria: Some("[]"),
            status: Some("draft"), body_format: None,
        }).await.unwrap();
        let run_id = match repo.reap_and_admit(djinn_db::AdmitRefinementRunRequest {
            proposal_id: proposal.id.clone(), idempotency_key: "exact-status-fixture".into(),
            source: djinn_db::RefinementAdmissionSource::Demand { demand_id: "exact-status-fixture".into() },
            heartbeat_grace_millis: 60_000,
        }).await.unwrap() {
            djinn_db::RefinementAdmissionOutcome::Admitted { run_id, .. } => run_id,
            djinn_db::RefinementAdmissionOutcome::Existing { .. } => unreachable!(),
        };

        let live = build_refinement_status(&repo, &proposal.id).await.unwrap();
        assert_eq!(live.run_id.as_deref(), Some(run_id.as_str()));
        assert_eq!(live.liveness_evidence.as_deref(), Some("pending_intent"));

        // Status must evaluate the durable expired lease rather than treating
        // every claimed intent as live.
        sqlx::query("UPDATE refinement_dispatch_intents SET state = 'claimed', claimed_by = 'test', claimed_at = '2000-01-01T00:00:00.000Z', claim_expires_at = '2000-01-01T00:00:01.000Z' WHERE run_id = $1")
            .bind(&run_id).execute(db.pool()).await.unwrap();
        sqlx::query("UPDATE refinement_runs SET heartbeat_at = '2000-01-01T00:00:00.000Z' WHERE id = $1")
            .bind(&run_id).execute(db.pool()).await.unwrap();
        let expired_claim = build_refinement_status(&repo, &proposal.id).await.unwrap();
        assert_eq!(expired_claim.liveness.as_deref(), Some("stale"));

        djinn_db::test_support::make_refinement_run_phantom_for_test(&db, &run_id).await;
        let before = djinn_db::test_support::refinement_run_read_only_snapshot_for_test(&db, &proposal.id, &run_id).await;
        let stale = build_refinement_status(&repo, &proposal.id).await.unwrap();
        let repeated = build_refinement_status(&repo, &proposal.id).await.unwrap();
        let after = djinn_db::test_support::refinement_run_read_only_snapshot_for_test(&db, &proposal.id, &run_id).await;
        assert_eq!(stale.liveness.as_deref(), Some("stale"));
        assert_eq!(
            serde_json::to_value(&stale).unwrap(),
            serde_json::to_value(&repeated).unwrap(),
        );
        assert_eq!(before, after, "status must not mutate a stale phantom");

        sqlx::query("UPDATE refinement_runs SET state = 'parked', park_kind = 'awaiting_review', parked_at = '2026-01-01T00:00:00.000Z' WHERE id = $1")
            .bind(&run_id).execute(db.pool()).await.unwrap();
        let parked = build_refinement_status(&repo, &proposal.id).await.unwrap();
        assert_eq!(parked.run_state.as_deref(), Some("parked"));
        assert_eq!(parked.liveness_evidence.as_deref(), Some("awaiting_review_park"));
        assert!(parked.awaiting_review);

        sqlx::query(r#"UPDATE refinement_runs SET state = 'terminal', terminal_at = '2026-01-01T00:00:00.000Z', stop_tag = 'operator_stop', stop_context = '{"actor": "test", "reason": null}'::jsonb WHERE id = $1"#)
            .bind(&run_id).execute(db.pool()).await.unwrap();
        let terminal = build_refinement_status(&repo, &proposal.id).await.unwrap();
        assert_eq!(terminal.run_state.as_deref(), Some("terminal"));
        assert_eq!(terminal.liveness.as_deref(), Some("terminal"));
        assert_eq!(terminal.stop_reason.as_deref(), Some("operator_stop"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exact_status_uses_latest_generation_not_prior_run_evidence() {
        let (_server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo.create(ProposalCreateInput {
            title: "Prior run isolation", body: "body", acceptance_criteria: Some("[]"),
            status: Some("draft"), body_format: None,
        }).await.unwrap();
        let admit = |key: &str| djinn_db::AdmitRefinementRunRequest {
            proposal_id: proposal.id.clone(), idempotency_key: key.into(),
            source: djinn_db::RefinementAdmissionSource::Demand { demand_id: key.into() },
            heartbeat_grace_millis: 60_000,
        };
        let first = match repo.reap_and_admit(admit("prior-run")).await.unwrap() {
            djinn_db::RefinementAdmissionOutcome::Admitted { run_id, .. } => run_id,
            _ => unreachable!(),
        };
        sqlx::query(r#"UPDATE refinement_runs SET state = 'terminal', stop_tag = 'operator_stop', stop_context = '{"actor": "test", "reason": null}'::jsonb WHERE id = $1"#)
            .bind(&first).execute(db.pool()).await.unwrap();
        let second = match repo.reap_and_admit(admit("current-run")).await.unwrap() {
            djinn_db::RefinementAdmissionOutcome::Admitted { run_id, .. } => run_id,
            _ => unreachable!(),
        };
        djinn_db::test_support::make_refinement_run_phantom_for_test(&db, &second).await;
        let status = build_refinement_status(&repo, &proposal.id).await.unwrap();
        assert_eq!(status.run_id.as_deref(), Some(second.as_str()));
        assert_eq!(status.liveness.as_deref(), Some("stale"));
        assert_ne!(status.stop_reason.as_deref(), Some("operator_stop"));
    }
