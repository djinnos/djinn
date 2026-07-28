use crate::finalize_handlers::{apply_ac_verdicts, handle_budget_park, process_finalize_payload};
use crate::finalize_types::AcVerdict;
use crate::test_helpers;
use djinn_db::TaskRepository;

const AUTHENTICATED_SESSION_ID: &str = "server-authenticated-session-123";

#[test]
fn apply_ac_verdicts_sets_met_flags_from_payload() {
    let existing =
        r#"[{"criterion":"write tests","met":false},{"criterion":"passing ci","met":false}]"#;
    let verdicts = vec![
        AcVerdict {
            criterion: "write tests".to_string(),
            met: true,
        },
        AcVerdict {
            criterion: "passing ci".to_string(),
            met: true,
        },
    ];
    let result = apply_ac_verdicts(existing, &verdicts);
    let parsed: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed[0]["met"], true);
    assert_eq!(parsed[1]["met"], true);
}

#[test]
fn apply_ac_verdicts_preserves_existing_criterion_text_when_empty() {
    let existing = r#"[{"criterion":"write tests","met":false}]"#;
    let verdicts = vec![AcVerdict {
        criterion: String::new(),
        met: true,
    }];
    let result = apply_ac_verdicts(existing, &verdicts);
    let parsed: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed[0]["criterion"], "write tests");
    assert_eq!(parsed[0]["met"], true);
}

#[test]
fn apply_ac_verdicts_handles_empty_existing_gracefully() {
    let existing = "not-valid-json";
    let verdicts = vec![AcVerdict {
        criterion: "x".to_string(),
        met: false,
    }];
    let result = apply_ac_verdicts(existing, &verdicts);
    let parsed: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed[0]["criterion"], "x");
    assert_eq!(parsed[0]["met"], false);
}

#[tokio::test]
async fn budget_park_logs_handoff_without_successful_submission() {
    let crate::test_helpers::ContextFixture {
        db,
        ctx,
        project: _,
        epic: _,
        task,
    } = crate::test_helpers::seed_context_fixture().await;
    handle_budget_park(
        "completed A; B remains",
        "budget-triggered wind-down summary captured",
        &task.id,
        &ctx,
    )
    .await;
    let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
    let entries = repo.list_activity(&task.id).await.unwrap();
    assert!(entries.iter().all(|e| e.event_type != "work_submitted"));
    let work_entries: Vec<_> = entries
        .iter()
        .filter(|e| e.event_type == "work_parked")
        .collect();
    assert_eq!(work_entries.len(), 1);
    let body: serde_json::Value = serde_json::from_str(&work_entries[0].payload).unwrap();
    assert_eq!(body["summary"], "completed A; B remains");
    assert_eq!(
        body["remaining_concerns"],
        "budget-parked: budget-triggered wind-down summary captured"
    );
}

#[tokio::test]
async fn budget_park_empty_summary_skips_activity() {
    let crate::test_helpers::ContextFixture {
        db,
        ctx,
        project: _,
        epic: _,
        task,
    } = crate::test_helpers::seed_context_fixture().await;
    handle_budget_park("   ", "ignored", &task.id, &ctx).await;
    let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
    let entries = repo.list_activity(&task.id).await.unwrap();
    assert!(entries.iter().all(|e| e.event_type != "work_parked"));
}

#[tokio::test]
async fn submit_work_accepts_server_authenticated_session_separately_from_payload() {
    let crate::test_helpers::ContextFixture {
        db,
        ctx,
        project: _,
        epic: _,
        task,
    } = crate::test_helpers::seed_context_fixture().await;
    let payload = Some(serde_json::json!({
        "task_id": task.short_id,
        "commit_title": "feat: implement the feature",
        "summary": "implemented the feature",
        "files_changed": ["src/main.rs", "src/lib.rs"],
        "remaining_concerns": ["needs perf testing"]
    }));
    process_finalize_payload(
        &payload,
        "submit_work",
        &task.id,
        AUTHENTICATED_SESSION_ID,
        &ctx,
    )
    .await;
    let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
    let entries = repo.list_activity(&task.id).await.unwrap();
    let work_entry = entries.iter().find(|e| e.event_type == "work_submitted");
    assert!(
        work_entry.is_some(),
        "expected work_submitted activity entry"
    );
    let body: serde_json::Value = serde_json::from_str(&work_entry.unwrap().payload).unwrap();
    assert_eq!(body["summary"], "implemented the feature");
    assert_eq!(body["files_changed"][0], "src/main.rs");
    assert_eq!(body["remaining_concerns"][0], "needs perf testing");
}

