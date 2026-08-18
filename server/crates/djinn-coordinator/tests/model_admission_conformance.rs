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
async fn resident_conjunction_truth_table_is_identical_across_admission_modes() {
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
