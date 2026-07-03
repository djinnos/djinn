use crate::finalize_handlers::{
    apply_ac_verdicts, handle_budget_park, process_auto_submit_payload, process_finalize_payload,
    process_finalize_payload_with_outcome, record_rejected_integrity_entry,
};
use crate::finalize_types::AcVerdict;
use crate::test_helpers;
use djinn_db::TaskRepository;
use djinn_db::repositories::task_run::TaskRunRepository;
use djinn_db::repositories::verify_run::TaskRejectedSubmissionIntegrityRepository;

// ─── Test helpers ─────────────────────────────────────────────────────────

fn init_git_repo_with_dirty_file() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("djinn-test-git-")
        .tempdir()
        .expect("create temp dir");

    let run_git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };

    run_git(&["init"]);
    run_git(&["config", "--local", "user.email", "test@test.com"]);
    run_git(&["config", "--local", "user.name", "Test User"]);
    run_git(&["config", "--local", "commit.gpgsign", "false"]);

    std::fs::write(dir.path().join("README.md"), "hello\n").expect("write readme");
    run_git(&["add", "README.md"]);
    run_git(&["commit", "-m", "init"]);
    run_git(&["branch", "-m", "main"]);

    // Make a dirty tracked edit so the fingerprint computes a Diff.
    std::fs::write(dir.path().join("README.md"), "hello\ndirty\n").expect("write dirty");

    dir
}

async fn create_run_with_workspace(
    db: &djinn_db::Database,
    project_id: &str,
    task_id: &str,
    workspace_path: Option<&str>,
) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    TaskRunRepository::new(db.clone())
        .create(djinn_db::repositories::task_run::CreateTaskRunParams {
            id: &id,
            project_id,
            task_id,
            trigger_type: djinn_core::models::TaskRunTrigger::NewTask.as_str(),
            status: None,
            workspace_path,
            mirror_ref: None,
        })
        .await
        .expect("create task run");
    id
}

// ─── apply_ac_verdicts ────────────────────────────────────────────────────

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

// ─── process_finalize_payload: submit_work ────────────────────────────────

#[tokio::test]
async fn budget_park_logs_extractor_compatible_work_submitted() {
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

    handle_budget_park(
        "completed A; B remains",
        "budget-triggered wind-down summary captured",
        &task.id,
        &ctx,
    )
    .await;

    let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
    let entries = repo.list_activity(&task.id).await.unwrap();
    let work_entries: Vec<_> = entries
        .iter()
        .filter(|e| e.event_type == "work_submitted")
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
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

    handle_budget_park("   ", "ignored", &task.id, &ctx).await;

    let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
    let entries = repo.list_activity(&task.id).await.unwrap();
    assert!(entries.iter().all(|e| e.event_type != "work_submitted"));
}

#[tokio::test]
async fn submit_work_logs_activity_with_summary_and_files() {
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

    let payload = Some(serde_json::json!({
        "task_id": task.short_id,
        "commit_title": "feat: implement the feature",
        "summary": "implemented the feature",
        "files_changed": ["src/main.rs", "src/lib.rs"],
        "remaining_concerns": ["needs perf testing"]
    }));

    process_finalize_payload(&payload, "submit_work", &task.id, &ctx).await;

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
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

    let payload = Some(serde_json::json!({
        "task_id": task.short_id,
        "commit_title": "park budget summary",
        "summary": "finished the safe subset before parking",
        "files_changed": ["src/lib.rs"],
        "remaining_concerns": ["budget-parked: finish the follow-up UI snapshot"]
    }));

    process_finalize_payload(&payload, "submit_work", &task.id, &ctx).await;

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
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

    // Missing required "summary" field.
    let payload = Some(serde_json::json!({"task_id": task.id}));
    // Should not panic.
    process_finalize_payload(&payload, "submit_work", &task.id, &ctx).await;
}

// ─── process_finalize_payload: submit_review ──────────────────────────────

#[tokio::test]
async fn submit_review_atomically_sets_ac_from_criteria_array() {
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

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

    process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;

    // AC should be updated in the DB.
    let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
    let updated = repo.get(&task.id).await.unwrap().unwrap();
    let ac: Vec<serde_json::Value> = serde_json::from_str(&updated.acceptance_criteria).unwrap();
    assert_eq!(ac[0]["met"], true);
    assert_eq!(ac[1]["met"], true);
}

