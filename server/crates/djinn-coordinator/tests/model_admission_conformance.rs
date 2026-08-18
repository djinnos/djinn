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

// ═══════════════════════════════════════════════════════════════════════════
// Resident-admission and Kueue non-interference conformance (task `krpy`)
// ═══════════════════════════════════════════════════════════════════════════

/// One row of the resident-admission truth table.
///
/// Every field is an input to `resident_admission_allows`; `expected` is the
/// decision the resident conjunction is required to produce. The table is
/// evaluated once per model-turn admission mode and the three results are
/// compared to each other *and* to `expected`, so neither a drifting mode nor a
/// drifting conjunction can hide behind the other.
struct ResidentCase {
    name: &'static str,
    role: &'static str,
    model_used: u32,
    lane_used: u32,
    /// `None` means the user has no `max_sessions` entry for this model, which
    /// the production conjunction reads as a cap of 1.
    model_cap: Option<u32>,
    lane_caps: Option<LaneMaxSessions>,
    expected: bool,
}

const RESIDENT_USER: &str = "resident-conformance-user";
const RESIDENT_MODEL: &str = "provider/resident-model";

const NO_LANE_LIMIT: Option<LaneMaxSessions> = None;
const IMPLEMENT_ONE: Option<LaneMaxSessions> = Some(LaneMaxSessions {
    plan: 4,
    implement: 1,
    review: 4,
});
const REVIEW_ONE: Option<LaneMaxSessions> = Some(LaneMaxSessions {
    plan: 4,
    implement: 4,
    review: 1,
});

/// The pinned truth table for
/// `max_sessions[model]` (missing ⇒ 1) ∧ `lane_max_sessions[for_role(role)]`.
const RESIDENT_TRUTH_TABLE: &[ResidentCase] = &[
    ResidentCase {
        name: "missing model cap defaults to one: idle user admitted",
        role: "worker",
        model_used: 0,
        lane_used: 0,
        model_cap: None,
        lane_caps: NO_LANE_LIMIT,
        expected: true,
    },
    ResidentCase {
        name: "missing model cap defaults to one: one running session denies",
        role: "worker",
        model_used: 1,
        lane_used: 0,
        model_cap: None,
        lane_caps: NO_LANE_LIMIT,
        expected: false,
    },
    ResidentCase {
        name: "zero model cap is clamped to one: idle user admitted",
        role: "worker",
        model_used: 0,
        lane_used: 0,
        model_cap: Some(0),
        lane_caps: NO_LANE_LIMIT,
        expected: true,
    },
    ResidentCase {
        name: "zero model cap is clamped to one: one running session denies",
        role: "worker",
        model_used: 1,
        lane_used: 0,
        model_cap: Some(0),
        lane_caps: NO_LANE_LIMIT,
        expected: false,
    },
    ResidentCase {
        name: "explicit model cap of two admits the second session",
        role: "worker",
        model_used: 1,
        lane_used: 0,
        model_cap: Some(2),
        lane_caps: NO_LANE_LIMIT,
        expected: true,
    },
    ResidentCase {
        name: "explicit model cap of two denies the third session",
        role: "worker",
        model_used: 2,
        lane_used: 0,
        model_cap: Some(2),
        lane_caps: NO_LANE_LIMIT,
        expected: false,
    },
    ResidentCase {
        name: "absent lane limits impose no lane ceiling at all",
        role: "worker",
        model_used: 0,
        lane_used: 9,
        model_cap: Some(2),
        lane_caps: NO_LANE_LIMIT,
        expected: true,
    },
    ResidentCase {
        name: "worker maps to the implement lane, whose full cap denies",
        role: "worker",
        model_used: 0,
        lane_used: 1,
        model_cap: Some(2),
        lane_caps: IMPLEMENT_ONE,
        expected: false,
    },
    ResidentCase {
        name: "a full review lane does not deny a worker in the implement lane",
        role: "worker",
        model_used: 0,
        lane_used: 1,
        model_cap: Some(2),
        lane_caps: REVIEW_ONE,
        // `lane_used` is charged to the ROLE's lane, so a review ceiling of one
        // is irrelevant to a worker. The implement ceiling here is four.
        expected: true,
    },
    ResidentCase {
        name: "reviewer maps to the review lane, whose full cap denies",
        role: "reviewer",
        model_used: 0,
        lane_used: 1,
        model_cap: Some(2),
        lane_caps: REVIEW_ONE,
        expected: false,
    },
    ResidentCase {
        name: "planner maps to the plan lane, which has room here",
        role: "planner",
        model_used: 0,
        lane_used: 3,
        model_cap: Some(2),
        lane_caps: IMPLEMENT_ONE,
        expected: true,
    },
    ResidentCase {
        name: "both conjuncts full denies",
        role: "worker",
        model_used: 2,
        lane_used: 1,
        model_cap: Some(2),
        lane_caps: IMPLEMENT_ONE,
        expected: false,
    },
];

/// Evaluate the whole table through the production conjunction.
///
/// Returns `(case name, decision)` pairs so a mismatch names the row.
fn evaluate_resident_truth_table() -> Vec<(&'static str, bool)> {
    RESIDENT_TRUTH_TABLE
        .iter()
        .map(|case| {
            let lane = ModelLane::for_role(case.role);
            let running_by_model: HashMap<(String, String), u32> = [(
                (RESIDENT_USER.to_owned(), RESIDENT_MODEL.to_owned()),
                case.model_used,
            )]
            .into_iter()
            .collect();
            let running_by_lane: HashMap<(String, ModelLane), u32> =
                [((RESIDENT_USER.to_owned(), lane), case.lane_used)]
                    .into_iter()
                    .collect();
            let max_sessions: HashMap<String, u32> = case
                .model_cap
                .map(|cap| (RESIDENT_MODEL.to_owned(), cap))
                .into_iter()
                .collect();
            let decision = djinn_coordinator::test_helpers::resident_admission_allows(
                &running_by_model,
                &running_by_lane,
                RESIDENT_USER,
                RESIDENT_MODEL,
                case.role,
                &max_sessions,
                case.lane_caps.as_ref(),
            );
            (case.name, decision)
        })
        .collect()
}

/// The resident conjunction is unchanged by the model-turn admission mode.
///
/// The three modes are **durable pool rows**, not enum literals: each is written
/// through `set_model_turn_phase_fixture` and then proven real by driving the
/// production `acquire_turn` against it and counting the `model_turn_leases`
/// rows it wrote — `off` and `shadow` write none, `enforce` writes exactly one.
/// So the mode genuinely differs at the model-turn boundary while the resident
/// truth table stays byte-identical, which is the non-interference claim.
///
/// A test that only evaluated the table three times would prove nothing: the
/// conjunction does not read the pool, so all three would agree even if the
/// modes had never been applied.
#[tokio::test]
async fn scenario_07_resident_conjunction_is_identical_across_admission_modes() {
    let db = djinn_coordinator::test_helpers::create_test_db();
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    let pool_id = seed_model_turn_admission_fixture(&db, "off", "supported", 2).await;

    let expected: Vec<(&'static str, bool)> = RESIDENT_TRUTH_TABLE
        .iter()
        .map(|case| (case.name, case.expected))
        .collect();

    // (mode, the model-turn outcome is an admission, expected durable lease rows)
    let modes: [(&str, bool, i64); 3] = [
        ("off", false, 0),
        ("shadow", false, 0),
        ("enforce", true, 1),
    ];
    let mut tables: Vec<(&str, Vec<(&'static str, bool)>)> = Vec::new();

    for (mode, admits, expected_leases) in modes {
        djinn_db::test_support::set_model_turn_phase_fixture(&db, pool_id, mode).await;
        let outcome = repository
            .acquire_turn(ModelTurnAcquireInput {
                pool_id,
                request_id: format!("conformance-mode-{mode}"),
                owner_pod_uid: Some(format!("pod-conformance-mode-{mode}")),
                generation: 1,
                debits: request_debit(1),
            })
            .await
            .expect("acquisition must not error in any mode");
        assert_eq!(
            matches!(outcome, ModelTurnAcquireOutcome::Admitted { .. }),
            admits,
            "mode {mode} produced the wrong model-turn outcome: {outcome:?}"
        );
        assert_eq!(
            model_turn_lease_count_for_pool_fixture(&db, pool_id).await,
            expected_leases,
            "mode {mode} must leave exactly {expected_leases} durable lease row(s)"
        );

        tables.push((mode, evaluate_resident_truth_table()));
    }

    for (mode, table) in &tables {
        assert_eq!(
            table, &expected,
            "the resident truth table changed under model-turn mode {mode}"
        );
    }
    let (first_mode, first_table) = &tables[0];
    for (mode, table) in &tables[1..] {
        assert_eq!(
            table, first_table,
            "mode {mode} produced a different resident truth table than {first_mode}"
        );
    }

    close(db).await;
}

/// Every role→lane mapping the dispatch path can ask for, pinned.
const ROLE_LANE_MAPPING: &[(&str, ModelLane)] = &[
    ("worker", ModelLane::Implement),
    ("reviewer", ModelLane::Review),
    ("planner", ModelLane::Plan),
    ("architect", ModelLane::Plan),
    ("chat", ModelLane::Plan),
    ("lead", ModelLane::Plan),
    ("arbiter", ModelLane::Plan),
    ("adversary", ModelLane::Plan),
    ("judge", ModelLane::Plan),
    ("", ModelLane::Plan),
    ("a-role-that-does-not-exist", ModelLane::Plan),
];

const ALL_LANES: [ModelLane; 3] = [ModelLane::Plan, ModelLane::Implement, ModelLane::Review];

/// `ModelLane::for_role` is pinned, and the mapping is load-bearing at the
/// admission boundary.
///
/// Two halves, because either alone is weak. The direct half pins the function.
/// The second half drives the same role through the production resident
/// conjunction with the *mapped* lane full and with every other lane full in
/// turn: a role whose mapping changed is then admitted where it must be denied,
/// or denied where it must be admitted. So a mapping change fails even if
/// somebody updated the pinned table above to match it.
#[test]
fn model_lane_role_mapping_is_pinned() {
    use djinn_coordinator::test_helpers::resident_admission_allows;

    for (role, lane) in ROLE_LANE_MAPPING {
        assert_eq!(
            ModelLane::for_role(role),
            *lane,
            "role {role:?} must map to {lane:?}"
        );
    }
    let mapped: Vec<ModelLane> = ROLE_LANE_MAPPING.iter().map(|(_, lane)| *lane).collect();
    for lane in ALL_LANES {
        assert!(
            mapped.contains(&lane),
            "the pinned table must exercise every lane; {lane:?} is unreached"
        );
    }

    // The model half always has room, so only the lane conjunct can decide.
    let max_sessions: HashMap<String, u32> =
        [(RESIDENT_MODEL.to_owned(), 10_u32)].into_iter().collect();
    let one_everywhere = LaneMaxSessions {
        plan: 1,
        implement: 1,
        review: 1,
    };
    let empty_model: HashMap<(String, String), u32> = HashMap::new();

    for (role, lane) in ROLE_LANE_MAPPING {
        for candidate in ALL_LANES {
            let running_by_lane: HashMap<(String, ModelLane), u32> =
                [((RESIDENT_USER.to_owned(), candidate), 1_u32)]
                    .into_iter()
                    .collect();
            let allowed = resident_admission_allows(
                &empty_model,
                &running_by_lane,
                RESIDENT_USER,
                RESIDENT_MODEL,
                role,
                &max_sessions,
                Some(&one_everywhere),
            );
            assert_eq!(
                allowed,
                candidate != *lane,
                "role {role:?} charged its session to {candidate:?}; it must be charged to {lane:?}"
            );
        }
    }
}

// ─── Reference test: no second resident authority was created ─────────────
//
// The epic's original wording was "no `resident_session_cap`, adaptive cluster
// budget, boot reservation, or application Kueue quota ledger is created or
// consulted". That is a universal negative over the whole codebase and no test
// run can establish it — a passing run means "I did not find one", which is
// exactly what a vacuous search returns too.
//
// What IS checkable is the *reachable* surface. `model_under_user_cap` and
// `lane_under_user_cap` are `pub(crate)`, so this crate's own source tree is
// the complete set of places that can call them; the only way out of the crate
// is the `test_helpers` forwarders, which exist under `cfg(test)` or the
// `test-support` feature and never in a production build. Pinning the exact
// per-file call-site census therefore turns "a second authority appeared" into
// a failing assertion with a diff, and the `acquire_turn` rule pins the
// direction of the dependency: dispatch decides residency, model-turn admission
// is downstream and is never reached from `src/dispatch/`.

/// Exact per-file call-site census for `model_under_user_cap`.
///
/// A *call site* is an occurrence of `model_under_user_cap(`; the definition
/// line (`fn model_under_user_cap(`) is excluded, and `use` re-exports have no
/// parenthesis so they never count.
const MODEL_CAP_CALL_SITES: &[(&str, usize)] = &[
    ("src/dispatch/task_dispatch.rs", 5),
    ("src/refinement_cap_tests.rs", 5),
    ("src/test_helpers.rs", 1),
];

/// Exact per-file call-site census for `lane_under_user_cap`.
const LANE_CAP_CALL_SITES: &[(&str, usize)] = &[
    ("src/dispatch/task_dispatch.rs", 9),
    ("src/test_helpers.rs", 1),
];

/// Files that must exist for the scan to be meaningful. A walker that silently
/// found nothing would otherwise satisfy every census above by returning an
/// empty map.
const SCAN_SENTINELS: &[&str] = &[
    "src/dispatch/admission.rs",
    "src/dispatch/task_dispatch.rs",
    "src/test_helpers.rs",
];

/// Every `.rs` path under the crate's `src/`, relative to the manifest dir,
/// with forward slashes.
fn crate_source_files() -> Vec<String> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
        let entries = std::fs::read_dir(dir)
            .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()));
        for entry in entries {
            let entry = entry.expect("directory entry");
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let relative = path
                    .strip_prefix(root)
                    .expect("path is under the manifest dir");
                out.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut out = Vec::new();
    walk(&root.join("src"), root, &mut out);
    out.sort();
    out
}

fn read_crate_source(relative: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

/// Count call sites of `name` in `source`, excluding its own definition line.
fn count_call_sites(source: &str, name: &str) -> usize {
    let definition = format!("fn {name}");
    source
        .lines()
        .filter(|line| !line.contains(&definition))
        .map(|line| line.matches(&format!("{name}(")).count())
        .sum()
}

fn census(files: &[String], name: &str) -> Vec<(String, usize)> {
    let mut found: Vec<(String, usize)> = files
        .iter()
        .filter_map(|relative| {
            let count = count_call_sites(&read_crate_source(relative), name);
            (count > 0).then(|| (relative.clone(), count))
        })
        .collect();
    found.sort();
    found
}

/// The resident-cap primitives have exactly the call sites listed, and nothing
/// under `src/dispatch/` reaches for model-turn acquisition.
#[test]
fn resident_admission_call_sites_are_pinned() {
    let files = crate_source_files();
    assert!(
        files.len() > 50,
        "the source scan found only {} files; the walker is broken",
        files.len()
    );
    for sentinel in SCAN_SENTINELS {
        assert!(
            files.iter().any(|path| path == sentinel),
            "the source scan missed {sentinel}; the walker is broken"
        );
    }

    for (name, pinned) in [
        ("model_under_user_cap", MODEL_CAP_CALL_SITES),
        ("lane_under_user_cap", LANE_CAP_CALL_SITES),
    ] {
        let expected: Vec<(String, usize)> = pinned
            .iter()
            .map(|(path, count)| ((*path).to_owned(), *count))
            .collect();
        assert_eq!(
            census(&files, name),
            expected,
            "the {name} call-site census changed. A new caller is a second \
             resident authority until proven otherwise: justify it, then update \
             MODEL_CAP_CALL_SITES / LANE_CAP_CALL_SITES in this file."
        );
    }

    // Both primitives are only reachable from inside this crate, so the census
    // above is complete. Prove that rather than asserting it.
    for (relative, source) in ["src/dispatch/admission.rs"]
        .iter()
        .map(|path| (*path, read_crate_source(path)))
    {
        for name in ["model_under_user_cap", "lane_under_user_cap"] {
            assert!(
                source.contains(&format!("pub(crate) fn {name}")),
                "{name} must stay pub(crate) in {relative}; a wider visibility \
                 would put callers outside this crate's source tree, where the \
                 census above cannot see them"
            );
        }
    }

    let dispatch_referencing_acquire_turn: Vec<&String> = files
        .iter()
        .filter(|relative| relative.starts_with("src/dispatch/"))
        .filter(|relative| read_crate_source(relative).contains("acquire_turn"))
        .collect();
    assert!(
        dispatch_referencing_acquire_turn.is_empty(),
        "src/dispatch/ must never reach for model-turn acquisition; found it in \
         {dispatch_referencing_acquire_turn:?}. Resident admission is the outer \
         boundary and model-turn admission is downstream of it."
    );
}

// ─── Kueue non-interference ───────────────────────────────────────────────

/// A workload Kueue has queued but not admitted writes no model-turn lease and
/// gets no replacement dispatch.
///
/// The mechanism, and why each assertion is the one that can catch a
/// regression:
///
/// * `model_turn_leases` rows are written by exactly one production caller,
///   `djinn_slot::reply_loop::model_turn_admission`'s `acquire_turn`, which runs
///   inside the task-run Pod. A Kueue-pending Job is created `suspend: true` and
///   has no Pod, so that boundary is never reached. Asserted as a **row count**
///   over the whole relation, not as a log line or a returned enum.
/// * The coordinator's protection against dispatching a *second* workload for
///   the same task while the first is queued is the respawn guard's
///   non-terminal-attempt rule. Asserted as: no new `task_runs` row, no
///   `sessions` row, and the actor's own `dispatched` counter still zero.
///
/// Non-vacuity: `dispatched == 0` is also what a pass that never saw the task
/// would produce. So the test additionally asserts that the pass **reached the
/// guard and deferred** — a `deferred` guard attempt row appears where there was
/// none. Terminalising the pending attempt and re-running produces no second
/// deferral, which pins that row to the Kueue-pending state rather than to the
/// pass merely having run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kueue_pending_workload_writes_no_lease_and_gets_no_replacement_dispatch() {
    use djinn_core::models::task_attempt::TaskAttemptOutcome;
    use djinn_db::{
        CreateTaskAttemptParams, CreateTaskRunParams, KueueWorkloadAdmissionRepository,
        SessionRepository, TaskAttemptRepository, TaskRepository, TaskRunRepository,
        UserRepository,
    };

    install_github_app_config_for_dispatch();
    let db = djinn_coordinator::test_helpers::create_test_db();
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel(256);
    let events = djinn_core::events::EventBus::noop();

    let project = djinn_coordinator::test_helpers::create_test_project(&db).await;
    let github_id = 700_000
        + i64::try_from(uuid::Uuid::now_v7().as_u128() % 1_000_000).expect("bounded github id");
    let user = UserRepository::new(db.clone())
        .upsert_from_github(
            github_id,
            &format!("kueue-conformance-{}", uuid::Uuid::now_v7()),
            None,
            None,
        )
        .await
        .expect("seed task creator");

    let tasks = TaskRepository::new(db.clone(), events.clone());
    let task = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user.id.clone()), async {
            tasks
                .create_fixture_in_project(
                    &project.id,
                    None,
                    "Kueue-pending conformance task",
                    "A ready task whose build Job is queued by Kueue.",
                    "",
                    "task",
                    2,
                    "test-owner",
                    Some("approved"),
                    None,
                )
                .await
                .expect("seed ready worker task")
        })
        .await;
    let task = tasks
        .set_status(&task.id, "open")
        .await
        .expect("make the task dispatch-ready");

    // The dispatch that produced the queued Job: a live `pending` attempt, and a
    // task-run whose Kueue Workload the reflector observed as `pending`.
    let attempt_id = uuid::Uuid::now_v7().to_string();
    let attempts = TaskAttemptRepository::new(db.clone());
    attempts
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &attempt_id,
            task_id: &task.id,
            role: "worker",
            dispatch_key: &format!("kueue-conformance-{}", uuid::Uuid::now_v7()),
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .expect("record the dispatch that created the queued Job");
    let task_run_id = uuid::Uuid::now_v7().to_string();
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: &task_run_id,
            project_id: &project.id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("pending"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .expect("create the task run the queued Job accounts for");
    KueueWorkloadAdmissionRepository::new(db.clone())
        .apply(
            &task_run_id,
            "pending",
            Some("QuotaReserved"),
            Some("djinn-taskrun-conformance"),
        )
        .await
        .expect("project the queued Kueue Workload");
    let queued = KueueWorkloadAdmissionRepository::new(db.clone())
        .get(&task_run_id)
        .await
        .expect("read the Kueue admission projection")
        .expect("the projection must hold the queued workload");
    assert_eq!(
        queued.admission, "pending",
        "the fixture must leave the workload queued, not admitted"
    );

    let pool_id = seed_model_turn_admission_fixture(&db, "enforce", "supported", 2).await;
    assert_eq!(
        model_turn_lease_total_count_fixture(&db).await,
        0,
        "no lease exists before the dispatch pass"
    );

    let guard_deferrals_before = guard_deferral_count(&attempts, &task.id).await;
    let (mut actor, cancel) =
        djinn_coordinator::test_helpers::make_coordinator_actor_cancellable(&db, &events_tx);
    djinn_coordinator::test_helpers::run_dispatch_ready_tasks(&mut actor, Some(&project.id)).await;

    assert_eq!(
        djinn_coordinator::test_helpers::dispatched_count(&actor),
        0,
        "a task whose workload is queued by Kueue must not be dispatched again"
    );
    assert_eq!(
        model_turn_lease_total_count_fixture(&db).await,
        0,
        "a Kueue-pending workload never reaches the provider-launch boundary, \
         so it must write no model_turn_leases row"
    );
    assert_eq!(
        model_turn_lease_count_for_pool_fixture(&db, pool_id).await,
        0,
        "and none against the enforce-phase pool in particular"
    );
    assert_eq!(
        task_run_ids_for_task(&db, &task.id).await,
        vec![task_run_id.clone()],
        "no replacement task run was created for the queued workload"
    );
    assert_eq!(
        SessionRepository::new(db.clone(), events.clone())
            .list_for_task(&task.id)
            .await
            .expect("list sessions for the task")
            .len(),
        0,
        "no replacement session was started for the queued workload"
    );

    // Non-vacuity: the pass reached the respawn guard and deferred there.
    let guard_deferrals_after = guard_deferral_count(&attempts, &task.id).await;
    assert_eq!(
        guard_deferrals_after,
        guard_deferrals_before + 1,
        "the dispatch pass must have reached the respawn guard and recorded a \
         deferral; without this, `dispatched == 0` could equally mean the pass \
         never saw the task"
    );

    // And the deferral is bound to the live attempt, not to the pass running:
    // terminalise it and the next pass records no second deferral.
    attempts
        .advance_to_terminal(djinn_db::TerminalTaskAttemptParams {
            id: &attempt_id,
            outcome: TaskAttemptOutcome::Cancelled,
            summary: Some("conformance fixture: release the queued attempt"),
            summary_json: None,
            log_tail: None,
            checkpoint_ref: None,
            submit_ref: None,
            pr_url: None,
            mirror_head_sha: None,
            github_head_sha: None,
        })
        .await
        .expect("terminalise the queued attempt");
    djinn_coordinator::test_helpers::run_dispatch_ready_tasks(&mut actor, Some(&project.id)).await;
    assert_eq!(
        guard_deferral_count(&attempts, &task.id).await,
        guard_deferrals_after,
        "with no live attempt the guard must not defer; the deferral above was \
         caused by the queued workload's attempt, not by the pass"
    );

    // Positive control on the instrument: the relation and the counter above
    // would have registered a lease had one been written. Without this, "zero
    // lease rows" is a claim no failure could ever contradict.
    let admitted = ModelTurnAdmissionRepository::new(db.clone())
        .acquire_turn(ModelTurnAcquireInput {
            pool_id,
            request_id: "conformance-kueue-instrument-control".to_owned(),
            owner_pod_uid: Some("pod-conformance-kueue-control".to_owned()),
            generation: 1,
            debits: request_debit(1),
        })
        .await
        .expect("control acquisition must not error");
    assert!(
        matches!(admitted, ModelTurnAcquireOutcome::Admitted { .. }),
        "the enforce pool the assertions above were made against must be able to          admit; got {admitted:?}"
    );
    assert_eq!(
        model_turn_lease_count_for_pool_fixture(&db, pool_id).await,
        1,
        "the lease counter registers a real acquisition, so the zeroes above are          observations rather than a blind spot"
    );

    cancel.cancel();
    close(db).await;
}

/// Install a GitHub App credential snapshot so the ready pass gets past its
/// first gate.
///
/// `dispatch_ready_tasks` refuses to dispatch anything at all when no App is
/// configured (ADR-039: PR creation needs installation tokens). That gate is
/// *not* stubbed out for integration targets — only for `cfg(test)` unit tests —
/// so without this the pass returns before it looks at a single task, and every
/// "nothing was dispatched" assertion below would pass vacuously.
///
/// This installs the same process-wide snapshot `AppState::init_app_config`
/// installs at server boot, through the production entry point. It is
/// idempotent and value-stable, so the normative single-process `cargo test`
/// run cannot observe it changing under a sibling scenario.
fn install_github_app_config_for_dispatch() {
    static INSTALLED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INSTALLED.get_or_init(|| {
        djinn_provider::github_app::install_runtime_config(std::sync::Arc::new(
            djinn_provider::github_app::AppConfig {
                app_id: 4_242,
                slug: "djinn-conformance".to_owned(),
                client_id: "Iv1.conformance".to_owned(),
                client_secret: "conformance-secret".to_owned(),
                pem: String::new(),
                webhook_secret: "conformance-webhook".to_owned(),
                public_url: "https://conformance.invalid".to_owned(),
            },
        ));
    });
}

/// Respawn-guard deferral rows recorded for `task_id`.
///
/// Matched on the production enums rather than string literals, so renaming a
/// variant is a compile error here instead of a silently-zero count.
async fn guard_deferral_count(attempts: &djinn_db::TaskAttemptRepository, task_id: &str) -> usize {
    use djinn_core::models::task_attempt::{GuardDecision, GuardReason, TaskAttemptOutcome};

    attempts
        .list_for_task(task_id)
        .await
        .expect("list task attempts")
        .iter()
        .filter(|attempt| {
            attempt.outcome == TaskAttemptOutcome::Deferred.as_str()
                && attempt.guard_decision.as_deref() == Some(GuardDecision::Defer.as_str())
                && attempt.guard_reason.as_deref() == Some(GuardReason::RespawnGuard.as_str())
        })
        .count()
}

/// Task-run ids recorded for `task_id`, in creation order.
async fn task_run_ids_for_task(db: &Database, task_id: &str) -> Vec<String> {
    djinn_db::TaskRunRepository::new(db.clone())
        .list_for_task(task_id)
        .await
        .expect("list task runs")
        .into_iter()
        .map(|run| run.id)
        .collect()
}

// ─── Phase D: bounded telemetry with a closed label allow-list (75iz) ──────

