//! Postgres conformance fixtures for the inert Phase A admission repository.
//!
//! These deliberately seed only durable rows and exercise admission through
//! independent repository handles. No provider or coordinator caller is needed
//! to establish the database fencing contract.

use std::sync::Arc;

use djinn_db::{
    Database, ModelTurnAcquireInput, ModelTurnAcquireOutcome, ModelTurnAdmissionRejection,
    ModelTurnAdmissionRepository, ModelTurnAdmissionWait, ModelTurnAuthoritativeUsage,
    ModelTurnBucketDebit, ModelTurnBucketKind, ModelTurnIdentityState, ModelTurnLeaseExpiryInput,
    ModelTurnLeaseLifecycle, ModelTurnLeaseMutationOutcome, ModelTurnLeaseReconciliationInput,
    ModelTurnLeaseTerminalOutcome,
};
use tokio::sync::Barrier;

const ALL_BUCKETS: [ModelTurnBucketKind; 4] = [
    ModelTurnBucketKind::Request,
    ModelTurnBucketKind::Input,
    ModelTurnBucketKind::Output,
    ModelTurnBucketKind::Combined,
];

fn bucket_name(kind: ModelTurnBucketKind) -> &'static str {
    match kind {
        ModelTurnBucketKind::Request => "request",
        ModelTurnBucketKind::Input => "input",
        ModelTurnBucketKind::Output => "output",
        ModelTurnBucketKind::Combined => "combined",
    }
}

fn acquire(pool_id: i64, request_id: &str, generation: i64, units: i64) -> ModelTurnAcquireInput {
    ModelTurnAcquireInput {
        pool_id,
        request_id: request_id.to_owned(),
        owner_pod_uid: Some(format!("pod-{request_id}")),
        generation,
        debits: ALL_BUCKETS
            .into_iter()
            .map(|bucket_kind| ModelTurnBucketDebit { bucket_kind, units })
            .collect(),
    }
}

fn usage(units: i64) -> ModelTurnAuthoritativeUsage {
    ModelTurnAuthoritativeUsage {
        request_units: units,
        input_units: units,
        output_units: units,
        combined_units: units,
    }
}

async fn seed_pool(db: &Database, name: &str, target: i64) -> i64 {
    db.ensure_initialized()
        .await
        .expect("initialize fixture database");
    let credential_id = format!("credential-{name}");
    sqlx::query(
        "INSERT INTO credentials (id, provider_id, key_name, encrypted_value) \
         VALUES ($1, 'fixture-provider', $2, decode('00', 'hex'))",
    )
    .bind(&credential_id)
    .bind(format!("fixture-key-{name}"))
    .execute(db.pool())
    .await
    .expect("seed credential row");
    let pool_id: i64 = sqlx::query_scalar(
        "INSERT INTO model_turn_pools \
         (credential_id, provider_id, model_id, phase, capability_state, learned_concurrency) \
         VALUES ($1, 'fixture-provider', $2, 'enforce', 'supported', $3) RETURNING id",
    )
    .bind(&credential_id)
    .bind(format!("fixture-model-{name}"))
    .bind(target)
    .fetch_one(db.pool())
    .await
    .expect("seed enforced pool");
    for kind in ALL_BUCKETS {
        sqlx::query(
            "INSERT INTO model_turn_bucket_bindings \
             (pool_id, bucket_kind, capacity_units, available_units) VALUES ($1, $2, 10, 10)",
        )
        .bind(pool_id)
        .bind(bucket_name(kind))
        .execute(db.pool())
        .await
        .expect("seed binding bucket");
    }
    pool_id
}

async fn race(
    left: ModelTurnAdmissionRepository,
    right: ModelTurnAdmissionRepository,
    left_input: ModelTurnAcquireInput,
    right_input: ModelTurnAcquireInput,
) -> [ModelTurnAcquireOutcome; 2] {
    let barrier = Arc::new(Barrier::new(2));
    let left_barrier = barrier.clone();
    let left_task = tokio::spawn(async move {
        left_barrier.wait().await;
        left.acquire_turn(left_input).await.expect("left acquire")
    });
    let right_task = tokio::spawn(async move {
        barrier.wait().await;
        right
            .acquire_turn(right_input)
            .await
            .expect("right acquire")
    });
    [
        left_task.await.expect("left task"),
        right_task.await.expect("right task"),
    ]
}

