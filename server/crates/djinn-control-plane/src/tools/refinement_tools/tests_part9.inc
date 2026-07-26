    // ── Exact-run status snapshot regressions ─────────────────────────────

    use djinn_core::refinement_liveness::{
        DbTimestamp, RefinementLivenessEvidence, RefinementLivenessResult,
    };

    async fn seed_status_task(db: &Database, run_id: &str, status: &str) -> (String, String) {
        let project_id = uuid::Uuid::now_v7().to_string();
        djinn_db::test_support::seed_project(db, &project_id, "refinement status evidence").await;
        let task_id = djinn_db::test_support::seed_task_row(
            db,
            djinn_db::test_support::UsageTestTaskSeed {
                project_id: &project_id,
                status,
                close_reason: None,
                total_reopen_count: 0,
            },
        )
        .await;
        sqlx::query("UPDATE tasks SET refinement_run_id = $1 WHERE id = $2")
            .bind(run_id).bind(&task_id).execute(db.pool()).await.unwrap();
        (project_id, task_id)
    }

    async fn seed_live_status_session(db: &Database, project_id: &str, task_id: &str) -> String {
        let session_id = uuid::Uuid::now_v7().to_string();
        djinn_db::test_support::seed_session_row_with_id(
            db,
            &session_id,
            djinn_db::test_support::UsageTestSessionSeed {
                project_id,
                model_id: "test",
                agent_type: "worker",
                started_at: "2000-01-01T00:00:00.000Z",
                tokens_in: 0,
                tokens_out: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cost_usd: None,
                cost_basis: "unpriced",
                task_id: Some(task_id),
            },
        ).await;
        sqlx::query("UPDATE sessions SET status = 'running' WHERE id = $1")
            .bind(&session_id).execute(db.pool()).await.unwrap();
        session_id
    }

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
    async fn exact_status_observes_repository_intent_task_session_and_heartbeat_evidence() {
        let (_server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo.create(ProposalCreateInput {
            title: "Exact evidence status", body: "body", acceptance_criteria: Some("[]"),
            status: Some("draft"), body_format: None,
        }).await.unwrap();
        let run_id = match repo.reap_and_admit(djinn_db::AdmitRefinementRunRequest {
            proposal_id: proposal.id.clone(), idempotency_key: "status-evidence".into(),
            source: djinn_db::RefinementAdmissionSource::Demand { demand_id: "status-evidence".into() },
            heartbeat_grace_millis: 60_000,
        }).await.unwrap() {
            djinn_db::RefinementAdmissionOutcome::Admitted { run_id, .. } => run_id,
            _ => unreachable!(),
        };
        let pending = build_refinement_status(&repo, &proposal.id).await.unwrap();
        assert_eq!(pending.run_id.as_deref(), Some(run_id.as_str()));
        assert_eq!(pending.run_state.as_deref(), Some("active"));
        assert_eq!(pending.liveness.as_deref(), Some("live"));
        assert_eq!(pending.liveness_evidence.as_deref(), Some("pending_intent"));
        assert!(pending.active);

        sqlx::query("UPDATE refinement_dispatch_intents SET state = 'claimed', claimed_by = 'test', claimed_at = '2000-01-01T00:00:00.000Z', claim_expires_at = '2999-01-01T00:00:00.000Z' WHERE run_id = $1")
            .bind(&run_id).execute(db.pool()).await.unwrap();
        let claimed = build_refinement_status(&repo, &proposal.id).await.unwrap();
        assert_eq!(claimed.run_id.as_deref(), Some(run_id.as_str()));
        assert_eq!(claimed.liveness.as_deref(), Some("live"));
        assert_eq!(claimed.liveness_evidence.as_deref(), Some("claimed_intent"));
        assert!(claimed.active);

        sqlx::query("UPDATE refinement_dispatch_intents SET state = 'materialized', claimed_by = NULL, claimed_at = NULL, claim_expires_at = NULL WHERE run_id = $1")
            .bind(&run_id).execute(db.pool()).await.unwrap();
        sqlx::query("UPDATE refinement_runs SET heartbeat_at = '2000-01-01T00:00:00.000Z' WHERE id = $1")
            .bind(&run_id).execute(db.pool()).await.unwrap();
        let (project_id, task_id) = seed_status_task(&db, &run_id, "open").await;
        for (task_status, evidence) in [
            ("open", "open_task"), ("queued", "queued_task"),
            ("in_progress", "running_task"), ("pool_paused", "pool_paused_task"),
        ] {
            sqlx::query("UPDATE tasks SET status = $1 WHERE id = $2")
                .bind(task_status).bind(&task_id).execute(db.pool()).await.unwrap();
            let status = build_refinement_status(&repo, &proposal.id).await.unwrap();
            assert_eq!(status.run_id.as_deref(), Some(run_id.as_str()));
            assert_eq!(status.liveness.as_deref(), Some("live"));
            assert_eq!(status.liveness_evidence.as_deref(), Some(evidence));
            assert!(status.active);
        }

        sqlx::query("UPDATE tasks SET status = 'closed' WHERE id = $1")
            .bind(&task_id).execute(db.pool()).await.unwrap();
        let session_id = seed_live_status_session(&db, &project_id, &task_id).await;
        let session = build_refinement_status(&repo, &proposal.id).await.unwrap();
        assert_eq!(session.run_id.as_deref(), Some(run_id.as_str()));
        assert_eq!(session.liveness.as_deref(), Some("live"));
        assert_eq!(session.liveness_evidence.as_deref(), Some("live_session"));
        assert!(session.active);
        sqlx::query("UPDATE sessions SET status = 'completed' WHERE id = $1")
            .bind(&session_id).execute(db.pool()).await.unwrap();
        sqlx::query("UPDATE refinement_runs SET heartbeat_at = to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $1")
            .bind(&run_id).execute(db.pool()).await.unwrap();
        let heartbeat = build_refinement_status(&repo, &proposal.id).await.unwrap();
        assert_eq!(heartbeat.run_id.as_deref(), Some(run_id.as_str()));
        assert_eq!(heartbeat.liveness.as_deref(), Some("live"));
        assert_eq!(heartbeat.liveness_evidence.as_deref(), Some("fresh_heartbeat"));
        assert!(heartbeat.active);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_only_unresolved_review_keeps_display_fields_but_not_liveness() {
        let (_server, db) = test_server().await;
        let repo = ProposalRepository::new(db, EventBus::noop());
        let proposal = repo.create(ProposalCreateInput {
            title: "Legacy review display", body: "body", acceptance_criteria: Some("[]"),
            status: Some("draft"), body_format: None,
        }).await.unwrap();
        repo.record_refinement_lifecycle(&proposal.id, "refinement_start", None).await.unwrap();
        repo.record_refinement_lifecycle(&proposal.id, "refinement_awaiting_review", Some(&serde_json::json!({
            "judge_summary": "legacy verdict", "snapshot_revision_seq": 7
        }))).await.unwrap();
        let status = build_refinement_status(&repo, &proposal.id).await.unwrap();
        assert!(!status.active);
        assert!(status.run_id.is_none());
        assert!(status.liveness.is_none());
        assert!(status.current_round.is_none());
        assert!(status.awaiting_review);
        assert_eq!(status.judge_summary.as_deref(), Some("legacy verdict"));
        assert_eq!(status.snapshot_revision_seq, Some(7));

        repo.record_refinement_lifecycle(&proposal.id, "refinement_stop", Some(&serde_json::json!({"reason_tag": "operator_stop"}))).await.unwrap();
        let resolved = build_refinement_status(&repo, &proposal.id).await.unwrap();
        assert!(!resolved.awaiting_review);
        assert!(resolved.judge_summary.is_none());
        assert!(resolved.snapshot_revision_seq.is_none());
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

        sqlx::query("UPDATE refinement_runs SET park_kind = 'awaiting_evidence' WHERE id = $1")
            .bind(&run_id).execute(db.pool()).await.unwrap();
        let awaiting_evidence = build_refinement_status(&repo, &proposal.id).await.unwrap();
        assert_eq!(awaiting_evidence.liveness_evidence.as_deref(), Some("awaiting_evidence_park"));
        assert!(!awaiting_evidence.awaiting_review);

        sqlx::query(r#"UPDATE refinement_runs SET state = 'terminal', parked_at = NULL, park_kind = NULL, terminal_at = '2026-01-01T00:00:00.000Z', stop_tag = 'operator_stop', stop_context = '{"actor": "test", "reason": null}'::jsonb WHERE id = $1"#)
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
        let (project_id, prior_task) = seed_status_task(&db, &first, "closed").await;
        let prior_session = seed_live_status_session(&db, &project_id, &prior_task).await;
        sqlx::query(r#"UPDATE refinement_runs SET state = 'terminal', terminal_at = '2026-01-01T00:00:00.000Z', stop_tag = 'operator_stop', stop_context = '{"actor": "test", "reason": null}'::jsonb WHERE id = $1"#)
            .bind(&first).execute(db.pool()).await.unwrap();
        let second = match repo.reap_and_admit(admit("current-run")).await.unwrap() {
            djinn_db::RefinementAdmissionOutcome::Admitted { run_id, .. } => run_id,
            _ => unreachable!(),
        };
        djinn_db::test_support::make_refinement_run_phantom_for_test(&db, &second).await;
        let status = build_refinement_status(&repo, &proposal.id).await.unwrap();
        assert_eq!(status.run_id.as_deref(), Some(second.as_str()));
        assert_eq!(status.liveness.as_deref(), Some("stale"));
        assert!(!status.active);
        assert_eq!(sqlx::query_scalar::<_, String>("SELECT status FROM sessions WHERE id = $1").bind(&prior_session).fetch_one(db.pool()).await.unwrap(), "running");
        assert_ne!(status.stop_reason.as_deref(), Some("operator_stop"));
    }