#[tokio::test]
async fn submit_review_logs_verdict_activity() {
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

    let payload = Some(serde_json::json!({
        "task_id": task.id,
        "verdict": "rejected",
        "acceptance_criteria": [],
        "feedback": "missing edge case handling"
    }));

    process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;

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
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

    // "verdict" is required but missing.
    let payload = Some(serde_json::json!({"task_id": task.id}));
    process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;
}

// ─── process_finalize_payload: submit_decision ────────────────────────────

#[tokio::test]
async fn submit_decision_logs_decision_activity() {
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

    let payload = Some(serde_json::json!({
        "task_id": task.id,
        "decision": "reopen",
        "rationale": "scope was too broad",
        "created_tasks": []
    }));

    process_finalize_payload(&payload, "submit_decision", &task.id, &ctx).await;

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
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

    // "decision" is required but missing.
    let payload = Some(serde_json::json!({"task_id": task.id}));
    process_finalize_payload(&payload, "submit_decision", &task.id, &ctx).await;
}

// ─── process_finalize_payload: submit_grooming ────────────────────────────

#[tokio::test]
async fn submit_grooming_logs_per_task_activity_entries() {
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task1 = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    let task2 = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

    let payload = Some(serde_json::json!({
        "tasks_reviewed": [
            {"task_id": task1.id, "action": "promoted", "changes": "bumped priority to 1"},
            {"task_id": task2.id, "action": "skipped", "changes": null}
        ],
        "summary": "groomed 2 tasks"
    }));

    // Planner is project-scoped; pass synthetic task_id.
    let synthetic_id = format!("project:{}:planner", project.id);
    process_finalize_payload(&payload, "submit_grooming", &synthetic_id, &ctx).await;

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
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = test_helpers::create_test_project(&db).await;
    let parked = test_helpers::create_test_epic(&db, &project.id).await;
    let blocker = test_helpers::create_test_epic(&db, &project.id).await;
    // The planning session runs on a real planning task under the parked epic.
    let planning_task = test_helpers::create_test_task(&db, &project.id, &parked.id).await;

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
    process_finalize_payload(&payload, "submit_grooming", &planning_task.id, &ctx).await;

    // The durable edge must exist and the gate must see an open blocker.
    assert!(
        epic_repo.has_unresolved_blockers(&parked.id).await.unwrap(),
        "blocked_on must durably record an epic-blocker edge"
    );
    let blockers = epic_repo.list_blockers(&parked.id).await.unwrap();
    assert_eq!(blockers.len(), 1);
    assert_eq!(blockers[0].epic_id, blocker.id);

    // Idempotent: re-declaring the same blocker must not error or duplicate.
    process_finalize_payload(&payload, "submit_grooming", &planning_task.id, &ctx).await;
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
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = test_helpers::create_test_project(&db).await;
    let parked = test_helpers::create_test_epic(&db, &project.id).await;
    let planning_task = test_helpers::create_test_task(&db, &project.id, &parked.id).await;

    let payload = Some(serde_json::json!({
        "tasks_reviewed": [],
        "blocked_on": ["does-not-exist"],
    }));
    process_finalize_payload(&payload, "submit_grooming", &planning_task.id, &ctx).await;

    let epic_repo = djinn_db::EpicRepository::new(db.clone(), ctx.event_bus.clone());
    assert!(
        !epic_repo.has_unresolved_blockers(&parked.id).await.unwrap(),
        "unresolvable blocked_on ref must not record a blocker"
    );
}

#[tokio::test]
async fn submit_grooming_malformed_payload_does_not_crash() {
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());

    // tasks_reviewed items missing required "action" field — SubmitGrooming itself
    // has tasks_reviewed as #[serde(default)] Vec, so malformed items are the issue.
    // Since tasks_reviewed has #[serde(default)], this will succeed with empty vec.
    // Test a completely invalid payload type instead.
    let payload = Some(serde_json::json!("not-an-object"));
    process_finalize_payload(&payload, "submit_grooming", "project:x:planner", &ctx).await;
}

