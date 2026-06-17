//! Doctor smoke test: registry integration + fabricated divergence fixture.
//!
//! This file implements T5's smoke test. It constructs an in-memory
//! [`MemoryFixture`] that satisfies every `CheckDb` / `ForceCloseCheckDb` /
//! `DispositionDb` trait owned by the `djinn-core` seed checks (T1–T3),
//! stages **one divergent row per check**, and drives the framework's
//! [`doctor_run`] entry point to confirm:
//!
//! 1. `doctor_run(check_names = None)` runs every registered check and
//!    returns ≥ 1 finding per check, each with a populated
//!    [`Finding::resolver_snapshot`].
//! 2. `doctor_run(check_names = Some(["zombie_running_session"]))` filters
//!    to the named check and returns exactly its finding (with the
//!    correct entity id and severity).
//! 3. `doctor_run(check_names = Some(["does_not_exist"]))` returns a
//!    structured [`DoctorError::UnknownCheck`] error.
//! 4. A healthy (no-divergence) fixture produces no findings.
//! 5. `doctor_run` calls each check's `run()` exactly once and never
//!    invokes `fix()` — a regression check that fails if a future
//!    change accidentally wires `fix` into the run path.
//! 6. Every finding returned by the divergent-fixture run has a
//!    non-empty `resolver_snapshot`.
//!
//! # Cross-crate design (decision log)
//!
//! T5 considered two options for the cross-crate registration bridge:
//!
//! - **Option 1 (chosen)**: `djinn-core::doctor` exposes a `DoctorRegistry`
//!   type with `register(Arc<dyn DoctorCheck>)` and a `doctor_run(...)`
//!   helper. `djinn-agent` registers its check via
//!   `djinn_agent::doctor::register_doctor_checks(reg, source)`. The
//!   control-plane wiring (`wo75` or its final-fix PR) calls both
//!   `djinn_core::doctor::register_default_checks(reg)` (no-op; see the
//!   docs on that function) and
//!   `djinn_agent::doctor::register_doctor_checks(reg, source)` to
//!   populate the registry before running.
//! - **Option 2 (rejected)**: a single global `inventory`-style collect.
//!   Less code, but more magic. Not adopted because the framework
//!   registry is already an explicit, hand-rolled static — keeping
//!   registration explicit preserves the `wo75`-style wired-init point
//!   and the testability of `DoctorRegistry::new` in unit tests.
//!
//! The seed-check suite spans two crates: `djinn-core::doctor::checks`
//! owns the six pure-core checks, and `djinn-agent::doctor::live_mover`
//! owns `live_mover_predicate`. The framework registry lives in
//! `djinn-core` so any caller can reach it without depending on
//! `djinn-agent`. `djinn-agent` exposes `register_doctor_checks` which
//! pushes its `live_mover_predicate` check into whichever registry the
//! caller hands in.
//!
//! Because `djinn-core` cannot depend on `djinn-agent` (the dependency
//! is one-way), this file covers only the six core checks. A parallel
//! smoke test in `djinn-agent` exercises the same `doctor_run` API
//! against the `live_mover_predicate` check via the
//! `register_doctor_checks` bridge. Together they cover all 7 seed
//! checks end-to-end.
//!
//! # State mutation
//!
//! This test is **read-only**. It does not call `fix`. It does not
//! invoke any supervisor / coordinator path. It does not touch a real
//! database or a real k8s client. The only side effects are
//! in-memory (`DoctorRegistry::register` is purely in-process).

use std::sync::Arc;

use time::OffsetDateTime;

use crate::doctor::checks::disposition::{
    DeferForeverCheck, DispositionDb, DispositionOrphanCheck, TaskDispositionRow,
};
use crate::doctor::checks::k8s::{K8sJobListing, TaskRunK8sLeakCheck, TaskRunRow};
use crate::doctor::checks::sessions::{
    ForceCloseCheckDb, ForceCloseOrphanSessionCheck, ForceCloseOrphanSessionRow, SessionRow,
    SlotRow as SessionSlotRow, ZombieRunningSessionCheck,
};
use crate::doctor::checks::slots::{SlotPoolDivergenceCheck, SlotRow as SlotsSlotRow};
use crate::doctor::{
    DoctorCheck, DoctorError, DoctorRegistry, Finding, FindingSeverity, doctor_run,
};

