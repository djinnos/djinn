//! Typed-evidence retry fixtures kept at the database boundary.

use crate::database::Database;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypedEvidenceRetryScenarioForTest {
    Failed,
    StaleFailure,
    NonFailed,
    OccupiedSlot,
}

/// Caller authority materialized alongside a retry fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypedEvidenceRetryAuthorityForTest {
    Judge,
    Advocate,
    Unauthorized,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedEvidenceRetryFixtureForTest {
    pub finding_id: String,
    pub failed_transition_id: String,
    pub latest_transition_id: String,
    pub prior_spike_task_id: String,
    pub authority_task_id: String,
    pub caller_user_id: String,
    pub authority: TypedEvidenceRetryAuthorityForTest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedEvidenceRetrySnapshotForTest {
    pub tasks: Vec<(String, String, String, String, serde_json::Value)>,
    pub attempts: Vec<(String, i32, String)>,
    pub transitions: Vec<(String, i32, Option<String>, String, serde_json::Value)>,
    pub planned_checks: Vec<(String, i32, String, String)>,
    pub debate_rows: Vec<(String, String, Option<String>)>,
    pub lifecycle_events: Vec<(String, String)>,
    pub retry_idempotency_rows: Vec<(String, String, String)>,
    pub prior_task_status: String,
    pub routing: Vec<(String, String)>,
    pub labels: Vec<(String, serde_json::Value)>,
}

/// Fixture for dispatched terminal-evidence disposition tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedEvidenceDispositionFixtureForTest {
    pub finding_id: String,
    pub validation_result_id: String,
    pub caller_user_id: String,
    pub authority_task_id: String,
}

/// All persistence relations a rejected disposition must preserve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedEvidenceDispositionSnapshotForTest {
    pub findings: Vec<(String, String)>,
    pub attempts: Vec<(String, i32, String)>,
    pub transitions: Vec<(String, i32, Option<String>, String)>,
    pub dispositions: Vec<(String, String, Option<String>, i32, String, String)>,
    pub tasks: Vec<(String, String, String)>,
    pub debate_rows: Vec<(String, String, Option<String>)>,
    pub lifecycle_events: Vec<(String, String)>,
    pub legacy_link_and_claim: (Option<String>, Option<String>),
}

