//! Tests for the verify-run, auto-submit-review, and rejected-submission
//! integrity repositories.

use djinn_core::events::EventBus;
use djinn_core::models::{
    AutoSubmitTriggerReason, RejectedVerdictKind, TaskRunTrigger, VerifyResult, VerifySource,
};

use super::*;
use crate::repositories::epic::EpicRepository;
use crate::repositories::task_run::{CreateTaskRunParams, TaskRunRepository};

fn test_db() -> Database {
    Database::open_in_memory().unwrap()
}

/// Create a project + task via raw SQL, returns (project_id, task_id).
async fn create_task(db: &Database, bus: EventBus) -> (String, String) {
    let epic_repo = EpicRepository::new(db.clone(), bus);
    let epic = epic_repo
        .create("Epic", "", "", "", "", None)
        .await
        .unwrap();

    let task_id = uuid::Uuid::now_v7().to_string();
    let short_id = format!("t{}{}", &task_id[..6], &task_id[task_id.len() - 6..]);
    sqlx::query!(
        "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                            issue_type, priority, owner, status, continuation_count, labels, acceptance_criteria, memory_refs)
         VALUES ($1, $2, $3, $4, 'Task', '', '', 'task', 0, '', 'open', 0, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb)",
        task_id,
        epic.project_id,
        short_id,
        epic.id
    )
    .execute(db.pool())
    .await
    .unwrap();

    (epic.project_id, task_id)
}

/// Create a task_run, returns the run id.
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
        })
        .await
        .unwrap();
    id
}

fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

