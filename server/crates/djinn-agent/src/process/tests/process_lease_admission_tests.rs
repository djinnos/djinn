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
/// - `lifts == 1` is the fenced `cpu.max` raise that the
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
        1,
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
    assert_eq!(*launcher.lifts.lock().unwrap(), 0);
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
        *launcher.lifts.lock().unwrap() == 0,
        "a failed epoch read must never lift cpu.max"
    );
}

// ---------------------------------------------------------------------------
// Decision -> birth-quota mapping, and the two decisions that must NOT lift.
//
// Moved here from `process_lease_tests.rs` (which was over the 51200-byte source
// guard) because they belong with the tests above: every one of them is about
// which admission decision reaches the launcher and what quota it commits.
// ---------------------------------------------------------------------------

/// AC1: a shadow epoch never lifts cpu.max even on a valid matching bind; the
/// spawn still traverses the launcher and the durable lease is reconciled.
#[tokio::test]
async fn shadow_epoch_binds_but_never_lifts() {
    let services = Arc::new(ScriptedServices::new(
        vec![granted(7)],
        vec![status(LeaseState::Active, Some(7))],
        vec![status(LeaseState::Active, Some(7)); 20],
    ));
    services.set_lift_decision(djinn_supervisor::services::InvocationLiftDecision::Shadow);
    services
        .release
        .lock()
        .unwrap()
        .push_back(LeaseResult::Released {
            candidate_cleanup: false,
        });
    let launcher = Arc::new(ScriptedLauncher::default());
    let cancel = CancellationToken::new();
    let runner = LeaseInvocationRunner::new(
        services.clone(),
        services.clone(),
        launcher.clone(),
        clock(),
    );
    let run_cancel = cancel.clone();
    let run = tokio::spawn(async move { runner.output(command(), config(), run_cancel).await });
    wait_for(&services.status_calls, 3).await;
    cancel.cancel();
    run.await.unwrap().unwrap();
    // The launcher was driven (queue + grant happened) but never lifted.
    assert_eq!(services.queue_calls.load(Ordering::SeqCst), 1);
    assert_eq!(services.grant_calls.load(Ordering::SeqCst), 1);
    assert!(
        *launcher.lifts.lock().unwrap() == 0,
        "shadow epoch must never lift cpu.max"
    );
    // Shadow is the one decision that clamps ON PURPOSE: it is an observation
    // mode, so the leaf must still be born at the unleased quota.
    assert_eq!(
        launcher.authorities(),
        vec![djinn_cgroup_launcher::LeaseAuthority::Armed],
        "shadow observation is only meaningful against a clamped leaf"
    );
    // The durable lease is still reconciled to terminal (fence recorded).
    assert!(services.release_calls.load(Ordering::SeqCst) <= 1);
    assert_eq!(launcher.kills.load(Ordering::SeqCst), 1);
}

/// AC3 (agent side): an unknown/stale/baseline/ABSENT epoch cannot lift, so the
/// leaf must be born WITHOUT a quota of its own — not pinned to the unleased
/// quota for the whole command.
///
/// # The production symptom this now names (goxi launcher blocker 11)
///
/// The previous version of this test asserted only `lifts.is_empty()` and
/// passed, because a no-op lift is exactly what the code did. What it could not
/// see is that the leaf had *already* been born at 250m, so "never lifts" meant
/// "clamped at 250m forever". Production ran with the durable
/// `admission_handoff` row absent — `Unleased` for every invocation — and a
/// measured leaf reached 21.1 CPU-seconds, 84x the 0.25 CPU-s escalation
/// threshold, with `cpu.max` still reading `25000 100000`. Builds ran ~16x
/// slower armed than disabled.
///
/// Reverting `birth_authority`'s `Unleased` arm to `Armed` reproduces that:
/// this assertion fails with `Armed`, i.e. "the leaf was clamped by an authority
/// that can never lift it".
#[tokio::test]
async fn unleased_epoch_is_born_unclamped_because_no_grant_can_ever_lift_it() {
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
    let launcher = Arc::new(ScriptedLauncher::default());
    let cancel = CancellationToken::new();
    let runner = LeaseInvocationRunner::new(
        services.clone(),
        services.clone(),
        launcher.clone(),
        clock(),
    );
    let run_cancel = cancel.clone();
    let run = tokio::spawn(async move { runner.output(command(), config(), run_cancel).await });
    wait_for(&services.status_calls, 3).await;
    cancel.cancel();
    run.await.unwrap().unwrap();
    assert_eq!(services.grant_calls.load(Ordering::SeqCst), 1);
    assert!(
        *launcher.lifts.lock().unwrap() == 0,
        "unleased epoch must never lift cpu.max"
    );
    assert_eq!(
        launcher.authorities(),
        vec![djinn_cgroup_launcher::LeaseAuthority::Unarmed],
        "an epoch that can never grant a lift must not clamp the leaf either; \
         `Armed` here IS the production defect (cpu.max pinned at 25000 100000 \
         while the leaf burned 21.1 CPU-seconds)"
    );
    assert!(services.release_calls.load(Ordering::SeqCst) <= 1);
}

/// The mapping itself, enumerated: every decision the durable authority can
/// produce, and the birth quota it commits.
///
/// `Unleased` is what `evaluate_invocation_lift` returns for an ABSENT handoff
/// row, which is the state production is actually in — see
/// `absent_handoff_row_is_unleased_and_therefore_unarmed` below for the
/// composition that ties the two together.
#[test]
fn birth_authority_is_armed_only_when_a_lift_is_reachable() {
    use djinn_cgroup_launcher::LeaseAuthority;
    use djinn_supervisor::services::InvocationLiftDecision;
    assert_eq!(
        crate::process::birth_authority(InvocationLiftDecision::Lift),
        LeaseAuthority::Armed
    );
    assert_eq!(
        crate::process::birth_authority(InvocationLiftDecision::Shadow),
        LeaseAuthority::Armed
    );
    assert_eq!(
        crate::process::birth_authority(InvocationLiftDecision::Unleased),
        LeaseAuthority::Unarmed
    );
}

/// The composition that failed in production: the DURABLE row state that
/// production actually has, projected through the real `evaluate_invocation_lift`
/// and the real `birth_authority`, must reach the launcher as `Unarmed`.
///
/// `djinn-server epoch show` on the production node reported
/// `admission handoff row: <absent>` while the launcher was armed. That is not a
/// hypothetical: it is the exact input below.
#[test]
fn absent_handoff_row_is_unleased_and_therefore_unarmed() {
    use djinn_cgroup_launcher::LeaseAuthority;
    use djinn_supervisor::services::evaluate_invocation_lift;
    // Absent row (production), and unreadable row (a DB blip) — both fail closed
    // on the lift, so neither may clamp.
    for row in [Ok(None), Err(())] {
        let decision = evaluate_invocation_lift(row);
        assert_eq!(
            crate::process::birth_authority(decision),
            LeaseAuthority::Unarmed,
            "decision {decision:?} cannot lift, so it must not clamp"
        );
    }
}
