//! Storage conformance for the persisted A→B→C→D compatibility-phase guard.
//!
//! Every assertion here is a persisted side effect: the `compatibility_phase`
//! column, the row count of `model_turn_pool_phase_transitions`, and the
//! `predicate_results` object actually stored on those rows. No test asserts a
//! returned status string on its own.

use djinn_db::{
    Database, ModelTurnAdmissionRepository, ModelTurnCapabilityHeartbeatInput,
    ModelTurnCompatibilityPhase, ModelTurnControllerFence, ModelTurnExpectedPathKey,
    ModelTurnPhaseCEvidenceInput, ModelTurnPhaseCEvidenceOutcome, ModelTurnPhaseCEvidenceStage,
    ModelTurnPhasePredicate, ModelTurnPhaseTransitionOutcome, ModelTurnPhaseTransitionRequest,
};

const INCARNATION: &str = "00000000-0000-7000-8000-0000000001ix";
const PROVIDER: &str = "provider";
const MODEL: &str = "model";
const FINGERPRINT: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

struct Fixture {
    db: Database,
    repository: ModelTurnAdmissionRepository,
    pool_id: i64,
    /// The instant every freshness bound is measured from, read from the
    /// database's own clock rather than the test process's.
    evaluated_at: String,
    expected_paths: Vec<ModelTurnExpectedPathKey>,
}

fn path(slot: &str, revision: &str) -> ModelTurnExpectedPathKey {
    ModelTurnExpectedPathKey {
        slot_pod_uid: slot.to_owned(),
        deployment_revision: revision.to_owned(),
    }
}

async fn db_now(db: &Database) -> String {
    sqlx::query_scalar(
        "SELECT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')",
    )
    .fetch_one(db.pool())
    .await
    .expect("read database clock")
}

/// The fence floor is exactly the freshness bound behind `evaluated_at`, so a
/// leadership lease that stops renewing for more than 60 seconds falls below it.
async fn db_now_minus_60(db: &Database) -> String {
    sqlx::query_scalar(
        "SELECT to_char(now() AT TIME ZONE 'utc' - interval '60 seconds', \
         'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')",
    )
    .fetch_one(db.pool())
    .await
    .expect("read database clock")
}

impl Fixture {
    /// A pool at phase `a` with every prerequisite for phase `b` satisfied.
    async fn healthy(name: &str) -> Self {
        let db = Database::ephemeral().await.expect("ephemeral database");
        db.ensure_initialized().await.expect("initialize database");
        sqlx::query(
            "INSERT INTO credentials (id, provider_id, key_name, encrypted_value) \
             VALUES ($1, 'provider', $2, decode('00', 'hex'))",
        )
        .bind(format!("credential-{name}"))
        .bind(format!("key-{name}"))
        .execute(db.pool())
        .await
        .expect("seed credential");
        let pool_id: i64 = sqlx::query_scalar(
            "INSERT INTO model_turn_pools \
             (credential_id, provider_id, model_id, phase, capability_state, learned_concurrency) \
             VALUES ($1, $2, $3, 'shadow', 'supported', 1) RETURNING id",
        )
        .bind(format!("credential-{name}"))
        .bind(PROVIDER)
        .bind(MODEL)
        .fetch_one(db.pool())
        .await
        .expect("seed pool");
        sqlx::query("INSERT INTO coordinator_incarnations (id) VALUES ($1)")
            .bind(INCARNATION)
            .execute(db.pool())
            .await
            .expect("seed incarnation");

        let repository = ModelTurnAdmissionRepository::new(db.clone());
        let expected_paths = vec![path("slot-1", "revision-1"), path("slot-2", "revision-1")];
        for expected in &expected_paths {
            repository
                .record_capability_heartbeat(ModelTurnCapabilityHeartbeatInput {
                    pool_id,
                    slot_pod_uid: expected.slot_pod_uid.clone(),
                    deployment_revision: expected.deployment_revision.clone(),
                    provider_id: PROVIDER.to_owned(),
                    model_id: MODEL.to_owned(),
                })
                .await
                .expect("record capability heartbeat");
        }
        record_complete_chain(&repository, pool_id, FINGERPRINT).await;

        let evaluated_at = db_now(&db).await;
        Self {
            db,
            repository,
            pool_id,
            evaluated_at,
            expected_paths,
        }
    }

