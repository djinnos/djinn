//! Hermetic end-to-end regression for the `zombie_running_session` doctor
//! check driven through the coordinator leader-tick cheap-doctor helper.
//!
//! This single test proves all five epic ed05 acceptance criteria:
//!
//! 1. The returned `Finding` carries `check_name == "zombie_running_session"`,
//!    `severity == Critical`, matching `session_id` / `task_id`, non-empty
//!    evidence, and resolver snapshot fields.
//! 2. A matching `doctor_findings` row is persisted with `check_name`,
//!    `severity`, matching entity ids / evidence, and a non-empty `created_at`.
//! 3. The rendered `djinn_doctor_findings{check="zombie_running_session"}` metric
//!    series is present with value >= 1 (gauge facade).
//! 4. Doctor detection latency is strictly less than the DB-truth zombie reaper
//!    window (`ZOMBIE_HARD_CAP_SECS`). This is a design-bound assertion — the
//!    test path finishes in < 1 s and does not sleep for the real 600 s window.
//! 5. A critical doctor board activity row exists in `activity_log` for the
//!    fabricated task/session.

use std::sync::Arc;
use std::time::{Duration, Instant};

use djinn_core::doctor::{DoctorRegistry, FindingSeverity};
use djinn_db::{
    CreateSessionParams, DoctorFindingRepository, RecentDoctorFindings, SessionRepository,
    TaskRepository,
};
use serde_json::Value;
use tokio::sync::broadcast;

use super::{coordinator_actor_for_tests, create_task_with_note};
use crate::dispatch::session_recovery::ZOMBIE_HARD_CAP_SECS;
use crate::doctor::leader_tick::{DOCTOR_CRITICAL_FINDING_ACTIVITY, run_cheap_doctor_checks};
use crate::doctor::stranded_ready::{
    MemoryStrandedReadySource, STRANDED_READY_CHECK_NAME, StrandedReadyCheck,
};
use crate::doctor::zombie_running_session::{
    ZOMBIE_RUNNING_SESSION_CHECK_NAME, check_from_coordinator_state,
};
use crate::events::event_bus_for;
use crate::test_helpers;
use djinn_db::LivenessEvidenceSnapshot;
use djinn_db::LivenessRepository;

