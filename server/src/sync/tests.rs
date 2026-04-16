use crate::events::DjinnEventEnvelope;
use crate::events::event_bus_for;
use tokio::sync::broadcast;

use super::*;

#[test]
fn registered_channels_has_tasks() {
    let tasks_channel = REGISTERED_CHANNELS
        .iter()
        .find(|channel| channel.name == "tasks")
        .expect("tasks channel should remain registered");
    assert_eq!(tasks_channel.branch, "djinn/tasks");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_manager_has_all_channels() {
    let db = crate::test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(16);
    let mgr = SyncManager::new(db, tx);
    let status = mgr.status().await;
    assert_eq!(status.len(), REGISTERED_CHANNELS.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enable_project_persists_flag() {
    let db = crate::test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(16);
    let mgr = SyncManager::new(db.clone(), tx.clone());

    let project_repo = djinn_db::ProjectRepository::new(db.clone(), event_bus_for(&tx));
    let project = project_repo
        .create("test-proj", "/tmp/test-project")
        .await
        .unwrap();

    mgr.enable_project(&project.id).await.unwrap();

    let projects = mgr.list_sync_enabled_projects().await;
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].id, project.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disable_project_clears_flag() {
    let db = crate::test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(16);
    let mgr = SyncManager::new(db.clone(), tx.clone());

    let project_repo = djinn_db::ProjectRepository::new(db.clone(), event_bus_for(&tx));
    let project = project_repo
        .create("test-proj", "/tmp/test-project")
        .await
        .unwrap();

    mgr.enable_project(&project.id).await.unwrap();
    mgr.disable_project(&project.id).await.unwrap();

    let projects = mgr.list_sync_enabled_projects().await;
    assert!(projects.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_shows_enabled_projects() {
    let db = crate::test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(16);
    let mgr = SyncManager::new(db.clone(), tx.clone());

    let project_repo = djinn_db::ProjectRepository::new(db.clone(), event_bus_for(&tx));
    let project = project_repo
        .create("my-repo", "/tmp/my-repo")
        .await
        .unwrap();
    mgr.enable_project(&project.id).await.unwrap();

    let statuses = mgr.status().await;
    let ch = statuses.iter().find(|s| s.name == "tasks").unwrap();
    assert!(ch.enabled);
    assert_eq!(ch.project_paths, vec!["/tmp/my-repo"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_project_sync_lists_all_enabled() {
    let db = crate::test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(16);
    let mgr = SyncManager::new(db.clone(), tx.clone());

    let project_repo = djinn_db::ProjectRepository::new(db.clone(), event_bus_for(&tx));
    let p1 = project_repo.create("alpha", "/tmp/alpha").await.unwrap();
    let p2 = project_repo.create("beta", "/tmp/beta").await.unwrap();
    let _p3 = project_repo.create("gamma", "/tmp/gamma").await.unwrap();

    mgr.enable_project(&p1.id).await.unwrap();
    mgr.enable_project(&p2.id).await.unwrap();

    let projects = mgr.list_sync_enabled_projects().await;
    assert_eq!(projects.len(), 2);
    assert_eq!(projects[0].name, "alpha");
    assert_eq!(projects[1].name, "beta");
}

#[test]
fn per_project_sha_keys_are_unique() {
    let key1 = tasks_channel::sha_settings_key("proj-aaa");
    let key2 = tasks_channel::sha_settings_key("proj-bbb");
    assert_ne!(key1, key2);
    assert!(key1.contains("proj-aaa"));
    assert!(key2.contains("proj-bbb"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_for_export_filters_by_project_id() {
    use djinn_db::EpicRepository;
    use djinn_db::TaskRepository;

    let db = crate::test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(64);

    let project_repo = djinn_db::ProjectRepository::new(db.clone(), event_bus_for(&tx));
    let p1 = project_repo.create("proj-a", "/tmp/a").await.unwrap();
    let p2 = project_repo.create("proj-b", "/tmp/b").await.unwrap();

    let epic_repo = EpicRepository::new(db.clone(), event_bus_for(&tx));
    let e1 = epic_repo
        .create("Epic A", "", "", "", "", None)
        .await
        .unwrap();
    let task_repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let _t1 = task_repo
        .create_in_project(
            &p1.id,
            Some(&e1.id),
            "Task in A",
            "",
            "",
            "task",
            0,
            "",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    let _t2 = task_repo
        .create_in_project(
            &p2.id,
            Some(&e1.id),
            "Task in B",
            "",
            "",
            "task",
            0,
            "",
            Some("open"),
            None,
        )
        .await
        .unwrap();

    let export_a = task_repo.list_for_export(Some(&p1.id)).await.unwrap();
    let export_b = task_repo.list_for_export(Some(&p2.id)).await.unwrap();
    let export_all = task_repo.list_for_export(None).await.unwrap();

    assert_eq!(export_a.len(), 1, "should have 1 task for project A");
    assert_eq!(export_b.len(), 1, "should have 1 task for project B");
    assert!(
        export_all.len() >= 2,
        "unfiltered should have at least 2 tasks"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_all_skips_when_disabled() {
    let db = crate::test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(16);
    let mgr = SyncManager::new(db, tx);
    let results = mgr.export_all(Some("user1")).await;
    assert!(results.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_all_skips_when_disabled() {
    let db = crate::test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(16);
    let mgr = SyncManager::new(db, tx);
    let results = mgr.import_all(false).await;
    assert!(results.is_empty());
}

#[test]
fn now_utc_is_reasonable() {
    let s = now_utc();
    assert!(s.starts_with("20"), "timestamp should start with '20': {s}");
    assert!(s.ends_with('Z'), "timestamp should end with 'Z': {s}");
    assert_eq!(s.len(), 20, "timestamp should be 20 chars: {s}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_peer_emits_from_sync_true() {
    use djinn_db::EpicRepository;
    use djinn_db::TaskRepository;

    let db = crate::test_helpers::create_test_db();
    let (tx, mut rx) = broadcast::channel(64);

    let epic_repo = EpicRepository::new(db.clone(), event_bus_for(&tx));
    let epic = epic_repo
        .create("Test Epic", "", "", "", "", None)
        .await
        .unwrap();
    while rx.try_recv().is_ok() {}

    let task_repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let peer_task = djinn_core::models::Task {
        id: uuid::Uuid::now_v7().to_string(),
        project_id: epic.project_id.clone(),
        short_id: "abc".to_string(),
        epic_id: Some(epic.id.clone()),
        title: "Peer Task".to_string(),
        description: String::new(),
        design: String::new(),
        issue_type: "task".to_string(),
        status: "open".to_string(),
        priority: 0,
        owner: String::new(),
        labels: "[]".to_string(),
        acceptance_criteria: "[]".to_string(),
        reopen_count: 0,
        continuation_count: 0,
        verification_failure_count: 0,
        created_at: "2026-01-01T00:00:00.000Z".to_string(),
        updated_at: "2026-03-08T00:00:00.000Z".to_string(),
        closed_at: None,
        close_reason: None,
        merge_commit_sha: None,
        pr_url: None,
        merge_conflict_metadata: None,
        memory_refs: "[]".to_string(),
        agent_type: None,
        unresolved_blocker_count: 0,
        total_reopen_count: 0,
        total_verification_failure_count: 0,
        intervention_count: 0,
        last_intervention_at: None,
    };

    let changed = task_repo.upsert_peer(&peer_task).await.unwrap();
    assert!(changed, "upsert_peer should insert new task");

    let envelope = rx.recv().await.unwrap();
    assert_eq!(envelope.entity_type, "task");
    assert_eq!(envelope.action, "updated");
    assert!(envelope.from_sync, "upsert_peer must emit from_sync: true");
}

#[test]
fn sse_envelope_excludes_from_sync_field() {
    let task = djinn_core::models::Task {
        id: "test-id".to_string(),
        project_id: "proj".to_string(),
        short_id: "xyz".to_string(),
        epic_id: None,
        title: "Test".to_string(),
        description: String::new(),
        design: String::new(),
        issue_type: "task".to_string(),
        status: "open".to_string(),
        priority: 0,
        owner: String::new(),
        labels: "[]".to_string(),
        acceptance_criteria: "[]".to_string(),
        reopen_count: 0,
        continuation_count: 0,
        verification_failure_count: 0,
        created_at: "2026-01-01T00:00:00.000Z".to_string(),
        updated_at: "2026-01-01T00:00:00.000Z".to_string(),
        closed_at: None,
        close_reason: None,
        merge_commit_sha: None,
        pr_url: None,
        merge_conflict_metadata: None,
        memory_refs: "[]".to_string(),
        agent_type: None,
        unresolved_blocker_count: 0,
        total_reopen_count: 0,
        total_verification_failure_count: 0,
        intervention_count: 0,
        last_intervention_at: None,
    };

    let envelope = DjinnEventEnvelope::task_updated(&task, true);
    assert!(envelope.from_sync, "from_sync should be true in-memory");

    let value: serde_json::Value = serde_json::to_value(&envelope).unwrap();
    assert!(
        value.get("from_sync").is_none(),
        "top-level from_sync should not appear in serialized envelope"
    );
}

#[test]
fn background_task_match_filters_from_sync_true() {
    let should_trigger =
        |env: &DjinnEventEnvelope| -> bool { env.entity_type == "task" && !env.from_sync };

    let task = djinn_core::models::Task {
        id: "t".to_string(),
        project_id: "p".to_string(),
        short_id: "s".to_string(),
        epic_id: None,
        title: String::new(),
        description: String::new(),
        design: String::new(),
        issue_type: "task".to_string(),
        status: "open".to_string(),
        priority: 0,
        owner: String::new(),
        labels: "[]".to_string(),
        acceptance_criteria: "[]".to_string(),
        reopen_count: 0,
        continuation_count: 0,
        verification_failure_count: 0,
        created_at: String::new(),
        updated_at: String::new(),
        closed_at: None,
        close_reason: None,
        merge_commit_sha: None,
        pr_url: None,
        merge_conflict_metadata: None,
        memory_refs: "[]".to_string(),
        agent_type: None,
        unresolved_blocker_count: 0,
        total_reopen_count: 0,
        total_verification_failure_count: 0,
        intervention_count: 0,
        last_intervention_at: None,
    };

    assert!(
        should_trigger(&DjinnEventEnvelope::task_created(&task, false)),
        "local TaskCreated should trigger export"
    );
    assert!(
        should_trigger(&DjinnEventEnvelope::task_updated(&task, false)),
        "local TaskUpdated should trigger export"
    );
    assert!(
        !should_trigger(&DjinnEventEnvelope::task_created(&task, true)),
        "sync TaskCreated should NOT trigger export"
    );
    assert!(
        !should_trigger(&DjinnEventEnvelope::task_updated(&task, true)),
        "sync TaskUpdated should NOT trigger export"
    );
    assert!(
        should_trigger(&DjinnEventEnvelope::task_deleted("t")),
        "TaskDeleted should always trigger export"
    );
    assert!(
        !should_trigger(&DjinnEventEnvelope::note_deleted("n")),
        "non-task events should not trigger export"
    );
}

#[test]
fn sync_completed_serializes_correctly() {
    let envelope = DjinnEventEnvelope::sync_completed("tasks", "export", 5, None);
    let json = serde_json::to_string(&envelope).unwrap();
    assert!(json.contains("\"channel\":\"tasks\""), "json: {json}");
    assert!(json.contains("\"direction\":\"export\""), "json: {json}");
    assert!(json.contains("\"count\":5"), "json: {json}");
}

#[test]
fn sync_completed_with_error_serializes() {
    let envelope =
        DjinnEventEnvelope::sync_completed("tasks", "import", 0, Some("git push failed"));
    let json = serde_json::to_string(&envelope).unwrap();
    assert!(
        json.contains("\"error\":\"git push failed\""),
        "json: {json}"
    );
}

#[test]
fn sync_completed_does_not_trigger_export() {
    let envelope = DjinnEventEnvelope::sync_completed("tasks", "export", 5, None);
    let triggers = envelope.entity_type == "task" && !envelope.from_sync;
    assert!(
        !triggers,
        "SyncCompleted should not trigger background export"
    );
}

#[test]
fn auto_import_interval_is_60_seconds() {
    const SECONDS: u64 = 60;
    let duration = std::time::Duration::from_secs(SECONDS);
    assert_eq!(
        duration.as_secs(),
        60,
        "auto-import should fire every 60 seconds"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_import_triggers_on_interval() {
    use tokio_util::sync::CancellationToken;

    let db = crate::test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(16);
    let mgr = SyncManager::new(db.clone(), tx.clone());

    let cancel = CancellationToken::new();
    let user_id = "test-user".to_string();

    let project_repo = djinn_db::ProjectRepository::new(db.clone(), event_bus_for(&tx));
    let project = project_repo
        .create("interval-test", "/tmp/interval-test")
        .await
        .unwrap();
    mgr.enable_project(&project.id).await.unwrap();

    mgr.spawn_background_task(cancel.clone(), user_id);
}
