//! Migration 176 typed tribunal evidence schema constraints.

use djinn_core::{
    events::EventBus,
    models::{TribunalEvidenceLifecycle, TribunalEvidenceOutcome},
};
use djinn_db::{
    AdmitRefinementRunRequest, AppendTypedEvidenceTransitionInput, Database,
    DemandTypedEvidenceInput, DisposeTypedEvidenceInput, ProposalRepository,
    RefinementAdmissionOutcome, RefinementAdmissionSource, TypedEvidenceRepository,
};

async fn seed(db: &Database) -> (String, String, String, String) {
    let project_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, 'djinnos', $3)",
    )
    .bind(&project_id)
    .bind(format!("typed-evidence-{project_id}"))
    .bind(format!("typed-evidence-{project_id}"))
    .execute(db.pool())
    .await
    .unwrap();

    let creator_id = uuid::Uuid::now_v7().to_string();
    let github_id = (uuid::Uuid::now_v7().as_u128() & i64::MAX as u128) as i64;
    sqlx::query("INSERT INTO users (id, github_id, github_login) VALUES ($1, $2, $3)")
        .bind(&creator_id)
        .bind(github_id)
        .bind(format!("typed-evidence-{creator_id}"))
        .execute(db.pool())
        .await
        .unwrap();

    let task_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO tasks \
         (id, project_id, short_id, title, description, design, labels, acceptance_criteria, memory_refs, created_by_user_id, agent_type) \
         VALUES ($1, $2, $3, 'typed evidence', '', '', '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $4, 'judge')",
    )
    .bind(&task_id)
    .bind(&project_id)
    .bind(task_id.replace('-', ""))
    .bind(&creator_id)
    .execute(db.pool())
    .await
    .unwrap();

    let proposal_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO proposals (id, short_id, title, body, body_format, acceptance_criteria, status, latest_revision_seq) \
         VALUES ($1, $2, 'typed evidence', '', 'markdown', '[]'::jsonb, 'draft', 1)",
    )
    .bind(&proposal_id)
    .bind(proposal_id.replace('-', ""))
    .execute(db.pool())
    .await
    .unwrap();
    (proposal_id, task_id, project_id, creator_id)
}

async fn insert_committed_revision(db: &Database, proposal_id: &str, seq: i32) {
    sqlx::query(
        "INSERT INTO proposal_revisions \
         (id, proposal_id, seq, title, body, body_format, acceptance_criteria, event_kind) \
         VALUES ($1, $2, $3, 'typed evidence', '', 'markdown', '[]', 'spec_revision')",
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(proposal_id)
    .bind(seq)
    .execute(db.pool())
    .await
    .unwrap();
}

async fn insert_finding(
    db: &Database,
    id: &str,
    proposal_id: &str,
    task_id: &str,
    lifecycle: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO typed_evidence_findings \
         (id, proposal_id, demand_hash, lifecycle, claim, demanded_revision_seq, created_by_task_id) \
         VALUES ($1, $2, $3, $4, '{}'::jsonb, 1, $5)",
    )
    .bind(id)
    .bind(proposal_id)
    .bind(format!("hash-{id}"))
    .bind(lifecycle)
    .bind(task_id)
    .execute(db.pool())
    .await?;
    Ok(())
}

