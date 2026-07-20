//! Regression: `list_ready` (and `claim`) must project the required
//! `created_by_user_id` column. Omitting it from a `Task` projection must fail
//! decoding rather than silently creating a task without its creator. These
//! regressions ensure the dispatcher receives the persisted creator for
//! per-user model and credential resolution.
use super::*;
use crate::database::Database;
use crate::repositories::user::UserRepository;
use djinn_core::auth_context::SESSION_USER_ID;
use djinn_core::events::EventBus;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_ready_projects_created_by_user_id() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();

    let project_id = uuid::Uuid::now_v7().to_string();
    sqlx::query!(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
        project_id,
        "p",
        "test",
        format!("ready-projection-{project_id}"),
    )
    .execute(db.pool())
    .await
    .unwrap();

    // FK on tasks.created_by_user_id requires a real users row.
    let user = UserRepository::new(db.clone())
        .upsert_from_github(525252, "ready-projection-tester", Some("Tester"), None)
        .await
        .unwrap();
    let user_id = user.id.clone();

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    // Insert under SESSION_USER_ID so the row is stamped with the creator.
    let task_id = SESSION_USER_ID
        .scope(Some(user_id.clone()), async {
            repo.create_fixture_in_project(
                &project_id,
                None,
                "ready + attributed",
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
            .id
        })
        .await;

    let ready = repo
        .list_ready(ReadyQuery {
            project_id: Some(project_id.clone()),
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap();

    let got = ready
        .iter()
        .find(|t| t.id == task_id)
        .expect("a freshly-created open task must appear in list_ready");
    assert_eq!(
        got.created_by_user_id, user_id,
        "list_ready must SELECT the required created_by_user_id"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frozen_proposal_build_holds_its_epic_tasks_from_dispatch() {
    use crate::repositories::proposal::{ProposalCreateInput, ProposalRepository};

    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let project_id = uuid::Uuid::now_v7().to_string();
    sqlx::query!(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
        project_id,
        "p",
        "test",
        format!("frozen-{project_id}"),
    )
    .execute(db.pool())
    .await
    .unwrap();

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let prepo = ProposalRepository::new(db.clone(), EventBus::noop());

    // A proposal graduated into one epic, with a task under that epic.
    let proposal = prepo
        .create(ProposalCreateInput {
            title: "Frozen",
            body: "",
            acceptance_criteria: None,
            status: None,
            body_format: None,
        })
        .await
        .unwrap();
    let epic_id = uuid::Uuid::now_v7().to_string();
    sqlx::query!(
            "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, status, owner, memory_refs, auto_breakdown)
             VALUES ($1, $2, 'fz01', 'T', '', '', '', 'open', '', '[]'::jsonb, true)",
            epic_id,
            project_id
        )
        .execute(db.pool())
        .await
        .unwrap();
    prepo
        .link_epic(&proposal.id, &epic_id, &project_id)
        .await
        .unwrap();

    let epic_task = repo
        .create_fixture_in_project(
            &project_id,
            Some(&epic_id),
            "under epic",
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
    // An epic-less task (mirrors the epic_breakdown task: epic_id IS NULL).
    let loose_task = repo
        .create_fixture_in_project(
            &project_id,
            None,
            "no epic",
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

    async fn ready_ids(repo: &TaskRepository, project_id: &str) -> Vec<String> {
        repo.list_ready(ReadyQuery {
            project_id: Some(project_id.to_owned()),
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.id)
        .collect()
    }

    // Not frozen: both dispatchable.
    let before = ready_ids(&repo, &project_id).await;
    assert!(before.contains(&epic_task));
    assert!(before.contains(&loose_task));

    // Frozen: the epic's task is held; the epic-less task is unaffected.
    prepo.set_frozen(&proposal.id, true).await.unwrap();
    let frozen = ready_ids(&repo, &project_id).await;
    assert!(
        !frozen.contains(&epic_task),
        "frozen build's epic task must be held from dispatch"
    );
    assert!(
        frozen.contains(&loose_task),
        "epic-less task (epic_id NULL) must stay dispatchable"
    );

    // Un-freezing re-admits it.
    prepo.set_frozen(&proposal.id, false).await.unwrap();
    assert!(ready_ids(&repo, &project_id).await.contains(&epic_task));
}

/// Same projection gap, second query path: `list_by_status_filtered`
/// feeds the coordinator's `needs_task_review` / `needs_lead_intervention`
/// dispatch (reviewer / lead roles). It must also project
/// `created_by_user_id`, or those tasks dispatch creator-less and the
/// reviewer/lead role fails eligibility with "no eligible model for task
/// owner".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_by_status_filtered_projects_created_by_user_id() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();

    let project_id = uuid::Uuid::now_v7().to_string();
    sqlx::query!(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
        project_id,
        "p",
        "test",
        format!("status-projection-{project_id}"),
    )
    .execute(db.pool())
    .await
    .unwrap();

    let user = UserRepository::new(db.clone())
        .upsert_from_github(626262, "status-projection-tester", Some("Tester"), None)
        .await
        .unwrap();
    let user_id = user.id.clone();

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let task_id = SESSION_USER_ID
        .scope(Some(user_id.clone()), async {
            repo.create_fixture_in_project(
                &project_id,
                None,
                "needs review + attributed",
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
            .id
        })
        .await;

    // Park it in needs_task_review directly (the status the reviewer role
    // dispatches from); the projection, not the transition path, is the SUT.
    sqlx::query!(
        "UPDATE tasks SET status = 'needs_task_review' WHERE id = $1",
        task_id
    )
    .execute(db.pool())
    .await
    .unwrap();

    let in_review = repo
        .list_by_status_filtered("needs_task_review", true)
        .await
        .unwrap();

    let got = in_review
        .iter()
        .find(|t| t.id == task_id)
        .expect("the needs_task_review task must appear in list_by_status_filtered");
    assert_eq!(
        got.created_by_user_id, user_id,
        "list_by_status_filtered must SELECT the required created_by_user_id"
    );
}