    fn request(&self, requested: ModelTurnCompatibilityPhase) -> ModelTurnPhaseTransitionRequest {
        ModelTurnPhaseTransitionRequest {
            pool_id: self.pool_id,
            requested_phase: requested,
            controller_generation: 7,
            fence: ModelTurnControllerFence {
                incarnation_id: INCARNATION.to_owned(),
                live_since_at: "1970-01-01T00:00:00.000Z".to_owned(),
            },
            evaluated_at: self.evaluated_at.clone(),
            expected_paths: self.expected_paths.clone(),
        }
    }

    async fn stored_phase(&self) -> ModelTurnCompatibilityPhase {
        self.repository
            .compatibility_phase(self.pool_id)
            .await
            .expect("read compatibility phase")
            .expect("pool exists")
    }

    async fn ledger_rows(&self) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM model_turn_pool_phase_transitions WHERE pool_id = $1",
        )
        .bind(self.pool_id)
        .fetch_one(self.db.pool())
        .await
        .expect("count ledger rows")
    }

    /// The `effective_phase` column of the newest ledger row — the phase the
    /// row *claims* became effective, which must always be the phase the pool
    /// actually holds afterwards.
    async fn stored_row_effective_phase(&self) -> String {
        sqlx::query_scalar(
            "SELECT effective_phase FROM model_turn_pool_phase_transitions \
             WHERE pool_id = $1 ORDER BY id DESC LIMIT 1",
        )
        .bind(self.pool_id)
        .fetch_one(self.db.pool())
        .await
        .expect("read stored effective phase")
    }

    /// The predicate object exactly as Postgres stored it on the newest row.
    async fn stored_predicate_results(&self) -> serde_json::Value {
        let raw: String = sqlx::query_scalar(
            "SELECT predicate_results::text FROM model_turn_pool_phase_transitions \
             WHERE pool_id = $1 ORDER BY id DESC LIMIT 1",
        )
        .bind(self.pool_id)
        .fetch_one(self.db.pool())
        .await
        .expect("read stored predicate results");
        serde_json::from_str(&raw).expect("stored predicate results are JSON")
    }

    async fn age_heartbeats(&self, interval: &str) {
        sqlx::query(
            "UPDATE model_turn_capability_heartbeats \
             SET heartbeat_at = now() - $2::interval WHERE pool_id = $1",
        )
        .bind(self.pool_id)
        .bind(interval)
        .execute(self.db.pool())
        .await
        .expect("age heartbeats");
    }

    async fn age_evidence(&self, interval: &str) {
        sqlx::query(
            "UPDATE model_turn_phase_c_evidence \
             SET recorded_at = now() - $2::interval WHERE pool_id = $1",
        )
        .bind(self.pool_id)
        .bind(interval)
        .execute(self.db.pool())
        .await
        .expect("age evidence");
    }
}

