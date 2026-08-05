use djinn_core::models::TribunalEvidenceAnchorMethod;
use djinn_db::{
    AllocateTypedEvidenceRetryInput, Database, DispatchTypedEvidenceRetryInput,
    PlannedTypedEvidenceCheckInput, TypedEvidenceRepository, TypedEvidenceRetryDispatchErrorInput,
};
use sha2::{Digest, Sha256};

async fn failed_attempt_snapshot(
    db: &Database,
    finding_id: &str,
    attempt_id: &str,
) -> serde_json::Value {
    sqlx::query_scalar(
        "SELECT jsonb_build_object( \
           'attempt',(SELECT jsonb_build_object('id',a.id,'finding_id',a.finding_id,'sequence',a.sequence,'spike_task_id',a.spike_task_id,'evidence_plan_id',a.evidence_plan_id) FROM typed_evidence_attempts a WHERE a.id=$2), \
           'checks',(SELECT COALESCE(jsonb_agg(to_jsonb(c) - 'id' ORDER BY c.ordinal),'[]'::jsonb) FROM typed_evidence_planned_checks c WHERE c.attempt_id=$2), \
           'validation',(SELECT jsonb_build_object('attempt_id',v.attempt_id,'payload_sha256',v.payload_sha256,'outcome',v.outcome,'validator_facts',v.validator_facts) FROM typed_evidence_validation_results v WHERE v.attempt_id=$2), \
           'results',(SELECT COALESCE(jsonb_agg(jsonb_build_object('status',r.status,'detail',r.detail) ORDER BY r.status),'[]'::jsonb) FROM typed_evidence_check_results r JOIN typed_evidence_validation_results v ON v.id=r.validation_result_id WHERE v.attempt_id=$2), \
           'issues',(SELECT COALESCE(jsonb_agg(jsonb_build_object('kind',i.kind,'code',i.code,'detail',i.detail) ORDER BY i.kind,i.code),'[]'::jsonb) FROM typed_evidence_issues i JOIN typed_evidence_validation_results v ON v.id=i.validation_result_id WHERE v.attempt_id=$2), \
           'transitions',(SELECT jsonb_agg(jsonb_build_object('ordinal',t.ordinal,'from',t.from_lifecycle,'to',t.to_lifecycle,'actor_task_id',t.actor_task_id,'metadata',t.metadata) ORDER BY t.ordinal) FROM typed_evidence_transitions t WHERE t.finding_id=$1 AND t.ordinal<=3) \
         )",
    )
    .bind(finding_id)
    .bind(attempt_id)
    .fetch_one(db.pool())
    .await
    .unwrap()
}

async fn task(db: &Database, project: &str, user: &str, evidence_spike: bool) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    let labels = if evidence_spike {
        r#"["refinement-evidence","read-only"]"#
    } else {
        "[]"
    };
    sqlx::query("INSERT INTO tasks (id,project_id,short_id,title,description,design,labels,acceptance_criteria,memory_refs,created_by_user_id) VALUES ($1,$2,$3,'evidence','','','[]','[]','[]',$4)")
        .bind(&id).bind(project).bind(id.replace('-', "")).bind(user).execute(db.pool()).await.unwrap();
    sqlx::query("UPDATE tasks SET labels=$1::jsonb WHERE id=$2")
        .bind(labels)
        .bind(&id)
        .execute(db.pool())
        .await
        .unwrap();
    id
}