#[tokio::test]
async fn budget_park_submit_work_activity_surfaces_unchanged() {
    let crate::test_helpers::ContextFixture {
        db,
        ctx,
        project: _,
        epic: _,
        task,
    } = crate::test_helpers::seed_context_fixture().await;
    let payload = Some(serde_json::json!({
        "task_id": task.short_id,
        "commit_title": "park budget summary",
        "summary": "finished the safe subset before parking",
        "files_changed": ["src/lib.rs"],
        "remaining_concerns": ["budget-parked: finish the follow-up UI snapshot"]
    }));
    process_finalize_payload(
        &payload,
        "submit_work",
        &task.id,
        AUTHENTICATED_SESSION_ID,
        &ctx,
    )
    .await;
    let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
    let entries = repo.list_activity(&task.id).await.unwrap();
    let work_entry = entries
        .iter()
        .find(|entry| entry.event_type == "work_submitted")
        .expect("expected budget-park work_submitted activity entry");
    let body: serde_json::Value = serde_json::from_str(&work_entry.payload).unwrap();
    assert_eq!(body["summary"], "finished the safe subset before parking");
    assert_eq!(
        body["remaining_concerns"][0],
        "budget-parked: finish the follow-up UI snapshot"
    );
}

#[tokio::test]
async fn submit_work_malformed_payload_does_not_crash() {
    let crate::test_helpers::ContextFixture {
        db: _,
        ctx,
        project: _,
        epic: _,
        task,
    } = crate::test_helpers::seed_context_fixture().await;
    // Missing required "summary" field.
    let payload = Some(serde_json::json!({"task_id": task.id}));
    // Should not panic.
    process_finalize_payload(
        &payload,
        "submit_work",
        &task.id,
        AUTHENTICATED_SESSION_ID,
        &ctx,
    )
    .await;
}

#[tokio::test]
async fn submit_review_atomically_sets_ac_from_criteria_array() {
    let crate::test_helpers::ContextFixture {
        db,
        ctx,
        project: _,
        epic: _,
        task,
    } = crate::test_helpers::seed_context_fixture().await;
    // Seed AC with met=false.
    TaskRepository::new(db.clone(), ctx.event_bus.clone())
        .set_acceptance_criteria(
            &task.id,
            r#"[{"criterion":"write tests","met":false},{"criterion":"passes ci","met":false}]"#,
        )
        .await
        .unwrap();
    let payload = Some(serde_json::json!({
        "task_id": task.id,
        "verdict": "approved",
        "acceptance_criteria": [
            {"criterion": "write tests", "met": true},
            {"criterion": "passes ci", "met": true}
        ],
        "feedback": null
    }));
    process_finalize_payload(
        &payload,
        "submit_review",
        &task.id,
        AUTHENTICATED_SESSION_ID,
        &ctx,
    )
    .await;
    // AC should be updated in the DB.
    let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
    let updated = repo.get(&task.id).await.unwrap().unwrap();
    let ac: Vec<serde_json::Value> = serde_json::from_str(&updated.acceptance_criteria).unwrap();
    assert_eq!(ac[0]["met"], true);
    assert_eq!(ac[1]["met"], true);
}

#[tokio::test]
async fn submit_review_logs_verdict_activity() {
    let crate::test_helpers::ContextFixture {
        db,
        ctx,
        project: _,
        epic: _,
        task,
    } = crate::test_helpers::seed_context_fixture().await;
    let payload = Some(serde_json::json!({
        "task_id": task.id,
        "verdict": "rejected",
        "acceptance_criteria": [],
        "feedback": "missing edge case handling"
    }));
    process_finalize_payload(
        &payload,
        "submit_review",
        &task.id,
        AUTHENTICATED_SESSION_ID,
        &ctx,
    )
    .await;
    let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
    let entries = repo.list_activity(&task.id).await.unwrap();
    let entry = entries.iter().find(|e| e.event_type == "review_submitted");
    assert!(entry.is_some(), "expected review_submitted activity entry");
    let body: serde_json::Value = serde_json::from_str(&entry.unwrap().payload).unwrap();
    assert_eq!(body["verdict"], "rejected");
    assert_eq!(body["feedback"], "missing edge case handling");
}