#[tokio::test]
async fn target_one_barrier_commits_every_binding_debit_or_none() {
    let owner = Database::ephemeral().await.expect("owner database");
    let pool_id = seed_pool(&owner, "target-one", 1).await;
    let dsn = owner.test_dsn().expect("fixture DSN");
    let outcomes = race(
        ModelTurnAdmissionRepository::new(Database::reopen_test(&dsn).expect("left connection")),
        ModelTurnAdmissionRepository::new(Database::reopen_test(&dsn).expect("right connection")),
        acquire(pool_id, "target-one-left", 1, 1),
        acquire(pool_id, "target-one-right", 1, 1),
    )
    .await;

    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, ModelTurnAcquireOutcome::Admitted { .. }))
            .count(),
        1,
        "a target-one pool admits exactly one simultaneous contender"
    );
    assert!(outcomes.iter().any(|outcome| matches!(
        outcome,
        ModelTurnAcquireOutcome::Wait(ModelTurnAdmissionWait::Concurrency {
            target: 1,
            in_flight: 1
        })
    )));
    let bindings: Vec<(String, i64)> = sqlx::query_as(
        "SELECT bucket_kind, available_units FROM model_turn_bucket_bindings \
         WHERE pool_id = $1 ORDER BY bucket_kind",
    )
    .bind(pool_id)
    .fetch_all(owner.pool())
    .await
    .expect("read all binding debits");
    assert_eq!(bindings.len(), ALL_BUCKETS.len());
    assert!(bindings.iter().all(|(_, available)| *available == 9));
    let rows: (i64, i64) = sqlx::query_as(
        "SELECT in_flight, (SELECT count(*) FROM model_turn_reservations WHERE pool_id = $1) \
         FROM model_turn_pools WHERE id = $1",
    )
    .bind(pool_id)
    .fetch_one(owner.pool())
    .await
    .expect("read pool accounting");
    assert_eq!(
        rows,
        (1, 1),
        "the losing transaction leaves no partial lease"
    );
}

#[tokio::test]
async fn target_two_expiry_heartbeat_and_concurrent_reconciliation_are_fenced() {
    let owner = Database::ephemeral().await.expect("owner database");
    let pool_id = seed_pool(&owner, "target-two", 2).await;
    let repository = ModelTurnAdmissionRepository::new(owner.clone());
    let lease_a = match repository
        .acquire_turn(acquire(pool_id, "request-a", 7, 2))
        .await
        .expect("admit A")
    {
        ModelTurnAcquireOutcome::Admitted { lease, .. } => lease,
        other => panic!("expected A admission, got {other:?}"),
    };
    let lease_b = match repository
        .acquire_turn(acquire(pool_id, "request-b", 42, 2))
        .await
        .expect("admit B")
    {
        ModelTurnAcquireOutcome::Admitted { lease, .. } => lease,
        other => panic!("expected B admission, got {other:?}"),
    };
    assert_eq!(
        repository
            .mark_dispatching(&lease_a.identity)
            .await
            .expect("dispatch A"),
        ModelTurnLeaseMutationOutcome::Applied
    );
    assert_eq!(
        repository
            .mark_dispatching(&lease_b.identity)
            .await
            .expect("dispatch B"),
        ModelTurnLeaseMutationOutcome::Applied
    );
    assert_eq!(
        repository
            .heartbeat(&lease_b.identity)
            .await
            .expect("heartbeat B"),
        ModelTurnLeaseMutationOutcome::Applied
    );

    let boundary: String = sqlx::query_scalar("SELECT (now() + interval '91 seconds')::text")
        .fetch_one(owner.pool())
        .await
        .expect("deterministic database boundary");
    assert_eq!(
        repository
            .expire_lease(ModelTurnLeaseExpiryInput {
                identity: lease_a.identity.clone(),
                observed_lifecycle: ModelTurnLeaseLifecycle::Dispatching,
                observed_heartbeat_at: None,
                boundary_at: boundary,
            })
            .await
            .expect("expire A"),
        ModelTurnLeaseMutationOutcome::Applied
    );
    assert_eq!(
        repository
            .heartbeat(&lease_a.identity)
            .await
            .expect("late A heartbeat"),
        ModelTurnLeaseMutationOutcome::Fenced
    );
    assert_eq!(
        repository
            .mark_active(&lease_a.identity)
            .await
            .expect("late A active"),
        ModelTurnLeaseMutationOutcome::Fenced
    );

    let b_state: (i64, String, Option<String>) = sqlx::query_as(
        "SELECT generation, lifecycle, heartbeat_at::text FROM model_turn_leases WHERE lease_id = $1::uuid",
    )
    .bind(&lease_b.identity.lease_id)
    .fetch_one(owner.pool())
    .await
    .expect("read healthy B");
    assert_eq!(b_state.0, 42, "generation is immutable per lease");
    assert_eq!(b_state.1, "dispatching");
    assert!(b_state.2.is_some(), "A expiry cannot erase B heartbeat");

    let dsn = owner.test_dsn().expect("fixture DSN");
    let left =
        ModelTurnAdmissionRepository::new(Database::reopen_test(&dsn).expect("left connection"));
    let right =
        ModelTurnAdmissionRepository::new(Database::reopen_test(&dsn).expect("right connection"));
    let input = ModelTurnLeaseReconciliationInput {
        identity: lease_b.identity.clone(),
        outcome: ModelTurnLeaseTerminalOutcome::Completed,
        authoritative_usage: Some(usage(1)),
        detail: Some("catalog:completed".to_owned()),
    };
    let barrier = Arc::new(Barrier::new(2));
    let first_barrier = barrier.clone();
    let first_input = input.clone();
    let first = tokio::spawn(async move {
        first_barrier.wait().await;
        left.reconcile_lease(first_input)
            .await
            .expect("first reconcile")
    });
    let second = tokio::spawn(async move {
        barrier.wait().await;
        right
            .reconcile_lease(input)
            .await
            .expect("second reconcile")
    });
    let concurrent = [
        first.await.expect("first task"),
        second.await.expect("second task"),
    ];
    assert_eq!(
        concurrent
            .iter()
            .filter(|outcome| **outcome == ModelTurnLeaseMutationOutcome::Applied)
            .count(),
        1
    );
    assert_eq!(
        concurrent
            .iter()
            .filter(|outcome| **outcome == ModelTurnLeaseMutationOutcome::Idempotent)
            .count(),
        1
    );
    let accounting: (i64, i64, i64) = sqlx::query_as(
        "SELECT p.in_flight, b.available_units, b.quarantined_units \
         FROM model_turn_pools p JOIN model_turn_bucket_bindings b ON b.pool_id = p.id \
         WHERE p.id = $1 AND b.bucket_kind = 'request'",
    )
    .bind(pool_id)
    .fetch_one(owner.pool())
    .await
    .expect("read exactly-once accounting");
    assert_eq!(
        accounting,
        (0, 7, 2),
        "expired A quarantines once; B is credited once"
    );
}

