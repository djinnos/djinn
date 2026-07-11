//! Regression: tasks closed with `close_reason = parent_closed` must be terminal
//! but must NOT be classified as merged/completed work. This exercises both the
//! parent-disposition close path and the `merged` pseudo-status query classifier.
use super::*;
use crate::database::Database;
use crate::repositories::epic::EpicCreateInput;
use crate::repositories::task::{ListQuery, TaskRepository};
use djinn_core::events::EventBus;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_closed_child_is_not_counted_as_merged() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();

    let project_id = uuid::Uuid::now_v7().to_string();
    sqlx::query!(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
        project_id,
        "p",
        "test",
        format!("parent-closed-{project_id}"),
    )
    .execute(db.pool())
    .await
    .unwrap();

    let epic_repo = crate::repositories::epic::EpicRepository::new(db.clone(), EventBus::noop());
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());

    let epic = epic_repo
        .create_for_project(
            &project_id,
            EpicCreateInput {
                title: "Parent epic",
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

    let child = task_repo
        .create_in_project(
            &project_id,
            Some(&epic.id),
            "child task",
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

    // Sanity: the child starts open and is not in the merged set.
    let before = task_repo
        .list_filtered(ListQuery {
            project_id: Some(project_id.clone()),
            status: Some("merged".to_string()),
            issue_type: None,
            priority: None,
            label: None,
            text: None,
            parent: None,
            sort: "priority".to_string(),
            limit: 50,
            offset: 0,
        })
        .await
        .unwrap();
    assert!(before.tasks.is_empty(), "no merged tasks before close");

    // Close the epic, which applies parent disposition to the direct child.
    let _ = epic_repo.close(&epic.id).await.unwrap();

    let child = task_repo.get(&child.id).await.unwrap().unwrap();
    assert_eq!(child.status, "closed");
    assert_eq!(
        child.close_reason.as_deref(),
        Some("parent_closed"),
        "child must be closed with parent_closed reason"
    );

    // The merged pseudo-status query must not include this terminal child.
    let merged = task_repo
        .list_filtered(ListQuery {
            project_id: Some(project_id.clone()),
            status: Some("merged".to_string()),
            issue_type: None,
            priority: None,
            label: None,
            text: None,
            parent: None,
            sort: "priority".to_string(),
            limit: 50,
            offset: 0,
        })
        .await
        .unwrap();
    assert!(
        !merged.tasks.iter().any(|t| t.id == child.id),
        "parent_closed child must not appear in merged results"
    );

    // count_grouped by status='merged' should likewise return 0.
    let grouped = task_repo
        .count_grouped(CountQuery {
            project_id: Some(project_id.clone()),
            status: Some("merged".to_string()),
            issue_type: None,
            priority: None,
            label: None,
            text: None,
            parent: None,
            group_by: None,
        })
        .await
        .unwrap();
    assert_eq!(
        grouped["total_count"].as_i64(),
        Some(0),
        "parent_closed child must not count as merged"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn true_merged_classifications_still_count_as_merged() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();

    let project_id = uuid::Uuid::now_v7().to_string();
    sqlx::query!(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
        project_id,
        "p",
        "test",
        format!("merged-{project_id}"),
    )
    .execute(db.pool())
    .await
    .unwrap();

    let repo = TaskRepository::new(db.clone(), EventBus::noop());

    // merge_commit_sha IS NOT NULL
    let merged_sha = repo
        .create_in_project(
            &project_id,
            None,
            "merged by sha",
            "",
            "",
            "task",
            0,
            "",
            None,
            None,
        )
        .await
        .unwrap()
        .id;
    sqlx::query!(
        "UPDATE tasks SET status = 'closed', close_reason = 'completed', merge_commit_sha = 'abc123', closed_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $1",
        merged_sha
    )
    .execute(db.pool())
    .await
    .unwrap();

    // pr_url IS NOT NULL AND close_reason = 'completed'
    let merged_pr = repo
        .create_in_project(
            &project_id,
            None,
            "merged by pr",
            "",
            "",
            "task",
            0,
            "",
            None,
            None,
        )
        .await
        .unwrap()
        .id;
    sqlx::query!(
        "UPDATE tasks SET status = 'closed', close_reason = 'completed', pr_url = 'https://github.com/test/1', closed_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $1",
        merged_pr
    )
    .execute(db.pool())
    .await
    .unwrap();

    let merged = repo
        .list_filtered(ListQuery {
            project_id: Some(project_id.clone()),
            status: Some("merged".to_string()),
            issue_type: None,
            priority: None,
            label: None,
            text: None,
            parent: None,
            sort: "priority".to_string(),
            limit: 50,
            offset: 0,
        })
        .await
        .unwrap();
    let merged_ids: Vec<&str> = merged.tasks.iter().map(|t| t.id.as_str()).collect();
    assert!(
        merged_ids.contains(&merged_sha.as_str()),
        "merge_commit_sha task counts as merged"
    );
    assert!(
        merged_ids.contains(&merged_pr.as_str()),
        "completed PR task counts as merged"
    );
}
