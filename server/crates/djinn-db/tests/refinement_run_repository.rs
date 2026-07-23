//! Repository-protocol regression fixtures for transactional refinement admission.
//!
//! These tests deliberately use real Postgres migrations and database triggers rather
//! than mocks. A trigger aborts the transaction at each write boundary, proving that
//! admission does not leak a reaped predecessor, lifecycle row, successor, or intent.

use std::sync::Arc;

use djinn_core::events::EventBus;
use djinn_core::refinement_liveness::{
    RefinementLivenessResult, RefinementParkKind, RefinementPhase, RefinementRole,
    RefinementStaleReason, RefinementStopReason,
};
use djinn_db::repositories::refinement_run::LoadRefinementRunSnapshotRequest;
use djinn_db::test_support::{
    UsageTestSessionSeed, UsageTestTaskSeed, seed_project, seed_session_row_with_id, seed_task_row,
};
use djinn_db::{
    AdmitRefinementRunRequest, Database, ParkRefinementRunFromIntentRequest, ProposalRepository,
    RefinementAdmissionOutcome, RefinementAdmissionSource, RefinementDurableProgress,
    RefinementIntentMutationError, ResolveRefinementHumanReviewRequest,
    SourceIntentTransitionRequest, TerminalRefinementRunFromIntentRequest,
};
use serde_json::Value;
use tokio::sync::Barrier;

const OLD: &str = "2000-01-01T00:00:00.000Z";
const GRACE: i64 = 60_000;

async fn fixture() -> (Database, ProposalRepository, String) {
    let db = Database::ephemeral().await.unwrap();
    db.ensure_initialized().await.unwrap();
    let proposal_id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO proposals (id, short_id, title, body, body_format, acceptance_criteria, status, latest_revision_seq) VALUES ($1, $2, 'repository protocol', '', 'markdown', '[]'::jsonb, 'draft', 1)")
        .bind(&proposal_id)
        .bind(proposal_id.replace('-', ""))
        .execute(db.pool())
        .await
        .unwrap();
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    (db, repo, proposal_id)
}

fn demand(proposal_id: String, key: impl Into<String>) -> AdmitRefinementRunRequest {
    AdmitRefinementRunRequest {
        proposal_id,
        idempotency_key: key.into(),
        source: RefinementAdmissionSource::Demand {
            demand_id: "repository-regression".into(),
        },
        heartbeat_grace_millis: GRACE,
    }
}

async fn stale_run(db: &Database, proposal_id: &str, generation: i32) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO refinement_runs (id, proposal_id, generation, idempotency_key, heartbeat_at) VALUES ($1, $2, $3, $4, $5)")
        .bind(&id)
        .bind(proposal_id)
        .bind(generation)
        .bind(format!("seed-{id}"))
        .bind(OLD)
        .execute(db.pool())
        .await
        .unwrap();
    id
}

async fn durable_shape(db: &Database, proposal_id: &str) -> Value {
    sqlx::query_scalar(
        "SELECT jsonb_build_object( \
           'runs', COALESCE((SELECT jsonb_agg(to_jsonb(r) ORDER BY r.id) FROM refinement_runs r WHERE r.proposal_id = $1), '[]'::jsonb), \
           'revisions', COALESCE((SELECT jsonb_agg(to_jsonb(p) ORDER BY p.id) FROM proposal_revisions p WHERE p.proposal_id = $1), '[]'::jsonb), \
           'intents', COALESCE((SELECT jsonb_agg(to_jsonb(i) ORDER BY i.id) FROM refinement_dispatch_intents i JOIN refinement_runs r ON r.id = i.run_id WHERE r.proposal_id = $1), '[]'::jsonb))",
    )
    .bind(proposal_id)
    .fetch_one(db.pool())
    .await
    .unwrap()
}