// ---------------------------------------------------------------------------
// Trait import shims
// ---------------------------------------------------------------------------
//
// `slots::CheckDb` and `sessions::CheckDb` are two distinct traits that
// both happen to be named `CheckDb`. We use the fully-qualified path at
// the impl site (no alias needed; the trait path makes the intent
// clear).

// ---------------------------------------------------------------------------
// Unified in-memory fixture
// ---------------------------------------------------------------------------
//
// One fixture that satisfies every `CheckDb` variant in `djinn-core`. The
// smoke test stages exactly one divergent row per check so each check's
// fabrication test (already in the check modules) maps 1:1 to one row in
// the divergent fixture. The `divergent()` builder stages every
// divergence; the `healthy()` builder stages a row that no check
// considers divergent.

#[derive(Default, Clone)]
struct MemoryFixture {
    // --- T1 — zombie_running_session (sessions::CheckDb) ---
    zombie_sessions: Vec<SessionRow>,
    zombie_slot_entries: Vec<SessionSlotRow>,
    /// `task_run_id -> is_connected` overrides.
    connected: std::collections::BTreeMap<String, bool>,
    /// `task_id -> pod_present` overrides.
    pods: std::collections::BTreeMap<String, bool>,

    // --- T1 — slot_pool_divergence (slots::CheckDb) ---
    slot_pool: Vec<SlotsSlotRow>,
    active_task_run_ids: Vec<String>,

    // --- T3 — force_close_orphan_session (sessions::ForceCloseCheckDb) ---
    force_close_orphan_sessions: Vec<ForceCloseOrphanSessionRow>,
    /// `task_id -> pod_present` overrides (re-used by ForceCloseCheckDb).
    force_close_pods: std::collections::BTreeMap<String, bool>,

    // --- T3 — task_run_k8s_leak (k8s::CheckDb) ---
    k8s_jobs: Vec<K8sJobListing>,
    /// `run_id -> TaskRunRow` overrides.
    task_runs: std::collections::BTreeMap<String, TaskRunRow>,

    // --- T2 — disposition_orphan + defer_forever (disposition::DispositionDb) ---
    disposition_candidates: Vec<TaskDispositionRow>,
}

// ----- Trait impls -----
//
// Each trait lives in a different submodule and uses a different method
// set, so the implementations do not collide.

impl crate::doctor::checks::sessions::CheckDb for MemoryFixture {
    fn zombie_running_sessions(&self) -> Vec<SessionRow> {
        self.zombie_sessions.clone()
    }
    fn slot_entries(&self) -> Vec<SessionSlotRow> {
        self.zombie_slot_entries.clone()
    }
    fn is_worker_connected(&self, task_run_id: Option<&str>) -> bool {
        task_run_id
            .and_then(|id| self.connected.get(id).copied())
            .unwrap_or(false)
    }
    fn pod_present(&self, task_id: &str) -> bool {
        self.pods.get(task_id).copied().unwrap_or(false)
    }
}

impl crate::doctor::checks::slots::CheckDb for MemoryFixture {
    fn slot_pool(&self) -> Vec<SlotsSlotRow> {
        self.slot_pool.clone()
    }
    fn active_task_run_ids(&self) -> Vec<String> {
        self.active_task_run_ids.clone()
    }
}

impl ForceCloseCheckDb for MemoryFixture {
    fn force_close_orphan_sessions(&self) -> Vec<ForceCloseOrphanSessionRow> {
        self.force_close_orphan_sessions.clone()
    }
    fn pod_present(&self, task_id: &str) -> bool {
        self.force_close_pods.get(task_id).copied().unwrap_or(false)
    }
}

impl crate::doctor::checks::k8s::CheckDb for MemoryFixture {
    fn k8s_jobs(&self) -> Vec<K8sJobListing> {
        self.k8s_jobs.clone()
    }
    fn task_run(&self, run_id: &str) -> Option<TaskRunRow> {
        self.task_runs.get(run_id).cloned()
    }
}

impl DispositionDb for MemoryFixture {
    fn disposition_candidates(&self) -> Vec<TaskDispositionRow> {
        self.disposition_candidates.clone()
    }
}

// ----- Builders -----