/// Alias the production reaper window constant so the latency assertion reads
/// naturally: doctor detection must complete in well under this bound, proving
/// the design property that doctor catches zombie sessions *before* the
/// existing 600-second DB-truth reaper would fire.
const REAPER_WINDOW_SECS: u64 = ZOMBIE_HARD_CAP_SECS;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zombie_running_session_doctor_e2e() {
    // ── 0. Telemetry init ───────────────────────────────────────────────
    let _ = djinn_telemetry::init();

    // ── 1. Fixture: test DB + broadcast channel ─────────────────────────
    let db = test_helpers::create_test_db();
    let (tx, _rx): (broadcast::Sender<_>, _) = broadcast::channel(256);

    // ── 2. Create a task/note and set task status to in_progress ────────
    let (task, _note) = create_task_with_note(&db, &tx, "doctor-zombie-e2e").await;
    TaskRepository::new(db.clone(), event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    // ── 3. Create a sessions row with status='running', backdate by 30 s ─
    // This is inside the 600 s DB-truth zombie reaper window, so the reaper
    // would NOT reap it — but doctor should still detect the divergence.
    let session_repo = SessionRepository::new(db.clone(), event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    // Backdate started_at by ~30 s (well inside the 600 s reaper window).
    session_repo
        .backdate_started_at(&session.id, "30 seconds")
        .await
        .unwrap();

    // ── 4. Preconditions ────────────────────────────────────────────────
    assert!(
        session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "precondition: the fabricated session must appear as active/running"
    );

    // Build the coordinator actor fixture — its slot pool is empty (no live
    // session dispatched for this task) and has no rpc_registry, so
    // check_from_coordinator_state will snapshot slot_pool_has_session=false,
    // worker_connected=false, pod_present=false.
    let actor = coordinator_actor_for_tests(&db, &tx);

    // Build the zombie check from coordinator state (the production seam).
    let zombie_check = check_from_coordinator_state(
        &db,
        &tx,
        &actor.pool,
        None, // no rpc_registry → worker_connected = false
    )
    .await
    .expect("check_from_coordinator_state succeeds");

    // Register into a fresh test registry.
    let registry = DoctorRegistry::new();
    djinn_core::doctor::register(&registry, zombie_check);

    // ── 5. Invoke the production cheap-doctor leader-tick helper ────────
    let before = Instant::now();
    let runs = run_cheap_doctor_checks(&registry, &db, &tx, Some("doctor-zombie-e2e-run")).await;
    let tick_elapsed = before.elapsed();

    // ── 6. Assertion block 1: returned finding ─────────────────────────
    let findings = runs
        .iter()
        .find(|run| run.check_name == ZOMBIE_RUNNING_SESSION_CHECK_NAME)
        .expect("zombie cheap-doctor run returned")
        .findings
        .as_slice();

    let finding = findings
        .iter()
        .find(|f| {
            f.check_name == ZOMBIE_RUNNING_SESSION_CHECK_NAME
                && f.entity_ids.get("session_id").map(String::as_str) == Some(&session.id)
                && f.entity_ids.get("task_id").map(String::as_str) == Some(&task.id)
        })
        .expect("zombie finding for the fabricated session");

    assert_eq!(finding.severity, FindingSeverity::Critical);
    assert_eq!(finding.check_name, ZOMBIE_RUNNING_SESSION_CHECK_NAME);
    assert_eq!(
        finding.entity_ids.get("session_id").map(String::as_str),
        Some(session.id.as_str())
    );
    assert_eq!(
        finding.entity_ids.get("task_id").map(String::as_str),
        Some(task.id.as_str())
    );
    assert!(finding.evidence != Value::Null, "evidence must be non-null");
    assert!(
        finding.evidence.is_object(),
        "evidence must be structured JSON"
    );
    assert!(
        !finding.resolver_snapshot.resolver.is_empty(),
        "resolver_snapshot.resolver must be non-empty"
    );
    assert!(
        finding.resolver_snapshot.inputs != Value::Null,
        "resolver_snapshot.inputs must be non-null"
    );
    assert!(
        finding.resolver_snapshot.outputs != Value::Null,
        "resolver_snapshot.outputs must be non-null"
    );
    assert!(
        !finding.detail.is_empty(),
        "finding detail must be non-empty"
    );
    // Classifier-aligned evidence: 5ric verdict/outcome/reason concepts.
    assert_eq!(
        finding.evidence["classifier"]["verdict"], "dead",
        "finding must carry a 5ric-aligned liveness verdict"
    );
    assert!(
        !finding.evidence["classifier"]["outcome"]
            .as_str()
            .unwrap_or("")
            .is_empty(),
        "finding must carry a 5ric-aligned liveness outcome"
    );

    // ── 7. Assertion block 2: persistence row ──────────────────────────
    let rows = DoctorFindingRepository::new(db.clone())
        .list_recent(RecentDoctorFindings {
            run_id: Some("doctor-zombie-e2e-run".to_owned()),
            check_name: Some(ZOMBIE_RUNNING_SESSION_CHECK_NAME.to_owned()),
            ..Default::default()
        })
        .await
        .expect("list recent doctor findings");

    let row = rows
        .iter()
        .find(|r| {
            // entity_ids is a JSONB object persisted by leader_tick;
            // assert it contains our session and task ids.
            r.entity_ids.to_string().contains(&session.id)
                && r.entity_ids.to_string().contains(&task.id)
        })
        .expect("persisted doctor_findings row for the zombie session");

    assert_eq!(row.check_name, ZOMBIE_RUNNING_SESSION_CHECK_NAME);
    assert_eq!(row.severity, "critical");
    assert!(
        !row.created_at.is_empty(),
        "created_at must be a non-empty timestamp"
    );
    assert!(
        row.evidence != Value::Null && row.evidence.is_object(),
        "persisted evidence must be non-null structured JSON"
    );
    assert!(
        row.resolver_snapshot.is_some(),
        "resolver_snapshot must be persisted"
    );
    // Verify entity_ids map contains the expected keys.
    assert_eq!(
        row.entity_ids.get("session_id").and_then(Value::as_str),
        Some(session.id.as_str()),
    );
    assert_eq!(
        row.entity_ids.get("task_id").and_then(Value::as_str),
        Some(task.id.as_str()),
    );

    // ── 8. Assertion block 3: Prometheus metric ────────────────────────
    // The landed metric facade is a gauge (set_findings). Assert the rendered
    // series for check="zombie_running_session" is present and >= 1.
    let rendered = djinn_telemetry::render().expect("render metrics");
    let zombie_metric_line = rendered
        .lines()
        .find(|line| {
            line.starts_with("djinn_doctor_findings{")
                && line.contains(ZOMBIE_RUNNING_SESSION_CHECK_NAME)
        })
        .expect("djinn_doctor_findings metric for zombie_running_session must be rendered");

    assert!(
        zombie_metric_line.contains(&format!("check=\"{}\"", ZOMBIE_RUNNING_SESSION_CHECK_NAME)),
        "metric line must carry the stable check label: {zombie_metric_line}"
    );
    let metric_value: f64 = zombie_metric_line
        .rsplit(' ')
        .next()
        .expect("metric sample has a value")
        .parse()
        .expect("metric sample is numeric");
    assert!(
        metric_value >= 1.0,
        "djinn_doctor_findings{{check=\"zombie_running_session\"}} must be >= 1, got {metric_value}"
    );

    // ── 9. Assertion block 4: latency bound vs reaper window ───────────
    // The fabricated session is backdated by only 30 s, so the existing
    // reap_zombie_sessions() hard cap (600 s) would not fire. Doctor
    // detects the divergence in well under a second via
    // run_cheap_doctor_checks — strictly less than the reaper window.
    // This is a design-bound assertion: it proves doctor catches zombie
    // sessions before the existing reaper, without a 600 s wall-clock wait.
    assert!(
        tick_elapsed < Duration::from_secs(REAPER_WINDOW_SECS),
        "doctor detection ({tick_elapsed:?}) must complete before the DB-truth \
         zombie reaper's hard cap ({REAPER_WINDOW_SECS}s); this is a design-bound \
         assertion (the test path finishes in <1s) rather than a wall-clock race \
         against the 600s reaper"
    );

    // ── 10. Assertion block 5: critical board activity ─────────────────
    let task_repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let activity_entries = task_repo
        .list_activity(&task.id)
        .await
        .expect("list task activity");

    let critical_entry = activity_entries
        .iter()
        .find(|entry| entry.event_type == DOCTOR_CRITICAL_FINDING_ACTIVITY)
        .expect("critical doctor activity entry must exist for the fabricated task");

    assert_eq!(critical_entry.actor_id, "coordinator");
    assert_eq!(critical_entry.actor_role, "system");

    let payload: Value =
        serde_json::from_str(&critical_entry.payload).expect("activity payload is JSON");

    assert_eq!(
        payload.get("severity").and_then(Value::as_str),
        Some("critical"),
        "activity payload must contain severity = critical"
    );
    assert_eq!(
        payload.get("check_name").and_then(Value::as_str),
        Some(ZOMBIE_RUNNING_SESSION_CHECK_NAME),
        "activity payload must contain check_name = zombie_running_session"
    );
    // The payload.entity_ids map must reference the fabricated session/task.
    let entity_ids = payload
        .get("entity_ids")
        .expect("activity payload must contain entity_ids");
    assert_eq!(
        entity_ids.get("session_id").and_then(Value::as_str),
        Some(session.id.as_str()),
        "activity entity_ids must reference the fabricated session"
    );
    assert_eq!(
        entity_ids.get("task_id").and_then(Value::as_str),
        Some(task.id.as_str()),
        "activity entity_ids must reference the fabricated task"
    );
}

/// Consistency test: zombie/liveness evidence reported by the doctor surface
/// must agree with the `liveness_outcomes` section produced by `board_health`.
///
/// This test seeds a zombie session with persisted liveness evidence (dead
/// verdict / dead_reclaimed outcome) via `LivenessRepository`, then runs the
/// cheap-doctor zombie check and queries `board_health` on the same DB.  It
/// asserts that both surfaces report the same task id, session id, verdict,
/// and outcome kind — proving the jk7v diagnostics contract holds end-to-end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zombie_liveness_consistency_across_doctor_and_board_health() {
    let _ = djinn_telemetry::init();

    let db = test_helpers::create_test_db();
    let (tx, _rx): (broadcast::Sender<_>, _) = broadcast::channel(256);

    // ── 1. Fixture: task + session ─────────────────────────────────────
    let (task, _note) = create_task_with_note(&db, &tx, "consistency-zombie").await;
    TaskRepository::new(db.clone(), event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    session_repo
        .backdate_started_at(&session.id, "30 seconds")
        .await
        .unwrap();

    // ── 2. Persist liveness evidence (jk7v DB fields) ─────────────────
    let liveness_repo = LivenessRepository::new(db.clone());
    let evidence_id = liveness_repo
        .persist_evidence(&LivenessEvidenceSnapshot {
            session_id: session.id.clone(),
            task_id: Some(task.id.clone()),
            task_run_id: None,
            verdict: "dead".to_owned(),
            outcome_kind: Some("dead_reclaimed".to_owned()),
            outcome_reason: Some("hard_runtime_exceeded".to_owned()),
            evidence: serde_json::json!({
                "pod_phase": "Succeeded",
                "claim_ttl_expired": true,
                "hard_runtime_exceeded": true,
            }),
        })
        .await
        .expect("persist liveness evidence");

    assert!(!evidence_id.is_empty());

    // ── 3. Doctor surface: run zombie check ────────────────────────────
    let actor = coordinator_actor_for_tests(&db, &tx);
    let zombie_check = check_from_coordinator_state(&db, &tx, &actor.pool, None)
        .await
        .expect("check_from_coordinator_state succeeds");

    let registry = DoctorRegistry::new();
    djinn_core::doctor::register(&registry, zombie_check);

    let runs = run_cheap_doctor_checks(&registry, &db, &tx, Some("consistency-zombie-run")).await;

    let zombie_run = runs
        .iter()
        .find(|r| r.check_name == ZOMBIE_RUNNING_SESSION_CHECK_NAME)
        .expect("zombie check ran");

    let doctor_finding = zombie_run
        .findings
        .iter()
        .find(|f| {
            f.entity_ids.get("task_id").map(String::as_str) == Some(&task.id)
                && f.entity_ids.get("session_id").map(String::as_str) == Some(&session.id)
        })
        .expect("doctor zombie finding for our task/session");

    // Doctor surface classifier evidence (5ric-aligned).
    assert_eq!(
        doctor_finding.evidence["classifier"]["verdict"], "dead",
        "doctor finding must carry dead verdict"
    );
    let doctor_outcome = doctor_finding.evidence["classifier"]["outcome"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    assert!(
        !doctor_outcome.is_empty(),
        "doctor finding must carry a non-empty outcome"
    );

    // ── 4. Board-health surface: query liveness_outcomes ───────────────
    let task_repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let board = task_repo
        .board_health(24)
        .await
        .expect("board_health query succeeds");

    let liveness_outcomes = &board["liveness_outcomes"];
    assert_eq!(
        liveness_outcomes["total"].as_i64(),
        Some(1),
        "board_health must surface exactly 1 liveness outcome"
    );

    let recent = liveness_outcomes["recent"]
        .as_array()
        .expect("liveness_outcomes.recent is an array");
    let board_item = recent
        .iter()
        .find(|item| {
            item["task_id"].as_str() == Some(&task.id)
                && item["session_id"].as_str() == Some(&session.id)
        })
        .expect("board_health liveness_outcomes contains our task/session");

    // ── 5. Consistency assertions ──────────────────────────────────────
    // Same task id.
    assert_eq!(
        board_item["task_id"].as_str(),
        doctor_finding.entity_ids.get("task_id").map(String::as_str),
        "task_id must match between board_health and doctor surfaces"
    );
    // Same session id.
    assert_eq!(
        board_item["session_id"].as_str(),
        doctor_finding
            .entity_ids
            .get("session_id")
            .map(String::as_str),
        "session_id must match between board_health and doctor surfaces"
    );
    // Same verdict.
    assert_eq!(
        board_item["verdict"].as_str(),
        Some("dead"),
        "board_health verdict must match the persisted evidence"
    );
    assert_eq!(
        board_item["verdict"].as_str(),
        doctor_finding.evidence["classifier"]["verdict"].as_str(),
        "verdict must agree across both surfaces"
    );
    // Same outcome_kind.
    assert_eq!(
        board_item["outcome_kind"].as_str(),
        Some("dead_reclaimed"),
        "board_health outcome_kind must match the persisted evidence"
    );
    // Same outcome_reason.
    assert_eq!(
        board_item["outcome_reason"].as_str(),
        Some("hard_runtime_exceeded"),
        "board_health outcome_reason must match the persisted evidence"
    );
}

/// Consistency test: stranded-ready findings reported by the doctor surface
/// must agree with the `stranded_ready` section produced by `board_health`.
///
/// This test creates a task that has been open and unclaimed beyond the
/// 30-minute stranded-ready threshold, then runs both the doctor
/// `stranded_ready` check and the `board_health` query.  It asserts that
/// both surfaces report the same task id, severity, threshold ladder, and
/// dispatch-gate evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stranded_ready_consistency_across_doctor_and_board_health() {
    let _ = djinn_telemetry::init();

    let db = test_helpers::create_test_db();
    let (tx, _rx): (broadcast::Sender<_>, _) = broadcast::channel(256);

    // ── 1. Fixture: open task with no active session, backdated ────────
    let (task, _note) = create_task_with_note(&db, &tx, "consistency-stranded").await;
    // The task is already "open" with no session.  Backdate its updated_at
    // well past the 30-minute threshold so board_health surfaces it.
    djinn_db::test_support::backdate_task_updated_at(&db, &task.id, "90 minutes").await;

    // ── 2. Board-health surface: query stranded_ready ──────────────────
    let task_repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let board = task_repo
        .board_health(24)
        .await
        .expect("board_health query succeeds");

    let stranded_section = &board["stranded_ready"];
    assert_eq!(
        stranded_section["threshold_minutes"].as_i64(),
        Some(30),
        "board_health must echo the base 30-minute threshold"
    );

    let board_findings = stranded_section["findings"]
        .as_array()
        .expect("stranded_ready.findings is an array");
    let board_finding = board_findings
        .iter()
        .find(|f| f["id"].as_str() == Some(&task.id))
        .expect("board_health stranded_ready contains our task");

    let board_severity = board_finding["severity"]
        .as_str()
        .expect("board severity is a string");
    let board_elapsed = board_finding["elapsed_minutes"]
        .as_i64()
        .expect("board elapsed_minutes is an integer");
    let board_gate = &board_finding["dispatch_gate"];
    let board_gate_verdict = board_gate["gate_verdict"]
        .as_str()
        .expect("board gate_verdict is a string");
    let board_threshold = &board_finding["threshold"];
    let board_warning = board_threshold["warning_minutes"]
        .as_i64()
        .expect("board warning_minutes");
    let board_error = board_threshold["error_minutes"]
        .as_i64()
        .expect("board error_minutes");
    let board_critical = board_threshold["critical_minutes"]
        .as_i64()
        .expect("board critical_minutes");

    // Sanity: 90 minutes elapsed → error severity (>= 60m, < 180m).
    assert!(
        board_elapsed >= 60,
        "elapsed should be at least 60 minutes for a 90-minute backdate, got {board_elapsed}"
    );
    assert_eq!(
        board_severity, "error",
        "90-minute backdate → error severity"
    );
    assert_eq!(
        board_gate_verdict, "stranded",
        "no gates should be open for a fresh test task"
    );

    // ── 3. Doctor surface: run stranded_ready check ────────────────────
    // Build a snapshot from the board_health stranded_ready section — the
    // production doctor check uses the same DB-backed snapshot via
    // TaskRepositoryStrandedReadySource.  The MemoryStrandedReadySource
    // double gives us a hermetic equivalent.
    let source = Arc::new(MemoryStrandedReadySource::new(stranded_section.clone()));
    let check = StrandedReadyCheck::new(source);
    let registry = DoctorRegistry::new();
    djinn_core::doctor::register(&registry, check);

    let runs = run_cheap_doctor_checks(&registry, &db, &tx, Some("consistency-stranded-run")).await;

    let stranded_run = runs
        .iter()
        .find(|r| r.check_name == STRANDED_READY_CHECK_NAME)
        .expect("stranded_ready check ran");

    let doctor_finding = stranded_run
        .findings
        .iter()
        .find(|f| f.entity_ids.get("task_id").map(String::as_str) == Some(&task.id))
        .expect("doctor stranded_ready finding for our task");

    // ── 4. Consistency assertions ──────────────────────────────────────
    // Same task id.
    assert_eq!(
        doctor_finding.entity_ids.get("task_id").map(String::as_str),
        board_finding["id"].as_str(),
        "task_id must match between doctor and board_health"
    );

    // Same severity.
    let doctor_severity = doctor_finding.evidence["severity"]
        .as_str()
        .expect("doctor severity is a string");
    assert_eq!(
        doctor_severity, board_severity,
        "severity must agree across surfaces"
    );

    // Same threshold ladder.
    assert_eq!(
        doctor_finding.evidence["threshold"]["warning_minutes"].as_i64(),
        Some(board_warning),
        "warning_minutes threshold must match"
    );
    assert_eq!(
        doctor_finding.evidence["threshold"]["error_minutes"].as_i64(),
        Some(board_error),
        "error_minutes threshold must match"
    );
    assert_eq!(
        doctor_finding.evidence["threshold"]["critical_minutes"].as_i64(),
        Some(board_critical),
        "critical_minutes threshold must match"
    );

    // Same elapsed minutes (within 1-minute tolerance for clock skew).
    let doctor_elapsed = doctor_finding.evidence["elapsed_minutes"]
        .as_i64()
        .expect("doctor elapsed_minutes");
    assert!(
        (doctor_elapsed - board_elapsed).abs() <= 1,
        "elapsed_minutes must be within 1-minute tolerance: doctor={doctor_elapsed} board={board_elapsed}"
    );

    // Same dispatch-gate evidence.
    let doctor_gate = &doctor_finding.evidence["dispatch_gate"];
    assert_eq!(
        doctor_gate["gate_verdict"].as_str(),
        Some(board_gate_verdict),
        "gate_verdict must agree across surfaces"
    );
    assert_eq!(
        doctor_gate["evaluated_role"].as_str(),
        board_gate["evaluated_role"].as_str(),
        "evaluated_role must agree across surfaces"
    );
    assert_eq!(
        doctor_gate["breaker_open"].as_bool(),
        board_gate["breaker_open"].as_bool(),
        "breaker_open must agree across surfaces"
    );
    assert_eq!(
        doctor_gate["manually_paused"].as_bool(),
        board_gate["manually_paused"].as_bool(),
        "manually_paused must agree across surfaces"
    );
    assert_eq!(
        doctor_gate["rate_limited"].as_bool(),
        board_gate["rate_limited"].as_bool(),
        "rate_limited must agree across surfaces"
    );
    assert_eq!(
        doctor_gate["credential_available"].as_bool(),
        board_gate["credential_available"].as_bool(),
        "credential_available must agree across surfaces"
    );
}