async fn record_complete_chain(
    repository: &ModelTurnAdmissionRepository,
    pool_id: i64,
    fingerprint: &str,
) {
    for (stage, outcome) in [
        (
            ModelTurnPhaseCEvidenceStage::Decision,
            ModelTurnPhaseCEvidenceOutcome::Recorded,
        ),
        (
            ModelTurnPhaseCEvidenceStage::Dispatch,
            ModelTurnPhaseCEvidenceOutcome::Recorded,
        ),
        (
            ModelTurnPhaseCEvidenceStage::Heartbeat,
            ModelTurnPhaseCEvidenceOutcome::Recorded,
        ),
        (
            ModelTurnPhaseCEvidenceStage::ProviderOutcome,
            ModelTurnPhaseCEvidenceOutcome::Succeeded,
        ),
        (
            ModelTurnPhaseCEvidenceStage::Reconcile,
            ModelTurnPhaseCEvidenceOutcome::Recorded,
        ),
    ] {
        repository
            .record_phase_c_evidence(ModelTurnPhaseCEvidenceInput {
                pool_id,
                slot_pod_uid: "slot-1".to_owned(),
                deployment_revision: "revision-1".to_owned(),
                provider_id: PROVIDER.to_owned(),
                model_id: MODEL.to_owned(),
                attempt_fingerprint: fingerprint.to_owned(),
                stage,
                outcome,
            })
            .await
            .expect("record phase-c evidence");
    }
}

fn failed_keys(outcome: &ModelTurnPhaseTransitionOutcome) -> Vec<&'static str> {
    match outcome {
        ModelTurnPhaseTransitionOutcome::Denied { failed, .. } => {
            failed.iter().map(|predicate| predicate.key()).collect()
        }
        other => panic!("expected a denial, got {other:?}"),
    }
}

// ── AC 3: the accepted path ────────────────────────────────────────────────

#[tokio::test]
async fn every_predicate_holding_advances_exactly_one_step_and_is_idempotent() {
    let fixture = Fixture::healthy("accept").await;
    assert_eq!(fixture.stored_phase().await, ModelTurnCompatibilityPhase::A);

    let outcome = fixture
        .repository
        .request_phase_transition_in_transaction(fixture.request(ModelTurnCompatibilityPhase::B))
        .await
        .expect("guarded transition");
    assert!(
        matches!(
            outcome,
            ModelTurnPhaseTransitionOutcome::Advanced {
                effective_phase: ModelTurnCompatibilityPhase::B,
                ..
            }
        ),
        "a fully satisfied request must advance; got {outcome:?}"
    );

    // The side effects, not the returned variant.
    assert_eq!(fixture.stored_phase().await, ModelTurnCompatibilityPhase::B);
    assert_eq!(fixture.ledger_rows().await, 1);
    assert_eq!(fixture.stored_row_effective_phase().await, "b");
    let stored = fixture.stored_predicate_results().await;
    for predicate in ModelTurnPhasePredicate::ALL {
        assert_eq!(
            stored.get(predicate.key()),
            Some(&serde_json::Value::Bool(true)),
            "predicate `{}` must be persisted as held",
            predicate.key()
        );
    }

    // Re-issuing the accepted request is a no-op: no second row, no second step.
    let repeat = fixture
        .repository
        .request_phase_transition_in_transaction(fixture.request(ModelTurnCompatibilityPhase::B))
        .await
        .expect("repeat transition");
    assert_eq!(
        repeat,
        ModelTurnPhaseTransitionOutcome::AlreadyEffective {
            effective_phase: ModelTurnCompatibilityPhase::B
        }
    );
    assert_eq!(fixture.ledger_rows().await, 1, "a replay must not append");
    assert_eq!(fixture.stored_phase().await, ModelTurnCompatibilityPhase::B);
}

// ── AC 2: a phase may not skip its prerequisite ────────────────────────────

#[tokio::test]
async fn requesting_d_from_b_writes_no_row_and_leaves_the_phase_alone() {
    let fixture = Fixture::healthy("skip").await;
    fixture
        .repository
        .request_phase_transition_in_transaction(fixture.request(ModelTurnCompatibilityPhase::B))
        .await
        .expect("advance to b");
    assert_eq!(fixture.stored_phase().await, ModelTurnCompatibilityPhase::B);
    let rows_before = fixture.ledger_rows().await;

    let outcome = fixture
        .repository
        .request_phase_transition_in_transaction(fixture.request(ModelTurnCompatibilityPhase::D))
        .await
        .expect("skip request");
    assert_eq!(
        outcome,
        ModelTurnPhaseTransitionOutcome::NotAdjacent {
            effective_phase: ModelTurnCompatibilityPhase::B,
            requested_phase: ModelTurnCompatibilityPhase::D,
        }
    );
    assert_eq!(
        fixture.ledger_rows().await,
        rows_before,
        "a skipped prerequisite must not write a transition row"
    );
    assert_eq!(fixture.stored_phase().await, ModelTurnCompatibilityPhase::B);
}

