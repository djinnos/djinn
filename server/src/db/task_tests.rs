pub(super) use crate::events::{EventBus, event_bus_for};
pub(super) use crate::test_helpers;
pub(super) use djinn_core::models::{Task, TaskStatus, TransitionAction};
pub(super) use djinn_db::Database;
pub(super) use djinn_db::EpicRepository;
pub(super) use djinn_db::Error;
pub(super) use djinn_db::{ActivityQuery, CountQuery, ListQuery, ReadyQuery, TaskRepository};
pub(super) use rstest::rstest;
pub(super) use tokio::sync::broadcast;

mod closed_task_sync;
mod existing;
mod filter_matrices;
mod state_machine;
mod sync_terminal_state;

async fn make_epic(db: &Database, events: EventBus) -> djinn_core::models::Epic {
    EpicRepository::new(db.clone(), events)
        .create("Test Epic", "", "", "", "", None)
        .await
        .unwrap()
}

async fn open_task(repo: &TaskRepository, epic_id: &str) -> Task {
    let task = repo
        .create(epic_id, "T", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    repo.update(
        &task.id,
        "T",
        "",
        "",
        0,
        "",
        "",
        r#"[{"description":"default","met":false}]"#,
    )
    .await
    .unwrap()
}

fn make_peer_task(
    id: &str,
    project_id: &str,
    epic_id: &str,
    status: &str,
    updated_at: &str,
) -> Task {
    Task {
        id: id.to_string(),
        project_id: project_id.to_string(),
        short_id: format!("p{}", &id[..3]),
        epic_id: Some(epic_id.to_string()),
        title: "Peer Task".to_string(),
        description: String::new(),
        design: String::new(),
        issue_type: "task".to_string(),
        status: status.to_string(),
        priority: 0,
        owner: String::new(),
        labels: "[]".to_string(),
        acceptance_criteria: "[]".to_string(),
        reopen_count: 0,
        continuation_count: 0,
        verification_failure_count: 0,
        created_at: "2026-01-01T00:00:00.000Z".to_string(),
        updated_at: updated_at.to_string(),
        closed_at: if status == "closed" {
            Some(updated_at.to_string())
        } else {
            None
        },
        close_reason: if status == "closed" {
            Some("completed".to_string())
        } else {
            None
        },
        merge_commit_sha: None,
        pr_url: None,
        merge_conflict_metadata: None,
        memory_refs: "[]".to_string(),
        agent_type: None,
        unresolved_blocker_count: 0,
        total_reopen_count: 0,
        total_verification_failure_count: 0,
        intervention_count: 0,
        last_intervention_at: None,
    }
}