#[tokio::test]
async fn typed_evidence_schema_retains_legacy_columns_and_enforces_identities() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let (proposal_id, task_id, project_id, creator_id) = seed(&db).await;

    let legacy_columns: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.columns WHERE table_name = 'proposals' \
         AND column_name IN ('linked_spike_task_id', 'needs_evidence_claim')",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        legacy_columns, 2,
        "migration 82 compatibility columns remain"
    );

    let finding_id = uuid::Uuid::now_v7().to_string();
    insert_finding(&db, &finding_id, &proposal_id, &task_id, "demanded")
        .await
        .unwrap();
    assert!(
        insert_finding(
            &db,
            &uuid::Uuid::now_v7().to_string(),
            &proposal_id,
            &task_id,
            "spike_active",
        )
        .await
        .is_err(),
        "a proposal admits only one unresolved finding"
    );

    let attempt_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO typed_evidence_attempts (id, finding_id, sequence, spike_task_id) \
         VALUES ($1, $2, 1, $3)",
    )
    .bind(&attempt_id)
    .bind(&finding_id)
    .bind(&task_id)
    .execute(db.pool())
    .await
    .unwrap();

    let second_task = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO tasks \
         (id, project_id, short_id, title, description, design, labels, acceptance_criteria, memory_refs, created_by_user_id) \
         VALUES ($1, $2, $3, 'second', '', '', '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $4)",
    )
    .bind(&second_task)
    .bind(&project_id)
    .bind(second_task.replace('-', ""))
    .bind(&creator_id)
    .execute(db.pool())
    .await
    .unwrap();
    assert!(
        sqlx::query(
            "INSERT INTO typed_evidence_attempts (id, finding_id, sequence, spike_task_id) \
             VALUES ($1, $2, 1, $3)",
        )
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(&finding_id)
        .bind(&second_task)
        .execute(db.pool())
        .await
        .is_err(),
        "attempt sequences are ordered and unique per finding"
    );
    assert!(
        sqlx::query(
            "INSERT INTO typed_evidence_attempts (id, finding_id, sequence, spike_task_id) \
             VALUES ($1, $2, 2, $3)",
        )
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(&finding_id)
        .bind(&task_id)
        .execute(db.pool())
        .await
        .is_err(),
        "a spike task can bind to only one attempt"
    );

    let update_error =
        sqlx::query("UPDATE typed_evidence_attempts SET spike_task_id = $1 WHERE id = $2")
            .bind(&second_task)
            .bind(&attempt_id)
            .execute(db.pool())
            .await
            .unwrap_err();
    assert!(
        update_error
            .to_string()
            .contains("typed evidence attempts are append-only"),
        "attempt history rejects direct updates"
    );

    let delete_error = sqlx::query("DELETE FROM typed_evidence_attempts WHERE id = $1")
        .bind(&attempt_id)
        .execute(db.pool())
        .await
        .unwrap_err();
    assert!(
        delete_error
            .to_string()
            .contains("typed evidence attempts are append-only"),
        "attempt history rejects direct deletes"
    );
}