// ── AC 1: one row per prerequisite ─────────────────────────────────────────

/// Every denial below asserts three persisted facts: the phase did not move,
/// exactly one ledger row was appended, and the stored `predicate_results`
/// names precisely the predicates that failed.
async fn assert_denied_naming(fixture: &Fixture, expected_failures: &[ModelTurnPhasePredicate]) {
    let outcome = fixture
        .repository
        .request_phase_transition_in_transaction(fixture.request(ModelTurnCompatibilityPhase::B))
        .await
        .expect("guarded transition");
    let expected_keys: Vec<&str> = expected_failures
        .iter()
        .map(|predicate| predicate.key())
        .collect();
    assert_eq!(
        failed_keys(&outcome),
        expected_keys,
        "the denial must name exactly the broken prerequisites"
    );
    assert_eq!(
        fixture.stored_phase().await,
        ModelTurnCompatibilityPhase::A,
        "a denied request must leave the effective phase untouched"
    );
    assert_eq!(fixture.ledger_rows().await, 1);
    assert_eq!(
        fixture.stored_row_effective_phase().await,
        "a",
        "a denial row must record the phase that is still in effect"
    );
    let stored = fixture.stored_predicate_results().await;
    for predicate in ModelTurnPhasePredicate::ALL {
        let expected = !expected_failures.contains(&predicate);
        assert_eq!(
            stored.get(predicate.key()),
            Some(&serde_json::Value::Bool(expected)),
            "stored predicate `{}` must be {expected}",
            predicate.key()
        );
    }
}

#[tokio::test]
async fn removing_the_schema_marker_denies_and_names_schema_marker() {
    let fixture = Fixture::healthy("schema").await;
    sqlx::query("DELETE FROM model_turn_admission_schema")
        .execute(fixture.db.pool())
        .await
        .expect("remove schema marker");
    assert_denied_naming(&fixture, &[ModelTurnPhasePredicate::SchemaMarker]).await;
}

#[tokio::test]
async fn a_report_that_no_longer_matches_the_b1_route_denies_and_names_capability_reports() {
    let fixture = Fixture::healthy("route").await;
    // The pool's B1 route moves; the already-persisted B2 reports keep the old
    // labels. Coverage is unaffected — it is keyed on slot and revision — so
    // this isolates the route-agreement prerequisite from the coverage one.
    fixture
        .repository
        .set_pool_labels_for_test(fixture.pool_id, PROVIDER, "model-relabelled")
        .await
        .expect("relabel pool");
    assert_denied_naming(&fixture, &[ModelTurnPhasePredicate::CapabilityReports]).await;
}

#[tokio::test]
async fn a_leadership_lease_aged_past_sixty_seconds_denies_and_names_leadership_generation() {
    let fixture = Fixture::healthy("fence").await;
    let floor = db_now_minus_60(&fixture.db).await;
    djinn_db::test_support::backdate_coordinator_incarnation_lease(
        &fixture.db,
        INCARNATION,
        "61 seconds",
    )
    .await;
    let mut request = fixture.request(ModelTurnCompatibilityPhase::B);
    request.fence.live_since_at = floor;
    let outcome = fixture
        .repository
        .request_phase_transition_in_transaction(request)
        .await
        .expect("guarded transition");
    assert_eq!(
        failed_keys(&outcome),
        vec!["leadership_generation"],
        "a lease that stopped renewing for more than 60s is not leadership"
    );
    assert_eq!(fixture.stored_phase().await, ModelTurnCompatibilityPhase::A);
    assert_eq!(fixture.ledger_rows().await, 1);
    assert_eq!(fixture.stored_row_effective_phase().await, "a");
    assert_eq!(
        fixture
            .stored_predicate_results()
            .await
            .get("leadership_generation"),
        Some(&serde_json::Value::Bool(false))
    );
}

