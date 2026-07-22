//! Regression: `list_ready` (and `claim`) must project the
//! `#[sqlx(default)]` columns the coordinator relies on — chiefly
//! `created_by_user_id`. Because that column is `#[sqlx(default)]` on
//! `Task`, omitting it from the SELECT does NOT error; it silently yields
//! `None`. That made the dispatcher treat every ready task as creator-less,
//! collapsing per-user model + credential resolution to the org-shared
//! fallback and emitting "no eligible model for task owner" for tasks whose
//! owner had a perfectly good per-user model/provider configured.
use super::*;
use crate::database::Database;
use crate::repositories::user::UserRepository;
use djinn_core::auth_context::SESSION_USER_ID;
use djinn_core::events::EventBus;
use djinn_core::models::TaskRefinementCorrelation;
use djinn_core::refinement_liveness::{RefinementPhase, RefinementRole};

fn assert_adversary_correlation(task: &djinn_core::models::Task, run_id: &str, intent_id: &str) {
    let correlation = task
        .refinement_correlation()
        .expect("ready projection must contain a complete valid correlation")
        .expect("ready projection must not collapse a correlated task to ordinary");
    assert_eq!(correlation.run_id(), run_id);
    assert_eq!(correlation.intent_id(), intent_id);
    assert_eq!(correlation.generation(), 7);
    assert_eq!(correlation.round(), 3);
    assert_eq!(correlation.phase(), RefinementPhase::AdversaryAttack);
    assert_eq!(correlation.role(), RefinementRole::Adversary);
}

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
        got.created_by_user_id.as_deref(),
        Some(user_id.as_str()),
        "list_ready must SELECT created_by_user_id, not default it to None"
    );
}

/// Both dispatch-read projections hydrate `Task` through runtime SQL, where
/// omitted `#[sqlx(default)]` fields otherwise silently become `None`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_and_claim_preserve_refinement_correlation() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let project_id = uuid::Uuid::now_v7().to_string();
    let proposal_id = uuid::Uuid::now_v7().to_string();
    let run_id = uuid::Uuid::now_v7().to_string();
    let intent_id = uuid::Uuid::now_v7().to_string();
    sqlx::query!(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
        project_id,
        "p",
        "test",
        format!("refinement-ready-projection-{project_id}"),
    )
    .execute(db.pool())
    .await
    .unwrap();

    // Migration 138 makes both task correlation IDs foreign keys, so persist
    // the exact run and intent identity exercised by these projections.
    sqlx::query(
        "INSERT INTO proposals (id, short_id, title, body, body_format, acceptance_criteria, status, latest_revision_seq) \
         VALUES ($1, $2, 'Ready projection proposal', '', 'markdown', '[]'::jsonb, 'draft', 1)",
    )
    .bind(&proposal_id)
    .bind(format!("p{}", &proposal_id[..8]))
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO refinement_runs \
         (id, proposal_id, generation, idempotency_key, state, terminal_at, stop_tag) \
         VALUES ($1, $2, 7, $3, 'terminal', '2026-01-01T00:00:00.000Z', 'operator_stop')",
    )
    .bind(&run_id)
    .bind(&proposal_id)
    .bind(format!("ready-projection-run-{run_id}"))
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO refinement_dispatch_intents \
         (id, run_id, round, phase, role, idempotency_key) \
         VALUES ($1, $2, 3, 'adversary_attack', 'adversary', $3)",
    )
    .bind(&intent_id)
    .bind(&run_id)
    .bind(format!("ready-projection-intent-{intent_id}"))
    .execute(db.pool())
    .await
    .unwrap();

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let task = repo
        .create_fixture_in_project(
            &project_id,
            None,
            "correlated ready task",
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
    let correlation = TaskRefinementCorrelation::new(
        run_id.clone(),
        intent_id.clone(),
        7,
        3,
        RefinementPhase::AdversaryAttack,
        RefinementRole::Adversary,
    )
    .unwrap();
    repo.set_refinement_correlation(&task.id, Some(&correlation))
        .await
        .unwrap();

    let ready = repo
        .list_ready(ReadyQuery {
            project_id: Some(project_id.clone()),
            limit: 1,
            ..Default::default()
        })
        .await
        .unwrap();
    let ready_task = ready
        .iter()
        .find(|candidate| candidate.id == task.id)
        .expect("correlated task must be ready");
    assert_adversary_correlation(ready_task, &run_id, &intent_id);

    let claimed = repo
        .claim(
            ReadyQuery {
                project_id: Some(project_id),
                limit: 1,
                ..Default::default()
            },
            "dispatcher",
            "system",
        )
        .await
        .unwrap()
        .expect("correlated task must be claimable");
    assert_eq!(claimed.id, task.id);
    assert_adversary_correlation(&claimed, &run_id, &intent_id);
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
        got.created_by_user_id.as_deref(),
        Some(user_id.as_str()),
        "list_by_status_filtered must SELECT created_by_user_id, not default it to None"
    );
}
