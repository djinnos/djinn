use std::sync::Arc;
use std::time::Duration;

use super::*;
use djinn_db::{CreateSessionParams, CreateTaskExecutionSessionParams, SessionRepository};

async fn guarded_fixture() -> (
    Database,
    Arc<TaskRepository>,
    Arc<SessionRepository>,
    String,
    String,
    String,
    i64,
) {
    let db = create_test_db();
    let events = noop_events();
    let epic = make_epic(&db, events.clone()).await;
    let tasks = Arc::new(TaskRepository::new(db.clone(), events.clone()));
    let task = open_task(&tasks, &epic.id).await;
    let task_run_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status) \
         VALUES ($1, $2, $3, 'dispatch', 'starting')",
    )
    .bind(&task_run_id)
    .bind(&task.project_id)
    .bind(&task.id)
    .execute(db.pool())
    .await
    .unwrap();

    let generation = tasks.allocate_execution_generation(&task.id).await.unwrap();
    let sessions = Arc::new(SessionRepository::new(db.clone(), events));
    (
        db,
        tasks,
        sessions,
        task.id,
        task.project_id,
        task_run_id,
        generation,
    )
}

fn guarded_params<'a>(
    task_id: &'a str,
    project_id: &'a str,
    task_run_id: &'a str,
    generation: i64,
) -> CreateTaskExecutionSessionParams<'a> {
    CreateTaskExecutionSessionParams {
        task_id,
        execution_generation: generation,
        session: CreateSessionParams {
            project_id,
            task_id: Some(task_id),
            model: "test-model",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(task_run_id),
            pricing: None,
            cost_basis: None,
        },
    }
}

/// Wait until `expected` repository operations are queued for a lock in this
/// test database. PostgreSQL may report the first waiter as the direct blocker
/// of later waiters, so counting only operations directly blocked by the
/// transaction holding the task row misses valid lock queues.
async fn wait_for_blocked_operations(db: &Database, expected: i64) {
    for _ in 0..100 {
        let blocked: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_stat_activity \
             WHERE datname = current_database() \
               AND cardinality(pg_blocking_pids(pid)) > 0",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        if blocked >= expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {expected} operations behind task-row lock");
}

async fn hold_task_row_lock<'a>(
    db: &'a Database,
    task_id: &str,
) -> sqlx::Transaction<'a, sqlx::Postgres> {
    let mut tx = db.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM tasks WHERE id = $1 FOR UPDATE")
        .bind(task_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guarded_create_lock_first_commits_before_later_fence() {
    let (db, tasks, sessions, task_id, project_id, task_run_id, generation) =
        guarded_fixture().await;
    let lock = hold_task_row_lock(&db, &task_id).await;

    let create_repo = sessions.clone();
    let create_task_id = task_id.clone();
    let create_project_id = project_id.clone();
    let create_run_id = task_run_id.clone();
    let create = tokio::spawn(async move {
        create_repo
            .create_task_execution_session(guarded_params(
                &create_task_id,
                &create_project_id,
                &create_run_id,
                generation,
            ))
            .await
    });
    wait_for_blocked_operations(&db, 1).await;

    let fence_repo = tasks.clone();
    let fence_task_id = task_id.clone();
    let fence = tokio::spawn(async move {
        fence_repo
            .fence_execution_generation_for_kill(&fence_task_id)
            .await
    });
    wait_for_blocked_operations(&db, 2).await;
    lock.commit().await.unwrap();

    let (created, fenced_generation) = tokio::join!(create, fence);
    let created = created.unwrap().unwrap();
    assert_eq!(fenced_generation.unwrap().unwrap(), generation + 1);
    assert_eq!(created.task_id.as_deref(), Some(task_id.as_str()));

    let session_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE id = $1")
        .bind(&created.id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        session_count, 1,
        "the committed pre-fence session remains visible"
    );
    let reconciled = sessions
        .reread_non_terminal_for_task(&task_id)
        .await
        .unwrap();
    assert_eq!(
        reconciled
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        vec![created.id.as_str()],
        "the complete reconciliation listing retains the exact pre-fence session"
    );
    let task_run_status: String = sqlx::query_scalar("SELECT status FROM task_runs WHERE id = $1")
        .bind(&task_run_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(task_run_status, "running");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fence_lock_first_rejects_stale_guarded_create_without_side_effects() {
    let (db, tasks, sessions, task_id, project_id, task_run_id, generation) =
        guarded_fixture().await;
    let lock = hold_task_row_lock(&db, &task_id).await;

    let fence_repo = tasks.clone();
    let fence_task_id = task_id.clone();
    let fence = tokio::spawn(async move {
        fence_repo
            .fence_execution_generation_for_kill(&fence_task_id)
            .await
    });
    wait_for_blocked_operations(&db, 1).await;

    let create_repo = sessions.clone();
    let create_task_id = task_id.clone();
    let create_project_id = project_id.clone();
    let create_run_id = task_run_id.clone();
    let create = tokio::spawn(async move {
        create_repo
            .create_task_execution_session(guarded_params(
                &create_task_id,
                &create_project_id,
                &create_run_id,
                generation,
            ))
            .await
    });
    wait_for_blocked_operations(&db, 2).await;
    lock.commit().await.unwrap();

    let (fenced_generation, created) = tokio::join!(fence, create);
    assert_eq!(fenced_generation.unwrap().unwrap(), generation + 1);
    assert!(matches!(
        created.unwrap(),
        Err(Error::DispatchGenerationRevoked {
            task_id: rejected_task_id,
            supplied_generation,
            current_generation,
        }) if rejected_task_id == task_id
            && supplied_generation == generation
            && current_generation == generation + 1
    ));

    let session_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE task_id = $1")
        .bind(&task_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        session_count, 0,
        "stale creation must not insert a session row"
    );
    let task_run_status: String = sqlx::query_scalar("SELECT status FROM task_runs WHERE id = $1")
        .bind(&task_run_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(task_run_status, "starting");
}

#[tokio::test]
async fn non_task_session_creation_remains_unrestricted() {
    let (_db, _tasks, sessions, _task_id, project_id, _task_run_id, _generation) =
        guarded_fixture().await;

    let created = sessions
        .create(CreateSessionParams {
            project_id: &project_id,
            task_id: None,
            model: "chat-model",
            agent_type: "chat",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    assert!(created.task_id.is_none());
    assert_eq!(created.status, "running");
}
