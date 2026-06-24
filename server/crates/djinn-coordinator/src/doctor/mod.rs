//! `djinn-agent`-side doctor seed checks.
//!
//! Checks that live in `djinn-agent` (rather than `djinn-core`) because they
//! consume the `pub(crate)` live-mover API exposed under
//! [`crate::supervisor_impl`] and re-exported through
//! [`crate::supervisor_impl`] (the crate-internal module root). A
//! `DoctorCheck` impl in `djinn-core` would either need a public-API bridge
//! (more plumbing) or would have to duplicate the evidence model (rejected by
//! the `pitfalls/coupling-non-pr-diagnostics-to-pr-open-disposition-code`
//! guardrail).
//!
//! The `DoctorCheck` trait itself is defined in [`djinn_core::doctor`]; this
//! module mirrors the framework's shape and is registered into the framework's
//! registry by [`register_doctor_checks`].
//!
//! `dead_code` is allowed at the module level because T4 delivers the check
//! implementation and bridge; T5 wires them into the registry. Until T5
//! lands, the non-test consumers do not yet exist.

#![allow(dead_code)]

pub mod leader_tick;
pub mod live_mover;
pub mod zombie_running_session;

use std::sync::Arc;

use djinn_core::doctor::{DoctorCheck, DoctorRegistry};

pub use live_mover::{ActiveTask, LiveMoverPredicateCheck, LiveMoverSource};
pub use zombie_running_session::{
    SnapshotZombieRunningSessionSource, ZOMBIE_RUNNING_SESSION_CHECK_NAME,
    ZombieRunningSessionCandidate, ZombieRunningSessionCheck, ZombieRunningSessionSource,
    check_from_coordinator_state,
};

/// Register all `djinn-agent`-side seed checks into `registry`.
///
/// Currently registers the `live_mover_predicate` check (T4), bound to
/// `source` — an in-memory or production-backed [`LiveMoverSource`].
///
/// The `source` is held by the registered check via [`std::sync::Arc`]
/// so the registry can store the check as a `'static` trait object. The
/// caller must therefore hand in an `Arc<dyn LiveMoverSource>` —
/// production code wraps a long-lived adapter over the coordinator's
/// evidence collector; tests wrap an in-memory double.
///
/// The control-plane wiring calls this after
/// `djinn_core::doctor::register_default_checks(reg)` so both halves of
/// the seed-check suite share a single [`DoctorRegistry`]. The smoke
/// test in `djinn-agent` exercises the same registration path against a
/// fabricated fixture.
///
/// Returns the list of check names that were replaced by this call
/// (empty if every check was newly registered). Re-registering the same
/// check name replaces the previous registration.
pub fn register_doctor_checks(
    registry: &DoctorRegistry,
    source: Arc<dyn LiveMoverSource>,
) -> Vec<String> {
    let mut registered = Vec::new();
    let check: Arc<dyn DoctorCheck> = Arc::new(LiveMoverPredicateCheck::new(source));
    if let Some(previous) = registry.register(check) {
        registered.push(previous);
    }
    registered
}

// ---------------------------------------------------------------------------
// Agent-side doctor smoke test (T5)
// ---------------------------------------------------------------------------
//
// Verifies that [`register_doctor_checks`] wires the live_mover check
// into a `DoctorRegistry` and that the framework's `doctor_run` API
// drives it correctly end-to-end. Combined with the djinn-core smoke
// test, this covers all 7 seed checks the epic requires.

#[cfg(test)]
mod smoke {
    use super::*;
    use crate::supervisor_impl::LiveMoverEvidence;
    use djinn_core::doctor::{Finding, FindingSeverity, doctor_run};
    use std::sync::Arc;

    /// In-memory `LiveMoverSource` test double. Stages a single task with
    /// `has_live_mover = false` so the check produces one finding.
    #[derive(Default, Clone)]
    struct MemoryLiveMoverSource {
        tasks: Vec<ActiveTask>,
    }

    impl MemoryLiveMoverSource {
        fn divergent() -> Self {
            // An active task with no live mover: every evidence flag is
            // false, so the supervisor predicate collapses to
            // `has_live_mover = false`.
            let task = ActiveTask {
                task_id: "task-orphan-mover".to_owned(),
                status: "in_progress".to_owned(),
                evidence: LiveMoverEvidence::default(),
            };
            Self { tasks: vec![task] }
        }