#[tokio::test]
async fn credential_rotation_replacement_and_identity_phases_preserve_the_contract() {
    let db = Database::ephemeral().await.expect("fixture database");
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    let pool_id = seed_pool(&db, "rotation", 1).await;
    let first = acquire(pool_id, "rotation-first", 1, 1);
    assert!(matches!(
        repository
            .acquire_turn(first)
            .await
            .expect("first acquisition"),
        ModelTurnAcquireOutcome::Admitted { .. }
    ));

    sqlx::query("UPDATE credentials SET encrypted_value = decode('deadbeef', 'hex') WHERE id = 'credential-rotation'")
        .execute(db.pool()).await.expect("rotate encrypted material in place");
    let preserved: (String, i64, i64) = sqlx::query_as(
        "SELECT credential_id, learned_concurrency, in_flight FROM model_turn_pools WHERE id = $1",
    )
    .bind(pool_id)
    .fetch_one(db.pool())
    .await
    .expect("read rotated pool");
    assert_eq!(preserved, ("credential-rotation".to_owned(), 1, 1));
    assert!(matches!(
        repository
            .acquire_turn(acquire(pool_id, "rotation-blocked", 2, 1))
            .await
            .expect("rotation admission"),
        ModelTurnAcquireOutcome::Wait(ModelTurnAdmissionWait::Concurrency { .. })
    ));

    let replacement_pool = seed_pool(&db, "replacement", 1).await;
    assert!(matches!(
        repository
            .acquire_turn(acquire(replacement_pool, "replacement-first", 1, 1))
            .await
            .expect("replacement admission"),
        ModelTurnAcquireOutcome::Admitted { .. }
    ));
    assert_ne!(
        pool_id, replacement_pool,
        "replacement credential owns an independent pool"
    );

    sqlx::query("UPDATE model_turn_pools SET phase = 'draining' WHERE id = $1")
        .bind(replacement_pool)
        .execute(db.pool())
        .await
        .expect("drain pool");
    assert!(matches!(
        repository
            .acquire_turn(acquire(replacement_pool, "draining", 2, 0))
            .await
            .expect("draining admission"),
        ModelTurnAcquireOutcome::Wait(ModelTurnAdmissionWait::Draining)
    ));
    sqlx::query(
        "UPDATE model_turn_pools SET phase = 'enforce', identity_state = 'ambiguous' WHERE id = $1",
    )
    .bind(replacement_pool)
    .execute(db.pool())
    .await
    .expect("mark ambiguous identity");
    assert!(matches!(
        repository
            .acquire_turn(acquire(replacement_pool, "ambiguous", 3, 0))
            .await
            .expect("ambiguous admission"),
        ModelTurnAcquireOutcome::Rejected(ModelTurnAdmissionRejection::IneligibleIdentity {
            state: ModelTurnIdentityState::Ambiguous
        })
    ));
    sqlx::query(
        "UPDATE model_turn_pools SET phase = 'enforce', identity_state = 'revoked' WHERE id = $1",
    )
    .bind(replacement_pool)
    .execute(db.pool())
    .await
    .expect("mark revoked identity");
    assert!(matches!(
        repository
            .acquire_turn(acquire(replacement_pool, "revoked", 4, 0))
            .await
            .expect("revoked admission"),
        ModelTurnAcquireOutcome::Rejected(ModelTurnAdmissionRejection::IneligibleIdentity {
            state: ModelTurnIdentityState::Revoked
        })
    ));
    sqlx::query(
        "UPDATE model_turn_pools SET phase = 'shadow', identity_state = 'colliding' WHERE id = $1",
    )
    .bind(replacement_pool)
    .execute(db.pool())
    .await
    .expect("mark colliding shadow identity");
    assert!(matches!(
        repository
            .acquire_turn(acquire(replacement_pool, "colliding", 4, 0))
            .await
            .expect("colliding admission"),
        ModelTurnAcquireOutcome::Rejected(ModelTurnAdmissionRejection::ShadowOnly)
    ));
}

