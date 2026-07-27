//! Tests for the rejected-submission integrity repository.

use djinn_core::events::EventBus;
use djinn_core::models::{RejectedVerdictKind, TaskRunTrigger};

use super::*;
use crate::repositories::epic::EpicRepository;
use crate::repositories::task_run::{CreateTaskRunParams, TaskRunRepository};

fn test_db() -> Database {
    Database::open_in_memory().unwrap()
}

async fn create_task(db: &Database, bus: EventBus) -> (String, String) {
    let epic = EpicRepository::new(db.clone(), bus)
        .create("Epic", "", "", "", "", None)
        .await
        .unwrap();

    let task_id = uuid::Uuid::now_v7().to_string();
    let short_id = format!("t{}{}", &task_id[..6], &task_id[task_id.len() - 6..]);
    let creator = crate::repositories::test_support::seed_test_user(db).await;
    sqlx::query!(
        "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                            issue_type, priority, owner, status, continuation_count, labels, acceptance_criteria, memory_refs, created_by_user_id)
         VALUES ($1, $2, $3, $4, 'Task', '', '', 'task', 0, '', 'open', 0, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $5)",
        task_id, epic.project_id, short_id, epic.id, creator
    )
    .execute(db.pool())
    .await
    .unwrap();

    (epic.project_id, task_id)
}

async fn create_run(db: &Database, project_id: &str, task_id: &str) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: &id,
            project_id,
            task_id,
            trigger_type: TaskRunTrigger::NewTask.as_str(),
            status: None,
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();
    id
}

fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

// ── TaskRejectedSubmissionIntegrityRepository tests ─────────────────────

