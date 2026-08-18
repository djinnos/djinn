//! The normative model-turn admission conformance target.
//!
//! Proposal `96fy` names one command as the verification surface for the whole
//! model-turn admission programme:
//!
//! ```text
//! cargo test -p djinn-coordinator --test model_admission_conformance
//! ```
//!
//! Before this file existed `djinn-coordinator` had **no** integration test
//! target at all, so that command selected nothing and exited zero — a filter
//! matching nothing is indistinguishable from a passing suite. This target is
//! the crate's first, and every later Phase D scenario is added here.
//!
//! ## Rules this target holds itself to
//!
//! * **No raw `sqlx`.** `scripts/check-raw-sql-boundary.sh` is default-deny and
//!   covers test files. Durable state is seeded and read exclusively through
//!   `djinn_db::test_support` fixtures and the production
//!   `ModelTurnAdmissionRepository`.
//! * **No production visibility widened.** The dispatch admission primitives
//!   stay `pub(crate)`; this target reaches them through the thin forwarders in
//!   `djinn_coordinator::test_helpers`, compiled only under the `test-support`
//!   feature. Each forwarder calls exactly the function production calls.
//! * **No process-global telemetry reads.** Metrics assertions run inside a
//!   fixture-local [`djinn_telemetry::IsolatedRecorder`] scope. Commit
//!   `c3d3bc675` removed destructive process-global recorder reads from this
//!   crate's tests because they collide across a shared process, and the
//!   normative command above is `cargo test`, which shares one process for the
//!   whole binary — strictly worse than the nextest CI path.
//! * **Assertions land on persisted rows.** An admission decision is proven by
//!   counting the `model_turn_leases` rows the production acquisition path
//!   wrote, never by the enum a fixture handed back or a log line.

// Test-only assertion ergonomics. The package denies these lints for production
// modules (see `Cargo.toml` `[lints.clippy]`, which applies to every target);
// `lib.rs` grants the same opt-out to unit tests via `cfg_attr(test, ...)`, and
// an integration target has to state it for itself.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods
)]

use std::collections::HashMap;

use djinn_core::models::{LaneMaxSessions, ModelLane};
use djinn_db::test_support::{
    model_turn_lease_count_for_pool_fixture, model_turn_lease_total_count_fixture,
    model_turn_max_pool_id_fixture, seed_model_turn_admission_fixture,
};
use djinn_db::{
    Database, MODEL_TURN_ADMISSION_SCHEMA_VERSION, ModelTurnAcquireInput, ModelTurnAcquireOutcome,
    ModelTurnAdmissionRejection, ModelTurnAdmissionRepository, ModelTurnBucketDebit,
    ModelTurnBucketKind,
};

/// The credential/provider/model scope `seed_model_turn_admission_fixture`
/// writes. Kept next to its only consumer so a scenario asserting pool
/// resolution names the same identity the fixture persisted.
const FIXTURE_CREDENTIAL_ID: &str = "credential-slot";
const FIXTURE_PROVIDER_ID: &str = "provider";
const FIXTURE_MODEL_ID: &str = "model";

/// Run `body` with every `metrics` emission on this thread routed to a
/// recorder no other test in the binary can reach, and hand back what `body`
/// returned together with that recorder's rendered contents.
///
/// This is the fixture-local boundary-observation recorder the target uses in
/// place of `djinn_telemetry::render()`. The process-global registry is
/// cumulative across the whole test binary, so an absolute assertion against it
/// is really an assertion about whichever sibling test wrote last.
///
/// Soundness: the scope is thread-local, and a `#[tokio::test]` body is polled
/// by `block_on` on the thread that created the guard, so the body's own
/// straight-line code is captured across `.await` points. Anything the body
/// hands to `tokio::spawn` records elsewhere and is simply absent from the
/// render — a loud failure, not a silent one.
async fn with_fixture_local_recorder<F, T>(body: F) -> (T, String)
where
    F: AsyncFnOnce() -> T,
{
    let recorder = djinn_telemetry::IsolatedRecorder::new();
    let guard = recorder.scope();
    let value = body().await;
    drop(guard);
    let rendered = recorder.render();
    (value, rendered)
}

