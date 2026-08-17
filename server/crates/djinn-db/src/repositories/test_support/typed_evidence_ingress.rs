use djinn_core::models::{TribunalEvidenceLifecycle, TribunalEvidenceOutcome};

use crate::{
    Database, DisposeTypedEvidenceInput, TypedEvidenceDispositionProjection,
    TypedEvidenceRepository,
};

/// Exact finding/attempt identity for a focused durable-ingress fixture.
///
/// **Not for production use.** Panics on SQL errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedEvidenceIngressFixtureForTest {
    pub finding_id: String,
    pub attempt_id: String,
}

/// Seed the repository rows needed to submit one durable typed evidence return.
/// Raw SQL remains inside the database owner crate; cross-crate tests exercise
/// submission and replay through the production repository APIs.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn seed_typed_evidence_ingress_fixture_for_test(
    db: &Database,
    proposal_id: &str,
    spike_task_id: &str,
    check_id: &str,
) -> TypedEvidenceIngressFixtureForTest {
    db.ensure_initialized().await.unwrap();
    let mut tx = db.pool().begin().await.unwrap();
    let existing = sqlx::query_as::<_, (String, String)>(
        "SELECT f.id,a.id FROM typed_evidence_findings f \
         JOIN typed_evidence_attempts a ON a.finding_id=f.id \
         WHERE f.proposal_id=$1 AND a.spike_task_id=$2",
    )
    .bind(proposal_id)
    .bind(spike_task_id)
    .fetch_optional(&mut *tx)
    .await
    .unwrap();
    let (finding_id, attempt_id) = if let Some(identity) = existing {
        identity
    } else {
        let finding_id = uuid::Uuid::now_v7().to_string();
        let attempt_id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO typed_evidence_findings (id,proposal_id,demand_hash,lifecycle,claim,demanded_revision_seq,created_by_task_id) VALUES ($1,$2,$3,'spike_active','{}',1,$4)")
            .bind(&finding_id)
            .bind(proposal_id)
            .bind(format!("ingress-demand-{finding_id}"))
            .bind(spike_task_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO typed_evidence_attempts (id,finding_id,sequence,spike_task_id) VALUES ($1,$2,1,$3)")
            .bind(&attempt_id)
            .bind(&finding_id)
            .bind(spike_task_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        (finding_id, attempt_id)
    };
    sqlx::query("DELETE FROM typed_evidence_planned_checks WHERE attempt_id=$1")
        .bind(&attempt_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO typed_evidence_planned_checks (id,attempt_id,ordinal,check_id,method) VALUES ($1,$2,1,$3,'code')")
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(&attempt_id)
        .bind(check_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    TypedEvidenceIngressFixtureForTest {
        finding_id,
        attempt_id,
    }
}

/// Count receipt and terminal-disposition transitions for the finding owning a
/// validation result. Demand/allocation transitions predate durable ingress and
/// are intentionally outside this replay-idempotency assertion.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn typed_evidence_transition_count_for_validation_for_test(
    db: &Database,
    validation_id: &str,
) -> i64 {
    db.ensure_initialized().await.unwrap();
    sqlx::query_scalar("SELECT count(*) FROM typed_evidence_transitions WHERE finding_id=(SELECT a.finding_id FROM typed_evidence_validation_results v JOIN typed_evidence_attempts a ON a.id=v.attempt_id WHERE v.id=$1) AND to_lifecycle IN ('evidence_received','resolved','withdrawn')")
        .bind(validation_id)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

/// Apply a terminal Judge disposition through the canonical typed repository.
/// The validation lookup is test scaffolding; lifecycle mutation remains owned
/// by `TypedEvidenceRepository::dispose_in_transaction`.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn dispose_typed_evidence_validation_for_test(
    db: &Database,
    validation_id: &str,
    judge_task_id: &str,
    disposition: TribunalEvidenceLifecycle,
) -> TypedEvidenceDispositionProjection {
    db.ensure_initialized().await.unwrap();
    let mut tx = db.pool().begin().await.unwrap();
    let finding_id: String = sqlx::query_scalar(
        "SELECT a.finding_id FROM typed_evidence_validation_results v JOIN typed_evidence_attempts a ON a.id=v.attempt_id WHERE v.id=$1",
    )
    .bind(validation_id)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    let result = TypedEvidenceRepository::dispose_in_transaction(
        &mut tx,
        DisposeTypedEvidenceInput {
            disposition_id: uuid::Uuid::now_v7().to_string(),
            transition_id: uuid::Uuid::now_v7().to_string(),
            finding_id,
            validation_result_id: (disposition == TribunalEvidenceLifecycle::Resolved)
                .then(|| validation_id.to_owned()),
            folding_revision: 1,
            outcome: TribunalEvidenceOutcome::Resolved,
            disposition,
            judge_task_id: judge_task_id.to_owned(),
            rationale: "terminal replay fixture disposition".into(),
            withdrawal_is_non_load_bearing: true,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    result
}
