//! Typed-evidence retry fixtures kept at the database boundary.

use crate::database::Database;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypedEvidenceRetryScenarioForTest {
    Failed,
    StaleFailure,
    NonFailed,
    OccupiedSlot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedEvidenceRetryFixtureForTest {
    pub finding_id: String,
    pub failed_transition_id: String,
    pub latest_transition_id: String,
    pub prior_spike_task_id: String,
    pub authority_task_id: String,
    pub caller_user_id: String,
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

/// Materialize retry boundary states which normally require a worker return.
pub async fn materialize_typed_evidence_retry_fixture_for_test(
    db: &Database,
    proposal_id: &str,
    project_id: &str,
    authority_task_id: &str,
    caller_user_id: &str,
    scenario: TypedEvidenceRetryScenarioForTest,
) -> TypedEvidenceRetryFixtureForTest {
    db.ensure_initialized().await.unwrap();
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
    sqlx::query("INSERT INTO tasks (id,project_id,short_id,title,description,design,issue_type,priority,owner,status,labels,acceptance_criteria,created_by_user_id,agent_type) VALUES ($1,$2,$3,'Prior evidence spike','terminal retry fixture','','spike',0,'','closed',$4,'[]'::jsonb,$5,'architect')")
        .bind(&prior_spike_task_id).bind(project_id).bind(format!("e{}", &prior_spike_task_id[..7])).bind(serde_json::json!(["refinement-evidence", "read-only"])).bind(caller_user_id).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO typed_evidence_findings (id,proposal_id,demand_hash,lifecycle,claim,demanded_revision_seq,created_by_task_id) VALUES ($1,$2,$3,$4,$5,1,$6)")
        .bind(&finding_id).bind(proposal_id).bind(format!("retry-fixture-{finding_id}")).bind(lifecycle).bind(serde_json::json!({"fixture":"retry"})).bind(authority_task_id).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO typed_evidence_attempts (id,finding_id,sequence,spike_task_id) VALUES ($1,$2,1,$3)").bind(&attempt_id).bind(&finding_id).bind(&prior_spike_task_id).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO typed_evidence_planned_checks (id,attempt_id,ordinal,check_id,method) VALUES ($1,$2,1,'retry-fixture-check','code')").bind(uuid::Uuid::now_v7().to_string()).bind(&attempt_id).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO typed_evidence_transitions (id,finding_id,ordinal,from_lifecycle,to_lifecycle,actor_task_id,metadata) VALUES ($1,$2,1,NULL,'demanded',$3,'{}'),($4,$2,2,'demanded','spike_active',$3,'{}'),($5,$2,3,'spike_active',$6,$3,'{}')")
        .bind(uuid::Uuid::now_v7().to_string()).bind(&finding_id).bind(authority_task_id).bind(&failed_transition_id).bind(lifecycle).execute(&mut *tx).await.unwrap();
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
        caller_user_id: caller_user_id.into(),
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
