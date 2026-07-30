use std::sync::Arc;

use super::*;

async fn generation_task() -> (Database, Arc<TaskRepository>, String) {
    let db = create_test_db();
    let events = noop_events();
    let epic = make_epic(&db, events.clone()).await;
    let repo = Arc::new(TaskRepository::new(db.clone(), events));
    let task = open_task(&repo, &epic.id).await;
    (db, repo, task.id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allocation_and_kill_fence_are_sequential_and_committed() {
    let (db, repo, task_id) = generation_task().await;
    let initial: i64 = sqlx::query_scalar("SELECT execution_generation FROM tasks WHERE id = $1")
        .bind(&task_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(initial, 0, "never-run tasks start at generation zero");

    assert_eq!(
        repo.allocate_execution_generation(&task_id).await.unwrap(),
        1
    );
    assert_eq!(
        repo.fence_execution_generation_for_kill(&task_id)
            .await
            .unwrap(),
        2
    );
    let committed: i64 = sqlx::query_scalar("SELECT execution_generation FROM tasks WHERE id = $1")
        .bind(&task_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(committed, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_generation_calls_serialize_without_duplicates() {
    let (_db, repo, task_id) = generation_task().await;
    let left_repo = repo.clone();
    let right_repo = repo.clone();
    let left_task_id = task_id.clone();
    let (left, right) = tokio::join!(
        async move { left_repo.allocate_execution_generation(&left_task_id).await },
        async move {
            right_repo
                .fence_execution_generation_for_kill(&task_id)
                .await
        },
    );
    let mut generations = vec![left.unwrap(), right.unwrap()];
    generations.sort_unstable();
    assert_eq!(generations, vec![1, 2]);
}

#[tokio::test]
async fn generation_operations_report_missing_tasks() {
    let db = create_test_db();
    let repo = TaskRepository::new(db, noop_events());
    let error = repo
        .allocate_execution_generation("missing-task")
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::InvalidData(message) if message == "task not found: missing-task")
    );
}
