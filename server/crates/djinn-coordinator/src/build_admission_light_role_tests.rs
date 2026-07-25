//! Light (orchestration-only) task-run roles must never consume a build slot.
//!
//! Task `h1yv` (folded into `7deu`). The production cap is 3 concurrent build
//! task-runs on a 12-vCPU node. Planners, Reviewers, Leads and the refinement
//! tribunal (Advocate/Adversary/Judge) never run the project's compile/test
//! toolchain, so charging them a slot would queue them behind builds they never
//! compete with and collapse throughput.
//!
//! These tests are deterministic: an in-memory journal, an explicit cap, and
//! durable occupancy read back from the journal rather than inferred.

use std::sync::Arc;

use djinn_db::{AdmissionDomain, AdmissionJournalRepository, Database};
use djinn_k8s::{WarmAdmission, WarmAdmissionTransition};
use djinn_runtime::RoleResourceClass;

use crate::build_admission::{
    BuildAdmissionController, BuildAdmissionDecision, BuildAdmissionMode, TaskRunRole,
};

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

fn controller(mode: BuildAdmissionMode, cap: i64) -> Arc<BuildAdmissionController> {
    let controller = BuildAdmissionController::new(
        Arc::new(AdmissionJournalRepository::new(
            Database::open_in_memory().unwrap(),
        )),
        mode,
        cap,
        "light-role-test-epoch",
    );
    controller.mark_ready();
    Arc::new(controller)
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

async fn occupancy(controller: &BuildAdmissionController) -> i64 {
    controller
        .journal()
        .count_task_or_warm_occupancy()
        .await
        .expect("durable occupancy")
}

/// THE regression. Cap = 1, a build-capable worker already holds the only slot,
/// Enforce mode: every light role must still be Permitted. Before the resource
/// class existed each of these was Denied and left its task queued behind a
/// build it was never going to compete with.
#[tokio::test]
async fn light_roles_are_permitted_while_a_build_holds_the_only_slot() {
    let controller = controller(BuildAdmissionMode::Enforce, 1);

    let holder = match admit(&controller, Some("worker"), "build-holder").await {
        BuildAdmissionDecision::Permitted { permit, .. } => permit,
        other => panic!("a worker must take the only slot, got {other:?}"),
    };
    assert_eq!(occupancy(&controller).await, 1);

    // Control: a second build-capable task-run IS denied at the same cap, so
    // the light permits below are not passing for a trivial reason.
    assert!(
        matches!(
            admit(&controller, Some("worker"), "second-build").await,
            BuildAdmissionDecision::Denied {
                occupancy: 1,
                cap: 1
            }
        ),
        "a second build-capable task-run must be denied at cap 1"
    );
    assert!(
        matches!(
            admit(&controller, Some("architect"), "second-architect").await,
            BuildAdmissionDecision::Denied { .. }
        ),
        "the architect compiles and must be denied at cap 1"
    );

    for role in LIGHT_ROLE_NAMES {
        let decision = admit(&controller, Some(role), &format!("light-{role}")).await;
        assert!(
            matches!(decision, BuildAdmissionDecision::Permitted { .. }),
            "light role {role} must be permitted while the cap is fully consumed, got {decision:?}"
        );
        assert_eq!(
            occupancy(&controller).await,
            1,
            "light role {role} must not add durable occupancy"
        );
    }

    // The build slot is still exactly the worker's, and releasing it hands the
    // slot to the next build — the light admissions changed nothing.
    controller
        .transition(&holder, WarmAdmissionTransition::CreateStarted)
        .await
        .unwrap();
    controller
        .transition(
            &holder,
            WarmAdmissionTransition::Live {
                uid: "holder-uid".to_owned(),
            },
        )
        .await
        .unwrap();
    controller
        .transition(
            &holder,
            WarmAdmissionTransition::Terminal {
                uid: "holder-uid".to_owned(),
            },
        )
        .await
        .unwrap();
    assert_eq!(occupancy(&controller).await, 0);
    assert!(matches!(
        admit(&controller, Some("worker"), "second-build").await,
        BuildAdmissionDecision::Permitted { .. }
    ));
}

/// A light permit's full lifecycle — including the terminal transition — must
/// be a no-op against occupancy. After N light permits are taken and released,
/// a build-capable admit sees exactly the occupancy it would have seen had the
/// light permits never existed.
#[tokio::test]
async fn light_permit_lifecycle_does_not_corrupt_build_occupancy() {
    // Reference run: no light traffic at all.
    let reference = controller(BuildAdmissionMode::Enforce, 2);
    let _reference_build = match admit(&reference, Some("worker"), "ref-build").await {
        BuildAdmissionDecision::Permitted { permit, .. } => permit,
        other => panic!("reference build must be permitted, got {other:?}"),
    };
    let expected = occupancy(&reference).await;
    assert_eq!(expected, 1);

    // Same run, with light permits taken and fully released in between.
    let controller = controller(BuildAdmissionMode::Enforce, 2);
    let mut light_permits = Vec::new();
    for (index, role) in LIGHT_ROLE_NAMES.iter().enumerate() {
        match admit(&controller, Some(role), &format!("light-{index}")).await {
            BuildAdmissionDecision::Permitted { permit, .. } => light_permits.push(permit),
            other => panic!("light role {role} must be permitted, got {other:?}"),
        }
    }
    assert_eq!(
        occupancy(&controller).await,
        0,
        "light permits must reserve nothing"
    );

    for (index, permit) in light_permits.iter().enumerate() {
        let uid = format!("light-uid-{index}");
        // Every transition the dispatch path drives must succeed on a light
        // permit — it is registered in the controller's permit map, so this is
        // a clean no-op rather than an UnknownPermit error.
        controller
            .transition(permit, WarmAdmissionTransition::CreateStarted)
            .await
            .expect("light permit accepts CreateStarted");
        controller
            .transition(permit, WarmAdmissionTransition::Live { uid: uid.clone() })
            .await
            .expect("light permit accepts Live");
        controller
            .transition(permit, WarmAdmissionTransition::Terminal { uid })
            .await
            .expect("light permit accepts Terminal");
    }
    assert_eq!(
        occupancy(&controller).await,
        0,
        "releasing light permits must not push occupancy negative or leak rows"
    );

    let _build = match admit(&controller, Some("worker"), "ref-build").await {
        BuildAdmissionDecision::Permitted { permit, .. } => permit,
        other => panic!("build must be permitted after light traffic, got {other:?}"),
    };
    assert_eq!(
        occupancy(&controller).await,
        expected,
        "light permits must leave build occupancy identical to the reference run"
    );

    // The remaining slot is still real: one more build fits, a third does not.
    assert!(matches!(
        admit(&controller, Some("worker"), "second-build").await,
        BuildAdmissionDecision::Permitted { .. }
    ));
    assert!(
        matches!(
            admit(&controller, Some("worker"), "third-build").await,
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
    let controller = controller(BuildAdmissionMode::Enforce, 1);
    let first = match admit(&controller, Some("planner"), "planner-task").await {
        BuildAdmissionDecision::Permitted { permit, idempotent } => {
            assert!(!idempotent, "the first light admission is not a replay");
            permit
        }
        other => panic!("planner must be permitted, got {other:?}"),
    };
    match admit(&controller, Some("planner"), "planner-task").await {
        BuildAdmissionDecision::Permitted { permit, idempotent } => {
            assert!(idempotent, "a light retry must report itself as a replay");
            assert_eq!(permit, first, "a light retry must return the same permit");
        }
        other => panic!("planner retry must be permitted, got {other:?}"),
    }
    assert_eq!(occupancy(&controller).await, 0);
}

/// The zero-slot class must not widen by accident: an unknown, empty or missing
/// role is still Unclassified (fail-safe), never a free pass.
#[tokio::test]
async fn unknown_roles_still_fail_safe_and_get_no_free_pass() {
    let controller = controller(BuildAdmissionMode::Enforce, 1);
    // Fill the only slot so a "free pass" would be visible as a Permitted.
    assert!(matches!(
        admit(&controller, Some("worker"), "build-holder").await,
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
        let decision = admit(&controller, role, "unknown-role-task").await;
        assert!(
            matches!(decision, BuildAdmissionDecision::Unclassified),
            "role {role:?} must stay Unclassified, got {decision:?}"
        );
    }
    assert_eq!(occupancy(&controller).await, 1);
}

/// Warm graph builds are unchanged: they still consume a slot.
#[tokio::test]
async fn graph_warm_jobs_still_consume_a_slot() {
    let controller = controller(BuildAdmissionMode::Enforce, 1);
    assert!(matches!(
        admit(&controller, Some("planner"), "planner-task").await,
        BuildAdmissionDecision::Permitted { .. }
    ));
    WarmAdmission::admit(
        controller.as_ref(),
        djinn_k8s::WarmAdmissionRequest {
            domain: "ignored".into(),
            work_id: "warm-one".into(),
            generation: 0,
            object_name: "warm-one-0".into(),
        },
    )
    .await
    .expect("the first warm build takes the slot");
    assert_eq!(occupancy(&controller).await, 1);

    let denied = WarmAdmission::admit(
        controller.as_ref(),
        djinn_k8s::WarmAdmissionRequest {
            domain: "ignored".into(),
            work_id: "warm-two".into(),
            generation: 0,
            object_name: "warm-two-0".into(),
        },
    )
    .await;
    assert!(
        denied.is_err(),
        "a warm build still competes for the cap: {denied:?}"
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
