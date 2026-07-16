//! End-to-end coverage for the coordinator's `stranded_ready` doctor check.

use std::sync::Arc;

use djinn_core::doctor::DoctorRegistry;
use djinn_db::TaskRepository;
use tokio::sync::broadcast;

use super::create_task_with_note;
use crate::doctor::leader_tick::run_cheap_doctor_checks;
use crate::doctor::stranded_ready::{
    MemoryStrandedReadySource, STRANDED_READY_CHECK_NAME, StrandedReadyCheck,
};
use crate::events::event_bus_for;
use crate::test_helpers;

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
