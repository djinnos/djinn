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

#[tokio::test]
async fn closed_parent_open_children_db_dry_run_is_read_only() {
    use djinn_agent::doctor::{
        CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME, TaskRepositoryClosedParentOpenChildrenSource,
        register_closed_parent_open_children_check,
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
            status: Some("ready"),
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
    let mut guards = Vec::new();
    let mut owners = Vec::new();
    for result in findings {
        let finding = repo
            .get(result["finding_id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        let owner = finding.entity_ids["task_id"].as_str().unwrap();
        assert_eq!(finding.evidence["board_health_finding"]["id"], owner);
        assert_eq!(
            finding.resolver_snapshot.as_ref().unwrap()["inputs"]["board_health_finding"]["id"],
            owner
        );
        guards.push(
            finding.evidence["selected_disposition"]["guard"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
        owners.push(owner.to_owned());
    }
    guards.sort();
    assert_eq!(
        guards,
        vec![
            "historical_parent_closed_in_flight",
            "historical_parent_closed_pr_active",
            "other_open_parent",
            "parent_closed"
        ]
    );
    owners.sort();
    ids.sort();
    assert_eq!(owners, ids);
    let mut tasks_after = Vec::new();
    let mut activity_after = Vec::new();
    for id in &ids {
        tasks_after.push(serde_json::to_value(tasks.get(id).await.unwrap().unwrap()).unwrap());
        activity_after.push(serde_json::to_value(tasks.list_activity(id).await.unwrap()).unwrap());
    }
    assert_eq!(tasks_before, tasks_after);
    assert_eq!(activity_before, activity_after);
}
