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

mod scenario_ledger;

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

/// Every terminal requirement token this test knows how to probe. The fixture
/// selects from this closed set; a token it names that is absent here panics,
/// and a token present here that the fixture omits must be provably *not*
/// enforced for that disposition.
const TERMINAL_REQUIREMENTS: [&str; 4] = [
    "nonempty_rationale",
    "judge_attribution",
    "committed_folding_revision",
    "non_load_bearing_assertion",
];

#[tokio::test]
async fn typed_evidence_lifecycle_v1() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/typed_evidence_lifecycle_v1.json")).unwrap();
    assert_eq!(fixture["version"], "typed_evidence_lifecycle_v1");
    let conflict_error = fixture["conflict_error"]
        .as_str()
        .expect("fixture pins the proposal-wide conflict error")
        .to_owned();
    let generic_transition_error = fixture["terminal_controls"]["generic_transition_error"]
        .as_str()
        .expect("fixture pins the generic terminal-transition refusal")
        .to_owned();
    // Ledger of the fixture-declared scenarios this body actually proves. The
    // two sets are compared at the end, so a renamed, added or removed scenario
    // in the fixture reddens this test.
    let declared_scenarios: Vec<String> = fixture["scenarios"]
        .as_array()
        .expect("fixture declares the scenario set")
        .iter()
        .map(|value| {
            value
                .as_str()
                .expect("each scenario is a string")
                .to_owned()
        })
        .collect();
    let mut proven = scenario_ledger::ProvenScenarios::new();

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
    let unresolved_at_n =
        TypedEvidenceRepository::has_unresolved_in_transaction(&mut tx, &proposal_id)
            .await
            .unwrap();
    // A replay from N+2 returns the original finding; a distinct N+1 demand
    // is blocked proposal-wide without adding history or another task binding.
    let replay = TypedEvidenceRepository::demand_in_transaction(
        &mut tx,
        demand(uuid::Uuid::now_v7().to_string(), "normalized-demand", 3),
    )
    .await
    .unwrap();
    proven.observes(
        "identical_demand_idempotent",
        replay.finding.id.clone(),
        finding_id.clone(),
        "an identical demand hash raised from a later revision returns the original finding",
    );
    let distinct = TypedEvidenceRepository::demand_in_transaction(
        &mut tx,
        demand(uuid::Uuid::now_v7().to_string(), "different-demand", 2),
    )
    .await;
    let distinct_refused = matches!(
        &distinct,
        Err(djinn_db::Error::InvalidTransition(message)) if *message == conflict_error
    );
    proven.observes(
        "cross_revision_n_n1_n2_blocking",
        (unresolved_at_n, distinct_refused),
        (true, true),
        "one unresolved finding blocks the proposal at N, admits the N+2 replay, and \
         refuses the distinct N+1 demand",
    );
    let transitions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM typed_evidence_transitions WHERE finding_id=$1")
            .bind(&finding_id)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    proven.observes(
        "distinct_demand_no_write_conflict",
        transitions,
        1,
        "the refused distinct demand writes no transition",
    );

    // Persist an allowed non-terminal edge, then reject an unlisted edge.
    proven.accepts(
        "persisted_nonterminal_transition",
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
        .await,
        "an edge the allowed set lists is persisted",
    );
    let unlisted = TypedEvidenceRepository::append_transition_in_transaction(
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
    .await;
    proven.observes(
        "unlisted_transition_rejected",
        matches!(
            &unlisted,
            Err(djinn_db::Error::InvalidTransition(message)) if message == "spike_active -> demanded"
        ),
        true,
        "an edge the allowed set omits is refused by name",
    );
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
    let generic_terminal = TypedEvidenceRepository::append_transition_in_transaction(
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
    .await;
    proven.observes(
        "generic_terminal_transition_rejected",
        matches!(
            &generic_terminal,
            Err(djinn_db::Error::InvalidTransition(message))
                if *message == generic_transition_error
        ),
        true,
        "the generic append primitive refuses every terminal edge",
    );
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
    // Identity is required of every disposition field, not only the two the
    // requirement tokens below name.
    assert!(matches!(
        TypedEvidenceRepository::dispose_in_transaction(
            &mut tx,
            dispose(TribunalEvidenceLifecycle::Resolved, "".into(), 1, "folded", true),
        )
        .await,
        Err(djinn_db::Error::InvalidData(message)) if message == "typed evidence identity fields must be non-empty"
    ));
    // `terminal_controls.{resolved,withdrawn}_requires` is the whole refusal
    // set for its disposition, in both directions: a token the fixture lists
    // must yield exactly its refusal, and a token it omits must not.
    for (disposition, key) in [
        (TribunalEvidenceLifecycle::Resolved, "resolved_requires"),
        (TribunalEvidenceLifecycle::Withdrawn, "withdrawn_requires"),
    ] {
        let required: Vec<String> = fixture["terminal_controls"][key]
            .as_array()
            .unwrap_or_else(|| panic!("fixture key `terminal_controls.{key}` must be an array"))
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .unwrap_or_else(|| panic!("`terminal_controls.{key}` holds only strings"))
                    .to_owned()
            })
            .collect();
        for token in &required {
            assert!(
                TERMINAL_REQUIREMENTS.contains(&token.as_str()),
                "`terminal_controls.{key}` names `{token}`, which no probe in this test enforces",
            );
        }
        let mut refused: Vec<String> = Vec::new();
        for token in TERMINAL_REQUIREMENTS {
            let (input, refusal) = match token {
                "nonempty_rationale" => (
                    dispose(disposition, judge_task_id.clone(), 1, "", true),
                    "typed evidence identity fields must be non-empty",
                ),
                "judge_attribution" => (
                    dispose(disposition, "not-the-judge".into(), 1, "folded", true),
                    "active Judge attribution required",
                ),
                "committed_folding_revision" => (
                    dispose(disposition, judge_task_id.clone(), 99, "folded", true),
                    "existing committed folding revision required",
                ),
                "non_load_bearing_assertion" => (
                    dispose(disposition, judge_task_id.clone(), 1, "folded", false),
                    "withdrawal requires non-load-bearing assertion",
                ),
                other => panic!("unknown terminal requirement probe `{other}`"),
            };
            let error = TypedEvidenceRepository::dispose_in_transaction(&mut tx, input)
                .await
                .expect_err("an invalid terminal disposition never persists");
            if matches!(
                &error,
                djinn_db::Error::InvalidData(message) if message == refusal
            ) {
                refused.push((*token).to_owned());
            }
        }
        refused.sort();
        let mut want = required.clone();
        want.sort();
        // The scenario is recorded BY this comparison, so deleting it deletes
        // the ledger entry too.
        proven.observes(
            match key {
                "resolved_requires" => "resolved_requires_judge_and_committed_folding_revision",
                _ => "withdrawn_requires_rationale_and_non_load_bearing_assertion",
            },
            refused,
            want,
            "`terminal_controls` lists exactly the tokens this disposition is refused for",
        );
    }
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
    let unresolved_after_resolved =
        TypedEvidenceRepository::has_unresolved_in_transaction(&mut tx, &proposal_id)
            .await
            .unwrap();
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
    let unresolved_after_withdrawn =
        TypedEvidenceRepository::has_unresolved_in_transaction(&mut tx, &proposal_id)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    proven.observes(
        "resolved_and_withdrawn_clear_unresolved_projection",
        (unresolved_after_resolved, unresolved_after_withdrawn),
        (false, false),
        "both terminal dispositions clear the proposal-wide unresolved projection",
    );

    // Database append-only triggers protect lifecycle transition history.
    let mutations = [
        "UPDATE typed_evidence_transitions SET metadata='{}'::jsonb WHERE finding_id=$1",
        "DELETE FROM typed_evidence_transitions WHERE finding_id=$1",
    ];
    let mut rejected: Vec<&str> = Vec::new();
    for statement in mutations {
        let error = sqlx::query(statement)
            .bind(&finding_id)
            .execute(db.pool())
            .await
            .unwrap_err();
        if error
            .to_string()
            .contains("typed evidence transitions are append-only")
        {
            rejected.push(statement);
        }
    }
    proven.observes(
        "append_only_history",
        rejected,
        mutations.to_vec(),
        "the append-only trigger refuses both an update and a delete of transition history",
    );

    scenario_ledger::assert_ledger_reconciles(proven.into_sorted(), &declared_scenarios);
}