/// Materialize retry and active-authority boundary states which normally
/// require a worker return. The authority task must belong to the active
/// refinement run; this helper writes its complete persisted authority tuple.
pub async fn materialize_typed_evidence_retry_fixture_for_test(
    db: &Database,
    proposal_id: &str,
    project_id: &str,
    authority_task_id: &str,
    caller_user_id: &str,
    scenario: TypedEvidenceRetryScenarioForTest,
    authority: TypedEvidenceRetryAuthorityForTest,
) -> TypedEvidenceRetryFixtureForTest {
    db.ensure_initialized().await.unwrap();
    let (authority_role, authority_phase) = match authority {
        TypedEvidenceRetryAuthorityForTest::Judge => ("judge", "judge_adjudication"),
        TypedEvidenceRetryAuthorityForTest::Advocate => ("advocate", "advocate_revision"),
        TypedEvidenceRetryAuthorityForTest::Unauthorized => ("judge", "judge_adjudication"),
    };
    let fixture_caller_user_id = if authority == TypedEvidenceRetryAuthorityForTest::Unauthorized {
        // Session identity is an opaque string; no user row is needed to prove
        // that it cannot match the authority task's persisted creator.
        uuid::Uuid::now_v7().to_string()
    } else {
        caller_user_id.to_owned()
    };
    let finding_id = uuid::Uuid::now_v7().to_string();
    let prior_spike_task_id = uuid::Uuid::now_v7().to_string();
    let failed_transition_id = uuid::Uuid::now_v7().to_string();
    let latest_transition_id = if scenario == TypedEvidenceRetryScenarioForTest::StaleFailure {
        uuid::Uuid::now_v7().to_string()
    } else {
        failed_transition_id.clone()
    };
    let attempt_id = uuid::Uuid::now_v7().to_string();
    let lifecycle = if scenario == TypedEvidenceRetryScenarioForTest::NonFailed {
        "spike_active"
    } else {
        "failed"
    };
    let mut tx = db.pool().begin().await.unwrap();
    sqlx::query(
        "UPDATE refinement_dispatch_intents SET role=$1, phase=$2, state='materialized' \
         WHERE task_id=$3",
    )
    .bind(authority_role)
    .bind(authority_phase)
    .bind(authority_task_id)
    .execute(&mut *tx)
    .await
    .expect("failed to materialize retry authority intent");
    sqlx::query(
        "UPDATE tasks SET agent_type=$1, refinement_role=$1, refinement_phase=$2, status='open' \
         WHERE id=$3",
    )
    .bind(authority_role)
    .bind(authority_phase)
    .bind(authority_task_id)
    .execute(&mut *tx)
    .await
    .expect("failed to materialize retry authority task");
    sqlx::query("INSERT INTO tasks (id,project_id,short_id,title,description,design,issue_type,priority,owner,status,labels,acceptance_criteria,created_by_user_id,agent_type) VALUES ($1,$2,$3,'Prior evidence spike','terminal retry fixture','','spike',0,'','closed',$4,'[]'::jsonb,$5,'architect')")
        .bind(&prior_spike_task_id).bind(project_id).bind(format!(
            "r{}",
            &prior_spike_task_id[prior_spike_task_id.len() - 7..]
        )).bind(serde_json::json!(["refinement-evidence", "read-only"])).bind(caller_user_id).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO typed_evidence_findings (id,proposal_id,demand_hash,lifecycle,claim,demanded_revision_seq,created_by_task_id) VALUES ($1,$2,$3,$4,$5,1,$6)")
        .bind(&finding_id).bind(proposal_id).bind(format!("retry-fixture-{finding_id}")).bind(lifecycle).bind(serde_json::json!({"fixture":"retry"})).bind(authority_task_id).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO typed_evidence_attempts (id,finding_id,sequence,spike_task_id) VALUES ($1,$2,1,$3)").bind(&attempt_id).bind(&finding_id).bind(&prior_spike_task_id).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO typed_evidence_planned_checks (id,attempt_id,ordinal,check_id,method) VALUES ($1,$2,1,'retry-fixture-check','code')").bind(uuid::Uuid::now_v7().to_string()).bind(&attempt_id).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO typed_evidence_transitions (id,finding_id,ordinal,from_lifecycle,to_lifecycle,actor_task_id,metadata) VALUES ($1,$2,1,NULL,'demanded',$3,'{}'),($4,$2,2,'demanded','spike_active',$3,'{}'),($5,$2,3,'spike_active',$6,$3,'{}')")
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(&finding_id)
        .bind(authority_task_id)
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(&failed_transition_id)
        .bind(lifecycle)
        .execute(&mut *tx)
        .await
        .unwrap();
    if scenario == TypedEvidenceRetryScenarioForTest::StaleFailure {
        sqlx::query("INSERT INTO typed_evidence_transitions (id,finding_id,ordinal,from_lifecycle,to_lifecycle,actor_task_id,metadata) VALUES ($1,$2,4,'failed','failed',$3,'{}')").bind(&latest_transition_id).bind(&finding_id).bind(authority_task_id).execute(&mut *tx).await.unwrap();
    }
    if scenario == TypedEvidenceRetryScenarioForTest::OccupiedSlot {
        let occupied = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO tasks (id,project_id,short_id,title,description,design,issue_type,priority,owner,status,labels,acceptance_criteria,created_by_user_id,agent_type) VALUES ($1,$2,$3,'Occupied evidence spike','fixture conflict','','spike',0,'','open',$4,'[]'::jsonb,$5,'architect')").bind(&occupied).bind(project_id).bind(format!("e{}", &occupied[..7])).bind(serde_json::json!(["refinement-evidence", "read-only"])).bind(caller_user_id).execute(&mut *tx).await.unwrap();
        sqlx::query("UPDATE proposals SET linked_spike_task_id=$1 WHERE id=$2")
            .bind(occupied)
            .bind(proposal_id)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    TypedEvidenceRetryFixtureForTest {
        finding_id,
        failed_transition_id,
        latest_transition_id,
        prior_spike_task_id,
        authority_task_id: authority_task_id.into(),
        caller_user_id: fixture_caller_user_id,
        authority,
    }
}