/// The whole model-turn telemetry surface, driven through production and
/// compared against the allow-list constant.
///
/// The claim being settled is a *closed shape*, not a sampled absence. The
/// original acceptance wording ("no secrets or raw credential/account/project/
/// user/request/lease IDs") is a universal negative over every emission the
/// process will ever make, and a test run can only sample. So this scenario
/// asserts set equality against
/// [`djinn_telemetry::model_turn_metrics::expected_label_triples`], which is
/// derived from `MODEL_TURN_SERIES` and nothing else; the redaction claim
/// itself is pattern-checked against that constant in `djinn-telemetry`.
///
/// Every series is produced by the production path that owns it:
///
/// * pool target, in-flight, reservation divergence, aggregate output rate,
///   identity eligibility and protocol coverage — the leader enforcement pass's
///   own emission seam;
/// * the four throttle classes — `ModelTurnAdmissionCoordinator::prepare`, the
///   slot boundary, once per bucket kind;
/// * per-stream output rate and time-to-first-token —
///   `ModelTurnAdmissionCoordinator::reconcile`, from the attempt's injected
///   clocks;
/// * both expiry dispositions — the leader's persisted-timestamp lease reaper.
///
/// **The `enforce` mode here is an explicitly seeded fixture.** No production
/// pool can legitimately reach `enforce` today: Phase B stored a capability
/// *instant* rather than a coverage interval and no authoritative-usage column,
/// so `qualify_aligned_phase_c_window_v1` reports `PartialCapabilityCoverage`/
/// `MissingUsage` for every real window and the Phase-D guard denies the
/// advance. Seeding the mode is how this scenario reaches the slot boundary
/// without widening the heartbeat or defaulting the usage — either of which
/// would forge the coverage the enforcement decision rests on.
#[tokio::test]
async fn phase_d_bounded_telemetry_matches_the_allow_list_exactly() {
    use djinn_db::{ModelTurnAuthoritativeUsage, ModelTurnLeaseIdentity};
    use djinn_provider::{
        ProviderAbortCapabilityV1, ProviderAdmissionPolicyV1, ProviderAttemptAbortHandleV1,
        ProviderAttemptAbortResultV1, ProviderAttemptCapabilitiesV1, ProviderAttemptPlanV1,
        ProviderAttemptRouteCoverageV1, ProviderAttemptScopeV1, ProviderAttemptTerminalV1,
        ProviderCredentialRecordScopeV1, ProviderHiddenRetryCapabilityV1, ProviderOutcomeV1,
        ProviderOutputReservationSourceV1, ProviderTokenEmissionV1,
    };
    use djinn_slot::reply_loop::model_turn_admission::{
        ModelTurnAdmissionCoordinator, ModelTurnAdmissionRequest, ModelTurnPreparation,
    };
    use djinn_telemetry::model_turn_metrics::{expected_label_triples, model_turn_label_triples};

    let db = djinn_coordinator::test_helpers::create_test_db();
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    let pool_id = seed_model_turn_admission_fixture(&db, "enforce", "supported", 2).await;
    // The fixture pins the learned target at 1; this scenario keeps two turns
    // in flight at once for the two expiry dispositions.
    repository
        .set_pool_learned_concurrency_for_test(pool_id, 4)
        .await
        .expect("raise the learned target");

    // The active catalog must resolve the fixture route, or nothing is emitted
    // at all — that is the point of the qualification, and it is asserted
    // separately in `djinn-telemetry`.
    let catalog = djinn_provider::catalog::CatalogService::new();
    catalog.add_custom_provider(
        djinn_core::models::Provider {
            id: FIXTURE_PROVIDER_ID.into(),
            name: "Conformance Provider".into(),
            npm: String::new(),
            env_vars: vec!["CONFORMANCE_API_KEY".into()],
            base_url: "https://example.invalid/v1".into(),
            docs_url: String::new(),
            is_openai_compatible: true,
        },
        vec![djinn_core::models::Model {
            id: FIXTURE_MODEL_ID.into(),
            provider_id: FIXTURE_PROVIDER_ID.into(),
            name: "Conformance Model".into(),
            tool_call: false,
            reasoning: false,
            attachment: false,
            context_window: 1,
            output_limit: 1,
            pricing: djinn_core::models::Pricing::default(),
        }],
    );

    const BUCKETS: [ModelTurnBucketKind; 4] = [
        ModelTurnBucketKind::Request,
        ModelTurnBucketKind::Input,
        ModelTurnBucketKind::Output,
        ModelTurnBucketKind::Combined,
    ];
    // Every bucket kind exists and is exhausted, so each `prepare` defers on
    // exactly the bucket it asked for.
    for kind in BUCKETS {
        repository
            .seed_bucket_binding_for_test(pool_id, kind, 8, 0)
            .await
            .expect("seed exhausted binding");
    }
    // One durable usage observation, so the aggregate-rate gauge divides a real
    // number rather than the zero it would also produce by doing nothing.
    repository
        .seed_output_observation_for_test(pool_id, 1, 600)
        .await
        .expect("seed observation");

    let plan = |kind: ModelTurnBucketKind, units: i64| ProviderAttemptPlanV1 {
        scope: ProviderAttemptScopeV1 {
            credential: ProviderCredentialRecordScopeV1::from_credential_record_id(
                FIXTURE_CREDENTIAL_ID,
            ),
            provider_id: FIXTURE_PROVIDER_ID.to_owned(),
            model_id: FIXTURE_MODEL_ID.to_owned(),
        },
        coverage: ProviderAttemptRouteCoverageV1::Covered {
            capabilities: ProviderAttemptCapabilitiesV1 {
                hidden_retries: ProviderHiddenRetryCapabilityV1::Disabled,
                abort: ProviderAbortCapabilityV1::Supported,
            },
            supported_bucket_bindings: BUCKETS.to_vec(),
            policy: ProviderAdmissionPolicyV1::Proactive,
        },
        debits: vec![ModelTurnBucketDebit {
            bucket_kind: kind,
            units,
        }],
        output_reservation_source: ProviderOutputReservationSourceV1::ExplicitLimit,
        abort: ProviderAttemptAbortHandleV1::new(),
    };

    let ((), rendered) = with_fixture_local_recorder(async || {
        let coordinator =
            ModelTurnAdmissionCoordinator::new(repository.clone()).with_catalog(catalog.clone());

        // ── Four throttle classes, one per bucket kind ────────────────────
        for (index, kind) in BUCKETS.into_iter().enumerate() {
            let preparation = coordinator
                .prepare(
                    &plan(kind, 1),
                    ModelTurnAdmissionRequest {
                        credential_id: FIXTURE_CREDENTIAL_ID.to_owned(),
                        request_id: format!("conformance-throttle-{index}"),
                        owner_pod_uid: None,
                        generation: 1,
                    },
                )
                .await
                .expect("prepare must not error");
            assert!(
                matches!(preparation, ModelTurnPreparation::Wait(_)),
                "an exhausted {kind:?} bucket must defer, got {preparation:?}"
            );
        }

        // ── One settled stream: TTFT and per-stream output rate ───────────
        repository
            .seed_bucket_binding_for_test(pool_id, ModelTurnBucketKind::Request, 8, 8)
            .await
            .expect("restore the request binding");
        let ModelTurnPreparation::Permit(mut permit) = coordinator
            .prepare(
                &plan(ModelTurnBucketKind::Request, 1),
                ModelTurnAdmissionRequest {
                    credential_id: FIXTURE_CREDENTIAL_ID.to_owned(),
                    request_id: "conformance-stream".to_owned(),
                    owner_pod_uid: Some("pod-conformance-stream".to_owned()),
                    generation: 1,
                },
            )
            .await
            .expect("prepare must not error")
        else {
            panic!("a pool with capacity must issue a send permit");
        };
        permit
            .mark_active()
            .await
            .expect("hand off to the provider");
        let identity = permit
            .lease
            .clone()
            .expect("an enforced permit owns a lease");
        coordinator
            .reconcile(
                identity,
                &ProviderOutcomeV1 {
                    terminal: ProviderAttemptTerminalV1::Completed,
                    authoritative_usage: Some(ModelTurnAuthoritativeUsage {
                        request_units: 1,
                        input_units: 10,
                        output_units: 40,
                        combined_units: 50,
                    }),
                    observation: None,
                    abort: ProviderAttemptAbortResultV1::NotRequested,
                    token_emission: ProviderTokenEmissionV1 {
                        attempt_started_monotonic_ms: Some(1_000),
                        first_token_monotonic_ms: Some(1_250),
                        last_token_monotonic_ms: Some(3_250),
                    },
                },
            )
            .await
            .expect("reconcile must not error");
        drop(permit);
        coordinator.wait_for_cleanup().await;

        // ── Both expiry dispositions, through the leader reaper ───────────
        //
        // Acquisition goes straight through the repository here, so the
        // refunded case keeps the `reserved` lifecycle the disposition is
        // defined by; the quarantined case is walked to `active` through the
        // same production transitions the slot uses.
        let mut identities: Vec<ModelTurnLeaseIdentity> = Vec::new();
        for (request_id, activate) in [
            ("conformance-expiry-refunded", false),
            ("conformance-expiry-quarantined", true),
        ] {
            let outcome = repository
                .acquire_turn(ModelTurnAcquireInput {
                    pool_id,
                    request_id: request_id.to_owned(),
                    owner_pod_uid: None,
                    generation: 1,
                    debits: request_debit(1),
                })
                .await
                .expect("acquire must not error");
            let ModelTurnAcquireOutcome::Admitted { lease, .. } = outcome else {
                panic!("the pool still has capacity, got {outcome:?}");
            };
            if activate {
                repository
                    .mark_dispatching(&lease.identity)
                    .await
                    .expect("dispatch");
                repository
                    .mark_active(&lease.identity)
                    .await
                    .expect("activate");
            }
            repository
                .backdate_lease_for_test(&lease.identity, "1970-01-01T00:00:00Z", None)
                .await
                .expect("backdate the only clock the reaper reads");
            identities.push(lease.identity);
        }
        assert_eq!(identities.len(), 2);
        let reaped =
            djinn_coordinator::model_turn_admission::controller::reap_stale_model_turn_leases_v1(
                &repository,
                "1970-01-01T00:10:00Z",
                64,
                Some(&catalog),
            )
            .await
            .expect("reaper pass");
        assert_eq!(
            reaped.expired, 2,
            "both stale leases must actually be expired, not merely observed"
        );

        // ── The six pool-scoped series, through the leader pass ───────────
        let pool = repository
            .pool_by_id(pool_id)
            .await
            .expect("read pool")
            .expect("the fixture pool exists");
        assert!(
            djinn_coordinator::model_turn_admission::enforcement::emit_pool_series_v1(
                &repository,
                &catalog,
                &pool,
                "2024-01-01T00:01:00Z",
            )
            .await
            .expect("pool telemetry"),
            "a catalog-resolved pool must emit its series"
        );
    })
    .await;

    let triples = model_turn_label_triples(&rendered);
    assert_eq!(
        triples,
        expected_label_triples(pool_id, FIXTURE_PROVIDER_ID, FIXTURE_MODEL_ID),
        "the emitted (metric, label_key, label_value) set must equal the allow-list"
    );
}

/// Let the runtime make real progress without moving the paused clock.
///
/// `tokio::time::pause()` auto-advances the clock to the next timer deadline
/// whenever the runtime parks with nothing to run, which would jump straight
/// past the boundaries the watchdog scenario measures. Keeping one task
/// permanently runnable stops the runtime from parking, while the scheduler's
/// own periodic driver poll still completes the real database round trips the
/// watchdog performs. The budget is measured with `std::time::Instant`, which
/// `pause()` does not touch.
async fn settle_without_advancing_virtual_time(budget: std::time::Duration) {
    let started = std::time::Instant::now();
    while started.elapsed() < budget {
        tokio::task::yield_now().await;
    }
}