async fn install_failure(db: &Database, boundary: &str) {
    sqlx::query(
        "CREATE OR REPLACE FUNCTION refinement_admission_failure_for_test() RETURNS trigger AS $$ \
         BEGIN RAISE EXCEPTION 'injected refinement admission failure'; END; $$ LANGUAGE plpgsql",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let statement = match boundary {
        "reap_update" => {
            "CREATE TRIGGER refinement_admission_failure BEFORE UPDATE ON refinement_runs FOR EACH ROW EXECUTE FUNCTION refinement_admission_failure_for_test()"
        }
        "run_insert" => {
            "CREATE TRIGGER refinement_admission_failure BEFORE INSERT ON refinement_runs FOR EACH ROW EXECUTE FUNCTION refinement_admission_failure_for_test()"
        }
        "lifecycle_start" => {
            "CREATE TRIGGER refinement_admission_failure BEFORE INSERT ON proposal_revisions FOR EACH ROW WHEN (NEW.event_kind = 'refinement_start') EXECUTE FUNCTION refinement_admission_failure_for_test()"
        }
        "first_intent" => {
            "CREATE TRIGGER refinement_admission_failure BEFORE INSERT ON refinement_dispatch_intents FOR EACH ROW EXECUTE FUNCTION refinement_admission_failure_for_test()"
        }
        // A deferred constraint trigger is invoked by the real COMMIT, after all
        // admission writes have succeeded, rather than by a mocked commit path.
        "commit" => {
            "CREATE CONSTRAINT TRIGGER refinement_admission_failure AFTER INSERT ON refinement_dispatch_intents DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION refinement_admission_failure_for_test()"
        }
        _ => panic!("unknown injection boundary {boundary}"),
    };
    sqlx::query(statement).execute(db.pool()).await.unwrap();
}

async fn assert_reap_rolled_back(db: &Database, proposal_id: &str, old_run_id: &str) {
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM refinement_runs WHERE id = $1")
            .bind(old_run_id)
            .fetch_one(db.pool())
            .await
            .unwrap(),
        "running"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM refinement_runs WHERE proposal_id = $1")
            .bind(proposal_id)
            .fetch_one(db.pool())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM proposal_revisions WHERE proposal_id = $1 AND event_kind IN ('refinement_start', 'refinement_stop')")
            .bind(proposal_id)
            .fetch_one(db.pool())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM refinement_dispatch_intents")
            .fetch_one(db.pool())
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn faults_at_every_admission_boundary_rollback_the_entire_reap_and_successor() {
    for boundary in [
        "reap_update",
        "run_insert",
        "lifecycle_start",
        "first_intent",
        "commit",
    ] {
        let (db, repo, proposal_id) = fixture().await;
        let old_run_id = stale_run(&db, &proposal_id, 1).await;
        install_failure(&db, boundary).await;

        assert!(
            repo.reap_and_admit(demand(proposal_id.clone(), format!("fault-{boundary}")))
                .await
                .is_err(),
            "{boundary} must abort admission"
        );
        assert_reap_rolled_back(&db, &proposal_id, &old_run_id).await;
    }
}

#[tokio::test]
async fn lost_post_commit_response_retries_the_single_pending_intent_without_a_second_run() {
    let (db, repo, proposal_id) = fixture().await;
    let request = demand(proposal_id.clone(), "lost-response");
    // Deliberately discard the response as a caller/process would after a successful commit.
    let _ = repo.admit_refinement_run(request.clone()).await.unwrap();
    let retried = repo.admit_refinement_run(request).await.unwrap();
    let (run_id, intent_id) = match retried {
        RefinementAdmissionOutcome::Existing {
            run_id, intent_id, ..
        } => (run_id, intent_id),
        other => panic!("retry must load committed winner, got {other:?}"),
    };
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM refinement_runs WHERE proposal_id = $1")
            .bind(&proposal_id)
            .fetch_one(db.pool())
            .await
            .unwrap(),
        1
    );
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM refinement_dispatch_intents WHERE run_id = $1 AND state = 'pending'").bind(&run_id).fetch_one(db.pool()).await.unwrap(), 1);
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT id FROM refinement_dispatch_intents WHERE run_id = $1"
        )
        .bind(&run_id)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        intent_id
    );
}

#[tokio::test]
async fn barriers_serialize_same_key_and_distinct_stale_replacement_demands() {
    let (_db, repo, proposal_id) = fixture().await;
    let barrier = Arc::new(Barrier::new(2));
    let left_request = demand(proposal_id.clone(), "same-key");
    let left_barrier = barrier.clone();
    let left = async {
        left_barrier.wait().await;
        repo.admit_refinement_run(left_request).await
    };
    let right_request = demand(proposal_id.clone(), "same-key");
    let right = async {
        barrier.wait().await;
        repo.admit_refinement_run(right_request).await
    };
    let (first, second) = tokio::join!(left, right);
    let first = first.unwrap();
    let second = second.unwrap();
    let identity = |outcome: RefinementAdmissionOutcome| match outcome {
        RefinementAdmissionOutcome::Admitted {
            run_id, intent_id, ..
        }
        | RefinementAdmissionOutcome::Existing {
            run_id, intent_id, ..
        } => (run_id, intent_id),
    };
    assert_eq!(identity(first), identity(second));

    let (db, repo, proposal_id) = fixture().await;
    let stale = stale_run(&db, &proposal_id, 1).await;
    let barrier = Arc::new(Barrier::new(2));
    let a_barrier = barrier.clone();
    let a_proposal = proposal_id.clone();
    let a = async {
        a_barrier.wait().await;
        repo.reap_and_admit(demand(a_proposal, "replacement-a"))
            .await
    };
    let b_proposal = proposal_id.clone();
    let b = async {
        barrier.wait().await;
        repo.reap_and_admit(demand(b_proposal, "replacement-b"))
            .await
    };
    let (a, b) = tokio::join!(a, b);
    let results = [a, b];
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Ok(RefinementAdmissionOutcome::Admitted { .. })))
            .count(),
        1
    );
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM proposal_revisions WHERE refinement_run_id = $1 AND refinement_stop_tag = 'reaped_phantom'").bind(&stale).fetch_one(db.pool()).await.unwrap(), 1);
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM refinement_runs WHERE proposal_id = $1 AND state IN ('running', 'parked')").bind(&proposal_id).fetch_one(db.pool()).await.unwrap(), 1);
}