#[tokio::test]
async fn typed_evidence_lifecycle_v1() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/typed_evidence_lifecycle_v1.json")).unwrap();
    assert_eq!(fixture["version"], "typed_evidence_lifecycle_v1");
    assert_eq!(fixture["conflict_error"], "active_evidence_conflict");
    assert_eq!(
        fixture["terminal_controls"]["generic_transition_error"],
        "terminal transitions require dispose_in_transaction"
    );
    assert!(
        fixture["terminal_controls"]["withdrawn_requires"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("non_load_bearing_assertion"))
    );

    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let (proposal_id, judge_task_id, _, _) = seed(&db).await;
    // Terminal disposition authority is the exact active Judge correlation,
    // rather than the task that originated the demand. Materialize the running
    // run, Judge intent, and task correlation production authorization needs.
    let proposal_repo = ProposalRepository::new(db.clone(), EventBus::noop());
    proposal_repo
        .record_refinement_lifecycle(&proposal_id, "refinement_start", None)
        .await
        .unwrap();
    let (run_id, generation) = match proposal_repo
        .reap_and_admit(AdmitRefinementRunRequest {
            proposal_id: proposal_id.clone(),
            idempotency_key: format!("typed-evidence-lifecycle/{proposal_id}"),
            source: RefinementAdmissionSource::Demand {
                demand_id: format!("typed-evidence-lifecycle/{proposal_id}"),
            },
            heartbeat_grace_millis: 60_000,
        })
        .await
        .unwrap()
    {
        RefinementAdmissionOutcome::Admitted {
            run_id, generation, ..
        } => (run_id, generation),
        outcome => panic!("expected admitted refinement run, got {outcome:?}"),
    };
    djinn_db::test_support::materialize_judge_authority_for_test(
        &db,
        &judge_task_id,
        &run_id,
        i64::from(generation),
    )
    .await;
    let demand = |id: String, hash: &str, revision: i32| DemandTypedEvidenceInput {
        finding_id: id,
        proposal_id: proposal_id.clone(),
        demand_hash: hash.into(),
        claim: serde_json::json!({"uncertainty": "load-bearing"}),
        demanded_revision_seq: revision,
        judge_task_id: judge_task_id.clone(),
    };

    let finding_id = uuid::Uuid::now_v7().to_string();
    let mut tx = db.pool().begin().await.unwrap();
    let created = TypedEvidenceRepository::demand_in_transaction(
        &mut tx,
        demand(finding_id.clone(), "normalized-demand", 1),
    )
    .await
    .unwrap();
    assert_eq!(created.finding.id, finding_id);
    assert!(
        TypedEvidenceRepository::has_unresolved_in_transaction(&mut tx, &proposal_id)
            .await
            .unwrap()
    );
    // A replay from N+2 returns the original finding; a distinct N+1 demand
    // is blocked proposal-wide without adding history or another task binding.
    let replay = TypedEvidenceRepository::demand_in_transaction(
        &mut tx,
        demand(uuid::Uuid::now_v7().to_string(), "normalized-demand", 3),
    )
    .await
    .unwrap();
    assert_eq!(replay.finding.id, finding_id);
    assert!(matches!(
        TypedEvidenceRepository::demand_in_transaction(
            &mut tx,
            demand(uuid::Uuid::now_v7().to_string(), "different-demand", 2),
        )
        .await,
        Err(djinn_db::Error::InvalidTransition(message)) if message == "active_evidence_conflict"
    ));
    let transitions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM typed_evidence_transitions WHERE finding_id=$1")
            .bind(&finding_id)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(transitions, 1, "conflict writes no transition");

    // Persist an allowed non-terminal edge, then reject an unlisted edge.
    TypedEvidenceRepository::append_transition_in_transaction(
        &mut tx,
        AppendTypedEvidenceTransitionInput {
            id: uuid::Uuid::now_v7().to_string(),
            finding_id: finding_id.clone(),
            ordinal: 2,
            from_lifecycle: Some(TribunalEvidenceLifecycle::Demanded),
            to_lifecycle: TribunalEvidenceLifecycle::SpikeActive,
            actor_task_id: Some(judge_task_id.clone()),
            metadata: serde_json::json!({}),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        TypedEvidenceRepository::append_transition_in_transaction(
            &mut tx,
            AppendTypedEvidenceTransitionInput {
                id: uuid::Uuid::now_v7().to_string(),
                finding_id: finding_id.clone(),
                ordinal: 3,
                from_lifecycle: Some(TribunalEvidenceLifecycle::SpikeActive),
                to_lifecycle: TribunalEvidenceLifecycle::Demanded,
                actor_task_id: Some(judge_task_id.clone()),
                metadata: serde_json::json!({}),
            },
        )
        .await,
        Err(djinn_db::Error::InvalidTransition(message)) if message == "spike_active -> demanded"
    ));
    TypedEvidenceRepository::append_transition_in_transaction(
        &mut tx,
        AppendTypedEvidenceTransitionInput {
            id: uuid::Uuid::now_v7().to_string(),
            finding_id: finding_id.clone(),
            ordinal: 3,
            from_lifecycle: Some(TribunalEvidenceLifecycle::SpikeActive),
            to_lifecycle: TribunalEvidenceLifecycle::EvidenceReceived,
            actor_task_id: Some(judge_task_id.clone()),
            metadata: serde_json::json!({}),
        },
    )
    .await
    .unwrap();

    // Terminal edges can only be appended by the Judge disposition primitive.
    assert!(matches!(
        TypedEvidenceRepository::append_transition_in_transaction(
            &mut tx,
            AppendTypedEvidenceTransitionInput {
                id: uuid::Uuid::now_v7().to_string(),
                finding_id: finding_id.clone(),
                ordinal: 2,
                from_lifecycle: Some(TribunalEvidenceLifecycle::Demanded),
                to_lifecycle: TribunalEvidenceLifecycle::Withdrawn,
                actor_task_id: None,
                metadata: serde_json::json!({}),
            },
        )
        .await,
        Err(djinn_db::Error::InvalidTransition(message))
            if message == "terminal transitions require dispose_in_transaction"
    ));
    let dispose = |disposition,
                   judge_task_id: String,
                   folding_revision,
                   rationale: &str,
                   non_load_bearing| {
        DisposeTypedEvidenceInput {
            disposition_id: uuid::Uuid::now_v7().to_string(),
            transition_id: uuid::Uuid::now_v7().to_string(),
            finding_id: finding_id.clone(),
            validation_result_id: None,
            folding_revision,
            outcome: TribunalEvidenceOutcome::Resolved,
            disposition,
            judge_task_id,
            rationale: rationale.into(),
            withdrawal_is_non_load_bearing: non_load_bearing,
        }
    };
    // Invalid terminal inputs leave this evidence_received finding unresolved.
    assert!(matches!(
        TypedEvidenceRepository::dispose_in_transaction(
            &mut tx,
            dispose(TribunalEvidenceLifecycle::Resolved, "".into(), 1, "folded", true),
        )
        .await,
        Err(djinn_db::Error::InvalidData(message)) if message == "typed evidence identity fields must be non-empty"
    ));
    assert!(matches!(
        TypedEvidenceRepository::dispose_in_transaction(
            &mut tx,
            dispose(TribunalEvidenceLifecycle::Resolved, "not-the-judge".into(), 1, "folded", true),
        )
        .await,
        Err(djinn_db::Error::InvalidData(message)) if message == "active Judge attribution required"
    ));
    assert!(matches!(
        TypedEvidenceRepository::dispose_in_transaction(
            &mut tx,
            dispose(TribunalEvidenceLifecycle::Resolved, judge_task_id.clone(), 99, "folded", true),
        )
        .await,
        Err(djinn_db::Error::InvalidData(message)) if message == "existing committed folding revision required"
    ));
    assert!(matches!(
        TypedEvidenceRepository::dispose_in_transaction(
            &mut tx,
            dispose(TribunalEvidenceLifecycle::Withdrawn, judge_task_id.clone(), 1, "", true),
        )
        .await,
        Err(djinn_db::Error::InvalidData(message)) if message == "typed evidence identity fields must be non-empty"
    ));
    assert!(matches!(
        TypedEvidenceRepository::dispose_in_transaction(
            &mut tx,
            dispose(TribunalEvidenceLifecycle::Withdrawn, judge_task_id.clone(), 1, "not load-bearing", false),
        )
        .await,
        Err(djinn_db::Error::InvalidData(message)) if message == "withdrawal requires non-load-bearing assertion"
    ));
    tx.commit().await.unwrap();

    insert_committed_revision(&db, &proposal_id, 1).await;
    let mut tx = db.pool().begin().await.unwrap();
    let resolved = TypedEvidenceRepository::dispose_in_transaction(
        &mut tx,
        dispose(
            TribunalEvidenceLifecycle::Resolved,
            judge_task_id.clone(),
            1,
            "evidence folded",
            true,
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        resolved.finding_lifecycle,
        TribunalEvidenceLifecycle::Resolved
    );
    assert!(
        !TypedEvidenceRepository::has_unresolved_in_transaction(&mut tx, &proposal_id)
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM typed_evidence_dispositions WHERE finding_id=$1",
        )
        .bind(&finding_id)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        1,
        "resolved disposition is persisted",
    );

    // A terminal resolved finding permits a new demand. A valid withdrawn
    // disposition is also persisted and clears the proposal-wide projection.
    let withdrawn_finding_id = uuid::Uuid::now_v7().to_string();
    let mut tx = db.pool().begin().await.unwrap();
    TypedEvidenceRepository::demand_in_transaction(
        &mut tx,
        demand(withdrawn_finding_id.clone(), "withdrawn-demand", 2),
    )
    .await
    .unwrap();
    let withdrawn = TypedEvidenceRepository::dispose_in_transaction(
        &mut tx,
        DisposeTypedEvidenceInput {
            disposition_id: uuid::Uuid::now_v7().to_string(),
            transition_id: uuid::Uuid::now_v7().to_string(),
            finding_id: withdrawn_finding_id.clone(),
            validation_result_id: None,
            folding_revision: 1,
            outcome: TribunalEvidenceOutcome::Unresolved,
            disposition: TribunalEvidenceLifecycle::Withdrawn,
            judge_task_id: judge_task_id.clone(),
            rationale: "superseded by a non-load-bearing clarification".into(),
            withdrawal_is_non_load_bearing: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        withdrawn.finding_lifecycle,
        TribunalEvidenceLifecycle::Withdrawn
    );
    assert!(
        !TypedEvidenceRepository::has_unresolved_in_transaction(&mut tx, &proposal_id)
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();

    // Database append-only triggers protect lifecycle transition history.
    for statement in [
        "UPDATE typed_evidence_transitions SET metadata='{}'::jsonb WHERE finding_id=$1",
        "DELETE FROM typed_evidence_transitions WHERE finding_id=$1",
    ] {
        let error = sqlx::query(statement)
            .bind(&finding_id)
            .execute(db.pool())
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("typed evidence transitions are append-only"),
            "transition history rejects mutation: {statement}",
        );
    }
}