#[allow(clippy::too_many_arguments)]
fn rejected_params<'a>(
    id: &'a str,
    task_id: &'a str,
    task_run_id: Option<&'a str>,
    review_id: Option<&'a str>,
    verdict_kind: &'a str,
    rejected_at: &'a str,
    diff_fingerprint: &'a str,
    no_progress_streak: i32,
) -> RecordTaskRejectedSubmissionParams<'a> {
    RecordTaskRejectedSubmissionParams {
        id,
        task_id,
        task_run_id,
        review_id,
        verdict_kind,
        activity_id: None,
        rejected_at,
        diff_fingerprint,
        no_progress_streak,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_integrity_latest_for_task_returns_none_when_no_history() {
    // The no-comparison historical case: a task with no recorded rejected
    // fingerprint must query as None, not fabricated state.
    let db = test_db();
    let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
    let _ = (project_id,);
    let repo = TaskRejectedSubmissionIntegrityRepository::new(db);

    let latest = repo.latest_for_task(&task_id).await.unwrap();
    assert!(
        latest.is_none(),
        "task with no rejected fingerprint must return None"
    );

    let streak = repo
        .latest_no_progress_streak_for_task(&task_id)
        .await
        .unwrap();
    assert_eq!(streak, 0, "missing history defaults to streak 0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_integrity_records_and_reloads_single_fingerprint() {
    let db = test_db();
    let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
    let task_run_id = create_run(&db, &project_id, &task_id).await;
    let repo = TaskRejectedSubmissionIntegrityRepository::new(db);

    let id = new_id();
    let created = repo
        .record(rejected_params(
            &id,
            &task_id,
            Some(&task_run_id),
            Some("review-001"),
            RejectedVerdictKind::NoProgress.as_str(),
            "2025-01-15T10:30:00.000Z",
            "sha256:abc123",
            1,
        ))
        .await
        .unwrap();

    assert_eq!(created.id, id);
    assert_eq!(created.task_id, task_id);
    assert_eq!(created.task_run_id.as_deref(), Some(task_run_id.as_str()));
    assert_eq!(created.review_id.as_deref(), Some("review-001"));
    assert_eq!(
        created.verdict_kind,
        RejectedVerdictKind::NoProgress.as_str()
    );
    assert_eq!(created.rejected_at, "2025-01-15T10:30:00.000Z");
    assert_eq!(created.diff_fingerprint, "sha256:abc123");
    assert_eq!(created.no_progress_streak, 1);
    assert!(!created.created_at.is_empty());

    let latest = repo
        .latest_for_task(&task_id)
        .await
        .unwrap()
        .expect("must exist after record");
    assert_eq!(latest.id, id);
    assert_eq!(latest.diff_fingerprint, "sha256:abc123");
    assert_eq!(latest.no_progress_streak, 1);

    let streak = repo
        .latest_no_progress_streak_for_task(&task_id)
        .await
        .unwrap();
    assert_eq!(streak, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_integrity_latest_for_task_picks_most_recent_by_rejected_at() {
    let db = test_db();
    let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
    let task_run_id = create_run(&db, &project_id, &task_id).await;
    let repo = TaskRejectedSubmissionIntegrityRepository::new(db.clone());

    // First (older) rejection.
    let first_id = new_id();
    repo.record(rejected_params(
        &first_id,
        &task_id,
        Some(&task_run_id),
        None,
        RejectedVerdictKind::NoProgress.as_str(),
        "2025-01-15T09:00:00.000Z",
        "sha256:older",
        1,
    ))
    .await
    .unwrap();

    // Second (newer) rejection on a different task_run — must win.
    let second_run_id = create_run(&db, &project_id, &task_id).await;
    let second_id = new_id();
    repo.record(rejected_params(
        &second_id,
        &task_id,
        Some(&second_run_id),
        None,
        RejectedVerdictKind::NoProgress.as_str(),
        "2025-01-15T11:00:00.000Z",
        "sha256:newer",
        2,
    ))
    .await
    .unwrap();

    let latest = repo
        .latest_for_task(&task_id)
        .await
        .unwrap()
        .expect("must exist");
    assert_eq!(
        latest.id, second_id,
        "latest must be the most recent rejection across task runs"
    );
    assert_eq!(latest.diff_fingerprint, "sha256:newer");
    assert_eq!(latest.no_progress_streak, 2);
    assert_eq!(latest.task_run_id.as_deref(), Some(second_run_id.as_str()));

    // list_for_task returns newest first.
    let rows = repo.list_for_task(&task_id).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, second_id);
    assert_eq!(rows[1].id, first_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_integrity_latest_for_task_is_scoped_per_task() {
    let db = test_db();
    let (_, task_a) = create_task(&db, EventBus::noop()).await;
    let (_, task_b) = create_task(&db, EventBus::noop()).await;
    let repo = TaskRejectedSubmissionIntegrityRepository::new(db);

    let id_a = new_id();
    repo.record(rejected_params(
        &id_a,
        &task_a,
        None,
        None,
        RejectedVerdictKind::NoProgress.as_str(),
        "2025-01-15T10:00:00.000Z",
        "sha256:fpa",
        1,
    ))
    .await
    .unwrap();

    // task_b has no history.
    let latest_b = repo.latest_for_task(&task_b).await.unwrap();
    assert!(latest_b.is_none(), "task_b must have no comparison state");

    // task_a still resolves to its own row.
    let latest_a = repo
        .latest_for_task(&task_a)
        .await
        .unwrap()
        .expect("must exist");
    assert_eq!(latest_a.diff_fingerprint, "sha256:fpa");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_integrity_streak_increments_across_rejections() {
    // Simulates the task-level no_progress_streak increment path: each
    // consecutive no-progress rejection records streak+1.
    let db = test_db();
    let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
    let task_run_id = create_run(&db, &project_id, &task_id).await;
    let repo = TaskRejectedSubmissionIntegrityRepository::new(db);

    // Initial rejection: streak 1.
    let id1 = new_id();
    repo.record(rejected_params(
        &id1,
        &task_id,
        Some(&task_run_id),
        None,
        RejectedVerdictKind::NoProgress.as_str(),
        "2025-01-15T09:00:00.000Z",
        "sha256:fp1",
        1,
    ))
    .await
    .unwrap();
    assert_eq!(
        repo.latest_no_progress_streak_for_task(&task_id)
            .await
            .unwrap(),
        1
    );

    // Second consecutive no-progress rejection: streak 2.
    let id2 = new_id();
    repo.record(rejected_params(
        &id2,
        &task_id,
        Some(&task_run_id),
        None,
        RejectedVerdictKind::NoProgress.as_str(),
        "2025-01-15T10:00:00.000Z",
        "sha256:fp2",
        2,
    ))
    .await
    .unwrap();
    assert_eq!(
        repo.latest_no_progress_streak_for_task(&task_id)
            .await
            .unwrap(),
        2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_integrity_streak_resets_to_zero() {
    let db = test_db();
    let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
    let task_run_id = create_run(&db, &project_id, &task_id).await;
    let repo = TaskRejectedSubmissionIntegrityRepository::new(db);

    // A prior rejection with streak 3.
    let id1 = new_id();
    repo.record(rejected_params(
        &id1,
        &task_id,
        Some(&task_run_id),
        None,
        RejectedVerdictKind::NoProgress.as_str(),
        "2025-01-15T09:00:00.000Z",
        "sha256:fpprior",
        3,
    ))
    .await
    .unwrap();
    assert_eq!(
        repo.latest_no_progress_streak_for_task(&task_id)
            .await
            .unwrap(),
        3
    );

    // Progress observed → reset streak to 0 via a fresh, progressed
    // fingerprint recorded at a later timestamp.
    let reset = repo
        .reset_no_progress_streak(
            &task_id,
            "sha256:fresh-progressed",
            "2025-01-15T11:00:00.000Z",
            Some(&task_run_id),
        )
        .await
        .unwrap();

    assert_eq!(reset.no_progress_streak, 0);
    assert_eq!(reset.diff_fingerprint, "sha256:fresh-progressed");
    assert_eq!(reset.verdict_kind, RejectedVerdictKind::NoProgress.as_str());

    // latest_for_task now observes the reset row.
    let latest = repo
        .latest_for_task(&task_id)
        .await
        .unwrap()
        .expect("must exist");
    assert_eq!(latest.no_progress_streak, 0);
    assert_eq!(latest.diff_fingerprint, "sha256:fresh-progressed");
    assert_eq!(
        repo.latest_no_progress_streak_for_task(&task_id)
            .await
            .unwrap(),
        0
    );

    // The audit trail is preserved (the prior streak=3 row is still there).
    let rows = repo.list_for_task(&task_id).await.unwrap();
    assert_eq!(rows.len(), 2, "reset must be append-only");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_integrity_persists_across_task_run_boundaries() {
    // The cross-task-run reload case: a rejection recorded in one task_run
    // is reloadable by task_id in a fresh task_run.
    let db = test_db();
    let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
    let first_run_id = create_run(&db, &project_id, &task_id).await;
    let repo = TaskRejectedSubmissionIntegrityRepository::new(db.clone());

    let id = new_id();
    repo.record(rejected_params(
        &id,
        &task_id,
        Some(&first_run_id),
        Some("review-orig"),
        RejectedVerdictKind::ReviewerReject.as_str(),
        "2025-01-15T09:00:00.000Z",
        "sha256:rejected-fp",
        1,
    ))
    .await
    .unwrap();

    // A new task run is created (redispatch boundary).
    let second_run_id = create_run(&db, &project_id, &task_id).await;
    let repo_new_run = TaskRejectedSubmissionIntegrityRepository::new(db);

    let latest = repo_new_run
        .latest_for_task(&task_id)
        .await
        .unwrap()
        .expect("must reload across task-run boundary");
    assert_eq!(latest.diff_fingerprint, "sha256:rejected-fp");
    assert_eq!(
        latest.verdict_kind,
        RejectedVerdictKind::ReviewerReject.as_str()
    );
    assert_eq!(latest.review_id.as_deref(), Some("review-orig"));
    // The original task_run association survives.
    assert_eq!(latest.task_run_id.as_deref(), Some(first_run_id.as_str()));
    // The new task run has no bearing on the lookup.
    assert_ne!(latest.task_run_id.as_deref(), Some(second_run_id.as_str()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_integrity_null_optional_fields() {
    let db = test_db();
    let (_, task_id) = create_task(&db, EventBus::noop()).await;
    let repo = TaskRejectedSubmissionIntegrityRepository::new(db);

    let id = new_id();
    let created = repo
        .record(rejected_params(
            &id,
            &task_id,
            None,
            None,
            RejectedVerdictKind::Looping.as_str(),
            "2025-01-15T10:00:00.000Z",
            "sha256:looping-fp",
            0,
        ))
        .await
        .unwrap();

    assert!(created.task_run_id.is_none());
    assert!(created.review_id.is_none());
    assert!(created.activity_id.is_none());
    assert_eq!(created.verdict_kind, RejectedVerdictKind::Looping.as_str());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_integrity_get_returns_none_for_missing() {
    let db = test_db();
    let repo = TaskRejectedSubmissionIntegrityRepository::new(db);

    let missing = repo.get("nonexistent-id").await.unwrap();
    assert!(missing.is_none());
}