fn request_debit(units: i64) -> Vec<ModelTurnBucketDebit> {
    vec![ModelTurnBucketDebit {
        bucket_kind: ModelTurnBucketKind::Request,
        units,
    }]
}

/// Phase A's durable prerequisite, asserted end to end against persisted rows.
///
/// Three facts, in order:
///
/// 1. The `model_turn_admission_schema` marker is installed at exactly the
///    revision this binary understands. Admission storage that a binary cannot
///    read is not a prerequisite that "mostly" holds.
/// 2. A pool seeded under that schema **resolves** through the production
///    `resolve_pool` lookup, and acquiring against it writes exactly one
///    durable `model_turn_leases` row.
/// 3. Acquiring against a pool id that does **not** resolve is rejected as
///    `PoolUnavailable` and writes **zero** lease rows — the denial is proven
///    by the row count, not by the returned enum.
///
/// The unresolvable acquisition deliberately runs *before* the admitted one, so
/// the assertion after it is `0` rather than "unchanged": a lease written by
/// the denied path could not hide behind a pre-existing row.
#[tokio::test]
async fn phase_a_schema_prerequisite() {
    let db = djinn_coordinator::test_helpers::create_test_db();
    let repository = ModelTurnAdmissionRepository::new(db.clone());

    let readiness = repository
        .schema_readiness()
        .await
        .expect("probe model-turn admission schema readiness")
        .expect("migration 173 installs the model-turn admission schema marker");
    assert_eq!(
        readiness.model_turn_admission_schema, MODEL_TURN_ADMISSION_SCHEMA_VERSION,
        "the installed marker must match the revision this binary understands"
    );

    assert_eq!(
        model_turn_lease_total_count_fixture(&db).await,
        0,
        "a freshly migrated database holds no model-turn leases"
    );

    let pool_id = seed_model_turn_admission_fixture(&db, "enforce", "supported", 2).await;
    let resolved = repository
        .resolve_pool(FIXTURE_CREDENTIAL_ID, FIXTURE_PROVIDER_ID, FIXTURE_MODEL_ID)
        .await
        .expect("resolve the seeded admission pool")
        .expect("the seeded scope must resolve to exactly one pool");
    assert_eq!(
        resolved.id, pool_id,
        "resolution must return the pool the fixture persisted"
    );

    // ── Unresolvable pool ⇒ denial, and no durable lease ──────────────────
    let unresolvable_pool_id = model_turn_max_pool_id_fixture(&db)
        .await
        .expect("the seeded pool makes the ledger non-empty")
        + 1_000;
    let denied = repository
        .acquire_turn(ModelTurnAcquireInput {
            pool_id: unresolvable_pool_id,
            request_id: "conformance-phase-a-unresolvable".to_owned(),
            owner_pod_uid: None,
            generation: 1,
            debits: request_debit(1),
        })
        .await
        .expect("acquisition against an unresolvable pool must not error");
    assert!(
        matches!(
            denied,
            ModelTurnAcquireOutcome::Rejected(ModelTurnAdmissionRejection::PoolUnavailable)
        ),
        "an unresolvable pool is rejected as PoolUnavailable, got {denied:?}"
    );
    assert_eq!(
        model_turn_lease_total_count_fixture(&db).await,
        0,
        "a denied acquisition must write no model_turn_leases row"
    );

    // ── Resolvable enforce pool ⇒ exactly one durable lease ───────────────
    let admitted = repository
        .acquire_turn(ModelTurnAcquireInput {
            pool_id,
            request_id: "conformance-phase-a-admitted".to_owned(),
            owner_pod_uid: Some("pod-conformance-phase-a".to_owned()),
            generation: 1,
            debits: request_debit(1),
        })
        .await
        .expect("acquisition against the resolved pool must not error");
    assert!(
        matches!(admitted, ModelTurnAcquireOutcome::Admitted { .. }),
        "an enforce-phase pool with a supported capability admits, got {admitted:?}"
    );
    assert_eq!(
        model_turn_lease_count_for_pool_fixture(&db, pool_id).await,
        1,
        "the admitted acquisition writes exactly one lease against its own pool"
    );
    assert_eq!(
        model_turn_lease_total_count_fixture(&db).await,
        1,
        "no lease exists anywhere except the one the admitted acquisition wrote"
    );

    close(db).await;
}

