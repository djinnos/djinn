//! Light task-run roles must never be pre-charged a build slot at dispatch.
//!
//! Task `h1yv` (folded into `7deu`). The production cap is 3 concurrent build
//! task-runs on a 12-vCPU node. Planners, Reviewers, Leads and the refinement
//! tribunal (Advocate/Adversary/Judge) are *unlikely* to run the project's
//! compile/test toolchain — measured at 5.5% of light sessions — so charging
//! them a slot 100% of the time would queue them behind builds they almost
//! never compete with and collapse throughput. This is a dispatch-admission
//! prior, not a capability boundary: the minority that do compile are governed
//! by the measured, role-agnostic invocation lease. See
//! [`djinn_runtime::RoleResourceClass`].
//!
//! These tests are deterministic: an in-memory database, an explicit cap, and
//! occupancy read back from the ONE capacity authority rather than inferred.
//! They compose through [`crate::build_admission_capacity_support`], so the
//! controller under test reaches capacity exactly the way `AppState` wires it —
//! a bare controller has no authority at all and permits everything, which
//! would make every assertion below vacuous.

use djinn_db::AdmissionDomain;
use djinn_k8s::{WarmAdmission, WarmAdmissionTransition};
use djinn_runtime::RoleResourceClass;

use crate::build_admission::{
    BuildAdmissionController, BuildAdmissionDecision, BuildAdmissionMode, TaskRunRole,
};
use crate::build_admission_capacity_support::{CapacityHarness, controller_with_capacity};

/// Every role the coordinator can dispatch, in the order `TaskRunRole` declares
/// them. Kept in sync with the enum by
/// [`task_run_role_classification_matches_runtime_resource_class`], whose
/// exhaustive match fails to compile when a variant is added.
const ALL_ROLES: &[TaskRunRole] = &[
    TaskRunRole::Worker,
    TaskRunRole::Reviewer,
    TaskRunRole::Lead,
    TaskRunRole::Planner,
    TaskRunRole::Architect,
    TaskRunRole::Advocate,
    TaskRunRole::Adversary,
    TaskRunRole::Judge,
];

/// The role names the light class must cover, spelled as the dispatch layer
/// spells them: `RoleRegistry` dispatch roles plus refinement `agent_type`s.
const LIGHT_ROLE_NAMES: &[&str] = &[
    "planner",
    "reviewer",
    "lead",
    "advocate",
    "adversary",
    "judge",
];

async fn harness(mode: BuildAdmissionMode, cap: i64) -> CapacityHarness {
    let harness = controller_with_capacity(mode, cap, "light-role-test-epoch").await;
    harness.controller.mark_ready();
    harness
}

async fn admit(
    controller: &BuildAdmissionController,
    role: Option<&str>,
    work_id: &str,
) -> BuildAdmissionDecision {
    controller
        .admit_task_run(
            role,
            AdmissionDomain::TaskObservation,
            work_id.to_owned(),
            0,
            format!("task-run-{work_id}-0"),
        )
        .await
        .expect("admission decision")
}

/// Occupied build SLOTS, from the one capacity authority.
///
/// Deliberately not `count_task_or_warm_occupancy()`: that counts LIFECYCLE
/// rows, which reserve no CPU. The claim every test here makes — "a light role
/// costs nothing" — is a claim about slots.
async fn occupancy(harness: &CapacityHarness) -> i64 {
    harness.occupancy().await
}