async fn record_complete_final_pass(
    repo: &VerifyRunRepository,
    task_run_id: &str,
    id: &str,
    fingerprint: &str,
) {
    let required_commands = [
        RequiredFinalVerificationCommand {
            descriptor_id: "fmt",
        },
        RequiredFinalVerificationCommand {
            descriptor_id: "test",
        },
    ];
    let ordered_commands = serde_json::json!([
        {"descriptor_id": "fmt", "result": "pass", "passed": true},
        {"descriptor_id": "test", "result": "pass", "passed": true}
    ]);
    let covered_checks = serde_json::json!(["format", "tests"]);
    let required_checks = vec!["format".to_owned(), "tests".to_owned()];
    let environment_identity = serde_json::json!({"runner": "test"});

    repo.record_eligible_final_verification_pass(RecordEligibleFinalVerificationPassParams {
        id,
        task_run_id,
        verify_source: VerifySource::Ci.as_str(),
        verify_run_id: "final-run",
        verification_attempt_id: "attempt-1",
        required_commands: &required_commands,
        ordered_commands: &ordered_commands,
        covered_checks: &covered_checks,
        required_checks: &required_checks,
        verification_input_fingerprint: fingerprint,
        manifest_version: "manifest-v1",
        environment_identity_json: &environment_identity,
        environment_identity_digest: "identity-digest-v1",
        environment_identity_version: "identity-v1",
        completed_at: "2025-01-15T10:00:00.000Z",
        diff_fingerprint: "legacy-audit-fingerprint",
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eligible_final_pass_rejects_partial_commands_without_a_row() {
    let db = test_db();
    let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
    let task_run_id = create_run(&db, &project_id, &task_id).await;
    let repo = VerifyRunRepository::new(db);
    let id = new_id();
    let required_commands = [RequiredFinalVerificationCommand {
        descriptor_id: "fmt",
    }];
    let partial_commands = serde_json::json!([{"result": "pass", "passed": true}]);
    let covered_checks = serde_json::json!(["format"]);
    let required_checks = vec!["format".to_owned()];
    let identity = serde_json::json!({"runner": "test"});

    assert!(
        repo.record_eligible_final_verification_pass(RecordEligibleFinalVerificationPassParams {
            id: &id,
            task_run_id: &task_run_id,
            verify_source: VerifySource::Ci.as_str(),
            verify_run_id: "partial-run",
            verification_attempt_id: "partial-attempt",
            required_commands: &required_commands,
            ordered_commands: &partial_commands,
            covered_checks: &covered_checks,
            required_checks: &required_checks,
            verification_input_fingerprint: "fingerprint-v1",
            manifest_version: "manifest-v1",
            environment_identity_json: &identity,
            environment_identity_digest: "digest-v1",
            environment_identity_version: "identity-v1",
            completed_at: "2025-01-15T10:00:00.000Z",
            diff_fingerprint: "audit-fingerprint",
        })
        .await
        .is_err()
    );
    assert!(repo.get(&id).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compatible_final_pass_lookup_is_task_and_fingerprint_scoped_and_newest() {
    let db = test_db();
    let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
    let task_run_id = create_run(&db, &project_id, &task_id).await;
    let (_, other_task_id) = create_task(&db, EventBus::noop()).await;
    let other_run_id = create_run(&db, &project_id, &other_task_id).await;
    let repo = VerifyRunRepository::new(db);
    let first_id = new_id();
    record_complete_final_pass(&repo, &task_run_id, &first_id, "fingerprint-v1").await;
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let newest_id = new_id();
    record_complete_final_pass(&repo, &task_run_id, &newest_id, "fingerprint-v1").await;
    record_complete_final_pass(&repo, &other_run_id, &new_id(), "fingerprint-v1").await;
    let required_checks = vec!["format".to_owned(), "tests".to_owned()];

    let hit = repo
        .latest_compatible_passing_final_verification(
            &task_id,
            "fingerprint-v1",
            "manifest-v1",
            "identity-v1",
            &required_checks,
        )
        .await
        .unwrap()
        .expect("matching final pass must be reusable");
    assert_eq!(hit.id, newest_id);
    assert!(
        repo.latest_compatible_passing_final_verification(
            &task_id,
            "fingerprint-other",
            "manifest-v1",
            "identity-v1",
            &required_checks,
        )
        .await
        .unwrap()
        .is_none(),
        "a different fingerprint on the same task must not reuse the pass"
    );
    assert!(
        repo.latest_compatible_passing_final_verification(
            &other_task_id,
            "fingerprint-other",
            "manifest-v1",
            "identity-v1",
            &required_checks,
        )
        .await
        .unwrap()
        .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compatible_final_pass_lookup_rejects_legacy_identity_rows() {
    let db = test_db();
    let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
    let task_run_id = create_run(&db, &project_id, &task_id).await;
    let repo = VerifyRunRepository::new(db);
    repo.create(CreateVerifyRunParams {
        id: &new_id(),
        task_run_id: &task_run_id,
        verify_source: VerifySource::Ci.as_str(),
        verify_run_id: "legacy-run",
        command_version: Some("legacy-command"),
        profile_version: Some("legacy-profile"),
        completed_at: "2025-01-15T10:00:00.000Z",
        result: VerifyResult::Pass.as_str(),
        diff_fingerprint: "fingerprint-v1",
        check_coverage: None,
    })
    .await
    .unwrap();
    let required_checks = vec!["format".to_owned()];
    assert!(
        repo.latest_compatible_passing_final_verification(
            &task_id,
            "fingerprint-v1",
            "manifest-v1",
            "identity-v1",
            &required_checks,
        )
        .await
        .unwrap()
        .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compatible_final_pass_lookup_does_not_reuse_another_tasks_pass() {
    let db = test_db();
    let (project_id, source_task_id) = create_task(&db, EventBus::noop()).await;
    let source_run_id = create_run(&db, &project_id, &source_task_id).await;
    let (_, target_task_id) = create_task(&db, EventBus::noop()).await;
    let repo = VerifyRunRepository::new(db);
    record_complete_final_pass(&repo, &source_run_id, &new_id(), "fingerprint-v1").await;
    let required_checks = vec!["format".to_owned(), "tests".to_owned()];

    assert!(
        repo.latest_compatible_passing_final_verification(
            &target_task_id,
            "fingerprint-v1",
            "manifest-v1",
            "identity-v1",
            &required_checks,
        )
        .await
        .unwrap()
        .is_none()
    );
}

// ── VerifyRunRepository tests ──────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eligible_final_pass_roundtrips_complete_generic_audit_projection() {
    let db = test_db();
    let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
    let task_run_id = create_run(&db, &project_id, &task_id).await;
    let repo = VerifyRunRepository::new(db);
    let id = new_id();

    record_complete_final_pass(&repo, &task_run_id, &id, "fingerprint-v1").await;

    let fetched = repo.get(&id).await.unwrap().expect("audit row must exist");
    let expected_commands = serde_json::json!([
        {"descriptor_id": "fmt", "result": "pass", "passed": true},
        {"descriptor_id": "test", "result": "pass", "passed": true}
    ]);
    let expected_coverage = serde_json::json!(["format", "tests"]);
    assert_eq!(fetched.source_phase.as_deref(), Some("final_verification"));
    assert_eq!(
        fetched.verification_attempt_id.as_deref(),
        Some("attempt-1")
    );
    assert_eq!(fetched.ordered_commands.as_ref(), Some(&expected_commands));
    assert_eq!(fetched.covered_checks.as_ref(), Some(&expected_coverage));
    assert_eq!(fetched.check_coverage.as_ref(), Some(&expected_coverage));
    assert_eq!(
        fetched.verification_input_fingerprint.as_deref(),
        Some("fingerprint-v1")
    );
    assert_eq!(fetched.manifest_version.as_deref(), Some("manifest-v1"));
    assert_eq!(
        fetched.environment_identity_json,
        Some(serde_json::json!({"runner": "test"}))
    );
    assert_eq!(
        fetched.environment_identity_digest.as_deref(),
        Some("identity-digest-v1")
    );
    assert_eq!(
        fetched.environment_identity_version.as_deref(),
        Some("identity-v1")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_run_create_and_get_roundtrips() {
    let db = test_db();
    let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
    let task_run_id = create_run(&db, &project_id, &task_id).await;
    let repo = VerifyRunRepository::new(db);

    let coverage = serde_json::json!({"lint": true, "test": true, "typecheck": false});
    let id = new_id();
    let created = repo
        .create(CreateVerifyRunParams {
            id: &id,
            task_run_id: &task_run_id,
            verify_source: VerifySource::Ci.as_str(),
            verify_run_id: "run-12345",
            command_version: Some("1.2.0"),
            profile_version: Some("v3"),
            completed_at: "2025-01-15T10:30:00.000Z",
            result: VerifyResult::Pass.as_str(),
            diff_fingerprint: "abc123def456",
            check_coverage: Some(&coverage),
        })
        .await
        .unwrap();

    assert_eq!(created.id, id);
    assert_eq!(created.task_run_id, task_run_id);
    assert_eq!(created.verify_source, VerifySource::Ci.as_str());
    assert_eq!(created.verify_run_id, "run-12345");
    assert_eq!(created.command_version.as_deref(), Some("1.2.0"));
    assert_eq!(created.profile_version.as_deref(), Some("v3"));
    assert_eq!(created.completed_at, "2025-01-15T10:30:00.000Z");
    assert_eq!(created.result, VerifyResult::Pass.as_str());
    assert_eq!(created.diff_fingerprint, "abc123def456");
    assert_eq!(created.check_coverage.as_ref(), Some(&coverage));
    assert!(!created.created_at.is_empty());

    let fetched = repo.get(&id).await.unwrap().expect("must exist");
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.verify_source, created.verify_source);
    assert_eq!(fetched.check_coverage.as_ref(), Some(&coverage));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_run_get_returns_none_for_missing() {
    let db = test_db();
    let repo = VerifyRunRepository::new(db);

    let missing = repo.get("nonexistent-id").await.unwrap();
    assert!(missing.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_run_latest_for_task_run_returns_most_recent() {
    let db = test_db();
    let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
    let task_run_id = create_run(&db, &project_id, &task_id).await;
    let repo = VerifyRunRepository::new(db);

    // Insert first (older) verify run.
    let first_id = new_id();
    repo.create(CreateVerifyRunParams {
        id: &first_id,
        task_run_id: &task_run_id,
        verify_source: VerifySource::Local.as_str(),
        verify_run_id: "local-run-1",
        command_version: None,
        profile_version: None,
        completed_at: "2025-01-15T09:00:00.000Z",
        result: VerifyResult::Fail.as_str(),
        diff_fingerprint: "old_fingerprint",
        check_coverage: None,
    })
    .await
    .unwrap();

    // Small stagger so created_at ordering is deterministic.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Insert second (newer) verify run.
    let second_id = new_id();
    repo.create(CreateVerifyRunParams {
        id: &second_id,
        task_run_id: &task_run_id,
        verify_source: VerifySource::Ci.as_str(),
        verify_run_id: "ci-run-2",
        command_version: Some("2.0.0"),
        profile_version: Some("v4"),
        completed_at: "2025-01-15T10:00:00.000Z",
        result: VerifyResult::Pass.as_str(),
        diff_fingerprint: "new_fingerprint",
        check_coverage: None,
    })
    .await
    .unwrap();

    let latest = repo
        .latest_for_task_run(&task_run_id)
        .await
        .unwrap()
        .expect("must exist");
    assert_eq!(latest.id, second_id, "latest must be the most recent");
    assert_eq!(latest.result, VerifyResult::Pass.as_str());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_run_latest_pass_for_task_and_fingerprint_cache_lookup() {
    let db = test_db();
    let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
    let first_run = create_run(&db, &project_id, &task_id).await;
    let repo = VerifyRunRepository::new(db.clone());

    // A FAIL for the fingerprint must not count as a cache hit.
    repo.create(CreateVerifyRunParams {
        id: &new_id(),
        task_run_id: &first_run,
        verify_source: VerifySource::Local.as_str(),
        verify_run_id: "gate",
        command_version: None,
        profile_version: None,
        completed_at: "2025-01-15T09:00:00.000Z",
        result: VerifyResult::Fail.as_str(),
        diff_fingerprint: "fp-abc",
        check_coverage: None,
    })
    .await
    .unwrap();
    assert!(
        repo.latest_pass_for_task_and_fingerprint(&task_id, "fp-abc")
            .await
            .unwrap()
            .is_none(),
        "a failing run is not a cache hit"
    );

    // A PASS recorded on a DIFFERENT task run for the same task+fingerprint
    // must be reloadable by task_id (cross-run cache).
    let second_run = create_run(&db, &project_id, &task_id).await;
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let pass_id = new_id();
    repo.create(CreateVerifyRunParams {
        id: &pass_id,
        task_run_id: &second_run,
        verify_source: VerifySource::Local.as_str(),
        verify_run_id: "gate",
        command_version: None,
        profile_version: None,
        completed_at: "2025-01-15T10:00:00.000Z",
        result: VerifyResult::Pass.as_str(),
        diff_fingerprint: "fp-abc",
        check_coverage: Some(&serde_json::json!({"clippy_all_targets": true})),
    })
    .await
    .unwrap();

    let hit = repo
        .latest_pass_for_task_and_fingerprint(&task_id, "fp-abc")
        .await
        .unwrap()
        .expect("green run must be a cache hit");
    assert_eq!(hit.id, pass_id);
    assert_eq!(hit.result, VerifyResult::Pass.as_str());

    // A different fingerprint is a cache miss.
    assert!(
        repo.latest_pass_for_task_and_fingerprint(&task_id, "fp-other")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_run_latest_for_task_run_returns_none_when_empty() {
    let db = test_db();
    let repo = VerifyRunRepository::new(db);

    let latest = repo.latest_for_task_run("nonexistent-run").await.unwrap();
    assert!(latest.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_run_list_for_task_run_returns_descending() {
    let db = test_db();
    let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
    let task_run_id = create_run(&db, &project_id, &task_id).await;
    let repo = VerifyRunRepository::new(db.clone());

    let mut ids: Vec<String> = Vec::new();
    for i in 0..3 {
        let id = new_id();
        repo.create(CreateVerifyRunParams {
            id: &id,
            task_run_id: &task_run_id,
            verify_source: VerifySource::Worker.as_str(),
            verify_run_id: &format!("run-{i}"),
            command_version: None,
            profile_version: None,
            completed_at: "2025-01-15T10:00:00.000Z",
            result: VerifyResult::Pass.as_str(),
            diff_fingerprint: &format!("fp-{i}"),
            check_coverage: None,
        })
        .await
        .unwrap();
        ids.push(id);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // Noise on a different task_run.
    let other_run_id = create_run(&db, &project_id, &task_id).await;
    let noise_id = new_id();
    repo.create(CreateVerifyRunParams {
        id: &noise_id,
        task_run_id: &other_run_id,
        verify_source: VerifySource::Ci.as_str(),
        verify_run_id: "noise-run",
        command_version: None,
        profile_version: None,
        completed_at: "2025-01-15T10:00:00.000Z",
        result: VerifyResult::Pass.as_str(),
        diff_fingerprint: "noise",
        check_coverage: None,
    })
    .await
    .unwrap();

    let runs = repo.list_for_task_run(&task_run_id).await.unwrap();
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0].id, ids[2], "newest first");
    assert_eq!(runs[2].id, ids[0], "oldest last");
    for run in &runs {
        assert_eq!(run.task_run_id, task_run_id);
        assert_ne!(run.id, noise_id);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_run_create_with_null_optional_versions() {
    let db = test_db();
    let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
    let task_run_id = create_run(&db, &project_id, &task_id).await;
    let repo = VerifyRunRepository::new(db);

    let id = new_id();
    let created = repo
        .create(CreateVerifyRunParams {
            id: &id,
            task_run_id: &task_run_id,
            verify_source: VerifySource::Worker.as_str(),
            verify_run_id: "worker-verify-1",
            command_version: None,
            profile_version: None,
            completed_at: "2025-01-15T10:00:00.000Z",
            result: VerifyResult::Error.as_str(),
            diff_fingerprint: "err_fp",
            check_coverage: None,
        })
        .await
        .unwrap();

    assert!(created.command_version.is_none());
    assert!(created.profile_version.is_none());
    assert!(created.check_coverage.is_none());
}

// ── AutoSubmitReviewRepository tests ───────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_submit_review_create_and_get_roundtrips() {
    let db = test_db();
    let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
    let task_run_id = create_run(&db, &project_id, &task_id).await;
    let repo = AutoSubmitReviewRepository::new(db);

    let id = new_id();
    let created = repo
        .create(CreateAutoSubmitReviewParams {
            id: &id,
            task_run_id: &task_run_id,
            trigger_reason: AutoSubmitTriggerReason::Idle.as_str(),
            diff_fingerprint: "diff_abc123",
            verify_source: Some(VerifySource::Ci.as_str()),
            verify_run_id: Some("ci-run-42"),
            verify_timestamp: Some("2025-01-15T10:00:00.000Z"),
            session_id: Some("sess-001"),
            model_id: Some("claude-sonnet-4-20250514"),
            no_progress_streak: 3,
            model_called_submit_work: false,
        })
        .await
        .unwrap();

    assert_eq!(created.id, id);
    assert_eq!(created.task_run_id, task_run_id);
    assert_eq!(
        created.trigger_reason,
        AutoSubmitTriggerReason::Idle.as_str()
    );
    assert_eq!(created.diff_fingerprint, "diff_abc123");
    assert_eq!(
        created.verify_source.as_deref(),
        Some(VerifySource::Ci.as_str())
    );
    assert_eq!(created.verify_run_id.as_deref(), Some("ci-run-42"));
    assert_eq!(
        created.verify_timestamp.as_deref(),
        Some("2025-01-15T10:00:00.000Z")
    );
    assert_eq!(created.session_id.as_deref(), Some("sess-001"));
    assert_eq!(
        created.model_id.as_deref(),
        Some("claude-sonnet-4-20250514")
    );
    assert_eq!(created.no_progress_streak, 3);
    assert!(!created.model_called_submit_work);
    assert!(!created.created_at.is_empty());

    let fetched = repo.get(&id).await.unwrap().expect("must exist");
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.trigger_reason, created.trigger_reason);
    assert_eq!(fetched.no_progress_streak, created.no_progress_streak);
    assert_eq!(
        fetched.model_called_submit_work,
        created.model_called_submit_work
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_submit_review_get_returns_none_for_missing() {
    let db = test_db();
    let repo = AutoSubmitReviewRepository::new(db);

    let missing = repo.get("nonexistent-id").await.unwrap();
    assert!(missing.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_submit_review_list_for_task_run_returns_descending() {
    let db = test_db();
    let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
    let task_run_id = create_run(&db, &project_id, &task_id).await;
    let repo = AutoSubmitReviewRepository::new(db.clone());

    let mut ids: Vec<String> = Vec::new();
    for reason in [
        AutoSubmitTriggerReason::Idle,
        AutoSubmitTriggerReason::NoProgress,
        AutoSubmitTriggerReason::SoftDeadline,
    ] {
        let id = new_id();
        repo.create(CreateAutoSubmitReviewParams {
            id: &id,
            task_run_id: &task_run_id,
            trigger_reason: reason.as_str(),
            diff_fingerprint: "fp",
            verify_source: None,
            verify_run_id: None,
            verify_timestamp: None,
            session_id: None,
            model_id: None,
            no_progress_streak: 0,
            model_called_submit_work: false,
        })
        .await
        .unwrap();
        ids.push(id);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // Noise row on a different task_run.
    let other_run_id = create_run(&db, &project_id, &task_id).await;
    let noise_id = new_id();
    repo.create(CreateAutoSubmitReviewParams {
        id: &noise_id,
        task_run_id: &other_run_id,
        trigger_reason: AutoSubmitTriggerReason::Looping.as_str(),
        diff_fingerprint: "noise_fp",
        verify_source: None,
        verify_run_id: None,
        verify_timestamp: None,
        session_id: None,
        model_id: None,
        no_progress_streak: 0,
        model_called_submit_work: false,
    })
    .await
    .unwrap();

    let reviews = repo.list_for_task_run(&task_run_id).await.unwrap();
    assert_eq!(reviews.len(), 3);
    assert_eq!(reviews[0].id, ids[2], "newest first");
    assert_eq!(reviews[2].id, ids[0], "oldest last");
    for review in &reviews {
        assert_eq!(review.task_run_id, task_run_id);
        assert_ne!(review.id, noise_id);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_submit_review_model_called_submit_work_true() {
    let db = test_db();
    let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
    let task_run_id = create_run(&db, &project_id, &task_id).await;
    let repo = AutoSubmitReviewRepository::new(db);

    let id = new_id();
    let created = repo
        .create(CreateAutoSubmitReviewParams {
            id: &id,
            task_run_id: &task_run_id,
            trigger_reason: AutoSubmitTriggerReason::ControlledTermination.as_str(),
            diff_fingerprint: "fp_term",
            verify_source: Some(VerifySource::Worker.as_str()),
            verify_run_id: Some("w-run-99"),
            verify_timestamp: Some("2025-01-15T12:00:00.000Z"),
            session_id: Some("sess-term"),
            model_id: Some("gpt-4o"),
            no_progress_streak: 5,
            model_called_submit_work: true,
        })
        .await
        .unwrap();

    assert!(created.model_called_submit_work);
    assert_eq!(created.no_progress_streak, 5);
    assert_eq!(
        created.trigger_reason,
        AutoSubmitTriggerReason::ControlledTermination.as_str()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_submit_review_null_optional_fields() {
    let db = test_db();
    let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
    let task_run_id = create_run(&db, &project_id, &task_id).await;
    let repo = AutoSubmitReviewRepository::new(db);

    let id = new_id();
    let created = repo
        .create(CreateAutoSubmitReviewParams {
            id: &id,
            task_run_id: &task_run_id,
            trigger_reason: AutoSubmitTriggerReason::Looping.as_str(),
            diff_fingerprint: "fp_loop",
            verify_source: None,
            verify_run_id: None,
            verify_timestamp: None,
            session_id: None,
            model_id: None,
            no_progress_streak: 0,
            model_called_submit_work: false,
        })
        .await
        .unwrap();

    assert!(created.verify_source.is_none());
    assert!(created.verify_run_id.is_none());
    assert!(created.verify_timestamp.is_none());
    assert!(created.session_id.is_none());
    assert!(created.model_id.is_none());
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
