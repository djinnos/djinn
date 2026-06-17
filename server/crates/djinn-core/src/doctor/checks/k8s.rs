//! `task_run_k8s_leak` doctor check.
//!
//! A "leaked" k8s resource is a `Job`/`Pod` in the `djinn` namespace
//! (labeled with a run id) that has no corresponding *active*
//! `task_runs` row — either the row is missing entirely, or its
//! `status` is `closed` / `completed` / `terminated`. From incident
//! 4369: a leaked task-run pod monopolised a dispatch slot for hours
//! because nothing cleaned up the k8s Job after the task-run row moved
//! to a terminal state.
//!
//! The check is unusual in that it needs both database access (to look
//! up `task_runs`) and k8s API access (to enumerate Jobs/Pods). For
//! Wave 1 the [`CheckDb`] trait's `k8s_jobs` method is simulated by an
//! in-memory test double that returns a snapshot of k8s Jobs as
//! observed by an external collector — the production wiring that calls
//! the real k8s API is out of scope and is a follow-up.
//!
//! Per the doctor design, the check is a *detector* — it never mutates
//! state, never calls the k8s API in tests, and never imports
//! `supervisor_impl::pr`. The framework's `fix()` is left as the
//! default `Err(FixNotSupported)` because per-check fixers are out of
//! scope for the seed-check wave.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::doctor::{DoctorCheck, DoctorResult, Finding, FindingSeverity, ResolverSnapshot};

/// A read-only projection of the inputs the k8s-leak check needs.
///
/// The check takes a generic `CheckDb` so the fabrication tests can use
/// a pure in-memory double — the check itself never opens a real
/// database or calls the real k8s API. A future adapter (in a
/// follow-up epic) will provide an impl backed by `djinn_db::task_runs`
/// + the k8s client.
pub trait CheckDb {
    /// Every k8s `Job`/`Pod` in the `djinn` namespace labeled with a
    /// run id, as observed by an external collector. In Wave 1 this is
    /// a simulated snapshot; production wiring is a follow-up.
    fn k8s_jobs(&self) -> Vec<K8sJobListing>;

    /// Look up the `task_runs` row whose id matches `run_id`. Returns
    /// `None` when no row exists (the canonical leak: orphaned k8s Job
    /// with no database backing).
    fn task_run(&self, run_id: &str) -> Option<TaskRunRow>;
}

/// A minimal projection of a k8s `Job`/`Pod` the check consumes.
///
/// Field-for-field this mirrors the shape an external k8s collector
/// would emit. We keep a private struct so the check is testable
/// without `kube`/`k8s-openapi` as a build-time dep of `djinn-core`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct K8sJobListing {
    /// The k8s resource name (e.g. `djinn-taskrun-<runid>-abc12`).
    pub name: String,
    /// The run id extracted from the resource's labels. Maps 1:1 to
    /// `task_runs.id`.
    pub run_id: String,
    /// The k8s namespace. Expected to be `djinn` for djinn-managed
    /// resources; the check does not filter on it in Wave 1 (the
    /// collector already scopes its query) but carries it in the
    /// snapshot for the future fix path.
    pub namespace: String,
    /// When the k8s resource reached a completed state, if it has.
    /// `None` means the Job/Pod is still `Running`. ISO-8601 string to
    /// avoid a `chrono` build dep in `djinn-core`.
    pub completed_at: Option<String>,
}

/// A minimal projection of a `task_runs` row. Only the fields the
/// leak check reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRunRow {
    pub run_id: String,
    /// One of `running`, `closed`, `completed`, `terminated`, etc.
    /// Stored as a string so the check can grow new terminal states
    /// without a schema change.
    pub status: String,
}

/// Inputs the resolver consumes for one candidate k8s Job.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct K8sLeakInputs {
    pub k8s_job_name: String,
    pub namespace: String,
    pub run_id: String,
    pub completed_at: Option<String>,
    /// The `task_runs.id` that matched `run_id`, if any. `None` means
    /// no `task_runs` row exists — the canonical leak.
    pub matched_task_run_id: Option<String>,
    /// The `task_runs.status` that matched `run_id`, if any. `None`
    /// means no row; `Some("closed" | "completed" | "terminated")` means
    /// the row is terminal and the k8s Job should have been cleaned up.
    pub task_run_status: Option<String>,
}

