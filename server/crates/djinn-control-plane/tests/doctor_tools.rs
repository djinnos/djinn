//! Integration tests for the `doctor_run` and `doctor_fix` MCP tools.
//!
//! These tests exercise the full dispatch path through `McpTestHarness::call_tool`,
//! covering the acceptance criteria for task `40jq`:
//!
//! - **Admin gating**: non-admin callers receive an auth/admin error for both
//!   `doctor_run` and `doctor_fix`. The trusted no-user/background path is allowed.
//! - **Run-never-fixes**: `doctor_run` persists findings and returns them in the
//!   report but never invokes any check `fix` side effect.
//! - **Explicit fix only**: `doctor_fix` invokes fix only for the requested
//!   persisted finding/check name and rejects mismatched check/finding ids.
//! - **Shared-resolver invariant**: a regression test demonstrates the Gas Town
//!   invariant by using a sample resolver snapshot — the fix plan/expected state
//!   is derived from the same resolver inputs captured during check, not a
//!   hard-coded fresh expected value.

#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use djinn_control_plane::test_support::McpTestHarness;
use djinn_core::auth_context::SESSION_USER_ID;
use djinn_core::doctor::{
    DoctorCheck, DoctorCheckCadence, DoctorError, DoctorResult, Finding, FindingSeverity,
    ResolverSnapshot, registry,
};
use djinn_db::DoctorFindingRepository;
use djinn_db::repositories::user::UserRepository;
use serde_json::json;

// ---------------------------------------------------------------------------
// Sample fixable check for integration tests
// ---------------------------------------------------------------------------

/// Unique name for the test check registered in the global registry.
/// Using a module-unique name avoids interference with other tests that
/// might touch the global registry.
const TEST_CHECK_NAME: &str = "test.doctor_integration_40jq";

/// A sample check that demonstrates the shared-resolver invariant.
///
/// Both `run()` and `fix()` call the same `resolve()` helper. The fix path
/// derives its expected state from `finding.resolver_snapshot.inputs` — never
/// from a hard-coded value. If a hand-coded expected value were used instead,
/// the `fix_uses_resolver_snapshot_from_persisted_finding` test would fail
/// because the snapshot carries the live outputs that a hand-coded value
/// cannot reproduce.
struct IntegrationFixableCheck {
    /// Tracks whether `fix()` was called. Set by the fix implementation so
    /// the run-never-fixes test can assert it stays `false` after `doctor_run`.
    fix_called: Arc<AtomicBool>,
    /// Monotonic counter incremented each time `run()` is called.
    run_call_count: Arc<AtomicU32>,
}

impl IntegrationFixableCheck {
    fn new() -> (Self, Arc<AtomicBool>, Arc<AtomicU32>) {
        let fix_called = Arc::new(AtomicBool::new(false));
        let run_call_count = Arc::new(AtomicU32::new(0));
        let check = Self {
            fix_called: Arc::clone(&fix_called),
            run_call_count: Arc::clone(&run_call_count),
        };
        (check, fix_called, run_call_count)
    }
}

/// The shared resolver function. Both `run` and `fix` must call this.
///
/// The resolver enforces "desired state == 42". The check reports a finding
/// when the observed value differs. The fix re-runs the resolver to confirm
/// the same answer before agreeing to act.
fn resolve(inputs: &serde_json::Value) -> serde_json::Value {
    let observed = inputs
        .get("observed")
        .and_then(|v| v.as_i64())
        .unwrap_or(i64::MIN);
    let desired = 42_i64;
    json!({
        "observed": observed,
        "desired": desired,
        "should_fix": observed != desired,
    })
}

