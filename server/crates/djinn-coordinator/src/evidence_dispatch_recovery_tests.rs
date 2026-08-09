use crate::evidence_dispatch_recovery::{
    EvidenceDispatchTestOutcome, evidence_dispatch_test_count, set_evidence_dispatch_test_script,
};
use djinn_core::events::DjinnEventEnvelope;
use djinn_db::{Database, TaskRepository};
use serde_json::Value;

#[derive(Debug)]
struct Fixture {
    db: Database,
    finding_id: String,
    proposal_id: String,
    task_id: String,
    attempt_id: String,
    demand_hash: String,
}

async fn fixture(is_retry: bool) -> Fixture {
    let db = crate::test_helpers::create_test_db();
    db.ensure_initialized().await.unwrap();
    let project_id = uuid::Uuid::now_v7().to_string();
    let user_id = uuid::Uuid::now_v7().to_string();
    let task_id = uuid::Uuid::now_v7().to_string();
    let proposal_id = uuid::Uuid::now_v7().to_string();
    let finding_id = uuid::Uuid::now_v7().to_string();
    let attempt_id = uuid::Uuid::now_v7().to_string();
    let demand_hash = format!("demand-{finding_id}");

    sqlx::query(
        "INSERT INTO projects (id,name,github_owner,github_repo) VALUES ($1,$2,'owner',$3)",
    )
    .bind(&project_id)
    .bind(format!("project-{project_id}"))
    .bind(format!("repo-{project_id}"))
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query("INSERT INTO users (id,github_id,github_login) VALUES ($1,1,$2)")
        .bind(&user_id)
        .bind(format!("user-{user_id}"))
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO tasks (id,project_id,short_id,title,description,design,issue_type,labels,acceptance_criteria,memory_refs,created_by_user_id) VALUES ($1,$2,$3,'evidence','','','spike','[\"refinement-evidence\",\"read-only\"]','[]','[]',$4)")
        .bind(&task_id)
        .bind(&project_id)
        .bind(task_id.replace('-', ""))
        .bind(&user_id)
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO proposals (id,short_id,title,body,body_format,acceptance_criteria,status,latest_revision_seq,linked_spike_task_id,needs_evidence_claim) VALUES ($1,$2,'proposal','','markdown','[]','draft',1,$3,'{}')")
        .bind(&proposal_id)
        .bind(proposal_id.replace('-', ""))
        .bind(&task_id)
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO typed_evidence_findings (id,proposal_id,demand_hash,lifecycle,claim,demanded_revision_seq,created_by_task_id) VALUES ($1,$2,$3,'demanded','{}',1,$4)")
        .bind(&finding_id)
        .bind(&proposal_id)
        .bind(&demand_hash)
        .bind(&task_id)
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO typed_evidence_attempts (id,finding_id,sequence,spike_task_id) VALUES ($1,$2,$3,$4)")
        .bind(&attempt_id)
        .bind(&finding_id)
        .bind(if is_retry { 2 } else { 1 })
        .bind(&task_id)
        .execute(db.pool())
        .await
        .unwrap();

    if is_retry {
        let failed_transition_id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO typed_evidence_transitions (id,finding_id,ordinal,from_lifecycle,to_lifecycle,metadata) VALUES ($1,$2,1,'spike_active','failed','{}')")
            .bind(&failed_transition_id)
            .bind(&finding_id)
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO typed_evidence_transitions (id,finding_id,ordinal,from_lifecycle,to_lifecycle,metadata) VALUES ($1,$2,2,'failed','demanded',$3)")
            .bind(uuid::Uuid::now_v7().to_string())
            .bind(&finding_id)
            .bind(serde_json::json!({"retry_attempt_id":attempt_id,"failed_transition_id":failed_transition_id}))
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO typed_evidence_retry_idempotency (finding_id,failed_transition_id,retry_attempt_id) VALUES ($1,$2,$3)")
            .bind(&finding_id)
            .bind(&failed_transition_id)
            .bind(&attempt_id)
            .execute(db.pool())
            .await
            .unwrap();
    } else {
        sqlx::query("INSERT INTO typed_evidence_transitions (id,finding_id,ordinal,from_lifecycle,to_lifecycle,metadata) VALUES ($1,$2,1,NULL,'demanded','{}')")
            .bind(uuid::Uuid::now_v7().to_string())
            .bind(&finding_id)
            .execute(db.pool())
            .await
            .unwrap();
    }

    Fixture {
        db,
        finding_id,
        proposal_id,
        task_id,
        attempt_id,
        demand_hash,
    }
}

async fn identity(f: &Fixture) -> Value {
    sqlx::query_scalar(
        "SELECT jsonb_build_object( \
         'finding_id',f.id,'proposal_id',f.proposal_id,'demand_hash',f.demand_hash, \
         'attempt_id',a.id,'sequence',a.sequence,'task_id',a.spike_task_id, \
         'legacy_link',p.linked_spike_task_id, \
         'finding_slot_count',(SELECT count(*) FROM typed_evidence_findings sf WHERE sf.proposal_id=f.proposal_id AND sf.lifecycle IN ('demanded','spike_active','evidence_received','failed')), \
         'attempt_count',(SELECT count(*) FROM typed_evidence_attempts sa WHERE sa.finding_id=f.id)) \
         FROM typed_evidence_findings f JOIN typed_evidence_attempts a ON a.finding_id=f.id JOIN proposals p ON p.id=f.proposal_id WHERE f.id=$1 AND a.id=$2",
    )
    .bind(&f.finding_id)
    .bind(&f.attempt_id)
    .fetch_one(f.db.pool())
    .await
    .unwrap()
}