#[tokio::test]
async fn evidence_aged_past_sixty_seconds_denies_and_names_observation_history() {
    let fixture = Fixture::healthy("history-age").await;
    fixture.age_evidence("61 seconds").await;
    assert_denied_naming(&fixture, &[ModelTurnPhasePredicate::ObservationHistory]).await;
}

#[tokio::test]
async fn an_incomplete_attempt_chain_denies_and_names_observation_history() {
    let fixture = Fixture::healthy("history-gap").await;
    sqlx::query(
        "DELETE FROM model_turn_phase_c_evidence WHERE pool_id = $1 AND stage = 'reconcile'",
    )
    .bind(fixture.pool_id)
    .execute(fixture.db.pool())
    .await
    .expect("drop the terminal edge");
    assert_denied_naming(&fixture, &[ModelTurnPhasePredicate::ObservationHistory]).await;
}

#[tokio::test]
async fn an_expected_path_without_coverage_denies_and_names_expected_path_coverage() {
    let mut fixture = Fixture::healthy("coverage").await;
    // A third live slot the coordinator expects but which never reported.
    fixture.expected_paths.push(path("slot-3", "revision-1"));
    assert_denied_naming(&fixture, &[ModelTurnPhasePredicate::ExpectedPathCoverage]).await;
}

#[tokio::test]
async fn heartbeats_aged_past_sixty_seconds_deny_the_coverage_prerequisite() {
    let fixture = Fixture::healthy("coverage-age").await;
    fixture.age_heartbeats("61 seconds").await;
    // With no fresh report at all, both report prerequisites fail — and the
    // stored object says so for each of them independently.
    assert_denied_naming(
        &fixture,
        &[
            ModelTurnPhasePredicate::CapabilityReports,
            ModelTurnPhasePredicate::ExpectedPathCoverage,
        ],
    )
    .await;
}

#[tokio::test]
async fn an_ineligible_identity_denies_and_names_identity_eligibility() {
    let fixture = Fixture::healthy("identity").await;
    sqlx::query("UPDATE model_turn_pools SET identity_state = 'colliding' WHERE id = $1")
        .bind(fixture.pool_id)
        .execute(fixture.db.pool())
        .await
        .expect("collide identity");
    assert_denied_naming(&fixture, &[ModelTurnPhasePredicate::IdentityEligibility]).await;
}

// ── AC 5: the closed shape and its allow-list ──────────────────────────────

/// The allow-list is a fixed constant. Pattern-checking the constant itself is
/// what makes the redaction claim provable: it holds for every row the writer
/// can ever produce, not merely for the rows one run happened to sample.
#[test]
fn the_predicate_allow_list_is_fixed_and_carries_no_identifier() {
    let keys: Vec<&str> = ModelTurnPhasePredicate::ALL
        .iter()
        .map(|predicate| predicate.key())
        .collect();
    assert_eq!(
        keys,
        vec![
            "schema_marker",
            "capability_reports",
            "leadership_generation",
            "observation_history",
            "expected_path_coverage",
            "identity_eligibility",
        ]
    );
    for key in &keys {
        for forbidden in [
            "credential",
            "account",
            "project",
            "user",
            "request",
            "lease",
            "_id",
            "uid",
            "token",
            "secret",
            "key",
        ] {
            assert!(
                !key.contains(forbidden),
                "allow-list key `{key}` must not carry `{forbidden}`"
            );
        }
    }
}