impl MemoryFixture {
    /// Fabricated divergent fixture: one divergent row per check.
    fn divergent() -> Self {
        let mut f = Self::default();

        // --- zombie_running_session (T1) ---
        // A running session with no pod, no slot, not connected.
        f.zombie_sessions.push(SessionRow {
            id: "sess-zombie".to_owned(),
            task_id: Some("task-zombie".to_owned()),
            agent_type: "worker".to_owned(),
            started_at: "2026-01-02T03:04:05.000Z".to_owned(),
            tokens_in: 0,
            tokens_out: 0,
            task_run_id: Some("run-zombie".to_owned()),
        });

        // --- slot_pool_divergence (T1) ---
        // Two free slots indexed under (model-dup, user-dup).
        f.slot_pool.push(SlotsSlotRow {
            slot_id: "slot-dup-a".to_owned(),
            model_id: "model-dup".to_owned(),
            user_id: "user-dup".to_owned(),
            state: "free".to_owned(),
            busy_for_task: None,
        });
        f.slot_pool.push(SlotsSlotRow {
            slot_id: "slot-dup-b".to_owned(),
            model_id: "model-dup".to_owned(),
            user_id: "user-dup".to_owned(),
            state: "free".to_owned(),
            busy_for_task: None,
        });

        // --- force_close_orphan_session (T3) ---
        f.force_close_orphan_sessions
            .push(ForceCloseOrphanSessionRow {
                session_id: "sess-force-orphan".to_owned(),
                task_id: "task-force".to_owned(),
                session_status: "running".to_owned(),
            });

        // --- task_run_k8s_leak (T3) ---
        f.k8s_jobs.push(K8sJobListing {
            name: "djinn-taskrun-run-leak-abc12".to_owned(),
            run_id: "run-leak".to_owned(),
            namespace: "djinn".to_owned(),
            completed_at: None,
        });
        // task_run(run_id) returns None → canonical leak (no row).

        // --- disposition_orphan (T2) ---
        f.disposition_candidates.push(TaskDispositionRow {
            task_id: "task-orphan".to_owned(),
            status: "in_progress".to_owned(),
            has_running_session: false,
            has_inflight_dispatch: false,
            has_open_pr: false,
            deferred_until: None,
            image_ready: false,
            no_blockers: false,
            capacity_free: false,
        });

        // --- defer_forever (T2) ---
        // A task deferred 7 hours ago whose dispatch gate is satisfied.
        let deferred_until = OffsetDateTime::now_utc() - time::Duration::hours(7);
        f.disposition_candidates.push(TaskDispositionRow {
            task_id: "task-deferred".to_owned(),
            status: "deferred".to_owned(),
            has_running_session: false,
            has_inflight_dispatch: false,
            has_open_pr: false,
            deferred_until: Some(deferred_until),
            image_ready: true,
            no_blockers: true,
            capacity_free: true,
        });

        f
    }