/// The 20-second heartbeat cadence and the 40-second abort deadline, proven as
/// behaviour under paused time.
///
/// Proposal `96fy`'s first criterion asks this target to *prove* both timers.
/// It used to discharge them with `source.contains("Duration::from_secs(20)")`
/// against another crate's source text — an assertion that survives moving the
/// interval out of the select, making the heartbeat conditional, or deleting
/// the watchdog spawn and leaving the constants in dead code. The behaviour was
/// genuinely proven, but in `djinn-slot`'s own unit tests, which the fixed
/// command does not run: exactly the substitute target the eleventh criterion
/// forbids.
///
/// Task `kcso` lifted the loop out of
/// `CoveredAttemptTerminalGuard::start_watchdog` into `run_turn_watchdog_v1`
/// with no behaviour change, so the production loop runs here directly against
/// a real repository and two real leases. Nothing is faked: a committed
/// heartbeat is an `UPDATE model_turn_leases SET heartbeat_at` observed in the
/// row, and a refused one is the production generation fence.
///
/// **Cadence — a two-sided bound.** A lease that has never heartbeat carries
/// `heartbeat_at IS NULL`. At t=19 it is still null; just past t=20 it is not.
/// A shorter interval fails the first assertion and a longer one fails the
/// second, so the constant is pinned from both directions rather than matched
/// as text.
///
/// **Deadline — also two-sided.** The second watchdog is handed an identity
/// whose generation the lease does not own, so every heartbeat it presents is
/// `Fenced` and its last *committed* heartbeat stays at t≈0. Its ticks fall at
/// t=20 and t=40. At t=20 the 40-second deadline has not passed, so the tick
/// heartbeats (and is refused) rather than aborting; just past t=40 it has, and
/// the attempt is aborted. A deadline shorter than one tick makes the *first*
/// tick abort instead of heartbeating, which reddens the cadence assertion
/// above; a deadline of 41 seconds leaves the t=40 tick inside the deadline, so
/// nothing aborts and the last assertion reddens. Both directions were
/// confirmed by mutation, not predicted.
///
/// **Partition, abort, quarantine, replacement.** The abort is what a
/// partitioned slot does on its own: it stops sending. It releases nothing, and
/// it must not — the attempt may already have reached the provider. The rest of
/// the chronology is asserted after it: while the aborted lease is still in
/// flight a second slot gets `Wait(Concurrency { target: 1, in_flight: 1 })`
/// rather than a second dispatch; the leader-side reaper then terminalises it
/// from the persisted timestamps alone at the 90-second boundary, recording
/// `expired`/`quarantined` because it may have been sent; and only then does the
/// *same* refused request dispatch. This clause was proven in `djinn-slot`'s
/// `two_slots_share_target_one_and_quarantine_after_partitioned_watchdog_abort`
/// before task `kcso` — the second substitute target the eleventh criterion
/// forbids.
///
/// **Why the millisecond nudges.** Tokio's timer wheel fires an entry when the
/// clock moves strictly *past* its deadline, so an advance that lands exactly
/// on a deadline does not fire it. Each boundary below is therefore crossed by
/// a couple of milliseconds. The nudge before the first assertion is what makes
/// each `interval`'s immediate first tick complete at t≈0, which is where both
/// watchdogs stamp the instant their deadlines are measured from.
#[tokio::test]
async fn the_turn_watchdog_commits_every_twenty_seconds_and_aborts_after_forty() {
    use djinn_db::{ModelTurnLeaseIdentity, ModelTurnLeaseMutationOutcome};
    use djinn_slot::reply_loop::model_turn_admission::ModelTurnAdmissionCoordinator;
    use djinn_slot::reply_loop::streaming::run_turn_watchdog_v1;

    /// Two milliseconds: enough to cross a deadline, far too little to reach
    /// the next one twenty seconds away.
    const NUDGE: std::time::Duration = std::time::Duration::from_millis(2);
    /// Real-time budget given to each *negative* observation: how long the
    /// watchdog is offered to misbehave before the assertion that it did not.
    /// Longer is strictly stricter.
    const SETTLE: std::time::Duration = std::time::Duration::from_millis(500);
    /// Real-time cap on each *positive* observation. A loaded runner makes a
    /// real database round trip take arbitrarily long in real time while the
    /// virtual clock stands still, so these wait on the condition, not a
    /// budget; the cap only stops the test hanging if the condition never
    /// arrives.
    const PATIENCE: std::time::Duration = std::time::Duration::from_secs(15);

    let db = djinn_coordinator::test_helpers::create_test_db();
    let repository = ModelTurnAdmissionRepository::new(db.clone());

    /// Acquire one real lease against a fresh enforcing pool and take it
    /// through the production pre-send fence, so each watchdog below is
    /// heartbeating a lease that genuinely exists and is genuinely in flight.
    async fn dispatched_lease(
        db: &Database,
        repository: &ModelTurnAdmissionRepository,
        credential: &str,
    ) -> (i64, ModelTurnLeaseIdentity) {
        let pool_id = cfni_seed_pool(db, credential, "enforce").await;
        repository
            .seed_request_bucket_binding_for_test(pool_id, 4, 4)
            .await
            .expect("seed the request binding");
        let ModelTurnAcquireOutcome::Admitted { lease, .. } = repository
            .acquire_turn(ModelTurnAcquireInput {
                pool_id,
                request_id: format!("{credential}-request"),
                owner_pod_uid: Some("pod-cfni-watchdog".to_owned()),
                generation: 1,
                debits: request_debit(1),
            })
            .await
            .expect("acquisition must not error")
        else {
            panic!("the seeded pool must admit, or the watchdog has no lease");
        };
        assert_eq!(
            repository
                .mark_dispatching(&lease.identity)
                .await
                .expect("mark dispatching"),
            ModelTurnLeaseMutationOutcome::Applied,
        );
        (pool_id, lease.identity)
    }

    let (_, owned) = dispatched_lease(&db, &repository, "cfni-watchdog-cadence").await;
    let (partitioned_pool, held) =
        dispatched_lease(&db, &repository, "cfni-watchdog-deadline").await;
    // The same lease row under a generation it does not own. Every heartbeat
    // this presents is refused by the production fence, so the *committed*
    // heartbeat clock never moves.
    let disowned = ModelTurnLeaseIdentity {
        lease_id: held.lease_id.clone(),
        generation: held.generation + 1,
        request_id: held.request_id.clone(),
    };
    // Both halves need their premise checked before the chronology starts: the
    // deadline half needs heartbeats that genuinely fail, and the cadence half
    // needs a lease whose heartbeats genuinely succeed. If the first of these
    // applied, the abort below would prove nothing.
    assert_eq!(
        repository
            .heartbeat(&disowned)
            .await
            .expect("disowned heartbeat"),
        ModelTurnLeaseMutationOutcome::Fenced,
    );
    assert_eq!(
        repository.heartbeat(&held).await.expect("held heartbeat"),
        ModelTurnLeaseMutationOutcome::Applied
    );

    let heartbeat_at = async |lease_id: &str| {
        djinn_db::test_support::model_turn_lease_heartbeat_snapshot_fixture(&db, lease_id)
            .await
            .1
    };
    assert_eq!(
        heartbeat_at(&owned.lease_id).await,
        None,
        "the cadence lease must start with no heartbeat instant at all, or the \
         null-to-non-null transition below is not the watchdog's"
    );

    tokio::time::pause();
    let spin = tokio_util::sync::CancellationToken::new();
    let spinner = tokio::spawn({
        let spin = spin.clone();
        async move {
            while !spin.is_cancelled() {
                tokio::task::yield_now().await;
            }
        }
    });

    let cadence_stop = tokio_util::sync::CancellationToken::new();
    let cadence = tokio::spawn(run_turn_watchdog_v1(
        ModelTurnAdmissionCoordinator::new(repository.clone()),
        owned.clone(),
        djinn_provider::ProviderAttemptAbortHandleV1::new(),
        cadence_stop.clone(),
        tokio_util::sync::CancellationToken::new(),
    ));

    let abort = djinn_provider::ProviderAttemptAbortHandleV1::new();
    let fired = tokio_util::sync::CancellationToken::new();
    let deadline_stop = tokio_util::sync::CancellationToken::new();
    let deadline = tokio::spawn(run_turn_watchdog_v1(
        ModelTurnAdmissionCoordinator::new(repository.clone()),
        disowned,
        abort.clone(),
        deadline_stop.clone(),
        fired.clone(),
    ));

    // t≈0. Both loops take their immediate first `interval` tick and stamp the
    // instant their 40-second deadlines are measured from.
    settle_without_advancing_virtual_time(SETTLE).await;
    tokio::time::advance(NUDGE).await;
    settle_without_advancing_virtual_time(SETTLE).await;
    assert_eq!(
        heartbeat_at(&owned.lease_id).await,
        None,
        "the first interval tick is the loop starting, not a heartbeat"
    );

    // t=19: one second short of the cadence.
    tokio::time::advance(std::time::Duration::from_secs(19)).await;
    settle_without_advancing_virtual_time(SETTLE).await;
    assert_eq!(
        heartbeat_at(&owned.lease_id).await,
        None,
        "no heartbeat may be committed before 20 seconds have elapsed"
    );
    assert!(
        !abort.is_aborted(),
        "and nothing may abort 19 seconds into a 40-second deadline"
    );

    // t=20: the cadence. The positive observations wait on the condition
    // rather than on a fixed budget, because a loaded runner makes a real
    // database round trip take arbitrarily long in *real* time while the
    // virtual clock stays exactly where it is. The negative observations above
    // and below keep a fixed budget, for which longer is only ever stricter.
    tokio::time::advance(std::time::Duration::from_secs(1) + NUDGE).await;
    let mut committed = false;
    let waited = std::time::Instant::now();
    while waited.elapsed() < PATIENCE {
        if heartbeat_at(&owned.lease_id).await.is_some() {
            committed = true;
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        committed,
        "the watchdog must commit a heartbeat once 20 seconds have elapsed"
    );
    assert!(
        !abort.is_aborted(),
        "the 20-second tick heartbeats; it does not abort, because 40 seconds \
         of silence have not yet passed"
    );

    // t=39: still inside the deadline the fenced watchdog is running down.
    tokio::time::advance(std::time::Duration::from_secs(19)).await;
    settle_without_advancing_virtual_time(SETTLE).await;
    assert!(
        !abort.is_aborted(),
        "39 seconds without a committed heartbeat is not yet the deadline"
    );
    assert!(!fired.is_cancelled());

    // t=40: the deadline.
    tokio::time::advance(std::time::Duration::from_secs(1) + NUDGE).await;
    let waited = std::time::Instant::now();
    while waited.elapsed() < PATIENCE && !abort.is_aborted() {
        tokio::task::yield_now().await;
    }
    assert!(
        abort.is_aborted(),
        "40 seconds without a committed heartbeat must abort the provider \
         attempt"
    );
    assert!(
        fired.is_cancelled(),
        "and it must say so on its own signal, so a caller can tell a watchdog \
         abort from a caller-requested one"
    );

    cadence_stop.cancel();
    deadline_stop.cancel();
    spin.cancel();
    tokio::time::resume();
    let _ = cadence.await;
    let _ = deadline.await;
    let _ = spinner.await;

    // ── Partition, abort, quarantine, replacement ─────────────────────────
    //
    // The abort above is what a partitioned slot does on its own: it stops
    // sending. It does not release anything, and it must not — the attempt may
    // already have reached the provider. This is the rest of that chronology,
    // and every step is a production function.
    assert_eq!(
        repository
            .acquire_turn(ModelTurnAcquireInput {
                pool_id: partitioned_pool,
                request_id: "cfni-watchdog-replacement".to_owned(),
                owner_pod_uid: Some("pod-cfni-watchdog-b".to_owned()),
                generation: 1,
                debits: request_debit(1),
            })
            .await
            .expect("replacement acquisition must not error"),
        ModelTurnAcquireOutcome::Wait(djinn_db::ModelTurnAdmissionWait::Concurrency {
            target: 1,
            in_flight: 1,
        }),
        "while the aborted lease is still in flight a second slot gets a typed \
         wait, not a second dispatch: the abort released nothing"
    );

    // The leader-side reaper is what terminalises it, from the persisted
    // timestamps alone, at the 90-second boundary.
    let reaped =
        djinn_coordinator::model_turn_admission::controller::reap_stale_model_turn_leases_v1(
            &repository,
            &rfc3339_offset_seconds(120),
            djinn_coordinator::model_turn_admission::controller::REAPER_PASS_LIMIT,
            None,
        )
        .await
        .expect("the reaper pass must not error");
    assert_eq!(
        (reaped.expired, reaped.fenced),
        (2, 0),
        "the pass sweeps every stale in-flight lease this fixture holds — the \
         cadence half's and the partitioned one — and none of them moved \
         between the read and the compare-and-swap; got {reaped:?}"
    );
    assert_eq!(
        djinn_db::test_support::model_turn_terminal_fixture(
            &db,
            &held.lease_id,
            held.generation,
            &held.request_id,
        )
        .await,
        ("expired".to_owned(), "quarantined".to_owned()),
        "a lease expired out of `dispatching` may already have been sent, so \
         its spend stays quarantined rather than being refunded"
    );

    // Only now may the replacement dispatch, and it is the *same* request that
    // was refused above.
    assert!(
        matches!(
            repository
                .acquire_turn(ModelTurnAcquireInput {
                    pool_id: partitioned_pool,
                    request_id: "cfni-watchdog-replacement".to_owned(),
                    owner_pod_uid: Some("pod-cfni-watchdog-b".to_owned()),
                    generation: 1,
                    debits: request_debit(1),
                })
                .await
                .expect("replacement acquisition must not error"),
            ModelTurnAcquireOutcome::Admitted { .. }
        ),
        "once the reaper has terminalised the partitioned lease the replacement \
         dispatches"
    );

    close(db).await;
}

/// An inert frame parser.
///
/// The spawn test below never feeds the guard a frame — its provider stream is
/// permanently pending — so the adapter's only job is to exist. Producing no
/// events is the honest choice: a parser that invented one would put text into
/// a turn no provider sent.
struct CfniInertFrameParser;

impl djinn_provider::provider::ProviderSseFrameParserV1 for CfniInertFrameParser {
    fn parse(
        &mut self,
        _frame: djinn_provider::provider::client::SseFrame,
    ) -> Vec<anyhow::Result<djinn_provider::provider::StreamEvent>> {
        Vec::new()
    }
}

/// A production launch of an enforced covered attempt actually **spawns** the
/// turn watchdog.
///
/// `the_turn_watchdog_commits_every_twenty_seconds_and_aborts_after_forty`
/// proves the loop's *behaviour* by driving `run_turn_watchdog_v1` directly.
/// That is the right shape for a timer proof and the wrong shape for a
/// reachability one. Adversarial verification round two of proposal `96fy`
/// neutralised the single line inside
/// `CoveredAttemptTerminalGuard::start_watchdog` that spawns the loop —
/// leaving the constants, the loop body, the guard and the timer test all
/// exactly where they were — and every test in this target stayed green. That
/// is not a missing diagnostic: a dispatched turn that never heartbeats runs
/// its lease down to the 90-second boundary, where the reaper expires and
/// quarantines it. An unspawned watchdog is every enforced attempt losing its
/// lease mid-flight.
///
/// So this test starts at the top of the production chain and asserts that a
/// **row moves because of it**. It does not call `start_watchdog` and it does
/// not call `run_turn_watchdog_v1`; it calls
/// `launch_prepared_covered_attempt_with_lease`, which is the function
/// `run_reply_loop` uses to launch a prepared turn, and then reads
/// `model_turn_leases.heartbeat_at`.
///
/// **What each half rules out.**
///
/// * Deleting `guard.start_watchdog()` from the launch path, or neutralising
///   the `run_turn_watchdog_v1(…)` call inside `start_watchdog`, leaves
///   `heartbeat_at` null forever, and the second assertion fails naming the
///   spawn. A source-text scan for the call would survive an `if false { … }`
///   wrapper or a spawn moved behind a condition that is never true; a
///   committed heartbeat survives neither, because the row only moves if the
///   loop actually ran.
/// * The first assertion is the other side of that bound: at t≈0 the row must
///   still be null, so the transition observed twenty seconds later is the
///   watchdog's tick and not something the launch itself wrote.
///   `mark_active` moves `lifecycle` and `active_at` and touches
///   `heartbeat_at` never — but asserting it here means a future writer that
///   did cannot be mistaken for a running watchdog.
///
/// The premise is checked before either: the pool enforces, so `prepare` hands
/// back a permit that genuinely **owns a lease**. A shadow permit carries no
/// lease and `start_watchdog` returns immediately by design, so a fixture that
/// accidentally produced one would prove nothing at all.
///
/// The millisecond nudges and the pinned spinner are there for the same reason
/// they are in the cadence test above; see its comment.
#[tokio::test]
async fn an_enforced_covered_attempt_launch_spawns_the_turn_watchdog() {
    use djinn_slot::reply_loop::model_turn_admission::{
        ModelTurnAdmissionCoordinator, ModelTurnAdmissionRequest, ModelTurnPreparation,
    };
    use djinn_slot::reply_loop::turn::launch_prepared_covered_attempt_with_lease;

    /// Enough to cross a deadline, far too little to reach the next one.
    const NUDGE: std::time::Duration = std::time::Duration::from_millis(2);
    /// Real-time budget for the *negative* observation at t≈0.
    const SETTLE: std::time::Duration = std::time::Duration::from_millis(500);
    /// Real-time cap on the positive observation. It waits on the condition,
    /// not on a budget; this only stops a hang if the condition never arrives.
    const PATIENCE: std::time::Duration = std::time::Duration::from_secs(15);

    let db = djinn_coordinator::test_helpers::create_test_db();
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    let pool_id = cfni_seed_pool(&db, "cfni-watchdog-spawn", "enforce").await;
    repository
        .seed_request_bucket_binding_for_test(pool_id, 4, 4)
        .await
        .expect("seed the request binding");

    let coordinator =
        ModelTurnAdmissionCoordinator::new(repository.clone()).with_catalog(cfni_catalog());
    let preparation = coordinator
        .prepare(
            &cfni_plan(request_debit(1)),
            ModelTurnAdmissionRequest {
                credential_id: "cfni-watchdog-spawn".to_owned(),
                request_id: "cfni-watchdog-spawn-request".to_owned(),
                owner_pod_uid: Some("pod-cfni-watchdog-spawn".to_owned()),
                generation: 1,
            },
        )
        .await
        .expect("prepare must not error");
    let lease = match &preparation {
        ModelTurnPreparation::Permit(permit) => permit
            .lease
            .clone()
            .expect("an enforcing pool must hand back a permit that owns a lease"),
        other => panic!("the seeded pool must admit; got {other:?}"),
    };

    let heartbeat_at = async |lease_id: &str| {
        djinn_db::test_support::model_turn_lease_heartbeat_snapshot_fixture(&db, lease_id)
            .await
            .1
    };
    assert_eq!(
        heartbeat_at(&lease.lease_id).await,
        None,
        "the lease must start with no heartbeat instant at all, or the \
         null-to-non-null transition below is not the watchdog's"
    );

    tokio::time::pause();
    let spin = tokio_util::sync::CancellationToken::new();
    let spinner = tokio::spawn({
        let spin = spin.clone();
        async move {
            while !spin.is_cancelled() {
                tokio::task::yield_now().await;
            }
        }
    });

    // The provider attempt never yields a frame and never terminates on its
    // own, so the only thing that can move the lease row is the watchdog.
    let (outcome_tx, outcome_rx) = tokio::sync::oneshot::channel();
    let abort = djinn_provider::ProviderAttemptAbortHandleV1::new();
    let guard = launch_prepared_covered_attempt_with_lease(
        preparation,
        move || {
            Ok((
                djinn_provider::provider::client::ProviderSseAttemptV1::for_test(
                    Box::pin(futures::stream::pending()),
                    abort,
                    outcome_rx,
                ),
                Box::new(CfniInertFrameParser)
                    as Box<dyn djinn_provider::provider::ProviderSseFrameParserV1>,
            ))
        },
        coordinator,
        tokio_util::sync::CancellationToken::new(),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect("the enforced launch must hand back a terminal guard");

    // t≈0. The spawned loop takes its immediate first `interval` tick, which
    // is the loop starting rather than a heartbeat.
    settle_without_advancing_virtual_time(SETTLE).await;
    tokio::time::advance(NUDGE).await;
    settle_without_advancing_virtual_time(SETTLE).await;
    assert_eq!(
        heartbeat_at(&lease.lease_id).await,
        None,
        "the launch itself must not write a heartbeat instant; only the \
         watchdog's tick may"
    );

    // t=20: the cadence. If nothing spawned the loop, this never arrives.
    tokio::time::advance(std::time::Duration::from_secs(20) + NUDGE).await;
    let mut committed = false;
    let waited = std::time::Instant::now();
    while waited.elapsed() < PATIENCE {
        if heartbeat_at(&lease.lease_id).await.is_some() {
            committed = true;
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        committed,
        "twenty seconds after a production launch of an enforced covered \
         attempt the lease has committed no heartbeat, so nothing spawned \
         `run_turn_watchdog_v1`: either the launch path stopped calling \
         `CoveredAttemptTerminalGuard::start_watchdog`, or `start_watchdog` \
         stopped reaching the loop"
    );

    spin.cancel();
    tokio::time::resume();
    let _ = spinner.await;
    drop(outcome_tx);
    drop(guard);
    close(db).await;
}

/// The subscription controller is reachable from the fenced leader cycle, and
/// `model_turn_pools.learned_concurrency` has a production writer.
///
/// Both halves were missing until task `0qi9`. Adversarial verification of
/// `96fy` established that outside its own module and tests,
/// `ingest_qualified_window_v1` — which the learner's own doc comment calls
/// "the sole production ingestion path" — had **no caller at all**, and that
/// the only `UPDATE … SET learned_concurrency` in the tree was
/// `set_pool_learned_concurrency_for_test`. The learner was unreachable and had
/// no column to write to, which is why the usual "delete the one wiring line
/// and watch a test go red" probe kept coming back clean: there was no line.
///
/// The behavioural proof that the wiring carries weight is in `scenario_09`,
/// where a trainable window moves the persisted target and deleting the learner
/// call from `run_completed_window_cycle_v1` reddens it. This test pins the
/// structural facts that proof depends on. It reads only the **production
/// half** of each file — the text before the first `#[cfg(test)]` attribute —
/// and then only the body of the named function, so a call moved into a test
/// module, or out of the leader cycle into some unreached helper, is red here
/// even though the file still contains the string.
#[test]
fn the_subscription_learner_is_wired_to_the_fenced_leader_cycle() {
    /// Everything before the first `#[cfg(test)]` in `source`.
    ///
    /// Each file under scan ends with `#[cfg(test)] #[path = …] mod tests;`, so
    /// this is where its unit tests begin.
    fn production_half(source: &str) -> &str {
        source
            .split_once("#[cfg(test)]")
            .map_or(source, |(before, _)| before)
    }

    /// The body of the item introduced by `signature`, up to the first closing
    /// brace in column zero.
    fn body<'a>(source: &'a str, signature: &str) -> &'a str {
        let (_, rest) = source
            .split_once(signature)
            .unwrap_or_else(|| panic!("the source does not define `{signature}`"));
        rest.split_once("\n}\n").map_or(rest, |(inside, _)| inside)
    }

    let controller =
        read_sibling_crate_source("djinn-coordinator/src/model_turn_admission_controller.rs");
    let learner = read_sibling_crate_source(
        "djinn-coordinator/src/model_turn_admission_subscription_learner.rs",
    );
    let repository = read_sibling_crate_source("djinn-db/src/repositories/model_turn_admission.rs");
    for (name, source) in [
        ("controller", &controller),
        ("learner", &learner),
        ("repository", &repository),
    ] {
        assert!(
            source.len() > 4_000 && production_half(source).len() > 1_000,
            "the {name} source read is empty or truncated; the scan is broken"
        );
    }

    // 1. The fenced leader cycle itself calls the learner's production entry
    //    point — not the file, the function.
    assert!(
        body(
            production_half(&controller),
            "pub async fn run_completed_window_cycle_v1(",
        )
        .contains("learn_and_persist_window_target_v1("),
        "the fenced leader cycle must call the subscription learner"
    );
    // And the leader tick calls the fenced cycle, so the chain reaches an actor
    // the advisory-lock leader actually runs.
    assert!(
        body(
            production_half(&controller),
            "pub(crate) async fn run_completed_phase_c_window(",
        )
        .contains("run_completed_window_cycle_v1("),
        "the leader tick must call the fenced cycle"
    );

    // 2. The learner reaches the window only through the exact-bound,
    //    catalog-qualified seam: no raw controller-window query and no caller
    //    verdict about whether the window is trainable.
    let persist_path = body(
        production_half(&learner),
        "pub async fn learn_and_persist_window_target_v1(",
    );
    assert!(
        persist_path.contains("ingest_qualified_window_v1("),
        "the learner's persist path must ingest through the catalog-qualified \
         seam"
    );
    assert!(
        persist_path.contains("apply_learned_concurrency("),
        "and it must be what drives the production writer"
    );

    // 3. The column has a production writer, fenced in the same statement as
    //    the update.
    let production_repository = production_half(&repository);
    assert!(
        production_repository.contains("pub async fn apply_learned_concurrency("),
        "`model_turn_pools.learned_concurrency` must have a production writer"
    );
    let writer = body(
        production_repository,
        "pub async fn apply_learned_concurrency(",
    );
    assert!(
        writer.contains("SET learned_concurrency = $2")
            && writer.contains("FROM coordinator_incarnations c"),
        "the production writer must apply the leadership fence in the same \
         statement as the update, as `upsert_controller_window` does"
    );
    // 4. The test-only setter stays test-only. It is text in the production
    //    half of the file, but it is not *compiled* into a production build,
    //    and the attribute that makes that true is what is pinned here.
    assert!(
        production_repository.contains(
            "#[cfg(any(test, feature = \"test-support\"))]\n    pub async fn \
             set_pool_learned_concurrency_for_test("
        ),
        "`set_pool_learned_concurrency_for_test` must remain gated behind \
         `test`/`test-support` and must not become the production path"
    );
}

/// `enforce` has exactly one production route, and that route demands *current*
/// coverage.
///
/// `ModelTurnAdmissionRepository::set_pool_mode_in_transaction` can express the
/// `shadow → enforce` edge, and it gates that edge on the compatibility phase
/// and the identity — but not on coverage, because it is handed no
/// expected-path denominator to compare against. The compatibility phase is
/// durable, so a pool that reached `d` while covered would still read `d` after
/// coverage was lost. If some future caller reached for that function, an
/// uncovered pool could enforce for the width of one controller window.
///
/// The leader pass closes that: `apply_enforcement_pass_in_transaction`
/// re-observes coverage inside the transaction that mutates, and additionally
/// requires the window to have qualified. This test pins the fact the argument
/// rests on — that nothing outside `djinn-db`'s own tests asks
/// `set_pool_mode_in_transaction` for `Enforce`.
///
/// A `grep` is the right instrument here precisely because the claim is about
/// the *absence* of a caller: the search space is the source tree, and it is
/// enumerated rather than sampled.
#[test]
fn enforce_has_no_production_caller_outside_the_guarded_leader_pass() {
    use std::path::Path;

    fn rust_sources(root: &Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name == "target") {
                    continue;
                }
                rust_sources(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates directory");
    let mut sources = Vec::new();
    rust_sources(crates, &mut sources);
    assert!(
        sources.len() > 200,
        "the source scan found only {} files; the walker is broken",
        sources.len()
    );
    assert!(
        sources
            .iter()
            .any(|path| path.ends_with("djinn-db/src/repositories/model_turn_admission.rs")),
        "the source scan missed the admission repository; the walker is broken"
    );

    let mut offenders = Vec::new();
    for path in &sources {
        // `djinn-db`'s own tests exercise the edge deliberately; they are the
        // reason it is gated at all.
        if path.starts_with(crates.join("djinn-db")) {
            continue;
        }
        // This file names both tokens only in order to search for them, so it
        // would otherwise match itself. Excluded by exact path rather than by a
        // blanket "skip tests" rule, which would also hide an inline
        // `#[cfg(test)]` caller sitting inside a production module.
        if path.ends_with("djinn-coordinator/tests/model_admission_conformance.rs") {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(path) else {
            continue;
        };
        if source.contains("set_pool_mode_in_transaction")
            && source.contains("ModelTurnAdmissionPhase::Enforce")
        {
            offenders.push(path.strip_prefix(crates).unwrap_or(path).to_owned());
        }
    }
    assert!(
        offenders.is_empty(),
        "`enforce` must be reachable only through the guarded leader pass, which \
         re-observes coverage inside the transaction that mutates. Found a \
         caller pairing set_pool_mode_in_transaction with Enforce in \
         {offenders:?}. If this is deliberate, it needs its own coverage \
         re-observation first."
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Consolidated normative scenarios and the required-scenario manifest (`cfni`)
// ═══════════════════════════════════════════════════════════════════════════
//
// Proposal `96fy` attaches eleven acceptance criteria. The eleventh names the
// command; the other ten are the *criterion groups* below. Each group is owned
// by exactly one `scenario_NN_*` function in this file, and
// [`REQUIRED_SCENARIOS`] is the manifest that binds group to function.
//
// The manifest holds the function **item**, not only its name, so deleting a
// scenario or renaming it without updating the manifest does not compile. The
// name string is held alongside it and compared against the set of
// `scenario_*` functions this file actually defines, so a scenario cannot be
// added without registering it either.
//
// Where a scenario needs a state production cannot currently reach — a trained
// Phase-C window, or a pool in `enforce` — it seeds that state explicitly and
// says so at the seeding line. Phase B stored a capability *instant* rather
// than a coverage interval and `model_turn_phase_c_evidence` has no usage
// column, so `qualify_aligned_phase_c_window_v1` reports
// `PartialCapabilityCoverage`/`MissingUsage` for every real window and no
// production pool can advance. Widening the instant or defaulting the usage
// would forge exactly the coverage these decisions rest on, so neither is done
// here.

/// The provider/model scope every consolidated scenario resolves through.
const CFNI_PROVIDER: &str = "cfni-provider";
const CFNI_MODEL: &str = "namespace/cfni-model";
/// The one live Ready slot the expected-path denominator is built from.
const CFNI_SLOT: &str = "cfni-live-slot";
const CFNI_REVISION: &str = "cfni-rev-1";
/// A durable coordinator incarnation id, so the leadership fence is real.
const CFNI_INCARNATION: &str = "01a01246-0000-7000-8000-00000000cf01";
const CFNI_GENERATION: i64 = 7;

/// A catalog that resolves the scenario route.
///
/// Without it the coordinator drops the route from every projection and from
/// every telemetry emission — resolution is the coordinator's authority, and a
/// stored label never gets to claim it.
fn cfni_catalog() -> djinn_provider::catalog::CatalogService {
    let catalog = djinn_provider::catalog::CatalogService::new();
    catalog.add_custom_provider(
        djinn_core::models::Provider {
            id: CFNI_PROVIDER.into(),
            name: "cfni Provider".into(),
            npm: String::new(),
            env_vars: vec!["CFNI_API_KEY".into()],
            base_url: "https://example.invalid/v1".into(),
            docs_url: String::new(),
            is_openai_compatible: true,
        },
        vec![djinn_core::models::Model {
            id: CFNI_MODEL.into(),
            provider_id: CFNI_PROVIDER.into(),
            name: "cfni Model".into(),
            tool_call: false,
            reasoning: false,
            attachment: false,
            context_window: 1,
            output_limit: 1,
            pricing: djinn_core::models::Pricing::default(),
        }],
    );
    catalog
}

/// Seed one credential record and its pool at `phase`, through the DB
/// crate's fixture seam rather than raw SQL.
async fn cfni_seed_pool(db: &Database, credential_id: &str, phase: &str) -> i64 {
    djinn_db::repositories::test_support::seed_scoped_model_turn_admission_fixture(
        db,
        credential_id,
        CFNI_PROVIDER,
        CFNI_MODEL,
        phase,
        "supported",
        1,
    )
    .await
}

/// The one live Ready slot workload every projection is built from.
fn cfni_ready_slot() -> djinn_k8s::WorkloadRecord {
    djinn_k8s::WorkloadRecord {
        kind: djinn_k8s::WorkloadObjectKind::Pod,
        name: "cfni-slot".into(),
        uid: Some(CFNI_SLOT.to_owned()),
        labels: std::collections::BTreeMap::new(),
        terminal: false,
        ready: true,
        deployment_revision: Some(CFNI_REVISION.to_owned()),
        images: vec![],
        commands: vec![],
    }
}

fn cfni_expected_key() -> djinn_db::ModelTurnExpectedPathKey {
    djinn_db::ModelTurnExpectedPathKey {
        slot_pod_uid: CFNI_SLOT.to_owned(),
        deployment_revision: CFNI_REVISION.to_owned(),
    }
}

fn cfni_fence() -> djinn_db::ModelTurnControllerFence {
    djinn_db::ModelTurnControllerFence {
        incarnation_id: CFNI_INCARNATION.to_owned(),
        live_since_at: "1970-01-01T00:00:00Z".to_owned(),
    }
}

async fn cfni_register_incarnation(db: &Database) {
    djinn_db::CoordinatorIncarnationRepository::new(db.clone())
        .register(CFNI_INCARNATION)
        .await
        .expect("register the coordinator incarnation the fence checks");
}

fn now_rfc3339() -> String {
    ::time::OffsetDateTime::now_utc()
        .format(&::time::format_description::well_known::Rfc3339)
        .expect("format now")
}

fn rfc3339_offset_seconds(seconds: i64) -> String {
    (::time::OffsetDateTime::now_utc() + ::time::Duration::seconds(seconds))
        .format(&::time::format_description::well_known::Rfc3339)
        .expect("format offset instant")
}

/// Report B2 coverage for the one live slot, through the production writer.
async fn cfni_cover_route(repository: &ModelTurnAdmissionRepository, pool_id: i64) {
    repository
        .record_capability_heartbeat(djinn_db::ModelTurnCapabilityHeartbeatInput {
            pool_id,
            slot_pod_uid: CFNI_SLOT.to_owned(),
            deployment_revision: CFNI_REVISION.to_owned(),
            provider_id: CFNI_PROVIDER.to_owned(),
            model_id: CFNI_MODEL.to_owned(),
        })
        .await
        .expect("record the B2 capability heartbeat");
}

/// The five v1 attempt stages a complete observation chain is made of.
const CFNI_CHAIN: [djinn_db::ModelTurnPhaseCEvidenceStage; 5] = [
    djinn_db::ModelTurnPhaseCEvidenceStage::Decision,
    djinn_db::ModelTurnPhaseCEvidenceStage::Dispatch,
    djinn_db::ModelTurnPhaseCEvidenceStage::Heartbeat,
    djinn_db::ModelTurnPhaseCEvidenceStage::ProviderOutcome,
    djinn_db::ModelTurnPhaseCEvidenceStage::Reconcile,
];

/// Record one attempt chain containing exactly `stages`, through the
/// production evidence writer.
async fn cfni_record_chain(
    repository: &ModelTurnAdmissionRepository,
    pool_id: i64,
    fingerprint: &str,
    stages: &[djinn_db::ModelTurnPhaseCEvidenceStage],
) {
    for stage in stages {
        repository
            .record_phase_c_evidence(djinn_db::ModelTurnPhaseCEvidenceInput {
                pool_id,
                slot_pod_uid: CFNI_SLOT.to_owned(),
                deployment_revision: CFNI_REVISION.to_owned(),
                provider_id: CFNI_PROVIDER.to_owned(),
                model_id: CFNI_MODEL.to_owned(),
                attempt_fingerprint: fingerprint.to_owned(),
                stage: *stage,
                outcome: djinn_db::ModelTurnPhaseCEvidenceOutcome::Recorded,
            })
            .await
            .expect("record phase-C evidence");
    }
}

/// A covered v1 provider plan for the scenario route, debiting `debits`.
///
/// Built with the capabilities the B1 contract requires (`hidden_retries =
/// false`, abort supported); `scenario_04`/`scenario_08` build *uncovered*
/// variants through the production planner instead of hand-writing them.
fn cfni_plan(debits: Vec<ModelTurnBucketDebit>) -> djinn_provider::ProviderAttemptPlanV1 {
    djinn_provider::ProviderAttemptPlanV1 {
        scope: djinn_provider::ProviderAttemptScopeV1 {
            credential: djinn_provider::ProviderCredentialRecordScopeV1::from_credential_record_id(
                "cfni-credential",
            ),
            provider_id: CFNI_PROVIDER.to_owned(),
            model_id: CFNI_MODEL.to_owned(),
        },
        coverage: djinn_provider::ProviderAttemptRouteCoverageV1::Covered {
            capabilities: djinn_provider::ProviderAttemptCapabilitiesV1 {
                hidden_retries: djinn_provider::ProviderHiddenRetryCapabilityV1::Disabled,
                abort: djinn_provider::ProviderAbortCapabilityV1::Supported,
            },
            supported_bucket_bindings: vec![
                ModelTurnBucketKind::Request,
                ModelTurnBucketKind::Input,
                ModelTurnBucketKind::Output,
                ModelTurnBucketKind::Combined,
            ],
            policy: djinn_provider::ProviderAdmissionPolicyV1::Proactive,
        },
        debits,
        output_reservation_source: djinn_provider::ProviderOutputReservationSourceV1::ExplicitLimit,
        abort: djinn_provider::ProviderAttemptAbortHandleV1::new(),
    }
}

/// Read this target's own source text.
///
/// The `[[test]]` entry in `Cargo.toml` pins this path, so renaming the file is
/// already a build error; this read therefore cannot silently miss.
fn read_target_source() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/model_admission_conformance.rs");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// The body one simulated provider attempt sends.
const CFNI_PROVIDER_REQUEST_BODY: &str = "cfni-provider-request-body";

/// Send one hand-framed HTTP request to `endpoint` over a raw socket.
///
/// This is the "network bytes" half of the unleased-attempt scenario. It is
/// deliberately not an HTTP client: `scripts/check-http-boundary.sh` reserves
/// outbound HTTP client construction to `djinn-provider`, and a raw socket
/// write is the more literal instrument anyway — the recorder on the other end
/// counts bytes that actually arrived.
///
/// Blocking, so callers run it on a blocking pool rather than on a runtime
/// worker that the recorder's own server also needs.
fn send_provider_bytes(endpoint: &str) {
    use std::io::{Read, Write};

    let mut stream =
        std::net::TcpStream::connect(endpoint).expect("connect to the boundary recorder");
    let request = format!(
        "POST /cfni HTTP/1.1\r\nHost: {endpoint}\r\nContent-Type: text/plain\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{CFNI_PROVIDER_REQUEST_BODY}",
        CFNI_PROVIDER_REQUEST_BODY.len()
    );
    stream
        .write_all(request.as_bytes())
        .expect("write the request bytes");
    stream.flush().expect("flush the request bytes");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read the recorder's response");
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "the boundary recorder must have accepted the send; got:\n{response}"
    );
}

/// Read a sibling crate's source file, relative to `server/crates/`.
fn read_sibling_crate_source(relative: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

// ─── Group 1: the enforced attempt lifecycle ───────────────────────────────

/// Every enforced attempt acquires all of its buckets or none, commits the
/// dispatch fence before any send, heartbeats under an immutable generation,
/// expires only at the 90-second boundary, and reconciles exactly once.
///
/// Each claim is settled against a persisted row, never against the enum the
/// repository handed back:
///
/// * **Atomic multi-bucket acquisition.** The pool has four request units and
///   zero input units. An attempt debiting both is refused, and the proof that
///   the request debit did *not* apply is that a later single-bucket attempt
///   for the whole four units still succeeds. A partially-applied debit would
///   leave three and that attempt would defer.
/// * **Multi-pod barrier at target 1.** Two acquisitions with distinct request
///   ids run concurrently on two runtime workers. Exactly one is admitted and
///   `model_turn_leases` holds exactly one row for the pool.
/// * **Dispatch before send.** `mark_dispatching` applies once and is
///   idempotent on replay; a heartbeat presenting a different generation is
///   fenced and does not move the lease.
/// * **The 90-second boundary is exact.** The lease is backdated to a fixed
///   instant and the reaper's compare-and-swap is offered a boundary at 89 and
///   then at 90 seconds. The first is refused, the second applies.
/// * **Unknown spend stays quarantined.** A lease expired out of `dispatching`
///   — possibly sent — records `quarantined`; a lease expired out of
///   `reserved` — provably unsent — records `refunded`.
/// * **Reconcile is idempotent.** Two reconciliations of the same identity
///   produce `Applied` then `Idempotent` and exactly one terminal row.
///
/// The 20-second heartbeat cadence and the 40-second abort deadline are
/// slot-local timers. They are proven behaviourally, under paused time and
/// against the production loop, by
/// [`the_turn_watchdog_commits_every_twenty_seconds_and_aborts_after_forty`] in
/// this same target.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scenario_01_enforced_attempt_is_atomic_fenced_and_reconciled_once() {
    use djinn_db::{
        ModelTurnLeaseExpiryInput, ModelTurnLeaseIdentity, ModelTurnLeaseLifecycle,
        ModelTurnLeaseMutationOutcome, ModelTurnLeaseReconciliationInput,
        ModelTurnLeaseTerminalOutcome,
    };

    let db = djinn_coordinator::test_helpers::create_test_db();
    let repository = ModelTurnAdmissionRepository::new(db.clone());

    // ── Atomicity across bucket scopes ────────────────────────────────────
    let atomic_pool = cfni_seed_pool(&db, "cfni-lifecycle-atomic", "enforce").await;
    repository
        .seed_bucket_binding_for_test(atomic_pool, ModelTurnBucketKind::Request, 4, 4)
        .await
        .expect("seed the request binding");
    repository
        .seed_bucket_binding_for_test(atomic_pool, ModelTurnBucketKind::Input, 4, 0)
        .await
        .expect("seed the starved input binding");

    let both = repository
        .acquire_turn(ModelTurnAcquireInput {
            pool_id: atomic_pool,
            request_id: "cfni-atomic-both".to_owned(),
            owner_pod_uid: Some("pod-cfni-atomic".to_owned()),
            generation: 1,
            debits: vec![
                ModelTurnBucketDebit {
                    bucket_kind: ModelTurnBucketKind::Request,
                    units: 1,
                },
                ModelTurnBucketDebit {
                    bucket_kind: ModelTurnBucketKind::Input,
                    units: 1,
                },
            ],
        })
        .await
        .expect("acquisition must not error");
    assert!(
        matches!(
            both,
            ModelTurnAcquireOutcome::Wait(djinn_db::ModelTurnAdmissionWait::BucketUnavailable {
                bucket_kind: ModelTurnBucketKind::Input,
                ..
            })
        ),
        "a starved input bucket must defer the whole attempt, got {both:?}"
    );
    assert_eq!(
        model_turn_lease_count_for_pool_fixture(&db, atomic_pool).await,
        0,
        "a deferred acquisition writes no lease row"
    );
    // If the refused attempt had debited the request bucket, three units would
    // remain and this would defer instead of admitting.
    let whole_request_bucket = repository
        .acquire_turn(ModelTurnAcquireInput {
            pool_id: atomic_pool,
            request_id: "cfni-atomic-probe".to_owned(),
            owner_pod_uid: Some("pod-cfni-atomic".to_owned()),
            generation: 1,
            debits: vec![ModelTurnBucketDebit {
                bucket_kind: ModelTurnBucketKind::Request,
                units: 4,
            }],
        })
        .await
        .expect("probe acquisition must not error");
    assert!(
        matches!(
            whole_request_bucket,
            ModelTurnAcquireOutcome::Admitted { .. }
        ),
        "the refused attempt must have left all four request units, got \
         {whole_request_bucket:?}"
    );

    // ── The multi-pod barrier at target 1 ─────────────────────────────────
    let pool_id = cfni_seed_pool(&db, "cfni-lifecycle", "enforce").await;
    repository
        .set_pool_learned_concurrency_for_test(pool_id, 1)
        .await
        .expect("pin the learned target at 1");
    repository
        .seed_request_bucket_binding_for_test(pool_id, 8, 8)
        .await
        .expect("seed the request binding");

    let acquire = |request_id: &'static str, pod: &'static str| {
        let repository = repository.clone();
        async move {
            repository
                .acquire_turn(ModelTurnAcquireInput {
                    pool_id,
                    request_id: request_id.to_owned(),
                    owner_pod_uid: Some(pod.to_owned()),
                    generation: 1,
                    debits: request_debit(1),
                })
                .await
                .expect("acquisition must not error")
        }
    };
    let (first, second) = tokio::join!(
        acquire("cfni-pod-a", "pod-cfni-a"),
        acquire("cfni-pod-b", "pod-cfni-b")
    );
    let admitted = [&first, &second]
        .into_iter()
        .filter(|outcome| matches!(outcome, ModelTurnAcquireOutcome::Admitted { .. }))
        .count();
    assert_eq!(
        admitted, 1,
        "target 1 admits exactly one of two simultaneous pods; got \
         {first:?} and {second:?}"
    );
    assert_eq!(
        model_turn_lease_count_for_pool_fixture(&db, pool_id).await,
        1,
        "and exactly one durable lease row exists"
    );
    let loser = [&first, &second]
        .into_iter()
        .find(|outcome| !matches!(outcome, ModelTurnAcquireOutcome::Admitted { .. }))
        .expect("one caller must have been deferred");
    assert!(
        matches!(
            loser,
            ModelTurnAcquireOutcome::Wait(djinn_db::ModelTurnAdmissionWait::Concurrency {
                target: 1,
                ..
            })
        ),
        "the loser receives a typed concurrency wait, got {loser:?}"
    );

    let ModelTurnAcquireOutcome::Admitted { lease, .. } = [first, second]
        .into_iter()
        .find(|outcome| matches!(outcome, ModelTurnAcquireOutcome::Admitted { .. }))
        .expect("one caller must have been admitted")
    else {
        panic!("filtered to the admitted outcome");
    };
    let identity = lease.identity.clone();

    // ── Dispatch marking is the pre-send fence, and is fenced by generation ─
    assert_eq!(
        repository
            .mark_dispatching(&identity)
            .await
            .expect("mark dispatching"),
        ModelTurnLeaseMutationOutcome::Applied
    );
    assert_eq!(
        repository
            .mark_dispatching(&identity)
            .await
            .expect("replay mark dispatching"),
        ModelTurnLeaseMutationOutcome::Idempotent,
        "replaying the pre-send fence is idempotent, not a second dispatch"
    );
    let impostor = ModelTurnLeaseIdentity {
        lease_id: identity.lease_id.clone(),
        generation: identity.generation + 1,
        request_id: identity.request_id.clone(),
    };
    assert_eq!(
        repository
            .heartbeat(&impostor)
            .await
            .expect("impostor heartbeat"),
        ModelTurnLeaseMutationOutcome::Fenced,
        "a lease's generation is immutable; a different one owns nothing"
    );
    assert_eq!(
        repository.heartbeat(&identity).await.expect("heartbeat"),
        ModelTurnLeaseMutationOutcome::Applied
    );

    // ── The 90-second expiry boundary, exactly ────────────────────────────
    const BACKDATED: &str = "1970-01-02T00:00:00Z";
    repository
        .backdate_lease_for_test(&identity, BACKDATED, Some(BACKDATED))
        .await
        .expect("backdate the lease the reaper reads");
    let expiry = |boundary: &'static str| ModelTurnLeaseExpiryInput {
        identity: identity.clone(),
        observed_lifecycle: ModelTurnLeaseLifecycle::Dispatching,
        observed_heartbeat_at: Some(BACKDATED.to_owned()),
        boundary_at: boundary.to_owned(),
    };
    assert_eq!(
        repository
            .expire_lease(expiry("1970-01-02T00:01:29Z"))
            .await
            .expect("early expiry"),
        ModelTurnLeaseMutationOutcome::Fenced,
        "89 seconds is inside the 90-second lease boundary"
    );
    assert_eq!(
        repository
            .expire_lease(expiry("1970-01-02T00:01:30Z"))
            .await
            .expect("boundary expiry"),
        ModelTurnLeaseMutationOutcome::Applied,
        "90 seconds is the boundary itself"
    );
    assert_eq!(
        djinn_db::test_support::model_turn_terminal_fixture(
            &db,
            &identity.lease_id,
            identity.generation,
            &identity.request_id,
        )
        .await,
        ("expired".to_owned(), "quarantined".to_owned()),
        "a lease expired out of `dispatching` may have been sent, so its spend \
         stays quarantined"
    );

    // ── A provably unsent lease is refunded, and reconcile is idempotent ───
    let unsent_pool = cfni_seed_pool(&db, "cfni-lifecycle-unsent", "enforce").await;
    repository
        .seed_request_bucket_binding_for_test(unsent_pool, 4, 4)
        .await
        .expect("seed the request binding");
    let ModelTurnAcquireOutcome::Admitted { lease: unsent, .. } = repository
        .acquire_turn(ModelTurnAcquireInput {
            pool_id: unsent_pool,
            request_id: "cfni-unsent".to_owned(),
            owner_pod_uid: Some("pod-cfni-unsent".to_owned()),
            generation: 1,
            debits: request_debit(1),
        })
        .await
        .expect("acquisition must not error")
    else {
        panic!("the unsent pool must admit");
    };
    let unsent = unsent.identity.clone();
    repository
        .backdate_lease_for_test(&unsent, BACKDATED, None)
        .await
        .expect("backdate the reserved lease");
    assert_eq!(
        repository
            .expire_lease(ModelTurnLeaseExpiryInput {
                identity: unsent.clone(),
                observed_lifecycle: ModelTurnLeaseLifecycle::Reserved,
                observed_heartbeat_at: None,
                boundary_at: "1970-01-02T00:01:30Z".to_owned(),
            })
            .await
            .expect("expire the reserved lease"),
        ModelTurnLeaseMutationOutcome::Applied
    );
    assert_eq!(
        djinn_db::test_support::model_turn_terminal_fixture(
            &db,
            &unsent.lease_id,
            unsent.generation,
            &unsent.request_id,
        )
        .await,
        ("expired".to_owned(), "refunded".to_owned()),
        "a lease that never left `reserved` was provably unsent and is refunded"
    );

    let reconcile_pool = cfni_seed_pool(&db, "cfni-lifecycle-reconcile", "enforce").await;
    repository
        .seed_request_bucket_binding_for_test(reconcile_pool, 4, 4)
        .await
        .expect("seed the request binding");
    let ModelTurnAcquireOutcome::Admitted {
        lease: reconciled, ..
    } = repository
        .acquire_turn(ModelTurnAcquireInput {
            pool_id: reconcile_pool,
            request_id: "cfni-reconcile".to_owned(),
            owner_pod_uid: Some("pod-cfni-reconcile".to_owned()),
            generation: 1,
            debits: request_debit(1),
        })
        .await
        .expect("acquisition must not error")
    else {
        panic!("the reconcile pool must admit");
    };
    let reconciled = reconciled.identity.clone();
    let terminals_before =
        djinn_db::test_support::count_rows_for_test(&db, "model_turn_lease_terminals").await;
    let input = || ModelTurnLeaseReconciliationInput {
        identity: reconciled.clone(),
        outcome: ModelTurnLeaseTerminalOutcome::Completed,
        authoritative_usage: None,
        detail: None,
    };
    assert_eq!(
        repository.reconcile(input()).await.expect("reconcile"),
        ModelTurnLeaseMutationOutcome::Applied
    );
    assert_eq!(
        repository.reconcile(input()).await.expect("re-reconcile"),
        ModelTurnLeaseMutationOutcome::Idempotent,
        "reconciliation is idempotent by (lease_id, generation, request_id)"
    );
    assert_eq!(
        djinn_db::test_support::count_rows_for_test(&db, "model_turn_lease_terminals").await,
        terminals_before + 1,
        "two reconciliations of one lease store exactly one terminal record"
    );
    assert_eq!(
        repository
            .pool_control_state_for_test(reconcile_pool)
            .await
            .expect("pool state")
            .expect("pool exists")
            .4,
        0,
        "the reconciliation released concurrency exactly once"
    );

    // The 20-second cadence and the 40-second abort deadline were pinned here
    // by a source-string match on `djinn-slot`'s text until task `kcso`. They
    // are now proven as behaviour, under paused time and against the production
    // loop, in `the_turn_watchdog_commits_every_twenty_seconds_and_aborts_after_forty`.

    close(db).await;
}

// ─── Group 2: independent immutable generations ────────────────────────────

/// At target 2, expiring lease A never touches lease B.
///
/// Both leases are acquired through the production path against one pool whose
/// learned target is 2, so they are genuinely concurrent rather than
/// sequential. A is backdated past the boundary and expired; every assertion
/// afterwards reads B's persisted row, not A's return value:
///
/// * B's lifecycle and generation are unchanged.
/// * B still heartbeats under its own generation, and reconciles under it.
/// * Every late A mutation — heartbeat, a second expiry, a reconcile — is
///   fenced, and the terminal ledger still holds exactly one A row.
/// * Concurrency is reclaimed once per lease, so `in_flight` lands on zero
///   rather than below it, and the request bucket is credited at most once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scenario_02_expiring_one_lease_leaves_its_sibling_untouched() {
    use djinn_db::{
        ModelTurnLeaseExpiryInput, ModelTurnLeaseLifecycle, ModelTurnLeaseMutationOutcome,
        ModelTurnLeaseReconciliationInput, ModelTurnLeaseTerminalOutcome,
    };

    let db = djinn_coordinator::test_helpers::create_test_db();
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    let pool_id = cfni_seed_pool(&db, "cfni-generations", "enforce").await;
    repository
        .set_pool_learned_concurrency_for_test(pool_id, 2)
        .await
        .expect("raise the learned target to 2");
    repository
        .seed_request_bucket_binding_for_test(pool_id, 4, 4)
        .await
        .expect("seed the request binding");

    let mut identities = Vec::new();
    for (request_id, pod) in [
        ("cfni-lease-a", "pod-cfni-a"),
        ("cfni-lease-b", "pod-cfni-b"),
    ] {
        let ModelTurnAcquireOutcome::Admitted { lease, .. } = repository
            .acquire_turn(ModelTurnAcquireInput {
                pool_id,
                request_id: request_id.to_owned(),
                owner_pod_uid: Some(pod.to_owned()),
                generation: 1,
                debits: request_debit(1),
            })
            .await
            .expect("acquisition must not error")
        else {
            panic!("target 2 must admit both {request_id}");
        };
        identities.push(lease.identity.clone());
    }
    let (a, b) = (identities[0].clone(), identities[1].clone());
    assert_ne!(
        a.lease_id, b.lease_id,
        "two concurrent leases must be distinct rows"
    );
    for identity in [&a, &b] {
        assert_eq!(
            repository
                .mark_dispatching(identity)
                .await
                .expect("mark dispatching"),
            ModelTurnLeaseMutationOutcome::Applied
        );
    }
    assert_eq!(
        model_turn_lease_count_for_pool_fixture(&db, pool_id).await,
        2
    );

    const BACKDATED: &str = "1970-01-02T00:00:00Z";
    repository
        .backdate_lease_for_test(&a, BACKDATED, Some(BACKDATED))
        .await
        .expect("backdate A only");
    assert_eq!(
        repository
            .expire_lease(ModelTurnLeaseExpiryInput {
                identity: a.clone(),
                observed_lifecycle: ModelTurnLeaseLifecycle::Dispatching,
                observed_heartbeat_at: Some(BACKDATED.to_owned()),
                boundary_at: "1970-01-02T00:01:30Z".to_owned(),
            })
            .await
            .expect("expire A"),
        ModelTurnLeaseMutationOutcome::Applied
    );

    // B is untouched: its persisted lifecycle still admits a heartbeat under
    // its original generation.
    assert_eq!(
        djinn_db::test_support::model_turn_lease_lifecycle_fixture(&db, &b.lease_id).await,
        "dispatching",
        "expiring A must not move B"
    );
    assert_eq!(
        repository.heartbeat(&b).await.expect("B heartbeat"),
        ModelTurnLeaseMutationOutcome::Applied,
        "B still owns its lease under its original generation"
    );

    // Every late A mutation fails.
    assert_eq!(
        repository.heartbeat(&a).await.expect("late A heartbeat"),
        ModelTurnLeaseMutationOutcome::Fenced
    );
    assert_eq!(
        repository
            .expire_lease(ModelTurnLeaseExpiryInput {
                identity: a.clone(),
                observed_lifecycle: ModelTurnLeaseLifecycle::Dispatching,
                observed_heartbeat_at: Some(BACKDATED.to_owned()),
                boundary_at: "1970-01-02T00:01:30Z".to_owned(),
            })
            .await
            .expect("late A expiry"),
        ModelTurnLeaseMutationOutcome::Fenced,
        "a terminal lease cannot be terminalised twice"
    );
    assert_eq!(
        repository
            .reconcile(ModelTurnLeaseReconciliationInput {
                identity: a.clone(),
                outcome: ModelTurnLeaseTerminalOutcome::Completed,
                authoritative_usage: None,
                detail: None,
            })
            .await
            .expect("late A reconcile"),
        ModelTurnLeaseMutationOutcome::Fenced
    );
    assert_eq!(
        djinn_db::test_support::model_turn_terminal_fixture(
            &db,
            &a.lease_id,
            a.generation,
            &a.request_id,
        )
        .await,
        ("expired".to_owned(), "quarantined".to_owned()),
        "A's single terminal record still says exactly what it said"
    );
    assert_eq!(
        repository
            .pool_control_state_for_test(pool_id)
            .await
            .expect("pool state")
            .expect("pool exists")
            .4,
        1,
        "A's expiry reclaimed concurrency once; B still holds the other slot"
    );

    // B reconciles under its own generation and the pool settles at zero.
    assert_eq!(
        repository
            .reconcile(ModelTurnLeaseReconciliationInput {
                identity: b.clone(),
                outcome: ModelTurnLeaseTerminalOutcome::Completed,
                authoritative_usage: None,
                detail: None,
            })
            .await
            .expect("reconcile B"),
        ModelTurnLeaseMutationOutcome::Applied
    );
    assert_eq!(
        repository
            .pool_control_state_for_test(pool_id)
            .await
            .expect("pool state")
            .expect("pool exists")
            .4,
        0,
        "concurrency is reclaimed exactly once per lease, never twice"
    );

    close(db).await;
}

// ─── Group 3: conservative reservation and the typed wait ──────────────────

/// Request/input/output estimates are conservative, and one remaining unit
/// admits exactly one of two callers.
///
/// The estimate half runs the production planner, not a restatement of its
/// arithmetic: a 300-byte body has a byte fallback of 100 units, and the plan
/// must reserve 115 — the fallback plus the mandated 15% — because the
/// provider's own estimate is lower. Raising the provider estimate above the
/// fallback must move the reservation, or the `max` is dead. An absent
/// `max_output_tokens` falls back to the model default, and a default above
/// 16,384 is capped there.
///
/// The atomicity half then drives two callers at one remaining request unit
/// through the production `acquire_turn`: exactly one lease row exists
/// afterwards and the loser holds a typed wait naming the bucket, the
/// available units and what it needed. A bucket whose reset instant is set
/// defers with `ResetAt` rather than spending against the next epoch.
///
/// The last half drives `ProviderApiKeyNormalizerV1` — the production API-key
/// response normalizer — through the remaining named clauses: cold-start
/// discovery, usage reconciliation, stale headers in both directions, reset
/// epochs and epoch regression, incomplete/impossible/malformed headers,
/// `retry-after` in delta, HTTP-date and overflow forms, and reactive-only
/// Gemini refusing to take a proactive capacity from headers at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scenario_03_reservation_is_conservative_and_atomic_at_one_unit() {
    use djinn_provider::{
        MAX_OUTPUT_RESERVATION_UNITS_V1, ProviderAbortCapabilityV1, ProviderAttemptCapabilitiesV1,
        ProviderAttemptScopeV1, ProviderCredentialRecordScopeV1, ProviderHiddenRetryCapabilityV1,
        ProviderOutputReservationSourceV1, plan_provider_attempt_v1,
    };

    const BUCKETS: [ModelTurnBucketKind; 4] = [
        ModelTurnBucketKind::Request,
        ModelTurnBucketKind::Input,
        ModelTurnBucketKind::Output,
        ModelTurnBucketKind::Combined,
    ];
    let scope = || ProviderAttemptScopeV1 {
        credential: ProviderCredentialRecordScopeV1::from_credential_record_id("cfni-credential"),
        provider_id: CFNI_PROVIDER.to_owned(),
        model_id: CFNI_MODEL.to_owned(),
    };
    let capabilities = ProviderAttemptCapabilitiesV1 {
        hidden_retries: ProviderHiddenRetryCapabilityV1::Disabled,
        abort: ProviderAbortCapabilityV1::Supported,
    };
    let body = vec![b'x'; 300];
    let units = |plan: &djinn_provider::ProviderAttemptPlanV1, kind: ModelTurnBucketKind| {
        plan.debits
            .iter()
            .find(|debit| debit.bucket_kind == kind)
            .map(|debit| debit.units)
    };

    // Byte fallback dominates: ceil(300/3) = 100, plus 15% = 115.
    let plan = plan_provider_attempt_v1(
        scope(),
        Some(&body),
        Some(10),
        Some(256),
        Some(1_024),
        BUCKETS,
        capabilities,
    )
    .expect("a covered route must plan");
    assert_eq!(units(&plan, ModelTurnBucketKind::Request), Some(1));
    assert_eq!(
        units(&plan, ModelTurnBucketKind::Input),
        Some(115),
        "the estimate is the greater of the provider estimate and bytes/3, plus 15%"
    );
    assert_eq!(
        units(&plan, ModelTurnBucketKind::Output),
        Some(256),
        "an explicit output limit is reserved verbatim"
    );
    assert_eq!(
        units(&plan, ModelTurnBucketKind::Combined),
        Some(115 + 256),
        "the combined bucket binds input plus output"
    );
    assert_eq!(
        plan.output_reservation_source,
        ProviderOutputReservationSourceV1::ExplicitLimit
    );

    // The provider estimate is load-bearing when it exceeds the fallback.
    let richer = plan_provider_attempt_v1(
        scope(),
        Some(&body),
        Some(1_000),
        Some(256),
        Some(1_024),
        BUCKETS,
        capabilities,
    )
    .expect("a covered route must plan");
    assert_eq!(
        units(&richer, ModelTurnBucketKind::Input),
        Some(1_150),
        "a provider estimate above the byte fallback must win"
    );

    // No explicit limit: the model default is used, and it is capped.
    let defaulted = plan_provider_attempt_v1(
        scope(),
        Some(&body),
        None,
        None,
        Some(1_000_000),
        BUCKETS,
        capabilities,
    )
    .expect("a covered route must plan");
    assert_eq!(
        units(&defaulted, ModelTurnBucketKind::Output),
        Some(MAX_OUTPUT_RESERVATION_UNITS_V1),
        "an unbounded model default is capped at 16,384"
    );
    assert_eq!(
        defaulted.output_reservation_source,
        ProviderOutputReservationSourceV1::ModelDefault
    );

    // Uncomputable requests fail closed rather than reserving nothing.
    assert!(
        plan_provider_attempt_v1(
            scope(),
            None,
            Some(10),
            Some(256),
            Some(1_024),
            BUCKETS,
            capabilities,
        )
        .is_err(),
        "a request whose bytes cannot be serialized is uncovered, not free"
    );

    // ── One remaining unit admits exactly one of two callers ──────────────
    let db = djinn_coordinator::test_helpers::create_test_db();
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    let pool_id = cfni_seed_pool(&db, "cfni-reservation", "enforce").await;
    // Target 4, so concurrency is not what settles this: the bucket is.
    repository
        .set_pool_learned_concurrency_for_test(pool_id, 4)
        .await
        .expect("raise the learned target");
    repository
        .seed_request_bucket_binding_for_test(pool_id, 4, 1)
        .await
        .expect("seed one remaining request unit");

    let acquire = |request_id: &'static str| {
        let repository = repository.clone();
        async move {
            repository
                .acquire_turn(ModelTurnAcquireInput {
                    pool_id,
                    request_id: request_id.to_owned(),
                    owner_pod_uid: Some("pod-cfni-reservation".to_owned()),
                    generation: 1,
                    debits: request_debit(1),
                })
                .await
                .expect("acquisition must not error")
        }
    };
    let (first, second) = tokio::join!(acquire("cfni-unit-a"), acquire("cfni-unit-b"));
    assert_eq!(
        [&first, &second]
            .into_iter()
            .filter(|outcome| matches!(outcome, ModelTurnAcquireOutcome::Admitted { .. }))
            .count(),
        1,
        "one remaining unit admits exactly one caller; got {first:?} and {second:?}"
    );
    assert_eq!(
        model_turn_lease_count_for_pool_fixture(&db, pool_id).await,
        1,
        "and exactly one durable lease row exists"
    );
    let loser = [first, second]
        .into_iter()
        .find(|outcome| !matches!(outcome, ModelTurnAcquireOutcome::Admitted { .. }))
        .expect("one caller must have been deferred");
    assert_eq!(
        loser,
        ModelTurnAcquireOutcome::Wait(djinn_db::ModelTurnAdmissionWait::BucketUnavailable {
            bucket_kind: ModelTurnBucketKind::Request,
            available_units: 0,
            required_units: 1,
            reset_at: None,
        }),
        "the loser receives a typed wait naming the bucket it needs"
    );

    // ── The API-key response normalizer, end to end ───────────────────────
    //
    // Six of this criterion's nine named clauses — cold-start discovery, usage
    // reconciliation, retry-after, reset epochs, stale headers and malformed
    // headers, plus reactive-only Gemini — used to be discharged in
    // `djinn-provider/tests/model_turn_normalizer.rs`, which the fixed command
    // does not run. `ProviderApiKeyNormalizerV1` appeared zero times in this
    // target. Task `kcso` brought them here. No production change was needed:
    // the type is already `pub` and `djinn-provider` is already a dependency,
    // so what follows drives the production normalizer directly.
    {
        use djinn_provider::{
            ProviderAdmissionPolicyV1, ProviderApiKeyNormalizerV1, ProviderDiscoveryOwnershipV1,
            ProviderObservationIgnoreReasonV1, ProviderReceiptTimeV1, ProviderUsageObservationV1,
        };

        /// A complete capacity header set: an explicit reset epoch and the four
        /// remaining-unit buckets. `request` varies per observation because it
        /// is what the growth and staleness rules below are read from.
        fn headers(
            epoch: &'static str,
            request: &'static str,
        ) -> [(&'static str, &'static str); 5] {
            [
                ("x-ratelimit-remaining-requests", request),
                ("x-ratelimit-remaining-input-tokens", "8"),
                ("x-ratelimit-remaining-output-tokens", "4"),
                ("x-ratelimit-remaining-tokens", "12"),
                ("x-ratelimit-reset", epoch),
            ]
        }
        /// A fixed receipt instant, so every retry-after deadline below is an
        /// exact number rather than a range: no wall clock enters the result.
        fn receipt() -> ProviderReceiptTimeV1 {
            ProviderReceiptTimeV1 {
                wall: std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000),
                monotonic_ms: 77,
            }
        }

        // ── Cold-start discovery and usage reconciliation ─────────────────
        let mut normalizer = ProviderApiKeyNormalizerV1::new(ProviderAdmissionPolicyV1::Proactive);
        assert_eq!(
            normalizer.discovery_ownership(),
            ProviderDiscoveryOwnershipV1::DiscoveryRequired,
            "a credential with no observed capacity has not discovered anything \
             yet, so it may not enforce against an assumed limit"
        );
        assert_eq!(
            normalizer.claim_discovery(1),
            ProviderDiscoveryOwnershipV1::DiscoveryOwned {
                request_sequence: 1
            },
            "exactly one request owns the cold-start probe"
        );
        let first = normalizer.observe(
            1,
            &headers("10", "2"),
            ProviderUsageObservationV1 {
                input_units: Some(3),
                output_units: Some(5),
                combined_units: Some(8),
            },
            receipt(),
        );
        assert_eq!(
            first
                .authoritative_usage
                .expect("a complete usage observation is authoritative")
                .request_units,
            1,
            "usage reconciliation attributes exactly one request unit to the \
             attempt that produced it"
        );
        assert_eq!(
            first
                .available_capacity
                .expect("complete headers establish capacity")
                .request_units,
            2,
        );
        assert_eq!(
            first.discovery,
            ProviderDiscoveryOwnershipV1::Known,
            "once capacity is observed the credential is no longer in cold start"
        );

        // ── Stale headers: an older response may lower capacity, never raise it
        let stale_growth = normalizer.observe(
            0,
            &headers("10", "99"),
            ProviderUsageObservationV1::default(),
            receipt(),
        );
        assert_eq!(
            stale_growth.ignored,
            Some(ProviderObservationIgnoreReasonV1::Stale),
            "an out-of-order response may not grow enforceable capacity"
        );
        assert_eq!(stale_growth.available_capacity, None);
        let stale_decrease = normalizer.observe(
            0,
            &headers("10", "1"),
            ProviderUsageObservationV1::default(),
            receipt(),
        );
        assert_eq!(
            stale_decrease.ignored, None,
            "but the same out-of-order response may still lower it"
        );
        assert_eq!(
            stale_decrease
                .available_capacity
                .expect("a lowering observation applies")
                .request_units,
            1,
        );
        let decreased = normalizer.observe(
            2,
            &headers("10", "1"),
            ProviderUsageObservationV1::default(),
            receipt(),
        );
        assert_eq!(
            decreased
                .available_capacity
                .expect("an in-order observation applies")
                .request_units,
            1,
            "and the growth watermark did not move backwards behind it"
        );

        // ── Reset epochs: a larger epoch starts a window, a smaller one is a
        //    regression and is refused outright ─────────────────────────────
        let mut epochs = ProviderApiKeyNormalizerV1::new(ProviderAdmissionPolicyV1::Proactive);
        epochs.observe(
            4,
            &headers("10", "2"),
            ProviderUsageObservationV1::default(),
            receipt(),
        );
        let reset = epochs.observe(
            5,
            &headers("11", "9"),
            ProviderUsageObservationV1::default(),
            receipt(),
        );
        assert_eq!(
            reset.reset_epoch,
            Some(11),
            "a larger explicit epoch authoritatively opens a new window"
        );
        let regressing = epochs.observe(
            6,
            &headers("9", "20"),
            ProviderUsageObservationV1::default(),
            receipt(),
        );
        assert_eq!(
            regressing.ignored,
            Some(ProviderObservationIgnoreReasonV1::Regressing),
            "an epoch that runs backwards is not a new window"
        );

        // ── Malformed and incomplete headers fail closed ──────────────────
        let incomplete = epochs.observe(
            7,
            &[("x-ratelimit-remaining-requests", "1")],
            ProviderUsageObservationV1::default(),
            receipt(),
        );
        assert_eq!(
            incomplete.ignored,
            Some(ProviderObservationIgnoreReasonV1::Incomplete),
            "a partial header set establishes no capacity at all"
        );
        let impossible = epochs.observe(
            8,
            &headers("11", "-1"),
            ProviderUsageObservationV1::default(),
            receipt(),
        );
        assert_eq!(
            impossible.ignored,
            Some(ProviderObservationIgnoreReasonV1::Impossible),
            "negative remaining units are not a small number"
        );
        let malformed = epochs.observe(
            9,
            &headers("not-an-epoch", "1"),
            ProviderUsageObservationV1::default(),
            receipt(),
        );
        assert_eq!(
            malformed.ignored,
            Some(ProviderObservationIgnoreReasonV1::Malformed),
        );
        assert_eq!(
            malformed.diagnostics.malformed, 1,
            "and the refusal is counted, so a provider that always sends \
             garbage is visible rather than silently ignored"
        );

        // ── retry-after: delta seconds, HTTP-date, and overflow ───────────
        let delta = epochs.observe(
            10,
            &[("retry-after", "2")],
            ProviderUsageObservationV1::default(),
            receipt(),
        );
        assert_eq!(
            delta.retry_after_deadline_monotonic_ms,
            Some(2_077),
            "delta seconds are measured from the receipt's own monotonic \
             reading, never from a fresh clock read"
        );
        let date = epochs.observe(
            11,
            &[("retry-after", "Thu, 01 Jan 1970 00:16:42 GMT")],
            ProviderUsageObservationV1::default(),
            receipt(),
        );
        assert_eq!(
            date.retry_after_deadline_monotonic_ms,
            Some(2_077),
            "and an HTTP-date form naming the same instant produces the same \
             deadline"
        );
        let overflow = epochs.observe(
            12,
            &[("retry-after", "18446744073709551615")],
            ProviderUsageObservationV1::default(),
            ProviderReceiptTimeV1 {
                monotonic_ms: u64::MAX,
                ..receipt()
            },
        );
        assert_eq!(
            overflow.retry_after_deadline_monotonic_ms, None,
            "a deadline that cannot be represented is no deadline"
        );
        assert_eq!(
            overflow.ignored,
            Some(ProviderObservationIgnoreReasonV1::Impossible)
        );
        assert_eq!(overflow.diagnostics.impossible, 2);
        let unparsable = epochs.observe(
            13,
            &[("retry-after", "banana")],
            ProviderUsageObservationV1::default(),
            receipt(),
        );
        assert_eq!(
            unparsable.ignored,
            Some(ProviderObservationIgnoreReasonV1::Malformed)
        );
        assert_eq!(unparsable.diagnostics.malformed, 2);

        // ── Reactive-only Gemini: headers never establish capacity ────────
        let mut gemini =
            ProviderApiKeyNormalizerV1::new(ProviderAdmissionPolicyV1::ReactiveOnlyTarget1);
        let observation = gemini.observe(
            1,
            &headers("10", "99"),
            ProviderUsageObservationV1::default(),
            receipt(),
        );
        assert_eq!(
            observation.available_capacity, None,
            "a reactive-only route may not be given a proactive capacity from \
             headers, however complete they are"
        );
        assert_eq!(
            observation.discovery,
            ProviderDiscoveryOwnershipV1::DiscoveryRequired,
            "and it never leaves cold start on the strength of them"
        );
        let wait = gemini.observe(
            2,
            &[("retry-after", "1")],
            ProviderUsageObservationV1::default(),
            receipt(),
        );
        assert_eq!(
            wait.retry_after_deadline_monotonic_ms,
            Some(1_077),
            "what it does honour is the provider telling it to wait"
        );
    }

    close(db).await;
}