/// Capture all retry-owned relations without raw SQL in consumer tests.
pub async fn typed_evidence_retry_snapshot_for_test(
    db: &Database,
    proposal_id: &str,
    finding_id: &str,
    prior_spike_task_id: &str,
) -> TypedEvidenceRetrySnapshotForTest {
    db.ensure_initialized().await.unwrap();
    let tasks = sqlx::query_as("SELECT id,status,issue_type,agent_type,labels FROM tasks WHERE project_id=(SELECT project_id FROM proposal_targets WHERE proposal_id=$1 LIMIT 1) ORDER BY id").bind(proposal_id).fetch_all(db.pool()).await.unwrap();
    let attempts = sqlx::query_as("SELECT id,sequence,spike_task_id FROM typed_evidence_attempts WHERE finding_id=$1 ORDER BY sequence").bind(finding_id).fetch_all(db.pool()).await.unwrap();
    let transitions = sqlx::query_as("SELECT id,ordinal,from_lifecycle,to_lifecycle,metadata FROM typed_evidence_transitions WHERE finding_id=$1 ORDER BY ordinal").bind(finding_id).fetch_all(db.pool()).await.unwrap();
    let planned_checks = sqlx::query_as("SELECT c.attempt_id,c.ordinal,c.check_id,c.method FROM typed_evidence_planned_checks c JOIN typed_evidence_attempts a ON a.id=c.attempt_id WHERE a.finding_id=$1 ORDER BY a.sequence,c.ordinal").bind(finding_id).fetch_all(db.pool()).await.unwrap();
    let debate_rows = sqlx::query_as(
        "SELECT id,kind,source_task_id FROM proposal_debate_trail WHERE proposal_id=$1 ORDER BY id",
    )
    .bind(proposal_id)
    .fetch_all(db.pool())
    .await
    .unwrap();
    let lifecycle_events = sqlx::query_as(
        "SELECT id,event_kind FROM proposal_revisions WHERE proposal_id=$1 ORDER BY id",
    )
    .bind(proposal_id)
    .fetch_all(db.pool())
    .await
    .unwrap();
    let retry_idempotency_rows = sqlx::query_as("SELECT finding_id,failed_transition_id,retry_attempt_id FROM typed_evidence_retry_idempotency WHERE finding_id=$1 ORDER BY failed_transition_id").bind(finding_id).fetch_all(db.pool()).await.unwrap();
    let prior_task_status = sqlx::query_scalar("SELECT status FROM tasks WHERE id=$1")
        .bind(prior_spike_task_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    let routing = sqlx::query_as("SELECT id,agent_type FROM tasks WHERE project_id=(SELECT project_id FROM proposal_targets WHERE proposal_id=$1 LIMIT 1) ORDER BY id").bind(proposal_id).fetch_all(db.pool()).await.unwrap();
    let labels = sqlx::query_as("SELECT id,labels FROM tasks WHERE project_id=(SELECT project_id FROM proposal_targets WHERE proposal_id=$1 LIMIT 1) ORDER BY id").bind(proposal_id).fetch_all(db.pool()).await.unwrap();
    TypedEvidenceRetrySnapshotForTest {
        tasks,
        attempts,
        transitions,
        planned_checks,
        debate_rows,
        lifecycle_events,
        retry_idempotency_rows,
        prior_task_status,
        routing,
        labels,
    }
}

pub async fn materialize_typed_evidence_disposition_fixture_for_test(
    db: &Database,
    proposal_id: &str,
    project_id: &str,
    judge_task_id: &str,
    caller_user_id: &str,
) -> TypedEvidenceDispositionFixtureForTest {
    db.ensure_initialized().await.unwrap();
    let finding_id = uuid::Uuid::now_v7().to_string();
    let attempt_id = uuid::Uuid::now_v7().to_string();
    let validation_result_id = uuid::Uuid::now_v7().to_string();
    let spike = uuid::Uuid::now_v7().to_string();
    let claim = serde_json::json!({
        "fixture": "disposition",
        "created_by_task_id": judge_task_id,
        "against_revision_seq": 1,
    });
    let demand_hash = crate::repositories::typed_evidence::legacy_demand_hash(&claim, Some(&spike));
    let mut tx = db.pool().begin().await.unwrap();
    sqlx::query("INSERT INTO tasks (id,project_id,short_id,title,description,design,issue_type,priority,owner,status,labels,acceptance_criteria,created_by_user_id,agent_type) VALUES ($1,$2,$3,'Disposition spike','fixture','','spike',0,'','open',$4,'[]'::jsonb,$5,'architect')").bind(&spike).bind(project_id).bind(format!("d{}", &spike[..7])).bind(serde_json::json!(["refinement-evidence", "read-only"])).bind(caller_user_id).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO typed_evidence_findings (id,proposal_id,demand_hash,lifecycle,claim,demanded_revision_seq,created_by_task_id) VALUES ($1,$2,$3,'evidence_received',$4,1,$5)").bind(&finding_id).bind(proposal_id).bind(&demand_hash).bind(&claim).bind(judge_task_id).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO typed_evidence_attempts (id,finding_id,sequence,spike_task_id) VALUES ($1,$2,1,$3)").bind(&attempt_id).bind(&finding_id).bind(&spike).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO typed_evidence_validation_results (id,attempt_id,payload_sha256,outcome,validator_facts) VALUES ($1,$2,'fixture-sha','resolved','{}')").bind(&validation_result_id).bind(&attempt_id).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO typed_evidence_transitions (id,finding_id,ordinal,from_lifecycle,to_lifecycle,actor_task_id,metadata) VALUES ($1,$2,1,NULL,'demanded',$3,'{}'),($4,$2,2,'demanded','spike_active',$3,'{}'),($5,$2,3,'spike_active','evidence_received',$3,'{}')").bind(uuid::Uuid::now_v7().to_string()).bind(&finding_id).bind(judge_task_id).bind(uuid::Uuid::now_v7().to_string()).bind(uuid::Uuid::now_v7().to_string()).execute(&mut *tx).await.unwrap();
    sqlx::query(
        "UPDATE proposals SET linked_spike_task_id=NULL,needs_evidence_claim=NULL WHERE id=$1",
    )
    .bind(proposal_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    TypedEvidenceDispositionFixtureForTest {
        finding_id,
        validation_result_id,
        caller_user_id: caller_user_id.into(),
        authority_task_id: judge_task_id.into(),
    }
}

