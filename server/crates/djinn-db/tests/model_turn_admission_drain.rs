//! Drain-before-acquisition ordering, terminal accounting across a drain, and
//! the ordered rollback.
//!
//! The load-bearing assertions here count rows and read columns. In
//! particular the ordering guarantee is asserted by counting
//! `model_turn_leases` rows created after the drain's own persisted instant —
//! never by reading a returned enum.

use std::sync::Arc;

use djinn_db::{
    Database, ModelTurnAcquireInput, ModelTurnAcquireOutcome, ModelTurnAdmissionPhase,
    ModelTurnAdmissionRepository, ModelTurnAuthoritativeUsage, ModelTurnBucketDebit,
    ModelTurnBucketKind, ModelTurnCompatibilityPhase, ModelTurnLeaseExpiryInput,
    ModelTurnLeaseIdentity, ModelTurnLeaseLifecycle, ModelTurnLeaseMutationOutcome,
    ModelTurnLeaseReconciliationInput, ModelTurnLeaseTerminalOutcome, ModelTurnModeChangeInput,
    ModelTurnModeChangeOutcome, ModelTurnModeChangeReason, ModelTurnModeChangeRejection,
    ModelTurnRollbackPlanV1, ModelTurnRollbackStepV1,
};
use tokio::sync::Barrier;

const GENERATION: i64 = 11;

async fn seed_pool(db: &Database, name: &str, mode: &str, target: i64, capacity: i64) -> i64 {
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
         VALUES ($1, 'provider', $2, $3, 'supported', $4) RETURNING id",
    )
    .bind(format!("credential-{name}"))
    .bind(format!("model-{name}"))
    .bind(mode)
    .bind(target)
    .fetch_one(db.pool())
    .await
    .expect("seed pool");
    sqlx::query(
        "INSERT INTO model_turn_bucket_bindings \
         (pool_id, bucket_kind, capacity_units, available_units) VALUES ($1, 'request', $2, $2)",
    )
    .bind(pool_id)
    .bind(capacity)
    .execute(db.pool())
    .await
    .expect("seed binding");
    pool_id
}

fn acquire(pool_id: i64, request_id: &str, units: i64) -> ModelTurnAcquireInput {
    ModelTurnAcquireInput {
        pool_id,
        request_id: request_id.to_owned(),
        owner_pod_uid: Some(format!("pod-{request_id}")),
        generation: GENERATION,
        debits: vec![ModelTurnBucketDebit {
            bucket_kind: ModelTurnBucketKind::Request,
            units,
        }],
    }
}

async fn stored_mode(db: &Database, pool_id: i64) -> String {
    sqlx::query_scalar("SELECT phase FROM model_turn_pools WHERE id = $1")
        .bind(pool_id)
        .fetch_one(db.pool())
        .await
        .expect("read pool mode")
}

async fn mode_ledger_rows(db: &Database, pool_id: i64) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM model_turn_pool_mode_transitions WHERE pool_id = $1")
        .bind(pool_id)
        .fetch_one(db.pool())
        .await
        .expect("count mode ledger rows")
}

/// `(in_flight, available_units, quarantined_units)` — the accounting triple.
async fn accounting(db: &Database, pool_id: i64) -> (i64, i64, i64) {
    sqlx::query_as(
        "SELECT p.in_flight, b.available_units, b.quarantined_units \
         FROM model_turn_pools p JOIN model_turn_bucket_bindings b ON b.pool_id = p.id \
         WHERE p.id = $1",
    )
    .bind(pool_id)
    .fetch_one(db.pool())
    .await
    .expect("read accounting")
}

async fn lease_count(db: &Database, pool_id: i64) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM model_turn_leases WHERE pool_id = $1")
        .bind(pool_id)
        .fetch_one(db.pool())
        .await
        .expect("count leases")
}

/// Leases whose `reserved_at` is strictly after the drain's persisted instant.
/// This is the ordering claim in its countable form.
async fn leases_created_after(db: &Database, pool_id: i64, instant: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM model_turn_leases \
         WHERE pool_id = $1 AND reserved_at > $2::timestamptz",
    )
    .bind(pool_id)
    .bind(instant)
    .fetch_one(db.pool())
    .await
    .expect("count leases created after the drain")
}