// ─── Group 4: identity eligibility and non-identifying telemetry ───────────

/// Enforcement is keyed by the durable credential record, and an ambiguous or
/// colliding record can never enforce.
///
/// * **The key is `(credential_record_id, provider, model_scope)`.** Two pools
///   are seeded for the same provider/model under different credential
///   records; `resolve_pool` returns a different pool id for each, and neither
///   resolves under the other's credential. That is the pool identity claim,
///   settled against the production lookup.
/// * **Secret rotation preserves controller state.** Rotating the stored
///   secret does not touch the pool row, so a pool that already carries a
///   learned target keeps it. The rotation is applied through the production
///   `CredentialRepository`, and the learned target is read back from the
///   pool.
/// * **Ambiguous, colliding and revoked records never enforce.** For each
///   state, the production `acquire_turn` against an `enforce` pool is
///   rejected as `IneligibleIdentity` and writes zero lease rows, and
///   `set_pool_mode_in_transaction` refuses the advance with
///   `identity_ineligible`. The row count is the proof, not the enum.
/// * **Telemetry carries no identity.** The pool's own series are emitted
///   through the production seam inside a fixture-local recorder, and the
///   render is searched for the credential record id, the request id and the
///   lease id that this scenario actually created. Absence of a string that
///   was never created would prove nothing; these were.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scenario_04_identity_is_keyed_by_the_credential_record_and_never_leaks() {
    use djinn_db::{
        ModelTurnAdmissionPhase, ModelTurnIdentityState, ModelTurnModeChangeInput,
        ModelTurnModeChangeOutcome, ModelTurnModeChangeReason, ModelTurnModeChangeRejection,
    };

    let db = djinn_coordinator::test_helpers::create_test_db();
    let events = djinn_core::events::EventBus::noop();
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    const RECORD_A: &str = "cfni-credential-record-a";
    const RECORD_B: &str = "cfni-credential-record-b";
    let pool_a = cfni_seed_pool(&db, RECORD_A, "enforce").await;
    let pool_b = cfni_seed_pool(&db, RECORD_B, "shadow").await;
    assert_ne!(pool_a, pool_b);
    repository
        .seed_request_bucket_binding_for_test(pool_a, 4, 4)
        .await
        .expect("seed the request binding");

    // The pool key is the credential record, not the provider alone.
    for (record, expected) in [(RECORD_A, pool_a), (RECORD_B, pool_b)] {
        let resolved = repository
            .resolve_pool(record, CFNI_PROVIDER, CFNI_MODEL)
            .await
            .expect("resolve must not error")
            .expect("the seeded pool must resolve");
        assert_eq!(
            resolved.id, expected,
            "credential record {record} must resolve to its own pool"
        );
        assert_eq!(resolved.credential_id, record);
    }
    assert!(
        repository
            .resolve_pool("cfni-credential-record-absent", CFNI_PROVIDER, CFNI_MODEL)
            .await
            .expect("resolve must not error")
            .is_none(),
        "an unknown credential record resolves to no pool at all"
    );

    // Secret rotation within a record preserves the pool's controller state.
    repository
        .set_pool_learned_concurrency_for_test(pool_a, 5)
        .await
        .expect("give the pool a learned target to preserve");
    // The seam writes by `key_name`, which is what makes this a rotation
    // rather than a replacement: the row keeps its id — the credential record
    // identity the pool is keyed by — and only its secret changes.
    let credentials = djinn_db::CredentialRepository::new(db.clone(), events.clone());
    let rotated = credentials
        .set(
            CFNI_PROVIDER,
            &format!("model-turn-admission-fixture-{RECORD_A}"),
            "rotated-secret",
        )
        .await
        .expect("rotate the stored secret inside the same credential record");
    assert_eq!(
        rotated.id, RECORD_A,
        "rotation must keep the credential record identity the pool is keyed by"
    );
    assert_eq!(
        credentials
            .get_decrypted(&format!("model-turn-admission-fixture-{RECORD_A}"))
            .await
            .expect("read the rotated secret")
            .as_deref(),
        Some("rotated-secret"),
        "the secret really did change"
    );
    let after_rotation = repository
        .resolve_pool(RECORD_A, CFNI_PROVIDER, CFNI_MODEL)
        .await
        .expect("resolve must not error")
        .expect("the pool survives rotation");
    assert_eq!(
        after_rotation.id, pool_a,
        "rotating the secret must not replace the pool"
    );
    assert_eq!(
        after_rotation.learned_concurrency, 5,
        "rotating the secret must preserve the learned target"
    );

    // Ambiguous, colliding and revoked records never enforce.
    let leases_before = model_turn_lease_total_count_fixture(&db).await;
    for state in [
        ModelTurnIdentityState::Ambiguous,
        ModelTurnIdentityState::Colliding,
        ModelTurnIdentityState::Revoked,
    ] {
        repository
            .set_pool_identity_for_test(pool_a, state)
            .await
            .expect("set the durable identity state");
        let outcome = repository
            .acquire_turn(ModelTurnAcquireInput {
                pool_id: pool_a,
                request_id: format!("cfni-identity-{state:?}"),
                owner_pod_uid: Some("pod-cfni-identity".to_owned()),
                generation: 1,
                debits: request_debit(1),
            })
            .await
            .expect("acquisition must not error");
        assert_eq!(
            outcome,
            ModelTurnAcquireOutcome::Rejected(
                djinn_db::ModelTurnAdmissionRejection::IneligibleIdentity { state }
            ),
            "identity {state:?} must be refused at the acquisition boundary"
        );
        assert_eq!(
            model_turn_lease_total_count_fixture(&db).await,
            leases_before,
            "a refused identity writes no lease row anywhere"
        );

        // And the durable mode writer refuses the advance for the same reason.
        repository
            .set_pool_compatibility_phase_for_test(pool_b, djinn_db::ModelTurnCompatibilityPhase::D)
            .await
            .expect("stand the shadow pool at compatibility phase d");
        repository
            .set_pool_identity_for_test(pool_b, state)
            .await
            .expect("set the durable identity state");
        assert_eq!(
            repository
                .set_pool_mode_in_transaction(ModelTurnModeChangeInput {
                    pool_id: pool_b,
                    target_mode: ModelTurnAdmissionPhase::Enforce,
                    reason: ModelTurnModeChangeReason::OperatorRequest,
                    controller_generation: CFNI_GENERATION,
                })
                .await
                .expect("mode change must not error"),
            ModelTurnModeChangeOutcome::Rejected(
                ModelTurnModeChangeRejection::IdentityIneligible { state }
            ),
            "identity {state:?} must be refused at the mode writer too"
        );
    }

    // Restore eligibility and prove the instrument: the same pool does admit,
    // so the zeroes above are observations rather than a blind spot.
    repository
        .set_pool_identity_for_test(pool_a, ModelTurnIdentityState::Eligible)
        .await
        .expect("restore eligibility");
    const CONTROL_REQUEST: &str = "cfni-identity-control-request";
    let ModelTurnAcquireOutcome::Admitted { lease, .. } = repository
        .acquire_turn(ModelTurnAcquireInput {
            pool_id: pool_a,
            request_id: CONTROL_REQUEST.to_owned(),
            owner_pod_uid: Some("pod-cfni-identity".to_owned()),
            generation: 1,
            debits: request_debit(1),
        })
        .await
        .expect("acquisition must not error")
    else {
        panic!("an eligible identity must admit");
    };
    assert_eq!(
        model_turn_lease_total_count_fixture(&db).await,
        leases_before + 1
    );

    // ── Telemetry carries no identity ─────────────────────────────────────
    let catalog = cfni_catalog();
    let pool = repository
        .pool_by_id(pool_a)
        .await
        .expect("read the pool")
        .expect("the pool exists");
    let ((), rendered) = with_fixture_local_recorder(async || {
        let emitted = djinn_coordinator::model_turn_admission::enforcement::emit_pool_series_v1(
            &repository,
            &catalog,
            &pool,
            &now_rfc3339(),
        )
        .await
        .expect("emit the pool series");
        assert!(
            emitted,
            "the catalog resolves this route, so the series must be emitted"
        );
    })
    .await;
    assert!(
        rendered.contains("djinn_model_turn_pool_target"),
        "the fixture-local recorder must have captured the production emission; \
         rendered:\n{rendered}"
    );
    for secret in [
        RECORD_A,
        CONTROL_REQUEST,
        lease.identity.lease_id.as_str(),
        "rotated-secret",
    ] {
        assert!(
            !rendered.contains(secret),
            "model-turn telemetry must not carry {secret}; rendered:\n{rendered}"
        );
    }

    close(db).await;
}

