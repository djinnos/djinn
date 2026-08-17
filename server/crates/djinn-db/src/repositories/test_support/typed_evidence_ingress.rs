use djinn_core::models::{TribunalEvidenceLifecycle, TribunalEvidenceOutcome};
use sqlx::Row;

use crate::{
    Database, DisposeTypedEvidenceInput, TypedEvidenceDispositionProjection,
    TypedEvidenceRepository,
};

/// The production-valid outcomes available from the canonical ingress fixture.
/// This prevents a test from inventing a passed check without evidence facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalTypedEvidenceReturnOutcomeForTest {
    Resolved,
    Partial,
    Unresolved,
}

/// Read the persisted validation and every normalized child contract.
/// **Not for production use.** Panics on SQL errors.
pub async fn typed_evidence_validation_snapshot_for_test(
    db: &Database,
    validation_id: &str,
) -> TypedEvidenceValidationSnapshotForTest {
    let row = sqlx::query("SELECT v.id,v.payload_sha256,v.outcome,v.validator_facts,f.lifecycle FROM typed_evidence_validation_results v JOIN typed_evidence_attempts a ON a.id=v.attempt_id JOIN typed_evidence_findings f ON f.id=a.finding_id WHERE v.id=$1").bind(validation_id).fetch_one(db.pool()).await.unwrap();
    let checks = sqlx::query("SELECT c.id AS check_result_id,p.check_id,p.method,c.status,c.detail,i.invocation_id,i.usable FROM typed_evidence_check_results c JOIN typed_evidence_planned_checks p ON p.id=c.planned_check_id LEFT JOIN typed_evidence_invocation_provenance i ON i.check_result_id=c.id WHERE c.validation_result_id=$1 ORDER BY p.ordinal").bind(validation_id).fetch_all(db.pool()).await.unwrap().into_iter().map(|r| serde_json::json!({"check_result_id":r.get::<String,_>("check_result_id"),"check_id":r.get::<String,_>("check_id"),"method":r.get::<String,_>("method"),"status":r.get::<String,_>("status"),"detail":r.get::<Option<String>,_>("detail"),"invocation_id":r.get::<Option<String>,_>("invocation_id"),"invocation_usable":r.get::<Option<bool>,_>("usable")})).collect();
    let check_anchors = sqlx::query("SELECT a.id AS anchor_id,p.check_id,a.method,a.locator,h.health,h.detail,h.immutable_identity,h.method_compatible FROM typed_evidence_anchors a JOIN typed_evidence_anchor_health h ON h.anchor_id=a.id JOIN typed_evidence_check_results c ON c.id=a.check_result_id JOIN typed_evidence_planned_checks p ON p.id=c.planned_check_id WHERE c.validation_result_id=$1 ORDER BY p.ordinal,a.method,a.locator,a.id").bind(validation_id).fetch_all(db.pool()).await.unwrap().into_iter().map(|r| serde_json::json!({"anchor_id":r.get::<String,_>("anchor_id"),"check_id":r.get::<String,_>("check_id"),"method":r.get::<String,_>("method"),"locator":r.get::<String,_>("locator"),"health":r.get::<String,_>("health"),"detail":r.get::<Option<String>,_>("detail"),"immutable_identity":r.get::<serde_json::Value,_>("immutable_identity"),"method_compatible":r.get::<bool,_>("method_compatible")})).collect();
    let findings = sqlx::query("SELECT f.id AS finding_id,p.check_id,f.conclusion,f.usable FROM typed_evidence_return_findings f JOIN typed_evidence_planned_checks p ON p.id=f.planned_check_id WHERE f.validation_result_id=$1 ORDER BY p.ordinal").bind(validation_id).fetch_all(db.pool()).await.unwrap().into_iter().map(|r| serde_json::json!({"finding_id":r.get::<String,_>("finding_id"),"check_id":r.get::<String,_>("check_id"),"conclusion":r.get::<String,_>("conclusion"),"usable":r.get::<bool,_>("usable")})).collect();
    let finding_anchors = sqlx::query("SELECT a.id AS anchor_id,p.check_id,a.method,a.locator,a.health,a.detail,a.immutable_identity,a.method_compatible FROM typed_evidence_return_finding_anchors a JOIN typed_evidence_return_findings f ON f.id=a.finding_id JOIN typed_evidence_planned_checks p ON p.id=f.planned_check_id WHERE f.validation_result_id=$1 ORDER BY p.ordinal,a.method,a.locator,a.id").bind(validation_id).fetch_all(db.pool()).await.unwrap().into_iter().map(|r| serde_json::json!({"anchor_id":r.get::<String,_>("anchor_id"),"check_id":r.get::<String,_>("check_id"),"method":r.get::<String,_>("method"),"locator":r.get::<String,_>("locator"),"health":r.get::<String,_>("health"),"detail":r.get::<Option<String>,_>("detail"),"immutable_identity":r.get::<serde_json::Value,_>("immutable_identity"),"method_compatible":r.get::<bool,_>("method_compatible")})).collect();
    let issues = sqlx::query("SELECT p.check_id,i.kind,i.code,i.detail FROM typed_evidence_issues i JOIN typed_evidence_planned_checks p ON p.id=i.planned_check_id WHERE i.validation_result_id=$1 ORDER BY i.kind,p.ordinal,i.code").bind(validation_id).fetch_all(db.pool()).await.unwrap();
    let (mut failures, mut gaps) = (vec![], vec![]);
    for r in issues {
        let v = serde_json::json!({"check_id":r.get::<String,_>("check_id"),"code":r.get::<String,_>("code"),"detail":r.get::<String,_>("detail")});
        if r.get::<String, _>("kind") == "failure" {
            failures.push(v)
        } else {
            gaps.push(v)
        }
    }
    TypedEvidenceValidationSnapshotForTest {
        validation_id: row.get("id"),
        payload_sha256: row.get("payload_sha256"),
        outcome: row.get("outcome"),
        validator_facts: row.get("validator_facts"),
        checks,
        check_anchors,
        findings,
        finding_anchors,
        failures,
        gaps,
        finding_lifecycle: row.get("lifecycle"),
        transition_count: typed_evidence_transition_count_for_validation_for_test(
            db,
            validation_id,
        )
        .await,
    }
}