    /// Healthy fixture: rows that no check considers divergent.
    fn healthy() -> Self {
        let mut f = Self::default();

        // A running session with a live slot AND a connected worker.
        f.zombie_sessions.push(SessionRow {
            id: "sess-healthy".to_owned(),
            task_id: Some("task-healthy".to_owned()),
            agent_type: "worker".to_owned(),
            started_at: "2026-01-02T03:04:05.000Z".to_owned(),
            tokens_in: 12,
            tokens_out: 34,
            task_run_id: Some("run-healthy".to_owned()),
        });
        f.zombie_slot_entries.push(SessionSlotRow {
            slot_id: "slot-healthy".to_owned(),
            model_id: "model-healthy".to_owned(),
            user_id: "user-healthy".to_owned(),
            state: "busy".to_owned(),
            busy_for_task: Some("task-healthy".to_owned()),
        });
        f.connected.insert("run-healthy".to_owned(), true);
        f.pods.insert("task-healthy".to_owned(), true);

        // A unique slot per (model, user).
        f.slot_pool.push(SlotsSlotRow {
            slot_id: "slot-h-1".to_owned(),
            model_id: "model-h".to_owned(),
            user_id: "user-h".to_owned(),
            state: "busy".to_owned(),
            busy_for_task: Some("run-h-active".to_owned()),
        });
        f.active_task_run_ids.push("run-h-active".to_owned());

        // No force-close orphans.

        // A k8s Job whose `task_run` row is `running` (healthy).
        f.k8s_jobs.push(K8sJobListing {
            name: "djinn-taskrun-run-h-abc12".to_owned(),
            run_id: "run-h".to_owned(),
            namespace: "djinn".to_owned(),
            completed_at: None,
        });
        f.task_runs.insert(
            "run-h".to_owned(),
            TaskRunRow {
                run_id: "run-h".to_owned(),
                status: "running".to_owned(),
            },
        );

        // An in_progress task with a running session.
        f.disposition_candidates.push(TaskDispositionRow {
            task_id: "task-healthy-disp".to_owned(),
            status: "in_progress".to_owned(),
            has_running_session: true,
            has_inflight_dispatch: false,
            has_open_pr: false,
            deferred_until: None,
            image_ready: false,
            no_blockers: false,
            capacity_free: false,
        });

        // A non-deferred task.
        f.disposition_candidates.push(TaskDispositionRow {
            task_id: "task-not-deferred".to_owned(),
            status: "in_progress".to_owned(),
            has_running_session: true,
            has_inflight_dispatch: false,
            has_open_pr: false,
            deferred_until: None,
            image_ready: false,
            no_blockers: false,
            capacity_free: false,
        });

        f
    }
}

// ---------------------------------------------------------------------------
// Registry builder
// ---------------------------------------------------------------------------
//
// The smoke test instantiates each of the six core checks against the
// `MemoryFixture` and pushes them into a fresh `DoctorRegistry`. The
// fixture type is `MemoryFixture`; each check is a concrete
// `XxxCheck<MemoryFixture>` type, which is sized and `Send + Sync`,
// so it can be coerced to `Arc<dyn DoctorCheck>`.

