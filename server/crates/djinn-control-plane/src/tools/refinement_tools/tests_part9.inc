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

    /// Observe through the same fenced repository adapter as the registered
    /// phantom doctor. The shared evaluator, not this assertion helper, owns
    /// evidence precedence.
    async fn doctor_stale_run_ids(repo: &ProposalRepository) -> Vec<String> {
        let mut stale = repo
            .load_recoverable_refinement_run_snapshots(60_000)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|exact| {
                if matches!(
                    djinn_core::refinement_liveness::evaluate_refinement_liveness(
                        &exact.snapshot,
                        exact.observed_at,
                    ),
                    RefinementLivenessResult::Stale { .. }
                ) {
                    Some(exact.snapshot.run.run_id)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        stale.sort();
        stale
    }

    #[test]
    fn exact_status_serializes_every_shared_liveness_evidence_class() {
        let cases = [
            (RefinementLivenessEvidence::AwaitingReviewPark, "awaiting_review_park"),
            (RefinementLivenessEvidence::AwaitingEvidencePark, "awaiting_evidence_park"),
            (RefinementLivenessEvidence::PendingIntent { intent_id: "i".into() }, "pending_intent"),
            (RefinementLivenessEvidence::ClaimedIntent { intent_id: "i".into() }, "claimed_intent"),
            (RefinementLivenessEvidence::MaterializedIntent { intent_id: "i".into() }, "materialized_intent"),
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
        // A materialized exact-run intent precedes all task, session, and
        // heartbeat fallbacks in the shared evaluator.
        for task_status in ["open", "queued", "in_progress", "pool_paused"] {
            sqlx::query("UPDATE tasks SET status = $1 WHERE id = $2")
                .bind(task_status).bind(&task_id).execute(db.pool()).await.unwrap();
            let status = build_refinement_status(&repo, &proposal.id).await.unwrap();
            assert_eq!(status.run_id.as_deref(), Some(run_id.as_str()));
            assert_eq!(status.liveness.as_deref(), Some("live"));
            assert_eq!(status.liveness_evidence.as_deref(), Some("materialized_intent"));
            assert!(status.active);
        }

        sqlx::query("UPDATE tasks SET status = 'closed' WHERE id = $1")
            .bind(&task_id).execute(db.pool()).await.unwrap();
        let session_id = seed_live_status_session(&db, &project_id, &task_id).await;
        let session = build_refinement_status(&repo, &proposal.id).await.unwrap();
        assert_eq!(session.run_id.as_deref(), Some(run_id.as_str()));
        assert_eq!(session.liveness.as_deref(), Some("live"));
        assert_eq!(session.liveness_evidence.as_deref(), Some("materialized_intent"));
        assert!(session.active);
        sqlx::query("UPDATE sessions SET status = 'completed' WHERE id = $1")
            .bind(&session_id).execute(db.pool()).await.unwrap();
        sqlx::query("UPDATE refinement_runs SET heartbeat_at = to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $1")
            .bind(&run_id).execute(db.pool()).await.unwrap();
        let heartbeat = build_refinement_status(&repo, &proposal.id).await.unwrap();
        assert_eq!(heartbeat.run_id.as_deref(), Some(run_id.as_str()));
        assert_eq!(heartbeat.liveness.as_deref(), Some("live"));
        assert_eq!(heartbeat.liveness_evidence.as_deref(), Some("materialized_intent"));
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

    /// Exercise status, doctor, board health, and both admissions against the historic
    /// 09no/96fy shape. The live session belongs only to a terminal prior run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cross_surface_phantom_reads_are_pure_and_admission_reaps_once() {
        for (name, tool) in [
            ("start", "proposal_refinement_start"),
            ("demand", "proposal_refinement_demand_round"),
        ] {
            let (server, db) = test_server().await;
            let repo = ProposalRepository::new(db.clone(), EventBus::noop());
            let proposal = repo.create(ProposalCreateInput {
                title: "09no 96fy exact-run phantom", body: "body", acceptance_criteria: Some("[]"),
                status: Some("draft"), body_format: None,
            }).await.unwrap();
            let project_id = link_proposal_to_project(&db, &repo, &proposal.id).await;
            let admission = |key: String| djinn_db::AdmitRefinementRunRequest {
                proposal_id: proposal.id.clone(), idempotency_key: key.clone(),
                source: djinn_db::RefinementAdmissionSource::Demand { demand_id: key },
                heartbeat_grace_millis: 60_000,
            };

            let prior = match repo.reap_and_admit(admission(format!("{name}-prior"))).await.unwrap() {
                djinn_db::RefinementAdmissionOutcome::Admitted { run_id, .. } => run_id,
                _ => unreachable!(),
            };
            let (prior_project, prior_task) = seed_status_task(&db, &prior, "closed").await;
            let _prior_session = seed_live_status_session(&db, &prior_project, &prior_task).await;
            sqlx::query("UPDATE refinement_runs SET state = 'terminal', terminal_at = '2000-01-01T00:00:00.000Z', stop_tag = 'operator_stop' WHERE id = $1")
                .bind(&prior).execute(db.pool()).await.unwrap();
            let phantom = match repo.reap_and_admit(admission(format!("{name}-phantom"))).await.unwrap() {
                djinn_db::RefinementAdmissionOutcome::Admitted { run_id, .. } => run_id,
                _ => unreachable!(),
            };
            djinn_db::test_support::make_refinement_run_phantom_for_test(&db, &phantom).await;
            let before = djinn_db::test_support::refinement_run_read_only_snapshot_for_test(&db, &proposal.id, &phantom).await;

            for _ in 0..2 {
                let status = server.dispatch_tool("proposal_refinement_status", serde_json::json!({ "proposal_id": proposal.id })).await.unwrap();
                assert_eq!(status["refinement"]["run_id"], phantom);
                assert_eq!(status["refinement"]["liveness"], "stale");
                assert_eq!(doctor_stale_run_ids(&repo).await, vec![phantom.clone()],
                    "{name} doctor observation must ignore terminal prior-run evidence");
                let board = server.dispatch_tool("board_health", serde_json::json!({
                    "project": format!("test-owner/test-repo-{project_id}")
                })).await.unwrap();
                assert_eq!(board["refinement_phantom_active_count"], 1, "{name} board count");
            }
            assert_eq!(djinn_db::test_support::refinement_run_read_only_snapshot_for_test(&db, &proposal.id, &phantom).await, before,
                "{name}: reads must not reap or mutate the phantom");

            let args = match tool {
                "proposal_refinement_start" => serde_json::json!({
                    "proposal_id": proposal.id, "request_id": format!("09no-96fy-{name}")
                }),
                "proposal_refinement_demand_round" => serde_json::json!({
                    "proposal_id": proposal.id, "reason": "recover exact stale phantom",
                    "request_id": format!("09no-96fy-{name}")
                }),
                _ => unreachable!(),
            };
            for _ in 0..2 {
                let response = server.dispatch_tool(tool, args.clone()).await.unwrap();
                assert_eq!(response["error"], "accepted; dispatch pending", "{name} post-commit wake failure");
            }

            let runs = repo.load_refinement_run_aggregates(&proposal.id, 60_000).await.unwrap();
            assert_eq!(runs.len(), 3, "{name}: prior, phantom, one successor");
            let successor = runs.iter().find(|run| run.run_id != prior && run.run_id != phantom).unwrap();
            let snapshot = repo.load_refinement_run_snapshot(djinn_db::LoadRefinementRunSnapshotRequest {
                run_id: successor.run_id.clone(), heartbeat_grace_millis: 60_000,
            }).await.unwrap().unwrap();
            assert_eq!(snapshot.snapshot.intents.len(), 1, "{name}: exactly one pending intent");
            assert_eq!(snapshot.snapshot.intents[0].state, djinn_core::refinement_liveness::RefinementIntentState::Pending);
            let revisions = repo.revisions(&proposal.id).await.unwrap();
            assert_eq!(revisions.iter().filter(|row| row.refinement_stop_tag.as_deref() == Some("reaped_phantom")).count(), 1);
            assert_eq!(revisions.iter().filter(|row| row.event_kind == "refinement_stop").count(), 1, "{name}: no handler-error stop");
            assert_eq!(revisions.iter().filter(|row| row.event_kind == "refinement_start").count(), 3, "{name}: retry returns the winner");
            let board = server.dispatch_tool("board_health", serde_json::json!({
                "project": format!("test-owner/test-repo-{project_id}")
            })).await.unwrap();
            assert_eq!(board["refinement_phantom_active_count"], 0, "{name}: reap clears current stale count");
            assert_eq!(board["refinement_phantom_reaps_24h"], 1, "{name}: one durable bounded reap increment");
        }
    }