#[tokio::test]
async fn readiness_is_inert_and_telemetry_is_bounded_and_opaque() {
    let db = Database::ephemeral().await.expect("fixture database");
    db.ensure_initialized()
        .await
        .expect("initialize fixture database");
    sqlx::query("INSERT INTO settings (key, value) VALUES ('resident-admission', 'unchanged')")
        .execute(db.pool())
        .await
        .expect("seed legacy setting");
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    assert_eq!(
        repository
            .schema_readiness()
            .await
            .expect("schema probe")
            .expect("marker")
            .model_turn_admission_schema,
        1
    );
    let inert: (i64, String) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM model_turn_pools), (SELECT value FROM settings WHERE key = 'resident-admission')",
    )
    .fetch_one(db.pool()).await.expect("read inert state");
    assert_eq!(
        inert,
        (0, "unchanged".to_owned()),
        "a readiness probe creates no admission state"
    );

    let pool_id = seed_pool(&db, "opaque", 1).await;
    let opaque = "catalog:rate_limit";
    sqlx::query("INSERT INTO model_turn_pool_capabilities (pool_id, capability_state, detail) VALUES ($1, 'supported', $2)")
        .bind(pool_id).bind(opaque).execute(db.pool()).await.expect("write bounded capability");
    for sequence in 0_i64..257 {
        sqlx::query("INSERT INTO model_turn_observations (pool_id, sequence, kind, detail) VALUES ($1, $2, 'rate_limit', $3)")
            .bind(pool_id).bind(sequence).bind(opaque).execute(db.pool()).await.expect("write opaque observation");
    }
    let telemetry: (i64, String, String) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM model_turn_observations WHERE pool_id = $1), \
         (SELECT detail FROM model_turn_pool_capabilities WHERE pool_id = $1), \
         (SELECT detail FROM model_turn_observations WHERE pool_id = $1 ORDER BY sequence DESC LIMIT 1)",
    )
    .bind(pool_id).fetch_one(db.pool()).await.expect("read telemetry");
    assert_eq!(telemetry, (256, opaque.to_owned(), opaque.to_owned()));
    for forbidden in [
        "credential-opaque",
        "fixture-key-opaque",
        "request-",
        "pod-",
    ] {
        assert!(!telemetry.1.contains(forbidden));
        assert!(!telemetry.2.contains(forbidden));
    }
    let oversized = "x".repeat(1025);
    assert!(sqlx::query("INSERT INTO model_turn_observations (pool_id, sequence, kind, detail) VALUES ($1, 1000, 'usage', $2)")
        .bind(pool_id).bind(oversized).execute(db.pool()).await.is_err(), "schema rejects unbounded diagnostics");
}
