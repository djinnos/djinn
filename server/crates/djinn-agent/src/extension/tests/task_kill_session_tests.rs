//! Focused tests for the `task_kill_session` tool handler — the public
//! agent-side entry point for the user-driven "force-kill this session"
//! surface. The handler is intentionally a thin DB-settle (it transitions
//! any `paused` session row to `interrupted`) rather than a `pool.kill_session`
//! call: paused sessions are stored in the DB but do not own a live
//! K8s task-run Job (the worker pod has already terminated and the
//! session was parked for redispatch). A user-initiated `task_kill_session`
//! is therefore a state-cleanup, not a pod-kill.
//!
//! These tests pin the public contract:
//! - Unknown task id → `{"error": "..."}` response, no panic.
//! - Paused session present → handler returns ok and the session row
//!   transitions to `interrupted`.
//! - No paused session present → handler still returns ok (no-op).

use super::*;
use crate::extension::handlers::call_task_kill_session;
use djinn_core::models::SessionStatus;
use djinn_db::{CreateSessionParams, SessionRepository};

fn kill_args(task_id: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    Some(
        serde_json::json!({ "id": task_id })
            .as_object()
            .expect("kill_session args object")
            .clone(),
    )
}

#[tokio::test]
async fn call_task_kill_session_unknown_task_returns_error_payload_without_panic() {
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    let args = kill_args("nonexistent-019e");

    let result = call_task_kill_session(&state, &args)
        .await
        .expect("call_task_kill_session should not error on unknown task");
    assert_eq!(
        result.get("error").and_then(|v| v.as_str()).unwrap_or(""),
        "task not found: nonexistent-019e",
        "unknown task must return the documented error payload (got: {result})"
    );
}

#[tokio::test]
async fn call_task_kill_session_settles_paused_session_to_interrupted() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let task_id = task.id.clone();
    let session_repo = SessionRepository::new(db.clone(), EventBus::noop());
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task_id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("session create should succeed");
    // Park the session in `Paused` via the repository's `pause`
    // primitive (the `task_kill_session` handler targets paused rows
    // specifically — a running row is owned by the slot pool /
    // supervisor).
    let paused = session_repo
        .pause(&session.id, 0, 0)
        .await
        .expect("session pause should succeed");
    assert_eq!(paused.status, SessionStatus::Paused.as_str());

    let state = agent_context_from_db(db, CancellationToken::new());
    let args = kill_args(&task_id);
    let result = call_task_kill_session(&state, &args)
        .await
        .expect("call_task_kill_session should succeed");
    assert_eq!(
        result.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "kill_session on a known task must return ok=true (got: {result})"
    );
    assert_eq!(
        result.get("task_id").and_then(|v| v.as_str()),
        Some(task.short_id.as_str())
    );
    let after = SessionRepository::new(state.db.clone(), state.event_bus.clone())
        .get(&session.id)
        .await
        .expect("session get should succeed")
        .expect("session row should still exist");
    assert_eq!(
        after.status,
        SessionStatus::Interrupted.as_str(),
        "paused session must be settled to interrupted by the kill handler"
    );
}

#[tokio::test]
async fn call_task_kill_session_is_idempotent_on_already_interrupted_session() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let task_id = task.id.clone();
    let session_repo = SessionRepository::new(db.clone(), EventBus::noop());
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task_id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("session create should succeed");
    // Park as paused, then settle once. A second `task_kill_session`
    // call (e.g. the user double-clicked the button) must be a no-op:
    // no row exists in `paused_for_task`, the handler still returns
    // ok=true, and the session stays `interrupted` (not regressed to
    // `running` or any other state).
    session_repo
        .pause(&session.id, 0, 0)
        .await
        .expect("session pause should succeed");
    let state = agent_context_from_db(db, CancellationToken::new());
    let args = kill_args(&task_id);
    let _ = call_task_kill_session(&state, &args)
        .await
        .expect("first kill should succeed");
    let result2 = call_task_kill_session(&state, &args)
        .await
        .expect("second kill should also succeed (idempotent)");
    assert_eq!(
        result2.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "second kill on an already-interrupted session must still return ok=true"
    );
    let after = SessionRepository::new(state.db.clone(), state.event_bus.clone())
        .get(&session.id)
        .await
        .expect("session get should succeed")
        .expect("session row should still exist");
    assert_eq!(
        after.status,
        SessionStatus::Interrupted.as_str(),
        "session must stay interrupted across a duplicate kill"
    );
}
