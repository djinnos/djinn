use djinn_core::events::EventBus;
use djinn_core::models::task_attempt::TaskAttemptOutcome;

use crate::Database;
use crate::repositories::epic::EpicRepository;
use crate::repositories::task_attempt::{
    CreateTaskAttemptParams, TaskAttemptRepository, TerminalTaskAttemptParams,
};
use crate::repositories::test_support::{add_blocker_edge, close_task_at};

fn test_db() -> Database {
    Database::open_in_memory().unwrap()
}

async fn create_task(db: &Database) -> (String, String) {
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

fn new_attempt_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Every dispatch allocates `attempt_seq` from two concurrent writers (the
/// coordinator's dispatch-start and the slot supervisor's exact-attempt
/// insert), so simultaneous auto-allocations on one task race on
/// `(task_id, attempt_seq)`.  The loser must retry with a recomputed sequence
/// instead of surfacing the unique violation (incident m0ed: the supervisor's
/// lost race hard-failed the dispatch and wedged the respawn guard for the
/// full orphan-sweep window).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_auto_seq_allocation_retries_past_unique_race() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;

    let mut handles = Vec::new();
    for i in 0..10 {
        let db = db.clone();
        let task_id = task_id.clone();
        handles.push(tokio::spawn(async move {
            let repo = TaskAttemptRepository::new(db);
            let id = new_attempt_id();
            let dispatch_key = format!("{task_id}:worker:race-{i}");
            repo.create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: &dispatch_key,
                session_id: None,
                attempt_seq: None,
            })
            .await
        }));
    }

    let mut seqs = std::collections::HashSet::new();
    for handle in handles {
        let attempt = handle
            .await
            .expect("allocation task must not panic")
            .expect("concurrent auto allocation must retry past the (task_id, attempt_seq) race");
        assert!(
            seqs.insert(attempt.attempt_seq),
            "attempt_seq {} allocated twice",
            attempt.attempt_seq
        );
    }
    assert_eq!(seqs.len(), 10, "every concurrent insert must land a row");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_blocker_parent_summaries_orders_and_bounds() {
    let db = test_db();
    let (_pid, dependent_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db.clone());

    // Three additional tasks to act as closed blocker parents.
    let (_p1dep, p1) = create_task(&db).await;
    let (_p2dep, p2) = create_task(&db).await;
    let (_p3dep, p3) = create_task(&db).await;

    // Flip each parent to closed with distinct timestamps.
    close_task_at(&db, &p1, "2025-01-01T00:00:00Z").await;
    close_task_at(&db, &p2, "2025-03-01T00:00:00Z").await;
    close_task_at(&db, &p3, "2025-02-01T00:00:00Z").await;

    // Wire blocker edges: each parent blocks the dependent task.
    add_blocker_edge(&db, &dependent_id, &p1).await;
    add_blocker_edge(&db, &dependent_id, &p2).await;
    add_blocker_edge(&db, &dependent_id, &p3).await;

    // Seed a completed attempt for each parent so latest_completed_prompt_summary returns it.
    for (i, parent_id) in [&p1, &p2, &p3].iter().enumerate() {
        let attempt_id = new_attempt_id();
        repo.create_or_get_pending(CreateTaskAttemptParams {
            id: &attempt_id,
            task_id: parent_id,
            role: "worker",
            dispatch_key: &format!("dk-parent-{i}"),
            session_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();
        repo.advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt_id,
            outcome: TaskAttemptOutcome::Completed,
            pr_url: Some(&format!("https://example.com/pr/{i}")),
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some(&format!("parent summary {i}")),
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();
    }

    // Ordering: p2 (Mar) > p3 (Feb) > p1 (Jan).
    let parents = repo
        .completed_blocker_parent_summaries(&dependent_id, 5)
        .await
        .unwrap();
    assert_eq!(parents.len(), 3);
    assert_eq!(parents[0].task_id, p2);
    assert_eq!(parents[1].task_id, p3);
    assert_eq!(parents[2].task_id, p1);
    // Each parent carries its latest completed attempt summary.
    assert!(parents[0].latest_completed_attempt.is_some());
    assert_eq!(
        parents[0]
            .latest_completed_attempt
            .as_ref()
            .unwrap()
            .summary
            .as_deref(),
        Some("parent summary 1")
    );
    assert_eq!(
        parents[0]
            .latest_completed_attempt
            .as_ref()
            .unwrap()
            .pr_url
            .as_deref(),
        Some("https://example.com/pr/1")
    );

    // Bounds: limit to 2 returns the two newest.
    let limited = repo
        .completed_blocker_parent_summaries(&dependent_id, 2)
        .await
        .unwrap();
    assert_eq!(limited.len(), 2);
    assert_eq!(limited[0].task_id, p2);
    assert_eq!(limited[1].task_id, p3);

    // Zero/negative limit returns empty.
    assert!(
        repo.completed_blocker_parent_summaries(&dependent_id, 0)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_blocker_parent_summaries_excludes_non_completed_parents() {
    let db = test_db();
    let (_pid, dependent_id) = create_task(&db).await;
    let (_pdep, open_parent) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db.clone());

    // The open_parent stays in `open` status — should be excluded.
    add_blocker_edge(&db, &dependent_id, &open_parent).await;

    let parents = repo
        .completed_blocker_parent_summaries(&dependent_id, 5)
        .await
        .unwrap();
    assert!(parents.is_empty(), "open blocker parent should be excluded");
}