// ─── no-op cases ──────────────────────────────────────────────────────────

#[tokio::test]
async fn none_payload_is_a_noop() {
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    // Should not panic or error.
    process_finalize_payload(&None, "submit_work", "any-task-id", &ctx).await;
}

#[tokio::test]
async fn unknown_finalize_tool_is_a_noop() {
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let payload = Some(serde_json::json!({"anything": "here"}));
    process_finalize_payload(&payload, "submit_unknown", "any-task-id", &ctx).await;
}

// ─── auto-submit review metadata persistence ────────────────────────────

#[tokio::test]
async fn submit_work_with_auto_submit_metadata_records_model_called_true() {
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

    // Create a task_run so the metadata can reference it.
    let run_id = uuid::Uuid::now_v7().to_string();
    TaskRunRepository::new(db.clone())
        .create(djinn_db::repositories::task_run::CreateTaskRunParams {
            id: &run_id,
            project_id: &project.id,
            task_id: &task.id,
            trigger_type: djinn_core::models::TaskRunTrigger::NewTask.as_str(),
            status: None,
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .expect("create task run");

    let payload = Some(serde_json::json!({
        "task_id": task.short_id,
        "commit_title": "feat: model submitted",
        "summary": "model called submit_work with review metadata",
        "files_changed": ["src/main.rs"],
        "remaining_concerns": [],
        "auto_submit_review_metadata": {
            "task_run_id": run_id,
            "trigger_reason": "idle",
            "diff_fingerprint": "abc123",
            "verify_source": "ci",
            "verify_run_id": "ci-42",
            "verify_timestamp": "2026-07-01T10:00:00.000Z",
            "session_id": "sess-1",
            "model_id": "model-1",
            "no_progress_streak": 2
        }
    }));

    // Called via process_finalize_payload_with_outcome — this is the normal
    // model-called submit_work path. The `model_called_submit_work` flag
    // should be `true` in the persisted record.
    let ok = process_finalize_payload_with_outcome(&payload, "submit_work", &task.id, &ctx).await;
    assert!(ok);

    // work_submitted activity should be logged.
    let task_repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
    let entries = task_repo.list_activity(&task.id).await.unwrap();
    assert!(entries.iter().any(|e| e.event_type == "work_submitted"));

    // Auto-submit review record should be persisted with model_called=true.
    let records = djinn_db::repositories::verify_run::AutoSubmitReviewRepository::new(db)
        .list_for_task_run(&run_id)
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    assert!(records[0].model_called_submit_work);
    assert_eq!(records[0].trigger_reason, "idle");
    assert_eq!(records[0].diff_fingerprint, "abc123");
    assert_eq!(records[0].verify_source.as_deref(), Some("ci"));
    assert_eq!(records[0].verify_run_id.as_deref(), Some("ci-42"));
    assert_eq!(
        records[0].verify_timestamp.as_deref(),
        Some("2026-07-01T10:00:00.000Z")
    );
    assert_eq!(records[0].session_id.as_deref(), Some("sess-1"));
    assert_eq!(records[0].model_id.as_deref(), Some("model-1"));
    assert_eq!(records[0].no_progress_streak, 2);
}

#[tokio::test]
async fn auto_submit_payload_records_model_called_false() {
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

    let run_id = uuid::Uuid::now_v7().to_string();
    TaskRunRepository::new(db.clone())
        .create(djinn_db::repositories::task_run::CreateTaskRunParams {
            id: &run_id,
            project_id: &project.id,
            task_id: &task.id,
            trigger_type: djinn_core::models::TaskRunTrigger::NewTask.as_str(),
            status: None,
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .expect("create task run");

    let payload = serde_json::json!({
        "task_id": task.short_id,
        "commit_title": "auto-submit verified worker diff",
        "summary": "Auto-submitted eligible green exact diff.",
        "files_changed": ["src/lib.rs"],
        "remaining_concerns": [],
        "auto_submit_review_metadata": {
            "task_run_id": run_id,
            "trigger_reason": "controlled_termination",
            "diff_fingerprint": "diff-789",
            "verify_source": "worker",
            "verify_run_id": "worker-run-5",
            "verify_timestamp": "2026-07-02T08:00:00.000Z",
            "session_id": "sess-5",
            "model_id": "model-5",
            "no_progress_streak": 4
        }
    });

    // Called via process_auto_submit_payload — this is the auto-submit
    // path. The `model_called_submit_work` flag should be `false`.
    let ok = process_auto_submit_payload(&payload, &task.id, &ctx).await;
    assert!(ok);

    let records = djinn_db::repositories::verify_run::AutoSubmitReviewRepository::new(db)
        .list_for_task_run(&run_id)
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    assert!(!records[0].model_called_submit_work);
    assert_eq!(records[0].trigger_reason, "controlled_termination");
    assert_eq!(records[0].diff_fingerprint, "diff-789");
    assert_eq!(records[0].verify_source.as_deref(), Some("worker"));
    assert_eq!(records[0].verify_run_id.as_deref(), Some("worker-run-5"));
    assert_eq!(
        records[0].verify_timestamp.as_deref(),
        Some("2026-07-02T08:00:00.000Z")
    );
    assert_eq!(records[0].session_id.as_deref(), Some("sess-5"));
    assert_eq!(records[0].model_id.as_deref(), Some("model-5"));
    assert_eq!(records[0].no_progress_streak, 4);
}

// ── rejected submission fingerprint persistence ────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_review_records_fingerprint_when_worktree_has_diff() {
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

    let worktree = init_git_repo_with_dirty_file();
    let _run_id = create_run_with_workspace(
        &db,
        &project.id,
        &task.id,
        Some(worktree.path().to_str().unwrap()),
    )
    .await;

    let payload = Some(serde_json::json!({
        "task_id": task.id,
        "verdict": "rejected",
        "acceptance_criteria": [],
        "feedback": "missing edge case handling"
    }));

    process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;

    let integrity_repo = TaskRejectedSubmissionIntegrityRepository::new(db);
    let latest = integrity_repo
        .latest_for_task(&task.id)
        .await
        .unwrap()
        .expect("expected rejected integrity record after rejected review");

    assert_eq!(
        latest.verdict_kind,
        djinn_core::models::RejectedVerdictKind::ReviewerReject.as_str()
    );
    assert!(!latest.diff_fingerprint.is_empty());
    assert_eq!(latest.no_progress_streak, 1);
    assert!(latest.task_run_id.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_review_skips_persistence_when_worktree_is_nodiff() {
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

    // Create a clean git repo with no dirty changes.
    let dir = tempfile::Builder::new()
        .prefix("djinn-test-nodiff-")
        .tempdir()
        .expect("create temp dir");
    let run_git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run_git(&["init"]);
    run_git(&["config", "--local", "user.email", "test@test.com"]);
    run_git(&["config", "--local", "user.name", "Test User"]);
    run_git(&["config", "--local", "commit.gpgsign", "false"]);
    std::fs::write(dir.path().join("README.md"), "hello\n").expect("write readme");
    run_git(&["add", "README.md"]);
    run_git(&["commit", "-m", "init"]);
    run_git(&["branch", "-m", "main"]);
    // No dirty edits — NoDiff case.

    let _run_id = create_run_with_workspace(
        &db,
        &project.id,
        &task.id,
        Some(dir.path().to_str().unwrap()),
    )
    .await;

    let payload = Some(serde_json::json!({
        "task_id": task.id,
        "verdict": "rejected",
        "acceptance_criteria": [],
        "feedback": "needs more work"
    }));

    process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;

    let integrity_repo = TaskRejectedSubmissionIntegrityRepository::new(db);
    let latest = integrity_repo.latest_for_task(&task.id).await.unwrap();
    assert!(
        latest.is_none(),
        "NoDiff worktree must not produce a rejected fingerprint record"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_review_skips_persistence_when_no_worktree() {
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

    // Task run with no workspace_path (historical / no-worktree case).
    let _run_id = create_run_with_workspace(&db, &project.id, &task.id, None).await;

    let payload = Some(serde_json::json!({
        "task_id": task.id,
        "verdict": "rejected",
        "acceptance_criteria": [],
        "feedback": "no worktree available"
    }));

    process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;

    let integrity_repo = TaskRejectedSubmissionIntegrityRepository::new(db);
    let latest = integrity_repo.latest_for_task(&task.id).await.unwrap();
    assert!(
        latest.is_none(),
        "no-worktree case must not produce a rejected fingerprint record"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_review_does_not_record_rejected_fingerprint() {
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

    let worktree = init_git_repo_with_dirty_file();
    let _run_id = create_run_with_workspace(
        &db,
        &project.id,
        &task.id,
        Some(worktree.path().to_str().unwrap()),
    )
    .await;

    let payload = Some(serde_json::json!({
        "task_id": task.id,
        "verdict": "approved",
        "acceptance_criteria": [],
        "feedback": null
    }));

    process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;

    let integrity_repo = TaskRejectedSubmissionIntegrityRepository::new(db);
    let latest = integrity_repo.latest_for_task(&task.id).await.unwrap();
    assert!(
        latest.is_none(),
        "approved review must not record rejected fingerprint"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_fingerprint_persists_across_task_run_boundaries() {
    // Simulate: task run 1 records a rejected fingerprint, then a new
    // task run 2 is created (redispatch). The latest_for_task query
    // must still see the rejection from run 1.
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

    let worktree = init_git_repo_with_dirty_file();
    let run1_id = create_run_with_workspace(
        &db,
        &project.id,
        &task.id,
        Some(worktree.path().to_str().unwrap()),
    )
    .await;

    // Record the rejection via handle_submit_review.
    let payload = Some(serde_json::json!({
        "task_id": task.id,
        "verdict": "rejected",
        "acceptance_criteria": [],
        "feedback": "needs work"
    }));
    process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;

    let integrity_repo = TaskRejectedSubmissionIntegrityRepository::new(db.clone());
    let latest = integrity_repo
        .latest_for_task(&task.id)
        .await
        .unwrap()
        .expect("rejection must be recorded");
    assert_eq!(latest.task_run_id.as_deref(), Some(run1_id.as_str()));
    assert_eq!(latest.no_progress_streak, 1);

    // Create a new task run (simulating redispatch).
    let _run2_id = create_run_with_workspace(
        &db,
        &project.id,
        &task.id,
        Some(worktree.path().to_str().unwrap()),
    )
    .await;

    // The latest rejection should still be from run 1 (cross-run persistence).
    let latest_after_redispatch = integrity_repo
        .latest_for_task(&task.id)
        .await
        .unwrap()
        .expect("must persist across task run boundaries");
    assert_eq!(
        latest_after_redispatch.task_run_id.as_deref(),
        Some(run1_id.as_str()),
        "rejection from run 1 must survive redispatch to run 2"
    );
    assert_eq!(
        latest_after_redispatch.diff_fingerprint,
        latest.diff_fingerprint
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_rejected_integrity_entry_direct_call_increments_streak() {
    // Test the shared helper directly: two consecutive rejections should
    // increment the streak from 0→1→2.
    let db = test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

    let run_id = create_run_with_workspace(&db, &project.id, &task.id, None).await;

    // First rejection.
    record_rejected_integrity_entry(
        &task.id,
        &ctx,
        djinn_core::models::RejectedVerdictKind::ReviewerReject.as_str(),
        None,
        Some(&run_id),
        "sha256:first-reject",
    )
    .await;

    let integrity_repo = TaskRejectedSubmissionIntegrityRepository::new(db.clone());
    let latest = integrity_repo
        .latest_for_task(&task.id)
        .await
        .unwrap()
        .expect("first rejection must be recorded");
    assert_eq!(latest.no_progress_streak, 1);
    assert_eq!(latest.diff_fingerprint, "sha256:first-reject");

    // Second rejection (streak should be 2).
    record_rejected_integrity_entry(
        &task.id,
        &ctx,
        djinn_core::models::RejectedVerdictKind::ReviewerReject.as_str(),
        None,
        Some(&run_id),
        "sha256:second-reject",
    )
    .await;

    let latest2 = integrity_repo
        .latest_for_task(&task.id)
        .await
        .unwrap()
        .expect("second rejection must be recorded");
    assert_eq!(latest2.no_progress_streak, 2);
    assert_eq!(latest2.diff_fingerprint, "sha256:second-reject");
}
