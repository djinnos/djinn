//! Focused integration coverage for repository-owned refinement liveness reads.

use djinn_core::events::EventBus;
use djinn_core::refinement_liveness::{
    RefinementLivenessEvidence, RefinementLivenessResult, RefinementParkKind,
    RefinementStaleReason, RefinementStopReason, RefinementTaskState,
};
use djinn_db::repositories::refinement_run::LoadRefinementRunSnapshotRequest;
use djinn_db::test_support::{
    UsageTestSessionSeed, UsageTestTaskSeed, seed_project, seed_session_row_with_id, seed_task_row,
};
use djinn_db::{
    AdmitRefinementRunRequest, Database, ProposalRepository, RefinementAdmissionError,
    RefinementAdmissionOutcome, RefinementAdmissionSource,
};

const OLD: &str = "2000-01-01T00:00:00.000Z";
const FUTURE: &str = "2999-01-01T00:00:00.000Z";
const GRACE: i64 = 60_000;

async fn proposal(db: &Database) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO proposals (id, short_id, title, body, body_format, acceptance_criteria, status, latest_revision_seq) VALUES ($1, $2, 'snapshot', '', 'markdown', '[]'::jsonb, 'draft', 1)")
        .bind(&id).bind(id.replace('-', "")).execute(db.pool()).await.unwrap();
    id
}

#[tokio::test]
async fn migration_138_terminal_context_loads_and_aggregates() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal_id = proposal(&db).await;
    let run_id = run(&db, &proposal_id, 1).await;
    let legacy_context = serde_json::json!({
        "legacy_source_revision_id": "legacy-stop-row",
        "legacy_metadata": {"stop_reason": "operator_stop"}
    });
    sqlx::query(
        "UPDATE refinement_runs SET state = 'terminal', terminal_at = $2, \
         stop_tag = 'operator_stop', stop_context = $3 WHERE id = $1",
    )
    .bind(&run_id)
    .bind(OLD)
    .bind(legacy_context)
    .execute(db.pool())
    .await
    .unwrap();

    let expected = RefinementStopReason::OperatorStop {
        actor: "legacy_migration".into(),
        reason: None,
    };
    let snapshot = repo
        .load_refinement_run_snapshot(request(run_id.clone()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        snapshot.liveness,
        RefinementLivenessResult::Terminal {
            reason: Some(expected.clone())
        }
    );
    let aggregates = repo
        .load_refinement_run_aggregates(&proposal_id, GRACE)
        .await
        .unwrap();
    assert_eq!(aggregates.len(), 1);
    assert_eq!(aggregates[0].run_id, run_id);
    assert_eq!(
        aggregates[0].liveness,
        RefinementLivenessResult::Terminal {
            reason: Some(expected)
        }
    );
}

async fn run(db: &Database, proposal_id: &str, generation: i32) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO refinement_runs (id, proposal_id, generation, idempotency_key, heartbeat_at) VALUES ($1, $2, $3, $4, $5)")
        .bind(&id).bind(proposal_id).bind(generation).bind(format!("run-{id}")).bind(OLD)
        .execute(db.pool()).await.unwrap();
    id
}

async fn intent(db: &Database, run_id: &str, state: &str, expiry: Option<&str>) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    let (owner, claimed_at) = if state == "claimed" {
        (Some("reader"), Some(OLD))
    } else {
        (None, None)
    };
    sqlx::query("INSERT INTO refinement_dispatch_intents (id, run_id, round, phase, role, state, idempotency_key, claimed_by, claimed_at, claim_expires_at) VALUES ($1, $2, 1, 'adversary_attack', 'adversary', $3, $4, $5, $6, $7)")
        .bind(&id).bind(run_id).bind(state).bind(format!("intent-{id}")).bind(owner).bind(claimed_at).bind(expiry)
        .execute(db.pool()).await.unwrap();
    id
}

async fn task(db: &Database, run_id: &str, intent_id: Option<&str>, status: &str) -> String {
    let project_id = uuid::Uuid::now_v7().to_string();
    seed_project(db, &project_id, &format!("snapshot-{project_id}")).await;
    let task_id = seed_task_row(
        db,
        UsageTestTaskSeed {
            project_id: &project_id,
            status,
            close_reason: None,
            total_reopen_count: 0,
        },
    )
    .await;
    sqlx::query("UPDATE tasks SET refinement_run_id = $2, refinement_intent_id = $3 WHERE id = $1")
        .bind(&task_id)
        .bind(run_id)
        .bind(intent_id)
        .execute(db.pool())
        .await
        .unwrap();
    task_id
}

