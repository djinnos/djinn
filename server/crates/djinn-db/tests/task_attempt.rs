//! Integration tests for the task attempt repository.
//!
//! These tests exercise `djinn-db` and `djinn-core` domain logic only. They
//! require a live Dolt test instance (see `djinn_db::Database::open_in_memory`).

pub(crate) use djinn_core::events::EventBus;
pub(crate) use djinn_core::models::task_attempt::{
    GuardDecision, GuardReason, TASK_ATTEMPT_DISPATCH_KEY_MAX_LEN, TASK_ATTEMPT_LOG_TAIL_MAX_LEN,
    TASK_ATTEMPT_SUMMARY_MAX_LEN, TaskAttemptOutcome,
};
pub(crate) use djinn_db::{
    CreateTaskAttemptParams, Database, DispatchGroupTerminalEvidence, EpicRepository,
    FillTaskAttemptParams, GuardDeferTaskAttemptParams, SubmitTaskAttemptParams,
    TaskAttemptRepository, TerminalTaskAttemptParams,
};
#[path = "task_attempt/create_and_lifecycle.rs"]
mod create_and_lifecycle;
#[path = "task_attempt/dispatch_group_terminalization.rs"]
mod dispatch_group_terminalization;
#[path = "task_attempt/infra_death_persistence.rs"]
mod infra_death_persistence;
#[path = "task_attempt/lookups_and_bounds.rs"]
mod lookups_and_bounds;

pub(crate) fn test_db() -> Database {
    Database::open_in_memory().unwrap()
}

pub(crate) async fn create_task(db: &Database) -> (String, String) {
    let epic_repo = EpicRepository::new(db.clone(), EventBus::noop());
    let epic = epic_repo
        .create("Epic", "", "", "", "", None)
        .await
        .unwrap();

    let task_id = uuid::Uuid::now_v7().to_string();
    let short_id = format!("t{}{}", &task_id[..6], &task_id[task_id.len() - 6..]);
    sqlx::query!(
        "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                            issue_type, priority, owner, status, continuation_count, labels, acceptance_criteria, memory_refs)
         VALUES ($1, $2, $3, $4, 'Task', '', '', 'task', 0, '', 'open', 0, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb)",
        task_id,
        epic.project_id,
        short_id,
        epic.id
    )
    .execute(db.pool())
    .await
    .unwrap();

    (epic.project_id, task_id)
}

pub(crate) fn new_attempt_id() -> String {
    uuid::Uuid::now_v7().to_string()
}
