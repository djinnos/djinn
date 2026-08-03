//! Independent-connection conformance fixtures for model-turn acquisition.

use std::sync::Arc;

use djinn_db::{
    Database, ModelTurnAcquireInput, ModelTurnAcquireOutcome, ModelTurnAdmissionRepository,
    ModelTurnAdmissionWait, ModelTurnBucketDebit, ModelTurnBucketKind,
};
use tokio::sync::Barrier;

async fn seed_pool(db: &Database, name: &str, target: i64, capability: &str) -> i64 {
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
    sqlx::query_scalar(
        "INSERT INTO model_turn_pools \
         (credential_id, provider_id, model_id, phase, capability_state, learned_concurrency) \
         VALUES ($1, 'provider', $2, 'enforce', $3, $4) RETURNING id",
    )
    .bind(format!("credential-{name}"))
    .bind(format!("model-{name}"))
    .bind(capability)
    .bind(target)
    .fetch_one(db.pool())
    .await
    .expect("seed enforced pool")
}

async fn seed_binding(db: &Database, pool_id: i64, kind: ModelTurnBucketKind) {
    let name = match kind {
        ModelTurnBucketKind::Request => "request",
        ModelTurnBucketKind::Input => "input",
        ModelTurnBucketKind::Output => "output",
        ModelTurnBucketKind::Combined => "combined",
    };
    sqlx::query(
        "INSERT INTO model_turn_bucket_bindings \
         (pool_id, bucket_kind, capacity_units, available_units) VALUES ($1, $2, 1, 1)",
    )
    .bind(pool_id)
    .bind(name)
    .execute(db.pool())
    .await
    .expect("seed binding");
}

fn input(pool_id: i64, request_id: &str, kind: ModelTurnBucketKind) -> ModelTurnAcquireInput {
    ModelTurnAcquireInput {
        pool_id,
        request_id: request_id.to_owned(),
        owner_pod_uid: Some(format!("pod-{request_id}")),
        generation: 1,
        debits: vec![ModelTurnBucketDebit {
            bucket_kind: kind,
            units: 1,
        }],
    }
}

async fn race(
    left: ModelTurnAdmissionRepository,
    right: ModelTurnAdmissionRepository,
    left_input: ModelTurnAcquireInput,
    right_input: ModelTurnAcquireInput,
) -> [ModelTurnAcquireOutcome; 2] {
    let barrier = Arc::new(Barrier::new(2));
    let left_barrier = barrier.clone();
    let left = tokio::spawn(async move {
        left_barrier.wait().await;
        left.acquire_turn(left_input).await.expect("left acquire")
    });
    let right = tokio::spawn(async move {
        barrier.wait().await;
        right
            .acquire_turn(right_input)
            .await
            .expect("right acquire")
    });
    [
        left.await.expect("left task"),
        right.await.expect("right task"),
    ]
}

#[tokio::test]
async fn target_one_race_across_independent_connections_admits_once() {
    let owner = Database::ephemeral().await.expect("owner database");
    let pool_id = seed_pool(&owner, "target-one", 1, "supported").await;
    seed_binding(&owner, pool_id, ModelTurnBucketKind::Request).await;
    let dsn = owner.test_dsn().expect("test database DSN");
    let left = ModelTurnAdmissionRepository::new(Database::reopen_test(&dsn).expect("left DB"));
    let right = ModelTurnAdmissionRepository::new(Database::reopen_test(&dsn).expect("right DB"));

    let outcomes = race(
        left,
        right,
        input(pool_id, "target-one-left", ModelTurnBucketKind::Request),
        input(pool_id, "target-one-right", ModelTurnBucketKind::Request),
    )
    .await;
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, ModelTurnAcquireOutcome::Admitted { .. }))
            .count(),
        1,
        "only one independent contender may occupy target 1"
    );
    assert!(outcomes.iter().any(|outcome| matches!(
        outcome,
        ModelTurnAcquireOutcome::Wait(ModelTurnAdmissionWait::Concurrency {
            target: 1,
            in_flight: 1
        })
    )));
    let (in_flight, available, reservations): (i64, i64, i64) = sqlx::query_as(
        "SELECT p.in_flight, b.available_units, \
         (SELECT count(*) FROM model_turn_reservations WHERE pool_id = p.id) \
         FROM model_turn_pools p JOIN model_turn_bucket_bindings b ON b.pool_id = p.id \
         WHERE p.id = $1",
    )
    .bind(pool_id)
    .fetch_one(owner.pool())
    .await
    .expect("read accounting");
    assert_eq!((in_flight, available, reservations), (1, 0, 1));
}