/// Outputs the resolver returns. The fields are the *observed* truth
/// the fix path will replay `resolve()` against.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct K8sLeakOutputs {
    pub is_leak: bool,
    pub reason: K8sLeakReason,
}

/// Why the resolver concluded the k8s Job is or is not a leak.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum K8sLeakReason {
    /// No `task_runs` row matches the Job's `run_id` label. The
    /// database has no record of this k8s resource — it is a pure
    /// orphan.
    NoTaskRunRow,
    /// A `task_runs` row exists but its `status` is terminal
    /// (`closed` / `completed` / `terminated`). The row has moved on
    /// but the k8s Job was not cleaned up.
    InactiveTaskRun,
    /// The `task_runs` row is still active (`running`). The k8s Job is
    /// expected and healthy — no finding.
    Healthy,
}

/// `task_runs.status` values that indicate the run is no longer active.
/// A k8s Job still alive when its run is in one of these states is a
/// leak.
const TERMINAL_TASK_RUN_STATUSES: &[&str] = &["closed", "completed", "terminated"];

/// `true` iff `status` is a terminal `task_runs` status.
fn is_terminal_status(status: &str) -> bool {
    TERMINAL_TASK_RUN_STATUSES.contains(&status)
}

/// The shared resolver. Both `run()` and the (future) `fix()` call this
/// so the snapshot's `inputs` can reproduce the snapshot's `outputs`
/// exactly — the shared-resolver invariant from the doctor framework
/// module docs.
fn resolve_state(inputs: &K8sLeakInputs) -> K8sLeakOutputs {
    match &inputs.task_run_status {
        None => K8sLeakOutputs {
            is_leak: true,
            reason: K8sLeakReason::NoTaskRunRow,
        },
        Some(status) if is_terminal_status(status) => K8sLeakOutputs {
            is_leak: true,
            reason: K8sLeakReason::InactiveTaskRun,
        },
        Some(_) => K8sLeakOutputs {
            is_leak: false,
            reason: K8sLeakReason::Healthy,
        },
    }
}

/// `DoctorCheck` impl that flags k8s Jobs/Pods with no corresponding
/// active `task_runs` row.
///
/// The check is read-only. It does not call the k8s API in tests, does
/// not import `supervisor_impl::pr`, and does not clean up any k8s
/// resource — any k8s cleanup is the follow-up fix epic's job.
pub struct TaskRunK8sLeakCheck<D: CheckDb> {
    db: D,
}

impl<D: CheckDb> TaskRunK8sLeakCheck<D> {
    /// Construct a check bound to a specific `CheckDb` projection. In
    /// production this will be backed by a thin adapter over
    /// `djinn_db::task_runs` + the k8s client; in tests it is backed by
    /// `MemoryCheckDb`.
    pub fn new(db: D) -> Self {
        Self { db }
    }

    /// Resolve one candidate k8s Job into a [`Finding`], if it is a
    /// leak. Kept private so the snapshot's `inputs`/`outputs` fields
    /// are guaranteed to come from the *same* `resolve_state()` call
    /// the checker used.
    fn resolve(inputs: &K8sLeakInputs) -> Option<Finding> {
        let outputs = resolve_state(inputs);
        if !outputs.is_leak {
            return None;
        }

        let resolver_inputs_json = serde_json::to_value(inputs).expect("K8sLeakInputs serializes");
        let resolver_outputs_json =
            serde_json::to_value(&outputs).expect("K8sLeakOutputs serializes");
        let snapshot = ResolverSnapshot::new(
            "resolve_task_run_k8s_leak",
            resolver_inputs_json.clone(),
            resolver_outputs_json,
        );

        let evidence = json!({
            "k8s_job_name": inputs.k8s_job_name,
            "namespace": inputs.namespace,
            "run_id": inputs.run_id,
            "completed_at": inputs.completed_at,
            "matched_task_run_id": inputs.matched_task_run_id,
            "task_run_status": inputs.task_run_status,
            "reason": match outputs.reason {
                K8sLeakReason::NoTaskRunRow => "no_task_run_row",
                K8sLeakReason::InactiveTaskRun => "inactive_task_run",
                K8sLeakReason::Healthy => "healthy",
            },
        });

        let detail = match outputs.reason {
            K8sLeakReason::NoTaskRunRow => format!(
                "k8s Job '{}' (namespace '{}', run_id '{}') has no corresponding \
                 task_runs row — the database has no record of this k8s resource; \
                 it is a leaked orphan that may be consuming cluster resources or \
                 holding a dispatch slot",
                inputs.k8s_job_name, inputs.namespace, inputs.run_id,
            ),
            K8sLeakReason::InactiveTaskRun => format!(
                "k8s Job '{}' (namespace '{}', run_id '{}') is still alive but its \
                 task_runs row is in terminal status '{}' — the row has moved on but \
                 the k8s Job was not cleaned up (from incident 4369: a leaked pod \
                 monopolised a dispatch slot for hours)",
                inputs.k8s_job_name,
                inputs.namespace,
                inputs.run_id,
                inputs.task_run_status.as_deref().unwrap_or("?"),
            ),
            K8sLeakReason::Healthy => unreachable!("is_leak filters Healthy out"),
        };

        let mut finding = Finding::new(
            FindingSeverity::Critical,
            "task_run_k8s_leak",
            snapshot,
            detail,
        );
        finding = finding
            .with_entity_id("k8s_job_name", inputs.k8s_job_name.clone())
            .with_entity_id("run_id", inputs.run_id.clone())
            .with_evidence(evidence);
        if let Some(task_run_id) = inputs.matched_task_run_id.as_deref() {
            finding = finding.with_entity_id("task_run_id", task_run_id.to_owned());
        }
        Some(finding)
    }
}