/// THE regression. Cap = 1, a build-capable worker already holds the only slot,
/// Enforce mode: every light role must still be Permitted. Before the resource
/// class existed each of these was Denied and left its task queued behind a
/// build it was never going to compete with.
#[tokio::test]
async fn light_roles_are_permitted_while_a_build_holds_the_only_slot() {
    let h = harness(BuildAdmissionMode::Enforce, 1).await;

    let holder = match admit(&h.controller, Some("worker"), "build-holder").await {
        BuildAdmissionDecision::Permitted { permit, .. } => permit,
        other => panic!("a worker must take the only slot, got {other:?}"),
    };
    assert_eq!(occupancy(&h).await, 1);

    // Control: a second build-capable task-run IS denied at the same cap, so
    // the light permits below are not passing for a trivial reason.
    assert!(
        matches!(
            admit(&h.controller, Some("worker"), "second-build").await,
            BuildAdmissionDecision::Denied {
                occupancy: 1,
                cap: 1
            }
        ),
        "a second build-capable task-run must be denied at cap 1"
    );
    assert!(
        matches!(
            admit(&h.controller, Some("architect"), "second-architect").await,
            BuildAdmissionDecision::Denied { .. }
        ),
        "the architect compiles and must be denied at cap 1"
    );

    for role in LIGHT_ROLE_NAMES {
        let decision = admit(&h.controller, Some(role), &format!("light-{role}")).await;
        assert!(
            matches!(decision, BuildAdmissionDecision::Permitted { .. }),
            "light role {role} must be permitted while the cap is fully consumed, got {decision:?}"
        );
        assert_eq!(
            occupancy(&h).await,
            1,
            "light role {role} must not add a build slot"
        );
    }

    // The build slot is still exactly the worker's, and releasing it hands the
    // slot to the next build — the light admissions changed nothing.
    h.controller
        .transition(&holder, WarmAdmissionTransition::CreateStarted)
        .await
        .unwrap();
    h.controller
        .transition(
            &holder,
            WarmAdmissionTransition::Live {
                uid: "holder-uid".to_owned(),
            },
        )
        .await
        .unwrap();
    h.controller
        .transition(
            &holder,
            WarmAdmissionTransition::Terminal {
                uid: "holder-uid".to_owned(),
            },
        )
        .await
        .unwrap();
    // The worker's LEDGER row is gone...
    assert_eq!(
        h.ledger_rows().await,
        0,
        "the released worker leaves no occupying lifecycle row"
    );
    // ...and its slot went straight to the FIFO head. A denied dispatch keeps
    // its queue position, so releasing capacity grants the oldest waiter rather
    // than leaving the pool idle for whoever retries first. `second-build` was
    // denied before `second-architect`, so it is the one that now holds the
    // slot — which is exactly what its retry below observes.
    assert_eq!(
        occupancy(&h).await,
        1,
        "the released slot is handed to the queued build, not to nobody"
    );
    assert!(matches!(
        admit(&h.controller, Some("worker"), "second-build").await,
        BuildAdmissionDecision::Permitted { .. }
    ));
    assert_eq!(
        occupancy(&h).await,
        1,
        "the successor occupies the SAME one slot; the cap never widened"
    );
    assert!(
        matches!(
            admit(&h.controller, Some("architect"), "second-architect").await,
            BuildAdmissionDecision::Denied { .. }
        ),
        "the still-queued architect stays denied behind the successor"
    );
}

