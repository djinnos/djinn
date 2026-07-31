// Test: eprintln reports the live governor numbers AC3 requires to be RECORDED.
#![allow(clippy::print_stderr)]
//! AC3 + AC5 of `fbiy-B2a` (`c6ej`): the invocation-lease cap, driven from three
//! LIVE Pod UIDs against a real PostgreSQL governor.
//!
//! Sibling of `kueue_disruption_conformance` (AC1/AC2), which carries the module
//! docs for the whole live harness — the four live findings, the gating split,
//! and the cluster-isolation rules all apply here unchanged. The two are
//! separate test TARGETS only because one file exceeded
//! `scripts/check-file-size.sh`; they share `tests/kueue_disruption/mod.rs`.
//!
//! Run them together, single-threaded — both drive the same disposable cluster
//! and the same `pods` quota:
//!
//! ```bash
//! DJINN_TEST_KUEUE_CLUSTER=1 cargo test -p djinn-k8s \
//!     --test kueue_disruption_governor -- --ignored --test-threads=1
//! ```

use djinn_db::{
    BuildLeaseConsumerKind, BuildLeaseKey, BuildLeaseRepository, BuildLeaseState, Database,
    GrantNextBuildLeaseResult, InvocationLeaseAuthorityRepository, InvocationLeaseMode,
    QueueBuildLeaseInput,
};
use djinn_supervisor::services::{InvocationLiftDecision, evaluate_invocation_lift};

mod kueue_disruption;
use kueue_disruption::*;

// ===========================================================================
// AC3 + AC5 — the cap bound, driven from three LIVE Pod UIDs
// ===========================================================================

fn invocation_key(pod_uid: &str) -> BuildLeaseKey {
    BuildLeaseKey {
        consumer_kind: BuildLeaseConsumerKind::TaskInvocation,
        consumer_id: format!("invocation-{pod_uid}"),
    }
}