async fn assert_state(f: &Fixture, stable: &Value, lifecycle: &str, errors: i64) {
    assert_eq!(&identity(f).await, stable, "allocation identity changed");
    assert_eq!(stable["finding_id"], f.finding_id);
    assert_eq!(stable["proposal_id"], f.proposal_id);
    assert_eq!(stable["task_id"], f.task_id);
    assert_eq!(stable["attempt_id"], f.attempt_id);
    assert_eq!(stable["demand_hash"], f.demand_hash);
    assert_eq!(stable["legacy_link"], f.task_id);
    assert_eq!(stable["finding_slot_count"], 1);
    assert_eq!(stable["attempt_count"], 1);
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT lifecycle FROM typed_evidence_findings WHERE id=$1"
        )
        .bind(&f.finding_id)
        .fetch_one(f.db.pool())
        .await
        .unwrap(),
        lifecycle
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM typed_evidence_retry_dispatch_errors WHERE finding_id=$1 AND attempt_id=$2 AND spike_task_id=$3")
            .bind(&f.finding_id)
            .bind(&f.attempt_id)
            .bind(&f.task_id)
            .fetch_one(f.db.pool())
            .await
            .unwrap(),
        errors
    );
}

async fn coordinator_fault_injection_case(is_retry: bool) {
    let f = fixture(is_retry).await;
    let (events_tx, _) = tokio::sync::broadcast::channel(32);
    let (mut actor, cancel) =
        crate::test_helpers::make_coordinator_actor_cancellable(&f.db, &events_tx);
    let task = TaskRepository::new(f.db.clone(), djinn_core::events::EventBus::noop())
        .get(&f.task_id)
        .await
        .unwrap()
        .unwrap();
    let live = DjinnEventEnvelope::task_created(&task, false);
    let stable = identity(&f).await;
    set_evidence_dispatch_test_script(
        &f.task_id,
        [
            EvidenceDispatchTestOutcome::EnqueueFailed,
            EvidenceDispatchTestOutcome::EnqueueFailed,
            EvidenceDispatchTestOutcome::Accepted,
            EvidenceDispatchTestOutcome::AlreadyActive,
        ],
        true,
    );

    actor.handle_event(live.clone()).await;
    assert_state(&f, &stable, "demanded", 1).await;

    // Duplicate live allocation delivery is another deterministic re-drive. It
    // appends an error but cannot allocate another slot, task, or attempt.
    actor.handle_event(live).await;
    assert_state(&f, &stable, "demanded", 2).await;

    // Startup-style recovery reaches the pool, then loses activation. Durable
    // state remains demanded and the earlier errors remain immutable.
    actor.redrive_demanded_evidence_dispatches().await;
    assert_state(&f, &stable, "demanded", 2).await;

    // The next startup pass observes exact-task prior acceptance and commits
    // activation for that same initial/retry attempt.
    actor.redrive_demanded_evidence_dispatches().await;
    assert_state(&f, &stable, "spike_active", 2).await;
    assert_eq!(evidence_dispatch_test_count(&f.task_id), 4);

    // Repeated startup is a no-op after durable activation.
    actor.redrive_demanded_evidence_dispatches().await;
    assert_state(&f, &stable, "spike_active", 2).await;
    assert_eq!(evidence_dispatch_test_count(&f.task_id), 4);

    // A terminal spike is excluded from inventory and never reopened.
    sqlx::query("UPDATE tasks SET status='closed' WHERE id=$1")
        .bind(&f.task_id)
        .execute(f.db.pool())
        .await
        .unwrap();
    actor.redrive_demanded_evidence_dispatches().await;
    assert_state(&f, &stable, "spike_active", 2).await;
    assert_eq!(evidence_dispatch_test_count(&f.task_id), 4);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM tasks WHERE id=$1")
            .bind(&f.task_id)
            .fetch_one(f.db.pool())
            .await
            .unwrap(),
        "closed"
    );

    cancel.cancel();
}

async fn terminal_allocation_is_never_dispatched(is_retry: bool) {
    let f = fixture(is_retry).await;
    let stable = identity(&f).await;
    sqlx::query("UPDATE tasks SET status='closed' WHERE id=$1")
        .bind(&f.task_id)
        .execute(f.db.pool())
        .await
        .unwrap();
    set_evidence_dispatch_test_script(&f.task_id, [EvidenceDispatchTestOutcome::Accepted], false);
    let (events_tx, _) = tokio::sync::broadcast::channel(32);
    let (mut actor, cancel) =
        crate::test_helpers::make_coordinator_actor_cancellable(&f.db, &events_tx);

    // Repeated startup inventory cannot consume the scripted acceptance or
    // mutate a demanded allocation once its exact spike task is terminal.
    actor.redrive_demanded_evidence_dispatches().await;
    assert_state(&f, &stable, "demanded", 0).await;
    actor.redrive_demanded_evidence_dispatches().await;
    assert_state(&f, &stable, "demanded", 0).await;
    assert_eq!(evidence_dispatch_test_count(&f.task_id), 0);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM tasks WHERE id=$1")
            .bind(&f.task_id)
            .fetch_one(f.db.pool())
            .await
            .unwrap(),
        "closed"
    );

    cancel.cancel();
}

#[tokio::test]
async fn initial_demand_redrive_recovers_exact_accepted_attempt() {
    coordinator_fault_injection_case(false).await;
    terminal_allocation_is_never_dispatched(false).await;
}

#[tokio::test]
async fn retry_redrive_recovers_exact_accepted_attempt() {
    coordinator_fault_injection_case(true).await;
    terminal_allocation_is_never_dispatched(true).await;
}
