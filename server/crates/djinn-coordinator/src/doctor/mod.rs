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

#![allow(dead_code)]

pub mod leader_tick;
pub mod live_mover;
pub mod closed_parent_open_children;
pub mod stranded_ready;
pub mod zombie_running_session;

use std::sync::Arc;

use djinn_core::doctor::{DoctorCheck, DoctorRegistry};

pub use live_mover::{ActiveTask, LiveMoverPredicateCheck, LiveMoverSource};
pub use closed_parent_open_children::{
    CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME, ClosedParentOpenChildrenCheck,
    ClosedParentOpenChildrenSource, MemoryClosedParentOpenChildrenSource,
    TaskRepositoryClosedParentOpenChildrenSource,
};
pub use stranded_ready::{
    MemoryStrandedReadySource, STRANDED_READY_CHECK_NAME, StrandedReadyCandidate,
    StrandedReadyCheck, StrandedReadySource, TaskRepositoryStrandedReadySource,
};
pub use zombie_running_session::{
    SnapshotZombieRunningSessionSource, ZOMBIE_RUNNING_SESSION_CHECK_NAME,
    ZombieRunningSessionCandidate, ZombieRunningSessionCheck, ZombieRunningSessionSource,
    check_from_coordinator_state,
};

/// Register all `djinn-agent`-side seed checks into `registry`.
///
/// Registers the `live_mover_predicate` and `stranded_ready` checks. Both
/// sources are held via [`std::sync::Arc`] so the registry can store the
/// checks as `'static` trait objects. Production code wraps long-lived adapters
/// over the coordinator's evidence collector and DB repository; tests wrap
/// in-memory doubles.
///
/// The control-plane wiring calls this after
/// `djinn_core::doctor::register_default_checks(reg)` so both halves of
/// the seed-check suite share a single [`DoctorRegistry`].
///
/// Returns the list of check names that were replaced by this call
/// (empty if every check was newly registered). Re-registering the same
/// check name replaces the previous registration.
pub fn register_doctor_checks(
    registry: &DoctorRegistry,
    live_mover_source: Arc<dyn LiveMoverSource>,
    stranded_source: Arc<dyn StrandedReadySource>,
) -> Vec<String> {
    let mut registered = Vec::new();
    let live_mover: Arc<dyn DoctorCheck> =
        Arc::new(LiveMoverPredicateCheck::new(live_mover_source));
    if let Some(previous) = registry.register(live_mover) {
        registered.push(previous);
    }
    let stranded: Arc<dyn DoctorCheck> = Arc::new(StrandedReadyCheck::new(stranded_source));
    if let Some(previous) = registry.register(stranded) {
        registered.push(previous);
    }
    registered
}

/// Register the closed-parent orphan dry-run check while preserving the older
/// two-source registration API used by coordinator tests.
pub fn register_closed_parent_open_children_check(
    registry: &DoctorRegistry,
    source: Arc<dyn ClosedParentOpenChildrenSource>,
) -> Option<String> {
    registry.register(Arc::new(ClosedParentOpenChildrenCheck::new(source)))
}

// ---------------------------------------------------------------------------
// Agent-side doctor smoke test (T5)
// ---------------------------------------------------------------------------
//
// Verifies that [`register_doctor_checks`] wires both the live_mover and
// stranded_ready checks into a `DoctorRegistry` and that the framework's
// `doctor_run` API drives them correctly end-to-end.

#[cfg(test)]
mod smoke {
    use super::*;
    use crate::supervisor_impl::LiveMoverEvidence;
    use djinn_core::doctor::{Finding, FindingSeverity, doctor_run};
    use serde_json::json;
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

    fn empty_stranded_source() -> Arc<dyn StrandedReadySource> {
        Arc::new(MemoryStrandedReadySource::new(json!({
            "total": 0,
            "threshold_minutes": 30,
            "findings": [],
        })))
    }

    fn one_stranded_source() -> Arc<dyn StrandedReadySource> {
        Arc::new(MemoryStrandedReadySource::new(json!({
            "total": 1,
            "threshold_minutes": 30,
            "findings": [{
                "id": "task-stranded-1",
                "short_id": "ts1",
                "title": "Stranded",
                "status": "open",
                "owner": "owner",
                "epic_short_id": "ep01",
                "unclaimed_since": "2026-01-01T00:00:00.000Z",
                "unclaimed_since_confidence": "high",
                "elapsed_minutes": 45,
                "severity": "warn",
                "threshold": {"warning_minutes": 30, "error_minutes": 60, "critical_minutes": 180},
                "dispatch_gate": {
                    "evaluated_role": "worker",
                    "toolset": ["task_edit"],
                    "model_requirement": "provider/model-a",
                    "image_ready": true,
                    "breaker_open": false,
                    "manually_paused": false,
                    "rate_limited": false,
                    "credential_available": true,
                    "gate_verdict": "stranded",
                    "reasons": []
                }
            }],
        })))
    }