/// Exact finding/attempt identity and a repository-valid durable return.
///
/// **Not for production use.** Panics on SQL errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedEvidenceIngressFixtureForTest {
    pub finding_id: String,
    pub attempt_id: String,
    pub return_payload: String,
}

/// A complete persisted validation contract for coordinator parity assertions.
/// Each array is normalized server-owned data, including hydrated provenance.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedEvidenceValidationSnapshotForTest {
    pub validation_id: String,
    pub payload_sha256: String,
    pub outcome: String,
    pub validator_facts: serde_json::Value,
    pub checks: Vec<serde_json::Value>,
    /// Hydrated check-result anchors, including server-derived health and
    /// immutable evidence identity.
    pub check_anchors: Vec<serde_json::Value>,
    pub findings: Vec<serde_json::Value>,
    /// Hydrated return-finding anchor provenance.
    pub finding_anchors: Vec<serde_json::Value>,
    pub failures: Vec<serde_json::Value>,
    pub gaps: Vec<serde_json::Value>,
    pub finding_lifecycle: String,
    pub transition_count: i64,
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
    seed_canonical_typed_evidence_ingress_fixture_for_test(
        db,
        proposal_id,
        spike_task_id,
        check_id,
        CanonicalTypedEvidenceReturnOutcomeForTest::Resolved,
    )
    .await
}

