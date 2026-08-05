use super::*;
use djinn_core::models::{SessionFailureCause, SessionStatus};
use djinn_db::{CreateSessionParams, CreateTaskRunParams, SessionRepository, TaskRunRepository};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconciliation_lists_and_settles_each_duplicate_session_by_exact_id() {
    let db = create_test_db();
    let events = noop_events();
    let epic = make_epic(&db, events.clone()).await;
    let tasks = TaskRepository::new(db.clone(), events.clone());
    let task = open_task(&tasks, &epic.id).await;
    let sessions = SessionRepository::new(db.clone(), events);

    let task_run_id = uuid::Uuid::now_v7().to_string();
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: &task_run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    let without_run = sessions
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    let with_run = sessions
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5-mini",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(&task_run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    // Equal timestamps make `id ASC` the required database tie-breaker.
    let tie_time = "2026-08-01T00:00:00.000Z";
    for session_id in [&without_run.id, &with_run.id] {
        sqlx::query("UPDATE sessions SET started_at = $1 WHERE id = $2")
            .bind(tie_time)
            .bind(session_id)
            .execute(db.pool())
            .await
            .unwrap();
    }

    let mut expected_ids = vec![without_run.id.clone(), with_run.id.clone()];
    expected_ids.sort();
    let listed = sessions.list_non_terminal_for_task(&task.id).await.unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>(),
        expected_ids
    );
    assert!(listed.iter().any(|session| session.task_run_id.is_none()));
    assert!(
        listed
            .iter()
            .any(|session| session.task_run_id.as_deref() == Some(&task_run_id))
    );

    assert!(
        sessions
            .settle_non_terminal_by_id(&without_run.id)
            .await
            .unwrap()
    );
    assert!(
        !sessions
            .settle_non_terminal_by_id(&without_run.id)
            .await
            .unwrap(),
        "a terminal named row must not be settled a second time"
    );

    let remaining = sessions
        .reread_non_terminal_for_task(&task.id)
        .await
        .unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, with_run.id);
    assert_eq!(remaining[0].status, SessionStatus::Running.as_str());

    let settled = sessions.get(&without_run.id).await.unwrap().unwrap();
    assert_eq!(settled.status, SessionStatus::Interrupted.as_str());
    assert!(settled.ended_at.is_some());
    assert_eq!(
        settled.failure_cause,
        Some(SessionFailureCause::Protocol),
        "the exact-id stale-board reconciliation backstop must persist Protocol"
    );

    assert!(
        sessions
            .settle_non_terminal_by_id(&with_run.id)
            .await
            .unwrap()
    );
    assert!(
        sessions
            .reread_non_terminal_for_task(&task.id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_terminal_update_persists_unknown_instead_of_a_null_cause() {
    let db = create_test_db();
    let events = noop_events();
    let epic = make_epic(&db, events.clone()).await;
    let tasks = TaskRepository::new(db.clone(), events.clone());
    let task = open_task(&tasks, &epic.id).await;
    let sessions = SessionRepository::new(db, events);
    let session = sessions
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    sessions
        .update(&session.id, SessionStatus::Interrupted, 0, 0, 0, 0, None)
        .await
        .unwrap();
    let persisted = sessions.get(&session.id).await.unwrap().unwrap();
    assert_eq!(persisted.status, SessionStatus::Interrupted.as_str());
    assert_eq!(
        persisted.failure_cause,
        Some(SessionFailureCause::Unknown),
        "legacy-compatible terminal writers must explicitly persist Unknown"
    );
}
