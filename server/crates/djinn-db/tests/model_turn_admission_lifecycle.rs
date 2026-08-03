use djinn_db::{
    Database, ModelTurnAcquireInput, ModelTurnAcquireOutcome, ModelTurnAdmissionRepository,
    ModelTurnAuthoritativeUsage, ModelTurnBucketDebit, ModelTurnBucketKind,
    ModelTurnLeaseExpiryInput, ModelTurnLeaseLifecycle, ModelTurnLeaseMutationOutcome,
    ModelTurnLeaseReconciliationInput, ModelTurnLeaseTerminalOutcome,
};

async fn seed_pool(db: &Database) -> i64 {
    db.ensure_initialized().await.expect("initialize database");
    sqlx::query("INSERT INTO credentials (id, provider_id, key_name, encrypted_value) VALUES ('credential-lifecycle', 'provider', 'key-lifecycle', decode('00', 'hex'))")
        .execute(db.pool()).await.expect("seed credential");
    let pool_id: i64 = sqlx::query_scalar("INSERT INTO model_turn_pools (credential_id, provider_id, model_id, phase, capability_state, learned_concurrency) VALUES ('credential-lifecycle', 'provider', 'model-lifecycle', 'enforce', 'supported', 2) RETURNING id")
        .fetch_one(db.pool()).await.expect("seed pool");
    sqlx::query("INSERT INTO model_turn_bucket_bindings (pool_id, bucket_kind, capacity_units, available_units) VALUES ($1, 'request', 10, 10)")
        .bind(pool_id).execute(db.pool()).await.expect("seed binding");
    pool_id
}

fn acquire(pool_id: i64, request_id: &str, generation: i64) -> ModelTurnAcquireInput {
    ModelTurnAcquireInput {
        pool_id,
        request_id: request_id.to_owned(),
        owner_pod_uid: Some(format!("pod-{request_id}")),
        generation,
        debits: vec![ModelTurnBucketDebit {
            bucket_kind: ModelTurnBucketKind::Request,
            units: 2,
        }],
    }
}

fn usage(request_units: i64) -> ModelTurnAuthoritativeUsage {
    ModelTurnAuthoritativeUsage {
        request_units,
        input_units: 0,
        output_units: 0,
        combined_units: 0,
    }
}

#[tokio::test]
async fn target_two_expiry_fences_a_and_preserves_b_generation_and_accounting() {
    let db = Database::ephemeral().await.expect("database");
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    let pool_id = seed_pool(&db).await;
    let lease_a = match repository
        .acquire_turn(acquire(pool_id, "request-a", 7))
        .await
        .expect("acquire A")
    {
        ModelTurnAcquireOutcome::Admitted { lease, .. } => lease,
        other => panic!("expected A admission, got {other:?}"),
    };
    let lease_b = match repository
        .acquire_turn(acquire(pool_id, "request-b", 42))
        .await
        .expect("acquire B")
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

    // The caller supplies a deterministic observation boundary rather than this
    // repository sleeping; B's fresh heartbeat is not part of A's CAS.
    let boundary: String = sqlx::query_scalar("SELECT (now() + interval '91 seconds')::text")
        .fetch_one(db.pool())
        .await
        .expect("fake-time boundary");
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

    let b_before: (i64, String, Option<String>) = sqlx::query_as("SELECT generation, lifecycle, heartbeat_at::text FROM model_turn_leases WHERE lease_id = $1::uuid")
        .bind(&lease_b.identity.lease_id).fetch_one(db.pool()).await.expect("read B");
    assert_eq!(b_before.0, 42);
    assert_eq!(b_before.1, "dispatching");
    assert!(b_before.2.is_some(), "B heartbeat must remain intact");
    assert_eq!(
        repository
            .mark_active(&lease_a.identity)
            .await
            .expect("late active A"),
        ModelTurnLeaseMutationOutcome::Fenced
    );
    assert_eq!(
        repository
            .heartbeat(&lease_a.identity)
            .await
            .expect("late heartbeat A"),
        ModelTurnLeaseMutationOutcome::Fenced
    );
    assert_eq!(
        repository
            .reconcile_lease(ModelTurnLeaseReconciliationInput {
                identity: lease_a.identity.clone(),
                outcome: ModelTurnLeaseTerminalOutcome::Completed,
                authoritative_usage: None,
                detail: None
            })
            .await
            .expect("late reconciliation A"),
        ModelTurnLeaseMutationOutcome::Fenced
    );

    assert_eq!(
        repository
            .reconcile_lease(ModelTurnLeaseReconciliationInput {
                identity: lease_b.identity.clone(),
                outcome: ModelTurnLeaseTerminalOutcome::Completed,
                authoritative_usage: Some(usage(2)),
                detail: None
            })
            .await
            .expect("reconcile B"),
        ModelTurnLeaseMutationOutcome::Applied
    );
    // Expiry quarantined A's possible spend. The matching terminal outcome can
    // later resolve that quarantine once, without changing the outcome.
    let delayed = ModelTurnLeaseReconciliationInput {
        identity: lease_a.identity.clone(),
        outcome: ModelTurnLeaseTerminalOutcome::Expired,
        authoritative_usage: Some(usage(1)),
        detail: None,
    };
    assert_eq!(
        repository
            .reconcile_lease(delayed.clone())
            .await
            .expect("resolve A usage"),
        ModelTurnLeaseMutationOutcome::Applied
    );
    assert_eq!(
        repository
            .reconcile_lease(delayed)
            .await
            .expect("replay A usage"),
        ModelTurnLeaseMutationOutcome::Idempotent
    );
    let accounting: (i64, i64, i64) = sqlx::query_as("SELECT p.in_flight, b.available_units, b.quarantined_units FROM model_turn_pools p JOIN model_turn_bucket_bindings b ON b.pool_id = p.id WHERE p.id = $1")
        .bind(pool_id).fetch_one(db.pool()).await.expect("read accounting");
    assert_eq!(
        accounting,
        (0, 7, 0),
        "delayed authority credits exactly once"
    );
    let terminal: (String, String) = sqlx::query_as("SELECT outcome, accounting_state FROM model_turn_lease_terminals WHERE lease_id = $1::uuid")
        .bind(&lease_a.identity.lease_id).fetch_one(db.pool()).await.expect("read A terminal");
    assert_eq!(terminal, ("expired".to_owned(), "authoritative".to_owned()));
}