#[tokio::test]
async fn one_remaining_unit_races_leave_no_partial_debit_for_every_bucket_kind() {
    let owner = Database::ephemeral().await.expect("owner database");
    let dsn = owner.test_dsn().expect("test database DSN");
    for (index, kind) in [
        ModelTurnBucketKind::Request,
        ModelTurnBucketKind::Input,
        ModelTurnBucketKind::Output,
        ModelTurnBucketKind::Combined,
    ]
    .into_iter()
    .enumerate()
    {
        let pool_id = seed_pool(&owner, &format!("unit-{index}"), 2, "supported").await;
        seed_binding(&owner, pool_id, kind).await;
        let outcomes = race(
            ModelTurnAdmissionRepository::new(Database::reopen_test(&dsn).expect("left DB")),
            ModelTurnAdmissionRepository::new(Database::reopen_test(&dsn).expect("right DB")),
            input(pool_id, &format!("unit-{index}-left"), kind),
            input(pool_id, &format!("unit-{index}-right"), kind),
        )
        .await;
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, ModelTurnAcquireOutcome::Admitted { .. }))
                .count(),
            1,
            "only one {kind:?} unit may be reserved"
        );
        assert!(outcomes.iter().any(|outcome| matches!(
            outcome,
            ModelTurnAcquireOutcome::Wait(ModelTurnAdmissionWait::BucketUnavailable {
                bucket_kind,
                available_units: 0,
                required_units: 1,
                reset_at: None,
            }) if *bucket_kind == kind
        )));
        let (in_flight, available, reservations): (i64, i64, i64) = sqlx::query_as(
            "SELECT p.in_flight, b.available_units, \
             (SELECT count(*) FROM model_turn_reservations WHERE pool_id = p.id) \
             FROM model_turn_pools p JOIN model_turn_bucket_bindings b ON b.pool_id = p.id \
             WHERE p.id = $1",
        )
        .bind(pool_id)
        .fetch_one(owner.pool())
        .await
        .expect("read accounting");
        assert_eq!(
            (in_flight, available, reservations),
            (1, 0, 1),
            "the denied {kind:?} attempt must not leave a partial debit"
        );
    }
}

#[tokio::test]
async fn unknown_capability_elects_one_durable_discovery_owner() {
    let owner = Database::ephemeral().await.expect("owner database");
    let pool_id = seed_pool(&owner, "discovery", 1, "unknown").await;
    let dsn = owner.test_dsn().expect("test database DSN");
    let outcomes = race(
        ModelTurnAdmissionRepository::new(Database::reopen_test(&dsn).expect("left DB")),
        ModelTurnAdmissionRepository::new(Database::reopen_test(&dsn).expect("right DB")),
        input(pool_id, "discover-left", ModelTurnBucketKind::Request),
        input(pool_id, "discover-right", ModelTurnBucketKind::Request),
    )
    .await;
    let waits: Vec<_> = outcomes
        .iter()
        .map(|outcome| match outcome {
            ModelTurnAcquireOutcome::Wait(ModelTurnAdmissionWait::DiscoveryRequired {
                owner_request_id,
                is_owner,
            }) => (owner_request_id, is_owner),
            unexpected => panic!("expected discovery wait, got {unexpected:?}"),
        })
        .collect();
    assert_eq!(waits.iter().filter(|(_, is_owner)| **is_owner).count(), 1);
    assert_eq!(waits[0].0, waits[1].0, "all waiters observe one owner");
    let owner_request: String = sqlx::query_scalar(
        "SELECT owner_request_id FROM model_turn_capability_discoveries WHERE pool_id = $1",
    )
    .bind(pool_id)
    .fetch_one(owner.pool())
    .await
    .expect("read durable discovery owner");
    assert_eq!(owner_request, *waits[0].0);
}