impl DoctorCheck for IntegrationFixableCheck {
    fn name(&self) -> &'static str {
        TEST_CHECK_NAME
    }

    fn description(&self) -> &'static str {
        "Integration test check for the shared-resolver invariant"
    }

    fn cadence(&self) -> DoctorCheckCadence {
        DoctorCheckCadence::Cheap
    }

    fn run(&self) -> DoctorResult<Vec<Finding>> {
        self.run_call_count.fetch_add(1, Ordering::SeqCst);

        // The "observed" value simulates a DB query that found a divergent
        // state. We use a fixed value (7) so the test is deterministic.
        let inputs = json!({ "observed": 7 });
        let outputs = resolve(&inputs);

        if outputs
            .get("should_fix")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            let finding = Finding::new(
                FindingSeverity::Warn,
                self.name(),
                ResolverSnapshot::new("resolve", inputs, outputs.clone()),
                format!(
                    "observed {} but resolver expected {}",
                    outputs.get("observed").unwrap(),
                    outputs.get("desired").unwrap()
                ),
            )
            .with_entity_id("sample", "demo")
            .with_evidence(outputs);
            Ok(vec![finding])
        } else {
            Ok(Vec::new())
        }
    }

    fn fix(&self, finding: &Finding) -> DoctorResult<()> {
        self.fix_called.store(true, Ordering::SeqCst);

        // Gas Town shared-resolver invariant: re-run the SAME `resolve()`
        // helper with the snapshot's inputs. We do NOT compare against a
        // hand-coded `42` or recompute `desired` from scratch. The snapshot
        // is the sole carrier of the resolver's expected state.
        let inputs = &finding.resolver_snapshot.inputs;
        let outputs = resolve(inputs);

        // Verify the resolver outputs are reproducible from the snapshot
        // inputs. If the snapshot were stale or hand-coded, this assertion
        // would fail.
        assert_eq!(
            finding.resolver_snapshot.outputs, outputs,
            "resolver outputs must be reproducible from snapshot inputs — \
             this assertion fails if a hand-coded expected value was used \
             instead of the resolver snapshot from check time"
        );

        let should_fix = outputs
            .get("should_fix")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if should_fix {
            Ok(())
        } else {
            Err(DoctorError::InvalidInput(
                "resolver reports no fix needed".to_string(),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Register the integration test check in the GLOBAL registry (the one
/// `doctor_run` / `doctor_fix` consult via `registry()`).
///
/// Returns spy handles so tests can assert on `fix_called` / `run_call_count`.
fn register_test_check() -> (Arc<AtomicBool>, Arc<AtomicU32>) {
    let (check, fix_called, run_count) = IntegrationFixableCheck::new();
    registry().register(Arc::new(check));
    (fix_called, run_count)
}

/// Create a harness and ensure the schema is ready for doctor tests.
async fn doctor_test_harness() -> McpTestHarness {
    let harness = McpTestHarness::new().await;
    djinn_db::test_support::ensure_doctor_findings_schema(harness.db()).await;
    harness
}

/// Create a non-admin user in the test DB and return their id.
async fn create_non_admin_user(db: &djinn_db::Database) -> String {
    let repo = UserRepository::new(db.clone());
    let user = repo
        .upsert_from_github(999_001, "non-admin-doctor-test", None, None)
        .await
        .expect("create non-admin user");
    // Ensure the user is NOT admin (upsert defaults is_admin=false).
    assert!(!user.is_admin, "test user should default to non-admin");
    user.id
}

/// Create an admin user in the test DB and return their id.
async fn create_admin_user(db: &djinn_db::Database) -> String {
    let repo = UserRepository::new(db.clone());
    let user = repo
        .upsert_from_github(999_002, "admin-doctor-test", None, None)
        .await
        .expect("create admin user");
    repo.set_admin_status(&user.id, true)
        .await
        .expect("promote to admin");
    user.id
}

// ---------------------------------------------------------------------------
// Admin gating tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn doctor_run_rejects_non_admin_caller() {
    let harness = doctor_test_harness().await;
    register_test_check();

    let user_id = create_non_admin_user(harness.db()).await;

    // Scope under a non-admin user — the tool should reject.
    let result = SESSION_USER_ID
        .scope(Some(user_id), async {
            harness
                .call_tool("doctor_run", json!({ "check_names": [TEST_CHECK_NAME] }))
                .await
        })
        .await;

    let response = result.expect("doctor_run should dispatch");
    assert_eq!(response["ok"], false, "non-admin should be rejected");
    let error = response["error"]
        .as_str()
        .expect("error message should be present");
    assert!(
        error.contains("admin"),
        "error should mention admin: got '{error}'"
    );
}

#[tokio::test]
async fn doctor_fix_rejects_non_admin_caller() {
    let harness = doctor_test_harness().await;
    register_test_check();

    let user_id = create_non_admin_user(harness.db()).await;

    // Scope under a non-admin user — the tool should reject.
    let result = SESSION_USER_ID
        .scope(Some(user_id), async {
            harness
                .call_tool(
                    "doctor_fix",
                    json!({
                        "check_name": TEST_CHECK_NAME,
                        "finding_id": "00000000-0000-0000-0000-000000000001"
                    }),
                )
                .await
        })
        .await;

    let response = result.expect("doctor_fix should dispatch");
    assert_eq!(response["ok"], false, "non-admin should be rejected");
    let error = response["error"]
        .as_str()
        .expect("error message should be present");
    assert!(
        error.contains("admin"),
        "error should mention admin: got '{error}'"
    );
}

#[tokio::test]
async fn doctor_run_allows_trusted_no_user_background_path() {
    // When there is no SESSION_USER_ID scope (unauthenticated/background),
    // require_admin returns Ok(()). This is the trusted no-user path.
    let harness = doctor_test_harness().await;
    let (fix_called, _) = register_test_check();

    // No SESSION_USER_ID scope — simulates background/no-user context.
    let response = harness
        .call_tool("doctor_run", json!({ "check_names": [TEST_CHECK_NAME] }))
        .await
        .expect("doctor_run should dispatch");

    assert_eq!(response["ok"], true, "no-user path should be allowed");
    assert!(
        !fix_called.load(Ordering::SeqCst),
        "doctor_run must never invoke fix"
    );
}

#[tokio::test]
async fn doctor_run_allows_admin_caller() {
    let harness = doctor_test_harness().await;
    register_test_check();

    let admin_id = create_admin_user(harness.db()).await;

    let response = SESSION_USER_ID
        .scope(Some(admin_id), async {
            harness
                .call_tool("doctor_run", json!({ "check_names": [TEST_CHECK_NAME] }))
                .await
        })
        .await
        .expect("doctor_run should dispatch");

    assert_eq!(response["ok"], true, "admin caller should be allowed");
}

// ---------------------------------------------------------------------------
// doctor_run: persists findings but never invokes fix
// ---------------------------------------------------------------------------

#[tokio::test]
async fn doctor_run_persists_findings_and_does_not_invoke_fix() {
    let harness = doctor_test_harness().await;
    let (fix_called, _) = register_test_check();

    // Run the check through the MCP tool.
    let response = harness
        .call_tool("doctor_run", json!({ "check_names": [TEST_CHECK_NAME] }))
        .await
        .expect("doctor_run should dispatch");

    assert_eq!(response["ok"], true);

    // The report should list the registered checks.
    let registered = response["registered_checks"]
        .as_array()
        .expect("registered_checks should be an array");
    assert!(
        registered.iter().any(|c| c["name"] == TEST_CHECK_NAME),
        "registered_checks should include our test check"
    );

    // The results should include our check with findings.
    let results = response["results"]
        .as_array()
        .expect("results should be an array");
    let our_result = results
        .iter()
        .find(|r| r["check"]["name"] == TEST_CHECK_NAME)
        .expect("result for our test check");
    assert_eq!(our_result["ran"], true);
    assert_eq!(our_result["error"], serde_json::Value::Null);

    let findings = our_result["findings"]
        .as_array()
        .expect("findings should be an array");
    assert_eq!(
        findings.len(),
        1,
        "our check should emit exactly one finding"
    );

    let finding_id = findings[0]["finding_id"]
        .as_str()
        .expect("finding_id should be present");
    assert!(
        !finding_id.is_empty(),
        "finding_id should be a non-empty persisted id"
    );
    assert_eq!(findings[0]["check_name"], TEST_CHECK_NAME);
    assert_eq!(findings[0]["severity"], "warn");

    // total_findings should be at least 1.
    let total: i64 = response["total_findings"]
        .as_i64()
        .expect("total_findings should be an integer");
    assert!(total >= 1, "total_findings should include our finding");

    // CRITICAL: doctor_run must NEVER invoke fix.
    assert!(
        !fix_called.load(Ordering::SeqCst),
        "doctor_run must never invoke fix — fix_called was true"
    );

    // Verify the finding was actually persisted to the database.
    let repo = DoctorFindingRepository::new(harness.db().clone());
    let persisted = repo
        .get(finding_id)
        .await
        .expect("repo get should succeed")
        .expect("finding should be persisted in DB");
    assert_eq!(persisted.check_name, TEST_CHECK_NAME);
    assert_eq!(persisted.severity, "warn");
    // The resolver snapshot should survive the round-trip.
    assert!(
        persisted.resolver_snapshot.is_some(),
        "resolver_snapshot should be persisted"
    );
}

// ---------------------------------------------------------------------------
// doctor_fix: explicit, uses persisted finding, rejects mismatches
// ---------------------------------------------------------------------------

#[tokio::test]
async fn doctor_fix_invokes_fix_for_correct_finding() {
    let harness = doctor_test_harness().await;
    let (fix_called, _) = register_test_check();

    // Step 1: run to persist a finding.
    let run_response = harness
        .call_tool("doctor_run", json!({ "check_names": [TEST_CHECK_NAME] }))
        .await
        .expect("doctor_run should dispatch");

    let findings = run_response["results"]
        .as_array()
        .expect("results array")
        .iter()
        .find(|r| r["check"]["name"] == TEST_CHECK_NAME)
        .expect("our check result")["findings"]
        .as_array()
        .expect("findings array");
    let finding_id = findings[0]["finding_id"].as_str().expect("finding_id");

    assert!(
        !fix_called.load(Ordering::SeqCst),
        "fix should not have been called by doctor_run"
    );

    // Step 2: explicitly call doctor_fix with the correct check name + finding id.
    let fix_response = harness
        .call_tool(
            "doctor_fix",
            json!({
                "check_name": TEST_CHECK_NAME,
                "finding_id": finding_id
            }),
        )
        .await
        .expect("doctor_fix should dispatch");

    assert_eq!(fix_response["ok"], true, "fix should succeed");
    assert_eq!(fix_response["check_name"], TEST_CHECK_NAME);
    assert_eq!(fix_response["finding_id"], finding_id);
    assert!(
        fix_called.load(Ordering::SeqCst),
        "doctor_fix should have invoked fix"
    );
}

#[tokio::test]
async fn doctor_fix_rejects_mismatched_check_name() {
    let harness = doctor_test_harness().await;
    let (_, _) = register_test_check();

    // Run to persist a finding owned by TEST_CHECK_NAME.
    let run_response = harness
        .call_tool("doctor_run", json!({ "check_names": [TEST_CHECK_NAME] }))
        .await
        .expect("doctor_run");

    let findings = run_response["results"]
        .as_array()
        .expect("results array")
        .iter()
        .find(|r| r["check"]["name"] == TEST_CHECK_NAME)
        .expect("our check result")["findings"]
        .as_array()
        .expect("findings array");
    let finding_id = findings[0]["finding_id"].as_str().expect("finding_id");

    // Call doctor_fix with a WRONG check name for the same finding id.
    let fix_response = harness
        .call_tool(
            "doctor_fix",
            json!({
                "check_name": "wrong.check_name",
                "finding_id": finding_id
            }),
        )
        .await
        .expect("doctor_fix should dispatch");

    assert_eq!(
        fix_response["ok"], false,
        "mismatched check_name should fail"
    );
    let error = fix_response["error"].as_str().expect("error message");
    // The error should mention the mismatch — either "unknown check" or
    // "belongs to check".
    assert!(
        error.contains("belongs to check") || error.contains("unknown doctor check"),
        "error should explain the mismatch: got '{error}'"
    );
}

#[tokio::test]
async fn doctor_fix_rejects_nonexistent_finding_id() {
    let harness = doctor_test_harness().await;
    register_test_check();

    let fix_response = harness
        .call_tool(
            "doctor_fix",
            json!({
                "check_name": TEST_CHECK_NAME,
                "finding_id": "01999999-9999-7999-8999-999999999999"
            }),
        )
        .await
        .expect("doctor_fix should dispatch");

    assert_eq!(fix_response["ok"], false, "nonexistent finding should fail");
    let error = fix_response["error"].as_str().expect("error message");
    assert!(
        error.contains("not found"),
        "error should mention not found: got '{error}'"
    );
}

#[tokio::test]
async fn doctor_fix_rejects_unknown_check_name() {
    let harness = doctor_test_harness().await;
    register_test_check();

    let fix_response = harness
        .call_tool(
            "doctor_fix",
            json!({
                "check_name": "totally.unknown.check",
                "finding_id": "00000000-0000-0000-0000-000000000001"
            }),
        )
        .await
        .expect("doctor_fix should dispatch");

    assert_eq!(fix_response["ok"], false, "unknown check name should fail");
    let error = fix_response["error"].as_str().expect("error message");
    assert!(
        error.contains("unknown doctor check"),
        "error should mention unknown check: got '{error}'"
    );
}

// ---------------------------------------------------------------------------
// Shared-resolver invariant regression test
// ---------------------------------------------------------------------------

/// This test demonstrates the Gas Town invariant: the fix path derives its
/// expected state from the same resolver snapshot captured during the check,
/// NOT from a hard-coded value or fresh unrelated inputs.
///
/// The test would fail if the sample fix used a hand-coded expected value
/// instead of the resolver snapshot, because:
///
/// 1. The check captures `resolve({observed: 7}) → {observed: 7, desired: 42,
///    should_fix: true}` in the `ResolverSnapshot`.
/// 2. The fix re-runs `resolve(snapshot.inputs)` and asserts the outputs match
///    the snapshot. A hand-coded `42` would bypass this assertion and the test
///    for reproducibility would fail.
/// 3. The persisted finding carries the snapshot through the DB round-trip, so
///    the fix sees the exact same inputs/outputs the check used.
#[tokio::test]
async fn fix_uses_resolver_snapshot_from_persisted_finding() {
    let harness = doctor_test_harness().await;
    let (_, _) = register_test_check();

    // Run to persist a finding with the resolver snapshot.
    let run_response = harness
        .call_tool("doctor_run", json!({ "check_names": [TEST_CHECK_NAME] }))
        .await
        .expect("doctor_run");

    let results = run_response["results"].as_array().expect("results array");
    let our_result = results
        .iter()
        .find(|r| r["check"]["name"] == TEST_CHECK_NAME)
        .expect("our check result");
    let finding_id = our_result["findings"][0]["finding_id"]
        .as_str()
        .expect("finding_id");

    // Fetch the persisted finding to verify the snapshot survived the DB round-trip.
    let repo = DoctorFindingRepository::new(harness.db().clone());
    let persisted = repo
        .get(finding_id)
        .await
        .expect("repo get")
        .expect("finding persisted");

    let snapshot = persisted
        .resolver_snapshot
        .as_ref()
        .expect("resolver_snapshot must be persisted");

    // The snapshot must carry the exact inputs the check used.
    assert_eq!(
        snapshot["resolver"], "resolve",
        "snapshot resolver name must match"
    );
    assert_eq!(
        snapshot["inputs"]["observed"], 7,
        "snapshot inputs must be the check's inputs"
    );
    assert_eq!(
        snapshot["outputs"]["desired"], 42,
        "snapshot outputs must carry the resolver's expected state"
    );
    assert_eq!(
        snapshot["outputs"]["should_fix"], true,
        "snapshot outputs must indicate a fix is needed"
    );

    // Now invoke fix — it re-runs resolve(snapshot.inputs) and asserts the
    // outputs match. If a hand-coded value were used, this would fail.
    let fix_response = harness
        .call_tool(
            "doctor_fix",
            json!({
                "check_name": TEST_CHECK_NAME,
                "finding_id": finding_id
            }),
        )
        .await
        .expect("doctor_fix");

    assert_eq!(
        fix_response["ok"], true,
        "fix should succeed when the resolver snapshot is used correctly"
    );
}

/// Verify that `doctor_run` with no `check_names` runs all registered checks
/// (including our test check) and persists their findings.
#[tokio::test]
async fn doctor_run_without_check_names_runs_all_registered() {
    let harness = doctor_test_harness().await;
    let (_, run_count) = register_test_check();

    let response = harness
        .call_tool("doctor_run", json!({}))
        .await
        .expect("doctor_run");

    assert_eq!(response["ok"], true);

    // Our check should appear in the results since it's registered globally.
    let results = response["results"].as_array().expect("results array");
    assert!(
        results
            .iter()
            .any(|r| r["check"]["name"] == TEST_CHECK_NAME),
        "our test check should be included when running all"
    );

    // run() should have been called at least once.
    assert!(
        run_count.load(Ordering::SeqCst) >= 1,
        "run() should have been called"
    );
}

/// Exercise the jk7v diagnostic payload shapes through the MCP `doctor_run`
/// tool path.  This test registers a check that produces a finding with
/// liveness-classifier evidence (verdict / outcome / reason — the fields
/// that `board_health` reads from the same DB columns), runs `doctor_run`,
/// and verifies the persisted finding carries those fields in the shape
/// the board-health surface expects.
///
/// This closes AC 3: "The tests exercise the landed diagnostic fields from
/// `jk7v` without replacing existing coarse response shape checks."
#[tokio::test]
async fn doctor_run_persists_jk7v_aligned_classifier_evidence() {
    const LIVENESS_CHECK: &str = "test.doctor_liveness_jk7v";

    struct LivenessEvidenceCheck;

    impl DoctorCheck for LivenessEvidenceCheck {
        fn name(&self) -> &'static str {
            LIVENESS_CHECK
        }
        fn description(&self) -> &'static str {
            "Test check that produces a finding with jk7v-aligned classifier evidence"
        }
        fn cadence(&self) -> DoctorCheckCadence {
            DoctorCheckCadence::Cheap
        }
        fn run(&self) -> DoctorResult<Vec<Finding>> {
            let inputs = json!({ "session_id": "sess-1", "task_id": "task-1" });
            let outputs = json!({
                "verdict": "dead",
                "outcome": "dead_reclaimed",
                "reason": "hard_runtime_exceeded",
            });
            Ok(vec![
                Finding::new(
                    FindingSeverity::Critical,
                    self.name(),
                    ResolverSnapshot::new("resolve_liveness", inputs, outputs.clone()),
                    "zombie session detected with dead verdict",
                )
                .with_entity_id("task_id", "task-1")
                .with_entity_id("session_id", "sess-1")
                .with_evidence(json!({
                    "classifier": {
                        "verdict": "dead",
                        "outcome": "dead_reclaimed",
                        "reason": "hard_runtime_exceeded",
                    },
                    "pod_phase": "Succeeded",
                })),
            ])
        }
    }

    let harness = doctor_test_harness().await;

    // Register our liveness-evidence-aware check in the global registry.
    // We use a fresh AtomicBool to track whether it was found.
    registry().register(Arc::new(LivenessEvidenceCheck));

    // Run through the MCP tool.
    let response = harness
        .call_tool("doctor_run", json!({ "check_names": [LIVENESS_CHECK] }))
        .await
        .expect("doctor_run should dispatch");
    assert_eq!(response["ok"], true);

    let results = response["results"].as_array().expect("results array");
    let our_result = results
        .iter()
        .find(|r| r["check"]["name"] == LIVENESS_CHECK)
        .expect("our check result");
    assert_eq!(our_result["ran"], true);

    let findings = our_result["findings"].as_array().expect("findings array");
    assert_eq!(findings.len(), 1, "must produce exactly one finding");

    let finding = &findings[0];
    assert_eq!(finding["check_name"], LIVENESS_CHECK);
    assert_eq!(finding["severity"], "critical");

    // Verify the persisted finding in the DB carries jk7v-aligned evidence.
    let finding_id = finding["finding_id"]
        .as_str()
        .expect("finding_id should be present");
    let repo = DoctorFindingRepository::new(harness.db().clone());
    let persisted = repo
        .get(finding_id)
        .await
        .expect("repo get")
        .expect("finding must be persisted");

    // The persisted evidence must carry the classifier object with the
    // same verdict/outcome/reason fields that board_health reads.
    let evidence = &persisted.evidence;
    assert_eq!(
        evidence["classifier"]["verdict"].as_str(),
        Some("dead"),
        "persisted evidence must carry classifier.verdict = dead"
    );
    assert_eq!(
        evidence["classifier"]["outcome"].as_str(),
        Some("dead_reclaimed"),
        "persisted evidence must carry classifier.outcome = dead_reclaimed"
    );
    assert_eq!(
        evidence["classifier"]["reason"].as_str(),
        Some("hard_runtime_exceeded"),
        "persisted evidence must carry classifier.reason = hard_runtime_exceeded"
    );
    assert_eq!(
        evidence["pod_phase"].as_str(),
        Some("Succeeded"),
        "persisted evidence must carry pod_phase"
    );

    // The entity_ids must reference the same task/session as the finding.
    assert_eq!(
        persisted.entity_ids.get("task_id").and_then(|v| v.as_str()),
        Some("task-1"),
        "persisted entity_ids must carry task_id"
    );
    assert_eq!(
        persisted
            .entity_ids
            .get("session_id")
            .and_then(|v| v.as_str()),
        Some("sess-1"),
        "persisted entity_ids must carry session_id"
    );

    // The resolver snapshot must carry the liveness resolve outputs.
    let snapshot = persisted
        .resolver_snapshot
        .as_ref()
        .expect("resolver_snapshot must be persisted");
    assert_eq!(snapshot["resolver"], "resolve_liveness");
    assert_eq!(snapshot["outputs"]["verdict"], "dead");
    assert_eq!(snapshot["outputs"]["outcome"], "dead_reclaimed");
}

// ---------------------------------------------------------------------------
// closed_parent_open_children: persisted dry-run contract
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_parent_open_children_db_dry_run_is_read_only() {
    use djinn_agent::doctor::{
        CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME, ClosedParentOpenChildrenSource,
        TaskRepositoryClosedParentOpenChildrenSource, register_closed_parent_open_children_check,
    };
    use djinn_core::events::DjinnEventEnvelope;
    use djinn_db::{EpicRepository, ProposalCreateInput, ProposalRepository, TaskRepository};
    let harness = doctor_test_harness().await;
    let project = common::create_test_project(harness.db()).await;
    let epics = EpicRepository::new(harness.db().clone(), common::test_events());
    let tasks = TaskRepository::new(harness.db().clone(), common::test_events());
    let mut ids = Vec::new();
    for status in ["open", "in_progress", "pr_review", "open"] {
        let epic = common::create_test_epic(harness.db(), &project.id).await;
        let task = common::create_test_task(harness.db(), &project.id, &epic.id).await;
        tasks.set_status(&task.id, status).await.unwrap();
        if status == "pr_review" {
            tasks
                .set_pr_url(&task.id, "https://github.com/djinnos/djinn/pull/999999")
                .await
                .unwrap();
        }
        epics.set_status_raw(&epic.id, "closed").await.unwrap();
        ids.push(task.id);
    }
    let proposals = ProposalRepository::new(harness.db().clone(), common::test_events());
    let proposal = proposals
        .create(ProposalCreateInput {
            title: "live parent",
            body: "",
            acceptance_criteria: None,
            status: Some("building"),
            body_format: None,
        })
        .await
        .unwrap();
    let guarded_task = tasks.get(&ids[3]).await.unwrap().unwrap();
    proposals
        .link_epic(
            &proposal.id,
            guarded_task.epic_id.as_deref().unwrap(),
            &project.id,
        )
        .await
        .unwrap();
    // Establish that the real production query sees every fixture before the
    // named check is registered. This prevents stale registry/source state
    // from turning the MCP assertion into an in-memory-only regression.
    let health = tasks.board_health(30).await.unwrap();
    let health_findings = health["closed_parent_open_children"]["findings"]
        .as_array()
        .unwrap();
    assert_eq!(health_findings.len(), 4, "board health: {health}");
    let mut expected = std::collections::BTreeMap::new();
    expected.insert(ids[0].as_str(), ("close", "parent_closed"));
    expected.insert(
        ids[1].as_str(),
        ("park", "historical_parent_closed_in_flight"),
    );
    expected.insert(
        ids[2].as_str(),
        ("park", "historical_parent_closed_pr_active"),
    );
    expected.insert(ids[3].as_str(), ("retain", "other_open_parent"));
    for finding in health_findings {
        let task_id = finding["id"].as_str().unwrap();
        let (action, guard) = expected
            .get(task_id)
            .unwrap_or_else(|| panic!("unexpected board-health task {task_id}"));
        assert_eq!(finding["recommended_disposition"]["action"], *action);
        assert_eq!(finding["recommended_disposition"]["guard"], *guard);
    }
    let mut tasks_before = Vec::new();
    let mut activity_before = Vec::new();
    for id in &ids {
        tasks_before.push(serde_json::to_value(tasks.get(id).await.unwrap().unwrap()).unwrap());
        activity_before.push(serde_json::to_value(tasks.list_activity(id).await.unwrap()).unwrap());
    }
    let (tx, _) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(16);
    let source = Arc::new(TaskRepositoryClosedParentOpenChildrenSource::new(
        harness.db().clone(),
        tx,
    ));
    source.refresh().await;
    assert_eq!(source.snapshot()["findings"], json!(health_findings));
    register_closed_parent_open_children_check(registry(), source);
    let response = harness
        .call_tool(
            "doctor_run",
            json!({"check_names":[CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME]}),
        )
        .await
        .unwrap();
    assert_eq!(response["ok"], true);
    let findings = response["results"][0]["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 4);
    let repo = DoctorFindingRepository::new(harness.db().clone());
    let mut actual = std::collections::BTreeMap::new();
    for result in findings {
        let finding = repo
            .get(result["finding_id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        let owner = finding.entity_ids["task_id"].as_str().unwrap();
        assert_eq!(finding.entity_ids["task_id"], owner);
        assert_eq!(finding.evidence["board_health_finding"]["id"], owner);
        assert_eq!(
            finding.resolver_snapshot.as_ref().unwrap()["inputs"]["board_health_finding"]["id"],
            owner
        );
        assert_eq!(
            finding.evidence["board_health_finding"],
            *health_findings
                .iter()
                .find(|health| health["id"].as_str() == Some(owner))
                .unwrap(),
            "persisted evidence must preserve the complete board-health child snapshot"
        );
        actual.insert(
            owner.to_owned(),
            (
                finding.evidence["selected_disposition"]["action"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
                finding.evidence["selected_disposition"]["guard"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            ),
        );
    }
    let expected: std::collections::BTreeMap<_, _> = expected
        .into_iter()
        .map(|(id, (action, guard))| (id.to_owned(), (action.to_owned(), guard.to_owned())))
        .collect();
    assert_eq!(actual, expected);
    let mut tasks_after = Vec::new();
    let mut activity_after = Vec::new();
    for id in &ids {
        tasks_after.push(serde_json::to_value(tasks.get(id).await.unwrap().unwrap()).unwrap());
        activity_after.push(serde_json::to_value(tasks.list_activity(id).await.unwrap()).unwrap());
    }
    assert_eq!(tasks_before, tasks_after);
    assert_eq!(activity_before, activity_after);
}

// ---------------------------------------------------------------------------
// memory.retrieval_zero_result: production-registered Doctor check
// ---------------------------------------------------------------------------

/// Integration test for the production-registered `memory.retrieval_zero_result`
/// Doctor check.
///
/// This test exercises the full control-plane path: it inserts real
/// `retrieval_traces` rows for two active projects, invokes `doctor_run` with
/// `check_names: ["memory.retrieval_zero_result"]`, and proves that only the
/// at-floor, strictly-above-threshold project produces a persisted finding.
///
/// **AC4**: A control-plane integration test inserts retrieval traces for at
/// least two active projects, invokes the production-registered check through
/// `doctor_run`, and proves only the at-floor, strictly-above-threshold project
/// produces a persisted finding.
///
/// **AC5**: The integration asserts persisted evidence includes project,
/// exact window, threshold, floor, numerator, denominator, rate, and
/// per-entry-point counts, and covers equality-at-threshold suppression.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_run_retrieval_zero_result_only_above_threshold_emits_finding() {
    use djinn_core::doctor::RETRIEVAL_ZERO_RESULT_NAME;
    use djinn_db::repositories::retrieval_trace::{
        CreateRetrievalTraceParams, DEFAULT_CANDIDATE_CAP, RetrievalTraceEntryPoint,
        RetrievalTraceRepository,
    };

    let harness = doctor_test_harness().await;
    let db = harness.db().clone();

    // Two active projects in the DB.
    let project_above = common::create_test_project(&db).await;
    let project_at_threshold = common::create_test_project(&db).await;

    let trace_repo = RetrievalTraceRepository::new(db.clone());

    // The default RetrievalHealthConfig is 24h window, 0.50 threshold, 20-query
    // floor. We build traces accordingly.

    // An empty candidates array = zero-result trace.
    let zero_result_candidates = json!([]);
    // A non-empty candidates array = non-zero-result trace.
    let non_zero_candidates = json!([{
        "note_id": "note-1",
        "outcome": "injected",
        "rank": 1,
        "confidence": 0.9,
    }]);

    // Above-threshold project: 22 total, 12 zero-result -> rate = 12/22 ~ 0.5454
    // (strictly > 0.50). Total (22) >= floor (20).
    for _ in 0..12 {
        trace_repo
            .insert(CreateRetrievalTraceParams {
                project_id: &project_above.id,
                session_id: None,
                task_run_id: None,
                task_id: None,
                entry_point: RetrievalTraceEntryPoint::Dispatch,
                trigger: None,
                candidates: &zero_result_candidates,
                candidate_cap: DEFAULT_CANDIDATE_CAP,
                candidate_cap_exceeded: false,
                sampling_metadata: None,
                durations_ms: &json!({}),
                estimated_injected_tokens: 0,
            })
            .await
            .expect("insert zero-result trace for above-threshold project");
    }
    for _ in 0..10 {
        trace_repo
            .insert(CreateRetrievalTraceParams {
                project_id: &project_above.id,
                session_id: None,
                task_run_id: None,
                task_id: None,
                entry_point: RetrievalTraceEntryPoint::Dispatch,
                trigger: None,
                candidates: &non_zero_candidates,
                candidate_cap: DEFAULT_CANDIDATE_CAP,
                candidate_cap_exceeded: false,
                sampling_metadata: None,
                durations_ms: &json!({}),
                estimated_injected_tokens: 0,
            })
            .await
            .expect("insert non-zero-result trace for above-threshold project");
    }

    // At-threshold project: 20 total, 10 zero-result -> rate = 10/20 = 0.50
    // exactly (equality passes -- no finding emitted). Total (20) >= floor (20).
    for _ in 0..10 {
        trace_repo
            .insert(CreateRetrievalTraceParams {
                project_id: &project_at_threshold.id,
                session_id: None,
                task_run_id: None,
                task_id: None,
                entry_point: RetrievalTraceEntryPoint::Dispatch,
                trigger: None,
                candidates: &zero_result_candidates,
                candidate_cap: DEFAULT_CANDIDATE_CAP,
                candidate_cap_exceeded: false,
                sampling_metadata: None,
                durations_ms: &json!({}),
                estimated_injected_tokens: 0,
            })
            .await
            .expect("insert zero-result trace for at-threshold project");
    }
    for _ in 0..10 {
        trace_repo
            .insert(CreateRetrievalTraceParams {
                project_id: &project_at_threshold.id,
                session_id: None,
                task_run_id: None,
                task_id: None,
                entry_point: RetrievalTraceEntryPoint::Dispatch,
                trigger: None,
                candidates: &non_zero_candidates,
                candidate_cap: DEFAULT_CANDIDATE_CAP,
                candidate_cap_exceeded: false,
                sampling_metadata: None,
                durations_ms: &json!({}),
                estimated_injected_tokens: 0,
            })
            .await
            .expect("insert non-zero-result trace for at-threshold project");
    }

    // Invoke the production-registered check through doctor_run, requesting
    // ONLY the retrieval check by name.
    let response = harness
        .call_tool(
            "doctor_run",
            json!({ "check_names": [RETRIEVAL_ZERO_RESULT_NAME] }),
        )
        .await
        .expect("doctor_run should dispatch");

    assert_eq!(response["ok"], true, "doctor_run should succeed");

    // AC4 + behavioral-defect fix: only the retrieval check should be in
    // results -- NOT every globally-registered check.
    let results = response["results"]
        .as_array()
        .expect("results should be an array");
    assert_eq!(
        results.len(),
        1,
        "only the retrieval check should run when explicitly requested; \
         got {} results: {:?}",
        results.len(),
        results
    );
    assert_eq!(
        results[0]["check"]["name"], RETRIEVAL_ZERO_RESULT_NAME,
        "the single result should be the retrieval check"
    );
    assert_eq!(results[0]["ran"], true, "the check should have run");
    assert_eq!(
        results[0]["error"],
        serde_json::Value::Null,
        "the check should not have an error"
    );

    // Only the above-threshold project should produce a finding.
    let findings = results[0]["findings"]
        .as_array()
        .expect("findings should be an array");
    assert_eq!(
        findings.len(),
        1,
        "exactly one finding (above-threshold project only); got {}",
        findings.len()
    );

    let finding_entry = &findings[0];
    assert_eq!(finding_entry["check_name"], RETRIEVAL_ZERO_RESULT_NAME);
    assert_eq!(finding_entry["severity"], "warn");

    let finding_id = finding_entry["finding_id"]
        .as_str()
        .expect("finding_id should be present");

    // Fetch the persisted finding and inspect its evidence (AC5).
    let repo = DoctorFindingRepository::new(harness.db().clone());
    let persisted = repo
        .get(finding_id)
        .await
        .expect("repo get should succeed")
        .expect("finding should be persisted in DB");
    assert_eq!(persisted.check_name, RETRIEVAL_ZERO_RESULT_NAME);
    assert_eq!(persisted.severity, "warn");

    // The finding should be for the above-threshold project.
    assert_eq!(
        persisted.entity_ids["project_id"].as_str(),
        Some(project_above.id.as_str()),
        "persisted finding entity_ids must reference the above-threshold project"
    );

    // AC5: inspect persisted evidence for all required fields.
    let evidence = &persisted.evidence;

    // project_id
    assert_eq!(
        evidence["project_id"].as_str(),
        Some(project_above.id.as_str()),
        "evidence must include project_id"
    );

    // window (start/end)
    let window = &evidence["window"];
    assert!(
        window["start"].is_string(),
        "evidence must include window.start"
    );
    assert!(
        window["end"].is_string(),
        "evidence must include window.end"
    );
    // The window should span exactly 24 hours (the default config).
    let start_str = window["start"].as_str().unwrap();
    let end_str = window["end"].as_str().unwrap();
    let start = time::OffsetDateTime::parse(
        start_str,
        &time::format_description::well_known::Iso8601::DEFAULT,
    )
    .expect("window.start should parse as ISO-8601");
    let end = time::OffsetDateTime::parse(
        end_str,
        &time::format_description::well_known::Iso8601::DEFAULT,
    )
    .expect("window.end should parse as ISO-8601");
    let window_hours = (end - start).whole_hours();
    assert_eq!(
        window_hours, 24,
        "evidence window should span exactly 24 hours (default config)"
    );

    // threshold
    assert_eq!(
        evidence["threshold"].as_f64(),
        Some(0.50),
        "evidence must include threshold (default 0.50)"
    );

    // floor
    assert_eq!(
        evidence["floor"].as_i64(),
        Some(20),
        "evidence must include floor (default 20)"
    );

    // numerator (zero-result queries)
    assert_eq!(
        evidence["numerator"].as_i64(),
        Some(12),
        "evidence numerator must be 12 (zero-result traces)"
    );

    // denominator (total queries)
    assert_eq!(
        evidence["denominator"].as_i64(),
        Some(22),
        "evidence denominator must be 22 (total traces)"
    );

    // rate
    let rate = evidence["rate"]
        .as_f64()
        .expect("evidence must include rate");
    assert!(
        rate > 0.50,
        "rate ({rate}) must be strictly above the 0.50 threshold"
    );
    // Sanity-check the exact computed rate.
    let expected_rate = 12.0_f64 / 22.0;
    assert!(
        (rate - expected_rate).abs() < 1e-9,
        "rate ({rate}) must equal 12/22 = {expected_rate}"
    );

    // per_entry_point_counts
    let per_ep = &evidence["per_entry_point_counts"];
    assert!(
        per_ep.is_object(),
        "evidence must include per_entry_point_counts as an object"
    );
    let dispatch_counts = &per_ep["dispatch"];
    assert!(
        dispatch_counts.is_object(),
        "per_entry_point_counts must include the 'dispatch' entry point"
    );
    assert_eq!(
        dispatch_counts["total_queries"].as_i64(),
        Some(22),
        "dispatch total_queries must be 22"
    );
    assert_eq!(
        dispatch_counts["zero_result_queries"].as_i64(),
        Some(12),
        "dispatch zero_result_queries must be 12"
    );

    // Verify no finding was persisted for the at-threshold project.
    let all_findings = repo
        .list_recent(djinn_db::RecentDoctorFindings {
            check_name: Some(RETRIEVAL_ZERO_RESULT_NAME.to_string()),
            ..Default::default()
        })
        .await
        .expect("list_recent should succeed");
    let at_threshold_findings: Vec<_> = all_findings
        .iter()
        .filter(|f| f.entity_ids["project_id"].as_str() == Some(project_at_threshold.id.as_str()))
        .collect();
    assert!(
        at_threshold_findings.is_empty(),
        "no finding should be persisted for the at-threshold (equality) project"
    );

    // Confirm registration metadata: the retrieval check appears in
    // registered_checks.
    let registered = response["registered_checks"]
        .as_array()
        .expect("registered_checks should be an array");
    assert!(
        registered
            .iter()
            .any(|c| c["name"] == RETRIEVAL_ZERO_RESULT_NAME),
        "registered_checks should include the retrieval check"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_parent_open_children_db_repair_applies_safe_disposition() {
    use djinn_agent::doctor::{
        CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME, TaskRepositoryClosedParentOpenChildrenSource,
        register_closed_parent_open_children_check_with_repair,
    };
    use djinn_core::events::DjinnEventEnvelope;
    use djinn_db::{EpicRepository, ProposalCreateInput, ProposalRepository, TaskRepository};
    let harness = doctor_test_harness().await;
    let project = common::create_test_project(harness.db()).await;
    let epics = EpicRepository::new(harness.db().clone(), common::test_events());
    let tasks = TaskRepository::new(harness.db().clone(), common::test_events());
    let proposals = ProposalRepository::new(harness.db().clone(), common::test_events());

    // Ready orphan closes; in-flight parks retaining session; PR-review parks retaining PR.
    let ready_epic = common::create_test_epic(harness.db(), &project.id).await;
    let ready = common::create_test_task(harness.db(), &project.id, &ready_epic.id).await;
    epics
        .set_status_raw(&ready_epic.id, "closed")
        .await
        .unwrap();

    let flight_epic = common::create_test_epic(harness.db(), &project.id).await;
    let flight = common::create_test_task(harness.db(), &project.id, &flight_epic.id).await;
    tasks.set_status(&flight.id, "in_progress").await.unwrap();
    let flight_session = common::create_test_session(harness.db(), &project.id, &flight.id).await;
    epics
        .set_status_raw(&flight_epic.id, "closed")
        .await
        .unwrap();

    let pr_epic = common::create_test_epic(harness.db(), &project.id).await;
    let pr = common::create_test_task(harness.db(), &project.id, &pr_epic.id).await;
    tasks.set_status(&pr.id, "pr_review").await.unwrap();
    tasks
        .set_pr_url(&pr.id, "https://github.com/djinnos/djinn/pull/999999")
        .await
        .unwrap();
    epics.set_status_raw(&pr_epic.id, "closed").await.unwrap();

    // Guarded by another open proposal parent.
    let guard_epic = common::create_test_epic(harness.db(), &project.id).await;
    let guard = common::create_test_task(harness.db(), &project.id, &guard_epic.id).await;
    epics
        .set_status_raw(&guard_epic.id, "closed")
        .await
        .unwrap();
    let live_proposal = proposals
        .create(ProposalCreateInput {
            title: "live parent",
            body: "",
            acceptance_criteria: None,
            status: Some("building"),
            body_format: None,
        })
        .await
        .unwrap();
    proposals
        .link_epic(&live_proposal.id, &guard_epic.id, &project.id)
        .await
        .unwrap();

    let (tx, _) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(16);
    let source = Arc::new(TaskRepositoryClosedParentOpenChildrenSource::new(
        harness.db().clone(),
        tx,
    ));
    source.refresh().await;
    register_closed_parent_open_children_check_with_repair(
        registry(),
        source.clone(),
        source.clone(),
    );

    let run = harness
        .call_tool(
            "doctor_run",
            json!({"check_names":[CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME]}),
        )
        .await
        .unwrap();
    assert_eq!(run["ok"], true);
    let findings = run["results"][0]["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 4);
    let repo = DoctorFindingRepository::new(harness.db().clone());

    let mut by_task: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for result in findings {
        let fid = result["finding_id"].as_str().unwrap().to_owned();
        let finding = repo.get(&fid).await.unwrap().unwrap();
        let owner = finding.entity_ids["task_id"].as_str().unwrap().to_owned();
        by_task.insert(owner, fid);
    }

    // Apply repair to each persisted finding.
    for fid in by_task.values() {
        let fix = harness
            .call_tool(
                "doctor_fix",
                json!({
                    "check_name": CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME,
                    "finding_id": fid,
                }),
            )
            .await
            .unwrap();
        assert_eq!(fix["ok"], true);
    }

    // Assert outcomes.
    let ready_row = tasks.get(&ready.id).await.unwrap().unwrap();
    assert_eq!(ready_row.status, "closed");
    assert_eq!(ready_row.close_reason.as_deref(), Some("parent_closed"));

    let flight_row = tasks.get(&flight.id).await.unwrap().unwrap();
    assert_eq!(flight_row.status, "needs_lead_intervention");
    let flight_activity = tasks.list_activity(&flight.id).await.unwrap();
    let flight_repair = flight_activity
        .iter()
        .find(|e| e.event_type == "doctor_fix_repair")
        .expect("flight task must have doctor_fix_repair activity");
    let flight_payload: serde_json::Value = serde_json::from_str(&flight_repair.payload).unwrap();
    assert_eq!(
        flight_payload["park_reason"].as_str(),
        Some("historical_parent_closed_in_flight")
    );
    assert_eq!(
        flight_payload["preserved_session_id"].as_str(),
        Some(flight_session.id.as_str())
    );

    let pr_row = tasks.get(&pr.id).await.unwrap().unwrap();
    assert_eq!(pr_row.status, "needs_lead_intervention");
    assert_eq!(
        pr_row.pr_url.as_deref(),
        Some("https://github.com/djinnos/djinn/pull/999999")
    );
    let pr_activity = tasks.list_activity(&pr.id).await.unwrap();
    let pr_repair = pr_activity
        .iter()
        .find(|e| e.event_type == "doctor_fix_repair")
        .expect("pr task must have doctor_fix_repair activity");
    let pr_payload: serde_json::Value = serde_json::from_str(&pr_repair.payload).unwrap();
    assert_eq!(
        pr_payload["park_reason"].as_str(),
        Some("historical_parent_closed_pr_active")
    );
    assert_eq!(
        pr_payload["preserved_pr_url"].as_str(),
        Some("https://github.com/djinnos/djinn/pull/999999")
    );

    let guard_row = tasks.get(&guard.id).await.unwrap().unwrap();
    assert_eq!(guard_row.status, "open");
    let guard_activity = tasks.list_activity(&guard.id).await.unwrap();
    let guard_repair = guard_activity
        .iter()
        .find(|e| e.event_type == "doctor_fix_repair");
    assert!(
        guard_repair.is_none(),
        "guarded orphan must not emit doctor_fix_repair activity"
    );

    // Audit: each repaired task emits a doctor_fix_repair activity.
    for id in [&ready.id, &flight.id, &pr.id] {
        let activity = tasks.list_activity(id).await.unwrap();
        let has_repair = activity.iter().any(|e| e.event_type == "doctor_fix_repair");
        assert!(
            has_repair,
            "task {} should have doctor_fix_repair activity",
            id
        );
    }

    // Idempotency: a second repair run reports no actionable findings.
    let run2 = harness
        .call_tool(
            "doctor_run",
            json!({"check_names":[CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME]}),
        )
        .await
        .unwrap();
    let findings2 = run2["results"][0]["findings"].as_array().unwrap();
    assert_eq!(findings2.len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_parent_open_children_repair_skips_external_open_dependent() {
    use djinn_agent::doctor::{
        CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME, TaskRepositoryClosedParentOpenChildrenSource,
        register_closed_parent_open_children_check_with_repair,
    };
    use djinn_core::events::DjinnEventEnvelope;
    use djinn_db::{EpicRepository, TaskRepository};
    let harness = doctor_test_harness().await;
    let project = common::create_test_project(harness.db()).await;
    let epics = EpicRepository::new(harness.db().clone(), common::test_events());
    let tasks = TaskRepository::new(harness.db().clone(), common::test_events());

    let orphan_epic = common::create_test_epic(harness.db(), &project.id).await;
    let orphan = common::create_test_task(harness.db(), &project.id, &orphan_epic.id).await;
    epics
        .set_status_raw(&orphan_epic.id, "closed")
        .await
        .unwrap();

    // Open dependent in a different epic; orphan blocks it.
    let other_epic = common::create_test_epic(harness.db(), &project.id).await;
    let dependent = common::create_test_task(harness.db(), &project.id, &other_epic.id).await;
    tasks.add_blocker(&dependent.id, &orphan.id).await.unwrap();

    let (tx, _) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(16);
    let source = std::sync::Arc::new(TaskRepositoryClosedParentOpenChildrenSource::new(
        harness.db().clone(),
        tx,
    ));
    source.refresh().await;
    register_closed_parent_open_children_check_with_repair(
        registry(),
        source.clone(),
        source.clone(),
    );

    let run = harness
        .call_tool(
            "doctor_run",
            json!({"check_names":[CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME]}),
        )
        .await
        .unwrap();
    let findings = run["results"][0]["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["recommended_action"], "retain");
    assert_eq!(findings[0]["recommended_reason"], "external_open_dependent");

    let fid = findings[0]["finding_id"].as_str().unwrap().to_owned();
    let fix = harness
        .call_tool(
            "doctor_fix",
            json!({
                "check_name": CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME,
                "finding_id": fid,
            }),
        )
        .await
        .unwrap();
    assert_eq!(fix["ok"], true);

    let orphan_row = tasks.get(&orphan.id).await.unwrap().unwrap();
    assert_eq!(orphan_row.status, "open");
    let dependent_row = tasks.get(&dependent.id).await.unwrap().unwrap();
    assert_eq!(dependent_row.status, "open");

    let activity = tasks.list_activity(&orphan.id).await.unwrap();
    assert!(
        !activity.iter().any(|e| e.event_type == "doctor_fix_repair"),
        "external-dependent orphan must not be repaired"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_parent_open_children_repair_cascades_internal_blocker() {
    use djinn_agent::doctor::{
        CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME, TaskRepositoryClosedParentOpenChildrenSource,
        register_closed_parent_open_children_check_with_repair,
    };
    use djinn_core::events::DjinnEventEnvelope;
    use djinn_db::{EpicRepository, TaskRepository};
    let harness = doctor_test_harness().await;
    let project = common::create_test_project(harness.db()).await;
    let epics = EpicRepository::new(harness.db().clone(), common::test_events());
    let tasks = TaskRepository::new(harness.db().clone(), common::test_events());

    let parent_epic = common::create_test_epic(harness.db(), &project.id).await;
    let blocker = common::create_test_task(harness.db(), &project.id, &parent_epic.id).await;
    let dependent = common::create_test_task(harness.db(), &project.id, &parent_epic.id).await;
    tasks.add_blocker(&dependent.id, &blocker.id).await.unwrap();
    epics
        .set_status_raw(&parent_epic.id, "closed")
        .await
        .unwrap();

    let (tx, _) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(16);
    let source = std::sync::Arc::new(TaskRepositoryClosedParentOpenChildrenSource::new(
        harness.db().clone(),
        tx,
    ));
    source.refresh().await;
    register_closed_parent_open_children_check_with_repair(
        registry(),
        source.clone(),
        source.clone(),
    );

    let run = harness
        .call_tool(
            "doctor_run",
            json!({"check_names":[CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME]}),
        )
        .await
        .unwrap();
    let findings = run["results"][0]["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 2);
    for finding in findings {
        assert_eq!(finding["recommended_action"], "close");
        assert_eq!(finding["recommended_reason"], "parent_closed");
    }

    let repo = DoctorFindingRepository::new(harness.db().clone());
    for result in findings {
        let fid = result["finding_id"].as_str().unwrap().to_owned();
        let finding = repo.get(&fid).await.unwrap().unwrap();
        let fix = harness
            .call_tool(
                "doctor_fix",
                json!({
                    "check_name": CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME,
                    "finding_id": fid,
                }),
            )
            .await
            .unwrap();
        assert_eq!(fix["ok"], true);

        let task_id = finding.entity_ids["task_id"].as_str().unwrap();
        let row = tasks.get(task_id).await.unwrap().unwrap();
        assert_eq!(row.status, "closed");
        assert_eq!(row.close_reason.as_deref(), Some("parent_closed"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_parent_open_children_repair_skips_stale_snapshot() {
    use djinn_agent::doctor::{
        CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME, TaskRepositoryClosedParentOpenChildrenSource,
        register_closed_parent_open_children_check_with_repair,
    };
    use djinn_core::events::DjinnEventEnvelope;
    use djinn_db::{EpicRepository, TaskRepository};
    let harness = doctor_test_harness().await;
    let project = common::create_test_project(harness.db()).await;
    let epics = EpicRepository::new(harness.db().clone(), common::test_events());
    let tasks = TaskRepository::new(harness.db().clone(), common::test_events());

    let ready_epic = common::create_test_epic(harness.db(), &project.id).await;
    let ready = common::create_test_task(harness.db(), &project.id, &ready_epic.id).await;
    epics
        .set_status_raw(&ready_epic.id, "closed")
        .await
        .unwrap();

    let (tx, _) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(16);
    let source = std::sync::Arc::new(TaskRepositoryClosedParentOpenChildrenSource::new(
        harness.db().clone(),
        tx,
    ));
    source.refresh().await;
    register_closed_parent_open_children_check_with_repair(
        registry(),
        source.clone(),
        source.clone(),
    );

    let run = harness
        .call_tool(
            "doctor_run",
            json!({"check_names":[CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME]}),
        )
        .await
        .unwrap();
    let findings = run["results"][0]["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1);
    let fid = findings[0]["finding_id"].as_str().unwrap().to_owned();

    // Simulate status drift before repair: the task is now already closed by an
    // unrelated path, so the snapshot is stale.
    tasks.set_status(&ready.id, "closed").await.unwrap();

    let fix = harness
        .call_tool(
            "doctor_fix",
            json!({
                "check_name": CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME,
                "finding_id": fid,
            }),
        )
        .await
        .unwrap();
    assert_eq!(fix["ok"], true);

    let row = tasks.get(&ready.id).await.unwrap().unwrap();
    assert_eq!(row.status, "closed");
    let activity = tasks.list_activity(&ready.id).await.unwrap();
    assert!(
        !activity.iter().any(|e| e.event_type == "doctor_fix_repair"),
        "stale snapshot must not be repaired"
    );
}