#[tokio::test]
async fn submit_review_malformed_payload_does_not_crash() {
    let crate::test_helpers::ContextFixture {
        db: _,
        ctx,
        project: _,
        epic: _,
        task,
    } = crate::test_helpers::seed_context_fixture().await;
    // "verdict" is required but missing.
    let payload = Some(serde_json::json!({"task_id": task.id}));
    process_finalize_payload(
        &payload,
        "submit_review",
        &task.id,
        AUTHENTICATED_SESSION_ID,
        &ctx,
    )
    .await;
}

#[tokio::test]
async fn submit_decision_logs_decision_activity() {
    let crate::test_helpers::ContextFixture {
        db,
        ctx,
        project: _,
        epic: _,
        task,
    } = crate::test_helpers::seed_context_fixture().await;
    let payload = Some(serde_json::json!({
        "task_id": task.id,
        "decision": "reopen",
        "rationale": "scope was too broad",
        "created_tasks": []
    }));
    process_finalize_payload(
        &payload,
        "submit_decision",
        &task.id,
        AUTHENTICATED_SESSION_ID,
        &ctx,
    )
    .await;
    let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
    let entries = repo.list_activity(&task.id).await.unwrap();
    let entry = entries
        .iter()
        .find(|e| e.event_type == "decision_submitted");
    assert!(
        entry.is_some(),
        "expected decision_submitted activity entry"
    );
    let body: serde_json::Value = serde_json::from_str(&entry.unwrap().payload).unwrap();
    assert_eq!(body["decision"], "reopen");
    assert_eq!(body["rationale"], "scope was too broad");
}

#[tokio::test]
async fn submit_decision_malformed_payload_does_not_crash() {
    let crate::test_helpers::ContextFixture {
        db: _,
        ctx,
        project: _,
        epic: _,
        task,
    } = crate::test_helpers::seed_context_fixture().await;
    // "decision" is required but missing.
    let payload = Some(serde_json::json!({"task_id": task.id}));
    process_finalize_payload(
        &payload,
        "submit_decision",
        &task.id,
        AUTHENTICATED_SESSION_ID,
        &ctx,
    )
    .await;
}

#[tokio::test]
async fn submit_grooming_logs_per_task_activity_entries() {
    let db = crate::test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = crate::test_helpers::create_test_project(&db).await;
    let epic = crate::test_helpers::create_test_epic(&db, &project.id).await;
    let task1 = crate::test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    let task2 = crate::test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    let payload = Some(serde_json::json!({
        "tasks_reviewed": [
            {"task_id": task1.id, "action": "promoted", "changes": "bumped priority to 1"},
            {"task_id": task2.id, "action": "skipped", "changes": null}
        ],
        "summary": "groomed 2 tasks"
    }));
    // Planner is project-scoped; pass synthetic task_id.
    let synthetic_id = format!("project:{}:planner", project.id);
    process_finalize_payload(
        &payload,
        "submit_grooming",
        &synthetic_id,
        AUTHENTICATED_SESSION_ID,
        &ctx,
    )
    .await;
    let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
    let entries1 = repo.list_activity(&task1.id).await.unwrap();
    let e1 = entries1.iter().find(|e| e.event_type == "planning_entry");
    assert!(e1.is_some(), "expected planning_entry for task1");
    let b1: serde_json::Value = serde_json::from_str(&e1.unwrap().payload).unwrap();
    assert_eq!(b1["action"], "promoted");
    assert_eq!(b1["changes"], "bumped priority to 1");
    let entries2 = repo.list_activity(&task2.id).await.unwrap();
    let e2 = entries2.iter().find(|e| e.event_type == "planning_entry");
    assert!(e2.is_some(), "expected planning_entry for task2");
    let b2: serde_json::Value = serde_json::from_str(&e2.unwrap().payload).unwrap();
    assert_eq!(b2["action"], "skipped");
}

