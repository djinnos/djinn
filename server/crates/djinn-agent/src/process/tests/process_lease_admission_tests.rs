//! The in-pod admission composition: does an ARMED durable epoch actually reach
//! the launcher as a lifted quota?
//!
//! # Why this file exists (goxi launcher blocker 13)
//!
//! Every other test in this suite injected the lift decision through the
//! `ScriptedServices` double, because the decision used to be a **defaulted**
//! method on `SupervisorServices`. That is precisely why the suite was green
//! while production was inert: the double overrode the default, and the object
//! the production composition actually hands the runner — the worker's
//! `Arc<RpcServices>`, built in `ShellLaunchContext::broker_backed` — did not.
//! So every in-pod invocation resolved the fail-closed default while
//! `djinn-server epoch show` reported a fully armed epoch:
//!
//! ```text
//! phase ForwardOverlap · epoch 3 · v0_mode Enforce · v1_mode Enforce · cap 3
//! emergency_ack_epoch 3 · invocation_ack_epoch 3
//! ```
//!
//! ```text
//! INFO djinn_agent::process: lease invocation launched into a cgroup leaf
//!   decision=Unleased authority=Unarmed threshold_usec=250000
//! ```
//!
//! Measured on the launcher's delegated cgroup, every per-invocation leaf was
//! born at `cpu.max=[max 100000]` and never transitioned; `nr_throttled` stayed
//! 0 across many invocations. The mechanism was inert while its status field said
//! "armed" — the thirteenth consecutive blocker in this feature with exactly that
//! shape.
//!
//! The tests below therefore assert the **side effect the feature exists to
//! produce** — a leaf born at the 250m unleased quota (`LeaseAuthority::Armed`)
//! that is then raised by a fenced lift — against a REAL durable row read by the
//! REAL production authority, with a lease-authority double that deliberately has
//! no admission opinion of its own. Neutralize the fix (make
//! `LeaseInvocationRunner::output` resolve the decision from `self.services`
//! again, i.e. what the trait default did) and the first test fails on both
//! assertions: `authority=Unarmed`, no lift.

use super::*;
use djinn_supervisor::services::DurableInvocationLiftAuthority;

/// The production authority over the in-pod platform database. `origin` matches
/// what `ShellLaunchContext::broker_backed` passes so a failure in this test
/// reads like the pod's own log line.
fn in_pod_authority(db: &djinn_db::Database) -> Arc<DurableInvocationLiftAuthority> {
    Arc::new(DurableInvocationLiftAuthority::new(
        db.clone(),
        "in-pod worker",
    ))
}

/// A lease-authority double that grants, binds, and releases — and that, like the
/// real `RpcServices` the pod hands the runner, has NO admission opinion.
///
/// `set_lift_decision(Unleased)` is the point, not an incidental default: it
/// encodes "the services object cannot authorize a lift". The runner must ask the
/// injected [`DurableInvocationLiftAuthority`] instead. If anyone rewires it to
/// ask `self.services` again, these assertions fail.
fn lease_authority_without_an_admission_opinion() -> Arc<ScriptedServices> {
    let services = Arc::new(ScriptedServices::new(
        vec![granted(7)],
        vec![status(LeaseState::Active, Some(7))],
        vec![status(LeaseState::Active, Some(7)); 20],
    ));
    services.set_lift_decision(djinn_supervisor::services::InvocationLiftDecision::Unleased);
    services
        .release
        .lock()
        .unwrap()
        .push_back(LeaseResult::Released {
            candidate_cleanup: false,
        });
    services
}

/// THE regression test. An armed durable epoch — `ForwardOverlap`, v1 `Enforce`,
/// both acknowledgements at the current epoch, the exact production row — must
/// produce a leaf that is born clamped at the unleased quota and then LIFTED.
///
/// Both assertions matter and neither is a status field:
///
/// - `authorities == [Armed]` is the birth quota the broker committed (250m).
/// - `lifts == [LeaseFencingToken(7)]` is the fenced `cpu.max` raise that the
///   whole feature exists to perform.
///
/// Asserting only the second would let a regression that never clamps pass;
/// asserting only the first is what shipped for four rollouts.
#[tokio::test]
async fn armed_epoch_is_born_clamped_and_then_lifted_through_the_in_pod_composition() {
    let db = crate::test_helpers::create_test_db();
    super::lease_degrade_tests::arm_invocation_lift(&db).await;

    let services = lease_authority_without_an_admission_opinion();
    let launcher = Arc::new(ScriptedLauncher::default());
    let cancel = CancellationToken::new();
    let runner = LeaseInvocationRunner::new(
        services.clone(),
        in_pod_authority(&db),
        launcher.clone(),
        clock(),
    );
    let run_cancel = cancel.clone();
    let run = tokio::spawn(async move { runner.output(command(), config(), run_cancel).await });
    wait_for(&services.status_calls, 3).await;
    cancel.cancel();
    run.await.unwrap().unwrap();

    assert_eq!(
        launcher.authorities(),
        vec![djinn_cgroup_launcher::LeaseAuthority::Armed],
        "an armed epoch must clamp the leaf at birth — a lift can only raise a \
         quota that exists"
    );
    assert_eq!(
        *launcher.lifts.lock().unwrap(),
        vec![LeaseFencingToken(7)],
        "the armed epoch reached a matching durable bind, so cpu.max MUST be \
         lifted under the granted fence. An empty vec here is goxi blocker 13: \
         `decision=Unleased authority=Unarmed` against `ForwardOverlap · epoch 3 \
         · v1 Enforce · both acks 3`, every leaf born at `max 100000` and never \
         transitioning"
    );
}