/// The resident-admission seam is reachable, and behaves identically, from
/// outside the crate.
///
/// `krpy` pins the full three-mode truth table on top of this; the point here is
/// narrower and is a property of the *target*, not of admission: the
/// `test-support` forwarders compile and dispatch to the production functions
/// from an integration target, so later scenarios have a seam to stand on.
/// A conjunction is proven by exhibiting each branch that can falsify it.
///
/// The gauge assertion reads a fixture-local registry. It proves the primitive
/// this target calls is the instrumented production one rather than a
/// re-implementation, and it does so without touching the process-global
/// recorder.
#[tokio::test]
async fn resident_admission_seam_is_reachable_out_of_crate() {
    use djinn_coordinator::test_helpers::{
        lane_under_user_cap, model_under_user_cap, resident_admission_allows,
    };

    let user = "conformance-user";
    let model = "provider/model";
    // `worker` maps to the implement lane; a lane ceiling of 1 there is what the
    // lane half of the conjunction is asked about.
    let role = "worker";
    let max_sessions: HashMap<String, u32> = [(model.to_owned(), 2_u32)].into_iter().collect();
    let lane_max_sessions = LaneMaxSessions {
        plan: 5,
        implement: 1,
        review: 5,
    };

    let empty_model: HashMap<(String, String), u32> = HashMap::new();
    let empty_lane: HashMap<(String, ModelLane), u32> = HashMap::new();

    let ((), rendered) = with_fixture_local_recorder(async || {
        assert!(
            resident_admission_allows(
                &empty_model,
                &empty_lane,
                user,
                model,
                role,
                &max_sessions,
                Some(&lane_max_sessions),
            ),
            "an idle user is admitted"
        );

        // Model half falsifies the conjunction on its own.
        let at_model_cap: HashMap<(String, String), u32> =
            [((user.to_owned(), model.to_owned()), 2_u32)]
                .into_iter()
                .collect();
        assert!(
            !resident_admission_allows(
                &at_model_cap,
                &empty_lane,
                user,
                model,
                role,
                &max_sessions,
                Some(&lane_max_sessions),
            ),
            "a user at the per-model cap is denied"
        );

        // Lane half falsifies the conjunction on its own.
        let at_lane_cap: HashMap<(String, ModelLane), u32> =
            [((user.to_owned(), ModelLane::Implement), 1_u32)]
                .into_iter()
                .collect();
        assert!(
            !resident_admission_allows(
                &empty_model,
                &at_lane_cap,
                user,
                model,
                role,
                &max_sessions,
                Some(&lane_max_sessions),
            ),
            "a user at the lane cap is denied even with model headroom"
        );

        // The forwarded primitives agree with the conjunction they compose.
        assert!(!model_under_user_cap(&at_model_cap, user, model, 2));
        assert!(!lane_under_user_cap(
            &at_lane_cap,
            user,
            ModelLane::Implement,
            Some(1)
        ));
        assert!(lane_under_user_cap(
            &at_lane_cap,
            user,
            ModelLane::Implement,
            None
        ));
    })
    .await;

    assert!(
        rendered.contains("djinn_user_cap_utilization")
            && rendered.contains(&format!("user=\"{user}\"")),
        "the forwarder reached the instrumented production cap primitive; \
         fixture-local registry rendered:\n{rendered}"
    );
}

/// Close the fixture database's pool.
///
/// `Database::open_in_memory` clones a template database and holds a real
/// connection pool; the binary runs many scenarios in one process under the
/// normative `cargo test` command, so each scenario returns its connections
/// rather than holding them to process exit.
async fn close(db: Database) {
    db.pool().close().await;
}