#[tokio::test]
async fn the_storage_boundary_rejects_an_unknown_predicate_key() {
    let fixture = Fixture::healthy("closed-shape").await;
    let complete = |extra: &str| {
        let mut object = serde_json::Map::new();
        for predicate in ModelTurnPhasePredicate::ALL {
            object.insert(predicate.key().to_owned(), serde_json::Value::Bool(true));
        }
        if !extra.is_empty() {
            object.insert(extra.to_owned(), serde_json::Value::Bool(true));
        }
        serde_json::Value::Object(object).to_string()
    };

    // The exact allow-list is accepted...
    fixture
        .repository
        .insert_raw_phase_transition_for_test(fixture.pool_id, "b", "b", &complete(""), 1)
        .await
        .expect("the exact allow-list is storable");

    // ...an extra key is not, even though it is a plain boolean.
    let extra = fixture
        .repository
        .insert_raw_phase_transition_for_test(
            fixture.pool_id,
            "b",
            "b",
            &complete("credential_id_seen"),
            1,
        )
        .await;
    assert!(
        extra.is_err(),
        "an unknown predicate key must be rejected at the storage boundary"
    );

    // ...and neither is a missing key.
    let missing = fixture
        .repository
        .insert_raw_phase_transition_for_test(
            fixture.pool_id,
            "b",
            "b",
            "{\"schema_marker\": true}",
            1,
        )
        .await;
    assert!(missing.is_err(), "a partial object must be rejected");

    // ...and neither is a non-boolean value, which is how a free-text
    // diagnostic would have smuggled an identifier in.
    let mut object = serde_json::Map::new();
    for predicate in ModelTurnPhasePredicate::ALL {
        object.insert(predicate.key().to_owned(), serde_json::Value::Bool(true));
    }
    object.insert(
        "identity_eligibility".to_owned(),
        serde_json::Value::String("credential-abc".to_owned()),
    );
    let typed = fixture
        .repository
        .insert_raw_phase_transition_for_test(
            fixture.pool_id,
            "b",
            "b",
            &serde_json::Value::Object(object).to_string(),
            1,
        )
        .await;
    assert!(
        typed.is_err(),
        "a non-boolean predicate value must be rejected"
    );

    assert_eq!(
        fixture.ledger_rows().await,
        1,
        "only the allow-list-shaped row may exist"
    );
}

#[tokio::test]
async fn every_persisted_predicate_object_holds_exactly_the_allow_list() {
    let fixture = Fixture::healthy("shape").await;
    fixture
        .repository
        .request_phase_transition_in_transaction(fixture.request(ModelTurnCompatibilityPhase::B))
        .await
        .expect("guarded transition");
    let stored = fixture.stored_predicate_results().await;
    let object = stored.as_object().expect("predicate results are an object");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut allowed: Vec<&str> = ModelTurnPhasePredicate::ALL
        .iter()
        .map(|predicate| predicate.key())
        .collect();
    allowed.sort_unstable();
    assert_eq!(
        keys, allowed,
        "the stored key set must equal the allow-list"
    );
    assert!(object.values().all(serde_json::Value::is_boolean));
}

// ── AC 4: the migration is additive ────────────────────────────────────────

#[tokio::test]
async fn a_reader_of_the_pre_migration_column_set_still_resolves_pools() {
    let fixture = Fixture::healthy("additive").await;
    // `resolve_pool` selects only the columns that existed before migration
    // 211. It must keep resolving the same pool after the new column lands.
    let resolved = fixture
        .repository
        .resolve_pool(&format!("credential-{}", "additive"), PROVIDER, MODEL)
        .await
        .expect("resolve pool")
        .expect("pool resolves through the pre-migration column set");
    assert_eq!(resolved.id, fixture.pool_id);
    assert_eq!(resolved.provider_id, PROVIDER);
    assert_eq!(resolved.model_id, MODEL);
    // And the new column defaults for every row that predates it.
    assert_eq!(fixture.stored_phase().await, ModelTurnCompatibilityPhase::A);
}
