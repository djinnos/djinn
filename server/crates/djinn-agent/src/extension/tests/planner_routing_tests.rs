//! Focused tests for the deprecated `request_lead` handler compatibility path
//! and `request_planner` handler.
//!
//! These tests verify the drain-compatibility behavior required by epic 10qg:
//! - A stale `request_lead` call logs a typed `deprecated_request_lead` activity.
//! - The handler does NOT transition the source task to `needs_lead_intervention`.
//! - When no coordinator is available, the handler returns a clear error
//!   (confirming the drain path reaches the dispatch point).
//! - `suggested_breakdown` is preserved in the activity body.
//! - `request_planner` logs a role-neutral planner-request activity.

use super::*;
use crate::extension::handlers::{call_request_lead, call_request_planner};
use djinn_db::TaskRepository;

fn request_lead_args(
    task_id: &str,
    reason: &str,
    suggested_breakdown: Option<&str>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let mut map = serde_json::json!({
        "id": task_id,
        "reason": reason,
    })
    .as_object()
    .expect("object")
    .clone();
    if let Some(bd) = suggested_breakdown {
        map.insert(
            "suggested_breakdown".to_string(),
            serde_json::Value::String(bd.to_string()),
        );
    }
    Some(map)
}

fn request_planner_args(
    task_id: &str,
    reason: &str,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    Some(
        serde_json::json!({
            "id": task_id,
            "reason": reason,
        })
        .as_object()
        .expect("object")
        .clone(),
    )
}

#[tokio::test]
async fn request_lead_unknown_task_returns_error_payload() {
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    let args = request_lead_args("nonexistent-task", "stuck", None);

    let result = call_request_lead(&state, &args)
        .await
        .expect("handler should not panic on unknown task");
    assert_eq!(
        result.get("error").and_then(|v| v.as_str()).unwrap_or(""),
        "task not found: nonexistent-task",
        "unknown task must return documented error payload (got: {result})"
    );
}

#[tokio::test]
async fn request_lead_logs_deprecated_request_lead_activity() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let task_id = task.id.clone();

    let state = agent_context_from_db(db.clone(), CancellationToken::new());
    let args = request_lead_args(&task_id, "task is too large for one session", None);

    // Call the handler — it will fail at coordinator dispatch (no coordinator
    // in test context), but the activity must have been logged by then.
    let result = call_request_lead(&state, &args)
        .await
        .expect("handler must not error");

    // The handler should indicate coordinator is unavailable (expected in tests).
    assert!(
        result.get("error").is_some(),
        "expected coordinator-unavailable error, got: {result}"
    );
    let err = result["error"].as_str().unwrap_or("");
    assert!(
        err.contains("coordinator not available"),
        "error must mention coordinator unavailability, got: {err}"
    );

    // Verify the typed deprecated_request_lead activity was logged.
    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let activities = repo
        .list_activity(&task_id)
        .await
        .expect("list_activity must succeed");

    let deprecated_activities: Vec<_> = activities
        .iter()
        .filter(|a| a.event_type == "deprecated_request_lead")
        .collect();
    assert_eq!(
        deprecated_activities.len(),
        1,
        "expected exactly one deprecated_request_lead activity, found {}; all activities: {:?}",
        deprecated_activities.len(),
        activities.iter().map(|a| &a.event_type).collect::<Vec<_>>()
    );

    let activity = deprecated_activities[0];
    let payload: serde_json::Value =
        serde_json::from_str(&activity.payload).expect("activity payload must be valid JSON");
    let body = payload["body"].as_str().unwrap_or("");
    assert!(
        body.contains("DEPRECATED"),
        "activity body must contain DEPRECATED marker, got: {body}"
    );
    assert!(
        body.contains("task is too large for one session"),
        "activity body must preserve the caller's reason, got: {body}"
    );
}

#[tokio::test]
async fn request_lead_preserves_suggested_breakdown_in_activity() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let task_id = task.id.clone();

    let state = agent_context_from_db(db.clone(), CancellationToken::new());
    let args = request_lead_args(
        &task_id,
        "too complex",
        Some("1. auth module\n2. API layer\n3. tests"),
    );

    let _ = call_request_lead(&state, &args)
        .await
        .expect("handler must not error");

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let activities = repo
        .list_activity(&task_id)
        .await
        .expect("list_activity must succeed");

    let deprecated = activities
        .iter()
        .find(|a| a.event_type == "deprecated_request_lead")
        .expect("deprecated_request_lead activity must exist");

    let payload: serde_json::Value = serde_json::from_str(&deprecated.payload).expect("valid JSON");
    let body = payload["body"].as_str().unwrap_or("");
    assert!(
        body.contains("Suggested breakdown:"),
        "activity body must contain suggested breakdown section, got: {body}"
    );
    assert!(
        body.contains("1. auth module"),
        "activity body must contain the breakdown content, got: {body}"
    );
}