async fn live_session(db: &Database, task_id: &str) -> String {
    let project_id: String = sqlx::query_scalar("SELECT project_id FROM tasks WHERE id = $1")
        .bind(task_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    let id = uuid::Uuid::now_v7().to_string();
    seed_session_row_with_id(
        db,
        &id,
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
            task_id: Some(task_id),
        },
    )
    .await;
    sqlx::query("UPDATE sessions SET status = 'running' WHERE id = $1")
        .bind(&id)
        .execute(db.pool())
        .await
        .unwrap();
    id
}

fn request(run_id: String) -> LoadRefinementRunSnapshotRequest {
    LoadRefinementRunSnapshotRequest {
        run_id,
        heartbeat_grace_millis: GRACE,
    }
}

fn admission(proposal_id: String, idempotency_key: impl Into<String>) -> AdmitRefinementRunRequest {
    AdmitRefinementRunRequest {
        proposal_id,
        idempotency_key: idempotency_key.into(),
        source: RefinementAdmissionSource::Demand {
            demand_id: "concurrent-demand".into(),
        },
        heartbeat_grace_millis: GRACE,
    }
}

fn winner(outcome: &RefinementAdmissionOutcome) -> (&str, &str, i32) {
    match outcome {
        RefinementAdmissionOutcome::Admitted {
            run_id,
            intent_id,
            generation,
        }
        | RefinementAdmissionOutcome::Existing {
            run_id,
            intent_id,
            generation,
        } => (run_id, intent_id, *generation),
    }
}

#[tokio::test]
async fn concurrent_same_key_admission_has_one_winner_and_first_intent() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal_id = proposal(&db).await;
    let request = admission(proposal_id.clone(), "same-key-race");

    let (left, right) = tokio::join!(
        repo.admit_refinement_run(request.clone()),
        repo.admit_refinement_run(request)
    );
    let left = left.unwrap();
    let right = right.unwrap();
    let (left_run, left_intent, left_generation) = winner(&left);
    let (right_run, right_intent, right_generation) = winner(&right);
    assert_eq!(
        (left_run, left_intent, left_generation),
        (right_run, right_intent, right_generation)
    );
    assert!(matches!(
        (&left, &right),
        (RefinementAdmissionOutcome::Admitted { .. }, RefinementAdmissionOutcome::Existing { .. })
            | (RefinementAdmissionOutcome::Existing { .. }, RefinementAdmissionOutcome::Admitted { .. })
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM refinement_runs WHERE proposal_id = $1 AND state IN ('running', 'parked')",
        )
        .bind(&proposal_id)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM refinement_dispatch_intents WHERE run_id = $1 AND state = 'pending'",
        )
        .bind(left_run)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        1
    );
}

#[tokio::test]
async fn concurrent_distinct_reap_admission_has_one_successor_and_audit() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal_id = proposal(&db).await;
    let stale_run_id = run(&db, &proposal_id, 1).await;

    let (left, right) = tokio::join!(
        repo.reap_and_admit(admission(proposal_id.clone(), "stale-race-left")),
        repo.reap_and_admit(admission(proposal_id.clone(), "stale-race-right"))
    );
    let admitted = match (left, right) {
        (
            Ok(RefinementAdmissionOutcome::Admitted { run_id, .. }),
            Err(RefinementAdmissionError::AlreadyActive { .. }),
        )
        | (
            Err(RefinementAdmissionError::AlreadyActive { .. }),
            Ok(RefinementAdmissionOutcome::Admitted { run_id, .. }),
        ) => run_id,
        results => panic!(
            "expected one admitted successor and one AlreadyActive result, got {results:?}"
        ),
    };
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM refinement_runs WHERE proposal_id = $1 AND state IN ('running', 'parked')",
        )
        .bind(&proposal_id)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i32>("SELECT generation FROM refinement_runs WHERE id = $1")
            .bind(&admitted)
            .fetch_one(db.pool())
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM refinement_dispatch_intents WHERE run_id = $1 AND state = 'pending'",
        )
        .bind(&admitted)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM proposal_revisions WHERE refinement_run_id = $1 AND event_kind = 'refinement_stop' AND refinement_stop_tag = 'reaped_phantom'",
        )
        .bind(&stale_run_id)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        1
    );
}

