use djinn_core::models::TribunalEvidenceAnchorMethod;
use djinn_db::{
    AllocateTypedEvidenceRetryInput, Database,
    DispatchTypedEvidenceRetryInput, PlannedTypedEvidenceCheckInput, TypedEvidenceRepository,
    TypedEvidenceRetryDispatchErrorInput,
};

async fn task(db: &Database, project: &str, user: &str) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO tasks (id,project_id,short_id,title,description,design,labels,acceptance_criteria,memory_refs,created_by_user_id) VALUES ($1,$2,$3,'evidence','','','[]','[]','[]',$4)")
        .bind(&id).bind(project).bind(id.replace('-', "")).bind(user).execute(db.pool()).await.unwrap();
    id
}

#[tokio::test]
async fn typed_evidence_retry_v1() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!("fixtures/typed_evidence_retry_v1.json")).unwrap();
    assert_eq!(fixture["version"], "typed_evidence_retry_v1");
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let project = uuid::Uuid::now_v7().to_string();
    let user = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO projects (id,name,github_owner,github_repo) VALUES ($1,$2,'o',$3)").bind(&project).bind(format!("p{project}")).bind(format!("r{project}")).execute(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO users (id,github_id,github_login) VALUES ($1,1,$2)").bind(&user).bind(format!("u{user}")).execute(db.pool()).await.unwrap();
    let old_task = task(&db, &project, &user).await;
    let retry_task = task(&db, &project, &user).await;
    let proposal = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO proposals (id,short_id,title,body,body_format,acceptance_criteria,status,latest_revision_seq) VALUES ($1,$2,'x','','markdown','[]','draft',1)").bind(&proposal).bind(proposal.replace('-', "")).execute(db.pool()).await.unwrap();
    let finding = uuid::Uuid::now_v7().to_string();
    let old_attempt = uuid::Uuid::now_v7().to_string();
    let failed = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO typed_evidence_findings (id,proposal_id,demand_hash,lifecycle,claim,demanded_revision_seq,created_by_task_id) VALUES ($1,$2,$3,'failed','{}',1,$4)").bind(&finding).bind(&proposal).bind(format!("h{finding}")).bind(&old_task).execute(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO typed_evidence_attempts (id,finding_id,sequence,spike_task_id) VALUES ($1,$2,1,$3)").bind(&old_attempt).bind(&finding).bind(&old_task).execute(db.pool()).await.unwrap();
    for (ordinal, (id, from, to)) in [(1, uuid::Uuid::now_v7().to_string(), None, "demanded"), (2, uuid::Uuid::now_v7().to_string(), Some("demanded"), "spike_active"), (3, failed.clone(), Some("spike_active"), "failed")] {
        sqlx::query("INSERT INTO typed_evidence_transitions (id,finding_id,ordinal,from_lifecycle,to_lifecycle,metadata) VALUES ($1,$2,$3,$4,$5,'{}')").bind(id).bind(&finding).bind(ordinal).bind(from).bind(to).execute(db.pool()).await.unwrap();
    }
    sqlx::query("UPDATE tasks SET status='closed' WHERE id=$1").bind(&old_task).execute(db.pool()).await.unwrap();
    let retry_attempt = uuid::Uuid::now_v7().to_string();
    let demanded = uuid::Uuid::now_v7().to_string();
    let input = || AllocateTypedEvidenceRetryInput { finding_id: finding.clone(), failed_transition_id: failed.clone(), retry_attempt_id: retry_attempt.clone(), retry_spike_task_id: retry_task.clone(), evidence_plan_id: None, planned_checks: vec![PlannedTypedEvidenceCheckInput { id: uuid::Uuid::now_v7().to_string(), ordinal: 1, check_id: "retry-check".into(), method: TribunalEvidenceAnchorMethod::Code, evidence_plan_id: None, evidence_plan_check_id: None }], demanded_transition_id: demanded.clone(), actor_task_id: Some(old_task.clone()) };
    let mut tx = db.pool().begin().await.unwrap();
    let allocation = TypedEvidenceRepository::allocate_retry_in_transaction(&mut tx, input()).await.unwrap();
    assert_eq!(allocation.sequence, 2);
    assert_eq!(allocation.planned_checks.len(), 1);
    let duplicate = TypedEvidenceRepository::allocate_retry_in_transaction(&mut tx, input()).await.unwrap();
    assert_eq!(duplicate.attempt_id, retry_attempt);
    tx.commit().await.unwrap();
    let repo = TypedEvidenceRepository::new(db.clone());
    assert_eq!(repo.retry_attempt_for_failure(&finding, &failed).await.unwrap().unwrap().attempt_id, retry_attempt);
    assert!(repo.append_retry_dispatch_error(TypedEvidenceRetryDispatchErrorInput { finding_id: finding.clone(), attempt_id: old_attempt.clone(), spike_task_id: old_task.clone(), error: "old".into() }).await.is_err());
    repo.append_retry_dispatch_error(TypedEvidenceRetryDispatchErrorInput { finding_id: finding.clone(), attempt_id: retry_attempt.clone(), spike_task_id: retry_task.clone(), error: "dispatch failed".into() }).await.unwrap();
    let mut tx = db.pool().begin().await.unwrap();
    assert!(TypedEvidenceRepository::dispatch_retry_success_in_transaction(&mut tx, DispatchTypedEvidenceRetryInput { finding_id: finding.clone(), attempt_id: old_attempt.clone(), spike_task_id: old_task.clone(), transition_id: uuid::Uuid::now_v7().to_string(), actor_task_id: None }).await.is_err());
    TypedEvidenceRepository::dispatch_retry_success_in_transaction(&mut tx, DispatchTypedEvidenceRetryInput { finding_id: finding.clone(), attempt_id: retry_attempt.clone(), spike_task_id: retry_task.clone(), transition_id: uuid::Uuid::now_v7().to_string(), actor_task_id: None }).await.unwrap();
    tx.commit().await.unwrap();
    let payload = serde_json::json!({"version":"TribunalEvidenceReturnV1","finding_id":finding,"spike_task_id":retry_task,"attempt_id":retry_attempt,"conclusion":"done","checks":[{"check_id":"retry-check","method":"code","status":"passed","anchors":[]} ]});
    assert_eq!(repo.submit_return_v1(&serde_json::to_vec(&payload).unwrap()).await.unwrap().lifecycle.as_str(), "evidence_received");
}
