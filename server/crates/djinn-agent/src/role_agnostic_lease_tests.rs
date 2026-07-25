//! The invocation lease is role-agnostic, and the `class` label is bounded.
//!
//! `djinn_runtime::RoleResourceClass` governs exactly two things: the pod's CPU
//! **request** and **dispatch admission**. It must never reach the invocation
//! lease, which queues purely on measured `cpu.stat`. These tests are the
//! executable half of that claim — the other half is the documented absence of
//! a `may_take_invocation_lease` predicate on the enum.

use crate::process::LeaseInvocationConfig;
use djinn_runtime::RoleResourceClass;
use djinn_telemetry::role_class::{ALL_CLASSES, CLASS_BUILD_CAPABLE, CLASS_LIGHT};

/// `LeaseInvocationRunner` takes NO role input.
///
/// This is a compile-time assertion wearing a test's clothes: the exhaustive
/// destructure below fails to compile the moment anyone adds a field to
/// `LeaseInvocationConfig`, which is the only per-invocation input the runner
/// accepts. A reviewer adding `role` (or `resource_class`, or `is_light`) to
/// make leasing role-dependent has to delete this test to do it, and deleting
/// it is a visible act. Every field listed here is identity, measurement or
/// deadline — nothing about who is driving.
#[test]
fn lease_invocation_config_carries_no_role_field() {
    let config = LeaseInvocationConfig {
        task_id: "task".into(),
        task_run_id: "run".into(),
        pod_uid: "pod".into(),
        cpu_usage_threshold_usec: 1,
        queue_deadline_ms: 1,
        launch_deadline_ms: 1,
        timeout: std::time::Duration::from_secs(1),
    };
    // Exhaustive: no `..` rest pattern. Adding a field breaks this line.
    let LeaseInvocationConfig {
        task_id,
        task_run_id,
        pod_uid,
        cpu_usage_threshold_usec,
        queue_deadline_ms,
        launch_deadline_ms,
        timeout,
    } = config;
    assert_eq!(task_id, "task");
    assert_eq!(task_run_id, "run");
    assert_eq!(pod_uid, "pod");
    assert_eq!(cpu_usage_threshold_usec, 1);
    assert_eq!(queue_deadline_ms, 1);
    assert_eq!(launch_deadline_ms, 1);
    assert_eq!(timeout, std::time::Duration::from_secs(1));
}

/// The `class` label vocabulary is exactly two values, and they are byte-equal
/// to `RoleResourceClass::as_str()`.
///
/// `djinn-telemetry` cannot depend on `djinn-runtime`, so the two spellings are
/// physically separate; this is the seam that keeps them from drifting and the
/// Prometheus cardinality from growing.
#[test]
fn class_label_vocabulary_is_bounded_to_exactly_two_values() {
    assert_eq!(ALL_CLASSES.len(), 2, "the class label must stay two-valued");
    assert_eq!(ALL_CLASSES, [CLASS_LIGHT, CLASS_BUILD_CAPABLE]);
    assert_eq!(RoleResourceClass::Light.as_str(), CLASS_LIGHT);
    assert_eq!(
        RoleResourceClass::BuildCapable.as_str(),
        CLASS_BUILD_CAPABLE
    );

    // Every role name any layer can produce renders one of the two, and nothing
    // else. `for_role_name` fails safe, so this also covers unknown input.
    for name in [
        "worker",
        "reviewer",
        "planner",
        "lead",
        "architect",
        "verifier",
        "refinement",
        "advocate",
        "adversary",
        "judge",
        "grooming",
        "",
        "MYSTERY",
    ] {
        let rendered = RoleResourceClass::for_role_name(name).as_str();
        assert!(
            ALL_CLASSES.contains(&rendered),
            "role {name} rendered an out-of-vocabulary class label: {rendered}"
        );
    }
}

/// The shell handler's classifier is `RoleResourceClass`, and it produces the
/// answers `spec.rs` documents — including the two the deleted local table got
/// wrong: Reviewer is Light (it was classed with Worker) and Architect is
/// build-capable (it was claimed not to run cargo).
#[test]
fn session_role_classification_matches_the_single_classifier() {
    for light in [
        "reviewer",
        "planner",
        "lead",
        "refinement",
        "advocate",
        "adversary",
        "judge",
    ] {
        assert_eq!(
            RoleResourceClass::for_role_name(light),
            RoleResourceClass::Light,
            "{light} must be light"
        );
    }
    for build in ["worker", "architect"] {
        assert_eq!(
            RoleResourceClass::for_role_name(build),
            RoleResourceClass::BuildCapable,
            "{build} must be build-capable"
        );
    }
    // Verifier is an in-pod stage with no dispatch arm; it reaches the
    // fail-safe default, which is the correct answer for a role that compiles.
    assert_eq!(
        RoleResourceClass::for_role_name("verifier"),
        RoleResourceClass::BuildCapable
    );
    // "grooming" is not a role name any layer emits (grooming dispatches as
    // `planner`), so the removed special case now takes the fail-safe path.
    assert_eq!(
        RoleResourceClass::for_role_name("grooming"),
        RoleResourceClass::BuildCapable
    );
}