fn build_registry_with_core_checks(
    fixture: MemoryFixture,
) -> (DoctorRegistry, Vec<Arc<dyn DoctorCheck>>) {
    let registry = DoctorRegistry::new();
    let mut handles: Vec<Arc<dyn DoctorCheck>> = Vec::new();

    let z = Arc::new(ZombieRunningSessionCheck::new(fixture.clone())) as Arc<dyn DoctorCheck>;
    registry.register(Arc::clone(&z));
    handles.push(z);

    let s = Arc::new(SlotPoolDivergenceCheck::new(fixture.clone())) as Arc<dyn DoctorCheck>;
    registry.register(Arc::clone(&s));
    handles.push(s);

    let d = Arc::new(DeferForeverCheck::new(Arc::new(fixture.clone()))) as Arc<dyn DoctorCheck>;
    registry.register(Arc::clone(&d));
    handles.push(d);

    let o =
        Arc::new(DispositionOrphanCheck::new(Arc::new(fixture.clone()))) as Arc<dyn DoctorCheck>;
    registry.register(Arc::clone(&o));
    handles.push(o);

    let k = Arc::new(TaskRunK8sLeakCheck::new(fixture.clone())) as Arc<dyn DoctorCheck>;
    registry.register(Arc::clone(&k));
    handles.push(k);

    let f = Arc::new(ForceCloseOrphanSessionCheck::new(fixture)) as Arc<dyn DoctorCheck>;
    registry.register(Arc::clone(&f));
    handles.push(f);

    (registry, handles)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `doctor_run(check_names = None)` runs every registered check and
/// returns at least one finding per check, each with a populated
/// `resolver_snapshot`.
#[test]
fn doctor_run_no_names_runs_all_core_checks() {
    let fixture = MemoryFixture::divergent();
    let (registry, _handles) = build_registry_with_core_checks(fixture);

    let results = doctor_run(&registry, None).expect("run succeeds");
    assert_eq!(
        results.len(),
        6,
        "expected exactly 6 (one per registered core check), got {}: {:?}",
        results.len(),
        results.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );

    // Each (check_name, findings) tuple must contain at least one
    // finding with a non-empty resolver snapshot.
    let expected_checks = [
        "zombie_running_session",
        "slot_pool_divergence",
        "defer_forever",
        "disposition_orphan",
        "task_run_k8s_leak",
        "force_close_orphan_session",
    ];
    let mut seen = std::collections::BTreeSet::new();
    for (name, findings) in &results {
        assert!(
            expected_checks.contains(&name.as_str()),
            "unexpected check name '{name}' in results"
        );
        seen.insert(name.clone());
        assert!(
            !findings.is_empty(),
            "check '{name}' produced no findings on the divergent fixture"
        );
        for finding in findings {
            assert!(
                finding.resolver_snapshot.resolver.starts_with("resolve_"),
                "check '{name}' produced finding with non-resolver resolver name: {:?}",
                finding.resolver_snapshot.resolver
            );
            assert!(
                finding.resolver_snapshot.inputs.is_object()
                    || finding.resolver_snapshot.inputs.is_array(),
                "check '{name}' produced finding with non-object/array resolver inputs: {:?}",
                finding.resolver_snapshot.inputs
            );
        }
    }
    assert_eq!(
        seen.len(),
        6,
        "expected exactly 6 distinct check names in results, got {seen:?}"
    );
}

/// `doctor_run(check_names = Some(["zombie_running_session"]))` filters to
/// only the named check and returns exactly that check's finding.
#[test]
fn doctor_run_named_subset_filters_correctly() {
    let fixture = MemoryFixture::divergent();
    let (registry, _handles) = build_registry_with_core_checks(fixture);

    let results =
        doctor_run(&registry, Some(&["zombie_running_session"])).expect("named run succeeds");

    assert_eq!(
        results.len(),
        1,
        "named subset must produce exactly one (check, findings) tuple"
    );
    let (name, findings) = &results[0];
    assert_eq!(name, "zombie_running_session");
    assert_eq!(findings.len(), 1, "zombie check should report one finding");

    let finding = &findings[0];
    assert_eq!(finding.severity, FindingSeverity::Critical);
    assert_eq!(
        finding.entity_ids.get("session_id").map(String::as_str),
        Some("sess-zombie"),
        "zombie finding must carry the divergent session id"
    );
    assert_eq!(
        finding.entity_ids.get("task_id").map(String::as_str),
        Some("task-zombie")
    );
    assert_eq!(
        finding.resolver_snapshot.resolver, "resolve_zombie_session",
        "zombie finding must use the shared resolver"
    );
}

/// `doctor_run(check_names = Some(["does_not_exist"]))` returns a
/// structured `UnknownCheck` error.
#[test]
fn doctor_run_unknown_check_name_returns_error() {
    let fixture = MemoryFixture::divergent();
    let (registry, _handles) = build_registry_with_core_checks(fixture);

    let err = doctor_run(&registry, Some(&["does_not_exist"]))
        .expect_err("unknown check name must error");
    match err {
        DoctorError::UnknownCheck(name) => {
            assert!(
                name.contains("does_not_exist"),
                "error must mention the unknown name, got {name:?}"
            );
        }
        other => panic!("expected UnknownCheck, got {other:?}"),
    }
}

/// A healthy (no-divergence) fixture produces no findings.
#[test]
fn doctor_run_healthy_fixture_produces_no_findings() {
    let fixture = MemoryFixture::healthy();
    let (registry, _handles) = build_registry_with_core_checks(fixture);

    let results = doctor_run(&registry, None).expect("run succeeds");
    assert_eq!(
        results.len(),
        6,
        "all six core checks must still be enumerated, got {}",
        results.len()
    );
    for (name, findings) in &results {
        assert!(
            findings.is_empty(),
            "healthy fixture must produce no findings for check '{name}', got {findings:?}"
        );
    }
}

/// Every finding on the divergent-fixture run has a populated
/// `resolver_snapshot` (with both inputs and outputs).
#[test]
fn all_findings_have_populated_resolver_snapshot() {
    let fixture = MemoryFixture::divergent();
    let (registry, _handles) = build_registry_with_core_checks(fixture);

    let results = doctor_run(&registry, None).expect("run succeeds");
    let total_findings: usize = results.iter().map(|(_, f)| f.len()).sum();
    assert!(
        total_findings >= 6,
        "expected at least one finding per check (≥ 6 total), got {total_findings}"
    );
    for (name, findings) in &results {
        for finding in findings {
            // inputs must be non-empty (object or array).
            let inputs_ok = finding.resolver_snapshot.inputs.is_object()
                || finding.resolver_snapshot.inputs.is_array();
            assert!(
                inputs_ok,
                "check '{name}' finding has non-object/array inputs: {:?}",
                finding.resolver_snapshot.inputs
            );
            // outputs must be non-null.
            assert!(
                !finding.resolver_snapshot.outputs.is_null(),
                "check '{name}' finding has null outputs: {:?}",
                finding.resolver_snapshot.outputs
            );
            // resolver name should start with `resolve_` (shared-resolver invariant).
            assert!(
                finding.resolver_snapshot.resolver.starts_with("resolve_"),
                "check '{name}' finding resolver must start with 'resolve_', got {:?}",
                finding.resolver_snapshot.resolver
            );
        }
    }
}

/// `doctor_run` never calls `fix()`. We assert this by wrapping a check
/// in a counter that records every `run()` and `fix()` call: `run()` must
/// be called exactly once per registered check, and `fix()` must never
/// be called by the `doctor_run` path.
#[test]
fn doctor_run_does_not_call_fix() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Wraps any `DoctorCheck` and counts `run()` / `fix()` calls.
    struct CountingCheck {
        inner: Arc<dyn DoctorCheck>,
        run_calls: Arc<AtomicUsize>,
        fix_calls: Arc<AtomicUsize>,
    }

    impl DoctorCheck for CountingCheck {
        fn name(&self) -> &'static str {
            self.inner.name()
        }
        fn description(&self) -> &'static str {
            self.inner.description()
        }
        fn run(&self) -> crate::doctor::DoctorResult<Vec<Finding>> {
            self.run_calls.fetch_add(1, Ordering::SeqCst);
            self.inner.run()
        }
        fn fix(&self, finding: &Finding) -> crate::doctor::DoctorResult<()> {
            self.fix_calls.fetch_add(1, Ordering::SeqCst);
            self.inner.fix(finding)
        }
    }

    let fixture = MemoryFixture::divergent();
    let (registry, handles) = build_registry_with_core_checks(fixture);

    let run_calls = Arc::new(AtomicUsize::new(0));
    let fix_calls = Arc::new(AtomicUsize::new(0));

    // Re-register each check under a CountingCheck wrapper. We unregister
    // the bare check first so the name collides only with the wrapper.
    for handle in &handles {
        registry.unregister(handle.name());
    }
    for handle in &handles {
        let wrapped: Arc<dyn DoctorCheck> = Arc::new(CountingCheck {
            inner: Arc::clone(handle),
            run_calls: Arc::clone(&run_calls),
            fix_calls: Arc::clone(&fix_calls),
        });
        registry.register(wrapped);
    }

    let _results = doctor_run(&registry, None).expect("run succeeds");
    assert_eq!(
        run_calls.load(Ordering::SeqCst),
        6,
        "doctor_run must call run() once per registered check (6)"
    );
    assert_eq!(
        fix_calls.load(Ordering::SeqCst),
        0,
        "doctor_run must NEVER call fix() on any check"
    );
}

/// `doctor_run` with an empty `Some(&[])` is treated as `None` (run all).
#[test]
fn doctor_run_empty_some_runs_all() {
    let fixture = MemoryFixture::divergent();
    let (registry, _handles) = build_registry_with_core_checks(fixture);

    let results = doctor_run(&registry, Some(&[])).expect("empty Some is treated as None");
    assert_eq!(
        results.len(),
        6,
        "empty Some(&[]) must run all 6 registered checks"
    );
}

/// Mixed subset: `Some(["zombie_running_session", "defer_forever"])`
/// returns both, in registration order.
#[test]
fn doctor_run_mixed_subset() {
    let fixture = MemoryFixture::divergent();
    let (registry, _handles) = build_registry_with_core_checks(fixture);

    let results = doctor_run(
        &registry,
        Some(&["defer_forever", "zombie_running_session"]),
    )
    .expect("mixed subset run succeeds");
    assert_eq!(results.len(), 2);
    let names: Vec<&str> = results.iter().map(|(n, _)| n.as_str()).collect();
    // Registry enumeration is alphabetical (BTreeMap).
    assert_eq!(
        names,
        vec!["defer_forever", "zombie_running_session"],
        "mixed subset must return the named checks (alphabetical)"
    );
}