#[tokio::test]
async fn phantom_09no_96fy_evidence_is_scoped_to_the_exact_run_and_reaped_with_context() {
    let (db, repo, proposal_id) = fixture().await;
    let prior = stale_run(&db, &proposal_id, 1).await;
    sqlx::query("UPDATE refinement_runs SET state = 'terminal', terminal_at = $2, stop_tag = 'operator_stop' WHERE id = $1").bind(&prior).bind(OLD).execute(db.pool()).await.unwrap();
    let prior_intent = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO refinement_dispatch_intents (id, run_id, round, phase, role, idempotency_key) VALUES ($1, $2, 1, 'adversary_attack', 'adversary', $3)").bind(&prior_intent).bind(&prior).bind(format!("prior-{prior_intent}")).execute(db.pool()).await.unwrap();
    let project_id = uuid::Uuid::now_v7().to_string();
    seed_project(&db, &project_id, "phantom-prior-evidence").await;
    let prior_task = seed_task_row(
        &db,
        UsageTestTaskSeed {
            project_id: &project_id,
            status: "open",
            close_reason: None,
            total_reopen_count: 0,
        },
    )
    .await;
    sqlx::query("UPDATE tasks SET refinement_run_id = $2, refinement_intent_id = $3 WHERE id = $1")
        .bind(&prior_task)
        .bind(&prior)
        .bind(&prior_intent)
        .execute(db.pool())
        .await
        .unwrap();
    let session_id = uuid::Uuid::now_v7().to_string();
    seed_session_row_with_id(
        &db,
        &session_id,
        UsageTestSessionSeed {
            project_id: &project_id,
            model_id: "test",
            agent_type: "worker",
            started_at: OLD,
            tokens_in: 0,
            tokens_out: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: None,
            cost_basis: "unpriced",
            task_id: Some(&prior_task),
        },
    )
    .await;
    sqlx::query("UPDATE sessions SET status = 'running' WHERE id = $1")
        .bind(&session_id)
        .execute(db.pool())
        .await
        .unwrap();
    let phantom = stale_run(&db, &proposal_id, 2).await;

    let outcome = repo
        .reap_and_admit(demand(proposal_id.clone(), "09no-96fy-recovery"))
        .await
        .unwrap();
    let successor = match outcome {
        RefinementAdmissionOutcome::Admitted {
            run_id, generation, ..
        } => {
            assert_eq!(generation, 3);
            run_id
        }
        _ => unreachable!(),
    };
    let context: Value =
        sqlx::query_scalar("SELECT stop_context FROM refinement_runs WHERE id = $1")
            .bind(&phantom)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(context["prior_run_id"], phantom);
    assert_eq!(context["generation"], 2);
    assert_eq!(
        context["evidence_summary"],
        "shared evaluator found no live exact-run evidence"
    );
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM proposal_revisions WHERE refinement_run_id = $1 AND event_kind = 'refinement_stop' AND refinement_stop_tag = 'reaped_phantom'").bind(&phantom).fetch_one(db.pool()).await.unwrap(), 1);
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM proposal_revisions WHERE refinement_run_id = $1 AND event_kind = 'refinement_start'").bind(&successor).fetch_one(db.pool()).await.unwrap(), 1);
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM refinement_dispatch_intents WHERE run_id = $1 AND state = 'pending'").bind(&successor).fetch_one(db.pool()).await.unwrap(), 1);

    // Make the winner stale first. A delayed progress notification for the reaped
    // generation must be rejected rather than reviving any exact-run snapshot.
    sqlx::query("UPDATE refinement_dispatch_intents SET state = 'cancelled', terminal_at = $2 WHERE run_id = $1").bind(&successor).bind(OLD).execute(db.pool()).await.unwrap();
    sqlx::query("UPDATE refinement_runs SET heartbeat_at = $2 WHERE id = $1")
        .bind(&successor)
        .bind(OLD)
        .execute(db.pool())
        .await
        .unwrap();
    let before_old_mutation = durable_shape(&db, &proposal_id).await;
    assert!(matches!(
        repo.record_refinement_durable_progress(
            &phantom,
            2,
            RefinementDurableProgress::DebateAppend,
        )
        .await,
        Err(RefinementIntentMutationError::GenerationConflict { .. })
    ));
    assert_eq!(durable_shape(&db, &proposal_id).await, before_old_mutation);
    let current = repo
        .load_current_refinement_run_snapshot(&proposal_id, GRACE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.snapshot.run.run_id, successor);
    assert_eq!(
        current.liveness,
        RefinementLivenessResult::Stale {
            reason: RefinementStaleReason::NoLiveEvidence
        }
    );
}

