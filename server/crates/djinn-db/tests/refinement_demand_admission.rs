//! Admission coverage for a human demand raised against a parked refinement run.
//!
//! A converged tribunal parks `awaiting_review`, and the shared liveness
//! evaluator classifies that park `Live` unconditionally. Admission therefore
//! used to reject EVERY human demand against a park with `AlreadyActive` — the
//! only state in which the UI's "Send feedback for another round" control is
//! ever offered, which made it a 100% dead button (proposal y6q4).
//!
//! Split from `refinement_run_snapshot.rs` to keep that file under the
//! repository size guard.

use djinn_core::events::EventBus;
use djinn_core::refinement_liveness::{
    RefinementLivenessEvidence, RefinementLivenessResult, RefinementParkKind,
};
use djinn_db::repositories::refinement_run::LoadRefinementRunSnapshotRequest;
use djinn_db::{
    AdmitRefinementRunRequest, Database, ParkRefinementRunRequest, ProposalRepository,
    RefinementAdmissionError, RefinementAdmissionOutcome, RefinementAdmissionSource,
};

const GRACE: i64 = 60_000;

async fn proposal(db: &Database) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO proposals (id, short_id, title, body, body_format, acceptance_criteria, status, latest_revision_seq) VALUES ($1, $2, 'demand', '', 'markdown', '[]'::jsonb, 'draft', 1)")
        .bind(&id).bind(id.replace('-', "")).execute(db.pool()).await.unwrap();
    id
}

fn request(run_id: String) -> LoadRefinementRunSnapshotRequest {
    LoadRefinementRunSnapshotRequest {
        run_id,
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

fn demand(proposal_id: String, idempotency_key: impl Into<String>) -> AdmitRefinementRunRequest {
    AdmitRefinementRunRequest {
        proposal_id,
        idempotency_key: idempotency_key.into(),
        source: RefinementAdmissionSource::Demand {
            demand_id: "human-demand".into(),
        },
        heartbeat_grace_millis: GRACE,
    }
}

fn explicit_start(
    proposal_id: String,
    idempotency_key: impl Into<String>,
) -> AdmitRefinementRunRequest {
    AdmitRefinementRunRequest {
        proposal_id,
        idempotency_key: idempotency_key.into(),
        source: RefinementAdmissionSource::ExplicitStart {
            actor: "human".into(),
        },
        heartbeat_grace_millis: GRACE,
    }
}

/// Admit a fresh run and return `(run_id, generation)`.
async fn started_run(repo: &ProposalRepository, proposal_id: &str) -> (String, i32) {
    match repo
        .admit_refinement_run(explicit_start(proposal_id.to_owned(), "start-1"))
        .await
        .unwrap()
    {
        RefinementAdmissionOutcome::Admitted {
            run_id, generation, ..
        }
        | RefinementAdmissionOutcome::Existing {
            run_id, generation, ..
        } => (run_id, generation),
    }
}

/// Park the run the way production does: `park_refinement_run_from_intent`
/// completes the source dispatch intent in the same transaction that parks the
/// run, so a parked run never carries a dispatchable intent.
async fn park(
    db: &Database,
    repo: &ProposalRepository,
    run_id: &str,
    generation: i32,
    kind: RefinementParkKind,
) {
    sqlx::query(
        "UPDATE refinement_dispatch_intents SET state = 'completed', \
         terminal_at = to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') \
         WHERE run_id = $1 AND state IN ('pending', 'claimed', 'materialized')",
    )
    .bind(run_id)
    .execute(db.pool())
    .await
    .unwrap();
    assert!(
        repo.park_refinement_run(ParkRefinementRunRequest {
            run_id: run_id.to_owned(),
            generation,
            kind,
        })
        .await
        .unwrap(),
        "fixture must park the run"
    );
}

#[tokio::test]
async fn human_demand_resumes_an_awaiting_review_park_on_the_exact_run_and_generation() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal_id = proposal(&db).await;
    let (run_id, generation) = started_run(&repo, &proposal_id).await;
    park(
        &db,
        &repo,
        &run_id,
        generation,
        RefinementParkKind::AwaitingReview,
    )
    .await;

    // Precondition: the park is classified Live, which is exactly what used to
    // make the demand fail.
    assert!(matches!(
        repo.load_refinement_run_snapshot(request(run_id.clone()))
            .await
            .unwrap()
            .unwrap()
            .liveness,
        RefinementLivenessResult::Live {
            evidence: RefinementLivenessEvidence::AwaitingReviewPark
        }
    ));

    let outcome = repo
        .reap_and_admit(demand(proposal_id.clone(), "demand-1"))
        .await
        .expect("a human demand must be admitted against an awaiting-review park");
    let (admitted_run, admitted_intent, admitted_generation) = winner(&outcome);
    assert_eq!(admitted_run, run_id, "the exact run must be reused");
    assert_eq!(
        admitted_generation, generation,
        "the exact generation must be reused"
    );

    let (state, park_kind): (String, Option<String>) =
        sqlx::query_as("SELECT state, park_kind FROM refinement_runs WHERE id = $1")
            .bind(&run_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(state, "running", "the park must be cleared");
    assert_eq!(park_kind, None);

    let (round, phase, role, intent_state): (i32, String, String, String) = sqlx::query_as(
        "SELECT round, phase, role, state FROM refinement_dispatch_intents WHERE id = $1",
    )
    .bind(admitted_intent)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        round, 2,
        "the demanded round must advance past the parked one"
    );
    assert_eq!(phase, "adversary_attack");
    assert_eq!(role, "adversary");
    assert_eq!(intent_state, "pending");
    // Exactly one dispatchable intent, so the coordinator cannot double-dispatch.
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM refinement_dispatch_intents WHERE run_id = $1 AND state = 'pending'",
        )
        .bind(&run_id)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        1
    );
}