/// A planner concluding "blocked on epic X, no tasks created" must durably
/// record the epic-blocker edge, so the coordinator parks this epic instead
/// of re-planning every stale-sweep (epic `mygq`, 2026-07-01).
#[tokio::test]
async fn submit_grooming_blocked_on_records_epic_blocker_durably() {
    let db = crate::test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = crate::test_helpers::create_test_project(&db).await;
    let parked = crate::test_helpers::create_test_epic(&db, &project.id).await;
    let blocker = crate::test_helpers::create_test_epic(&db, &project.id).await;
    // The planning session runs on a real planning task under the parked epic.
    let planning_task = crate::test_helpers::create_test_task(&db, &project.id, &parked.id).await;
    let epic_repo = djinn_db::EpicRepository::new(db.clone(), ctx.event_bus.clone());
    assert!(
        !epic_repo.has_unresolved_blockers(&parked.id).await.unwrap(),
        "precondition: parked epic starts with no blockers"
    );
    // Declare the blocker by short_id and create no tasks.
    let payload = Some(serde_json::json!({
        "tasks_reviewed": [],
        "summary": "blocked on foundation epic; no work created",
        "decision": "escalate",
        "blocked_on": [blocker.short_id],
    }));
    process_finalize_payload(
        &payload,
        "submit_grooming",
        &planning_task.id,
        AUTHENTICATED_SESSION_ID,
        &ctx,
    )
    .await;
    // The durable edge must exist and the gate must see an open blocker.
    assert!(
        epic_repo.has_unresolved_blockers(&parked.id).await.unwrap(),
        "blocked_on must durably record an epic-blocker edge"
    );
    let blockers = epic_repo.list_blockers(&parked.id).await.unwrap();
    assert_eq!(blockers.len(), 1);
    assert_eq!(blockers[0].epic_id, blocker.id);
    // Idempotent: re-declaring the same blocker must not error or duplicate.
    process_finalize_payload(
        &payload,
        "submit_grooming",
        &planning_task.id,
        AUTHENTICATED_SESSION_ID,
        &ctx,
    )
    .await;
    let blockers_again = epic_repo.list_blockers(&parked.id).await.unwrap();
    assert_eq!(
        blockers_again.len(),
        1,
        "re-declaring the same blocker must be idempotent"
    );
    // Closing the blocker clears the gate (event-driven wake path).
    epic_repo.close(&blocker.id).await.unwrap();
    assert!(
        !epic_repo.has_unresolved_blockers(&parked.id).await.unwrap(),
        "closing the blocker must clear the park gate"
    );
}

/// An unresolvable `blocked_on` ref (e.g. a task short_id, not an epic) must
/// be skipped without crashing and without recording a bogus edge.
#[tokio::test]
async fn submit_grooming_blocked_on_unresolvable_ref_is_skipped() {
    let db = crate::test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = crate::test_helpers::create_test_project(&db).await;
    let parked = crate::test_helpers::create_test_epic(&db, &project.id).await;
    let planning_task = crate::test_helpers::create_test_task(&db, &project.id, &parked.id).await;
    let payload = Some(serde_json::json!({
        "tasks_reviewed": [],
        "blocked_on": ["does-not-exist"],
    }));
    process_finalize_payload(
        &payload,
        "submit_grooming",
        &planning_task.id,
        AUTHENTICATED_SESSION_ID,
        &ctx,
    )
    .await;
    let epic_repo = djinn_db::EpicRepository::new(db.clone(), ctx.event_bus.clone());
    assert!(
        !epic_repo.has_unresolved_blockers(&parked.id).await.unwrap(),
        "unresolvable blocked_on ref must not record a blocker"
    );
}

#[tokio::test]
async fn submit_grooming_malformed_payload_does_not_crash() {
    let db = crate::test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    // Missing "tasks_reviewed" entirely.
    let payload = Some(serde_json::json!({}));
    process_finalize_payload(
        &payload,
        "submit_grooming",
        "any-task-id",
        AUTHENTICATED_SESSION_ID,
        &ctx,
    )
    .await;
}