        fn healthy() -> Self {
            // An active task whose `active_session` evidence is true →
            // `has_live_mover = true` → no finding.
            let task = ActiveTask {
                task_id: "task-healthy-mover".to_owned(),
                status: "in_progress".to_owned(),
                evidence: LiveMoverEvidence {
                    active_session: true,
                    ..Default::default()
                },
            };
            Self { tasks: vec![task] }
        }
    }

    impl LiveMoverSource for MemoryLiveMoverSource {
        fn active_tasks(&self) -> Vec<ActiveTask> {
            self.tasks.clone()
        }
    }

    #[test]
    fn register_doctor_checks_adds_live_mover_predicate() {
        let registry = DoctorRegistry::new();
        let source = Arc::new(MemoryLiveMoverSource::divergent());
        let replaced = register_doctor_checks(&registry, source);
        assert!(
            replaced.is_empty(),
            "fresh registry must accept the live_mover_predicate check without replacement"
        );
        assert_eq!(registry.len(), 1);
        let names: Vec<&str> = registry.enumerate().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["live_mover_predicate"]);
    }

    #[test]
    fn register_doctor_checks_replaces_existing_live_mover_predicate() {
        let registry = DoctorRegistry::new();
        let source_a = Arc::new(MemoryLiveMoverSource::divergent());
        let source_b = Arc::new(MemoryLiveMoverSource::healthy());
        register_doctor_checks(&registry, source_a);
        let replaced = register_doctor_checks(&registry, source_b);
        assert_eq!(
            replaced,
            vec!["live_mover_predicate".to_string()],
            "second registration must report the replaced name"
        );
        assert_eq!(registry.len(), 1, "still one check, the new one");
    }

    #[test]
    fn doctor_run_with_live_mover_predicate_finds_divergent_task() {
        let registry = DoctorRegistry::new();
        let source = Arc::new(MemoryLiveMoverSource::divergent());
        register_doctor_checks(&registry, source);

        let results = doctor_run(&registry, None).expect("run succeeds");
        assert_eq!(results.len(), 1);
        let (name, findings) = &results[0];
        assert_eq!(name, "live_mover_predicate");
        assert_eq!(findings.len(), 1);

        let finding: &Finding = &findings[0];
        assert_eq!(finding.severity, FindingSeverity::Critical);
        assert_eq!(
            finding.entity_ids.get("task_id").map(String::as_str),
            Some("task-orphan-mover")
        );
        assert_eq!(
            finding.resolver_snapshot.resolver,
            "resolve_live_mover_predicate"
        );
    }

    #[test]
    fn doctor_run_with_live_mover_predicate_healthy_source_no_findings() {
        let registry = DoctorRegistry::new();
        let source = Arc::new(MemoryLiveMoverSource::healthy());
        register_doctor_checks(&registry, source);

        let results = doctor_run(&registry, None).expect("run succeeds");
        assert_eq!(results.len(), 1);
        let (_, findings) = &results[0];
        assert!(
            findings.is_empty(),
            "healthy source must produce no findings, got {findings:?}"
        );
    }

    #[test]
    fn doctor_run_named_subset_with_live_mover_predicate() {
        let registry = DoctorRegistry::new();
        let source = Arc::new(MemoryLiveMoverSource::divergent());
        register_doctor_checks(&registry, source);

        let results =
            doctor_run(&registry, Some(&["live_mover_predicate"])).expect("named run succeeds");
        assert_eq!(results.len(), 1);
        let (name, findings) = &results[0];
        assert_eq!(name, "live_mover_predicate");
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn doctor_run_unknown_check_alongside_live_mover_predicate() {
        let registry = DoctorRegistry::new();
        let source = Arc::new(MemoryLiveMoverSource::divergent());
        register_doctor_checks(&registry, source);

        let err =
            doctor_run(&registry, Some(&["nope"])).expect_err("unknown check name must error");
        match err {
            djinn_core::doctor::DoctorError::UnknownCheck(name) => {
                assert!(name.contains("nope"));
            }
            other => panic!("expected UnknownCheck, got {other:?}"),
        }
    }
}