#[tokio::test]
async fn typed_evidence_retry_v1() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/typed_evidence_retry_v1.json")).unwrap();
    assert_eq!(fixture["version"], "typed_evidence_retry_v1");
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let project = uuid::Uuid::now_v7().to_string();
    let user = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO projects (id,name,github_owner,github_repo) VALUES ($1,$2,'o',$3)")
        .bind(&project)
        .bind(format!("p{project}"))
        .bind(format!("r{project}"))
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO users (id,github_id,github_login) VALUES ($1,1,$2)")
        .bind(&user)
        .bind(format!("u{user}"))
        .execute(db.pool())
        .await
        .unwrap();
    let old_task = task(&db, &project, &user, false).await;
    let retry_task = task(&db, &project, &user, true).await;
    let ordinary_task = task(&db, &project, &user, false).await;
    let proposal = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO proposals (id,short_id,title,body,body_format,acceptance_criteria,status,latest_revision_seq) VALUES ($1,$2,'x','','markdown','[]','draft',1)").bind(&proposal).bind(proposal.replace('-', "")).execute(db.pool()).await.unwrap();
    let finding = uuid::Uuid::now_v7().to_string();
    let old_attempt = uuid::Uuid::now_v7().to_string();
    let failed = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO typed_evidence_findings (id,proposal_id,demand_hash,lifecycle,claim,demanded_revision_seq,created_by_task_id) VALUES ($1,$2,$3,'failed','{}',1,$4)").bind(&finding).bind(&proposal).bind(format!("h{finding}")).bind(&old_task).execute(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO typed_evidence_attempts (id,finding_id,sequence,spike_task_id) VALUES ($1,$2,1,$3)").bind(&old_attempt).bind(&finding).bind(&old_task).execute(db.pool()).await.unwrap();
    let old_check = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO typed_evidence_planned_checks (id,attempt_id,ordinal,check_id,method) VALUES ($1,$2,1,'old-check','code')")
        .bind(&old_check)
        .bind(&old_attempt)
        .execute(db.pool())
        .await
        .unwrap();
    let old_payload = serde_json::json!({
        "version":"TribunalEvidenceReturnV1",
        "finding_id":finding,
        "spike_task_id":old_task,
        "attempt_id":old_attempt,
        "conclusion":"failed attempt",
        "checks":[{"check_id":"old-check","method":"code","status":"failed","detail":"repository unavailable","anchors":[]}],
        "failures":[{"check_id":"old-check","code":"repository_unavailable","detail":"repository unavailable"}]
    });
    let old_payload_bytes = serde_json::to_vec(&old_payload).unwrap();
    let old_hash = format!("{:x}", Sha256::digest(&old_payload_bytes));
    let validation = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO typed_evidence_validation_results (id,attempt_id,payload_sha256,outcome,validator_facts) VALUES ($1,$2,$3,'unresolved',$4)")
        .bind(&validation).bind(&old_attempt).bind(old_hash)
        .bind(serde_json::json!({"validator_version":"TribunalEvidenceReturnV1","raw_payload_sha256":format!("{:x}", Sha256::digest(&old_payload_bytes)),"server_hydrated":true,"outcome":"unresolved"}))
        .execute(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO typed_evidence_check_results (id,validation_result_id,planned_check_id,status,detail) VALUES ($1,$2,$3,'failed','repository unavailable')")
        .bind(uuid::Uuid::now_v7().to_string()).bind(&validation).bind(&old_check)
        .execute(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO typed_evidence_issues (id,validation_result_id,planned_check_id,kind,code,detail) VALUES ($1,$2,$3,'failure','repository_unavailable','repository unavailable')")
        .bind(uuid::Uuid::now_v7().to_string()).bind(&validation).bind(&old_check)
        .execute(db.pool()).await.unwrap();
    for (ordinal, id, from, to) in [
        (1, uuid::Uuid::now_v7().to_string(), None, "demanded"),
        (
            2,
            uuid::Uuid::now_v7().to_string(),
            Some("demanded"),
            "spike_active",
        ),
        (3, failed.clone(), Some("spike_active"), "failed"),
    ] {
        sqlx::query("INSERT INTO typed_evidence_transitions (id,finding_id,ordinal,from_lifecycle,to_lifecycle,metadata) VALUES ($1,$2,$3,$4,$5,'{}')").bind(id).bind(&finding).bind(ordinal).bind(from).bind(to).execute(db.pool()).await.unwrap();
    }
    let failed_snapshot = failed_attempt_snapshot(&db, &finding, &old_attempt).await;
    let retry_attempt = uuid::Uuid::now_v7().to_string();
    let demanded = uuid::Uuid::now_v7().to_string();
    let input = || AllocateTypedEvidenceRetryInput {
        finding_id: finding.clone(),
        failed_transition_id: failed.clone(),
        retry_attempt_id: retry_attempt.clone(),
        retry_spike_task_id: retry_task.clone(),
        evidence_plan_id: None,
        planned_checks: vec![PlannedTypedEvidenceCheckInput {
            id: uuid::Uuid::now_v7().to_string(),
            ordinal: 1,
            check_id: "retry-check".into(),
            method: TribunalEvidenceAnchorMethod::Code,
            evidence_plan_id: None,
            evidence_plan_check_id: None,
        }],
        demanded_transition_id: demanded.clone(),
        actor_task_id: Some(old_task.clone()),
    };
    let mut tx = db.pool().begin().await.unwrap();
    assert!(
        TypedEvidenceRepository::allocate_retry_in_transaction(&mut tx, input())
            .await
            .is_err()
    );
    sqlx::query("UPDATE tasks SET status='closed' WHERE id=$1")
        .bind(&old_task)
        .execute(&mut *tx)
        .await
        .unwrap();
    let mut ordinary_input = input();
    ordinary_input.retry_spike_task_id = ordinary_task;
    assert!(
        TypedEvidenceRepository::allocate_retry_in_transaction(&mut tx, ordinary_input)
            .await
            .is_err()
    );
    let mut stale_input = input();
    stale_input.failed_transition_id = uuid::Uuid::now_v7().to_string();
    assert!(
        TypedEvidenceRepository::allocate_retry_in_transaction(&mut tx, stale_input)
            .await
            .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM typed_evidence_attempts WHERE finding_id=$1"
        )
        .bind(&finding)
        .fetch_one(&mut *tx)
        .await
        .unwrap(),
        1
    );
    let allocation = TypedEvidenceRepository::allocate_retry_in_transaction(&mut tx, input())
        .await
        .unwrap();
    assert_eq!(allocation.sequence, 2);
    assert_eq!(allocation.planned_checks.len(), 1);
    let duplicate = TypedEvidenceRepository::allocate_retry_in_transaction(&mut tx, input())
        .await
        .unwrap();
    assert_eq!(duplicate.attempt_id, retry_attempt);
    tx.commit().await.unwrap();
    let repo = TypedEvidenceRepository::new(db.clone());
    let reserved = repo
        .retry_attempt_for_failure(&finding, &failed)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reserved.attempt_id, retry_attempt);
    assert_eq!(reserved.planned_checks.len(), 1);
    assert_eq!(
        sqlx::query_scalar::<_, i32>("SELECT sequence FROM typed_evidence_attempts WHERE id=$1")
            .bind(&old_attempt)
            .fetch_one(db.pool())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM typed_evidence_transitions WHERE finding_id=$1"
        )
        .bind(&finding)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        4
    );
    assert!(
        repo.append_retry_dispatch_error(TypedEvidenceRetryDispatchErrorInput {
            finding_id: finding.clone(),
            attempt_id: old_attempt.clone(),
            spike_task_id: old_task.clone(),
            error: "old".into()
        })
        .await
        .is_err()
    );
    repo.append_retry_dispatch_error(TypedEvidenceRetryDispatchErrorInput {
        finding_id: finding.clone(),
        attempt_id: retry_attempt.clone(),
        spike_task_id: retry_task.clone(),
        error: "dispatch failed".into(),
    })
    .await
    .unwrap();
    let mut tx = db.pool().begin().await.unwrap();
    assert!(
        TypedEvidenceRepository::dispatch_retry_success_in_transaction(
            &mut tx,
            DispatchTypedEvidenceRetryInput {
                finding_id: finding.clone(),
                attempt_id: old_attempt.clone(),
                spike_task_id: old_task.clone(),
                transition_id: uuid::Uuid::now_v7().to_string(),
                actor_task_id: None
            }
        )
        .await
        .is_err()
    );
    TypedEvidenceRepository::dispatch_retry_success_in_transaction(
        &mut tx,
        DispatchTypedEvidenceRetryInput {
            finding_id: finding.clone(),
            attempt_id: retry_attempt.clone(),
            spike_task_id: retry_task.clone(),
            transition_id: uuid::Uuid::now_v7().to_string(),
            actor_task_id: None,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let transition_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM typed_evidence_transitions WHERE finding_id=$1")
            .bind(&finding)
            .fetch_one(db.pool())
            .await
            .unwrap();
    let validation_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM typed_evidence_validation_results")
            .fetch_one(db.pool())
            .await
            .unwrap();
    let replay = repo.submit_return_v1(&old_payload_bytes).await.unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.lifecycle.as_str(), "spike_active");
    let mut malformed_old_payload = old_payload.clone();
    malformed_old_payload["version"] = serde_json::json!("malformed");
    assert!(
        repo.submit_return_v1(&serde_json::to_vec(&malformed_old_payload).unwrap())
            .await
            .unwrap_err()
            .to_string()
            .contains("unsupported_version")
    );
    assert_eq!(
        failed_attempt_snapshot(&db, &finding, &old_attempt).await,
        failed_snapshot
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM typed_evidence_transitions WHERE finding_id=$1"
        )
        .bind(&finding)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        transition_count
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM typed_evidence_validation_results")
            .fetch_one(db.pool())
            .await
            .unwrap(),
        validation_count
    );

    let payload = serde_json::json!({"version":"TribunalEvidenceReturnV1","finding_id":finding,"spike_task_id":retry_task,"attempt_id":retry_attempt,"conclusion":"done","checks":[{"check_id":"retry-check","method":"code","status":"passed","anchors":[]} ]});
    assert_eq!(
        repo.submit_return_v1(&serde_json::to_vec(&payload).unwrap())
            .await
            .unwrap()
            .lifecycle
            .as_str(),
        "evidence_received"
    );
    assert_eq!(
        failed_attempt_snapshot(&db, &finding, &old_attempt).await,
        failed_snapshot
    );
}