// ─── Group 5: the aggregate output-rate goldens ────────────────────────────

/// The normative rate traces, and the union-of-wall-clock rule that makes them
/// differ from summed stream-seconds.
///
/// Each of the three traces named in the proposal is computed by the
/// production `aggregate_output_throughput_v1`. Two of them would be wrong
/// under a summed-stream-seconds denominator, and the scenario computes that
/// wrong answer explicitly and asserts the production one is not it — so the
/// union rule is load-bearing rather than accidentally agreeing.
///
/// Clipping is then asserted at both edges of the half-open window, including
/// the case that distinguishes "assigned by emission timestamp" from "assigned
/// by stream": a stream still active at the boundary whose emission lands on
/// `end` contributes nothing to this window.
#[test]
fn scenario_05_aggregate_output_rate_divides_by_the_union_of_active_wall_clock() {
    use djinn_coordinator::model_turn_admission::AlignedPhaseCWindowV1;
    use djinn_coordinator::model_turn_admission::subscription_learner::{
        ActiveStreamV1, OutputTokenEmissionV1, aggregate_output_throughput_v1,
    };

    const START: i64 = 120;
    const END: i64 = 180;
    let window = AlignedPhaseCWindowV1::new(START).expect("aligned window");
    /// A stream active over `[start, end)` emitting `tokens` one second at a
    /// time, so tokens really are attributed by emission timestamp.
    fn stream(start: i64, end: i64, tokens: i64) -> ActiveStreamV1 {
        let seconds = end - start;
        assert!(
            seconds > 0 && tokens % seconds == 0,
            "fixture must divide evenly"
        );
        ActiveStreamV1 {
            started_at_second: start,
            ended_at_second: end,
            emissions: (start..end)
                .map(|emitted_at_second| OutputTokenEmissionV1 {
                    emitted_at_second,
                    output_tokens: tokens / seconds,
                })
                .collect(),
        }
    }

    // Trace 1: one stream, the whole window, 6,000 tokens → 100/s.
    let single = aggregate_output_throughput_v1(window, &[stream(START, END, 6_000)]);
    assert_eq!(single.output_tokens, 6_000);
    assert_eq!(single.active_union_seconds, 60);
    assert_eq!(single.tokens_per_second, 100.0);

    // Trace 2: two fully overlapping streams, 3,000 each → also 100/s, and
    // therefore a plateau against trace 1 rather than growth.
    let overlapping = [stream(START, END, 3_000), stream(START, END, 3_000)];
    let plateau = aggregate_output_throughput_v1(window, &overlapping);
    assert_eq!(plateau.output_tokens, 6_000);
    assert_eq!(
        plateau.active_union_seconds, 60,
        "two fully overlapping streams occupy 60 wall-clock seconds, not 120"
    );
    assert_eq!(plateau.tokens_per_second, 100.0);
    assert_eq!(
        plateau.tokens_per_second, single.tokens_per_second,
        "the proposal's plateau trace: concurrency alone is not throughput"
    );
    let summed_stream_seconds: i64 = overlapping
        .iter()
        .map(|stream| stream.ended_at_second - stream.started_at_second)
        .sum();
    assert_eq!(summed_stream_seconds, 120);
    assert_ne!(
        plateau.tokens_per_second,
        plateau.output_tokens as f64 / summed_stream_seconds as f64,
        "summing stream-seconds would have reported 50/s; the union rule is \
         what produces 100/s"
    );

    // Trace 3: two overlapping streams, 4,200 each → 140/s, real growth.
    let growth = aggregate_output_throughput_v1(
        window,
        &[stream(START, END, 4_200), stream(START, END, 4_200)],
    );
    assert_eq!(growth.output_tokens, 8_400);
    assert_eq!(growth.active_union_seconds, 60);
    assert_eq!(growth.tokens_per_second, 140.0);
    assert!(growth.tokens_per_second > plateau.tokens_per_second * 1.05);

    // Crossing streams clip to the half-open window and tile it exactly once.
    let crossing =
        aggregate_output_throughput_v1(window, &[stream(90, 150, 6_000), stream(150, 210, 6_000)]);
    assert_eq!(crossing.active_union_seconds, 60);
    assert_eq!(crossing.output_tokens, 6_000);
    assert_eq!(crossing.tokens_per_second, 100.0);

    // The window is half-open at both ends.
    for outside in [stream(60, START, 6_000), stream(END, 240, 6_000)] {
        let throughput = aggregate_output_throughput_v1(window, &[outside]);
        assert_eq!(throughput.active_union_seconds, 0);
        assert_eq!(throughput.output_tokens, 0);
    }

    // A token emitted at `end` belongs to the next window even though its
    // stream is still active — attribution is by emission timestamp.
    let boundary = ActiveStreamV1 {
        started_at_second: START,
        ended_at_second: 240,
        emissions: vec![
            OutputTokenEmissionV1 {
                emitted_at_second: END - 1,
                output_tokens: 6_000,
            },
            OutputTokenEmissionV1 {
                emitted_at_second: END,
                output_tokens: 999_999,
            },
        ],
    };
    let clipped = aggregate_output_throughput_v1(window, &[boundary]);
    assert_eq!(clipped.output_tokens, 6_000);
    assert_eq!(clipped.active_union_seconds, 60);
    assert_eq!(clipped.tokens_per_second, 100.0);
}

// ─── Group 6: the controller ladder ────────────────────────────────────────

/// The golden controller trace, end to end, through the production folder.
///
/// One state is walked through the whole ladder the proposal specifies:
/// baseline, eight qualifying probes to 9, one deduplicated loss to 8, a
/// duplicate loss that holds, an unqualified window and an ineligible window
/// that hold and do not even move the baseline, three non-growing probes that
/// suspend probing, five held plateau windows that growth cannot slip through,
/// and resumption on the sixth. The target is asserted after every step and is
/// separately driven past both ends of `[1, 32]`.
///
/// The last claim of the criterion — that no user-wide or per-pool emergency
/// cap is read — is a *closed shape* rather than a sampled absence, so it is
/// asserted over the controller's own source: neither the learner nor the
/// controller may mention the settings surfaces that would carry such a cap.
#[test]
fn scenario_06_controller_ladder_probes_backs_off_and_stays_in_bounds() {
    use djinn_coordinator::model_turn_admission::PhaseCLearnerWindowV1;
    use djinn_coordinator::model_turn_admission::subscription_learner::{
        AggregateThroughputV1, AttemptTerminalObservationV1, ControllerTransitionV1, MAX_TARGET,
        MIN_TARGET, SubscriptionControllerStateV1, SubscriptionWindowObservationV1,
        observe_window_v1,
    };
    use djinn_provider::{ProviderAttemptLossV1, ProviderAttemptTerminalV1};

    fn qualified(completed_turns: i64) -> PhaseCLearnerWindowV1 {
        PhaseCLearnerWindowV1 {
            pool_id: 1,
            window_sequence: 2,
            started_at: "1970-01-01T00:02:00Z".into(),
            ended_at: "1970-01-01T00:03:00Z".into(),
            admitted_turns: completed_turns,
            completed_turns,
        }
    }
    fn eligible_at(rate: f64) -> SubscriptionWindowObservationV1 {
        SubscriptionWindowObservationV1 {
            qualified: Some(qualified(8)),
            throughput: AggregateThroughputV1 {
                output_tokens: (rate * 60.0) as i64,
                active_union_seconds: 60,
                tokens_per_second: rate,
            },
            rate_samples: vec![rate, rate, rate],
            terminals: Vec::new(),
            bootstrap_seed: 42,
        }
    }
    fn loss(attempt: &str) -> AttemptTerminalObservationV1 {
        AttemptTerminalObservationV1 {
            attempt: attempt.to_owned(),
            terminal: ProviderAttemptTerminalV1::Failed(ProviderAttemptLossV1::RateLimited),
        }
    }

    let mut state = SubscriptionControllerStateV1::new();
    assert_eq!(state.target(), 1, "targets start at 1");

    let mut rate = 100.0;
    assert_eq!(
        observe_window_v1(&mut state, &eligible_at(rate)),
        ControllerTransitionV1::ProbeDidNotGrow,
        "the first eligible window only establishes the baseline"
    );
    assert_eq!(state.target(), 1);

    for probe in 1..=8 {
        rate *= 1.5;
        assert_eq!(
            observe_window_v1(&mut state, &eligible_at(rate)),
            ControllerTransitionV1::Grew,
            "growth probe {probe} must qualify"
        );
        assert_eq!(state.target(), 1 + probe);
    }
    assert_eq!(state.target(), 9, "eight qualifying probes reach 9");

    let mut backing_off = eligible_at(rate * 1.5);
    backing_off.terminals = vec![loss("cfni-attempt-a")];
    assert_eq!(
        observe_window_v1(&mut state, &backing_off),
        ControllerTransitionV1::BackedOff,
        "loss has precedence over the growth this same window would show"
    );
    assert_eq!(state.target(), 8, "floor(9 * 0.9) = 8");

    let mut duplicate = eligible_at(rate * 4.0);
    duplicate.terminals = vec![loss("cfni-attempt-a")];
    assert_eq!(
        observe_window_v1(&mut state, &duplicate),
        ControllerTransitionV1::HeldDuplicateLoss
    );
    assert_eq!(state.target(), 8);

    let baseline = state.baseline();
    let mut unqualified = eligible_at(rate * 4.0);
    unqualified.qualified = None;
    assert_eq!(
        observe_window_v1(&mut state, &unqualified),
        ControllerTransitionV1::HeldUnqualified
    );
    assert_eq!(state.target(), 8);
    assert_eq!(
        state.baseline(),
        baseline,
        "an unqualified window may not even move the baseline"
    );

    let mut ineligible = eligible_at(rate * 4.0);
    ineligible.qualified = Some(qualified(7));
    assert_eq!(
        observe_window_v1(&mut state, &ineligible),
        ControllerTransitionV1::HeldIneligible,
        "eight completed turns is the floor"
    );
    assert_eq!(state.target(), 8);
    assert_eq!(state.baseline(), baseline);

    // Three non-growing probes suspend probing for five windows.
    let flat = state.baseline().expect("baseline");
    for probe in 1..=2 {
        assert_eq!(
            observe_window_v1(&mut state, &eligible_at(flat)),
            ControllerTransitionV1::ProbeDidNotGrow,
            "flat probe {probe}"
        );
    }
    assert_eq!(
        observe_window_v1(&mut state, &eligible_at(flat)),
        ControllerTransitionV1::ProbeRejected,
        "the third consecutive non-growing probe suspends probing"
    );
    assert_eq!(state.remaining_hold_windows(), 5);
    let held_target = state.target();
    for held in 1..=5 {
        assert_eq!(
            observe_window_v1(&mut state, &eligible_at(flat * 100.0)),
            ControllerTransitionV1::HeldPlateau,
            "held window {held}: growth cannot slip through a plateau hold"
        );
        assert_eq!(state.target(), held_target);
        assert_eq!(state.remaining_hold_windows(), 5 - held);
    }
    assert_eq!(
        observe_window_v1(&mut state, &eligible_at(flat * 1_000.0)),
        ControllerTransitionV1::Grew,
        "probing resumes on the sixth window"
    );
    assert_eq!(state.target(), held_target + 1);

    // Neither end of `[1, 32]` can be left.
    let mut bounded = SubscriptionControllerStateV1::new();
    let mut climbing = 100.0;
    observe_window_v1(&mut bounded, &eligible_at(climbing));
    for _ in 0..64 {
        climbing *= 1.5;
        observe_window_v1(&mut bounded, &eligible_at(climbing));
        assert!((MIN_TARGET..=MAX_TARGET).contains(&bounded.target()));
    }
    assert_eq!(bounded.target(), MAX_TARGET);
    for index in 0..64 {
        let mut window = eligible_at(climbing);
        window.terminals = vec![loss(&format!("cfni-attempt-{index}"))];
        observe_window_v1(&mut bounded, &window);
        assert!((MIN_TARGET..=MAX_TARGET).contains(&bounded.target()));
    }
    assert_eq!(bounded.target(), MIN_TARGET);

    // No emergency cap is read anywhere on the controller path.
    for relative in [
        "src/model_turn_admission_subscription_learner.rs",
        "src/model_turn_admission_controller.rs",
    ] {
        let source = read_crate_source(relative);
        for forbidden in [
            "emergency",
            "UserSettingsRepository",
            "user_settings",
            "SettingsRepository",
        ] {
            assert!(
                !source.contains(forbidden),
                "{relative} must not read {forbidden}: this increment adds no \
                 user-wide or per-pool emergency cap"
            );
        }
    }
}

// ─── Group 8: breakers, the single retry owner, and idempotent cancellation ─

