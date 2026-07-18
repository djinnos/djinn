//! Regression: `board_health` must stay bounded by live (non-closed) work.
//!
//! The 2026-07-17 restart loop: the mismatch-candidate pull scanned every
//! task on the board (closed history included, ~20 correlated subqueries per
//! row) and the review-queue section fetched every closed task ever — both on
//! every `board_health` call, which the UI polls continuously. As the closed
//! backlog grew the calls reached 6–9 s, starving the coordinator tick and
//! the liveness probe. Closed tasks must appear in NEITHER section.
use crate::database::Database;
use crate::repositories::epic::EpicCreateInput;
use crate::repositories::task::TaskRepository;
use djinn_core::events::EventBus;

async fn setup_project(db: &Database) -> (String, String) {
    let project_id = uuid::Uuid::now_v7().to_string();
    sqlx::query!(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
        project_id,
        "p",
        "test",
        format!("board-health-bounds-{project_id}"),
    )
    .execute(db.pool())
    .await
    .unwrap();

    let epic_repo = crate::repositories::epic::EpicRepository::new(db.clone(), EventBus::noop());
    let epic = epic_repo
        .create_for_project(
            &project_id,
            EpicCreateInput {
                title: "Board health epic",
                description: "",
                emoji: "",
                color: "",
                owner: "",
                memory_refs: None,
                status: None,
                auto_breakdown: None,
                originating_adr_id: None,
                blocked_by: None,
            },
        )
        .await
        .unwrap();
    (project_id, epic.id)
}

/// A closed task with heavy reopen churn and planner-toolset signals must NOT
/// surface as a role/tool mismatch — the report is advisory about live work,
/// and pulling closed history made the candidate scan unbounded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_mismatch_candidates_exclude_closed_tasks() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let (project_id, epic_id) = setup_project(&db).await;
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());

    // Two identical churn-heavy tasks whose text carries planner signals
    // ("task_create") while the dispatched role for a plain `task` is worker.
    let mut ids = Vec::new();
    for title in ["live mismatch", "closed mismatch"] {
        let task = task_repo
            .create_in_project(
                &project_id,
                Some(&epic_id),
                title,
                "this needs task_create and epic_update to proceed",
                "",
                "task",
                0,
                "",
                None,
                None,
            )
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE tasks SET total_reopen_count = 5 WHERE id = $1",
            task.id
        )
        .execute(db.pool())
        .await
        .unwrap();
        ids.push(task.id);
    }
    sqlx::query!("UPDATE tasks SET status = 'closed' WHERE id = $1", ids[1])
        .execute(db.pool())
        .await
        .unwrap();

    let health = task_repo.board_health(24).await.unwrap();
    let mismatches = health["repeated_reopen_role_tool_mismatches"]
        .as_array()
        .expect("mismatch section is an array");
    let listed: Vec<&str> = mismatches.iter().filter_map(|m| m["id"].as_str()).collect();
    assert!(
        listed.contains(&ids[0].as_str()),
        "the live churn-heavy task must be reported; got {listed:?}"
    );
    assert!(
        !listed.contains(&ids[1].as_str()),
        "a closed task must never surface as a mismatch candidate; got {listed:?}"
    );
}

/// The review queue lists work WAITING for review; closed tasks are history,
/// and including them fetched the entire closed backlog on every call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_review_queue_excludes_closed_tasks() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let (project_id, epic_id) = setup_project(&db).await;
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());

    let waiting = task_repo
        .create_in_project(
            &project_id,
            Some(&epic_id),
            "waiting for review",
            "",
            "",
            "task",
            0,
            "",
            None,
            None,
        )
        .await
        .unwrap();
    sqlx::query!(
        "UPDATE tasks SET status = 'needs_task_review' WHERE id = $1",
        waiting.id
    )
    .execute(db.pool())
    .await
    .unwrap();

    let done = task_repo
        .create_in_project(
            &project_id,
            Some(&epic_id),
            "already closed",
            "",
            "",
            "task",
            0,
            "",
            None,
            None,
        )
        .await
        .unwrap();
    sqlx::query!("UPDATE tasks SET status = 'closed' WHERE id = $1", done.id)
        .execute(db.pool())
        .await
        .unwrap();

    let health = task_repo.board_health(24).await.unwrap();
    let queue = health["review_queue"]
        .as_array()
        .expect("review_queue is an array");
    let listed: Vec<&str> = queue.iter().filter_map(|m| m["id"].as_str()).collect();
    assert!(
        listed.contains(&waiting.id.as_str()),
        "a needs_task_review task must be in the review queue; got {listed:?}"
    );
    assert!(
        !listed.contains(&done.id.as_str()),
        "closed tasks must not be in the review queue; got {listed:?}"
    );
}