/// Fail-closed, unchanged: the same composition over a database whose
/// `admission_handoff` row is ABSENT must not lift, and must not clamp either
/// (nothing could ever raise that leaf — blocker 11).
#[tokio::test]
async fn absent_row_still_fails_closed_through_the_in_pod_composition() {
    let db = crate::test_helpers::create_test_db();
    super::lease_degrade_tests::arm_invocation_lift(&db).await;
    djinn_db::AdmissionHandoffRepository::new(db.clone())
        .delete_for_test()
        .await
        .unwrap();

    let services = lease_authority_without_an_admission_opinion();
    let launcher = Arc::new(ScriptedLauncher::default());
    let cancel = CancellationToken::new();
    let runner = LeaseInvocationRunner::new(
        services.clone(),
        in_pod_authority(&db),
        launcher.clone(),
        clock(),
    );
    let run_cancel = cancel.clone();
    let run = tokio::spawn(async move { runner.output(command(), config(), run_cancel).await });
    wait_for(&services.status_calls, 3).await;
    cancel.cancel();
    run.await.unwrap().unwrap();

    assert_eq!(
        launcher.authorities(),
        vec![djinn_cgroup_launcher::LeaseAuthority::Unarmed]
    );
    assert!(launcher.lifts.lock().unwrap().is_empty());
}

/// The wrong-database hazard, end to end.
///
/// A task-run Pod's worker container carries TWO Postgres DSNs:
/// `DJINN_DATABASE_URL` (the platform database, where `admission_handoff` lives)
/// and `DATABASE_URL` (the project's `svc-postgres` catalog-service sidecar,
/// which has no such table). If the lift decision is ever composed from the
/// wrong one, the read fails — and with the old `.map_err(|_| ())` that was
/// indistinguishable from an unarmed epoch, in behaviour AND in logs.
///
/// It must still fail closed (asserted here) while being reported as a defect
/// (`AdmissionEpochRead::Failed`, asserted in `djinn-supervisor`'s
/// `a_failed_read_is_distinguishable_from_an_absent_row_and_both_fail_closed`).
#[tokio::test]
async fn a_non_platform_database_fails_closed_instead_of_lifting() {
    // Reachable, valid Postgres — just not the platform database. Same shape as
    // pointing the pod at its catalog sidecar.
    let base = djinn_db::test_database_base_url();
    let trimmed = base.trim_end_matches('/');
    let server_prefix = trimmed
        .rsplit_once('/')
        .map_or(trimmed, |(prefix, _)| prefix);
    let not_the_platform_db = djinn_db::Database::open_with_config(
        djinn_db::DatabaseConnectConfig::Postgres(djinn_db::PostgresDatabaseConfig {
            url: format!("{server_prefix}/postgres"),
        }),
    )
    .unwrap();

    let services = lease_authority_without_an_admission_opinion();
    let launcher = Arc::new(ScriptedLauncher::default());
    let cancel = CancellationToken::new();
    let runner = LeaseInvocationRunner::new(
        services.clone(),
        in_pod_authority(&not_the_platform_db),
        launcher.clone(),
        clock(),
    );
    let run_cancel = cancel.clone();
    let run = tokio::spawn(async move { runner.output(command(), config(), run_cancel).await });
    wait_for(&services.status_calls, 3).await;
    cancel.cancel();
    run.await.unwrap().unwrap();

    assert_eq!(
        launcher.authorities(),
        vec![djinn_cgroup_launcher::LeaseAuthority::Unarmed],
        "a failed epoch read must never be treated as authorization"
    );
    assert!(
        launcher.lifts.lock().unwrap().is_empty(),
        "a failed epoch read must never lift cpu.max"
    );
}
