// djinn:allow-oversize
//
// Cross-surface exact-run liveness regressions: the same run projected
// identically through status, doctor, and board. The suite's whole point is
// that these surfaces are asserted together, so splitting it by surface would
// destroy what it tests.
//
// This file is over the BYTE threshold only, and only after this commit: as an
// `.inc` fragment it was invisible to `cargo fmt`, and rustfmt's first pass
// over it re-wrapped the long fixture calls that #2817 introduced, growing it
// from 50448 to 56403 bytes without adding a single statement. It is declared
// oversized rather than re-fragmented — the growth is formatting the file
// should have had all along.

use super::*;
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
    // Deliberately partial: run id only, no intent identity. That is the
    // durable shape that makes the task itself the liveness evidence.
    djinn_db::test_support::correlate_task_to_refinement_run_for_test(db, &task_id, run_id).await;
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
    )
    .await;
    djinn_db::SessionRepository::new(db.clone(), EventBus::noop())
        .set_running(&session_id)
        .await
        .unwrap();
    session_id
}

/// Observe through the registered production doctor source and check. In
/// particular, `refresh` retains the repository's exact-generation fence;
/// this fixture must not recreate that scan/evaluation path in test code.
fn doctor_stale_run_ids(
    source: std::sync::Arc<
        djinn_coordinator::doctor::ProposalRepositoryRefinementPhantomActiveSource,
    >,
) -> Vec<String> {
    use djinn_core::doctor::DoctorCheck;

    let mut stale = djinn_coordinator::doctor::RefinementPhantomActiveCheck::new(source)
        .run()
        .unwrap()
        .into_iter()
        .filter_map(|finding| {
            finding
                .evidence
                .get("run_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    stale.sort();
    stale
}

/// Seed one exact current run in one of the durable park states. The park
/// kind is data supplied by the caller, so this fixture does not recreate
/// the evaluator's evidence ordering.
async fn seed_parked_cross_surface_run(
    db: &Database,
    repo: &ProposalRepository,
    proposal_id: &str,
    key: &str,
    kind: djinn_core::refinement_liveness::RefinementParkKind,
) -> String {
    let (run_id, intent_id, generation) = match repo
        .reap_and_admit(djinn_db::AdmitRefinementRunRequest {
            proposal_id: proposal_id.to_owned(),
            idempotency_key: key.into(),
            source: djinn_db::RefinementAdmissionSource::Demand {
                demand_id: key.into(),
            },
            heartbeat_grace_millis: 60_000,
        })
        .await
        .unwrap()
    {
        djinn_db::RefinementAdmissionOutcome::Admitted {
            run_id,
            intent_id,
            generation,
            ..
        } => (run_id, intent_id, generation),
        djinn_db::RefinementAdmissionOutcome::Existing { .. } => unreachable!(),
    };
    // Admission creates a pending intent. Claim and consume that exact
    // source intent with the production park transition, rather than
    // parking only the run row and leaving alternate live intent evidence.
    assert!(
        repo.claim_refinement_intent(djinn_db::ClaimRefinementIntentRequest {
            run_id: run_id.clone(),
            intent_id: intent_id.clone(),
            generation,
            owner: "parked-cross-surface-fixture".into(),
            lease_millis: 60_000,
        })
        .await
        .unwrap()
        .is_some()
    );
    assert!(
        repo.park_refinement_run_from_intent(djinn_db::ParkRefinementRunFromIntentRequest {
            source: djinn_db::SourceIntentTransitionRequest {
                run_id: run_id.clone(),
                intent_id,
                generation,
                expected_round: 1,
                expected_phase: djinn_core::refinement_liveness::RefinementPhase::AdversaryAttack,
                expected_role: djinn_core::refinement_liveness::RefinementRole::Adversary,
            },
            kind,
        })
        .await
        .unwrap()
    );
    // The park transition intentionally preserves the most recent durable
    // heartbeat. Age it so this fixture has no fallback live evidence.
    djinn_db::test_support::elapse_refinement_run_wall_clock_for_test(db, &run_id, 3_600).await;
    run_id
}

/// A feedback cohort committed while role work is in flight must drain when
/// that role parks for human review. Retrying the already-consumed outcome is
/// a no-op: it cannot mint another successor or capture the source twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn awaiting_review_park_drains_pending_feedback_cohort_exactly_once() {
    let (_server, db) = test_server().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = repo
        .create(ProposalCreateInput {
            title: "Pending feedback review park",
            body: "body",
            acceptance_criteria: Some("[]"),
            status: Some("in_review"),
            body_format: None,
        })
        .await
        .unwrap();
    let (run_id, intent_id, generation) = match repo
        .reap_and_admit(djinn_db::AdmitRefinementRunRequest {
            proposal_id: proposal.id.clone(),
            idempotency_key: "review-park-source".into(),
            source: djinn_db::RefinementAdmissionSource::Demand {
                demand_id: "review-park-source".into(),
            },
            heartbeat_grace_millis: 60_000,
        })
        .await
        .unwrap()
    {
        djinn_db::RefinementAdmissionOutcome::Admitted {
            run_id,
            intent_id,
            generation,
        } => (run_id, intent_id, generation),
        other => panic!("expected source admission, got {other:?}"),
    };
    let (feedback, persisted) = repo
        .add_feedback_with_severity_and_pending_handoff(
            djinn_db::ProposalFeedbackCreateInput {
                proposal_id: &proposal.id,
                parent_id: None,
                author_kind: "user",
                author_model: None,
                body: "blocking feedback while judge is running",
            },
            "blocking",
            true,
        )
        .await
        .unwrap();
    assert!(persisted);
    assert!(
        repo.claim_refinement_intent(djinn_db::ClaimRefinementIntentRequest {
            run_id: run_id.clone(),
            intent_id: intent_id.clone(),
            generation,
            owner: "review-park-owner".into(),
            lease_millis: 60_000,
        })
        .await
        .unwrap()
        .is_some()
    );
    let transition = djinn_db::SourceIntentTransitionRequest {
        run_id: run_id.clone(),
        intent_id: intent_id.clone(),
        generation,
        expected_round: 1,
        expected_phase: djinn_core::refinement_liveness::RefinementPhase::AdversaryAttack,
        expected_role: djinn_core::refinement_liveness::RefinementRole::Adversary,
    };
    assert!(
        repo.park_refinement_run_from_intent(djinn_db::ParkRefinementRunFromIntentRequest {
            source: transition.clone(),
            kind: djinn_core::refinement_liveness::RefinementParkKind::AwaitingReview,
        })
        .await
        .unwrap()
    );
    assert!(
        !repo
            .park_refinement_run_from_intent(djinn_db::ParkRefinementRunFromIntentRequest {
                source: transition,
                kind: djinn_core::refinement_liveness::RefinementParkKind::AwaitingReview,
            })
            .await
            .unwrap()
    );

    let successor_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM refinement_dispatch_intents i JOIN refinement_runs r ON r.id=i.run_id WHERE r.proposal_id=$1 AND r.idempotency_key=$2",
    )
    .bind(&proposal.id)
    .bind(format!("pending-feedback/{}", feedback.id))
    .fetch_one(db.pool())
    .await
    .unwrap();
    let pending_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM pending_feedback_refinement_handoffs WHERE proposal_id=$1 AND state='pending'",
    )
    .bind(&proposal.id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    let captured_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM proposal_feedback_refinement_sources WHERE source_feedback_id=$1",
    )
    .bind(&feedback.id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(successor_count, 1, "one successor admission is durable");
    assert_eq!(pending_count, 0, "park drained the pending cohort");
    assert_eq!(captured_count, 1, "feedback source was captured once");
}

#[test]
fn exact_status_serializes_every_shared_liveness_evidence_class() {
    let cases = [
        (
            RefinementLivenessEvidence::AwaitingReviewPark,
            "awaiting_review_park",
        ),
        (
            RefinementLivenessEvidence::AwaitingEvidencePark,
            "awaiting_evidence_park",
        ),
        (
            RefinementLivenessEvidence::PendingIntent {
                intent_id: "i".into(),
            },
            "pending_intent",
        ),
        (
            RefinementLivenessEvidence::ClaimedIntent {
                intent_id: "i".into(),
            },
            "claimed_intent",
        ),
        (
            RefinementLivenessEvidence::MaterializedIntent {
                intent_id: "i".into(),
            },
            "materialized_intent",
        ),
        (
            RefinementLivenessEvidence::OpenTask {
                task_id: "t".into(),
            },
            "open_task",
        ),
        (
            RefinementLivenessEvidence::QueuedTask {
                task_id: "t".into(),
            },
            "queued_task",
        ),
        (
            RefinementLivenessEvidence::RunningTask {
                task_id: "t".into(),
            },
            "running_task",
        ),
        (
            RefinementLivenessEvidence::PoolPausedTask {
                task_id: "t".into(),
            },
            "pool_paused_task",
        ),
        (
            RefinementLivenessEvidence::LiveSession {
                session_id: "s".into(),
                task_id: "t".into(),
            },
            "live_session",
        ),
        (
            RefinementLivenessEvidence::BetweenPhase {
                intent_id: "i".into(),
            },
            "between_phase",
        ),
        (
            RefinementLivenessEvidence::FreshHeartbeat {
                heartbeat_at: DbTimestamp(1),
            },
            "fresh_heartbeat",
        ),
    ];
    for (evidence, expected) in cases {
        let (state, serialized) =
            crate::tools::refinement_helpers::liveness_fields(&RefinementLivenessResult::Live {
                evidence,
            });
        assert_eq!(state, "live");
        assert_eq!(serialized.as_deref(), Some(expected));
    }
}

/// This matrix derives every expected category from the shared evaluator;
/// fixture metadata chooses durable evidence without recreating precedence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporal_exact_run_matrix_projects_identically_across_read_surfaces() {
    #[derive(Clone, Copy)]
    enum Fixture {
        Session,
        Handoff,
        Heartbeat,
        ExpiredClaim,
        PriorOnly,
        Terminal,
        Stale,
    }
    let cases = [
        (
            "exact-run live session",
            Fixture::Session,
            "live_session",
            false,
        ),
        // Pending intent is the evaluator's earlier category; the exact
        // snapshot below also proves the correlated handoff is present.
        (
            "between-phase handoff",
            Fixture::Handoff,
            "pending_intent",
            false,
        ),
        (
            "heartbeat within grace",
            Fixture::Heartbeat,
            "fresh_heartbeat",
            false,
        ),
        (
            "expired claimed intent",
            Fixture::ExpiredClaim,
            "stale",
            true,
        ),
        (
            "prior-run-only live session",
            Fixture::PriorOnly,
            "stale",
            true,
        ),
        ("terminal current run", Fixture::Terminal, "terminal", false),
        ("truly stale current run", Fixture::Stale, "stale", true),
    ];

    for (name, fixture, category, stale) in cases {
        let (_server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Temporal exact-run matrix",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: Some("draft"),
                body_format: None,
            })
            .await
            .unwrap();
        let admit = |key: &str| djinn_db::AdmitRefinementRunRequest {
            proposal_id: proposal.id.clone(),
            idempotency_key: key.into(),
            source: djinn_db::RefinementAdmissionSource::Demand {
                demand_id: key.into(),
            },
            heartbeat_grace_millis: 60_000,
        };
        let (mut run_id, intent_id, generation) = match repo
            .reap_and_admit(admit("temporal-current"))
            .await
            .unwrap()
        {
            djinn_db::RefinementAdmissionOutcome::Admitted {
                run_id,
                intent_id,
                generation,
                ..
            } => (run_id, intent_id, generation),
            djinn_db::RefinementAdmissionOutcome::Existing { .. } => unreachable!(),
        };
        match fixture {
            Fixture::Session => {
                djinn_db::test_support::make_refinement_run_phantom_for_test(&db, &run_id).await;
                let (project_id, task_id) = seed_status_task(&db, &run_id, "closed").await;
                seed_live_status_session(&db, &project_id, &task_id).await;
            }
            Fixture::Handoff => {}
            Fixture::Heartbeat => {
                djinn_db::test_support::make_refinement_run_phantom_for_test(&db, &run_id).await;
                // The only durable heartbeat writer: it stamps
                // `transaction_timestamp()` on the exact generation.
                repo.record_refinement_durable_progress(
                    &run_id,
                    generation,
                    djinn_db::RefinementDurableProgress::DebateAppend,
                )
                .await
                .unwrap();
            }
            Fixture::ExpiredClaim => {
                assert!(
                    repo.claim_refinement_intent(djinn_db::ClaimRefinementIntentRequest {
                        run_id: run_id.clone(),
                        intent_id,
                        generation,
                        owner: "temporal-matrix".into(),
                        lease_millis: 60_000,
                    })
                    .await
                    .unwrap()
                    .is_some()
                );
                djinn_db::test_support::elapse_refinement_run_wall_clock_for_test(
                    &db, &run_id, 3_600,
                )
                .await;
            }
            Fixture::PriorOnly => {
                djinn_db::test_support::make_refinement_run_phantom_for_test(&db, &run_id).await;
                let (project_id, task_id) = seed_status_task(&db, &run_id, "closed").await;
                seed_live_status_session(&db, &project_id, &task_id).await;
                assert!(
                    repo.terminal_refinement_run(djinn_db::TerminalRefinementRunRequest {
                        run_id: run_id.clone(),
                        generation,
                        reason:
                            djinn_core::refinement_liveness::RefinementStopReason::OperatorStop {
                                actor: "temporal-matrix".into(),
                                reason: None
                            },
                    })
                    .await
                    .unwrap()
                );
                run_id = match repo
                    .reap_and_admit(admit("temporal-successor"))
                    .await
                    .unwrap()
                {
                    djinn_db::RefinementAdmissionOutcome::Admitted { run_id, .. } => run_id,
                    djinn_db::RefinementAdmissionOutcome::Existing { .. } => unreachable!(),
                };
                djinn_db::test_support::make_refinement_run_phantom_for_test(&db, &run_id).await;
            }
            Fixture::Terminal => {
                assert!(
                    repo.terminal_refinement_run(djinn_db::TerminalRefinementRunRequest {
                        run_id: run_id.clone(),
                        generation,
                        reason:
                            djinn_core::refinement_liveness::RefinementStopReason::OperatorStop {
                                actor: "temporal-matrix".into(),
                                reason: None
                            },
                    })
                    .await
                    .unwrap()
                );
            }
            Fixture::Stale => {
                djinn_db::test_support::make_refinement_run_phantom_for_test(&db, &run_id).await
            }
        }

        let exact = repo
            .load_refinement_run_snapshot(djinn_db::LoadRefinementRunSnapshotRequest {
                run_id: run_id.clone(),
                heartbeat_grace_millis: 60_000,
            })
            .await
            .unwrap()
            .unwrap();
        let expected = djinn_core::refinement_liveness::evaluate_refinement_liveness(
            &exact.snapshot,
            exact.observed_at,
        );
        assert_eq!(
            expected, exact.liveness,
            "{name}: repository shared evaluator"
        );
        assert!(
            match category {
                "live_session" => matches!(
                    expected,
                    RefinementLivenessResult::Live {
                        evidence: RefinementLivenessEvidence::LiveSession { .. }
                    }
                ),
                "pending_intent" => matches!(
                    expected,
                    RefinementLivenessResult::Live {
                        evidence: RefinementLivenessEvidence::PendingIntent { .. }
                    }
                ),
                "fresh_heartbeat" => matches!(
                    expected,
                    RefinementLivenessResult::Live {
                        evidence: RefinementLivenessEvidence::FreshHeartbeat { .. }
                    }
                ),
                "terminal" => matches!(expected, RefinementLivenessResult::Terminal { .. }),
                "stale" => matches!(expected, RefinementLivenessResult::Stale { .. }),
                _ => unreachable!(),
            },
            "{name}: shared category"
        );
        if matches!(fixture, Fixture::Handoff) {
            assert!(
                exact.snapshot.between_phase.is_some(),
                "{name}: durable handoff"
            );
        }

        let status = build_refinement_status(&repo, &proposal.id).await.unwrap();
        assert_eq!(
            status.run_id.as_deref(),
            Some(run_id.as_str()),
            "{name}: status run"
        );
        let status_liveness = if matches!(expected, RefinementLivenessResult::Live { .. }) {
            "live"
        } else {
            category
        };
        assert_eq!(
            status.liveness.as_deref(),
            Some(status_liveness),
            "{name}: status category"
        );
        assert_eq!(
            status.liveness_evidence.as_deref(),
            matches!(expected, RefinementLivenessResult::Live { .. }).then_some(category),
            "{name}: status evidence",
        );
        assert_eq!(
            status.active,
            !stale && category != "terminal",
            "{name}: status active"
        );
        let (events_tx, _events_rx) = tokio::sync::broadcast::channel(16);
        let source = std::sync::Arc::new(
            djinn_coordinator::doctor::ProposalRepositoryRefinementPhantomActiveSource::new(
                db.clone(),
                events_tx,
            ),
        );
        assert_eq!(
            doctor_stale_run_ids(source).contains(&run_id),
            stale,
            "{name}: doctor projection"
        );
        assert_eq!(
            repo.load_board_refinement_lifecycle_aggregate(60_000)
                .await
                .unwrap()
                .stale_run_count,
            i64::from(stale),
            "{name}: board projection"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_status_observes_repository_intent_task_session_and_heartbeat_evidence() {
    let (_server, db) = test_server().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = repo
        .create(ProposalCreateInput {
            title: "Exact evidence status",
            body: "body",
            acceptance_criteria: Some("[]"),
            status: Some("draft"),
            body_format: None,
        })
        .await
        .unwrap();
    let (run_id, intent_id, generation) = match repo
        .reap_and_admit(djinn_db::AdmitRefinementRunRequest {
            proposal_id: proposal.id.clone(),
            idempotency_key: "status-evidence".into(),
            source: djinn_db::RefinementAdmissionSource::Demand {
                demand_id: "status-evidence".into(),
            },
            heartbeat_grace_millis: 60_000,
        })
        .await
        .unwrap()
    {
        djinn_db::RefinementAdmissionOutcome::Admitted {
            run_id,
            intent_id,
            generation,
            ..
        } => (run_id, intent_id, generation),
        _ => unreachable!(),
    };
    let owner = "status-evidence-fixture".to_owned();
    let pending = build_refinement_status(&repo, &proposal.id).await.unwrap();
    assert_eq!(pending.run_id.as_deref(), Some(run_id.as_str()));
    assert_eq!(pending.run_state.as_deref(), Some("active"));
    assert_eq!(pending.liveness.as_deref(), Some("live"));
    assert_eq!(pending.liveness_evidence.as_deref(), Some("pending_intent"));
    assert!(pending.active);

    let lease = repo
        .claim_refinement_intent(djinn_db::ClaimRefinementIntentRequest {
            run_id: run_id.clone(),
            intent_id: intent_id.clone(),
            generation,
            owner: owner.clone(),
            lease_millis: 60_000,
        })
        .await
        .unwrap()
        .unwrap();
    let claimed = build_refinement_status(&repo, &proposal.id).await.unwrap();
    assert_eq!(claimed.run_id.as_deref(), Some(run_id.as_str()));
    assert_eq!(claimed.liveness.as_deref(), Some("live"));
    assert_eq!(claimed.liveness_evidence.as_deref(), Some("claimed_intent"));
    assert!(claimed.active);

    // Materialization is only reachable through the repository once the
    // task already carries the exact correlation, so the task is seeded
    // and correlated before the acknowledgement rather than after it.
    let (project_id, task_id) = seed_status_task(&db, &run_id, "open").await;
    djinn_db::TaskRepository::new(db.clone(), EventBus::noop())
        .set_refinement_correlation(
            &task_id,
            Some(
                &djinn_core::models::TaskRefinementCorrelation::new(
                    run_id.clone(),
                    intent_id.clone(),
                    i64::from(generation),
                    i64::from(lease.round),
                    lease.phase,
                    lease.role,
                )
                .unwrap(),
            ),
        )
        .await
        .unwrap();
    assert!(
        repo.acknowledge_refinement_task_materialization(
            djinn_db::AcknowledgeRefinementTaskMaterializationRequest {
                run_id: run_id.clone(),
                intent_id: intent_id.clone(),
                generation,
                task_id: task_id.clone(),
                owner: owner.clone(),
            }
        )
        .await
        .unwrap()
    );
    // Acknowledgement advances the heartbeat once; age it back out of grace
    // so no fresh-heartbeat fallback can stand in for the intent below.
    djinn_db::test_support::elapse_refinement_run_wall_clock_for_test(&db, &run_id, 3_600).await;
    let task_repo = djinn_db::TaskRepository::new(db.clone(), EventBus::noop());
    // A materialized exact-run intent precedes all task, session, and
    // heartbeat fallbacks in the shared evaluator.
    for task_status in ["open", "queued", "in_progress", "pool_paused"] {
        task_repo.set_status(&task_id, task_status).await.unwrap();
        let status = build_refinement_status(&repo, &proposal.id).await.unwrap();
        assert_eq!(status.run_id.as_deref(), Some(run_id.as_str()));
        assert_eq!(status.liveness.as_deref(), Some("live"));
        assert_eq!(
            status.liveness_evidence.as_deref(),
            Some("materialized_intent")
        );
        assert!(status.active);
    }

    task_repo.set_status(&task_id, "closed").await.unwrap();
    let session_id = seed_live_status_session(&db, &project_id, &task_id).await;
    let session = build_refinement_status(&repo, &proposal.id).await.unwrap();
    assert_eq!(session.run_id.as_deref(), Some(run_id.as_str()));
    assert_eq!(session.liveness.as_deref(), Some("live"));
    assert_eq!(
        session.liveness_evidence.as_deref(),
        Some("materialized_intent")
    );
    assert!(session.active);
    djinn_db::SessionRepository::new(db.clone(), EventBus::noop())
        .update(
            &session_id,
            djinn_core::models::SessionStatus::Completed,
            0,
            0,
            0,
            0,
            None,
        )
        .await
        .unwrap();
    repo.record_refinement_durable_progress(
        &run_id,
        generation,
        djinn_db::RefinementDurableProgress::DebateAppend,
    )
    .await
    .unwrap();
    let heartbeat = build_refinement_status(&repo, &proposal.id).await.unwrap();
    assert_eq!(heartbeat.run_id.as_deref(), Some(run_id.as_str()));
    assert_eq!(heartbeat.liveness.as_deref(), Some("live"));
    assert_eq!(
        heartbeat.liveness_evidence.as_deref(),
        Some("materialized_intent")
    );
    assert!(heartbeat.active);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_only_unresolved_review_keeps_display_fields_but_not_liveness() {
    let (_server, db) = test_server().await;
    let repo = ProposalRepository::new(db, EventBus::noop());
    let proposal = repo
        .create(ProposalCreateInput {
            title: "Legacy review display",
            body: "body",
            acceptance_criteria: Some("[]"),
            status: Some("draft"),
            body_format: None,
        })
        .await
        .unwrap();
    repo.record_refinement_lifecycle(&proposal.id, "refinement_start", None)
        .await
        .unwrap();
    repo.record_refinement_lifecycle(
        &proposal.id,
        "refinement_awaiting_review",
        Some(&serde_json::json!({
            "judge_summary": "legacy verdict", "snapshot_revision_seq": 7
        })),
    )
    .await
    .unwrap();
    let status = build_refinement_status(&repo, &proposal.id).await.unwrap();
    assert!(!status.active);
    assert!(status.run_id.is_none());
    assert!(status.liveness.is_none());
    assert!(status.current_round.is_none());
    assert!(status.awaiting_review);
    assert_eq!(status.judge_summary.as_deref(), Some("legacy verdict"));
    assert_eq!(status.snapshot_revision_seq, Some(7));

    repo.record_refinement_lifecycle(
        &proposal.id,
        "refinement_stop",
        Some(&serde_json::json!({"reason_tag": "operator_stop"})),
    )
    .await
    .unwrap();
    let resolved = build_refinement_status(&repo, &proposal.id).await.unwrap();
    assert!(!resolved.awaiting_review);
    assert!(resolved.judge_summary.is_none());
    assert!(resolved.snapshot_revision_seq.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_run_status_is_read_only_for_phantom_parked_and_terminal_runs() {
    let (_server, db) = test_server().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = repo
        .create(ProposalCreateInput {
            title: "Exact status snapshot",
            body: "body",
            acceptance_criteria: Some("[]"),
            status: Some("draft"),
            body_format: None,
        })
        .await
        .unwrap();
    let (run_id, intent_id, generation) = match repo
        .reap_and_admit(djinn_db::AdmitRefinementRunRequest {
            proposal_id: proposal.id.clone(),
            idempotency_key: "exact-status-fixture".into(),
            source: djinn_db::RefinementAdmissionSource::Demand {
                demand_id: "exact-status-fixture".into(),
            },
            heartbeat_grace_millis: 60_000,
        })
        .await
        .unwrap()
    {
        djinn_db::RefinementAdmissionOutcome::Admitted {
            run_id,
            intent_id,
            generation,
            ..
        } => (run_id, intent_id, generation),
        djinn_db::RefinementAdmissionOutcome::Existing { .. } => unreachable!(),
    };

    let live = build_refinement_status(&repo, &proposal.id).await.unwrap();
    assert_eq!(live.run_id.as_deref(), Some(run_id.as_str()));
    assert_eq!(live.liveness_evidence.as_deref(), Some("pending_intent"));

    // Status must evaluate the durable expired lease rather than treating
    // every claimed intent as live. Rewinding the run's wall clock expires
    // the real lease and ages the heartbeat out of grace together.
    assert!(
        repo.claim_refinement_intent(djinn_db::ClaimRefinementIntentRequest {
            run_id: run_id.clone(),
            intent_id,
            generation,
            owner: "exact-status-fixture".into(),
            lease_millis: 60_000,
        })
        .await
        .unwrap()
        .is_some()
    );
    djinn_db::test_support::elapse_refinement_run_wall_clock_for_test(&db, &run_id, 3_600).await;
    let expired_claim = build_refinement_status(&repo, &proposal.id).await.unwrap();
    assert_eq!(expired_claim.liveness.as_deref(), Some("stale"));

    djinn_db::test_support::make_refinement_run_phantom_for_test(&db, &run_id).await;
    let before = djinn_db::test_support::refinement_run_read_only_snapshot_for_test(
        &db,
        &proposal.id,
        &run_id,
    )
    .await;
    let stale = build_refinement_status(&repo, &proposal.id).await.unwrap();
    let repeated = build_refinement_status(&repo, &proposal.id).await.unwrap();
    let after = djinn_db::test_support::refinement_run_read_only_snapshot_for_test(
        &db,
        &proposal.id,
        &run_id,
    )
    .await;
    assert_eq!(stale.liveness.as_deref(), Some("stale"));
    assert_eq!(
        serde_json::to_value(&stale).unwrap(),
        serde_json::to_value(&repeated).unwrap(),
    );
    assert_eq!(before, after, "status must not mutate a stale phantom");

    // The park transition stamps a fresh heartbeat; age it straight back
    // out of grace so the park itself stays the only live evidence.
    assert!(
        repo.park_refinement_run(djinn_db::ParkRefinementRunRequest {
            run_id: run_id.clone(),
            generation,
            kind: djinn_core::refinement_liveness::RefinementParkKind::AwaitingReview,
        })
        .await
        .unwrap()
    );
    djinn_db::test_support::elapse_refinement_run_wall_clock_for_test(&db, &run_id, 3_600).await;
    let parked = build_refinement_status(&repo, &proposal.id).await.unwrap();
    assert_eq!(parked.run_state.as_deref(), Some("parked"));
    assert_eq!(
        parked.liveness_evidence.as_deref(),
        Some("awaiting_review_park")
    );
    assert!(parked.awaiting_review);

    // `park_refinement_run` fences on `state = 'running'`, so an already
    // parked run has no repository path to the other park kind.
    djinn_db::test_support::force_refinement_run_park_kind_for_test(
        &db,
        &run_id,
        djinn_core::refinement_liveness::RefinementParkKind::AwaitingEvidence,
    )
    .await;
    let awaiting_evidence = build_refinement_status(&repo, &proposal.id).await.unwrap();
    assert_eq!(
        awaiting_evidence.liveness_evidence.as_deref(),
        Some("awaiting_evidence_park")
    );
    assert!(!awaiting_evidence.awaiting_review);

    // A forced terminal keeps the run row the only evidence: the
    // repository transition would also append a `refinement_stop`
    // revision, which `build_refinement_status` reads as a display-only
    // stop-reason fallback.
    djinn_db::test_support::force_refinement_run_terminal_for_test(
        &db,
        &run_id,
        &djinn_core::refinement_liveness::RefinementStopReason::OperatorStop {
            actor: "test".into(),
            reason: None,
        },
        "2026-01-01T00:00:00.000Z",
    )
    .await;
    let terminal = build_refinement_status(&repo, &proposal.id).await.unwrap();
    assert_eq!(terminal.run_state.as_deref(), Some("terminal"));
    assert_eq!(terminal.liveness.as_deref(), Some("terminal"));
    assert_eq!(terminal.stop_reason.as_deref(), Some("operator_stop"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_status_uses_latest_generation_not_prior_run_evidence() {
    let (_server, db) = test_server().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = repo
        .create(ProposalCreateInput {
            title: "Prior run isolation",
            body: "body",
            acceptance_criteria: Some("[]"),
            status: Some("draft"),
            body_format: None,
        })
        .await
        .unwrap();
    let admit = |key: &str| djinn_db::AdmitRefinementRunRequest {
        proposal_id: proposal.id.clone(),
        idempotency_key: key.into(),
        source: djinn_db::RefinementAdmissionSource::Demand {
            demand_id: key.into(),
        },
        heartbeat_grace_millis: 60_000,
    };
    let first = match repo.reap_and_admit(admit("prior-run")).await.unwrap() {
        djinn_db::RefinementAdmissionOutcome::Admitted { run_id, .. } => run_id,
        _ => unreachable!(),
    };
    let (project_id, prior_task) = seed_status_task(&db, &first, "closed").await;
    let prior_session = seed_live_status_session(&db, &project_id, &prior_task).await;
    // Forced, not `terminal_refinement_run`: that transition also appends a
    // `refinement_stop` revision, and `build_refinement_status` falls back
    // to the latest such revision for `stop_reason` whenever the current
    // run has no terminal reason — which is exactly the leak this test
    // asserts does not happen.
    djinn_db::test_support::force_refinement_run_terminal_for_test(
        &db,
        &first,
        &djinn_core::refinement_liveness::RefinementStopReason::OperatorStop {
            actor: "test".into(),
            reason: None,
        },
        "2026-01-01T00:00:00.000Z",
    )
    .await;
    let second = match repo.reap_and_admit(admit("current-run")).await.unwrap() {
        djinn_db::RefinementAdmissionOutcome::Admitted { run_id, .. } => run_id,
        _ => unreachable!(),
    };
    djinn_db::test_support::make_refinement_run_phantom_for_test(&db, &second).await;
    let status = build_refinement_status(&repo, &proposal.id).await.unwrap();
    assert_eq!(status.run_id.as_deref(), Some(second.as_str()));
    assert_eq!(status.liveness.as_deref(), Some("stale"));
    assert!(!status.active);
    assert_eq!(
        djinn_db::SessionRepository::new(db.clone(), EventBus::noop())
            .get(&prior_session)
            .await
            .unwrap()
            .unwrap()
            .status,
        "running",
    );
    assert_ne!(status.stop_reason.as_deref(), Some("operator_stop"));
}

/// Both durable park forms must project as the same live exact run across
/// status, the repository-backed doctor, and the board aggregate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parked_exact_runs_are_live_across_status_doctor_and_board() {
    let cases = [
        (
            "operator awaiting-review park",
            djinn_core::refinement_liveness::RefinementParkKind::AwaitingReview,
            RefinementLivenessEvidence::AwaitingReviewPark,
            "awaiting_review_park",
        ),
        (
            "between-phase awaiting-evidence park",
            djinn_core::refinement_liveness::RefinementParkKind::AwaitingEvidence,
            RefinementLivenessEvidence::AwaitingEvidencePark,
            "awaiting_evidence_park",
        ),
    ];

    for (name, kind, expected_evidence, expected_status_evidence) in cases {
        let (_server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Parked exact-run projection",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: Some("draft"),
                body_format: None,
            })
            .await
            .unwrap();
        let run_id = seed_parked_cross_surface_run(
            &db,
            &repo,
            &proposal.id,
            &format!("parked-cross-surface-{name}"),
            kind,
        )
        .await;

        let exact = repo
            .load_refinement_run_snapshot(djinn_db::LoadRefinementRunSnapshotRequest {
                run_id: run_id.clone(),
                heartbeat_grace_millis: 60_000,
            })
            .await
            .unwrap()
            .unwrap();
        let expected = djinn_core::refinement_liveness::evaluate_refinement_liveness(
            &exact.snapshot,
            exact.observed_at,
        );
        assert_eq!(expected, exact.liveness, "{name}: repository snapshot");
        assert_eq!(
            exact.snapshot.park.as_ref().map(|park| park.kind),
            Some(kind),
            "{name}: selected durable park evidence"
        );
        assert!(
            exact.snapshot.intents.iter().all(|intent| {
                matches!(
                    intent.state,
                    djinn_core::refinement_liveness::RefinementIntentState::Completed
                )
            }),
            "{name}: source intent must be completed, not alternate live evidence"
        );
        assert!(
            exact.snapshot.tasks.is_empty(),
            "{name}: no task fallback evidence"
        );
        assert!(
            exact.snapshot.sessions.is_empty(),
            "{name}: no session fallback evidence"
        );
        assert!(
            exact.snapshot.between_phase.is_none(),
            "{name}: no handoff fallback evidence"
        );
        assert!(
            exact.snapshot.heartbeat.as_ref().is_none_or(|heartbeat| {
                heartbeat.heartbeat_at.0 + heartbeat.grace_millis <= exact.observed_at.0
            }),
            "{name}: no fresh-heartbeat fallback evidence"
        );
        assert_eq!(
            expected,
            RefinementLivenessResult::Live {
                evidence: expected_evidence,
            },
            "{name}: shared evaluator result"
        );

        let status = build_refinement_status(&repo, &proposal.id).await.unwrap();
        assert_eq!(
            status.run_id.as_deref(),
            Some(run_id.as_str()),
            "{name}: status run"
        );
        assert_eq!(
            status.liveness.as_deref(),
            Some("live"),
            "{name}: status liveness"
        );
        assert_eq!(
            status.liveness_evidence.as_deref(),
            Some(expected_status_evidence),
            "{name}: status evidence"
        );
        assert!(status.active, "{name}: status active");

        let (events_tx, _events_rx) = tokio::sync::broadcast::channel(16);
        let doctor_source = std::sync::Arc::new(
            djinn_coordinator::doctor::ProposalRepositoryRefinementPhantomActiveSource::new(
                db, events_tx,
            ),
        );
        assert!(
            doctor_stale_run_ids(doctor_source).is_empty(),
            "{name}: doctor must not report a live park"
        );
        assert_eq!(
            repo.load_board_refinement_lifecycle_aggregate(60_000)
                .await
                .unwrap()
                .stale_run_count,
            0,
            "{name}: board phantom count"
        );
    }
}

/// The remaining durable intent and task forms must agree across every
/// read-only consumer. Fixtures select durable state only; expected
/// liveness always comes from the shared evaluator over the exact snapshot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intent_and_queued_task_exact_runs_are_live_across_status_doctor_and_board() {
    enum Fixture {
        PendingIntent,
        ClaimedIntent,
        MaterializedIntentWithClosedRoleTask,
        QueuedOpenRoleTask,
    }

    let cases = [
        (
            "pending dispatch intent",
            Fixture::PendingIntent,
            "pending_intent",
        ),
        (
            "unexpired claimed intent",
            Fixture::ClaimedIntent,
            "claimed_intent",
        ),
        (
            "same-run materialized intent with closed role task",
            Fixture::MaterializedIntentWithClosedRoleTask,
            "materialized_intent",
        ),
        (
            "queued open role task",
            Fixture::QueuedOpenRoleTask,
            "queued_task",
        ),
    ];

    for (name, fixture, expected_status_evidence) in cases {
        let (_server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Intent and task exact-run projection",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: Some("draft"),
                body_format: None,
            })
            .await
            .unwrap();
        let (run_id, intent_id, generation) = match repo
            .reap_and_admit(djinn_db::AdmitRefinementRunRequest {
                proposal_id: proposal.id.clone(),
                idempotency_key: format!("intent-cross-surface-{name}"),
                source: djinn_db::RefinementAdmissionSource::Demand {
                    demand_id: format!("intent-cross-surface-{name}"),
                },
                heartbeat_grace_millis: 60_000,
            })
            .await
            .unwrap()
        {
            djinn_db::RefinementAdmissionOutcome::Admitted {
                run_id,
                intent_id,
                generation,
                ..
            } => (run_id, intent_id, generation),
            djinn_db::RefinementAdmissionOutcome::Existing { .. } => unreachable!(),
        };

        match fixture {
            Fixture::PendingIntent => {}
            Fixture::ClaimedIntent => {
                assert!(
                    repo.claim_refinement_intent(djinn_db::ClaimRefinementIntentRequest {
                        run_id: run_id.clone(),
                        intent_id: intent_id.clone(),
                        generation,
                        owner: "cross-surface-claimer".into(),
                        lease_millis: 60_000,
                    })
                    .await
                    .unwrap()
                    .is_some()
                );
            }
            Fixture::MaterializedIntentWithClosedRoleTask => {
                let owner = "cross-surface-materializer".to_owned();
                let lease = repo
                    .claim_refinement_intent(djinn_db::ClaimRefinementIntentRequest {
                        run_id: run_id.clone(),
                        intent_id: intent_id.clone(),
                        generation,
                        owner: owner.clone(),
                        lease_millis: 60_000,
                    })
                    .await
                    .unwrap()
                    .unwrap();
                let project_id = uuid::Uuid::now_v7().to_string();
                djinn_db::test_support::seed_project(&db, &project_id, "refinement intent fixture")
                    .await;
                let task_id = djinn_db::test_support::seed_task_row(
                    &db,
                    djinn_db::test_support::UsageTestTaskSeed {
                        project_id: &project_id,
                        status: "closed",
                        close_reason: None,
                        total_reopen_count: 0,
                    },
                )
                .await;
                djinn_db::TaskRepository::new(db.clone(), EventBus::noop())
                    .set_refinement_correlation(
                        &task_id,
                        Some(
                            &djinn_core::models::TaskRefinementCorrelation::new(
                                run_id.clone(),
                                intent_id.clone(),
                                i64::from(generation),
                                i64::from(lease.round),
                                djinn_core::refinement_liveness::RefinementPhase::AdversaryAttack,
                                djinn_core::refinement_liveness::RefinementRole::Adversary,
                            )
                            .unwrap(),
                        ),
                    )
                    .await
                    .unwrap();
                assert!(
                    repo.acknowledge_refinement_task_materialization(
                        djinn_db::AcknowledgeRefinementTaskMaterializationRequest {
                            run_id: run_id.clone(),
                            intent_id: intent_id.clone(),
                            generation,
                            task_id: task_id.clone(),
                            owner,
                        }
                    )
                    .await
                    .unwrap()
                );
            }
            Fixture::QueuedOpenRoleTask => {
                let project_id = uuid::Uuid::now_v7().to_string();
                djinn_db::test_support::seed_project(&db, &project_id, "refinement task fixture")
                    .await;
                let task_id = djinn_db::test_support::seed_task_row(
                    &db,
                    djinn_db::test_support::UsageTestTaskSeed {
                        project_id: &project_id,
                        status: "queued",
                        close_reason: None,
                        total_reopen_count: 0,
                    },
                )
                .await;
                djinn_db::TaskRepository::new(db.clone(), EventBus::noop())
                    .set_refinement_correlation(
                        &task_id,
                        Some(
                            &djinn_core::models::TaskRefinementCorrelation::new(
                                run_id.clone(),
                                intent_id.clone(),
                                i64::from(generation),
                                1,
                                djinn_core::refinement_liveness::RefinementPhase::AdversaryAttack,
                                djinn_core::refinement_liveness::RefinementRole::Adversary,
                            )
                            .unwrap(),
                        ),
                    )
                    .await
                    .unwrap();
                // The completed source intent and old heartbeat leave the queued task as the sole evidence.
                // `complete_refinement_intent` would persist a successor in the same transaction, and a
                // pending successor intent outranks the queued task in the shared evaluator.
                djinn_db::test_support::complete_refinement_intent_without_successor_for_test(
                    &db, &intent_id,
                )
                .await;
                djinn_db::test_support::elapse_refinement_run_wall_clock_for_test(
                    &db, &run_id, 3_600,
                )
                .await;
            }
        }

        let exact = repo
            .load_refinement_run_snapshot(djinn_db::LoadRefinementRunSnapshotRequest {
                run_id: run_id.clone(),
                heartbeat_grace_millis: 60_000,
            })
            .await
            .unwrap()
            .unwrap();
        let expected = djinn_core::refinement_liveness::evaluate_refinement_liveness(
            &exact.snapshot,
            exact.observed_at,
        );
        assert_eq!(expected, exact.liveness, "{name}: repository snapshot");
        assert!(
            matches!(expected, RefinementLivenessResult::Live { .. }),
            "{name}: evaluator result"
        );

        let status = build_refinement_status(&repo, &proposal.id).await.unwrap();
        assert_eq!(
            status.run_id.as_deref(),
            Some(run_id.as_str()),
            "{name}: status run"
        );
        assert_eq!(
            status.liveness.as_deref(),
            Some("live"),
            "{name}: status liveness"
        );
        assert_eq!(
            status.liveness_evidence.as_deref(),
            Some(expected_status_evidence),
            "{name}: status evidence"
        );
        assert!(status.active, "{name}: status active");

        let (events_tx, _events_rx) = tokio::sync::broadcast::channel(16);
        let doctor_source = std::sync::Arc::new(
            djinn_coordinator::doctor::ProposalRepositoryRefinementPhantomActiveSource::new(
                db, events_tx,
            ),
        );
        assert!(
            doctor_stale_run_ids(doctor_source).is_empty(),
            "{name}: doctor finding"
        );
        assert_eq!(
            repo.load_board_refinement_lifecycle_aggregate(60_000)
                .await
                .unwrap()
                .stale_run_count,
            0,
            "{name}: board phantom count"
        );
    }
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
        let proposal = repo
            .create(ProposalCreateInput {
                title: "09no 96fy exact-run phantom",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: Some("draft"),
                body_format: None,
            })
            .await
            .unwrap();
        let project_id = link_proposal_to_project(&db, &repo, &proposal.id).await;
        let admission = |key: String| djinn_db::AdmitRefinementRunRequest {
            proposal_id: proposal.id.clone(),
            idempotency_key: key.clone(),
            source: djinn_db::RefinementAdmissionSource::Demand { demand_id: key },
            heartbeat_grace_millis: 60_000,
        };

        let prior = match repo
            .reap_and_admit(admission(format!("{name}-prior")))
            .await
            .unwrap()
        {
            djinn_db::RefinementAdmissionOutcome::Admitted { run_id, .. } => run_id,
            _ => unreachable!(),
        };
        let (prior_project, prior_task) = seed_status_task(&db, &prior, "closed").await;
        let _prior_session = seed_live_status_session(&db, &prior_project, &prior_task).await;
        // Terminalized out of band on purpose: `terminal_refinement_run`
        // would also append a `refinement_stop` revision, and the assertion
        // below counts exactly those revisions to prove the tool handler
        // emitted no spurious stop. A fixture-written audit row would be
        // indistinguishable from the defect.
        //
        // `RefinementStopReason` is adjacently tagged (`tag`/`context`), and
        // `OperatorStop` is a struct variant — a row carrying the tag with a
        // null `stop_context` cannot be deserialized and fails every later
        // snapshot read with `invalid stop reason "operator_stop"`. The
        // helper derives the context the tag requires from the reason.
        djinn_db::test_support::force_refinement_run_terminal_for_test(
            &db,
            &prior,
            &djinn_core::refinement_liveness::RefinementStopReason::OperatorStop {
                actor: "operator".into(),
                reason: Some("prior run stopped by hand".into()),
            },
            "2000-01-01T00:00:00.000Z",
        )
        .await;
        let phantom = match repo
            .reap_and_admit(admission(format!("{name}-phantom")))
            .await
            .unwrap()
        {
            djinn_db::RefinementAdmissionOutcome::Admitted { run_id, .. } => run_id,
            _ => unreachable!(),
        };
        djinn_db::test_support::make_refinement_run_phantom_for_test(&db, &phantom).await;
        let (doctor_events_tx, _doctor_events_rx) = tokio::sync::broadcast::channel(16);
        let doctor_source = std::sync::Arc::new(
            djinn_coordinator::doctor::ProposalRepositoryRefinementPhantomActiveSource::new(
                db.clone(),
                doctor_events_tx,
            ),
        );
        let before = djinn_db::test_support::refinement_run_read_only_snapshot_for_test(
            &db,
            &proposal.id,
            &phantom,
        )
        .await;

        for _ in 0..2 {
            let status = server
                .dispatch_tool(
                    "proposal_refinement_status",
                    serde_json::json!({ "proposal_id": proposal.id }),
                )
                .await
                .unwrap();
            assert_eq!(status["refinement"]["run_id"], phantom);
            assert_eq!(status["refinement"]["liveness"], "stale");
            assert_eq!(
                doctor_stale_run_ids(doctor_source.clone()),
                vec![phantom.clone()],
                "{name} doctor observation must ignore terminal prior-run evidence"
            );
            let board = server
                .dispatch_tool(
                    "board_health",
                    serde_json::json!({
                        "project": format!("test-owner/test-repo-{project_id}")
                    }),
                )
                .await
                .unwrap();
            assert_eq!(
                board["refinement_phantom_active_count"], 1,
                "{name} board count"
            );
        }
        assert_eq!(
            djinn_db::test_support::refinement_run_read_only_snapshot_for_test(
                &db,
                &proposal.id,
                &phantom
            )
            .await,
            before,
            "{name}: reads must not reap or mutate the phantom"
        );

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
            // `error` carries "accepted; dispatch pending" only when the
            // post-commit wake FAILS (`wake_refinement_run(..).is_err()`).
            // `test_mcp_state` wires `StubRefinementAcceptingCoordinator`,
            // whose wake returns `Ok(())`, so a null `error` is what a
            // successful wake is supposed to produce here. Assert the
            // accepted shape instead: no error, and an active refinement.
            assert_eq!(
                response["error"],
                serde_json::Value::Null,
                "{name}: an accepted post-commit wake reports no error"
            );
            assert_eq!(
                response["refinement"]["active"], true,
                "{name}: the recovered run is reported active"
            );
        }

        let runs = repo
            .load_refinement_run_aggregates(&proposal.id, 60_000)
            .await
            .unwrap();
        assert_eq!(runs.len(), 3, "{name}: prior, phantom, one successor");
        let successor = runs
            .iter()
            .find(|run| run.run_id != prior && run.run_id != phantom)
            .unwrap();
        let snapshot = repo
            .load_refinement_run_snapshot(djinn_db::LoadRefinementRunSnapshotRequest {
                run_id: successor.run_id.clone(),
                heartbeat_grace_millis: 60_000,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            snapshot.snapshot.intents.len(),
            1,
            "{name}: exactly one pending intent"
        );
        assert_eq!(
            snapshot.snapshot.intents[0].state,
            djinn_core::refinement_liveness::RefinementIntentState::Pending
        );
        let revisions = repo.revisions(&proposal.id).await.unwrap();
        // `ProposalRevision` does not carry `refinement_stop_tag` — the column
        // lives on `proposal_revisions` but is not in the model's SELECT list.
        // This per-proposal aggregate counts exactly the same rows
        // (`refinement_stop_tag = 'reaped_phantom'` for this proposal), so the
        // assertion is unchanged in meaning without widening a shared model.
        let reaps = repo
            .load_refinement_lifecycle_aggregate(&proposal.id, 60_000)
            .await
            .unwrap();
        assert_eq!(
            reaps.reaped_phantom_last_24h, 1,
            "{name}: exactly one durable reaped_phantom revision"
        );
        assert_eq!(
            revisions
                .iter()
                .filter(|row| row.event_kind == "refinement_stop")
                .count(),
            1,
            "{name}: no handler-error stop"
        );
        assert_eq!(
            revisions
                .iter()
                .filter(|row| row.event_kind == "refinement_start")
                .count(),
            3,
            "{name}: retry returns the winner"
        );
        let board = server
            .dispatch_tool(
                "board_health",
                serde_json::json!({
                    "project": format!("test-owner/test-repo-{project_id}")
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            board["refinement_phantom_active_count"], 0,
            "{name}: reap clears current stale count"
        );
        assert_eq!(
            board["refinement_phantom_reaps_24h"], 1,
            "{name}: one durable bounded reap increment"
        );
    }
}