/// A light permit's full lifecycle — including the terminal transition — must
/// be a no-op against occupancy. After N light permits are taken and released,
/// a build-capable admit sees exactly the occupancy it would have seen had the
/// light permits never existed.
#[tokio::test]
async fn light_permit_lifecycle_does_not_corrupt_build_occupancy() {
    // Reference run: no light traffic at all.
    let reference = harness(BuildAdmissionMode::Enforce, 2).await;
    let _reference_build = match admit(&reference.controller, Some("worker"), "ref-build").await {
        BuildAdmissionDecision::Permitted { permit, .. } => permit,
        other => panic!("reference build must be permitted, got {other:?}"),
    };
    let expected = occupancy(&reference).await;
    assert_eq!(expected, 1);

    // Same run, with light permits taken and fully released in between.
    let h = harness(BuildAdmissionMode::Enforce, 2).await;
    let mut light_permits = Vec::new();
    for (index, role) in LIGHT_ROLE_NAMES.iter().enumerate() {
        match admit(&h.controller, Some(role), &format!("light-{index}")).await {
            BuildAdmissionDecision::Permitted { permit, .. } => light_permits.push(permit),
            other => panic!("light role {role} must be permitted, got {other:?}"),
        }
    }
    assert_eq!(occupancy(&h).await, 0, "light permits must reserve nothing");

    for (index, permit) in light_permits.iter().enumerate() {
        let uid = format!("light-uid-{index}");
        // Every transition the dispatch path drives must succeed on a light
        // permit — it is registered in the controller's permit map, so this is
        // a clean no-op rather than an UnknownPermit error.
        h.controller
            .transition(permit, WarmAdmissionTransition::CreateStarted)
            .await
            .expect("light permit accepts CreateStarted");
        h.controller
            .transition(permit, WarmAdmissionTransition::Live { uid: uid.clone() })
            .await
            .expect("light permit accepts Live");
        h.controller
            .transition(permit, WarmAdmissionTransition::Terminal { uid })
            .await
            .expect("light permit accepts Terminal");
    }
    assert_eq!(
        occupancy(&h).await,
        0,
        "releasing light permits must not push occupancy negative or leak slots"
    );

    let _build = match admit(&h.controller, Some("worker"), "ref-build").await {
        BuildAdmissionDecision::Permitted { permit, .. } => permit,
        other => panic!("build must be permitted after light traffic, got {other:?}"),
    };
    assert_eq!(
        occupancy(&h).await,
        expected,
        "light permits must leave build occupancy identical to the reference run"
    );

    // The remaining slot is still real: one more build fits, a third does not.
    assert!(matches!(
        admit(&h.controller, Some("worker"), "second-build").await,
        BuildAdmissionDecision::Permitted { .. }
    ));
    assert!(
        matches!(
            admit(&h.controller, Some("worker"), "third-build").await,
            BuildAdmissionDecision::Denied { .. }
        ),
        "the zero-slot class must not widen the effective cap"
    );
}

/// A light admission is idempotent on the same task generation: a retry returns
/// the SAME permit rather than minting a second one, matching how a reserved
/// build-capable permit replays.
#[tokio::test]
async fn light_admission_is_idempotent_for_the_same_generation() {
    let h = harness(BuildAdmissionMode::Enforce, 1).await;
    let first = match admit(&h.controller, Some("planner"), "planner-task").await {
        BuildAdmissionDecision::Permitted { permit, idempotent } => {
            assert!(!idempotent, "the first light admission is not a replay");
            permit
        }
        other => panic!("planner must be permitted, got {other:?}"),
    };
    match admit(&h.controller, Some("planner"), "planner-task").await {
        BuildAdmissionDecision::Permitted { permit, idempotent } => {
            assert!(idempotent, "a light retry must report itself as a replay");
            assert_eq!(permit, first, "a light retry must return the same permit");
        }
        other => panic!("planner retry must be permitted, got {other:?}"),
    }
    assert_eq!(occupancy(&h).await, 0);
}

/// The zero-slot class must not widen by accident: an unknown, empty or missing
/// role is still Unclassified (fail-safe), never a free pass.
#[tokio::test]
async fn unknown_roles_still_fail_safe_and_get_no_free_pass() {
    let h = harness(BuildAdmissionMode::Enforce, 1).await;
    // Fill the only slot so a "free pass" would be visible as a Permitted.
    assert!(matches!(
        admit(&h.controller, Some("worker"), "build-holder").await,
        BuildAdmissionDecision::Permitted { .. }
    ));

    for role in [
        Some("mystery"),
        Some("verifier"),
        Some("Planner"),
        Some("grooming"),
        Some(""),
        None,
    ] {
        let decision = admit(&h.controller, role, "unknown-role-task").await;
        assert!(
            matches!(decision, BuildAdmissionDecision::Unclassified),
            "role {role:?} must stay Unclassified, got {decision:?}"
        );
    }
    assert_eq!(occupancy(&h).await, 1);
}