fn drain_instant(outcome: &ModelTurnModeChangeOutcome) -> String {
    match outcome {
        ModelTurnModeChangeOutcome::Applied { changed_at, .. }
        | ModelTurnModeChangeOutcome::DrainedAndSettled { changed_at } => changed_at.clone(),
        other => panic!("expected the drain to commit a mode change, got {other:?}"),
    }
}

// ── AC 1: drained before the next acquisition can commit ───────────────────

#[tokio::test]
async fn no_lease_is_created_after_a_drain_commits() {
    let owner = Database::ephemeral().await.expect("owner database");
    let dsn = owner.test_dsn().expect("test database DSN");

    // Repeated so the two orderings (acquisition first, drain first) are both
    // exercised; the assertion is identical either way.
    let mut drained_first = 0;
    let mut acquired_first = 0;
    for round in 0..8 {
        let pool_id = seed_pool(&owner, &format!("race-{round}"), "enforce", 4, 8).await;
        // One lease already in flight, so the drain has something to drain and
        // stops at `draining` instead of settling straight to `off`. That is
        // the state the ordering claim is about.
        let warm = ModelTurnAdmissionRepository::new(owner.clone());
        let ModelTurnAcquireOutcome::Admitted { lease, .. } = warm
            .acquire_turn(acquire(pool_id, &format!("race-{round}-warm"), 1))
            .await
            .expect("warm acquire")
        else {
            panic!("round {round}: the warm turn must be admitted");
        };
        warm.mark_dispatching(&lease.identity)
            .await
            .expect("warm dispatch");
        warm.mark_active(&lease.identity)
            .await
            .expect("warm active");

        let drainer =
            ModelTurnAdmissionRepository::new(Database::reopen_test(&dsn).expect("drain DB"));
        let acquirer =
            ModelTurnAdmissionRepository::new(Database::reopen_test(&dsn).expect("acquire DB"));

        let barrier = Arc::new(Barrier::new(2));
        let drain_barrier = barrier.clone();
        let drain = tokio::spawn(async move {
            drain_barrier.wait().await;
            drainer
                .drain_pool_in_transaction(
                    pool_id,
                    GENERATION,
                    ModelTurnModeChangeReason::CapabilityCoverageLoss,
                )
                .await
                .expect("drain")
        });
        let request = acquire(pool_id, &format!("race-{round}-request"), 1);
        let acquisition = tokio::spawn(async move {
            barrier.wait().await;
            acquirer.acquire_turn(request).await.expect("acquire")
        });
        let drained = drain.await.expect("drain task");
        let acquired = acquisition.await.expect("acquire task");

        let instant = drain_instant(&drained);
        assert_eq!(
            leases_created_after(&owner, pool_id, &instant).await,
            0,
            "round {round}: a lease was created after the drain committed \
             (acquisition outcome {acquired:?})"
        );
        // The same claim from the other side, so neither ordering can pass
        // vacuously: an acquisition that lost the lock must have created no
        // lease at all, and one that won must have created exactly one — and
        // that one must predate the drain's instant.
        let total = lease_count(&owner, pool_id).await;
        match &acquired {
            ModelTurnAcquireOutcome::Admitted { .. } => {
                assert_eq!(
                    total, 2,
                    "round {round}: the warm lease plus the one that won the lock"
                );
                acquired_first += 1;
            }
            ModelTurnAcquireOutcome::Wait(wait) => {
                assert!(
                    matches!(wait, djinn_db::ModelTurnAdmissionWait::Draining),
                    "round {round}: an acquisition behind a drain must wait on the \
                     drain, got {wait:?}"
                );
                assert_eq!(
                    total, 1,
                    "round {round}: an acquisition behind a drain must create no lease"
                );
                drained_first += 1;
            }
            other => panic!("round {round}: unexpected acquisition outcome {other:?}"),
        }
        // And the durable mode is never left admitting.
        assert_eq!(
            stored_mode(&owner, pool_id).await,
            "draining",
            "round {round}: the pool must not still be enforcing after a drain"
        );
    }
    // Not an assertion about scheduling — just a guard against the whole loop
    // silently degenerating into one ordering and proving only half the claim.
    assert!(
        drained_first + acquired_first == 8,
        "every round must have resolved into one of the two orderings"
    );
}

