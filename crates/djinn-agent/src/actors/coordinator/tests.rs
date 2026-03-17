use super::*;
use crate::events::DjinnEventEnvelope;
use crate::storage::test_helpers;
use djinn_core::models::{ProjectState, SlotEvent, TaskState, WorkerRole};
use djinn_core::repositories::{EpicRepository, TaskRepository};
use djinn_core::transitions::TransitionAction;
use tokio::sync::broadcast;

async fn make_epic(db: &Database, tx: broadcast::Sender<DjinnEventEnvelope>) -> djinn_core::models::Epic {
    EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .create("Epic", "", "", "", "", None)
        .await
        .unwrap()
}

fn spawn_coordinator(db: &Database, tx: &broadcast::Sender<DjinnEventEnvelope>) -> CoordinatorHandle {
    let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(tx));
    let project_repo = crate::storage::repository::ProjectRepository::new(
        db.clone(),
        crate::events::event_bus_for(tx),
    );
    let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(tx));
    let role_registry = crate::agents::role_registry::RoleRegistry::default();
    let verification_tracker = crate::actors::verification_tracker::VerificationTracker::new();
    let (sender, actor) = CoordinatorActor::new(
        task_repo,
        project_repo,
        epic_repo,
        tx.clone(),
        role_registry,
        verification_tracker,
    );
    tokio::spawn(actor.run());
    CoordinatorHandle { sender }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_prioritizes_review_then_verify_then_worker() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let review_task = repo.create(&epic.id, "Review", "", "", "task", 0, "", Some("open")).await.unwrap();
    repo.update(&review_task.id, "Review", "", "", 0, "", "", r#"[{"description":"default","met":false}]"#).await.unwrap();
    repo.transition(&review_task.id, TransitionAction::Start, "test", "system", None, None).await.unwrap();
    repo.transition(&review_task.id, TransitionAction::SubmitTaskReview, "test", "system", None, None).await.unwrap();

    let verify_task = repo.create(&epic.id, "Verify", "", "", "task", 1, "", Some("in_verification")).await.unwrap();
    let worker_task = repo.create(&epic.id, "Worker", "", "", "task", 2, "", Some("open")).await.unwrap();

    let handle = spawn_coordinator(&db, &tx);
    handle.update_dispatch_limit(1).await.unwrap();

    handle.trigger_dispatch().await.unwrap();
    handle.wait_for_status(|s| s.tasks_dispatched >= 1).await;
    assert_eq!(repo.get(&review_task.id).await.unwrap().unwrap().status, "in_task_review");
    assert_eq!(repo.get(&verify_task.id).await.unwrap().unwrap().status, "in_verification");
    assert_eq!(repo.get(&worker_task.id).await.unwrap().unwrap().status, "open");

    handle.trigger_dispatch().await.unwrap();
    handle.wait_for_status(|s| s.tasks_dispatched >= 2).await;
    assert_eq!(repo.get(&verify_task.id).await.unwrap().unwrap().status, "in_verification");
    assert_eq!(repo.get(&worker_task.id).await.unwrap().unwrap().status, "open");

    handle.trigger_dispatch().await.unwrap();
    handle.wait_for_status(|s| s.tasks_dispatched >= 3).await;
    assert_eq!(repo.get(&worker_task.id).await.unwrap().unwrap().status, "in_progress");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_skips_unhealthy_project() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = repo.create(&epic.id, "Unhealthy", "", "", "task", 0, "", Some("open")).await.unwrap();

    let handle = spawn_coordinator(&db, &tx);
    handle.sender.send(CoordinatorMessage::SetProjectHealth { project_id: epic.project_id.clone(), healthy: false, error: Some("broken".to_string()) }).await.unwrap();
    handle.wait_for_project_status(&epic.project_id, |s| s.paused).await;

    handle.trigger_dispatch().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(repo.get(&task.id).await.unwrap().unwrap().status, "open");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_slot_exhaustion_no_overdispatch() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let t1 = repo.create(&epic.id, "A", "", "", "task", 0, "", Some("open")).await.unwrap();
    let t2 = repo.create(&epic.id, "B", "", "", "task", 1, "", Some("open")).await.unwrap();
    let t3 = repo.create(&epic.id, "C", "", "", "task", 2, "", Some("open")).await.unwrap();

    let handle = spawn_coordinator(&db, &tx);
    handle.trigger_dispatch().await.unwrap();
    handle.wait_for_status(|s| s.tasks_dispatched >= 2).await;

    let statuses = vec![
        repo.get(&t1.id).await.unwrap().unwrap().status,
        repo.get(&t2.id).await.unwrap().unwrap().status,
        repo.get(&t3.id).await.unwrap().unwrap().status,
    ];
    assert_eq!(statuses.iter().filter(|s| s.as_str() == "in_progress").count(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redispatch_on_slot_free() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let t1 = repo.create(&epic.id, "A", "", "", "task", 0, "", Some("open")).await.unwrap();
    let t2 = repo.create(&epic.id, "B", "", "", "task", 1, "", Some("open")).await.unwrap();

    let handle = spawn_coordinator(&db, &tx);
    handle.update_dispatch_limit(1).await.unwrap();
    handle.trigger_dispatch().await.unwrap();
    handle.wait_for_status(|s| s.tasks_dispatched >= 1).await;

    let first_in_progress = if repo.get(&t1.id).await.unwrap().unwrap().status == "in_progress" { t1.id.clone() } else { t2.id.clone() };
    let first_state = TaskState { id: first_in_progress.clone(), project_id: epic.project_id.clone(), role: WorkerRole::Worker };
    handle.sender.send(CoordinatorMessage::SlotFreed(SlotEvent::Free(first_state))).await.unwrap();

    handle.wait_for_status(|s| s.tasks_dispatched >= 2).await;
    let s1 = repo.get(&t1.id).await.unwrap().unwrap().status;
    let s2 = repo.get(&t2.id).await.unwrap().unwrap().status;
    assert!(s1 == "in_progress" && s2 == "in_progress" || s1 == "open" && s2 == "in_progress" || s1 == "in_progress" && s2 == "open");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_project_fairness_dispatches_both() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic1 = make_epic(&db, tx.clone()).await;
    let epic2 = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let p1 = repo.create(&epic1.id, "P1", "", "", "task", 0, "", Some("open")).await.unwrap();
    let p2 = repo.create(&epic2.id, "P2", "", "", "task", 0, "", Some("open")).await.unwrap();

    let handle = spawn_coordinator(&db, &tx);
    handle.trigger_dispatch().await.unwrap();
    handle.wait_for_status(|s| s.tasks_dispatched >= 2).await;

    assert_eq!(repo.get(&p1.id).await.unwrap().unwrap().status, "in_progress");
    assert_eq!(repo.get(&p2.id).await.unwrap().unwrap().status, "in_progress");
}