impl<D: CheckDb + Send + Sync> DoctorCheck for TaskRunK8sLeakCheck<D> {
    fn name(&self) -> &'static str {
        "task_run_k8s_leak"
    }

    fn description(&self) -> &'static str {
        "Flags k8s Jobs/Pods (namespace 'djinn', labeled with a run id) that \
         have no corresponding active task_runs row — either the row is missing \
         entirely, or its status is closed/completed/terminated. From incident \
         4369. No state mutation."
    }

    fn run(&self) -> DoctorResult<Vec<Finding>> {
        let jobs = self.db.k8s_jobs();
        let mut findings = Vec::new();
        for job in jobs {
            let task_run = self.db.task_run(&job.run_id);
            let inputs = K8sLeakInputs {
                k8s_job_name: job.name.clone(),
                namespace: job.namespace.clone(),
                run_id: job.run_id.clone(),
                completed_at: job.completed_at.clone(),
                matched_task_run_id: task_run.as_ref().map(|tr| tr.run_id.clone()),
                task_run_status: task_run.as_ref().map(|tr| tr.status.clone()),
            };
            if let Some(finding) = Self::resolve(&inputs) {
                findings.push(finding);
            }
        }
        Ok(findings)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// In-memory `CheckDb` test double. The fabrication tests use it
    /// to stage specific divergence patterns and assert the check
    /// returns the expected finding shape.
    #[derive(Default)]
    struct MemoryCheckDb {
        jobs: Vec<K8sJobListing>,
        /// `run_id -> TaskRunRow` overrides. Missing entries are
        /// treated as "no task_runs row" (the canonical leak).
        task_runs: BTreeMap<String, TaskRunRow>,
    }

    impl MemoryCheckDb {
        fn with_leaked_job_no_task_run() -> Self {
            let mut db = Self::default();
            db.jobs.push(K8sJobListing {
                name: "djinn-taskrun-run-leak-abc12".to_owned(),
                run_id: "run-leak".to_owned(),
                namespace: "djinn".to_owned(),
                completed_at: None,
            });
            db
        }

        fn with_leaked_job_terminal_task_run() -> Self {
            let mut db = Self::default();
            db.jobs.push(K8sJobListing {
                name: "djinn-taskrun-run-term-def34".to_owned(),
                run_id: "run-term".to_owned(),
                namespace: "djinn".to_owned(),
                completed_at: Some("2026-01-02T03:04:05.000Z".to_owned()),
            });
            db.task_runs.insert(
                "run-term".to_owned(),
                TaskRunRow {
                    run_id: "run-term".to_owned(),
                    status: "terminated".to_owned(),
                },
            );
            db
        }

        fn with_healthy_job() -> Self {
            let mut db = Self::default();
            db.jobs.push(K8sJobListing {
                name: "djinn-taskrun-run-ok-ghi56".to_owned(),
                run_id: "run-ok".to_owned(),
                namespace: "djinn".to_owned(),
                completed_at: None,
            });
            db.task_runs.insert(
                "run-ok".to_owned(),
                TaskRunRow {
                    run_id: "run-ok".to_owned(),
                    status: "running".to_owned(),
                },
            );
            db
        }
    }

    impl CheckDb for MemoryCheckDb {
        fn k8s_jobs(&self) -> Vec<K8sJobListing> {
            self.jobs.clone()
        }
        fn task_run(&self, run_id: &str) -> Option<TaskRunRow> {
            self.task_runs.get(run_id).cloned()
        }
    }

    fn run_check(db: MemoryCheckDb) -> Vec<Finding> {
        let check = TaskRunK8sLeakCheck::new(db);
        check.run().expect("run succeeds")
    }

    // -------------------------------------------------------------------
    // Happy path
    // -------------------------------------------------------------------

    #[test]
    fn happy_path_no_finding() {
        let findings = run_check(MemoryCheckDb::default());
        assert!(
            findings.is_empty(),
            "empty job list must produce no findings, got {:?}",
            findings
        );
    }

    #[test]
    fn happy_path_active_task_run_is_not_leak() {
        let findings = run_check(MemoryCheckDb::with_healthy_job());
        assert!(
            findings.is_empty(),
            "k8s job backed by an active task_run must not be flagged, got {:?}",
            findings
        );
    }

    // -------------------------------------------------------------------
    // Divergence
    // -------------------------------------------------------------------

    #[test]
    fn divergence_finding_shape_no_task_run_row() {
        // The canonical leak: a k8s Job with no corresponding
        // task_runs row at all.
        let findings = run_check(MemoryCheckDb::with_leaked_job_no_task_run());
        assert_eq!(findings.len(), 1, "exactly one leak finding expected");
        let finding = &findings[0];
        assert_eq!(finding.severity, FindingSeverity::Critical);
        assert_eq!(finding.check_name, "task_run_k8s_leak");
        assert_eq!(
            finding.entity_ids.get("k8s_job_name").map(String::as_str),
            Some("djinn-taskrun-run-leak-abc12"),
            "entity_ids must contain the leaked k8s job name"
        );
        assert_eq!(
            finding.entity_ids.get("run_id").map(String::as_str),
            Some("run-leak"),
            "entity_ids must contain the run id"
        );
        // Evidence must surface the leak-relevant fields.
        assert_eq!(
            finding.evidence["k8s_job_name"],
            "djinn-taskrun-run-leak-abc12"
        );
        assert_eq!(finding.evidence["namespace"], "djinn");
        assert_eq!(finding.evidence["run_id"], "run-leak");
        assert_eq!(
            finding.evidence["matched_task_run_id"],
            serde_json::Value::Null
        );
        assert_eq!(finding.evidence["task_run_status"], serde_json::Value::Null);

        // Snapshot must be populated and re-runnable: feeding
        // `snapshot.inputs` back into the same resolver reproduces
        // `snapshot.outputs` exactly.
        assert_eq!(
            finding.resolver_snapshot.resolver,
            "resolve_task_run_k8s_leak"
        );
        let snapshot_inputs: K8sLeakInputs =
            serde_json::from_value(finding.resolver_snapshot.inputs.clone())
                .expect("snapshot inputs deserialize as K8sLeakInputs");
        let replay_outputs = resolve_state(&snapshot_inputs);
        let replay_outputs_json = serde_json::to_value(&replay_outputs).expect("outputs serialize");
        assert_eq!(
            replay_outputs_json, finding.resolver_snapshot.outputs,
            "resolver snapshot must be reproducible from snapshot.inputs"
        );
        assert_eq!(snapshot_inputs.k8s_job_name, "djinn-taskrun-run-leak-abc12");
        assert_eq!(snapshot_inputs.namespace, "djinn");
        assert_eq!(snapshot_inputs.run_id, "run-leak");
        assert!(snapshot_inputs.matched_task_run_id.is_none());
        assert!(snapshot_inputs.task_run_status.is_none());
    }

    #[test]
    fn divergence_finding_shape_terminal_task_run() {
        // The secondary leak: a k8s Job whose task_runs row is in a
        // terminal status (closed/completed/terminated).
        let findings = run_check(MemoryCheckDb::with_leaked_job_terminal_task_run());
        assert_eq!(findings.len(), 1, "exactly one leak finding expected");
        let finding = &findings[0];
        assert_eq!(finding.severity, FindingSeverity::Critical);
        assert_eq!(finding.check_name, "task_run_k8s_leak");
        assert_eq!(
            finding.entity_ids.get("run_id").map(String::as_str),
            Some("run-term"),
        );
        assert_eq!(
            finding.entity_ids.get("task_run_id").map(String::as_str),
            Some("run-term"),
        );

        // Evidence must surface the terminal status.
        assert_eq!(finding.evidence["task_run_status"], "terminated");

        // Snapshot must be populated and re-runnable.
        let snapshot_inputs: K8sLeakInputs =
            serde_json::from_value(finding.resolver_snapshot.inputs.clone())
                .expect("snapshot inputs deserialize as K8sLeakInputs");
        let replay_outputs = resolve_state(&snapshot_inputs);
        let replay_outputs_json = serde_json::to_value(&replay_outputs).expect("outputs serialize");
        assert_eq!(
            replay_outputs_json, finding.resolver_snapshot.outputs,
            "resolver snapshot must be reproducible from snapshot.inputs"
        );
        assert_eq!(
            snapshot_inputs.task_run_status.as_deref(),
            Some("terminated"),
        );
    }

    // -------------------------------------------------------------------
    // Resolver purity / shared-resolver invariant
    // -------------------------------------------------------------------

    #[test]
    fn resolve_is_pure() {
        let inputs = K8sLeakInputs {
            k8s_job_name: "job-x".to_owned(),
            namespace: "djinn".to_owned(),
            run_id: "run-x".to_owned(),
            completed_at: None,
            matched_task_run_id: None,
            task_run_status: None,
        };
        let a = resolve_state(&inputs);
        let b = resolve_state(&inputs);
        assert_eq!(a, b);
        assert!(a.is_leak);
        assert_eq!(a.reason, K8sLeakReason::NoTaskRunRow);
    }

    #[test]
    fn resolve_healthy_when_task_run_active() {
        let inputs = K8sLeakInputs {
            k8s_job_name: "job-y".to_owned(),
            namespace: "djinn".to_owned(),
            run_id: "run-y".to_owned(),
            completed_at: None,
            matched_task_run_id: Some("run-y".to_owned()),
            task_run_status: Some("running".to_owned()),
        };
        let out = resolve_state(&inputs);
        assert!(!out.is_leak);
        assert_eq!(out.reason, K8sLeakReason::Healthy);
    }

    #[test]
    fn resolve_leak_for_each_terminal_status() {
        for status in ["closed", "completed", "terminated"] {
            let inputs = K8sLeakInputs {
                k8s_job_name: "job-t".to_owned(),
                namespace: "djinn".to_owned(),
                run_id: "run-t".to_owned(),
                completed_at: Some("2026-01-02T03:04:05.000Z".to_owned()),
                matched_task_run_id: Some("run-t".to_owned()),
                task_run_status: Some(status.to_owned()),
            };
            let out = resolve_state(&inputs);
            assert!(out.is_leak, "terminal status '{}' must be a leak", status);
            assert_eq!(out.reason, K8sLeakReason::InactiveTaskRun);
        }
    }

    // -------------------------------------------------------------------
    // Check name / description / default fix
    // -------------------------------------------------------------------

    #[test]
    fn check_name_and_description_are_stable() {
        let check = TaskRunK8sLeakCheck::new(MemoryCheckDb::default());
        assert_eq!(check.name(), "task_run_k8s_leak");
        assert!(
            check.description().contains("k8s"),
            "description should mention k8s: got {:?}",
            check.description()
        );
    }

    #[test]
    fn check_does_not_override_fix() {
        // Per the design, T3's checks do not override `fix`; the
        // default `Err(FixNotSupported)` from the framework is
        // intentional. Asserting the trait default keeps that contract
        // explicit.
        let check = TaskRunK8sLeakCheck::new(MemoryCheckDb::default());
        let finding = Finding::new(
            FindingSeverity::Critical,
            "task_run_k8s_leak",
            ResolverSnapshot::new("resolve_task_run_k8s_leak", json!({}), json!({})),
            "synthetic",
        );
        let err = check
            .fix(&finding)
            .expect_err("default fix must return FixNotSupported");
        match err {
            crate::doctor::DoctorError::FixNotSupported { check } => {
                assert_eq!(check, "task_run_k8s_leak");
            }
            other => panic!("expected FixNotSupported, got {other:?}"),
        }
    }
}