/// An open breaker dispatches nothing, the leader pass never resets one, a
/// retry cannot acquire a second lease before releasing its first, a
/// provider-internal retry cannot get a plan at all, and cancelling a typed
/// wait is idempotent.
///
/// * **An open breaker dispatches nothing.** A ready task is put behind the
///   exact map `dispatch_ready_tasks` consults before anything else, and the
///   production ready pass runs. The assertions are row counts — no task run,
///   no session, no `model_turn_leases` row — plus the fixture-local recorder
///   showing the pass took the `outcome="cooldown"` branch. Re-running with
///   the cooldown cleared produces no `cooldown` outcome, so that label is the
///   breaker's doing rather than the pass merely having run.
/// * **Controller actions never reset breakers.** `dispatch_state` is digested
///   before and after a leader enforcement pass that really did run (the pass
///   is asserted un-fenced first) and must be byte identical.
/// * **One retry owner.** Re-acquiring under a live lease's request id returns
///   that same lease as idempotent rather than a second one; the relation
///   still holds one row. Only after the terminal release does a fresh
///   acquisition produce a different lease.
/// * **Provider-internal retries cannot bypass admission.** The production
///   planner refuses a route whose adapter has not disabled hidden retries,
///   and one that cannot abort — so there is no plan, no debits, and nothing
///   to acquire against.
/// * **Cancelling a typed wait is idempotent.** `cancel_before_send` applies
///   once, is idempotent on replay, leaves exactly one terminal record, and
///   refunds rather than quarantines because the permit was definitely unsent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scenario_08_breakers_are_senior_and_retries_release_before_acquiring() {
    use djinn_db::{
        ModelTurnLeaseMutationOutcome, SessionRepository, TaskRepository, UserRepository,
    };
    use djinn_provider::{
        ProviderAbortCapabilityV1, ProviderAttemptCapabilitiesV1, ProviderAttemptRouteCoverageV1,
        ProviderAttemptScopeV1, ProviderAttemptUncoveredReasonV1, ProviderCredentialRecordScopeV1,
        ProviderHiddenRetryCapabilityV1, plan_provider_attempt_v1,
    };

    // ── A provider that has not disabled hidden retries gets no plan ──────
    const BUCKETS: [ModelTurnBucketKind; 4] = [
        ModelTurnBucketKind::Request,
        ModelTurnBucketKind::Input,
        ModelTurnBucketKind::Output,
        ModelTurnBucketKind::Combined,
    ];
    let scope = || ProviderAttemptScopeV1 {
        credential: ProviderCredentialRecordScopeV1::from_credential_record_id("cfni-credential"),
        provider_id: CFNI_PROVIDER.to_owned(),
        model_id: CFNI_MODEL.to_owned(),
    };
    let body = vec![b'x'; 300];
    for (capabilities, expected) in [
        (
            ProviderAttemptCapabilitiesV1 {
                hidden_retries: ProviderHiddenRetryCapabilityV1::Unsupported,
                abort: ProviderAbortCapabilityV1::Supported,
            },
            ProviderAttemptUncoveredReasonV1::HiddenRetriesNotDisabled,
        ),
        (
            ProviderAttemptCapabilitiesV1 {
                hidden_retries: ProviderHiddenRetryCapabilityV1::Disabled,
                abort: ProviderAbortCapabilityV1::Unsupported,
            },
            ProviderAttemptUncoveredReasonV1::AbortUnsupported,
        ),
    ] {
        assert_eq!(
            plan_provider_attempt_v1(
                scope(),
                Some(&body),
                Some(10),
                Some(256),
                Some(1_024),
                BUCKETS,
                capabilities,
            )
            .expect_err("an uncovered route must not produce a plan"),
            ProviderAttemptRouteCoverageV1::Uncovered(expected),
            "a route whose adapter retries below the boundary is uncovered, so \
             no debits exist for it to acquire against"
        );
    }

    // ── An open breaker dispatches nothing ────────────────────────────────
    install_github_app_config_for_dispatch();
    let db = djinn_coordinator::test_helpers::create_test_db();
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel(256);
    let events = djinn_core::events::EventBus::noop();
    let project = djinn_coordinator::test_helpers::create_test_project(&db).await;
    let github_id = 800_000
        + i64::try_from(uuid::Uuid::now_v7().as_u128() % 1_000_000).expect("bounded github id");
    let user = UserRepository::new(db.clone())
        .upsert_from_github(
            github_id,
            &format!("cfni-breaker-{}", uuid::Uuid::now_v7()),
            None,
            None,
        )
        .await
        .expect("seed task creator");
    let tasks = TaskRepository::new(db.clone(), events.clone());
    let task = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user.id.clone()), async {
            tasks
                .create_fixture_in_project(
                    &project.id,
                    None,
                    "Breaker-open conformance task",
                    "A ready task whose dispatch breaker is open.",
                    "",
                    "task",
                    2,
                    "test-owner",
                    Some("approved"),
                    None,
                )
                .await
                .expect("seed ready worker task")
        })
        .await;
    let task = tasks
        .set_status(&task.id, "open")
        .await
        .expect("make the task dispatch-ready");

    let pool_id = cfni_seed_pool(&db, "cfni-breaker", "enforce").await;
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    repository
        .seed_request_bucket_binding_for_test(pool_id, 4, 4)
        .await
        .expect("seed the request binding");

    let (mut actor, cancel) =
        djinn_coordinator::test_helpers::make_coordinator_actor_cancellable(&db, &events_tx);
    djinn_coordinator::test_helpers::set_dispatch_cooldown_for_test(
        &mut actor,
        &task.id,
        Some(std::time::Duration::from_secs(3_600)),
    );
    let ((), open_render) = with_fixture_local_recorder(async || {
        djinn_coordinator::test_helpers::run_dispatch_ready_tasks(&mut actor, Some(&project.id))
            .await;
    })
    .await;
    assert!(
        open_render.contains("djinn_dispatch_attempts_total")
            && open_render.contains("outcome=\"cooldown\""),
        "the pass must have reached the open breaker and taken the cooldown \
         branch; rendered:\n{open_render}"
    );
    assert_eq!(
        djinn_coordinator::test_helpers::dispatched_count(&actor),
        0,
        "an open breaker must dispatch nothing"
    );
    assert_eq!(
        model_turn_lease_total_count_fixture(&db).await,
        0,
        "a task that never dispatched never reaches the provider attempt \
         boundary, so it writes no model_turn_leases row"
    );
    assert!(
        task_run_ids_for_task(&db, &task.id).await.is_empty(),
        "an open breaker creates no task run"
    );
    assert_eq!(
        SessionRepository::new(db.clone(), events.clone())
            .list_for_task(&task.id)
            .await
            .expect("list sessions")
            .len(),
        0,
        "an open breaker starts no session"
    );

    // Non-vacuity: with the breaker closed the same pass on the same task
    // records no cooldown outcome at all, so the label above was the breaker's
    // doing rather than something every pass emits.
    djinn_coordinator::test_helpers::set_dispatch_cooldown_for_test(&mut actor, &task.id, None);
    let ((), closed_render) = with_fixture_local_recorder(async || {
        djinn_coordinator::test_helpers::run_dispatch_ready_tasks(&mut actor, Some(&project.id))
            .await;
    })
    .await;
    assert!(
        !closed_render.contains("outcome=\"cooldown\""),
        "with the breaker closed the pass must not report a cooldown outcome; \
         rendered:\n{closed_render}"
    );
    cancel.cancel();

    // ── A leader enforcement pass leaves breaker state byte identical ─────
    //
    // The pool is at admission mode `enforce` with a live cooldown row sitting
    // in `dispatch_state`, and the pass is driven with `window_trainable`
    // true — the most permissive input it can be given. Its own reported
    // denial is what proves it reached the enforcement decision rather than
    // returning early, so the unchanged digest below is a real observation.
    cfni_register_incarnation(&db).await;
    let leader_pool = cfni_seed_pool(&db, "cfni-breaker-leader", "shadow").await;
    cfni_cover_route(&repository, leader_pool).await;
    // A durable breaker row, so the digest below compares real content rather
    // than two absences.
    djinn_db::test_support::seed_breaker_open_dispatch_state(&db, &task.id, "cfni/model", 30).await;
    let before = djinn_db::test_support::table_digest_for_test(&db, "dispatch_state").await;
    assert_ne!(
        before, "empty",
        "the breaker relation must actually hold a row, or an unchanged digest \
         is a comparison between two absences"
    );
    let outcome = djinn_coordinator::model_turn_admission::enforcement::run_enforcement_pass_v1(
        &repository,
        &cfni_fence(),
        CFNI_GENERATION,
        &now_rfc3339(),
        &std::collections::BTreeMap::from([(leader_pool, vec![cfni_expected_key()])]),
        true,
    )
    .await
    .expect("enforcement pass must not error");
    assert!(
        !outcome.fenced,
        "the registered incarnation must hold the fence, or nothing ran at all"
    );
    assert_eq!(
        outcome.denials,
        vec![(leader_pool, "compatibility_phase_insufficient")],
        "the pass must have reached and recorded its enforcement decision, so \
         the unchanged digest below is an observation rather than the absence \
         of a pass"
    );
    assert_eq!(
        djinn_db::test_support::table_digest_for_test(&db, "dispatch_state").await,
        before,
        "the leader pass must not touch breaker or backoff state"
    );

    // ── One retry owner: release before acquiring afresh ──────────────────
    let retry_pool = cfni_seed_pool(&db, "cfni-retry", "enforce").await;
    repository
        .seed_request_bucket_binding_for_test(retry_pool, 4, 4)
        .await
        .expect("seed the request binding");
    const RETRY_REQUEST: &str = "cfni-retry-request";
    let acquire = || {
        repository.acquire_turn(ModelTurnAcquireInput {
            pool_id: retry_pool,
            request_id: RETRY_REQUEST.to_owned(),
            owner_pod_uid: Some("pod-cfni-retry".to_owned()),
            generation: 1,
            debits: request_debit(1),
        })
    };
    let ModelTurnAcquireOutcome::Admitted { lease: first, .. } =
        acquire().await.expect("acquisition must not error")
    else {
        panic!("the retry pool must admit");
    };
    let ModelTurnAcquireOutcome::Admitted {
        lease: replayed,
        idempotent,
        ..
    } = acquire().await.expect("acquisition must not error")
    else {
        panic!("a replay under a live lease must return that same lease");
    };
    assert!(
        idempotent,
        "a retry that has not released its lease gets the same lease back"
    );
    assert_eq!(replayed.identity.lease_id, first.identity.lease_id);
    assert_eq!(
        model_turn_lease_count_for_pool_fixture(&db, retry_pool).await,
        1,
        "a retry that has not reconciled cannot create a second lease"
    );

    // ── Cancelling a typed wait is idempotent ─────────────────────────────
    assert_eq!(
        repository
            .mark_dispatching(&first.identity)
            .await
            .expect("mark dispatching"),
        ModelTurnLeaseMutationOutcome::Applied
    );
    let terminals_before =
        djinn_db::test_support::count_rows_for_test(&db, "model_turn_lease_terminals").await;
    assert_eq!(
        repository
            .cancel_before_send(first.identity.clone())
            .await
            .expect("cancel before send"),
        ModelTurnLeaseMutationOutcome::Applied
    );
    assert_eq!(
        repository
            .cancel_before_send(first.identity.clone())
            .await
            .expect("replay cancel before send"),
        ModelTurnLeaseMutationOutcome::Idempotent,
        "cancellation of a typed wait is idempotent"
    );
    assert_eq!(
        djinn_db::test_support::count_rows_for_test(&db, "model_turn_lease_terminals").await,
        terminals_before + 1,
        "two cancellations of one lease store exactly one terminal record"
    );
    assert_eq!(
        djinn_db::test_support::model_turn_terminal_fixture(
            &db,
            &first.identity.lease_id,
            first.identity.generation,
            &first.identity.request_id,
        )
        .await,
        ("cancelled".to_owned(), "refunded".to_owned()),
        "a permit dropped before send is definitely unsent and is refunded"
    );

    // The released request id still replays to its own terminal lease — that
    // is the crash-safety guarantee, and it must not mint a second row.
    let ModelTurnAcquireOutcome::Admitted {
        lease: replayed_after_release,
        ..
    } = acquire().await.expect("acquisition must not error")
    else {
        panic!("a replay of a released request id must return its own lease");
    };
    assert_eq!(
        replayed_after_release.identity.lease_id,
        first.identity.lease_id
    );
    assert_eq!(
        model_turn_lease_count_for_pool_fixture(&db, retry_pool).await,
        1,
        "replaying a released request id creates no second lease"
    );

    // The retry itself is a new attempt with its own stable request id, and
    // it gets a genuinely fresh lease now that the old one has been released.
    let ModelTurnAcquireOutcome::Admitted { lease: fresh, .. } = repository
        .acquire_turn(ModelTurnAcquireInput {
            pool_id: retry_pool,
            request_id: "cfni-retry-request-attempt-2".to_owned(),
            owner_pod_uid: Some("pod-cfni-retry".to_owned()),
            generation: 1,
            debits: request_debit(1),
        })
        .await
        .expect("acquisition must not error")
    else {
        panic!("the released pool must admit the retry");
    };
    assert_ne!(
        fresh.identity.lease_id, first.identity.lease_id,
        "the retry acquires a fresh lease, it does not reuse the released one"
    );
    assert_eq!(
        model_turn_lease_count_for_pool_fixture(&db, retry_pool).await,
        2,
        "one released lease and one fresh lease"
    );

    close(db).await;
}

// ─── Group 9: the A→D compatibility phases and their prerequisites ─────────

/// The whole ready state one guarded phase request needs, so that a case can
/// remove exactly one prerequisite and leave the rest standing.
struct CompatibilityFixture {
    pool_id: i64,
    expected: std::collections::BTreeMap<i64, Vec<djinn_db::ModelTurnExpectedPathKey>>,
    fence: djinn_db::ModelTurnControllerFence,
    evaluated_at: String,
}

/// Build a pool for which every A→B predicate holds.
///
/// Coverage and the complete observation chain are written through the
/// production writers, not inserted; the identity and the schema marker are
/// what the migration already installs. The only thing seeded out of band is
/// the *existence* of a complete attempt chain, which production cannot
/// currently produce — Phase B never stored a coverage interval or an
/// authoritative usage column — and that is exactly the prerequisite the
/// individual cases below then remove again.
async fn cfni_ready_compatibility_fixture(
    db: &Database,
    repository: &ModelTurnAdmissionRepository,
    name: &str,
) -> CompatibilityFixture {
    let pool_id = cfni_seed_pool(db, name, "shadow").await;
    cfni_cover_route(repository, pool_id).await;
    cfni_record_chain(
        repository,
        pool_id,
        &format!("sha256:{:064x}", pool_id as u128),
        &CFNI_CHAIN,
    )
    .await;
    CompatibilityFixture {
        pool_id,
        expected: std::collections::BTreeMap::from([(pool_id, vec![cfni_expected_key()])]),
        fence: cfni_fence(),
        evaluated_at: now_rfc3339(),
    }
}

/// The persisted predicate verdicts of the pool's only phase decision.
///
/// Every case below reads this immediately after its single denial, so the
/// ledger must hold exactly one row — which also pins "a denial writes exactly
/// one row" rather than assuming it.
async fn cfni_last_predicates(
    repository: &ModelTurnAdmissionRepository,
    pool_id: i64,
) -> djinn_db::ModelTurnPhasePredicateResults {
    let rows = repository
        .phase_transitions(pool_id, 8)
        .await
        .expect("read the phase ledger");
    assert_eq!(
        rows.len(),
        1,
        "a denied phase request appends exactly one decision row"
    );
    rows[0].3.clone()
}