    #[test]
    fn register_doctor_checks_adds_both_checks() {
        let registry = DoctorRegistry::new();
        let live_mover = Arc::new(MemoryLiveMoverSource::divergent());
        let stranded = empty_stranded_source();
        let replaced = register_doctor_checks(&registry, live_mover, stranded);
        assert!(
            replaced.is_empty(),
            "fresh registry must accept both checks without replacement"
        );
        assert_eq!(registry.len(), 2);
        let mut names: Vec<&str> = registry.enumerate().into_iter().map(|(n, _)| n).collect();
        names.sort();
        assert_eq!(names, vec!["live_mover_predicate", "stranded_ready"]);
    }

    #[test]
    fn register_doctor_checks_replaces_existing_registrations() {
        let registry = DoctorRegistry::new();
        let live_mover_a = Arc::new(MemoryLiveMoverSource::divergent());
        let live_mover_b = Arc::new(MemoryLiveMoverSource::healthy());
        let stranded_a = empty_stranded_source();
        let stranded_b = one_stranded_source();
        register_doctor_checks(&registry, live_mover_a, stranded_a);
        let mut replaced = register_doctor_checks(&registry, live_mover_b, stranded_b);
        replaced.sort();
        assert_eq!(
            replaced,
            vec![
                "live_mover_predicate".to_string(),
                "stranded_ready".to_string()
            ],
            "second registration must report the replaced names"
        );
        assert_eq!(registry.len(), 2, "still two checks, the new ones");
    }

    #[test]
    fn doctor_run_with_live_mover_predicate_finds_divergent_task() {
        let registry = DoctorRegistry::new();
        let live_mover = Arc::new(MemoryLiveMoverSource::divergent());
        let stranded = empty_stranded_source();
        register_doctor_checks(&registry, live_mover, stranded);

        let results = doctor_run(&registry, None).expect("run succeeds");
        assert_eq!(results.len(), 2);
        let live_mover_results: Vec<_> = results
            .iter()
            .filter(|(name, _)| name == "live_mover_predicate")
            .collect();
        assert_eq!(live_mover_results.len(), 1);
        let (_, findings) = live_mover_results[0];
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
        let live_mover = Arc::new(MemoryLiveMoverSource::healthy());
        let stranded = empty_stranded_source();
        register_doctor_checks(&registry, live_mover, stranded);

        let results = doctor_run(&registry, None).expect("run succeeds");
        let live_mover_results: Vec<_> = results
            .iter()
            .filter(|(name, _)| name == "live_mover_predicate")
            .collect();
        let (_, findings) = live_mover_results[0];
        assert!(
            findings.is_empty(),
            "healthy source must produce no findings, got {findings:?}"
        );
    }

    #[test]
    fn doctor_run_named_subset_with_live_mover_predicate() {
        let registry = DoctorRegistry::new();
        let live_mover = Arc::new(MemoryLiveMoverSource::divergent());
        let stranded = empty_stranded_source();
        register_doctor_checks(&registry, live_mover, stranded);

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
        let live_mover = Arc::new(MemoryLiveMoverSource::divergent());
        let stranded = empty_stranded_source();
        register_doctor_checks(&registry, live_mover, stranded);

        let err =
            doctor_run(&registry, Some(&["nope"])).expect_err("unknown check name must error");
        match err {
            djinn_core::doctor::DoctorError::UnknownCheck(name) => {
                assert!(name.contains("nope"));
            }
            other => panic!("expected UnknownCheck, got {other:?}"),
        }
    }

    #[test]
    fn doctor_run_runs_stranded_ready_check() {
        let registry = DoctorRegistry::new();
        let live_mover = Arc::new(MemoryLiveMoverSource::healthy());
        let stranded = one_stranded_source();
        register_doctor_checks(&registry, live_mover, stranded);

        let results = doctor_run(&registry, Some(&["stranded_ready"])).expect("named run succeeds");
        assert_eq!(results.len(), 1);
        let (name, findings) = &results[0];
        assert_eq!(name, "stranded_ready");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, FindingSeverity::Warn);
    }
}
