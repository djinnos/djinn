//! Pure unit coverage for the Kueue admission gate's decision logic.
//!
//! The database-backed non-vacuity proofs live in
//! `djinn-db/tests/task_tests/board_health_kueue_admission.rs`; these pin the
//! three-way outcome that must never collapse into two.

use super::*;

fn row(task_run_id: &str, admission: &str, task_id: Option<&str>) -> KueueAdmissionRow {
    KueueAdmissionRow {
        task_run_id: task_run_id.to_owned(),
        admission: admission.to_owned(),
        reason: Some("Preempted".to_owned()),
        workload_name: Some(format!("job-{task_run_id}")),
        transitions: 3,
        task_id: task_id.map(ToOwned::to_owned),
        task_short_id: task_id.map(|_| "ab12".to_owned()),
        first_seen_at: Some("2026-07-30T00:00:00.000Z".to_owned()),
        observed_at: Some("2026-07-30T00:10:00.000Z".to_owned()),
        first_seen_age_seconds: 900,
        observed_age_seconds: 4,
    }
}

fn observing(rows: Vec<KueueAdmissionRow>) -> KueueProjection {
    let summary = KueueSummary {
        total: rows.len() as i64,
        pending: rows.iter().filter(|r| r.admission == "pending").count() as i64,
        admitted: rows.iter().filter(|r| r.admission == "admitted").count() as i64,
        finished: rows.iter().filter(|r| r.admission == "finished").count() as i64,
        without_task_run: rows.iter().filter(|r| r.task_id.is_none()).count() as i64,
        stalest_observation_age_seconds: Some(4),
        oldest_pending_first_seen_age_seconds: Some(900),
    };
    let pending = rows
        .iter()
        .filter(|r| r.admission == "pending")
        .cloned()
        .collect();
    let by_task = rows
        .into_iter()
        .filter_map(|r| r.task_id.clone().map(|id| (id, r)))
        .collect();
    KueueProjection::Observing {
        summary,
        pending,
        by_task,
    }
}

/// **AC2, in isolation.** An empty projection is the shipped default and must
/// never read as a queued Workload.
#[test]
fn an_empty_projection_is_inert_and_never_pending() {
    let outcome = kueue_gate(&KueueProjection::Inert, "task-1");
    assert_eq!(
        outcome.kueue_admission["projection_state"],
        "no_workloads_observed"
    );
    assert_eq!(outcome.kueue_admission["pending"], 0);
    assert!(
        outcome.reasons.is_empty(),
        "an unarmed cluster must contribute no dispatch reason, got {:?}",
        outcome.reasons
    );
    assert!(
        !outcome.evaluated,
        "an empty relation cannot distinguish an unarmed cluster from a dead reflector, so the \
         gate is NOT evaluated"
    );
    assert!(outcome.unevaluated_detail.is_some());
}

/// An unreadable relation must not be laundered into "no rows".
#[test]
fn an_unreadable_projection_is_not_an_empty_one() {
    let outcome = kueue_gate(
        &KueueProjection::Unobservable { detail: "boom" },
        "task-1",
    );
    assert!(outcome.kueue_admission.is_null());
    assert!(!outcome.evaluated);
    assert_eq!(outcome.unevaluated_detail, Some("boom"));
}

/// A pending Workload for THIS task is a blocking reason.
#[test]
fn a_pending_workload_for_this_task_blocks() {
    let outcome = kueue_gate(&observing(vec![row("run-1", "pending", Some("task-1"))]), "task-1");
    assert!(outcome.evaluated);
    assert_eq!(outcome.reasons, vec!["kueue_workload_pending"]);
    assert_eq!(outcome.kueue_workload["admission"], "pending");
    assert_eq!(outcome.kueue_admission["projection_state"], "observing");
}

/// The same projection, the other direction: an admitted Workload does not
/// contribute the pending reason.
#[test]
fn an_admitted_workload_does_not_report_pending() {
    let outcome = kueue_gate(
        &observing(vec![row("run-1", "admitted", Some("task-1"))]),
        "task-1",
    );
    assert_eq!(outcome.kueue_workload["admission"], "admitted");
    assert!(
        !outcome.reasons.contains(&"kueue_workload_pending"),
        "an admitted Workload is not queued, got {:?}",
        outcome.reasons
    );
}

/// **AC3, in isolation.** A row with no task attribution is counted and listed
/// but never becomes another task's evidence.
#[test]
fn an_unattributed_row_never_becomes_a_task_entry() {
    let projection = observing(vec![
        row("orphan-run", "pending", None),
        row("run-1", "admitted", Some("task-1")),
    ]);
    let outcome = kueue_gate(&projection, "task-1");
    assert_eq!(
        outcome.kueue_workload["task_run_id"], "run-1",
        "task-1 must see its OWN row, never the unattributed one"
    );
    assert!(
        !outcome.reasons.contains(&"kueue_workload_pending"),
        "an unattributed pending row must not strand a different task"
    );

    // A task with no row of its own sees no per-task block at all.
    let other = kueue_gate(&projection, "task-2");
    assert!(
        other.kueue_workload.is_null(),
        "an unattributed row must not attach itself to an unrelated task"
    );

    let listed = outcome.kueue_admission["pending_task_runs"]
        .as_array()
        .expect("pending_task_runs is an array");
    assert_eq!(listed.len(), 1);
    assert!(
        listed[0]["task_id"].is_null(),
        "an unattributable pending Workload is listed with a null task, never invented one"
    );
    assert_eq!(outcome.kueue_admission["without_task_run"], 1);
}