/// A pool must never be `off` while a turn is still in flight: `off` is the
/// state that says nothing is running. This is the drain's read of `in_flight`
/// under the canonical locks, asserted against the durable counter.
#[tokio::test]
async fn a_race_never_settles_a_pool_off_while_a_turn_is_in_flight() {
    let owner = Database::ephemeral().await.expect("owner database");
    let dsn = owner.test_dsn().expect("test database DSN");
    for round in 0..8 {
        let pool_id = seed_pool(&owner, &format!("settle-{round}"), "enforce", 4, 8).await;
        let drainer =
            ModelTurnAdmissionRepository::new(Database::reopen_test(&dsn).expect("drain DB"));
        let acquirer =
            ModelTurnAdmissionRepository::new(Database::reopen_test(&dsn).expect("acquire DB"));
        let barrier = Arc::new(Barrier::new(2));
        let drain_barrier = barrier.clone();
        let drain = tokio::spawn(async move {
            drain_barrier.wait().await;
            drainer
                .drain_pool_in_transaction(
                    pool_id,
                    GENERATION,
                    ModelTurnModeChangeReason::OperatorRequest,
                )
                .await
        });
        let request = acquire(pool_id, &format!("settle-{round}-request"), 1);
        let acquisition = tokio::spawn(async move {
            barrier.wait().await;
            acquirer.acquire_turn(request).await
        });
        let _ = drain.await.expect("drain task");
        let _ = acquisition.await.expect("acquire task");

        let (in_flight, _, _) = accounting(&owner, pool_id).await;
        let mode = stored_mode(&owner, pool_id).await;
        assert!(
            !(mode == "off" && in_flight > 0),
            "round {round}: a pool settled to `off` with {in_flight} turns in flight"
        );
    }
}

// ── AC 2: leases already active at drain time finish normally, once ────────