/// Seed immutable plan/invocation facts and one canonical resolved, partial, or
/// unresolved return. **Not for production use.** Panics on SQL errors.
pub async fn seed_canonical_typed_evidence_ingress_fixture_for_test(
    db: &Database,
    proposal_id: &str,
    spike_task_id: &str,
    check_id: &str,
    expected: CanonicalTypedEvidenceReturnOutcomeForTest,
) -> TypedEvidenceIngressFixtureForTest {
    db.ensure_initialized().await.unwrap();
    let mut tx = db.pool().begin().await.unwrap();
    let project_id: String = sqlx::query_scalar("SELECT project_id FROM tasks WHERE id=$1")
        .bind(spike_task_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    // Coordinator fixtures first establish authority through
    // `set_structured_needs_evidence_spike`, while standalone repository tests
    // intentionally start without typed authority. Reuse only the exact active
    // proposal/task binding; never create a competing unresolved finding or
    // silently attach evidence facts to a different attempt.
    let existing = sqlx::query(
        "SELECT f.id AS finding_id,f.lifecycle,a.id AS attempt_id,a.evidence_plan_id \
         FROM typed_evidence_findings f \
         LEFT JOIN typed_evidence_attempts a \
           ON a.finding_id=f.id AND a.spike_task_id=$2 \
         WHERE f.proposal_id=$1 \
           AND f.lifecycle IN ('demanded','spike_active','evidence_received','failed') \
         FOR UPDATE OF f",
    )
    .bind(proposal_id)
    .bind(spike_task_id)
    .fetch_optional(&mut *tx)
    .await
    .unwrap();
    let (finding_id, attempt_id, attempt_exists) = if let Some(row) = existing {
        assert_eq!(
            row.get::<String, _>("lifecycle"),
            "spike_active",
            "canonical ingress fixture requires spike_active authority"
        );
        let attempt_id = row
            .get::<Option<String>, _>("attempt_id")
            .expect("active typed authority must belong to the exact spike task");
        assert!(
            row.get::<Option<String>, _>("evidence_plan_id").is_none(),
            "canonical ingress fixture cannot replace an attempt's immutable evidence plan"
        );
        let already_shaped: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM typed_evidence_planned_checks WHERE attempt_id=$1 UNION ALL SELECT 1 FROM typed_evidence_validation_results WHERE attempt_id=$1)",
        )
        .bind(&attempt_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert!(
            !already_shaped,
            "canonical ingress fixture requires an unshaped, unvalidated authoritative attempt"
        );
        (row.get("finding_id"), attempt_id, true)
    } else {
        (
            uuid::Uuid::now_v7().to_string(),
            uuid::Uuid::now_v7().to_string(),
            false,
        )
    };
    let session_id = uuid::Uuid::now_v7().to_string();
    let plan_id = uuid::Uuid::now_v7().to_string();
    let invocation_id = uuid::Uuid::now_v7().to_string();
    let command_id = format!("{check_id}-command");
    let secondary_id = format!("{check_id}-secondary");
    if !attempt_exists {
        sqlx::query("INSERT INTO typed_evidence_findings (id,proposal_id,demand_hash,lifecycle,claim,demanded_revision_seq,created_by_task_id) VALUES ($1,$2,$3,'spike_active','{}',1,$4)")
            .bind(&finding_id)
            .bind(proposal_id)
            .bind(format!("ingress-demand-{finding_id}"))
            .bind(spike_task_id)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO sessions (id,project_id,task_id,model_id,agent_type,status) VALUES ($1,$2,$3,'fixture','worker','running')").bind(&session_id).bind(&project_id).bind(spike_task_id).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO evidence_plans (id,spike_task_id,session_id,captured_commit_sha,worktree_fingerprint) VALUES ($1,$2,$3,'abcdef0123456789','fixture')").bind(&plan_id).bind(spike_task_id).bind(&session_id).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO evidence_plan_checks (plan_id,ordinal,check_id,question,method) VALUES ($1,1,$2,'command observation','command')").bind(&plan_id).bind(&command_id).execute(&mut *tx).await.unwrap();
    if expected == CanonicalTypedEvidenceReturnOutcomeForTest::Partial {
        sqlx::query("INSERT INTO evidence_plan_checks (plan_id,ordinal,check_id,question,method) VALUES ($1,2,$2,'unavailable observation','code')").bind(&plan_id).bind(&secondary_id).execute(&mut *tx).await.unwrap();
    }
    if !attempt_exists {
        sqlx::query("INSERT INTO typed_evidence_attempts (id,finding_id,sequence,spike_task_id,evidence_plan_id) VALUES ($1,$2,1,$3,$4)")
            .bind(&attempt_id)
            .bind(&finding_id)
            .bind(spike_task_id)
            .bind(&plan_id)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO typed_evidence_planned_checks (id,attempt_id,ordinal,check_id,method,evidence_plan_id,evidence_plan_check_id) VALUES ($1,$2,1,$3,'command',$4,$3)")
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(&attempt_id)
        .bind(&command_id).bind(&plan_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    if expected == CanonicalTypedEvidenceReturnOutcomeForTest::Partial {
        sqlx::query("INSERT INTO typed_evidence_planned_checks (id,attempt_id,ordinal,check_id,method,evidence_plan_id,evidence_plan_check_id) VALUES ($1,$2,2,$3,'code',$4,$3)").bind(uuid::Uuid::now_v7().to_string()).bind(&attempt_id).bind(&secondary_id).bind(&plan_id).execute(&mut *tx).await.unwrap();
    }
    sqlx::query("INSERT INTO evidence_command_invocations (id,plan_id,spike_task_id,session_id,captured_commit_sha,worktree_fingerprint,check_id,argv,canonical_cwd,launch_state,process_state,exit_code,timed_out) VALUES ($1,$2,$3,$4,'abcdef0123456789','fixture',$5,'[\"true\"]','/repo','launched','exited',0,FALSE)").bind(&invocation_id).bind(&plan_id).bind(spike_task_id).bind(&session_id).bind(&command_id).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let passed = serde_json::json!({"check_id":command_id,"method":"command","status":"passed","invocation_id":invocation_id,"anchors":[{"method":"command","locator":format!("command:{invocation_id}")}]});
    let (checks, failures, gaps) = match expected {
        CanonicalTypedEvidenceReturnOutcomeForTest::Resolved => (vec![passed], vec![], vec![]),
        CanonicalTypedEvidenceReturnOutcomeForTest::Partial => (
            vec![
                passed,
                serde_json::json!({"check_id":secondary_id,"method":"code","status":"failed","detail":"canonical partial failure","anchors":[]}),
            ],
            vec![
                serde_json::json!({"check_id":secondary_id,"code":"canonical_failure","detail":"canonical partial failure"}),
            ],
            vec![],
        ),
        CanonicalTypedEvidenceReturnOutcomeForTest::Unresolved => (
            vec![
                serde_json::json!({"check_id":command_id,"method":"command","status":"not_run","detail":"canonical unresolved gap","anchors":[]}),
            ],
            vec![],
            vec![
                serde_json::json!({"check_id":command_id,"code":"canonical_gap","detail":"canonical unresolved gap"}),
            ],
        ),
    };
    TypedEvidenceIngressFixtureForTest {
        finding_id: finding_id.clone(),
        attempt_id: attempt_id.clone(),
        return_payload: serde_json::json!({"version":"TribunalEvidenceReturnV1","finding_id":finding_id,"spike_task_id":spike_task_id,"attempt_id":attempt_id,"conclusion":"canonical typed evidence","checks":checks,"failures":failures,"gaps":gaps}).to_string(),
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
