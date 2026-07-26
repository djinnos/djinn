//! Latest-row regressions for the coordinator's terminal PR safety gate.

use super::*;

const HANDOFF_REASON: &str = "terminal_close_deferred_pr_handoff";

fn assert_no_destructive_activity(activity: &[djinn_core::models::ActivityEntry]) {
    assert!(
        !activity.iter().any(|entry| {
            entry.payload.contains("force_close")
                || entry.payload.contains("cleanup")
                || entry.payload.contains("pr_cleanup")
        }),
        "PR handoff must not emit close or PR-cleanup activity: {activity:?}"
    );
}

/// A PR added after dispatch/retry captured its caller snapshot wins over that
/// stale snapshot. The terminal gate reloads the durable row and hands it to
/// the poller without creating remediation work or retaining backoff state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_gate_latest_pr_overrides_stale_prless_caller() {
    use djinn_db::{DispatchStateRepository, DispatchStateUpsert};

    const PR_URL: &str = "https://github.example/owner/repo/pull/9001";
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let (task, _) = create_task_with_note(&db, &tx, "terminal-latest-pr").await;
    let stale_caller = repo.get(&task.id).await.unwrap().unwrap();
    let task_count = repo.list_by_project(&task.project_id).await.unwrap().len();

    let state_repo = DispatchStateRepository::new(db.clone());
    state_repo
        .upsert(DispatchStateUpsert {
            task_id: &task.id,
            failure_streak: 3,
            cooldown_until: Some("2030-01-01T00:00:00Z"),
            escalation_count: 2,
            last_dispatched_at: Some("2029-12-31T23:59:00Z"),
            last_dispatched_role: Some("worker"),
            inflight_creator_user_id: Some("worker-owner"),
            inflight_model_id: Some("openai/gpt-5.5"),
        })
        .await
        .unwrap();
    repo.set_pr_url(&task.id, PR_URL).await.unwrap();

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    assert!(
        actor
            .terminally_fail_task(&stale_caller, "coordinator", "retry cap")
            .await
    );

    let latest = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(latest.status, "pr_review");
    assert_eq!(latest.pr_url.as_deref(), Some(PR_URL));
    assert_eq!(latest.owner, stale_caller.owner);
    assert_eq!(
        repo.list_by_project(&task.project_id).await.unwrap().len(),
        task_count
    );

    let state = state_repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(state.failure_streak, 0);
    assert!(state.cooldown_until.is_none());
    assert!(state.last_dispatched_at.is_none());
    assert!(state.last_dispatched_role.is_none());
    assert!(state.inflight_creator_user_id.is_none());
    assert!(state.inflight_model_id.is_none());

    let activity = repo.list_activity(&task.id).await.unwrap();
    assert!(
        activity
            .iter()
            .any(|entry| entry.payload.contains(HANDOFF_REASON))
    );
    assert_no_destructive_activity(&activity);
}

/// A PR removed after the caller captured a PR-bearing row must not block the
/// latest no-PR ForceClose path, including its running-session interruption.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_gate_latest_pr_removal_overrides_stale_pr_caller() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    const PR_URL: &str = "https://github.example/owner/repo/pull/9002";
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let (task, _) = create_task_with_note(&db, &tx, "terminal-latest-no-pr").await;
    repo.set_pr_url(&task.id, PR_URL).await.unwrap();
    let stale_caller = repo.get(&task.id).await.unwrap().unwrap();
    let sessions = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = sessions
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

    // Model a concurrent durable PR removal after the caller snapshot. The
    // terminal gate deliberately treats a blank durable URL as absent, so this
    // uses the existing repository seam without importing sqlx into this crate.
    repo.set_pr_url(&task.id, "").await.unwrap();

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    assert!(
        actor
            .terminally_fail_task(&stale_caller, "coordinator", "retry cap")
            .await
    );
    let latest = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(latest.status, "closed");
    assert!(
        latest
            .pr_url
            .as_deref()
            .is_none_or(|url| url.trim().is_empty()),
        "the latest durable URL is absent for terminal-gate purposes"
    );
    assert!(
        !sessions
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|active| active.id == session.id),
        "latest no-PR ForceClose must interrupt running sessions"
    );
    assert!(
        !repo
            .list_activity(&task.id)
            .await
            .unwrap()
            .iter()
            .any(|entry| entry.payload.contains(HANDOFF_REASON)),
        "latest no-PR path must not claim PR handoff"
    );
}

/// A task already owned by the review poller remains a no-op status transition,
/// while every terminal-gate invocation still leaves durable handoff evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_gate_pr_review_handoff_is_idempotent() {
    const PR_URL: &str = "https://github.example/owner/repo/pull/9003";
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let (task, _) = create_task_with_note(&db, &tx, "terminal-pr-review-idempotent").await;
    repo.set_pr_url(&task.id, PR_URL).await.unwrap();
    let poller_owned = repo.set_status(&task.id, "pr_review").await.unwrap();
    let task_count = repo.list_by_project(&task.project_id).await.unwrap().len();

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    assert!(
        actor
            .terminally_fail_task(&poller_owned, "coordinator", "retry cap")
            .await
    );
    assert!(
        actor
            .terminally_fail_task(&poller_owned, "coordinator", "retry cap")
            .await
    );

    let latest = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(latest.status, "pr_review");
    assert_eq!(latest.pr_url.as_deref(), Some(PR_URL));
    assert_eq!(latest.owner, poller_owned.owner);
    assert_eq!(
        repo.list_by_project(&task.project_id).await.unwrap().len(),
        task_count
    );
    let dispatch_state = djinn_db::DispatchStateRepository::new(db.clone())
        .get(&task.id)
        .await
        .unwrap()
        .expect("idempotent handoff must durably clear dispatch state");
    assert_eq!(dispatch_state.failure_streak, 0);
    assert!(dispatch_state.cooldown_until.is_none());
    assert!(dispatch_state.last_dispatched_at.is_none());
    assert!(dispatch_state.inflight_creator_user_id.is_none());

    let activity = repo.list_activity(&task.id).await.unwrap();
    assert_eq!(
        activity
            .iter()
            .filter(|entry| entry.payload.contains(HANDOFF_REASON))
            .count(),
        2,
        "each idempotent terminal handoff must be durably auditable"
    );
    assert_no_destructive_activity(&activity);
}