#[tokio::test]
async fn admission_rejects_key_that_cannot_form_first_intent_key() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal_id = proposal(&db).await;
    let key = "x".repeat(255 - "/adversary/1".len() + 1);

    assert!(matches!(
        repo.admit_refinement_run(admission(proposal_id, key)).await,
        Err(RefinementAdmissionError::InvalidRequest(_))
    ));
}

#[tokio::test]
async fn snapshot_maps_exact_run_evidence_and_excludes_late_prior_rows() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());

    let p = proposal(&db).await;
    let prior = run(&db, &p, 1).await;
    sqlx::query("UPDATE refinement_runs SET state = 'terminal', terminal_at = $2, stop_tag = 'operator_stop' WHERE id = $1").bind(&prior).bind(OLD).execute(db.pool()).await.unwrap();
    let prior_intent = intent(&db, &prior, "pending", None).await;
    let prior_task = task(&db, &prior, Some(&prior_intent), "open").await;
    live_session(&db, &prior_task).await;
    let current = run(&db, &p, 2).await;
    let snapshot = repo
        .load_refinement_run_snapshot(request(current.clone()))
        .await
        .unwrap()
        .unwrap();
    assert!(snapshot.snapshot.intents.is_empty());
    assert!(snapshot.snapshot.tasks.is_empty());
    assert!(snapshot.snapshot.sessions.is_empty());
    assert_eq!(
        snapshot.liveness,
        RefinementLivenessResult::Stale {
            reason: RefinementStaleReason::NoLiveEvidence
        }
    );

    let p = proposal(&db).await;
    let pending_run = run(&db, &p, 1).await;
    let pending_id = intent(&db, &pending_run, "pending", None).await;
    assert_eq!(
        repo.load_refinement_run_snapshot(request(pending_run))
            .await
            .unwrap()
            .unwrap()
            .liveness,
        RefinementLivenessResult::Live {
            evidence: RefinementLivenessEvidence::PendingIntent {
                intent_id: pending_id
            }
        }
    );

    let p = proposal(&db).await;
    let claimed_run = run(&db, &p, 1).await;
    let claimed_id = intent(&db, &claimed_run, "claimed", Some(FUTURE)).await;
    assert_eq!(
        repo.load_refinement_run_snapshot(request(claimed_run))
            .await
            .unwrap()
            .unwrap()
            .liveness,
        RefinementLivenessResult::Live {
            evidence: RefinementLivenessEvidence::ClaimedIntent {
                intent_id: claimed_id
            }
        }
    );

    let p = proposal(&db).await;
    let expired_run = run(&db, &p, 1).await;
    intent(&db, &expired_run, "claimed", Some(OLD)).await;
    assert!(matches!(
        repo.load_refinement_run_snapshot(request(expired_run))
            .await
            .unwrap()
            .unwrap()
            .liveness,
        RefinementLivenessResult::Stale { .. }
    ));

    let p = proposal(&db).await;
    let task_run = run(&db, &p, 1).await;
    let task_id = task(&db, &task_run, None, "open").await;
    assert_eq!(
        repo.load_refinement_run_snapshot(request(task_run))
            .await
            .unwrap()
            .unwrap()
            .liveness,
        RefinementLivenessResult::Live {
            evidence: RefinementLivenessEvidence::OpenTask { task_id }
        }
    );

    let p = proposal(&db).await;
    let parked_run = run(&db, &p, 1).await;
    sqlx::query("UPDATE refinement_runs SET state = 'parked', parked_at = $2, park_kind = 'awaiting_review' WHERE id = $1").bind(&parked_run).bind(OLD).execute(db.pool()).await.unwrap();
    let parked = repo
        .load_refinement_run_snapshot(request(parked_run))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        parked.snapshot.park.unwrap().kind,
        RefinementParkKind::AwaitingReview
    );
    assert_eq!(
        parked.liveness,
        RefinementLivenessResult::Live {
            evidence: RefinementLivenessEvidence::AwaitingReviewPark
        }
    );

    let p = proposal(&db).await;
    let handoff_run = run(&db, &p, 1).await;
    let handoff_id = intent(&db, &handoff_run, "pending", None).await;
    let handoff = repo
        .load_refinement_run_snapshot(request(handoff_run))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        handoff
            .snapshot
            .between_phase
            .unwrap()
            .next_intent
            .intent_id,
        handoff_id
    );

    let p = proposal(&db).await;
    let heartbeat_run = run(&db, &p, 1).await;
    sqlx::query("UPDATE refinement_runs SET heartbeat_at = to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $1").bind(&heartbeat_run).execute(db.pool()).await.unwrap();
    assert!(matches!(
        repo.load_refinement_run_snapshot(request(heartbeat_run))
            .await
            .unwrap()
            .unwrap()
            .liveness,
        RefinementLivenessResult::Live {
            evidence: RefinementLivenessEvidence::FreshHeartbeat { .. }
        }
    ));
}