/// Simultaneous lift attempts from the ORIGINAL, the REPLACEMENT and a NEWLY
/// ADMITTED Pod never authorize more than the live invocation-lease cap.
///
/// Both numbers are read live and neither is a constant in this test:
///
/// * `M` is the ClusterQueue's `admittedWorkloads` at its live peak, bounded by
///   the `pods` nominalQuota the chart installed;
/// * `K` is read back out of the durable invocation-lease authority — the row
///   `BuildLeaseService` adopts at runtime and `djinn-server epoch set-cap`
///   writes — after being armed through the real operator API.
///
/// `K < M` is asserted, because a cap at or above the number of admissible
/// Workloads bounds nothing and every assertion under it would be vacuous.
///
/// AC5's four deletions each fail a DIFFERENT assertion here, which is what
/// makes this suite non-vacuous rather than merely green:
///
/// * delete the **invocation queue** (`grant_next`'s queue read) → nothing is
///   ever granted → `authorized == K` fails at 0;
/// * delete the **weighted occupancy SUM** → every queued lease fits → all `M`
///   are granted → `authorized <= K` fails at `M`;
/// * delete the **cap** comparison → same, `authorized <= K` fails;
/// * delete **`bound_pod_uid` authorization** → the replacement UID's lift is
///   accepted → the rejection assertion fails.
///
/// Driven against a real PostgreSQL database rather than a mock repository
/// because the refusal is a locked read-modify-write plus a trigger-enforced
/// immutable column: a mock proves only that the Rust `if` was written.
#[test]
#[ignore]
fn live_simultaneous_lifts_from_three_pod_uids_never_exceed_the_live_cap() {
    if !live_tests_enabled() {
        return;
    }
    let context = harness_context();
    clear_task_run_jobs(&context);
    repair_cluster_queue_for_admission(&context);
    ensure_workload_image_on_the_node();
    let config = config_for(&context, WORKLOAD_IMAGE);

    // --- Three LIVE Pod UIDs -----------------------------------------------
    //
    // Not invented strings: an invented UID cannot show that the fence rejects
    // the object Kubernetes actually created.
    let (first_task_run, first_job) = launch_task_run(&context, &config);
    let (_, original_uid) = await_running_pod(&context, &first_task_run);

    let (second_task_run, second_job) = launch_task_run(&context, &config);
    let (_, newly_admitted_uid) = await_running_pod(&context, &second_task_run);

    let peak_admitted = admitted_workloads(&context);

    // The replacement: evict, then release, and take the UID of the Pod the
    // re-admission creates for the SAME Job.
    evict_and_release(&context, &first_task_run, &original_uid);
    let (_, replacement_uid) = await_running_pod(&context, &first_task_run);

    assert_ne!(
        replacement_uid, original_uid,
        "the re-admitted Pod must carry a different immutable UID, or there is nothing for the \
         fence to refuse",
    );
    let live_uids = [
        original_uid.clone(),
        replacement_uid.clone(),
        newly_admitted_uid.clone(),
    ];
    eprintln!("AC3 live Pod UIDs: {live_uids:?}");

    // --- K and M, both read live -------------------------------------------
    let m = peak_admitted.max(admitted_workloads(&context));
    assert!(
        m >= 2,
        "the live ClusterQueue admitted only {m} Workload(s); a cap cannot be strictly below 1",
    );

    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("build a tokio runtime for the durable governor");
    tokio_runtime.block_on(async move {
        let db = Database::open_in_memory().expect("real Postgres test database");
        let authority = InvocationLeaseAuthorityRepository::new(db.clone());
        let seeded = authority.seed_baseline().await.expect("seed the authority");
        // Armed through the real operator API, at a cap derived from the LIVE
        // admitted-Workload count rather than from a literal.
        authority
            .set_mode_and_cap(
                seeded.epoch,
                InvocationLeaseMode::Enforce,
                Some(i64::try_from(m).expect("M fits an i64") - 1),
            )
            .await
            .expect("arm the invocation-lease authority");

        // READ BACK. Everything below uses this value, never the one written.
        let live = authority
            .read()
            .await
            .expect("read the durable authority")
            .expect("the authority row exists once seeded");
        let k = live
            .cap
            .expect("an armed authority carries a reference cap");
        assert_eq!(
            evaluate_invocation_lift(Ok(Some(live.clone()))),
            InvocationLiftDecision::Lift,
            "the live authority must actually authorize lifts, or the cap bounds a population \
             that never lifts and AC3 measures nothing",
        );
        assert!(
            k < i64::try_from(m).expect("M fits an i64"),
            "the invocation-lease cap K={k} must be strictly below the M={m} Workloads the live \
             ClusterQueue admits, or it bounds nothing",
        );
        eprintln!(
            "AC3 live governor configuration: K={k} (durable authority), M={m} (live ClusterQueue)"
        );

        // --- Drive the lifts, all three at once -----------------------------
        let repository = std::sync::Arc::new(BuildLeaseRepository::new(db.clone()));
        for uid in &live_uids {
            repository
                .queue(&QueueBuildLeaseInput {
                    key: invocation_key(uid),
                    immutable_identity: format!("pod:{uid}"),
                    queue_deadline: None,
                    launch_deadline: None,
                    weight: 1,
                })
                .await
                .unwrap_or_else(|e| panic!("queue an invocation lease for {uid}: {e:?}"));
        }

        // Every grant the FIFO will make at the live cap, taken concurrently so
        // the bound is a property of the locked transaction rather than of the
        // order this test happened to call in.
        let mut granted = Vec::new();
        let mut attempts = tokio::task::JoinSet::new();
        for _ in 0..live_uids.len() {
            let repository = repository.clone();
            attempts.spawn(async move {
                repository
                    .grant_next(k, "2026-07-30T00:00:00.000Z", None)
                    .await
                    .expect("grant_next must not error")
            });
        }
        while let Some(result) = attempts.join_next().await {
            if let GrantNextBuildLeaseResult::Granted(row) = result.expect("grant task panicked") {
                granted.push(row);
            }
        }

        // Bind each grant to the live Pod UID it was queued for. `bind` is the
        // sole operation that carries a Pod identity, so this is the complete
        // lift surface.
        let mut authorized = 0i64;
        for row in &granted {
            let uid = row
                .immutable_identity
                .strip_prefix("pod:")
                .expect("the identity carries its Pod UID")
                .to_owned();
            let token = row.fencing_token.expect("a granted lease carries a token");
            let bound = repository
                .bind(&invocation_key(&uid), token, &uid, None)
                .await
                .unwrap_or_else(|e| panic!("bind the lease for {uid}: {e:?}"));
            assert_eq!(bound.state, BuildLeaseState::Bound);
            assert_eq!(bound.bound_pod_uid.as_deref(), Some(uid.as_str()));
            authorized += 1;
        }

        eprintln!("AC3 authorized lifts = {authorized} against live cap K={k} (M={m})");
        assert!(
            authorized <= k,
            "{authorized} invocations hold a lifted cpu.max, above the live cap K={k}",
        );
        assert_eq!(
            authorized, k,
            "the cap must be REACHED as well as respected: {authorized} of a possible K={k} were \
             authorized, which is what a deleted invocation queue looks like",
        );

        // --- The replacement UID, refused --------------------------------
        //
        // The original's lease, presented with the UID of the Pod the
        // re-admission created. This is the fence AC2 asks for, driven from a
        // UID Kubernetes minted rather than one this test made up.
        let original_key = invocation_key(&original_uid);
        if let Some(row) = repository
            .get(&original_key)
            .await
            .expect("read the original's lease row")
            .filter(|row| row.bound_pod_uid.is_some())
        {
            let token = row.fencing_token.expect("a bound lease carries a token");
            let rejected = repository
                .bind(&original_key, token, &replacement_uid, None)
                .await
                .expect_err(
                    "a lift presenting the LIVE replacement Pod's UID against the original's \
                     lease must be rejected",
                );
            assert!(
                format!("{rejected:?}").contains("pod UID does not match build lease"),
                "the refusal must be the pod-UID fence, not an unrelated error: {rejected:?}",
            );
            let after = repository
                .get(&original_key)
                .await
                .expect("re-read the lease row")
                .expect("the lease row exists");
            assert_eq!(
                after.bound_pod_uid.as_deref(),
                Some(original_uid.as_str()),
                "no rejected lift may have moved the durable Pod binding",
            );
        }
    });

    delete_job(&context, &first_job);
    delete_job(&context, &second_job);
}