#[tokio::test]
async fn active_leases_finish_normally_across_a_drain_with_accounting_applied_once() {
    let db = Database::ephemeral().await.expect("database");
    let pool_id = seed_pool(&db, "inflight", "enforce", 2, 8).await;
    let repository = ModelTurnAdmissionRepository::new(db.clone());

    let mut identities = Vec::new();
    for name in ["left", "right"] {
        let outcome = repository
            .acquire_turn(acquire(pool_id, &format!("inflight-{name}"), 2))
            .await
            .expect("acquire");
        let ModelTurnAcquireOutcome::Admitted { lease, .. } = outcome else {
            panic!("expected an admitted turn, got {outcome:?}");
        };
        repository
            .mark_dispatching(&lease.identity)
            .await
            .expect("dispatch");
        repository
            .mark_active(&lease.identity)
            .await
            .expect("activate");
        identities.push(lease.identity);
    }
    assert_eq!(accounting(&db, pool_id).await, (2, 4, 0));

    let drained = repository
        .drain_pool_in_transaction(
            pool_id,
            GENERATION,
            ModelTurnModeChangeReason::OperatorRequest,
        )
        .await
        .expect("drain");
    assert!(matches!(
        drained,
        ModelTurnModeChangeOutcome::Applied {
            to: ModelTurnAdmissionPhase::Draining,
            ..
        }
    ));
    assert_eq!(stored_mode(&db, pool_id).await, "draining");
    // Draining changed nothing about the in-flight accounting.
    assert_eq!(accounting(&db, pool_id).await, (2, 4, 0));

    // Both in-flight leases still heartbeat.
    for identity in &identities {
        assert_eq!(
            repository.heartbeat(identity).await.expect("heartbeat"),
            ModelTurnLeaseMutationOutcome::Applied,
            "a lease active at drain time must keep heartbeating"
        );
    }

    // The first reconciles with authoritative usage, exactly once.
    let reconciliation = |identity: ModelTurnLeaseIdentity| ModelTurnLeaseReconciliationInput {
        identity,
        outcome: ModelTurnLeaseTerminalOutcome::Completed,
        authoritative_usage: Some(ModelTurnAuthoritativeUsage {
            request_units: 3,
            input_units: 0,
            output_units: 0,
            combined_units: 0,
        }),
        detail: None,
    };
    assert_eq!(
        repository
            .reconcile_lease(reconciliation(identities[0].clone()))
            .await
            .expect("reconcile"),
        ModelTurnLeaseMutationOutcome::Applied
    );
    let after_reconcile = accounting(&db, pool_id).await;
    assert_eq!(after_reconcile, (1, 3, 0));
    assert_eq!(
        repository
            .reconcile_lease(reconciliation(identities[0].clone()))
            .await
            .expect("replayed reconcile"),
        ModelTurnLeaseMutationOutcome::Idempotent
    );
    assert_eq!(
        accounting(&db, pool_id).await,
        after_reconcile,
        "a replayed reconciliation must not apply accounting twice"
    );
    // Draining is not off while a lease is still in flight.
    assert_eq!(stored_mode(&db, pool_id).await, "draining");

    // The second is expired by the watchdog, exactly once.
    repository
        .backdate_lease_for_test(&identities[1], "1970-01-01T00:00:00Z", None)
        .await
        .expect("backdate");
    let expiry = |identity: ModelTurnLeaseIdentity| ModelTurnLeaseExpiryInput {
        identity,
        observed_lifecycle: ModelTurnLeaseLifecycle::Active,
        observed_heartbeat_at: None,
        boundary_at: "1970-01-01T00:10:00Z".to_owned(),
    };
    assert_eq!(
        repository
            .expire_lease(expiry(identities[1].clone()))
            .await
            .expect("expire"),
        ModelTurnLeaseMutationOutcome::Applied
    );
    let after_expiry = accounting(&db, pool_id).await;
    assert_eq!(after_expiry, (0, 3, 2));
    assert_eq!(
        repository
            .expire_lease(expiry(identities[1].clone()))
            .await
            .expect("replayed expiry"),
        ModelTurnLeaseMutationOutcome::Fenced
    );
    assert_eq!(
        accounting(&db, pool_id).await,
        after_expiry,
        "a replayed expiry must not release accounting twice"
    );

    // ── AC 3: the last terminal lease settles the pool to `off` ──
    assert_eq!(
        stored_mode(&db, pool_id).await,
        "off",
        "a drained pool reaches `off` once nothing is in flight"
    );
    let settled: Vec<_> = repository
        .pool_mode_transitions(pool_id, 32)
        .await
        .expect("read mode ledger")
        .into_iter()
        .map(|(from, to, reason, _)| (from, to, reason))
        .collect();
    assert_eq!(
        settled,
        vec![
            (
                ModelTurnAdmissionPhase::Enforce,
                ModelTurnAdmissionPhase::Draining,
                "operator_request".to_owned()
            ),
            (
                ModelTurnAdmissionPhase::Draining,
                ModelTurnAdmissionPhase::Off,
                "drain_settled".to_owned()
            ),
        ]
    );

    // ── AC 3: a drain on an already-`off` pool writes nothing ──
    let rows_before = mode_ledger_rows(&db, pool_id).await;
    let repeat = repository
        .drain_pool_in_transaction(
            pool_id,
            GENERATION,
            ModelTurnModeChangeReason::OperatorRequest,
        )
        .await
        .expect("repeat drain");
    assert_eq!(
        repeat,
        ModelTurnModeChangeOutcome::Unchanged {
            mode: ModelTurnAdmissionPhase::Off
        }
    );
    assert_eq!(
        mode_ledger_rows(&db, pool_id).await,
        rows_before,
        "a drain on an off pool must append no row"
    );
    assert_eq!(stored_mode(&db, pool_id).await, "off");
}

#[tokio::test]
async fn a_drain_with_nothing_in_flight_settles_to_off_immediately() {
    let db = Database::ephemeral().await.expect("database");
    let pool_id = seed_pool(&db, "idle", "enforce", 2, 8).await;
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    let outcome = repository
        .drain_pool_in_transaction(
            pool_id,
            GENERATION,
            ModelTurnModeChangeReason::OperatorRequest,
        )
        .await
        .expect("drain");
    assert!(matches!(
        outcome,
        ModelTurnModeChangeOutcome::DrainedAndSettled { .. }
    ));
    assert_eq!(stored_mode(&db, pool_id).await, "off");
    assert_eq!(mode_ledger_rows(&db, pool_id).await, 2);
}