#[tokio::test]
async fn snapshot_and_24_hour_aggregate_reads_are_byte_for_byte_pure() {
    let (db, repo, proposal_id) = fixture().await;
    let admitted = repo
        .admit_refinement_run(demand(proposal_id.clone(), "pure-reads"))
        .await
        .unwrap();
    let run_id = match admitted {
        RefinementAdmissionOutcome::Admitted { run_id, .. } => run_id,
        _ => unreachable!(),
    };
    // This fixture has both a stale active run and a recent phantom-reap audit
    // row, so the lifecycle aggregate exercises both its counters.
    sqlx::query("UPDATE refinement_dispatch_intents SET state = 'cancelled', terminal_at = $2 WHERE run_id = $1")
        .bind(&run_id)
        .bind(OLD)
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE refinement_runs SET heartbeat_at = $2 WHERE id = $1")
        .bind(&run_id)
        .bind(OLD)
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO proposal_revisions (id, proposal_id, seq, title, body, body_format, acceptance_criteria, edited_by_user_id, event_kind, event_metadata, refinement_run_id, refinement_stop_tag) VALUES ($1, $2, 2, '', '', 'markdown', '[]', NULL, 'refinement_stop', '{}'::jsonb, $3, 'reaped_phantom')")
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(&proposal_id)
        .bind(&run_id)
        .execute(db.pool())
        .await
        .unwrap();
    let before = durable_shape(&db, &proposal_id).await;
    for _ in 0..3 {
        repo.load_refinement_run_snapshot(LoadRefinementRunSnapshotRequest {
            run_id: run_id.clone(),
            heartbeat_grace_millis: GRACE,
        })
        .await
        .unwrap();
        repo.load_current_refinement_run_snapshot(&proposal_id, GRACE)
            .await
            .unwrap();
        let aggregate = repo
            .load_refinement_lifecycle_aggregate(&proposal_id, GRACE)
            .await
            .unwrap();
        assert_eq!(aggregate.stale_run_count, 1);
        assert_eq!(aggregate.reaped_phantom_last_24h, 1);
    }
    assert_eq!(durable_shape(&db, &proposal_id).await, before);
}

fn source(run_id: String, intent_id: String, generation: i32) -> SourceIntentTransitionRequest {
    SourceIntentTransitionRequest {
        run_id,
        intent_id,
        generation,
        expected_round: 1,
        expected_phase: RefinementPhase::AdversaryAttack,
        expected_role: RefinementRole::Adversary,
    }
}

