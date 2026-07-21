use super::*;
use crate::database::Database;
use djinn_core::events::EventBus;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

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

#[derive(Default)]
struct PageQueryTracker {
    active: AtomicUsize,
    max_active: AtomicUsize,
    queries: AtomicUsize,
}

impl PageQueryTracker {
    async fn load(&self, repo: &TaskRepository, epoch: i64) -> BoardHealthMismatchPage {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        self.queries.fetch_add(1, Ordering::SeqCst);
        let page = repo.load_board_health_mismatch_page(epoch).await.unwrap();
        self.active.fetch_sub(1, Ordering::SeqCst);
        page
    }
}

/// Evaluate the large fixture while counting every actual repository page query.
async fn evaluate_pass_with_query_tracking(
    repo: &TaskRepository,
    epoch: i64,
    query_tracker: &PageQueryTracker,
) -> (Vec<String>, usize, usize) {
    let mut evaluated = Vec::new();
    let mut pages = 0;
    let mut max_retained = 0;
    loop {
        let page = query_tracker.load(repo, epoch).await;
        max_retained = max_retained.max(page.candidates.len());
        if page.candidates.is_empty() {
            repo.complete_board_health_mismatch_pass(epoch)
                .await
                .unwrap();
            return (evaluated, pages, max_retained);
        }
        pages += 1;
        let expected_cursor = page.state.cursor_id.clone();
        let last_id = page.candidates.last().unwrap().id.clone();
        evaluated.extend(page.candidates.into_iter().map(|candidate| candidate.id));
        repo.commit_board_health_mismatch_page(epoch, expected_cursor.as_deref(), &last_id)
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_page_high_water_and_behind_cursor_changes_are_deferred_to_next_pass() {
    let (db, repo, project) = setup_repo().await;
    let behind = repo
        .create_fixture_in_project(
            &project, None, "behind", "ordinary", "", "task", 0, "", None, None,
        )
        .await
        .unwrap()
        .id;
    let mut initial = Vec::new();
    for number in 0..501 {
        initial.push(candidate(&repo, &db, &project, &format!("initial {number}")).await);
    }

    repo.start_or_resume_board_health_mismatch_pass(20)
        .await
        .unwrap();
    let first = repo.load_board_health_mismatch_page(20).await.unwrap();
    assert_eq!(
        first.candidates.len(),
        BOARD_HEALTH_MISMATCH_PAGE_SIZE as usize
    );
    let first_ids: Vec<_> = first
        .candidates
        .iter()
        .map(|candidate| candidate.id.clone())
        .collect();
    let first_last = first.candidates.last().unwrap().id.clone();
    repo.commit_board_health_mismatch_page(20, None, &first_last)
        .await
        .unwrap();

    let above_high_water = candidate(&repo, &db, &project, "inserted after high-water").await;
    sqlx::query("UPDATE tasks SET description = 'requires task_create', total_reopen_count = 3 WHERE id = $1")
        .bind(&behind)
        .execute(db.pool())
        .await
        .unwrap();

    let (mut evaluated, pages, max_retained) = evaluate_pass(&repo, 20).await;
    evaluated.splice(0..0, first_ids);
    assert_eq!(pages + 1, 3);
    assert_eq!(max_retained, BOARD_HEALTH_MISMATCH_PAGE_SIZE as usize);
    assert_eq!(evaluated.len(), initial.len());
    assert_eq!(
        evaluated
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        initial.len()
    );
    assert!(!evaluated.contains(&above_high_water));
    assert!(!evaluated.contains(&behind));

    repo.start_or_resume_board_health_mismatch_pass(20)
        .await
        .unwrap();
    let (next_pass, next_pages, next_max_retained) = evaluate_pass(&repo, 20).await;
    assert_eq!(next_pages, 3);
    assert!(next_pass.contains(&above_high_water));
    assert!(next_pass.contains(&behind));
    assert!(next_max_retained <= BOARD_HEALTH_MISMATCH_PAGE_SIZE as usize);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_evaluation_keeps_cursor_and_repeats_the_exact_page_after_restart_takeover() {
    let (db, repo, project) = setup_repo().await;
    for number in 0..300 {
        candidate(&repo, &db, &project, &format!("retry {number}")).await;
    }
    let started = repo
        .start_or_resume_board_health_mismatch_pass(30)
        .await
        .unwrap();
    let failed_page = repo.load_board_health_mismatch_page(30).await.unwrap();
    let failed_ids: Vec<_> = failed_page
        .candidates
        .iter()
        .map(|candidate| candidate.id.clone())
        .collect();
    drop(failed_page); // Simulated evaluator failure: do not commit.
    let persisted_cursor: Option<String> = sqlx::query_scalar(
        "SELECT cursor_id FROM board_health_mismatch_scan_state WHERE singleton",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert!(persisted_cursor.is_none());

    let restarted = TaskRepository::new(db.clone(), EventBus::noop());
    let resumed = restarted
        .start_or_resume_board_health_mismatch_pass(31)
        .await
        .unwrap();
    assert_eq!(resumed.pass_id, started.pass_id);
    assert_eq!(resumed.leader_epoch, 31);
    let retry = restarted.load_board_health_mismatch_page(31).await.unwrap();
    assert_eq!(
        retry
            .candidates
            .iter()
            .map(|candidate| &candidate.id)
            .collect::<Vec<_>>(),
        failed_ids.iter().collect::<Vec<_>>()
    );
    let retry_last = retry.candidates.last().unwrap().id.clone();
    restarted
        .commit_board_health_mismatch_page(31, None, &retry_last)
        .await
        .unwrap();
    let (remaining, pages, max_retained) = evaluate_pass(&restarted, 31).await;
    assert_eq!(pages, 1);
    assert_eq!(remaining.len(), 50);
    assert!(max_retained <= BOARD_HEALTH_MISMATCH_PAGE_SIZE as usize);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ten_thousand_candidates_finish_in_forty_bounded_pages_under_two_minutes() {
    let (db, repo, project) = setup_repo().await;
    let creator = crate::repositories::test_support::seed_test_user(&db).await;
    sqlx::query(
        "INSERT INTO tasks (id, project_id, short_id, title, description, design, issue_type, status, labels, acceptance_criteria, memory_refs, total_reopen_count, created_by_user_id) \
         SELECT '00000000-0000-0000-0000-' || lpad(n::text, 12, '0'), $1, 'bulk-' || n, 'bulk candidate', 'requires task_create', '', 'task', 'open', '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, 3, $2 \
         FROM generate_series(1, 10000) AS n",
    )
    .bind(&project)
    .bind(&creator)
    .execute(db.pool())
    .await
    .unwrap();

    let started_at = Instant::now();
    repo.start_or_resume_board_health_mismatch_pass(40)
        .await
        .unwrap();
    let query_tracker = PageQueryTracker::default();
    let (evaluated, pages, max_retained) =
        evaluate_pass_with_query_tracking(&repo, 40, &query_tracker).await;
    assert_eq!(evaluated.len(), 10_000);
    assert_eq!(pages, 40);
    assert_eq!(max_retained, BOARD_HEALTH_MISMATCH_PAGE_SIZE as usize);
    // The tracked calls include the final empty completion read; all are
    // repository page queries and must remain single-flight.
    assert_eq!(query_tracker.queries.load(Ordering::SeqCst), 41);
    assert_eq!(query_tracker.max_active.load(Ordering::SeqCst), 1);
    assert!(started_at.elapsed() < Duration::from_secs(120));
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

/// Evaluate and commit a complete pass while retaining only its current page.
async fn evaluate_pass(repo: &TaskRepository, epoch: i64) -> (Vec<String>, usize, usize) {
    let mut evaluated = Vec::new();
    let mut pages = 0;
    let mut max_retained = 0;
    loop {
        let page = repo.load_board_health_mismatch_page(epoch).await.unwrap();
        max_retained = max_retained.max(page.candidates.len());
        if page.candidates.is_empty() {
            repo.complete_board_health_mismatch_pass(epoch)
                .await
                .unwrap();
            return (evaluated, pages, max_retained);
        }
        pages += 1;
        let expected_cursor = page.state.cursor_id.clone();
        let last_id = page.candidates.last().unwrap().id.clone();
        evaluated.extend(page.candidates.into_iter().map(|candidate| candidate.id));
        repo.commit_board_health_mismatch_page(epoch, expected_cursor.as_deref(), &last_id)
            .await
            .unwrap();
    }
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
