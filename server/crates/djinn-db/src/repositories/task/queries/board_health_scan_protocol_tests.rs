use super::*;
use crate::database::Database;
use djinn_core::events::EventBus;

async fn setup_repo() -> (Database, TaskRepository, String) {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let project_id = uuid::Uuid::now_v7().to_string();
    sqlx::query!(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
        project_id,
        "p",
        "test",
        format!("scan-{project_id}")
    )
    .execute(db.pool())
    .await
    .unwrap();
    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    (db, repo, project_id)
}

async fn candidate(repo: &TaskRepository, db: &Database, project: &str, title: &str) -> String {
    let task = repo
        .create_fixture_in_project(
            project,
            None,
            title,
            "requires task_create",
            "",
            "task",
            0,
            "",
            None,
            None,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET total_reopen_count = 3 WHERE id = $1")
        .bind(&task.id)
        .execute(db.pool())
        .await
        .unwrap();
    task.id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_pass_restart_preserves_identity_then_completion_resets() {
    let (_db, repo, _project) = setup_repo().await;
    let started = repo
        .start_or_resume_board_health_mismatch_pass(1)
        .await
        .unwrap();
    assert!(started.active);
    assert!(started.eligible_high_water_id.is_none());
    let pass_id = started.pass_id.clone();
    let start_time = started.pass_started_at.clone();
    let restarted = TaskRepository::new(repo.db.clone(), EventBus::noop());
    let resumed = restarted
        .start_or_resume_board_health_mismatch_pass(2)
        .await
        .unwrap();
    assert_eq!(resumed.pass_id, pass_id);
    assert_eq!(resumed.pass_started_at, start_time);
    assert_eq!(resumed.leader_epoch, 2);
    assert!(
        restarted
            .load_board_health_mismatch_page(2)
            .await
            .unwrap()
            .candidates
            .is_empty()
    );
    let completed = restarted
        .complete_board_health_mismatch_pass(2)
        .await
        .unwrap();
    assert!(!completed.active);
    assert!(completed.completed_at.is_some());
    assert_ne!(
        restarted
            .start_or_resume_board_health_mismatch_pass(2)
            .await
            .unwrap()
            .pass_id,
        pass_id
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pages_are_bounded_retryable_guarded_fenced_and_reset_for_behind_cursor_rows() {
    let (db, repo, project) = setup_repo().await;
    let behind = repo
        .create_fixture_in_project(
            &project, None, "behind", "ordinary", "", "task", 0, "", None, None,
        )
        .await
        .unwrap()
        .id;
    for i in 0..251 {
        candidate(&repo, &db, &project, &format!("candidate {i}")).await;
    }
    let started = repo
        .start_or_resume_board_health_mismatch_pass(10)
        .await
        .unwrap();
    let first = repo.load_board_health_mismatch_page(10).await.unwrap();
    assert_eq!(
        first.candidates.len(),
        BOARD_HEALTH_MISMATCH_PAGE_SIZE as usize
    );
    let retry = repo.load_board_health_mismatch_page(10).await.unwrap();
    assert_eq!(retry.state.cursor_id, first.state.cursor_id);
    assert_eq!(
        retry.candidates.iter().map(|r| &r.id).collect::<Vec<_>>(),
        first.candidates.iter().map(|r| &r.id).collect::<Vec<_>>()
    );
    let last = first.candidates.last().unwrap().id.clone();
    assert!(
        repo.commit_board_health_mismatch_page(10, Some("wrong"), &last)
            .await
            .is_err()
    );
    repo.commit_board_health_mismatch_page(10, None, &last)
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET description = 'requires task_create', total_reopen_count = 3 WHERE id = $1").bind(&behind).execute(db.pool()).await.unwrap();
    let takeover = repo
        .start_or_resume_board_health_mismatch_pass(11)
        .await
        .unwrap();
    assert_eq!(takeover.pass_id, started.pass_id);
    // A stale coordinator cannot reclaim ownership after a newer epoch takes over.
    assert!(
        repo.start_or_resume_board_health_mismatch_pass(10)
            .await
            .is_err()
    );
    assert!(
        repo.commit_board_health_mismatch_page(10, Some(&last), &last)
            .await
            .is_err()
    );
    let second = repo.load_board_health_mismatch_page(11).await.unwrap();
    assert_eq!(second.candidates.len(), 1);
    assert!(repo.complete_board_health_mismatch_pass(11).await.is_err());
    repo.commit_board_health_mismatch_page(11, Some(&last), &second.candidates[0].id)
        .await
        .unwrap();
    assert!(
        repo.load_board_health_mismatch_page(11)
            .await
            .unwrap()
            .candidates
            .is_empty()
    );
    repo.complete_board_health_mismatch_pass(11).await.unwrap();
    repo.start_or_resume_board_health_mismatch_pass(11)
        .await
        .unwrap();
    assert!(
        repo.load_board_health_mismatch_page(11)
            .await
            .unwrap()
            .candidates
            .iter()
            .any(|row| row.id == behind)
    );
}