/// Warm graph builds are unchanged: they still consume a slot, and it is the
/// SAME slot a build-capable task-run would have taken.
///
/// Where the slot is taken moved, and only that. The warmer acquires its
/// graph-warm lease BEFORE it reaches admission, so the admission call is a
/// ledger append that cannot deny; capacity is refused one layer earlier, at
/// the lease. Asserting the denial at admission would now assert nothing, since
/// `CapacitySource::HeldByLease` never consults a cap.
#[tokio::test]
async fn graph_warm_jobs_still_consume_a_slot() {
    let h = harness(BuildAdmissionMode::Enforce, 1).await;
    assert!(matches!(
        admit(&h.controller, Some("planner"), "planner-task").await,
        BuildAdmissionDecision::Permitted { .. }
    ));
    let held = h
        .hold_warm_lease("warm-one")
        .await
        .expect("the first warm build takes the slot");
    WarmAdmission::admit(
        h.controller.as_ref(),
        djinn_k8s::WarmAdmissionRequest {
            domain: "ignored".into(),
            work_id: "warm-one".into(),
            generation: 0,
            object_name: "warm-one-0".into(),
        },
    )
    .await
    .expect("a leased warm build still writes its lifecycle row");
    assert_eq!(occupancy(&h).await, 1, "the warm Job occupies the one slot");
    assert_eq!(h.ledger_rows().await, 1, "and is visible in the ledger");

    assert!(
        h.hold_warm_lease("warm-two").await.is_none(),
        "a second warm build cannot get a lease while the pool is full"
    );
    // The unification, stated directly: warm and dispatch draw from ONE pool,
    // so the warm Job's slot denies a build-capable task-run. Before this they
    // each had their own N and the node ran 2N.
    assert!(
        matches!(
            admit(&h.controller, Some("worker"), "build-after-warm").await,
            BuildAdmissionDecision::Denied {
                occupancy: 1,
                cap: 1
            }
        ),
        "a warm Job's slot must deny a build-capable task-run at the same cap"
    );

    // Handing the warm lease back returns the slot to the same shared pool.
    h.release_warm_lease(held).await;
    assert!(
        matches!(
            admit(&h.controller, Some("worker"), "build-after-warm").await,
            BuildAdmissionDecision::Permitted { .. }
        ),
        "the released warm slot is available to dispatch"
    );
}

/// `TaskRunRole` must agree with `djinn_runtime::RoleResourceClass` for every
/// variant. The match is exhaustive on purpose: adding a `TaskRunRole` variant
/// without deciding its resource class fails to compile here rather than
/// silently inheriting a default.
#[test]
fn task_run_role_classification_matches_runtime_resource_class() {
    for role in ALL_ROLES {
        let expected = match role {
            // Runs the project's compile/test toolchain.
            TaskRunRole::Worker | TaskRunRole::Architect => RoleResourceClass::BuildCapable,
            // Orchestration-only.
            TaskRunRole::Reviewer
            | TaskRunRole::Lead
            | TaskRunRole::Planner
            | TaskRunRole::Advocate
            | TaskRunRole::Adversary
            | TaskRunRole::Judge => RoleResourceClass::Light,
        };
        assert_eq!(
            role.resource_class(),
            expected,
            "{} disagrees with the runtime resource class",
            role.as_str()
        );
        assert_eq!(
            role.resource_class(),
            RoleResourceClass::for_role_name(role.as_str()),
            "{} must delegate to djinn_runtime, not a second local table",
            role.as_str()
        );
        assert_eq!(
            role.resource_class().gated_at_dispatch(),
            expected == RoleResourceClass::BuildCapable,
            "{} dispatch gating must follow its class",
            role.as_str()
        );
    }
}

/// `as_str` is the exact inverse of `parse` for every variant, which is what
/// makes delegating to `RoleResourceClass::for_role_name` sound.
#[test]
fn task_run_role_as_str_round_trips_through_parse() {
    for role in ALL_ROLES {
        assert_eq!(
            TaskRunRole::parse(Some(role.as_str())),
            Some(*role),
            "{} must round-trip",
            role.as_str()
        );
    }
    // Every light dispatch-role name the coordinator can emit parses to a
    // light variant.
    for name in LIGHT_ROLE_NAMES {
        let role = TaskRunRole::parse(Some(name)).expect("light role must classify");
        assert_eq!(
            role.resource_class(),
            RoleResourceClass::Light,
            "{name} must be light"
        );
    }
}