#[tokio::test]
async fn exact_source_intent_transitions_fence_and_rollback_together() {
    let (db, repo, proposal_id) = fixture().await;
    let admitted = repo
        .admit_refinement_run(demand(proposal_id.clone(), "source-terminal"))
        .await
        .unwrap();
    let (run_id, intent_id, generation) = match admitted {
        RefinementAdmissionOutcome::Admitted {
            run_id,
            intent_id,
            generation,
        } => (run_id, intent_id, generation),
        _ => unreachable!(),
    };
    sqlx::query("UPDATE refinement_dispatch_intents SET state = 'claimed', claimed_by = 'worker', claimed_at = $2, claim_expires_at = '2999-01-01T00:00:00.000Z' WHERE id = $1")
        .bind(&intent_id)
        .bind(OLD)
        .execute(db.pool())
        .await
        .unwrap();
    let bad = TerminalRefinementRunFromIntentRequest {
        source: SourceIntentTransitionRequest {
            expected_role: RefinementRole::Judge,
            ..source(run_id.clone(), intent_id.clone(), generation)
        },
        reason: RefinementStopReason::OperatorStop {
            actor: "test".into(),
            reason: None,
        },
    };
    let before = durable_shape(&db, &proposal_id).await;
    assert!(repo.terminal_refinement_run_from_intent(bad).await.is_err());
    assert_eq!(durable_shape(&db, &proposal_id).await, before);
    assert!(
        repo.terminal_refinement_run_from_intent(TerminalRefinementRunFromIntentRequest {
            source: source(run_id.clone(), intent_id.clone(), generation),
            reason: RefinementStopReason::OperatorStop {
                actor: "test".into(),
                reason: None
            },
        })
        .await
        .unwrap()
    );
    assert!(
        !repo
            .terminal_refinement_run_from_intent(TerminalRefinementRunFromIntentRequest {
                source: source(run_id.clone(), intent_id.clone(), generation),
                reason: RefinementStopReason::OperatorStop {
                    actor: "test".into(),
                    reason: None
                },
            })
            .await
            .unwrap()
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT state FROM refinement_dispatch_intents WHERE id=$1"
        )
        .bind(&intent_id)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        "completed"
    );

    let (db, repo, proposal_id) = fixture().await;
    let admitted = repo
        .admit_refinement_run(demand(proposal_id.clone(), "source-rollback"))
        .await
        .unwrap();
    let (run_id, intent_id, generation) = match admitted {
        RefinementAdmissionOutcome::Admitted {
            run_id,
            intent_id,
            generation,
        } => (run_id, intent_id, generation),
        _ => unreachable!(),
    };
    sqlx::query("UPDATE refinement_dispatch_intents SET state = 'materialized' WHERE id = $1")
        .bind(&intent_id)
        .execute(db.pool())
        .await
        .unwrap();
    install_failure(&db, "reap_update").await;
    let before = durable_shape(&db, &proposal_id).await;
    assert!(
        repo.park_refinement_run_from_intent(ParkRefinementRunFromIntentRequest {
            source: source(run_id, intent_id, generation),
            kind: RefinementParkKind::AwaitingReview,
        })
        .await
        .is_err()
    );
    assert_eq!(durable_shape(&db, &proposal_id).await, before);
}

#[tokio::test]
async fn human_rejection_reverts_snapshot_and_terminalizes_atomically() {
    let (db, repo, proposal_id) = fixture().await;
    sqlx::query("INSERT INTO proposal_revisions (id, proposal_id, seq, title, body, body_format, acceptance_criteria, event_kind) VALUES ($1, $2, 1, 'captured', 'captured body', 'markdown', '[]', 'spec_revision')")
        .bind(uuid::Uuid::now_v7().to_string()).bind(&proposal_id).execute(db.pool()).await.unwrap();
    let admitted = repo
        .admit_refinement_run(demand(proposal_id.clone(), "human-reject"))
        .await
        .unwrap();
    let (run_id, generation) = match admitted {
        RefinementAdmissionOutcome::Admitted {
            run_id, generation, ..
        } => (run_id, generation),
        _ => unreachable!(),
    };
    sqlx::query("UPDATE proposals SET title='changed', body='changed body', status='in_review', latest_revision_seq=2 WHERE id=$1").bind(&proposal_id).execute(db.pool()).await.unwrap();
    sqlx::query("UPDATE refinement_runs SET state='parked', park_kind='awaiting_review', parked_at=$2 WHERE id=$1").bind(&run_id).bind(OLD).execute(db.pool()).await.unwrap();
    assert!(
        repo.resolve_refinement_human_review(ResolveRefinementHumanReviewRequest {
            run_id: run_id.clone(),
            generation,
            snapshot_revision_seq: 1,
            accept: false
        })
        .await
        .unwrap()
    );
    let reverted: (String, String, String) =
        sqlx::query_as("SELECT title, body, status FROM proposals WHERE id=$1")
            .bind(&proposal_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(
        reverted,
        ("captured".into(), "captured body".into(), "draft".into())
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT stop_tag FROM refinement_runs WHERE id=$1")
            .bind(&run_id)
            .fetch_one(db.pool())
            .await
            .unwrap(),
        "human_rejected"
    );
    assert!(
        !repo
            .resolve_refinement_human_review(ResolveRefinementHumanReviewRequest {
                run_id,
                generation,
                snapshot_revision_seq: 1,
                accept: false
            })
            .await
            .unwrap()
    );
}