#[tokio::test]
async fn delayed_authority_resolves_reconciled_quarantine_once_and_unsent_refunds() {
    let db = Database::ephemeral().await.expect("database");
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    let pool_id = seed_pool(&db).await;
    let sent = match repository
        .acquire_turn(acquire(pool_id, "request-delayed", 1))
        .await
        .expect("acquire sent lease")
    {
        ModelTurnAcquireOutcome::Admitted { lease, .. } => lease,
        other => panic!("expected admission, got {other:?}"),
    };
    assert_eq!(
        repository
            .mark_dispatching(&sent.identity)
            .await
            .expect("dispatch sent lease"),
        ModelTurnLeaseMutationOutcome::Applied
    );
    assert_eq!(
        repository
            .reconcile_lease(ModelTurnLeaseReconciliationInput {
                identity: sent.identity.clone(),
                outcome: ModelTurnLeaseTerminalOutcome::Failed,
                authoritative_usage: None,
                detail: None,
            })
            .await
            .expect("quarantine sent lease"),
        ModelTurnLeaseMutationOutcome::Applied
    );
    let delayed = ModelTurnLeaseReconciliationInput {
        identity: sent.identity.clone(),
        outcome: ModelTurnLeaseTerminalOutcome::Failed,
        authoritative_usage: Some(usage(1)),
        detail: None,
    };
    assert_eq!(
        repository
            .reconcile_lease(delayed.clone())
            .await
            .expect("resolve delayed usage"),
        ModelTurnLeaseMutationOutcome::Applied
    );
    assert_eq!(
        repository
            .reconcile_lease(delayed)
            .await
            .expect("replay delayed usage"),
        ModelTurnLeaseMutationOutcome::Idempotent
    );
    let after_sent: (i64, i64) = sqlx::query_as("SELECT available_units, quarantined_units FROM model_turn_bucket_bindings WHERE pool_id = $1 AND bucket_kind = 'request'")
        .bind(pool_id).fetch_one(db.pool()).await.expect("read sent accounting");
    assert_eq!(after_sent, (9, 0));

    let unsent = match repository
        .acquire_turn(acquire(pool_id, "request-unsent", 2))
        .await
        .expect("acquire unsent lease")
    {
        ModelTurnAcquireOutcome::Admitted { lease, .. } => lease,
        other => panic!("expected admission, got {other:?}"),
    };
    assert_eq!(
        repository
            .reconcile_lease(ModelTurnLeaseReconciliationInput {
                identity: unsent.identity,
                outcome: ModelTurnLeaseTerminalOutcome::Cancelled,
                authoritative_usage: None,
                detail: None,
            })
            .await
            .expect("refund unsent lease"),
        ModelTurnLeaseMutationOutcome::Applied
    );
    let after_unsent: (i64, i64, i64) = sqlx::query_as("SELECT p.in_flight, b.available_units, b.quarantined_units FROM model_turn_pools p JOIN model_turn_bucket_bindings b ON b.pool_id = p.id WHERE p.id = $1")
        .bind(pool_id).fetch_one(db.pool()).await.expect("read unsent accounting");
    assert_eq!(after_unsent, (0, 9, 0));
}
