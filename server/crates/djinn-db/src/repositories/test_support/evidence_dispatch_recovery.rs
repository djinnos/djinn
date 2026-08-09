//! Database-owned fixtures for coordinator evidence-dispatch recovery tests.

use crate::database::Database;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceDispatchRecoveryFixtureForTest {
    pub finding_id: String,
    pub proposal_id: String,
    pub spike_task_id: String,
    pub attempt_id: String,
    pub demand_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceDispatchRecoverySnapshotForTest {
    pub finding_id: String,
    pub proposal_id: String,
    pub demand_hash: String,
    pub lifecycle: String,
    pub attempt_id: String,
    pub attempt_sequence: i32,
    pub spike_task_id: String,
    pub legacy_link: Option<String>,
    pub finding_slot_count: i64,
    pub attempt_count: i64,
    pub dispatch_error_count: i64,
    pub task_status: String,
}

/// Materialize the exact initial or retry allocation consumed by coordinator
/// re-drive tests without exposing the database driver's API to that crate.
pub async fn materialize_evidence_dispatch_recovery_fixture_for_test(
    db: &Database,
    is_retry: bool,
) -> EvidenceDispatchRecoveryFixtureForTest {
    db.ensure_initialized().await.unwrap();
    let project_id = uuid::Uuid::now_v7().to_string();
    let user_id = uuid::Uuid::now_v7().to_string();
    let spike_task_id = uuid::Uuid::now_v7().to_string();
    let proposal_id = uuid::Uuid::now_v7().to_string();
    let finding_id = uuid::Uuid::now_v7().to_string();
    let attempt_id = uuid::Uuid::now_v7().to_string();
    let demand_hash = format!("demand-{finding_id}");
    let mut tx = db.pool().begin().await.unwrap();

    sqlx::query(
        "INSERT INTO projects (id,name,github_owner,github_repo) VALUES ($1,$2,'owner',$3)",
    )
    .bind(&project_id)
    .bind(format!("project-{project_id}"))
    .bind(format!("repo-{project_id}"))
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("INSERT INTO users (id,github_id,github_login) VALUES ($1,1,$2)")
        .bind(&user_id)
        .bind(format!("user-{user_id}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO tasks (id,project_id,short_id,title,description,design,issue_type,labels,acceptance_criteria,memory_refs,created_by_user_id) VALUES ($1,$2,$3,'evidence','','','spike','[\"refinement-evidence\",\"read-only\"]','[]','[]',$4)")
        .bind(&spike_task_id)
        .bind(&project_id)
        .bind(spike_task_id.replace('-', ""))
        .bind(&user_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO proposals (id,short_id,title,body,body_format,acceptance_criteria,status,latest_revision_seq,linked_spike_task_id,needs_evidence_claim) VALUES ($1,$2,'proposal','','markdown','[]','draft',1,$3,'{}')")
        .bind(&proposal_id)
        .bind(proposal_id.replace('-', ""))
        .bind(&spike_task_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO typed_evidence_findings (id,proposal_id,demand_hash,lifecycle,claim,demanded_revision_seq,created_by_task_id) VALUES ($1,$2,$3,'demanded','{}',1,$4)")
        .bind(&finding_id)
        .bind(&proposal_id)
        .bind(&demand_hash)
        .bind(&spike_task_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO typed_evidence_attempts (id,finding_id,sequence,spike_task_id) VALUES ($1,$2,$3,$4)")
        .bind(&attempt_id)
        .bind(&finding_id)
        .bind(if is_retry { 2 } else { 1 })
        .bind(&spike_task_id)
        .execute(&mut *tx)
        .await
        .unwrap();

    if is_retry {
        let failed_transition_id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO typed_evidence_transitions (id,finding_id,ordinal,from_lifecycle,to_lifecycle,metadata) VALUES ($1,$2,1,'spike_active','failed','{}')")
            .bind(&failed_transition_id)
            .bind(&finding_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO typed_evidence_transitions (id,finding_id,ordinal,from_lifecycle,to_lifecycle,metadata) VALUES ($1,$2,2,'failed','demanded',$3)")
            .bind(uuid::Uuid::now_v7().to_string())
            .bind(&finding_id)
            .bind(serde_json::json!({"retry_attempt_id":attempt_id,"failed_transition_id":failed_transition_id}))
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO typed_evidence_retry_idempotency (finding_id,failed_transition_id,retry_attempt_id) VALUES ($1,$2,$3)")
            .bind(&finding_id)
            .bind(&failed_transition_id)
            .bind(&attempt_id)
            .execute(&mut *tx)
            .await
            .unwrap();
    } else {
        sqlx::query("INSERT INTO typed_evidence_transitions (id,finding_id,ordinal,from_lifecycle,to_lifecycle,metadata) VALUES ($1,$2,1,NULL,'demanded','{}')")
            .bind(uuid::Uuid::now_v7().to_string())
            .bind(&finding_id)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();

    EvidenceDispatchRecoveryFixtureForTest {
        finding_id,
        proposal_id,
        spike_task_id,
        attempt_id,
        demand_hash,
    }
}

/// Capture allocation identity, lifecycle, append-only errors, and task state.
pub async fn evidence_dispatch_recovery_snapshot_for_test(
    db: &Database,
    fixture: &EvidenceDispatchRecoveryFixtureForTest,
) -> EvidenceDispatchRecoverySnapshotForTest {
    db.ensure_initialized().await.unwrap();
    let (
        finding_id,
        proposal_id,
        demand_hash,
        lifecycle,
        attempt_id,
        attempt_sequence,
        spike_task_id,
        legacy_link,
        task_status,
    ) = sqlx::query_as(
        "SELECT f.id,f.proposal_id,f.demand_hash,f.lifecycle,a.id,a.sequence,a.spike_task_id,p.linked_spike_task_id,t.status \
         FROM typed_evidence_findings f \
         JOIN typed_evidence_attempts a ON a.finding_id=f.id \
         JOIN proposals p ON p.id=f.proposal_id \
         JOIN tasks t ON t.id=a.spike_task_id \
         WHERE f.id=$1 AND a.id=$2",
    )
    .bind(&fixture.finding_id)
    .bind(&fixture.attempt_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    let finding_slot_count = sqlx::query_scalar(
        "SELECT count(*) FROM typed_evidence_findings WHERE proposal_id=$1 AND lifecycle IN ('demanded','spike_active','evidence_received','failed')",
    )
    .bind(&fixture.proposal_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    let attempt_count =
        sqlx::query_scalar("SELECT count(*) FROM typed_evidence_attempts WHERE finding_id=$1")
            .bind(&fixture.finding_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    let dispatch_error_count = sqlx::query_scalar(
        "SELECT count(*) FROM typed_evidence_retry_dispatch_errors WHERE finding_id=$1 AND attempt_id=$2 AND spike_task_id=$3",
    )
    .bind(&fixture.finding_id)
    .bind(&fixture.attempt_id)
    .bind(&fixture.spike_task_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    EvidenceDispatchRecoverySnapshotForTest {
        finding_id,
        proposal_id,
        demand_hash,
        lifecycle,
        attempt_id,
        attempt_sequence,
        spike_task_id,
        legacy_link,
        finding_slot_count,
        attempt_count,
        dispatch_error_count,
        task_status,
    }
}

pub async fn close_evidence_dispatch_task_for_test(db: &Database, task_id: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query("UPDATE tasks SET status='closed' WHERE id=$1")
        .bind(task_id)
        .execute(db.pool())
        .await
        .unwrap();
}