/// Post-worker task statuses remain valid exact-run evidence even though the
/// liveness evaluator intentionally projects them into a smaller vocabulary.
#[tokio::test]
async fn snapshot_and_aggregate_accept_post_worker_task_status() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal_id = proposal(&db).await;
    let run_id = run(&db, &proposal_id, 1).await;
    let task_id = task(&db, &run_id, None, "needs_task_review").await;

    let exact = repo
        .load_refinement_run_snapshot(request(run_id.clone()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(exact.snapshot.tasks.len(), 1);
    assert_eq!(exact.snapshot.tasks[0].task_id, task_id);
    assert_eq!(exact.snapshot.tasks[0].state, RefinementTaskState::Open);
    assert_eq!(
        exact.liveness,
        RefinementLivenessResult::Live {
            evidence: RefinementLivenessEvidence::OpenTask { task_id }
        }
    );

    let aggregates = repo
        .load_refinement_run_aggregates(&proposal_id, GRACE)
        .await
        .unwrap();
    assert_eq!(aggregates.len(), 1);
    assert_eq!(aggregates[0].run_id, run_id);
    assert!(matches!(
        aggregates[0].liveness,
        RefinementLivenessResult::Live {
            evidence: RefinementLivenessEvidence::OpenTask { .. }
        }
    ));
}

#[tokio::test]
async fn snapshot_current_and_aggregate_reads_do_not_mutate_durable_rows() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let p = proposal(&db).await;
    let run_id = run(&db, &p, 1).await;
    let intent_id = intent(&db, &run_id, "claimed", Some(FUTURE)).await;
    let task_id = task(&db, &run_id, Some(&intent_id), "closed").await;
    let session_id = live_session(&db, &task_id).await;
    let before: (String, String, String, i64, i64) = sqlx::query_as("SELECT r.heartbeat_at, r.updated_at, i.claim_expires_at, (SELECT COUNT(*) FROM tasks WHERE refinement_run_id = r.id), (SELECT COUNT(*) FROM sessions WHERE task_id = $2) FROM refinement_runs r JOIN refinement_dispatch_intents i ON i.id = $3 WHERE r.id = $1")
        .bind(&run_id).bind(&task_id).bind(&intent_id).fetch_one(db.pool()).await.unwrap();
    for _ in 0..2 {
        let exact = repo
            .load_refinement_run_snapshot(request(run_id.clone()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(exact.snapshot.sessions[0].session_id, session_id);
        assert_eq!(exact.snapshot.sessions[0].task_id, task_id);
        assert_eq!(
            repo.load_current_refinement_run_snapshot(&p, GRACE)
                .await
                .unwrap()
                .unwrap()
                .snapshot
                .run
                .run_id,
            run_id
        );
        assert_eq!(
            repo.load_refinement_run_aggregates(&p, GRACE)
                .await
                .unwrap()
                .len(),
            1
        );
    }
    let after: (String, String, String, i64, i64) = sqlx::query_as("SELECT r.heartbeat_at, r.updated_at, i.claim_expires_at, (SELECT COUNT(*) FROM tasks WHERE refinement_run_id = r.id), (SELECT COUNT(*) FROM sessions WHERE task_id = $2) FROM refinement_runs r JOIN refinement_dispatch_intents i ON i.id = $3 WHERE r.id = $1")
        .bind(&run_id).bind(&task_id).bind(&intent_id).fetch_one(db.pool()).await.unwrap();
    assert_eq!(before, after);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM sessions WHERE id = $1")
            .bind(&session_id)
            .fetch_one(db.pool())
            .await
            .unwrap(),
        "running"
    );
}