/// Every prerequisite of a compatibility phase advance is independently
/// load-bearing, incomplete Phase-C coverage is diagnostic and never trains,
/// and a drained Phase-D pool sends no provider bytes.
///
/// **Structure.** Each prerequisite gets its own pool whose other five
/// predicates hold. The one prerequisite is removed or staled, the guard is
/// asked, and the assertions are the persisted decision row: the phase did not
/// move and the row names *that* predicate `false` and every other `true`.
/// The prerequisite is then restored and the same request advances — so a
/// denial is caused by the removal rather than by the fixture never having
/// been ready.
///
/// **The B1 provider prerequisites** are removed at the planner instead,
/// because that is where they live: an uncovered route yields no plan, hence
/// no debits, hence nothing to acquire against. Estimate, bucket binding,
/// abort and hidden-retry capability are each removed on their own.
///
/// **A Phase-C window built from incomplete coverage** is run through the real
/// controller cycle. It is persisted — the diagnostic is durable, not dropped
/// — with `trainable = false` and a non-empty diagnostic list, and the pool's
/// learned target is unchanged afterwards.
///
/// **A Phase-D pool drains before a later acquisition commits**, asserted as a
/// lease row count across the drain, and an unleased enforced attempt sends
/// **zero** provider bytes to a fixture-local HTTP boundary recorder. The
/// control arm of that recorder — the same closure against a pool that does
/// issue a permit — sends exactly one non-empty request, so the zero is an
/// observation rather than a broken harness.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scenario_09_every_compatibility_prerequisite_is_independently_load_bearing() {
    use djinn_coordinator::model_turn_admission::enforcement::request_phase_advances_v1;
    use djinn_db::{
        MODEL_TURN_ADMISSION_SCHEMA_VERSION, ModelTurnCompatibilityPhase, ModelTurnIdentityState,
        ModelTurnModeChangeOutcome, ModelTurnModeChangeReason, ModelTurnPhasePredicate,
    };

    let db = djinn_coordinator::test_helpers::create_test_db();
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    cfni_register_incarnation(&db).await;

    /// Ask the guard once and report whether it advanced the pool.
    async fn ask(
        repository: &ModelTurnAdmissionRepository,
        fixture: &CompatibilityFixture,
    ) -> Vec<i64> {
        request_phase_advances_v1(
            repository,
            &fixture.fence,
            CFNI_GENERATION,
            &fixture.evaluated_at,
            &fixture.expected,
        )
        .await
        .expect("the phase guard must not error")
    }

    // ── Capability heartbeats ─────────────────────────────────────────────
    {
        let pool_id = cfni_seed_pool(&db, "cfni-phase-capability", "shadow").await;
        cfni_record_chain(
            &repository,
            pool_id,
            "sha256:0000000000000000000000000000000000000000000000000000000000000001",
            &CFNI_CHAIN,
        )
        .await;
        let fixture = CompatibilityFixture {
            pool_id,
            expected: std::collections::BTreeMap::from([(pool_id, vec![cfni_expected_key()])]),
            fence: cfni_fence(),
            evaluated_at: now_rfc3339(),
        };
        assert!(
            ask(&repository, &fixture).await.is_empty(),
            "with no B2 capability report at all the advance must be denied"
        );
        let predicates = cfni_last_predicates(&repository, pool_id).await;
        assert_eq!(predicates.get("capability_reports"), Some(&false));
        assert_eq!(predicates.get("expected_path_coverage"), Some(&false));
        for other in [
            ModelTurnPhasePredicate::SchemaMarker,
            ModelTurnPhasePredicate::LeadershipGeneration,
            ModelTurnPhasePredicate::ObservationHistory,
            ModelTurnPhasePredicate::IdentityEligibility,
        ] {
            assert_eq!(
                predicates.get(other.key()),
                Some(&true),
                "only the removed prerequisite may fail: {predicates:?}"
            );
        }
        cfni_cover_route(&repository, pool_id).await;
        assert_eq!(
            ask(&repository, &fixture).await,
            vec![pool_id],
            "restoring coverage must make the identical request advance"
        );
    }

    // ── Staleness: a report older than the 60-second bound is uncovered ───
    {
        let fixture = cfni_ready_compatibility_fixture(&db, &repository, "cfni-phase-stale").await;
        let stale = CompatibilityFixture {
            // Evaluated two minutes after the reports were written, so every
            // one of them falls outside the freshness bound.
            evaluated_at: rfc3339_offset_seconds(120),
            ..CompatibilityFixture {
                pool_id: fixture.pool_id,
                expected: fixture.expected.clone(),
                fence: fixture.fence.clone(),
                evaluated_at: fixture.evaluated_at.clone(),
            }
        };
        assert!(
            ask(&repository, &stale).await.is_empty(),
            "a report older than 60 seconds is not fresh coverage"
        );
        let predicates = cfni_last_predicates(&repository, fixture.pool_id).await;
        assert_eq!(predicates.get("capability_reports"), Some(&false));
        assert_eq!(predicates.get("expected_path_coverage"), Some(&false));
        assert_eq!(
            ask(&repository, &fixture).await,
            vec![fixture.pool_id],
            "the same request evaluated inside the bound advances"
        );
    }

    // ── Controller / reaper leadership ────────────────────────────────────
    {
        let fixture =
            cfni_ready_compatibility_fixture(&db, &repository, "cfni-phase-leadership").await;
        let superseded = CompatibilityFixture {
            fence: djinn_db::ModelTurnControllerFence {
                incarnation_id: "01a01246-0000-7000-8000-0000000000ff".to_owned(),
                live_since_at: "1970-01-01T00:00:00Z".to_owned(),
            },
            pool_id: fixture.pool_id,
            expected: fixture.expected.clone(),
            evaluated_at: fixture.evaluated_at.clone(),
        };
        assert!(
            ask(&repository, &superseded).await.is_empty(),
            "an incarnation that holds no lease may not advance a phase"
        );
        assert_eq!(
            cfni_last_predicates(&repository, fixture.pool_id)
                .await
                .get("leadership_generation"),
            Some(&false)
        );
        assert_eq!(
            ask(&repository, &fixture).await,
            vec![fixture.pool_id],
            "the live incarnation advances the identical request"
        );
    }

    // ── Complete observation chains (the slot wrapping stages) ────────────
    for missing in CFNI_CHAIN {
        let name = format!("cfni-phase-chain-{missing:?}").to_lowercase();
        let pool_id = cfni_seed_pool(&db, &name, "shadow").await;
        cfni_cover_route(&repository, pool_id).await;
        let partial: Vec<djinn_db::ModelTurnPhaseCEvidenceStage> = CFNI_CHAIN
            .into_iter()
            .filter(|stage| *stage != missing)
            .collect();
        cfni_record_chain(
            &repository,
            pool_id,
            &format!("sha256:{:064x}", 0x1000u128 + pool_id as u128),
            &partial,
        )
        .await;
        let fixture = CompatibilityFixture {
            pool_id,
            expected: std::collections::BTreeMap::from([(pool_id, vec![cfni_expected_key()])]),
            fence: cfni_fence(),
            evaluated_at: now_rfc3339(),
        };
        assert!(
            ask(&repository, &fixture).await.is_empty(),
            "a chain missing its {missing:?} stage is not a complete observation"
        );
        assert_eq!(
            cfni_last_predicates(&repository, pool_id)
                .await
                .get("observation_history"),
            Some(&false),
            "the denial must name the observation history"
        );
        cfni_record_chain(
            &repository,
            pool_id,
            &format!("sha256:{:064x}", 0x1000u128 + pool_id as u128),
            &[missing],
        )
        .await;
        assert_eq!(
            ask(&repository, &fixture).await,
            vec![pool_id],
            "completing the {missing:?} stage makes the identical request advance"
        );
    }

    // ── The expected-path denominator ─────────────────────────────────────
    {
        let fixture =
            cfni_ready_compatibility_fixture(&db, &repository, "cfni-phase-denominator").await;
        let unknown = CompatibilityFixture {
            expected: std::collections::BTreeMap::from([(
                fixture.pool_id,
                vec![
                    cfni_expected_key(),
                    djinn_db::ModelTurnExpectedPathKey {
                        slot_pod_uid: "cfni-silent-slot".to_owned(),
                        deployment_revision: CFNI_REVISION.to_owned(),
                    },
                ],
            )]),
            pool_id: fixture.pool_id,
            fence: fixture.fence.clone(),
            evaluated_at: fixture.evaluated_at.clone(),
        };
        assert!(
            ask(&repository, &unknown).await.is_empty(),
            "a live expected path with no fresh report leaves the denominator \
             uncovered"
        );
        assert_eq!(
            cfni_last_predicates(&repository, fixture.pool_id)
                .await
                .get("expected_path_coverage"),
            Some(&false)
        );
        assert_eq!(
            ask(&repository, &fixture).await,
            vec![fixture.pool_id],
            "the covered denominator advances the identical request"
        );
    }

    // ── Durable identity eligibility ──────────────────────────────────────
    {
        let fixture =
            cfni_ready_compatibility_fixture(&db, &repository, "cfni-phase-identity").await;
        repository
            .set_pool_identity_for_test(fixture.pool_id, ModelTurnIdentityState::Colliding)
            .await
            .expect("mark the record as sharing an upstream budget");
        assert!(
            ask(&repository, &fixture).await.is_empty(),
            "a colliding shared-account record may not advance"
        );
        assert_eq!(
            cfni_last_predicates(&repository, fixture.pool_id)
                .await
                .get("identity_eligibility"),
            Some(&false)
        );
        repository
            .set_pool_identity_for_test(fixture.pool_id, ModelTurnIdentityState::Eligible)
            .await
            .expect("restore eligibility");
        assert_eq!(
            ask(&repository, &fixture).await,
            vec![fixture.pool_id],
            "an eligible record advances the identical request"
        );
    }

    // ── Schema / repository readiness ─────────────────────────────────────
    //
    // The marker is one row for the whole database, so this case runs on its
    // own database and restores the marker before anything else reads it.
    {
        let schema_db = djinn_coordinator::test_helpers::create_test_db();
        let schema_repository = ModelTurnAdmissionRepository::new(schema_db.clone());
        cfni_register_incarnation(&schema_db).await;
        let fixture =
            cfni_ready_compatibility_fixture(&schema_db, &schema_repository, "cfni-phase-schema")
                .await;
        assert_eq!(
            schema_repository
                .schema_readiness()
                .await
                .expect("read the schema marker")
                .map(|readiness| readiness.model_turn_admission_schema),
            Some(MODEL_TURN_ADMISSION_SCHEMA_VERSION),
            "precondition: the marker starts at the revision this binary reads"
        );
        djinn_db::test_support::set_model_turn_schema_marker_present_for_test(&schema_db, false)
            .await;
        assert!(
            ask(&schema_repository, &fixture).await.is_empty(),
            "storage whose durable marker this binary cannot read is not a \
             prerequisite that holds"
        );
        assert_eq!(
            cfni_last_predicates(&schema_repository, fixture.pool_id)
                .await
                .get("schema_marker"),
            Some(&false)
        );
        djinn_db::test_support::set_model_turn_schema_marker_present_for_test(&schema_db, true)
            .await;
        assert_eq!(
            ask(&schema_repository, &fixture).await,
            vec![fixture.pool_id],
            "restoring the marker makes the identical request advance"
        );
        close(schema_db).await;
    }

    // ── A phase request may not skip a prerequisite phase ─────────────────
    {
        let fixture = cfni_ready_compatibility_fixture(&db, &repository, "cfni-phase-skip").await;
        let outcome = repository
            .request_phase_transition_in_transaction(djinn_db::ModelTurnPhaseTransitionRequest {
                pool_id: fixture.pool_id,
                requested_phase: ModelTurnCompatibilityPhase::D,
                controller_generation: CFNI_GENERATION,
                fence: fixture.fence.clone(),
                evaluated_at: fixture.evaluated_at.clone(),
                expected_paths: vec![cfni_expected_key()],
            })
            .await
            .expect("the guard must not error");
        assert_eq!(
            outcome,
            djinn_db::ModelTurnPhaseTransitionOutcome::NotAdjacent {
                effective_phase: ModelTurnCompatibilityPhase::A,
                requested_phase: ModelTurnCompatibilityPhase::D,
            },
            "a phase cannot become effective without its predecessor"
        );
        assert!(
            repository
                .phase_transitions(fixture.pool_id, 8)
                .await
                .expect("read the phase ledger")
                .is_empty(),
            "a skip evaluates no predicate and writes no ledger row at all"
        );
        // And the same pool walks A→B→C→D one step at a time.
        for step in [
            ModelTurnCompatibilityPhase::B,
            ModelTurnCompatibilityPhase::C,
            ModelTurnCompatibilityPhase::D,
        ] {
            assert_eq!(
                ask(&repository, &fixture).await,
                vec![fixture.pool_id],
                "each pass advances exactly one step"
            );
            assert_eq!(
                repository
                    .compatibility_phase(fixture.pool_id)
                    .await
                    .expect("read the phase")
                    .expect("pool exists"),
                step
            );
        }
    }

    // ── B1: each provider capability is independently load-bearing ────────
    {
        use djinn_provider::{
            ProviderAbortCapabilityV1, ProviderAttemptCapabilitiesV1,
            ProviderAttemptRouteCoverageV1, ProviderAttemptScopeV1,
            ProviderAttemptUncoveredReasonV1, ProviderCredentialRecordScopeV1,
            ProviderHiddenRetryCapabilityV1, plan_provider_attempt_v1,
        };
        const ALL: [ModelTurnBucketKind; 4] = [
            ModelTurnBucketKind::Request,
            ModelTurnBucketKind::Input,
            ModelTurnBucketKind::Output,
            ModelTurnBucketKind::Combined,
        ];
        let scope = || ProviderAttemptScopeV1 {
            credential: ProviderCredentialRecordScopeV1::from_credential_record_id(
                "cfni-credential",
            ),
            provider_id: CFNI_PROVIDER.to_owned(),
            model_id: CFNI_MODEL.to_owned(),
        };
        let ready = ProviderAttemptCapabilitiesV1 {
            hidden_retries: ProviderHiddenRetryCapabilityV1::Disabled,
            abort: ProviderAbortCapabilityV1::Supported,
        };
        let body = vec![b'x'; 300];
        // The control: with all four prerequisites present there is a plan.
        assert!(
            plan_provider_attempt_v1(
                scope(),
                Some(&body),
                Some(10),
                Some(256),
                Some(1_024),
                ALL,
                ready
            )
            .is_ok(),
            "the fully covered route must plan, or the removals below prove nothing"
        );
        // Estimate.
        assert_eq!(
            plan_provider_attempt_v1(scope(), None, Some(10), Some(256), Some(1_024), ALL, ready)
                .expect_err("no serialized body means no estimate"),
            ProviderAttemptRouteCoverageV1::Uncovered(
                ProviderAttemptUncoveredReasonV1::SerializationUnavailable
            )
        );
        // Normalized bucket bindings.
        assert_eq!(
            plan_provider_attempt_v1(
                scope(),
                Some(&body),
                Some(10),
                Some(256),
                Some(1_024),
                [
                    ModelTurnBucketKind::Request,
                    ModelTurnBucketKind::Output,
                    ModelTurnBucketKind::Combined
                ],
                ready,
            )
            .expect_err("a missing binding means no plan"),
            ProviderAttemptRouteCoverageV1::Uncovered(
                ProviderAttemptUncoveredReasonV1::MissingBucketBinding {
                    bucket_kind: ModelTurnBucketKind::Input
                }
            )
        );
        // Abort.
        assert_eq!(
            plan_provider_attempt_v1(
                scope(),
                Some(&body),
                Some(10),
                Some(256),
                Some(1_024),
                ALL,
                ProviderAttemptCapabilitiesV1 {
                    abort: ProviderAbortCapabilityV1::Unsupported,
                    ..ready
                },
            )
            .expect_err("an unabortable route means no plan"),
            ProviderAttemptRouteCoverageV1::Uncovered(
                ProviderAttemptUncoveredReasonV1::AbortUnsupported
            )
        );
        // No hidden retries.
        assert_eq!(
            plan_provider_attempt_v1(
                scope(),
                Some(&body),
                Some(10),
                Some(256),
                Some(1_024),
                ALL,
                ProviderAttemptCapabilitiesV1 {
                    hidden_retries: ProviderHiddenRetryCapabilityV1::Unsupported,
                    ..ready
                },
            )
            .expect_err("a route that retries below the boundary means no plan"),
            ProviderAttemptRouteCoverageV1::Uncovered(
                ProviderAttemptUncoveredReasonV1::HiddenRetriesNotDisabled
            )
        );
    }

    // ── Incomplete Phase-C coverage is diagnostic and never trains ────────
    {
        use djinn_coordinator::model_turn_admission::AlignedPhaseCWindowV1;
        use djinn_coordinator::model_turn_admission::controller::{
            PhaseCCompletedWindowV1, PhaseCWindowCountsV1, project_dispatch_topology_paths_v1,
            run_completed_window_cycle_v1, window_bounds_v1,
        };
        use djinn_coordinator::model_turn_admission::subscription_learner::{
            ControllerTransitionV1, SubscriptionControllerStateV1,
        };

        // The leader's per-pool controller state, carried across the windows
        // below exactly as the coordinator actor carries it across ticks.
        let mut controllers: std::collections::BTreeMap<i64, SubscriptionControllerStateV1> =
            std::collections::BTreeMap::new();

        let pool_id = cfni_seed_pool(&db, "cfni-phase-c-diagnostic", "shadow").await;
        repository
            .set_pool_learned_concurrency_for_test(pool_id, 3)
            .await
            .expect("give the pool a target that must not move");
        let catalog = cfni_catalog();
        let pools: Vec<djinn_db::ModelTurnPool> = repository
            .list_observable_pools(64)
            .await
            .expect("read the dispatch topology")
            .into_iter()
            .filter(|pool| pool.id == pool_id)
            .collect();
        assert_eq!(pools.len(), 1, "the fixture pool must be observable");
        let projection = project_dispatch_topology_paths_v1(&catalog, &[cfni_ready_slot()], &pools);
        assert_eq!(
            projection.expected_paths.len(),
            1,
            "one Ready slot crossed with one route is one expected path"
        );

        let window = AlignedPhaseCWindowV1::new(120).expect("aligned window");
        let (started_at, ended_at) = window_bounds_v1(window).expect("window bounds");
        let outcome = run_completed_window_cycle_v1(
            &repository,
            &catalog,
            &cfni_fence(),
            &PhaseCCompletedWindowV1 {
                window,
                started_at,
                ended_at,
                projection: &projection,
                // No capability evidence and no admitted attempts: coverage for
                // the window is incomplete, which is exactly the production
                // shape today.
                capability_evidence: &[],
                admitted_attempts: &[],
                counts: std::collections::BTreeMap::from([(
                    pool_id,
                    PhaseCWindowCountsV1 {
                        admitted_turns: 12,
                        completed_turns: 12,
                    },
                )]),
                // The production shape: the leader can reconstruct no stream
                // intervals and no per-attempt usage from the durable ledgers,
                // so it supplies none.
                activity: std::collections::BTreeMap::new(),
            },
            CFNI_GENERATION,
            &mut controllers,
        )
        .await
        .expect("the controller cycle must not error");

        assert_eq!(
            outcome.persisted_pools,
            vec![pool_id],
            "the diagnostic window is persisted, not dropped"
        );
        assert!(
            !outcome.qualification.admitted,
            "a window with no capability evidence cannot qualify"
        );
        assert!(
            !outcome.qualification.diagnostics.is_empty(),
            "and it must say why"
        );
        let summary = repository
            .controller_window_summary_for_test(pool_id, 2)
            .await
            .expect("read the persisted window")
            .expect("the window row exists");
        assert!(
            !summary.trainable,
            "the persisted row must record the window as untrainable"
        );
        assert!(
            !summary.diagnostics.is_empty(),
            "the persisted row must carry the diagnostic codes"
        );
        // The learner ran, and it *refused*. This is the assertion that used to
        // read `learned_concurrency == 3` and could not fail: before the
        // learner had a caller and the column had a production writer, no
        // window of any kind could move that number, so asserting it had not
        // moved asserted nothing. What is checked now is the learner's own
        // verdict for this pool — that it was consulted at all, and that the
        // durable window it re-read was not one it would train on. An empty
        // map (the learner was never called) and any other transition (it
        // qualified a diagnostic window) both fail here.
        assert_eq!(
            outcome.learner_transitions.get(&pool_id),
            Some(&ControllerTransitionV1::HeldUnqualified),
            "the subscription controller must run for every persisted pool and \
             must refuse a window the durable ledger calls untrainable; got \
             {:?}",
            outcome.learner_transitions
        );
        assert!(
            outcome.learned_pools.is_empty() && outcome.learner_fenced_pools.is_empty(),
            "a refused window commits no target at all; got learned={:?} \
             fenced={:?}",
            outcome.learned_pools,
            outcome.learner_fenced_pools
        );
        assert_eq!(
            repository
                .pool_control_state_for_test(pool_id)
                .await
                .expect("pool state")
                .expect("pool exists")
                .3,
            3,
            "and the persisted target stands where it was"
        );
        // And the same untrainable verdict denies the enforcement advance.
        let denied = djinn_coordinator::model_turn_admission::enforcement::run_enforcement_pass_v1(
            &repository,
            &cfni_fence(),
            CFNI_GENERATION,
            &now_rfc3339(),
            &std::collections::BTreeMap::from([(pool_id, vec![cfni_expected_key()])]),
            summary.trainable,
        )
        .await
        .expect("enforcement pass must not error");
        assert!(
            denied.enforced_pools.is_empty(),
            "an untrainable window may not enforce a pool"
        );
    }

    // ── A trainable window DOES move the persisted target ─────────────────
    //
    // The positive control for the block above. Without it, "an untrainable
    // window teaches nothing" is satisfied by a learner that is hard-wired to
    // teach nothing, and the refusal proves only that the code is inert.
    //
    // **Disclosed seeding.** Production cannot reach this state today and this
    // block does not pretend otherwise: `model_turn_capability_heartbeats`
    // persists a heartbeat *instant* and not a coverage interval, so
    // `capability_evidence_from_heartbeat` reconstructs the narrowest interval
    // the row supports and every real window is `PartialCapabilityCoverage`.
    // The `PhaseCCapabilityEvidenceV1` values below carry the window-covering
    // interval that Phase B2 storage will persist, supplied here by hand. That
    // is a fixture, stated at the line that writes it — the production leader
    // supplies no such evidence, and widening the stored instant to manufacture
    // it would forge the very coverage every decision here rests on.
    //
    // Everything downstream of that seed is production: the qualifier, the
    // fenced window upsert, the exact-bound catalog-qualified learner read, the
    // controller, and the fenced `learned_concurrency` writer.
    {
        use djinn_coordinator::model_turn_admission::controller::{
            PhaseCCompletedWindowV1, PhaseCWindowCountsV1, project_dispatch_topology_paths_v1,
            run_completed_window_cycle_v1, window_bounds_v1,
        };
        use djinn_coordinator::model_turn_admission::subscription_learner::{
            ActiveStreamV1, ControllerTransitionV1, OutputTokenEmissionV1,
            SubscriptionControllerStateV1, WindowActivityV1,
        };
        use djinn_coordinator::model_turn_admission::{
            AlignedPhaseCWindowV1, PhaseCCapabilityEvidenceV1,
        };
        use djinn_db::{
            MODEL_TURN_LEARNED_CONCURRENCY_MAX, ModelTurnControllerFence,
            ModelTurnLearnedConcurrencyInput, ModelTurnLeaseMutationOutcome,
        };

        let pool_id = cfni_seed_pool(&db, "cfni-phase-c-trainable", "shadow").await;
        let catalog = cfni_catalog();
        let target_now = async |pool_id: i64| {
            repository
                .pool_control_state_for_test(pool_id)
                .await
                .expect("pool state")
                .expect("pool exists")
                .3
        };
        assert_eq!(
            target_now(pool_id).await,
            1,
            "the seeded pool starts at the floor, so every move below is the \
             controller's and not the fixture's"
        );

        let pools: Vec<djinn_db::ModelTurnPool> = repository
            .list_observable_pools(64)
            .await
            .expect("read the dispatch topology")
            .into_iter()
            .filter(|pool| pool.id == pool_id)
            .collect();
        assert_eq!(pools.len(), 1, "the fixture pool must be observable");
        let projection = project_dispatch_topology_paths_v1(&catalog, &[cfni_ready_slot()], &pools);
        assert_eq!(projection.expected_paths.len(), 1);
        let path = projection.expected_paths[0].clone();

        // The seeded coverage interval — see the disclosure above.
        let covered = |window: AlignedPhaseCWindowV1| PhaseCCapabilityEvidenceV1 {
            path: path.clone(),
            coverage_start_second: window.start_second(),
            coverage_end_second: window.end_second(),
            observed_at_second: window.start_second() + 30,
            covered: true,
        };
        // One stream that produced `tokens` across the whole window: 60 seconds
        // of active wall clock, so the aggregate rate is `tokens / 60`.
        let stream = |window: AlignedPhaseCWindowV1, tokens: i64| WindowActivityV1 {
            streams: vec![ActiveStreamV1 {
                started_at_second: window.start_second(),
                ended_at_second: window.end_second(),
                emissions: vec![OutputTokenEmissionV1 {
                    emitted_at_second: window.start_second() + 30,
                    output_tokens: tokens,
                }],
            }],
            terminals: Vec::new(),
        };

        let mut controllers: std::collections::BTreeMap<i64, SubscriptionControllerStateV1> =
            std::collections::BTreeMap::new();
        let cycle = async |window: AlignedPhaseCWindowV1,
                           tokens: i64,
                           fence: &ModelTurnControllerFence,
                           controllers: &mut std::collections::BTreeMap<
            i64,
            SubscriptionControllerStateV1,
        >| {
            let (started_at, ended_at) = window_bounds_v1(window).expect("window bounds");
            let evidence = [covered(window)];
            run_completed_window_cycle_v1(
                &repository,
                &catalog,
                fence,
                &PhaseCCompletedWindowV1 {
                    window,
                    started_at,
                    ended_at,
                    projection: &projection,
                    capability_evidence: &evidence,
                    // No admitted attempts in the window, so the attempt-chain
                    // half of the qualifier has nothing to reject and the only
                    // seeded input is the coverage interval above.
                    admitted_attempts: &[],
                    counts: std::collections::BTreeMap::from([(
                        pool_id,
                        PhaseCWindowCountsV1 {
                            admitted_turns: 12,
                            completed_turns: 12,
                        },
                    )]),
                    activity: std::collections::BTreeMap::from([(pool_id, stream(window, tokens))]),
                },
                CFNI_GENERATION,
                controllers,
            )
            .await
            .expect("the controller cycle must not error")
        };

        // Window 2 at 100 tokens/second. It qualifies and it is eligible, but a
        // controller with no baseline has nothing to have grown against, so the
        // first eligible window only establishes one.
        let window_two = AlignedPhaseCWindowV1::new(120).expect("aligned window");
        let first = cycle(window_two, 6_000, &cfni_fence(), &mut controllers).await;
        assert!(
            first.qualification.admitted,
            "with complete coverage and no attempt evidence the window must \
             qualify, or nothing below is a test of the learner; got {:?}",
            first.qualification.diagnostics
        );
        assert_eq!(first.persisted_pools, vec![pool_id]);
        assert_eq!(
            first.learner_transitions.get(&pool_id),
            Some(&ControllerTransitionV1::ProbeDidNotGrow),
            "the first eligible window establishes the baseline"
        );
        assert!(first.learned_pools.is_empty());
        assert_eq!(
            target_now(pool_id).await,
            1,
            "and a window that moved no target commits none"
        );

        // Window 3 at 200 tokens/second, against a baseline of 100. The
        // bootstrap lower bound clears the 5% growth threshold, the controller
        // steps 1 -> 2, and *that* is what reaches the column.
        let window_three = AlignedPhaseCWindowV1::new(180).expect("aligned window");
        let grew = cycle(window_three, 12_000, &cfni_fence(), &mut controllers).await;
        assert_eq!(
            grew.learner_transitions.get(&pool_id),
            Some(&ControllerTransitionV1::Grew),
            "a window that doubles the observed rate must grow the target"
        );
        assert_eq!(
            grew.learned_pools,
            vec![(pool_id, 2)],
            "the cycle must report the target it committed"
        );
        assert!(grew.learner_fenced_pools.is_empty());
        assert_eq!(
            target_now(pool_id).await,
            2,
            "the persisted learned target moved through the production path; \
             this is the assertion that was impossible before the controller \
             had a caller and the column had a writer"
        );

        // The fence, at the cycle. A leader that is no longer the live
        // incarnation persists no window, so it never reaches the learner.
        let stale = ModelTurnControllerFence {
            incarnation_id: "01a01246-0000-7000-8000-0000000000ff".to_owned(),
            live_since_at: "1970-01-01T00:00:00Z".to_owned(),
        };
        let window_four = AlignedPhaseCWindowV1::new(240).expect("aligned window");
        let fenced = cycle(window_four, 60_000, &stale, &mut controllers).await;
        assert!(fenced.fenced, "an unknown incarnation may not commit");
        assert!(
            fenced.learner_transitions.is_empty() && fenced.learned_pools.is_empty(),
            "and a fenced cycle never reaches the learner at all"
        );
        assert_eq!(
            target_now(pool_id).await,
            2,
            "so the target a superseded leader computed does not land"
        );

        // The fence, at the writer itself — the clause above only proves the
        // window upsert is fenced. This one offers the learned-concurrency
        // write directly, with a stale fence and then a live one, so the
        // refusal is not simply "this writer never works".
        assert_eq!(
            repository
                .apply_learned_concurrency(ModelTurnLearnedConcurrencyInput {
                    pool_id,
                    learned_concurrency: 9,
                    controller_generation: CFNI_GENERATION,
                    fence: stale,
                })
                .await
                .expect("the fenced write must not error"),
            ModelTurnLeaseMutationOutcome::Fenced,
        );
        assert_eq!(
            target_now(pool_id).await,
            2,
            "a stale generation's write updates no row"
        );
        assert_eq!(
            repository
                .apply_learned_concurrency(ModelTurnLearnedConcurrencyInput {
                    pool_id,
                    learned_concurrency: 9,
                    controller_generation: CFNI_GENERATION,
                    fence: cfni_fence(),
                })
                .await
                .expect("the live write must not error"),
            ModelTurnLeaseMutationOutcome::Applied,
        );
        assert_eq!(
            target_now(pool_id).await,
            9,
            "and the identical write under the live fence does land"
        );

        // The bounds, at the same writer, under the *live* fence — so a
        // refusal here is the bound and not leadership.
        //
        // Zero is the dangerous one. `acquire_turn` admits against this column,
        // so a committed zero stops the pool admitting anything, silently and
        // without a `model_turn_pool_mode_transitions` row: it is an
        // undocumented second way to close a pool, and only the mode ledger may
        // do that. Adversarial verification round two found the guard real and
        // undefended — relaxing `learned_concurrency < 1` to `< 0` left the
        // whole target green — so this is what defends it.
        //
        // Each case asserts *two* things, because either alone is weak. The
        // `Err` alone would be satisfied by a writer that updated the row and
        // then complained; the unchanged target alone would be satisfied by a
        // writer that silently did nothing to anything. Together they say the
        // input was refused before it reached the statement.
        for refused in [0, -1, MODEL_TURN_LEARNED_CONCURRENCY_MAX + 1] {
            let error = repository
                .apply_learned_concurrency(ModelTurnLearnedConcurrencyInput {
                    pool_id,
                    learned_concurrency: refused,
                    controller_generation: CFNI_GENERATION,
                    fence: cfni_fence(),
                })
                .await
                .expect_err(
                    "a learned concurrency outside [1, MODEL_TURN_LEARNED_CONCURRENCY_MAX] \
                     must be refused by the writer, not committed",
                );
            assert!(
                matches!(error, djinn_db::Error::InvalidData(_)),
                "the refusal must be the validation guard and not an incidental \
                 database error; got {error:?}"
            );
            assert_eq!(
                target_now(pool_id).await,
                9,
                "and the refused target {refused} must leave the last committed \
                 target exactly where it was"
            );
        }
    }

    // ── Phase D drains before a later acquisition commits ─────────────────
    {
        let pool_id = cfni_seed_pool(&db, "cfni-phase-drain", "enforce").await;
        repository
            .seed_request_bucket_binding_for_test(pool_id, 8, 8)
            .await
            .expect("seed the request binding");
        repository
            .set_pool_learned_concurrency_for_test(pool_id, 4)
            .await
            .expect("raise the learned target");

        let before = repository
            .acquire_turn(ModelTurnAcquireInput {
                pool_id,
                request_id: "cfni-drain-before".to_owned(),
                owner_pod_uid: Some("pod-cfni-drain".to_owned()),
                generation: 1,
                debits: request_debit(1),
            })
            .await
            .expect("acquisition must not error");
        assert!(
            matches!(before, ModelTurnAcquireOutcome::Admitted { .. }),
            "the pool must admit before the drain, or the count below proves \
             nothing; got {before:?}"
        );
        let leases_at_drain = model_turn_lease_count_for_pool_fixture(&db, pool_id).await;
        assert_eq!(leases_at_drain, 1);

        let drained = repository
            .drain_pool_in_transaction(
                pool_id,
                CFNI_GENERATION,
                ModelTurnModeChangeReason::CapabilityCoverageLoss,
            )
            .await
            .expect("drain must not error");
        assert!(
            matches!(
                drained,
                ModelTurnModeChangeOutcome::Applied { .. }
                    | ModelTurnModeChangeOutcome::DrainedAndSettled { .. }
            ),
            "the enforcing pool must actually drain; got {drained:?}"
        );
        assert_eq!(
            repository
                .pool_control_state_for_test(pool_id)
                .await
                .expect("pool state")
                .expect("pool exists")
                .0,
            "draining",
            "the lease taken above is still in flight, so the drain does not \
             settle to `off` and the refusal below is genuinely the draining arm"
        );

        let after = repository
            .acquire_turn(ModelTurnAcquireInput {
                pool_id,
                request_id: "cfni-drain-after".to_owned(),
                owner_pod_uid: Some("pod-cfni-drain".to_owned()),
                generation: 1,
                debits: request_debit(1),
            })
            .await
            .expect("acquisition must not error");
        assert_eq!(
            after,
            ModelTurnAcquireOutcome::Wait(djinn_db::ModelTurnAdmissionWait::Draining),
            "once the drain commits, no later acquisition may commit"
        );
        assert_eq!(
            model_turn_lease_count_for_pool_fixture(&db, pool_id).await,
            leases_at_drain,
            "and the lease relation is unchanged by the refused acquisition"
        );
    }

    // ── An unleased enforced attempt sends zero provider bytes ────────────
    {
        use djinn_slot::reply_loop::model_turn_admission::{
            ModelTurnAdmissionCoordinator, ModelTurnAdmissionRequest, ModelTurnPreparation,
        };
        use wiremock::matchers::any;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // A real HTTP server standing in for the provider endpoint. It is the
        // boundary recorder: it counts the bytes that actually arrived, so a
        // send that happened cannot be argued away.
        let boundary = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200))
            .mount(&boundary)
            .await;
        let endpoint = boundary
            .uri()
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_owned();
        let coordinator =
            ModelTurnAdmissionCoordinator::new(repository.clone()).with_catalog(cfni_catalog());

        // The production attempt boundary in miniature: the send happens in
        // the `Permit` arm and nowhere else, exactly as
        // `djinn_slot::reply_loop::turn::launch_prepared_covered_attempt`
        // launches only from that arm.
        //
        // The bytes go out over a raw socket rather than through an HTTP
        // client: `scripts/check-http-boundary.sh` reserves outbound HTTP
        // client construction to `djinn-provider`, and a hand-framed request
        // is in any case the more literal instrument — what the recorder
        // counts is bytes that reached a listening socket.
        let attempt = async |credential: &str, request_id: &str| -> bool {
            let preparation = coordinator
                .prepare(
                    &cfni_plan(request_debit(1)),
                    ModelTurnAdmissionRequest {
                        credential_id: credential.to_owned(),
                        request_id: request_id.to_owned(),
                        owner_pod_uid: Some("pod-cfni-boundary".to_owned()),
                        generation: 1,
                    },
                )
                .await
                .expect("prepare must not error");
            match preparation {
                ModelTurnPreparation::Permit(_) => {
                    let endpoint = endpoint.clone();
                    tokio::task::spawn_blocking(move || send_provider_bytes(&endpoint))
                        .await
                        .expect("the send task must not panic");
                    true
                }
                ModelTurnPreparation::Wait(_)
                | ModelTurnPreparation::Rejected(_)
                | ModelTurnPreparation::DispatchFenced { .. } => false,
            }
        };

        // The unleased arm: a Phase-D pool that is draining.
        //
        // One lease is taken first so the drain cannot settle straight to
        // `off` — a pool with nothing in flight has nothing to drain, and this
        // scenario is about the *draining* state that a coverage loss puts a
        // live pool into.
        let drained_pool = cfni_seed_pool(&db, "cfni-boundary-drained", "enforce").await;
        repository
            .seed_request_bucket_binding_for_test(drained_pool, 8, 8)
            .await
            .expect("seed the request binding");
        repository
            .set_pool_learned_concurrency_for_test(drained_pool, 4)
            .await
            .expect("raise the learned target");
        assert!(
            matches!(
                repository
                    .acquire_turn(ModelTurnAcquireInput {
                        pool_id: drained_pool,
                        request_id: "cfni-boundary-in-flight".to_owned(),
                        owner_pod_uid: Some("pod-cfni-boundary".to_owned()),
                        generation: 1,
                        debits: request_debit(1),
                    })
                    .await
                    .expect("acquisition must not error"),
                ModelTurnAcquireOutcome::Admitted { .. }
            ),
            "one lease must be in flight so the drain does not settle to off"
        );
        assert!(
            matches!(
                repository
                    .drain_pool_in_transaction(
                        drained_pool,
                        CFNI_GENERATION,
                        ModelTurnModeChangeReason::CapabilityCoverageLoss,
                    )
                    .await
                    .expect("drain must not error"),
                ModelTurnModeChangeOutcome::Applied { .. }
            ),
            "the boundary fixture's pool must actually enter draining"
        );
        assert_eq!(
            repository
                .pool_control_state_for_test(drained_pool)
                .await
                .expect("pool state")
                .expect("pool exists")
                .0,
            "draining",
            "the arm this scenario exercises is `draining`, named explicitly so \
             it cannot silently become some other refusal"
        );
        let leases_before_attempt =
            model_turn_lease_count_for_pool_fixture(&db, drained_pool).await;
        let sent = attempt("cfni-boundary-drained", "cfni-boundary-unleased").await;
        assert!(!sent, "a draining pool issues no permit");
        assert_eq!(
            model_turn_lease_count_for_pool_fixture(&db, drained_pool).await,
            leases_before_attempt,
            "and writes no further lease"
        );
        let observed = boundary
            .received_requests()
            .await
            .expect("the boundary recorder must be readable");
        assert_eq!(
            observed.len(),
            0,
            "an unleased enforced attempt must send zero provider requests"
        );
        assert_eq!(
            observed
                .iter()
                .map(|request| request.body.len())
                .sum::<usize>(),
            0,
            "and zero provider bytes"
        );

        // The control arm: the identical closure against a pool that does
        // issue a permit sends exactly one non-empty request, so the zero
        // above is an observation rather than a broken harness.
        let live_pool = cfni_seed_pool(&db, "cfni-boundary-live", "enforce").await;
        repository
            .seed_request_bucket_binding_for_test(live_pool, 8, 8)
            .await
            .expect("seed the request binding");
        assert!(
            attempt("cfni-boundary-live", "cfni-boundary-leased").await,
            "a pool with capacity must issue a permit"
        );
        let observed = boundary
            .received_requests()
            .await
            .expect("the boundary recorder must be readable");
        assert_eq!(observed.len(), 1, "the leased attempt did send");
        assert!(
            !observed[0].body.is_empty(),
            "and the recorder really does count bytes"
        );
        assert_eq!(
            model_turn_lease_count_for_pool_fixture(&db, live_pool).await,
            1,
            "the leased attempt holds exactly one lease"
        );
    }

    close(db).await;
}

// ─── Group 10: the coverage denominator and the rollback order ─────────────

