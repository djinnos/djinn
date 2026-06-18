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

use std::time::{Duration, Instant};

use djinn_core::doctor::{DoctorRegistry, FindingSeverity};
use djinn_db::{
    CreateSessionParams, DoctorFindingRepository, RecentDoctorFindings, SessionRepository,
    TaskRepository,
};
use serde_json::Value;
use tokio::sync::broadcast;

use super::{coordinator_actor_for_tests, create_task_with_note};
use crate::actors::coordinator::dispatch::session_recovery::ZOMBIE_HARD_CAP_SECS;
use crate::doctor::leader_tick::{DOCTOR_CRITICAL_FINDING_ACTIVITY, run_cheap_doctor_checks};
use crate::doctor::zombie_running_session::{
    ZOMBIE_RUNNING_SESSION_CHECK_NAME, check_from_coordinator_state,
};
use crate::events::event_bus_for;
use crate::test_helpers;

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
    sqlx::query("UPDATE tasks SET status = 'in_progress' WHERE id = $1")
        .bind(&task.id)
        .execute(db.pool())
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
        })
        .await
        .unwrap();

    // Backdate started_at by ~30 s (well inside the 600 s reaper window).
    sqlx::query(
        "UPDATE sessions
           SET started_at = to_char(
                 now() AT TIME ZONE 'utc' - interval '30 seconds',
                 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
         WHERE id = $1",
    )
    .bind(&session.id)
    .execute(db.pool())
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
