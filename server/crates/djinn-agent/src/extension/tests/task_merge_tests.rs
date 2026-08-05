//! Regression coverage for the reviewer-rejection paused-session writer.

use crate::task_merge::interrupt_paused_worker_session;
use crate::test_helpers::{
    agent_context_from_db, create_test_db, create_test_epic, create_test_project, create_test_task,
};
use djinn_core::events::EventBus;
use djinn_core::models::{SessionFailureCause, SessionStatus};
use djinn_db::{CreateSessionParams, SessionRepository, TaskRepository};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn reviewer_rejection_cleanup_interrupts_paused_session_without_storing_diagnostic() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let events = EventBus::noop();
    let sessions = SessionRepository::new(db.clone(), events.clone());
    let session = sessions
        .create(CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("create paused worker session");
    sessions
        .pause(&session.id, 17, 29)
        .await
        .expect("park worker before reviewer rejection");

    let diagnostic = "review rejected; upstream Authorization: Bearer djinn_test_credential_7d4b";
    let activity = TaskRepository::new(db.clone(), events)
        .log_activity(
            Some(&task.id),
            "reviewer",
            "task_reviewer",
            "review_rejected",
            &serde_json::json!({ "reason": diagnostic }).to_string(),
        )
        .await
        .expect("record reviewer diagnostic as activity evidence");
    assert!(activity.payload.contains(diagnostic));

    let context = agent_context_from_db(db.clone(), CancellationToken::new());
    interrupt_paused_worker_session(&task.id, &context).await;

    let persisted = sessions
        .get(&session.id)
        .await
        .expect("reread settled session")
        .expect("session remains durable");
    assert_eq!(persisted.status, SessionStatus::Interrupted.as_str());
    assert_eq!(
        persisted.failure_cause,
        Some(SessionFailureCause::Cancelled)
    );
    assert!(persisted.ended_at.is_some());
    let session_json = serde_json::to_string(&persisted).expect("session serializes");
    assert!(
        !session_json.contains(diagnostic),
        "credential-shaped reviewer diagnostic belongs only to activity/log evidence, never sessions"
    );
    let activity_rows = TaskRepository::new(db, EventBus::noop())
        .list_activity(&task.id)
        .await
        .expect("reread activity evidence");
    assert!(
        activity_rows
            .iter()
            .any(|row| row.payload.contains(diagnostic))
    );
}
