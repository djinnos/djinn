//! Regression coverage for evidence-backed environmental interruptions.
//!
//! A worker/reviewer session killed by infrastructure leaves the task
//! dispatch-ready again for the same role. The reappearance path normally counts
//! that as a failure. Only the complete durable owner-expiry evidence tuple may
//! spare it; bare `interrupted` rows and genuine crashes still count.

use super::*;
use djinn_core::models::task_attempt::TaskAttemptOutcome;
use djinn_db::{TaskAttemptRepository, TerminalTaskAttemptParams};

/// Seed a terminal `(task, role)` attempt as dispatch persistence would leave it.
async fn seed_terminal_attempt(
    db: &Database,
    task_id: &str,
    role: &str,
    outcome: TaskAttemptOutcome,
    dispatch_owner_incarnation_id: Option<&str>,
    summary_json: Option<&str>,
) {
    let repo = TaskAttemptRepository::new(db.clone());
    let id = uuid::Uuid::now_v7().to_string();
    let dispatch_key = format!("{task_id}:{role}:{id}");
    repo.create_or_get_pending(djinn_db::CreateTaskAttemptParams {
        id: &id,
        task_id,
        role,
        dispatch_key: &dispatch_key,
        session_id: None,
        attempt_seq: None,
        dispatch_owner_incarnation_id,
        dispatch_group_id: None,
    })
    .await
    .unwrap();
    repo.advance_to_terminal(TerminalTaskAttemptParams {
        id: &id,
        outcome,
        pr_url: None,
        submit_ref: None,
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary: Some("seeded terminal attempt"),
        summary_json,
        log_tail: None,
    })
    .await
    .unwrap();
}

/// The actual same-role dispatch path evaluates one fully evidenced interruption
/// once: it re-dispatches immediately and emits exactly one exempted sample.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn environmental_interrupt_reappearance_dispatches_without_streak_or_cooldown() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _project_path) =
        create_simple_task(&db, &tx, "task", "reviewer interrupted by deploy").await;
    let task = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "needs_task_review")
        .await
        .unwrap();

    let owner = uuid::Uuid::now_v7().to_string();
    let evidence = serde_json::json!({
        "failure_class": "environmental_owner_expired",
        "owner_incarnation_id": owner,
        "owner_classification": "expired",
        "owner_lease_last_renewed_at": "2026-07-20T00:00:00Z",
    })
    .to_string();
    seed_terminal_attempt(
        &db,
        &task.id,
        "reviewer",
        TaskAttemptOutcome::Interrupted,
        Some(&owner),
        Some(&evidence),
    )
    .await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.last_dispatched.insert(
        task.id.clone(),
        DispatchMarker {
            instant: StdInstant::now(),
            role: "reviewer".to_owned(),
        },
    );

    let before = strike_decision_sample("exempted", "environmental_owner_expired");
    actor.dispatch_ready_tasks(None).await;
    let after = strike_decision_sample("exempted", "environmental_owner_expired");

    assert_eq!(
        actor.dispatched, 1,
        "an evidenced interruption must re-dispatch"
    );
    assert!(
        !actor.dispatch_failure_streak.contains_key(&task.id),
        "an evidenced interruption must not add a dispatch-failure streak"
    );
    assert!(
        !actor.dispatch_cooldowns.contains_key(&task.id),
        "an evidenced interruption must not apply an escalating cooldown"
    );
    assert_eq!(
        after - before,
        1.0,
        "the same-role retry path must emit exactly one exempted decision"
    );
}

/// A generic `interrupted` row has no durable owner-expiry proof. The actual
/// same-role dispatch path must count it once, rather than using outcome alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bare_interrupt_reappearance_counts_once_with_streak_and_cooldown() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _project_path) =
        create_simple_task(&db, &tx, "task", "reviewer bare interrupted").await;
    let task = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "needs_task_review")
        .await
        .unwrap();

    seed_terminal_attempt(
        &db,
        &task.id,
        "reviewer",
        TaskAttemptOutcome::Interrupted,
        None,
        None,
    )
    .await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.last_dispatched.insert(
        task.id.clone(),
        DispatchMarker {
            instant: StdInstant::now(),
            role: "reviewer".to_owned(),
        },
    );

    let before = strike_decision_sample("counted", "other_terminal");
    actor.dispatch_ready_tasks(None).await;
    let after = strike_decision_sample("counted", "other_terminal");

    assert_eq!(actor.dispatched, 0, "a bare interruption must back off");
    assert_eq!(
        actor.dispatch_failure_streak.get(&task.id).copied(),
        Some(1),
        "a bare interruption must consume a dispatch-failure strike"
    );
    assert!(
        actor.dispatch_cooldowns.contains_key(&task.id),
        "a bare interruption must apply an escalating cooldown"
    );
    assert_eq!(
        after - before,
        1.0,
        "the same-role retry path must emit exactly one counted decision"
    );
}

/// Control: a reviewer task whose prior session genuinely crashed is counted as
/// a same-role failure and therefore backs off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crashed_reappearance_still_counts_as_failure_with_streak_and_cooldown() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _project_path) =
        create_simple_task(&db, &tx, "task", "reviewer crashed for real").await;
    let task = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "needs_task_review")
        .await
        .unwrap();

    seed_terminal_attempt(
        &db,
        &task.id,
        "reviewer",
        TaskAttemptOutcome::Crashed,
        None,
        None,
    )
    .await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.last_dispatched.insert(
        task.id.clone(),
        DispatchMarker {
            instant: StdInstant::now(),
            role: "reviewer".to_owned(),
        },
    );

    actor.dispatch_ready_tasks(None).await;

    assert_eq!(
        actor.dispatched, 0,
        "a same-role crash reappearance backs off rather than dispatching this pass"
    );
    assert_eq!(
        actor.dispatch_failure_streak.get(&task.id).copied(),
        Some(1),
        "a genuine crash reappearance must advance the dispatch-failure streak"
    );
    assert!(
        actor.dispatch_cooldowns.contains_key(&task.id),
        "a genuine crash reappearance must apply an escalating cooldown"
    );
}

/// Read one bounded strike-decision series from the process telemetry recorder.
/// A delta around the actual dispatch invocation proves that this test's
/// reappearance evaluation emits one (and only one) decision sample.
fn strike_decision_sample(decision: &str, source: &str) -> f64 {
    let rendered = djinn_telemetry::render().unwrap();
    let prefix = format!(
        "djinn_dispatch_strike_decisions_total{{decision=\"{decision}\",source=\"{source}\"}} "
    );
    rendered
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("missing strike-decision series {decision}/{source}"))
        .parse()
        .unwrap()
}