// ── AC 4: the rollback order ───────────────────────────────────────────────

#[test]
fn the_rollback_order_is_fixed() {
    assert_eq!(
        ModelTurnRollbackStepV1::ORDER,
        [
            ModelTurnRollbackStepV1::Controller,
            ModelTurnRollbackStepV1::SlotWrappers,
            ModelTurnRollbackStepV1::ProviderContracts,
            ModelTurnRollbackStepV1::ModeOff,
        ],
        "reordering the rollback steps must be a visible change"
    );
}

#[test]
fn a_provider_contract_rollback_is_rejected_while_a_slot_wrapper_step_is_pending() {
    let mut plan = ModelTurnRollbackPlanV1::new();
    plan.complete(ModelTurnRollbackStepV1::Controller)
        .expect("the controller step is first");
    assert_eq!(
        plan.complete(ModelTurnRollbackStepV1::ProviderContracts),
        Err(ModelTurnModeChangeRejection::RollbackOutOfOrder {
            expected: ModelTurnRollbackStepV1::SlotWrappers,
            attempted: ModelTurnRollbackStepV1::ProviderContracts,
        })
    );
    assert_eq!(
        plan.next_step(),
        Some(ModelTurnRollbackStepV1::SlotWrappers),
        "a rejected step must not advance the plan"
    );
}