#[tokio::test]
async fn a_retried_human_demand_does_not_mint_a_second_round() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal_id = proposal(&db).await;
    let (run_id, generation) = started_run(&repo, &proposal_id).await;
    park(
        &db,
        &repo,
        &run_id,
        generation,
        RefinementParkKind::AwaitingReview,
    )
    .await;

    let first = repo
        .reap_and_admit(demand(proposal_id.clone(), "demand-retry"))
        .await
        .unwrap();
    let second = repo
        .reap_and_admit(demand(proposal_id.clone(), "demand-retry"))
        .await
        .expect("an identical retry must resolve to the same durable intent");
    assert_eq!(winner(&first).1, winner(&second).1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM refinement_dispatch_intents WHERE run_id = $1"
        )
        .bind(&run_id)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        2
    );
}

#[tokio::test]
async fn human_demand_against_a_genuinely_running_run_still_reports_already_active() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal_id = proposal(&db).await;
    let (run_id, _generation) = started_run(&repo, &proposal_id).await;
    // Not parked: a pending intent is live, in-flight tribunal work.
    assert!(matches!(
        repo.load_refinement_run_snapshot(request(run_id.clone()))
            .await
            .unwrap()
            .unwrap()
            .liveness,
        RefinementLivenessResult::Live {
            evidence: RefinementLivenessEvidence::PendingIntent { .. }
        }
    ));

    let error = repo
        .reap_and_admit(demand(proposal_id.clone(), "demand-while-running"))
        .await
        .expect_err("a demand must not preempt a genuinely running tribunal round");
    assert!(
        matches!(error, RefinementAdmissionError::AlreadyActive { .. }),
        "expected AlreadyActive, got {error:?}"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM refinement_dispatch_intents WHERE run_id = $1"
        )
        .bind(&run_id)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        1
    );
}

#[tokio::test]
async fn an_awaiting_evidence_park_is_not_resumed_by_a_human_demand() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal_id = proposal(&db).await;
    let (run_id, generation) = started_run(&repo, &proposal_id).await;
    park(
        &db,
        &repo,
        &run_id,
        generation,
        RefinementParkKind::AwaitingEvidence,
    )
    .await;

    // An evidence park owns an in-flight spike task and its own resume path;
    // unparking it here would strand that spike.
    let error = repo
        .reap_and_admit(demand(proposal_id.clone(), "demand-evidence-park"))
        .await
        .expect_err("an evidence park must not be resumed by a demand");
    assert!(
        matches!(error, RefinementAdmissionError::AlreadyActive { .. }),
        "expected AlreadyActive, got {error:?}"
    );
    let (state, park_kind): (String, Option<String>) =
        sqlx::query_as("SELECT state, park_kind FROM refinement_runs WHERE id = $1")
            .bind(&run_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(state, "parked");
    assert_eq!(park_kind.as_deref(), Some("awaiting_evidence"));
}