#[tokio::test]
async fn request_lead_does_not_transition_task_to_needs_lead_intervention() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let task_id = task.id.clone();
    let original_status = task.status.clone();

    let state = agent_context_from_db(db.clone(), CancellationToken::new());
    let args = request_lead_args(&task_id, "stuck on implementation", None);

    let _ = call_request_lead(&state, &args)
        .await
        .expect("handler must not error");

    // Verify the task was NOT transitioned to needs_lead_intervention.
    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let updated_task = repo
        .get(&task_id)
        .await
        .expect("get must succeed")
        .expect("task must still exist");

    assert_ne!(
        updated_task.status, "needs_lead_intervention",
        "deprecated request_lead must NOT transition task to needs_lead_intervention; \
         status should still be '{original_status}', got: {}",
        updated_task.status
    );
    assert_eq!(
        updated_task.status, original_status,
        "task status must be unchanged after deprecated request_lead, got: {}",
        updated_task.status
    );
}

#[tokio::test]
async fn request_lead_logs_no_lead_request_comment() {
    // The deprecated path must NOT log a [LEAD_REQUEST] comment — that was
    // the old convention. It must log a typed `deprecated_request_lead` event.
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let task_id = task.id.clone();

    let state = agent_context_from_db(db.clone(), CancellationToken::new());
    let args = request_lead_args(&task_id, "needs help", None);

    let _ = call_request_lead(&state, &args)
        .await
        .expect("handler must not error");

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let activities = repo
        .list_activity(&task_id)
        .await
        .expect("list_activity must succeed");

    // No [LEAD_REQUEST] comment must appear.
    let lead_request_comments: Vec<_> = activities
        .iter()
        .filter(|a| {
            a.event_type == "comment"
                && serde_json::from_str::<serde_json::Value>(&a.payload)
                    .ok()
                    .and_then(|p| p["body"].as_str().map(|b| b.contains("[LEAD_REQUEST]")))
                    .unwrap_or(false)
        })
        .collect();
    assert!(
        lead_request_comments.is_empty(),
        "deprecated request_lead must NOT log [LEAD_REQUEST] comments; found: {:?}",
        lead_request_comments
            .iter()
            .map(|a| &a.payload)
            .collect::<Vec<_>>()
    );

    // Instead, a typed deprecated_request_lead activity must exist.
    let deprecated: Vec<_> = activities
        .iter()
        .filter(|a| a.event_type == "deprecated_request_lead")
        .collect();
    assert_eq!(
        deprecated.len(),
        1,
        "expected exactly one deprecated_request_lead activity"
    );
}

#[tokio::test]
async fn request_planner_unknown_task_returns_error_payload() {
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    let args = request_planner_args("nonexistent-task", "needs replan");

    let result = call_request_planner(&state, &args)
        .await
        .expect("handler should not panic on unknown task");
    assert_eq!(
        result.get("error").and_then(|v| v.as_str()).unwrap_or(""),
        "task not found: nonexistent-task",
        "unknown task must return documented error payload (got: {result})"
    );
}

#[tokio::test]
async fn request_planner_logs_planner_request_activity() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let task_id = task.id.clone();

    let state = agent_context_from_db(db.clone(), CancellationToken::new());
    let args = request_planner_args(&task_id, "blocked on external dependency");

    let result = call_request_planner(&state, &args)
        .await
        .expect("handler must not error");

    // Coordinator unavailable in test context — expected error.
    assert!(
        result.get("error").is_some(),
        "expected coordinator-unavailable error, got: {result}"
    );

    // Verify the role-neutral planner-request activity was logged.
    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let activities = repo
        .list_activity(&task_id)
        .await
        .expect("list_activity must succeed");

    let planner_comments: Vec<_> = activities
        .iter()
        .filter(|a| {
            a.event_type == "comment"
                && serde_json::from_str::<serde_json::Value>(&a.payload)
                    .ok()
                    .and_then(|p| p["body"].as_str().map(|b| b.contains("[PLANNER_REQUEST]")))
                    .unwrap_or(false)
        })
        .collect();
    assert_eq!(
        planner_comments.len(),
        1,
        "expected exactly one [PLANNER_REQUEST] comment, found {}; all activities: {:?}",
        planner_comments.len(),
        activities.iter().map(|a| &a.event_type).collect::<Vec<_>>()
    );

    let payload: serde_json::Value =
        serde_json::from_str(&planner_comments[0].payload).expect("valid JSON");
    let body = payload["body"].as_str().unwrap_or("");
    assert!(
        body.contains("blocked on external dependency"),
        "planner request must preserve caller's reason, got: {body}"
    );
}