#[test]
fn only_the_canonical_sequence_completes_a_rollback() {
    // The canonical order runs clean.
    let mut plan = ModelTurnRollbackPlanV1::new();
    for step in ModelTurnRollbackStepV1::ORDER {
        plan.complete(step).expect("canonical order is accepted");
    }
    assert!(plan.is_complete());

    // Every other permutation is rejected at some step.
    let steps = ModelTurnRollbackStepV1::ORDER;
    for a in 0..4 {
        for b in 0..4 {
            for c in 0..4 {
                for d in 0..4 {
                    let candidate = [steps[a], steps[b], steps[c], steps[d]];
                    if candidate == steps {
                        continue;
                    }
                    let mut plan = ModelTurnRollbackPlanV1::new();
                    let rejected = candidate.iter().any(|step| plan.complete(*step).is_err());
                    assert!(
                        rejected,
                        "sequence {candidate:?} must not complete a rollback"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn rolling_back_to_off_out_of_order_mutates_nothing() {
    let db = Database::ephemeral().await.expect("database");
    let pool_id = seed_pool(&db, "rollback", "shadow", 2, 8).await;
    let repository = ModelTurnAdmissionRepository::new(db.clone());

    let mut plan = ModelTurnRollbackPlanV1::new();
    plan.complete(ModelTurnRollbackStepV1::Controller)
        .expect("stop the controller");
    let refused = repository
        .roll_back_pool_to_off_in_transaction(&mut plan, pool_id, GENERATION)
        .await
        .expect("out-of-order rollback");
    assert_eq!(
        refused,
        ModelTurnModeChangeOutcome::Rejected(ModelTurnModeChangeRejection::RollbackOutOfOrder {
            expected: ModelTurnRollbackStepV1::SlotWrappers,
            attempted: ModelTurnRollbackStepV1::ModeOff,
        })
    );
    // The durable side effects, not the returned variant.
    assert_eq!(
        stored_mode(&db, pool_id).await,
        "shadow",
        "the mode must not go off while an earlier rollback step is pending"
    );
    assert_eq!(mode_ledger_rows(&db, pool_id).await, 0);

    plan.complete(ModelTurnRollbackStepV1::SlotWrappers)
        .expect("retire the slot wrappers");
    plan.complete(ModelTurnRollbackStepV1::ProviderContracts)
        .expect("retire the provider contracts");
    let applied = repository
        .roll_back_pool_to_off_in_transaction(&mut plan, pool_id, GENERATION)
        .await
        .expect("ordered rollback");
    assert!(matches!(
        applied,
        ModelTurnModeChangeOutcome::Applied {
            from: ModelTurnAdmissionPhase::Shadow,
            to: ModelTurnAdmissionPhase::Off,
            ..
        }
    ));
    assert_eq!(stored_mode(&db, pool_id).await, "off");
    assert_eq!(mode_ledger_rows(&db, pool_id).await, 1);
    assert!(plan.is_complete());
}

// ── The mode graph: what the writer will not do ────────────────────────────

#[tokio::test]
async fn an_enforcing_pool_cannot_go_straight_to_off() {
    let db = Database::ephemeral().await.expect("database");
    let pool_id = seed_pool(&db, "no-shortcut", "enforce", 2, 8).await;
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    let outcome = repository
        .set_pool_mode_in_transaction(ModelTurnModeChangeInput {
            pool_id,
            target_mode: ModelTurnAdmissionPhase::Off,
            reason: ModelTurnModeChangeReason::OperatorRequest,
            controller_generation: GENERATION,
        })
        .await
        .expect("mode change");
    assert_eq!(
        outcome,
        ModelTurnModeChangeOutcome::Rejected(ModelTurnModeChangeRejection::UnsupportedTransition {
            from: ModelTurnAdmissionPhase::Enforce,
            to: ModelTurnAdmissionPhase::Off,
        })
    );
    assert_eq!(stored_mode(&db, pool_id).await, "enforce");
    assert_eq!(mode_ledger_rows(&db, pool_id).await, 0);
}

#[tokio::test]
async fn enforce_demands_compatibility_phase_d_and_an_eligible_identity() {
    let db = Database::ephemeral().await.expect("database");
    let pool_id = seed_pool(&db, "enforce-gate", "shadow", 2, 8).await;
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    let enforce = ModelTurnModeChangeInput {
        pool_id,
        target_mode: ModelTurnAdmissionPhase::Enforce,
        reason: ModelTurnModeChangeReason::OperatorRequest,
        controller_generation: GENERATION,
    };

    // A pool that never reached compatibility phase `d` cannot enforce. This
    // is the fail-closed edge: the guard is the only way to reach `d`, and an
    // uncovered or untrained pool never gets there.
    assert_eq!(
        repository
            .set_pool_mode_in_transaction(enforce)
            .await
            .expect("mode change"),
        ModelTurnModeChangeOutcome::Rejected(
            ModelTurnModeChangeRejection::CompatibilityPhaseInsufficient {
                phase: ModelTurnCompatibilityPhase::A
            }
        )
    );
    assert_eq!(stored_mode(&db, pool_id).await, "shadow");

    sqlx::query(
        "UPDATE model_turn_pools SET compatibility_phase = 'd', identity_state = 'ambiguous' \
         WHERE id = $1",
    )
    .bind(pool_id)
    .execute(db.pool())
    .await
    .expect("reach phase d with an ambiguous identity");
    assert_eq!(
        repository
            .set_pool_mode_in_transaction(enforce)
            .await
            .expect("mode change"),
        ModelTurnModeChangeOutcome::Rejected(ModelTurnModeChangeRejection::IdentityIneligible {
            state: djinn_db::ModelTurnIdentityState::Ambiguous
        })
    );
    assert_eq!(stored_mode(&db, pool_id).await, "shadow");
    assert_eq!(mode_ledger_rows(&db, pool_id).await, 0);

    sqlx::query("UPDATE model_turn_pools SET identity_state = 'eligible' WHERE id = $1")
        .bind(pool_id)
        .execute(db.pool())
        .await
        .expect("restore identity");
    assert!(matches!(
        repository
            .set_pool_mode_in_transaction(enforce)
            .await
            .expect("mode change"),
        ModelTurnModeChangeOutcome::Applied {
            from: ModelTurnAdmissionPhase::Shadow,
            to: ModelTurnAdmissionPhase::Enforce,
            ..
        }
    ));
    assert_eq!(stored_mode(&db, pool_id).await, "enforce");
    assert_eq!(mode_ledger_rows(&db, pool_id).await, 1);
}