/// The expected-path denominator comes from the coordinator's own inventory
/// and topology, never from the reports, and rollback runs D→C→B in order.
///
/// * **The denominator is authoritative.** It is built by the production
///   `project_dispatch_topology_paths_v1` from live Ready slot workloads
///   crossed with the durable pool routes that still resolve in the active
///   catalog. A terminal or not-Ready slot contributes nothing, and a pool
///   whose labels no longer resolve is dropped rather than carried on its own
///   say-so.
/// * **Silence, skew and unknown paths stay visible.** A silent slot — one in
///   the denominator with no fresh report — leaves the pool uncovered rather
///   than shrinking the denominator, asserted through the same guard predicate
///   the leader uses. A report from a *different* deployment revision does not
///   satisfy the expected key, so revision skew is uncovered too. A report
///   from a path that is not expected at all cannot add itself.
/// * **Rollback is ordered.** `ModelTurnRollbackPlanV1` accepts only
///   controller → slot wrappers → provider contracts → mode off, refusing any
///   other order with the expected/attempted pair; and the `off` mode itself
///   is reachable only after a drain, asserted against the production mode
///   writer.
/// * **Only the selected pool is affected, and nothing else is touched.** A
///   second covered pool keeps its mode and its learned target across the
///   drain of the first, and the user-settings relation that holds
///   `max_sessions`/`lane_max_sessions` is byte identical across the whole
///   scenario.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scenario_10_the_coverage_denominator_is_authoritative_and_rollback_is_ordered() {
    use djinn_coordinator::model_turn_admission::controller::project_dispatch_topology_paths_v1;
    use djinn_coordinator::model_turn_admission::enforcement::request_phase_advances_v1;
    use djinn_db::{
        ModelTurnAdmissionPhase, ModelTurnModeChangeInput, ModelTurnModeChangeOutcome,
        ModelTurnModeChangeReason, ModelTurnModeChangeRejection, ModelTurnRollbackPlanV1,
        ModelTurnRollbackStepV1,
    };

    let db = djinn_coordinator::test_helpers::create_test_db();
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    cfni_register_incarnation(&db).await;
    let settings_before = djinn_db::test_support::table_digest_for_test(&db, "user_settings").await;

    let selected = cfni_seed_pool(&db, "cfni-denominator-selected", "shadow").await;
    let bystander = cfni_seed_pool(&db, "cfni-denominator-bystander", "shadow").await;
    repository
        .set_pool_learned_concurrency_for_test(bystander, 6)
        .await
        .expect("give the bystander a target to preserve");
    let catalog = cfni_catalog();
    let pools = repository
        .list_observable_pools(64)
        .await
        .expect("read the dispatch topology");
    assert!(pools.len() >= 2, "both fixture pools must be observable");

    // ── The denominator is built from Ready slots and resolving routes ────
    let ready = cfni_ready_slot();
    let projection =
        project_dispatch_topology_paths_v1(&catalog, std::slice::from_ref(&ready), &pools);
    assert_eq!(
        projection.expected_paths.len(),
        pools.len(),
        "one Ready slot crossed with every resolving route"
    );

    let mut terminal = cfni_ready_slot();
    terminal.terminal = true;
    let mut not_ready = cfni_ready_slot();
    not_ready.ready = false;
    let mut no_revision = cfni_ready_slot();
    no_revision.deployment_revision = None;
    for (label, record) in [
        ("a terminal slot", terminal),
        ("a not-Ready slot", not_ready),
        ("a slot with no deployment revision", no_revision),
    ] {
        assert!(
            project_dispatch_topology_paths_v1(&catalog, &[record], &pools)
                .expected_paths
                .is_empty(),
            "{label} contributes nothing to the expected-path denominator"
        );
    }
    // A route whose labels the active catalog no longer resolves is dropped.
    assert!(
        project_dispatch_topology_paths_v1(
            &djinn_provider::catalog::CatalogService::new(),
            std::slice::from_ref(&ready),
            &pools,
        )
        .expected_paths
        .is_empty(),
        "a pool the active catalog cannot resolve is not carried on its own \
         stored labels"
    );

    // ── Silence and revision skew stay uncovered ──────────────────────────
    cfni_record_chain(
        &repository,
        selected,
        "sha256:0000000000000000000000000000000000000000000000000000000000000010",
        &CFNI_CHAIN,
    )
    .await;
    // The positive control: a report on the exact expected key covers it, so
    // the identical request on the pools below is denied for their skew and
    // their silence rather than for something the fixture forgot.
    cfni_cover_route(&repository, selected).await;
    let expected = std::collections::BTreeMap::from([(selected, vec![cfni_expected_key()])]);
    assert_eq!(
        request_phase_advances_v1(
            &repository,
            &cfni_fence(),
            CFNI_GENERATION,
            &now_rfc3339(),
            &expected,
        )
        .await
        .expect("the phase guard must not error"),
        vec![selected],
        "a report on the expected key covers it"
    );

    // Skew: a report from a different deployment revision. It neither covers
    // the expected key nor vanishes — it stays in the covered set and breaks
    // the exact set equality, which is how revision skew stays visible.
    let skewed = cfni_seed_pool(&db, "cfni-denominator-skewed", "shadow").await;
    cfni_record_chain(
        &repository,
        skewed,
        "sha256:0000000000000000000000000000000000000000000000000000000000000011",
        &CFNI_CHAIN,
    )
    .await;
    repository
        .record_capability_heartbeat(djinn_db::ModelTurnCapabilityHeartbeatInput {
            pool_id: skewed,
            slot_pod_uid: CFNI_SLOT.to_owned(),
            deployment_revision: "cfni-rev-old".to_owned(),
            provider_id: CFNI_PROVIDER.to_owned(),
            model_id: CFNI_MODEL.to_owned(),
        })
        .await
        .expect("record the skewed report");
    assert!(
        request_phase_advances_v1(
            &repository,
            &cfni_fence(),
            CFNI_GENERATION,
            &now_rfc3339(),
            &std::collections::BTreeMap::from([(skewed, vec![cfni_expected_key()])]),
        )
        .await
        .expect("the phase guard must not error")
        .is_empty(),
        "a report from another deployment revision does not cover the expected \
         path, so the denominator stays uncovered rather than shrinking"
    );
    assert_eq!(
        cfni_last_predicates(&repository, skewed)
            .await
            .get("expected_path_coverage"),
        Some(&false)
    );

    // Silence: a second live slot in the denominator with no report at all.
    // The denominator is the coordinator's, so adding a silent path makes the
    // pool uncovered instead of leaving it covered by the one that did report.
    assert!(
        request_phase_advances_v1(
            &repository,
            &cfni_fence(),
            CFNI_GENERATION,
            &now_rfc3339(),
            &std::collections::BTreeMap::from([(
                selected,
                vec![
                    cfni_expected_key(),
                    djinn_db::ModelTurnExpectedPathKey {
                        slot_pod_uid: "cfni-silent-slot".to_owned(),
                        deployment_revision: CFNI_REVISION.to_owned(),
                    },
                ],
            )]),
        )
        .await
        .expect("the phase guard must not error")
        .is_empty(),
        "a silent expected path leaves the pool uncovered rather than \
         disappearing from the denominator"
    );

    // ── Rollback order ────────────────────────────────────────────────────
    assert_eq!(
        ModelTurnRollbackStepV1::ORDER,
        [
            ModelTurnRollbackStepV1::Controller,
            ModelTurnRollbackStepV1::SlotWrappers,
            ModelTurnRollbackStepV1::ProviderContracts,
            ModelTurnRollbackStepV1::ModeOff,
        ],
        "rollback runs controller, then slot wrappers, then provider \
         contracts, then mode off"
    );
    let mut plan = ModelTurnRollbackPlanV1::new();
    assert_eq!(
        plan.complete(ModelTurnRollbackStepV1::ProviderContracts)
            .expect_err("a step out of order must be refused"),
        ModelTurnModeChangeRejection::RollbackOutOfOrder {
            expected: ModelTurnRollbackStepV1::Controller,
            attempted: ModelTurnRollbackStepV1::ProviderContracts,
        }
    );
    for step in ModelTurnRollbackStepV1::ORDER {
        assert_eq!(plan.next_step(), Some(step));
        plan.complete(step).expect("the ordered step is accepted");
    }
    assert!(plan.is_complete());
    assert!(plan.next_step().is_none());
    assert!(
        plan.complete(ModelTurnRollbackStepV1::ModeOff).is_err(),
        "a completed plan accepts nothing further"
    );

    // ── `off` is reachable only after a drain, and drains one pool only ───
    let enforcing = cfni_seed_pool(&db, "cfni-rollback-enforcing", "enforce").await;
    repository
        .seed_request_bucket_binding_for_test(enforcing, 8, 8)
        .await
        .expect("seed the request binding");
    assert_eq!(
        repository
            .set_pool_mode_in_transaction(ModelTurnModeChangeInput {
                pool_id: enforcing,
                target_mode: ModelTurnAdmissionPhase::Off,
                reason: ModelTurnModeChangeReason::Rollback,
                controller_generation: CFNI_GENERATION,
            })
            .await
            .expect("mode change must not error"),
        ModelTurnModeChangeOutcome::Rejected(ModelTurnModeChangeRejection::UnsupportedTransition {
            from: ModelTurnAdmissionPhase::Enforce,
            to: ModelTurnAdmissionPhase::Off,
        }),
        "an enforcing pool cannot go straight to off; it drains first"
    );
    let bystander_mode_before = repository
        .pool_control_state_for_test(bystander)
        .await
        .expect("pool state")
        .expect("pool exists");
    assert!(
        matches!(
            repository
                .drain_pool_in_transaction(
                    enforcing,
                    CFNI_GENERATION,
                    ModelTurnModeChangeReason::Rollback,
                )
                .await
                .expect("drain must not error"),
            ModelTurnModeChangeOutcome::Applied { .. }
                | ModelTurnModeChangeOutcome::DrainedAndSettled { .. }
        ),
        "the selected pool drains"
    );
    assert_eq!(
        repository
            .pool_control_state_for_test(bystander)
            .await
            .expect("pool state")
            .expect("pool exists"),
        bystander_mode_before,
        "draining one pool must not touch another pool's mode or target"
    );

    // Additive data survives rollback: the phase ledger and the pool rows the
    // earlier steps wrote are still readable.
    assert!(
        !repository
            .phase_transitions(selected, 8)
            .await
            .expect("read the phase ledger")
            .is_empty(),
        "rollback preserves the additive decision ledger"
    );
    assert_eq!(
        djinn_db::test_support::table_digest_for_test(&db, "user_settings").await,
        settings_before,
        "no part of this scenario may rewrite max_sessions or lane_max_sessions"
    );

    close(db).await;
}

// ═══════════════════════════════════════════════════════════════════════════
// The required-scenario manifest
// ═══════════════════════════════════════════════════════════════════════════

/// One normative criterion group of proposal `96fy` and the scenario that owns
/// it.
struct RequiredScenario {
    /// The 1-based index of the criterion group in `96fy`'s acceptance
    /// criteria. The eleventh criterion names the command itself and is
    /// discharged by the target existing and passing, not by a scenario.
    group: usize,
    /// A one-line restatement of what the group requires, so a reviewer can
    /// check the pairing without leaving this file.
    criterion: &'static str,
    /// The scenario function's name. Compared against the set of `scenario_*`
    /// functions this file actually defines.
    name: &'static str,
    /// The scenario function **item**. Holding the item rather than only its
    /// name is what makes the manifest impossible to drift away from silently:
    /// deleting the function, or renaming it without updating this entry, does
    /// not compile.
    scenario: fn(),
}

/// The manifest. Ten groups, ten scenarios, one each.
///
/// This is the checkable replacement for `96fy`'s original eleventh criterion
/// ("no substitute target or self-selected test set satisfies verification"),
/// which is a universal negative quantified over every possible test set and
/// which no code change and no test run can establish. What *is* checkable is
/// that this fixed list equals the set of scenarios the fixed target
/// registers, and `required_scenarios_manifest_covers_every_criterion_group`
/// below asserts exactly that.
const REQUIRED_SCENARIOS: &[RequiredScenario] = &[
    RequiredScenario {
        group: 1,
        criterion: "Atomic multi-bucket acquisition, dispatch marking before network send, \
                    heartbeating, the abort/expiry boundary, idempotent reconciliation, and a \
                    multi-pod barrier that permits exactly one dispatch at target 1.",
        name: "scenario_01_enforced_attempt_is_atomic_fenced_and_reconciled_once",
        scenario: scenario_01_enforced_attempt_is_atomic_fenced_and_reconciled_once,
    },
    RequiredScenario {
        group: 2,
        criterion: "Independent immutable generations: expiring one lease never mutates another, \
                    every late mutation from the expired owner fails, and concurrency and units \
                    are credited at most once.",
        name: "scenario_02_expiring_one_lease_leaves_its_sibling_untouched",
        scenario: scenario_02_expiring_one_lease_leaves_its_sibling_untouched,
    },
    RequiredScenario {
        group: 3,
        criterion: "Conservative request/input/output estimates and atomic reservation: with one \
                    remaining unit exactly one caller dispatches and the other receives a typed \
                    wait.",
        name: "scenario_03_reservation_is_conservative_and_atomic_at_one_unit",
        scenario: scenario_03_reservation_is_conservative_and_atomic_at_one_unit,
    },
    RequiredScenario {
        group: 4,
        criterion: "Enforcement is keyed by the existing credential record, secret rotation \
                    preserves state, ambiguous/colliding/revoked records never enforce, and \
                    telemetry carries no secret or identifier.",
        name: "scenario_04_identity_is_keyed_by_the_credential_record_and_never_leaks",
        scenario: scenario_04_identity_is_keyed_by_the_credential_record_and_never_leaks,
    },
    RequiredScenario {
        group: 5,
        criterion: "The normative aggregate output-rate traces: 100, 100 (plateau) and 140 \
                    tokens/second, computed over the union of active wall-clock intervals with \
                    tokens assigned by emission timestamp and streams clipped to the window.",
        name: "scenario_05_aggregate_output_rate_divides_by_the_union_of_active_wall_clock",
        scenario: scenario_05_aggregate_output_rate_divides_by_the_union_of_active_wall_clock,
    },
    RequiredScenario {
        group: 6,
        criterion: "The controller ladder: 1 to 9 on eight qualifying probes, back to 8 on one \
                    deduplicated loss, holds on duplicate loss and ineligible windows, three \
                    rejected probes, five plateau windows, never leaving [1, 32], and no \
                    emergency-cap setting read.",
        name: "scenario_06_controller_ladder_probes_backs_off_and_stays_in_bounds",
        scenario: scenario_06_controller_ladder_probes_backs_off_and_stays_in_bounds,
    },
    RequiredScenario {
        group: 7,
        criterion: "Off, shadow and enforce all preserve the exact existing conjunction of \
                    max_sessions (missing = 1) and lane_max_sessions, with no second resident \
                    authority.",
        name: "scenario_07_resident_conjunction_is_identical_across_admission_modes",
        scenario: scenario_07_resident_conjunction_is_identical_across_admission_modes,
    },
    RequiredScenario {
        group: 8,
        criterion: "An open breaker dispatches nothing, controller actions never reset breakers, \
                    every retry releases before acquiring afresh, provider-internal retries \
                    cannot be wrapped at all, and cancelling a typed wait is idempotent.",
        name: "scenario_08_breakers_are_senior_and_retries_release_before_acquiring",
        scenario: scenario_08_breakers_are_senior_and_retries_release_before_acquiring,
    },
    RequiredScenario {
        group: 9,
        criterion: "Compatibility phases A through D with each prerequisite independently removed \
                    or staled; incomplete phase-C coverage stays diagnostic and never trains; a \
                    phase-D pool drains before a later acquisition commits and an unleased \
                    enforced attempt sends zero provider bytes.",
        name: "scenario_09_every_compatibility_prerequisite_is_independently_load_bearing",
        scenario: scenario_09_every_compatibility_prerequisite_is_independently_load_bearing,
    },
    RequiredScenario {
        group: 10,
        criterion: "The expected-path denominator is built from dispatch topology and live Ready \
                    slots, silence and revision skew stay uncovered rather than disappearing, and \
                    rollback runs in order, draining only the selected pool and preserving \
                    additive data and legacy settings.",
        name: "scenario_10_the_coverage_denominator_is_authoritative_and_rollback_is_ordered",
        scenario: scenario_10_the_coverage_denominator_is_authoritative_and_rollback_is_ordered,
    },
];

/// One load-bearing test that is not a `scenario_*` function, and the clause it
/// carries.
///
/// Adversarial verification found that [`REQUIRED_SCENARIOS`] pinned only
/// functions whose names begin with `scenario_`, leaving four tests that each
/// discharge part of a criterion completely unpinned — deleting any of them
/// left the manifest test green. `kueue_pending_workload_…` is the clearest
/// case: it is what actually discharges the Kueue clause of criterion 7.
struct SupportingTest {
    /// The criterion group this test contributes to, or `0` when it guards the
    /// target's own integrity rather than a numbered criterion.
    group: usize,
    /// What this test is here to establish.
    clause: &'static str,
    name: &'static str,
    /// The function **item**, for the same reason [`RequiredScenario`] holds
    /// one: deleting it, or renaming it without updating this entry, does not
    /// compile.
    test: fn(),
}

/// Every load-bearing test in this target that is not a numbered scenario.
///
/// `required_scenarios_manifest_covers_every_criterion_group` asserts that this
/// list, plus [`REQUIRED_SCENARIOS`], plus the manifest test itself, is exactly
/// the set of tests the target defines — so a load-bearing test cannot be added
/// without being registered here, and cannot be deleted without breaking the
/// build.
const REQUIRED_SUPPORTING_TESTS: &[SupportingTest] = &[
    SupportingTest {
        group: 0,
        clause: "Phase A's durable prerequisite: the schema marker is installed at the revision \
                 this binary understands, a pool seeded under it resolves and writes exactly one \
                 lease row, and an unresolvable pool writes zero.",
        name: "phase_a_schema_prerequisite",
        test: phase_a_schema_prerequisite,
    },
    SupportingTest {
        group: 7,
        clause: "The resident admission seam is reachable from outside the crate, so criterion \
                 7's conjunction is asserted against the function dispatch actually calls.",
        name: "resident_admission_seam_is_reachable_out_of_crate",
        test: resident_admission_seam_is_reachable_out_of_crate,
    },
    SupportingTest {
        group: 7,
        clause: "Role-to-lane mapping is pinned, so a lane cap asserted for one role cannot \
                 silently start applying to another.",
        name: "model_lane_role_mapping_is_pinned",
        test: model_lane_role_mapping_is_pinned,
    },
    SupportingTest {
        group: 7,
        clause: "The census of session-cap and lane-cap call sites is pinned, so no second \
                 resident authority can appear alongside the one criterion 7 tests.",
        name: "resident_admission_call_sites_are_pinned",
        test: resident_admission_call_sites_are_pinned,
    },
    SupportingTest {
        group: 7,
        clause: "A Kueue-pending workload writes no model-turn lease and receives no replacement \
                 dispatch: the Kueue clause of criterion 7.",
        name: "kueue_pending_workload_writes_no_lease_and_gets_no_replacement_dispatch",
        test: kueue_pending_workload_writes_no_lease_and_gets_no_replacement_dispatch,
    },
    SupportingTest {
        group: 4,
        clause: "The emitted (metric, label key, label value) set equals the Phase-D allow list \
                 exactly, so telemetry can carry no credential or request identifier.",
        name: "phase_d_bounded_telemetry_matches_the_allow_list_exactly",
        test: phase_d_bounded_telemetry_matches_the_allow_list_exactly,
    },
    SupportingTest {
        group: 9,
        clause: "`enforce` has no production caller outside the guarded leader pass, which is the \
                 fact criterion 9's coverage argument rests on.",
        name: "enforce_has_no_production_caller_outside_the_guarded_leader_pass",
        test: enforce_has_no_production_caller_outside_the_guarded_leader_pass,
    },
    SupportingTest {
        group: 1,
        clause: "The 20-second heartbeat cadence and the 40-second abort deadline, proven as \
                 behaviour under paused time against the production watchdog loop, and the \
                 partition-to-abort-to-quarantine-to-replacement chronology that follows the \
                 abort.",
        name: "the_turn_watchdog_commits_every_twenty_seconds_and_aborts_after_forty",
        test: the_turn_watchdog_commits_every_twenty_seconds_and_aborts_after_forty,
    },
    SupportingTest {
        group: 1,
        clause: "A production launch of an enforced covered attempt spawns the watchdog: the \
                 lease commits a real heartbeat twenty seconds later, so criterion 1's cadence \
                 describes something every enforced attempt reaches and not only a loop that \
                 would behave if anything ran it.",
        name: "an_enforced_covered_attempt_launch_spawns_the_turn_watchdog",
        test: an_enforced_covered_attempt_launch_spawns_the_turn_watchdog,
    },
    SupportingTest {
        group: 6,
        clause: "The subscription controller is called from the fenced leader cycle and \
                 `learned_concurrency` has a fenced production writer, so criterion 6's ladder \
                 describes something production can reach.",
        name: "the_subscription_learner_is_wired_to_the_fenced_leader_cycle",
        test: the_subscription_learner_is_wired_to_the_fenced_leader_cycle,
    },
];

/// The manifest test itself. It is the one test in this target that no manifest
/// lists, because a manifest that had to list itself would be satisfied by its
/// own presence — it guards the others, and the `[[test]]` entry in
/// `Cargo.toml` guards it.
const MANIFEST_TEST: &str = "required_scenarios_manifest_covers_every_criterion_group";

/// The number of normative criterion groups in proposal `96fy`.
const NORMATIVE_CRITERION_GROUPS: usize = 10;

/// Every `scenario_*` function this target's source actually defines.
/// Every `#[test]` / `#[tokio::test]` function this target's source defines.
///
/// The scan is deliberately over the attribute rather than over the function
/// name: a test cannot avoid it by being named something other than
/// `scenario_*`, which is exactly how four load-bearing tests went unpinned
/// before task `kcso`. Helper functions declared inside a test body carry no
/// test attribute and are not collected.
fn registered_test_functions(source: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut expecting = false;
    for line in source.lines().map(str::trim_start) {
        if line.starts_with("#[test]") || line.starts_with("#[tokio::test") {
            expecting = true;
            continue;
        }
        if !expecting {
            continue;
        }
        let Some(rest) = line
            .strip_prefix("async fn ")
            .or_else(|| line.strip_prefix("fn "))
        else {
            // Attributes may stack between the test attribute and the item.
            continue;
        };
        expecting = false;
        names.push(
            rest.chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect::<String>(),
        );
    }
    names.sort();
    names.dedup();
    names
}

fn registered_scenario_functions(source: &str) -> Vec<String> {
    let mut names: Vec<String> = source
        .lines()
        .map(str::trim_start)
        .filter_map(|line| {
            let rest = line
                .strip_prefix("async fn ")
                .or_else(|| line.strip_prefix("fn "))?;
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            name.starts_with("scenario_").then_some(name)
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The manifest lists one scenario per normative criterion group, and equals
/// the set of scenarios the target registers.
///
/// **What fails, and why that is the point.**
///
/// * *Deleting* a scenario function: the manifest entry holds the function
///   item, so the target stops compiling and this test cannot run at all.
/// * *Renaming* a scenario function: same — the item reference dangles. If the
///   item reference is updated but the `name` string is not, the set equality
///   below fails; if only the string is updated, the item reference dangles.
///   Either way a rename is red until the manifest is updated in the same
///   change, which is the reviewable event the criterion asks for.
/// * *Adding* a `scenario_*` function without registering it: the set equality
///   fails, so the manifest cannot fall behind the target either.
/// * *Pointing two groups at one function*: the distinct-address check fails.
/// * *Adding **any** test without registering it*: the second half scans for
///   `#[test]`/`#[tokio::test]` attributes rather than for names, and requires
///   the set it finds to equal `REQUIRED_SCENARIOS` ∪ `REQUIRED_SUPPORTING_TESTS`
///   ∪ this test. Before task `kcso` the manifest bound only functions named
///   `scenario_*`, so four load-bearing tests were pinned by nothing at all.
///
/// Both scans are guarded against silently finding nothing by sentinel
/// assertions: each must find at least as many names as its manifest lists, and
/// each must find a name the source demonstrably contains.
#[test]
fn required_scenarios_manifest_covers_every_criterion_group() {
    assert_eq!(
        REQUIRED_SCENARIOS.len(),
        NORMATIVE_CRITERION_GROUPS,
        "proposal 96fy has ten normative criterion groups and the manifest must \
         list a scenario for each"
    );
    for (index, entry) in REQUIRED_SCENARIOS.iter().enumerate() {
        assert_eq!(
            entry.group,
            index + 1,
            "the manifest is ordered by criterion group, and every group from 1 \
             to {NORMATIVE_CRITERION_GROUPS} appears exactly once"
        );
        assert!(
            entry.criterion.len() > 40,
            "group {} must restate its criterion, not label it",
            entry.group
        );
    }

    // No two groups may be discharged by the same function.
    let mut addresses: Vec<usize> = REQUIRED_SCENARIOS
        .iter()
        .map(|entry| entry.scenario as usize)
        .collect();
    addresses.sort_unstable();
    addresses.dedup();
    assert_eq!(
        addresses.len(),
        REQUIRED_SCENARIOS.len(),
        "each criterion group needs its own scenario; two entries point at the \
         same function"
    );

    let source = read_target_source();
    let registered = registered_scenario_functions(&source);
    assert!(
        registered.len() >= NORMATIVE_CRITERION_GROUPS,
        "the source scan found only {} scenario functions; the scan is broken",
        registered.len()
    );
    assert!(
        registered
            .iter()
            .any(|name| name == "scenario_01_enforced_attempt_is_atomic_fenced_and_reconciled_once"),
        "the source scan missed a scenario this file demonstrably defines; the \
         scan is broken"
    );

    let mut manifest: Vec<String> = REQUIRED_SCENARIOS
        .iter()
        .map(|entry| entry.name.to_owned())
        .collect();
    manifest.sort();
    assert_eq!(
        manifest, registered,
        "the required-scenario manifest and the scenarios this target registers \
         must be the same set. A scenario was deleted, renamed or added without \
         updating REQUIRED_SCENARIOS."
    );

    // ── Every load-bearing test, not only the numbered scenarios ──────────
    for entry in REQUIRED_SUPPORTING_TESTS {
        assert!(
            entry.clause.len() > 40,
            "`{}` must restate the clause it carries, not label it",
            entry.name
        );
        assert!(
            entry.group <= NORMATIVE_CRITERION_GROUPS,
            "`{}` names criterion group {}, which does not exist",
            entry.name,
            entry.group
        );
    }
    let mut supporting_addresses: Vec<usize> = REQUIRED_SUPPORTING_TESTS
        .iter()
        .map(|entry| entry.test as usize)
        .collect();
    supporting_addresses.sort_unstable();
    supporting_addresses.dedup();
    assert_eq!(
        supporting_addresses.len(),
        REQUIRED_SUPPORTING_TESTS.len(),
        "two supporting entries point at the same function"
    );

    let defined = registered_test_functions(&source);
    assert!(
        defined.len() >= REQUIRED_SCENARIOS.len() + REQUIRED_SUPPORTING_TESTS.len(),
        "the attribute scan found only {} test functions; the scan is broken",
        defined.len()
    );
    assert!(
        defined.iter().any(|name| name == MANIFEST_TEST),
        "the attribute scan missed this very test; the scan is broken"
    );
    let mut pinned: Vec<String> = REQUIRED_SCENARIOS
        .iter()
        .map(|entry| entry.name.to_owned())
        .chain(
            REQUIRED_SUPPORTING_TESTS
                .iter()
                .map(|entry| entry.name.to_owned()),
        )
        .chain(std::iter::once(MANIFEST_TEST.to_owned()))
        .collect();
    pinned.sort();
    pinned.dedup();
    assert_eq!(
        pinned.len(),
        REQUIRED_SCENARIOS.len() + REQUIRED_SUPPORTING_TESTS.len() + 1,
        "a test is listed in both manifests"
    );
    assert_eq!(
        pinned, defined,
        "every test this target defines must be pinned by REQUIRED_SCENARIOS or \
         REQUIRED_SUPPORTING_TESTS. Before task `kcso` the manifest bound only \
         functions named `scenario_*`, so four load-bearing tests — including \
         the one that discharges criterion 7's Kueue clause — could be deleted \
         with this test still green."
    );
}