pub async fn typed_evidence_disposition_snapshot_for_test(
    db: &Database,
    proposal_id: &str,
) -> TypedEvidenceDispositionSnapshotForTest {
    db.ensure_initialized().await.unwrap();
    let findings = sqlx::query_as(
        "SELECT id,lifecycle FROM typed_evidence_findings WHERE proposal_id=$1 ORDER BY id",
    )
    .bind(proposal_id)
    .fetch_all(db.pool())
    .await
    .unwrap();
    let attempts = sqlx::query_as("SELECT a.id,a.sequence,a.spike_task_id FROM typed_evidence_attempts a JOIN typed_evidence_findings f ON f.id=a.finding_id WHERE f.proposal_id=$1 ORDER BY a.id").bind(proposal_id).fetch_all(db.pool()).await.unwrap();
    let transitions = sqlx::query_as("SELECT t.id,t.ordinal,t.from_lifecycle,t.to_lifecycle FROM typed_evidence_transitions t JOIN typed_evidence_findings f ON f.id=t.finding_id WHERE f.proposal_id=$1 ORDER BY t.id").bind(proposal_id).fetch_all(db.pool()).await.unwrap();
    let dispositions = sqlx::query_as("SELECT d.id,d.finding_id,d.validation_result_id,d.folding_revision,d.outcome,d.disposition FROM typed_evidence_dispositions d JOIN typed_evidence_findings f ON f.id=d.finding_id WHERE f.proposal_id=$1 ORDER BY d.id").bind(proposal_id).fetch_all(db.pool()).await.unwrap();
    let tasks = sqlx::query_as("SELECT id,status,agent_type FROM tasks WHERE project_id=(SELECT project_id FROM proposal_targets WHERE proposal_id=$1 LIMIT 1) ORDER BY id").bind(proposal_id).fetch_all(db.pool()).await.unwrap();
    let debate_rows = sqlx::query_as(
        "SELECT id,kind,source_task_id FROM proposal_debate_trail WHERE proposal_id=$1 ORDER BY id",
    )
    .bind(proposal_id)
    .fetch_all(db.pool())
    .await
    .unwrap();
    let lifecycle_events = sqlx::query_as(
        "SELECT id,event_kind FROM proposal_revisions WHERE proposal_id=$1 ORDER BY id",
    )
    .bind(proposal_id)
    .fetch_all(db.pool())
    .await
    .unwrap();
    let legacy_link_and_claim = sqlx::query_as(
        "SELECT linked_spike_task_id,needs_evidence_claim FROM proposals WHERE id=$1",
    )
    .bind(proposal_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    TypedEvidenceDispositionSnapshotForTest {
        findings,
        attempts,
        transitions,
        dispositions,
        tasks,
        debate_rows,
        lifecycle_events,
        legacy_link_and_claim,
    }
}
