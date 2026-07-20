//! Tests for the audit item scheduler.

use super::*;
use djinn_db::{
    AuditSamplerRepository, AuditStratum, CreateSampleFrameParams, CreateSamplePolicyParams,
    CreateSelectionParams, Database, EpicRepository, SelectionRow, UserRepository,
};
use serde_json::json;
use std::sync::atomic::{AtomicI64, Ordering};

static NEXT_SOURCE_GITHUB_ID: AtomicI64 = AtomicI64::new(9_000_000_000);

fn test_config() -> AuditSchedulerConfig {
    AuditSchedulerConfig {
        enabled: true,
        max_open_audits: 3,
        // Ordinary scheduler fixtures use fixed timestamps for deterministic
        // ordering. Keep their SLO horizon intentionally large so they do not
        // turn into wall-clock-dependent backlog tests; the dedicated SLO test
        // below overrides this with the production-like seven-day horizon.
        slo_age_hours: 100_000,
        per_tick_budget: 2,
        min_materialization_interval_hours: 0,
    }
}

fn test_db() -> Database {
    Database::open_in_memory().expect("in-memory db")
}

/// Seed a policy, frame, merged change, and selection for testing.
async fn seed_selection(
    db: &Database,
    project_id: &str,
    merge_sha: &str,
    stratum: &str,
    position: i32,
    created_at: &str,
) -> SelectionRow {
    let repo = AuditSamplerRepository::new(db.clone());
    let source_github_id = NEXT_SOURCE_GITHUB_ID.fetch_add(1, Ordering::Relaxed);
    let source_user = UserRepository::new(db.clone())
        .upsert_from_github(
            source_github_id,
            &format!("audit-source-{source_github_id}"),
            None,
            None,
        )
        .await
        .unwrap();
    let source_user_id = source_user.id;
    let source_task_id = djinn_db::test_support::seed_task_row(
        db,
        djinn_db::test_support::UsageTestTaskSeed {
            project_id,
            status: "closed",
            close_reason: None,
            total_reopen_count: 0,
        },
    )
    .await;
    TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .set_created_by_user_id(&source_task_id, &source_user_id)
        .await
        .unwrap();

    // Seed or reuse one policy per project; repeated helper calls for the
    // same project must not violate uq_audit_sample_policies_project_rev.
    let policy = match repo.get_latest_sample_policy(project_id).await.unwrap() {
        Some(policy) => policy,
        None => repo
            .create_sample_policy(CreateSamplePolicyParams {
                project_id,
                revision: 1,
                policy_json: &json!({"unflagged_rate": 0.02, "autonomous_rate": 0.10}),
            })
            .await
            .unwrap(),
    };

    // Seed a deterministic frame per helper invocation. Several scheduler
    // tests intentionally create multiple pending selections for one project,
    // so reusing the same (project, window, revision) would violate the frame
    // uniqueness constraint before the scheduler gets to exercise cadence or
    // backlog behavior.
    let window_start = "2026-06-24T00:00:00Z";
    let window_end = "2026-07-01T00:00:00Z";
    let frame_revision = repo
        .list_sample_frames_in_window(project_id, window_start, window_end)
        .await
        .unwrap()
        .len() as i32
        + 1;
    let content_hash = format!("test-frame-{merge_sha}-{frame_revision}");
    let frame = repo
        .create_sample_frame(CreateSampleFrameParams {
            project_id,
            policy_id: &policy.id,
            window_start,
            window_end,
            revision: frame_revision,
            eligible_change_ids: &json!([merge_sha]),
            content_hash: Some(&content_hash),
            exclusion_counts: &json!({}),
            exclusion_reasons: &json!([]),
            sealed_at: "2026-07-01T00:05:00Z",
        })
        .await
        .unwrap();

    // Seed merged change.
    let mc = repo
        .upsert_merged_change(djinn_db::UpsertMergedChangeParams {
            project_id,
            task_id: Some(&source_task_id),
            pr_number: Some(42),
            head_sha: Some("head-sha-1"),
            merge_commit_sha: merge_sha,
            merged_at: "2026-07-01T00:00:00Z",
            gate_outcome: "pass",
            gate_provenance: Some(&json!({"tripwire": "none"})),
            release_provenance: None,
            stratum: if stratum == "autonomous_release" {
                AuditStratum::AutonomousRelease
            } else {
                AuditStratum::UnflaggedMerged
            },
            excluded: false,
            exclusion_reason: None,
        })
        .await
        .unwrap();

    // Seed selection via repository with specific created_at.
    repo.create_selection(CreateSelectionParams {
        frame_id: &frame.id,
        merged_change_id: &mc.id,
        stratum: if stratum == "autonomous_release" {
            AuditStratum::AutonomousRelease
        } else {
            AuditStratum::UnflaggedMerged
        },
        selected_position: position,
        algorithm: "hmac-sha256-counter-v1",
        seed_commitment: "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890",
        seed_reveal: Some("fedcba0987654321fedcba0987654321fedcba0987654321fedcba0987654321"),
        replay_data: &json!([]),
        audit_task_id: None,
        created_at: Some(created_at),
    })
    .await
    .unwrap()
}

/// Create an open task to count toward max_open_audits.
async fn create_open_task(db: &Database, project_id: &str) -> String {
    djinn_db::test_support::seed_task_row(
        db,
        djinn_db::test_support::UsageTestTaskSeed {
            project_id,
            status: "open",
            close_reason: None,
            total_reopen_count: 0,
        },
    )
    .await
}

// ── Test: normal materialization ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_materialization_creates_task_and_links_selection() {
    let db = test_db();
    let project_id = uuid::Uuid::now_v7().to_string();
    djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;

    // Seed one selection.
    let sel = seed_selection(
        &db,
        &project_id,
        "sha-normal-001",
        "unflagged_merged",
        0,
        "2026-07-09T12:00:00Z",
    )
    .await;

    let config = test_config();
    let audit_repo = AuditSamplerRepository::new(db.clone());
    let events = djinn_core::events::EventBus::noop();
    let task_repo = TaskRepository::new(db.clone(), events.clone());
    let epic_repo = EpicRepository::new(db.clone(), events);

    let result = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;

    assert!(result.ran);
    assert!(!result.paused);
    assert_eq!(result.materialized_items.len(), 1);
    assert_eq!(result.total_unmaterialized, 1);

    let mat = &result.materialized_items[0];
    assert_eq!(mat.selection_id, sel.id);
    assert_eq!(mat.stratum, "unflagged_merged");

    // Verify the selection now has an audit_task_id.
    let updated = audit_repo
        .get_selection_by_id(&sel.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated.audit_task_id.as_deref(),
        Some(mat.audit_task_id.as_str())
    );

    // Verify the created task is an ordinary task (not a verification task).
    let task = task_repo.get(&mat.audit_task_id).await.unwrap().unwrap();
    assert_eq!(task.issue_type, "task");
    assert!(task.description.contains("Audit Review"));
    assert!(task.description.contains("sha-normal-001"));
    assert!(task.description.contains("hmac-sha256-counter-v1"));
}

// ── Test: idempotency across repeated ticks ───────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_tick_does_not_rematerialize() {
    let db = test_db();
    let project_id = uuid::Uuid::now_v7().to_string();
    djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;

    seed_selection(
        &db,
        &project_id,
        "sha-idempotent-001",
        "unflagged_merged",
        0,
        "2026-07-09T12:00:00Z",
    )
    .await;

    let config = test_config();
    let audit_repo = AuditSamplerRepository::new(db.clone());
    let events = djinn_core::events::EventBus::noop();
    let task_repo = TaskRepository::new(db.clone(), events.clone());
    let epic_repo = EpicRepository::new(db.clone(), events);

    // First tick: materializes.
    let result1 = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;
    assert_eq!(result1.materialized_items.len(), 1);

    // Second tick: nothing to materialize.
    let result2 = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;
    assert!(result2.materialized_items.is_empty());
    assert!(!result2.paused);
    assert_eq!(result2.total_unmaterialized, 0);
}

// ── Test: cadence/rate gate ────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cadence_gate_prevents_thirty_second_tick_overmaterialization() {
    let db = test_db();
    let project_id = uuid::Uuid::now_v7().to_string();
    djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;

    for i in 0..3 {
        seed_selection(
            &db,
            &project_id,
            &format!("sha-cadence-{i}"),
            "unflagged_merged",
            i,
            "2026-07-09T12:00:00Z",
        )
        .await;
    }

    let config = AuditSchedulerConfig {
        per_tick_budget: 1,
        min_materialization_interval_hours: 84,
        ..test_config()
    };
    let audit_repo = AuditSamplerRepository::new(db.clone());
    let events = djinn_core::events::EventBus::noop();
    let task_repo = TaskRepository::new(db.clone(), events.clone());
    let epic_repo = EpicRepository::new(db.clone(), events);

    let first = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;
    assert_eq!(first.materialized_items.len(), 1);

    let second = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;
    assert!(second.materialized_items.is_empty());
    assert!(!second.paused);
    assert_eq!(second.total_unmaterialized, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injectable_fast_cadence_allows_repeated_materialization() {
    let db = test_db();
    let project_id = uuid::Uuid::now_v7().to_string();
    djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;

    for i in 0..2 {
        seed_selection(
            &db,
            &project_id,
            &format!("sha-fast-cadence-{i}"),
            "unflagged_merged",
            i,
            "2026-07-09T12:00:00Z",
        )
        .await;
    }

    let config = AuditSchedulerConfig {
        per_tick_budget: 1,
        min_materialization_interval_hours: 0,
        ..test_config()
    };
    let audit_repo = AuditSamplerRepository::new(db.clone());
    let events = djinn_core::events::EventBus::noop();
    let task_repo = TaskRepository::new(db.clone(), events.clone());
    let epic_repo = EpicRepository::new(db.clone(), events);

    let first = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;
    let second = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;

    assert_eq!(first.materialized_items.len(), 1);
    assert_eq!(second.materialized_items.len(), 1);
    assert_ne!(
        first.materialized_items[0].selection_id,
        second.materialized_items[0].selection_id
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cadence_gate_recovers_when_persisted_interval_elapsed() {
    let db = test_db();
    let project_id = uuid::Uuid::now_v7().to_string();
    djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;

    let pending = seed_selection(
        &db,
        &project_id,
        "sha-cadence-recovery-pending",
        "unflagged_merged",
        0,
        "2026-07-09T12:00:00Z",
    )
    .await;

    let old_task_id = create_open_task(&db, &project_id).await;
    let old_selection = seed_selection(
        &db,
        &project_id,
        "sha-cadence-recovery-old",
        "unflagged_merged",
        99,
        "2025-01-01T00:00:00Z",
    )
    .await;
    AuditSamplerRepository::new(db.clone())
        .set_selection_audit_task(&old_selection.id, &old_task_id)
        .await
        .unwrap();
    djinn_db::test_support::close_task_at(&db, &old_task_id, "2025-01-02T00:00:00Z").await;

    let config = AuditSchedulerConfig {
        per_tick_budget: 1,
        min_materialization_interval_hours: 1,
        ..test_config()
    };
    let audit_repo = AuditSamplerRepository::new(db.clone());
    let events = djinn_core::events::EventBus::noop();
    let task_repo = TaskRepository::new(db.clone(), events.clone());
    let epic_repo = EpicRepository::new(db.clone(), events);

    let result = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;
    assert_eq!(result.materialized_items.len(), 1);
    assert_eq!(result.materialized_items[0].selection_id, pending.id);
}

// ── Test: max-open pause ──────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_open_audits_triggers_pause() {
    let db = test_db();
    let project_id = uuid::Uuid::now_v7().to_string();
    djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;

    // Seed a selection that won't be materialized because cap is hit.
    seed_selection(
        &db,
        &project_id,
        "sha-maxopen-001",
        "unflagged_merged",
        0,
        "2026-07-09T12:00:00Z",
    )
    .await;

    // Create 3 open tasks (matching max_open_audits = 3).
    for i in 0..3 {
        let tid = create_open_task(&db, &project_id).await;
        // Also link them to selections so count_open_audit_tasks counts them.
        let sel = seed_selection(
            &db,
            &project_id,
            &format!("sha-maxopen-linked-{i}"),
            "unflagged_merged",
            i + 10,
            "2026-07-09T10:00:00Z",
        )
        .await;
        AuditSamplerRepository::new(db.clone())
            .set_selection_audit_task(&sel.id, &tid)
            .await
            .unwrap();
    }

    let config = test_config(); // max_open_audits = 3
    let audit_repo = AuditSamplerRepository::new(db.clone());
    let events = djinn_core::events::EventBus::noop();
    let task_repo = TaskRepository::new(db.clone(), events.clone());
    let epic_repo = EpicRepository::new(db.clone(), events);

    let result = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;

    assert!(result.ran);
    assert!(result.paused);
    assert_eq!(result.pause_reason, Some(BacklogPauseReason::MaxOpenAudits));
    assert!(result.materialized_items.is_empty());
    assert_eq!(result.total_unmaterialized, 1);
}

// ── Test: SLO pause ───────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slo_age_exceeded_triggers_pause() {
    let db = test_db();
    let project_id = uuid::Uuid::now_v7().to_string();
    djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;

    // Seed a selection with created_at 20 days ago (exceeds 7-day SLO).
    seed_selection(
        &db,
        &project_id,
        "sha-slo-001",
        "unflagged_merged",
        0,
        "2026-06-20T12:00:00Z", // ~20 days before now
    )
    .await;

    let config = AuditSchedulerConfig {
        slo_age_hours: 168, // 7 days
        ..test_config()
    };
    let audit_repo = AuditSamplerRepository::new(db.clone());
    let events = djinn_core::events::EventBus::noop();
    let task_repo = TaskRepository::new(db.clone(), events.clone());
    let epic_repo = EpicRepository::new(db.clone(), events);

    let result = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;

    assert!(result.ran);
    assert!(result.paused);
    assert_eq!(
        result.pause_reason,
        Some(BacklogPauseReason::SLOAgeExceeded)
    );
    assert!(result.materialized_items.is_empty());
}

// ── Test: recovery after backlog falls below cap ──────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_after_backlog_falls_below_cap() {
    let db = test_db();
    let project_id = uuid::Uuid::now_v7().to_string();
    djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;

    // Seed a selection.
    let sel = seed_selection(
        &db,
        &project_id,
        "sha-recovery-001",
        "unflagged_merged",
        0,
        "2026-07-09T12:00:00Z",
    )
    .await;

    let config = AuditSchedulerConfig {
        max_open_audits: 1, // cap at 1
        ..test_config()
    };
    let audit_repo = AuditSamplerRepository::new(db.clone());
    let events = djinn_core::events::EventBus::noop();
    let task_repo = TaskRepository::new(db.clone(), events.clone());
    let epic_repo = EpicRepository::new(db.clone(), events);

    // First: create an open audit task to fill the cap.
    let existing_task_id = create_open_task(&db, &project_id).await;
    let existing_sel = seed_selection(
        &db,
        &project_id,
        "sha-recovery-existing",
        "unflagged_merged",
        99,
        "2026-07-09T10:00:00Z",
    )
    .await;
    audit_repo
        .set_selection_audit_task(&existing_sel.id, &existing_task_id)
        .await
        .unwrap();

    // First tick: should pause (cap reached).
    let result1 = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;
    assert!(result1.paused);
    assert_eq!(
        result1.pause_reason,
        Some(BacklogPauseReason::MaxOpenAudits)
    );

    // Close the existing task.
    djinn_db::test_support::close_task_at(&db, &existing_task_id, "2026-07-10T00:00:00Z").await;

    // Second tick: should succeed (cap no longer exceeded).
    let result2 = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;
    assert!(!result2.paused);
    assert_eq!(result2.materialized_items.len(), 1);
    assert_eq!(result2.materialized_items[0].selection_id, sel.id);
}

// ── Test: disabled scheduler ──────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_scheduler_does_nothing() {
    let db = test_db();
    let project_id = uuid::Uuid::now_v7().to_string();
    djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;

    seed_selection(
        &db,
        &project_id,
        "sha-disabled-001",
        "unflagged_merged",
        0,
        "2026-07-09T12:00:00Z",
    )
    .await;

    let config = AuditSchedulerConfig {
        enabled: false,
        ..test_config()
    };
    let audit_repo = AuditSamplerRepository::new(db.clone());
    let events = djinn_core::events::EventBus::noop();
    let task_repo = TaskRepository::new(db.clone(), events.clone());
    let epic_repo = EpicRepository::new(db.clone(), events);

    let result = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;

    assert!(!result.ran);
    assert!(result.materialized_items.is_empty());
}

// ── Test: provenance description ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_description_includes_provenance_data() {
    let db = test_db();
    let project_id = uuid::Uuid::now_v7().to_string();
    djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;

    seed_selection(
        &db,
        &project_id,
        "sha-provenance-001",
        "autonomous_release",
        0,
        "2026-07-09T12:00:00Z",
    )
    .await;

    let config = test_config();
    let audit_repo = AuditSamplerRepository::new(db.clone());
    let events = djinn_core::events::EventBus::noop();
    let task_repo = TaskRepository::new(db.clone(), events.clone());
    let epic_repo = EpicRepository::new(db.clone(), events);

    let result = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;

    assert_eq!(result.materialized_items.len(), 1);
    let task = task_repo
        .get(&result.materialized_items[0].audit_task_id)
        .await
        .unwrap()
        .unwrap();

    // Verify all required provenance fields are present in the description.
    let desc = &task.description;
    assert!(desc.contains("sha-provenance-001"), "merge SHA");
    assert!(desc.contains("autonomous_release"), "stratum");
    assert!(desc.contains("hmac-sha256-counter-v1"), "algorithm");
    assert!(desc.contains("Seed commitment"), "seed commitment label");
    assert!(desc.contains("revealed"), "seed status");
    assert!(desc.contains("Frame revision"), "frame revision");
    assert!(desc.contains("Policy revision"), "policy revision");
    assert!(desc.contains("Source task"), "source task label");
    assert!(desc.contains("42"), "PR number");
    assert!(desc.contains("head-sha-1"), "head SHA");
    assert!(desc.contains("Gate provenance"), "gate provenance section");
    assert!(
        desc.contains("Release provenance"),
        "release provenance section"
    );
}

// ── Test: ISO timestamp parsing ───────────────────────────────────────────

#[test]
fn parse_iso_timestamp_handles_common_formats() {
    // Standard format
    let ts = parse_iso_timestamp("2026-07-01T12:00:00Z");
    assert!(ts.is_some());

    // With milliseconds
    let ts = parse_iso_timestamp("2026-07-01T12:00:00.000Z");
    assert!(ts.is_some());

    // Without Z
    let ts = parse_iso_timestamp("2026-07-01T12:00:00");
    assert!(ts.is_some());

    // Invalid format
    let ts = parse_iso_timestamp("not-a-timestamp");
    assert!(ts.is_none());
}

#[test]
fn days_since_epoch_known_dates() {
    // 1970-01-01 = day 0
    assert_eq!(days_since_epoch(1970, 1, 1), Some(0));
    // 1970-01-02 = day 1
    assert_eq!(days_since_epoch(1970, 1, 2), Some(1));
    // 2024-03-01 is 19783 days from 1970-01-01 (54 years + 31 Jan + 29 Feb)
    assert_eq!(days_since_epoch(2024, 3, 1), Some(19783));
    // Invalid month
    assert_eq!(days_since_epoch(2024, 13, 1), None);
}

#[test]
fn check_slo_age_detects_old_selection() {
    // A selection from 30 days ago with a 7-day SLO should trigger.
    // We use a fixed timestamp well in the past.
    let old_ts = "2020-01-01T00:00:00Z";
    let result = check_slo_age(old_ts, 168);
    assert!(result.is_some());
    assert_eq!(result.unwrap(), BacklogPauseReason::SLOAgeExceeded);
}

#[test]
fn check_slo_age_ignores_recent_selection() {
    // A very recent timestamp should not trigger with a very large SLO.
    let recent = "2026-07-10T12:00:00Z";
    let result = check_slo_age(recent, 100_000);
    assert!(result.is_none());
}
// ── Test: crash-restart idempotency ──────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_restart_idempotency_atomic_materialization() {
    // Simulates the crash-restart scenario: the scheduler materializes a
    // selection, then a "restart" happens (second scheduler tick). The
    // atomic transaction ensures the second tick creates zero duplicate
    // tasks because list_unmaterialized_selections no longer returns the
    // already-linked selection.
    let db = test_db();
    let project_id = uuid::Uuid::now_v7().to_string();
    djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;

    seed_selection(
        &db,
        &project_id,
        "sha-crash-001",
        "unflagged_merged",
        0,
        "2026-07-09T12:00:00Z",
    )
    .await;

    let config = test_config();
    let audit_repo = AuditSamplerRepository::new(db.clone());
    let events = djinn_core::events::EventBus::noop();
    let task_repo = TaskRepository::new(db.clone(), events.clone());
    let epic_repo = EpicRepository::new(db.clone(), events);

    // "Tick 1" — succeeds (simulates normal materialization before crash).
    let result1 = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;
    assert_eq!(result1.materialized_items.len(), 1);
    let first_task_id = result1.materialized_items[0].audit_task_id.clone();

    // Verify the selection is now linked.
    let sel_id = result1.materialized_items[0].selection_id.clone();
    let updated = audit_repo
        .get_selection_by_id(&sel_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated.audit_task_id.as_deref(),
        Some(first_task_id.as_str())
    );

    // "Tick 2" after restart — must not re-materialize.
    let result2 = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;
    assert!(result2.materialized_items.is_empty());
    assert_eq!(result2.total_unmaterialized, 0);

    // Verify exactly one task exists (no duplicate).
    let task = task_repo.get(&first_task_id).await.unwrap().unwrap();
    assert_eq!(task.issue_type, "task");
    assert!(task.description.contains("Audit Review"));
}

// ── Test: atomic materialization directly ────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn atomic_materialization_creates_task_and_links_in_one_tx() {
    // Directly tests the materialize_audit_task_atomic method to verify
    // the task and selection link are created atomically.
    let db = test_db();
    let project_id = uuid::Uuid::now_v7().to_string();
    djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;

    let sel = seed_selection(
        &db,
        &project_id,
        "sha-atomic-001",
        "unflagged_merged",
        0,
        "2026-07-09T12:00:00Z",
    )
    .await;

    let events = djinn_core::events::EventBus::noop();
    let epic_repo = EpicRepository::new(db.clone(), events.clone());
    let task_repo = TaskRepository::new(db.clone(), events);
    let audit_repo = AuditSamplerRepository::new(db.clone());

    // Ensure the audit epic exists.
    let epic_id = ensure_audit_epic(&epic_repo, &project_id).await.unwrap();

    // Pass source-task provenance from the selection; it must determine the creator.
    let source_task_id = audit_repo
        .list_unmaterialized_selections()
        .await
        .unwrap()
        .into_iter()
        .find(|item| item.selection_id == sel.id)
        .and_then(|item| item.task_id)
        .expect("seeded selection has source task provenance");
    let source_creator_id = task_repo
        .created_by_user_id(&source_task_id)
        .await
        .unwrap()
        .expect("seeded source task has a creator");
    let task_id = audit_repo
        .materialize_audit_task_atomic(
            &sel.id,
            &project_id,
            Some(&epic_id),
            Some(&source_task_id),
            "Audit review: test",
            "test description",
        )
        .await
        .unwrap();

    // Verify the task exists.
    let task = task_repo.get(&task_id).await.unwrap().unwrap();
    assert_eq!(task.issue_type, "task");
    assert_eq!(task.description, "test description");
    assert_eq!(
        task.created_by_user_id.as_deref(),
        Some(source_creator_id.as_str())
    );

    // Verify the selection is linked.
    let updated = audit_repo
        .get_selection_by_id(&sel.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.audit_task_id.as_deref(), Some(task_id.as_str()));

    // Verify list_unmaterialized no longer returns this selection.
    let unmaterialized = audit_repo.list_unmaterialized_selections().await.unwrap();
    assert!(
        !unmaterialized.iter().any(|u| u.selection_id == sel.id),
        "linked selection must not appear in unmaterialized list"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn atomic_materialization_rolls_back_when_creator_is_unavailable() {
    let db = test_db();
    let project_id = uuid::Uuid::now_v7().to_string();
    djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;
    let sel = seed_selection(
        &db,
        &project_id,
        "sha-creator-unavailable",
        "unflagged_merged",
        0,
        "2026-07-09T12:00:00Z",
    )
    .await;
    let audit_repo = AuditSamplerRepository::new(db.clone());
    let source_task_id = uuid::Uuid::now_v7().to_string();
    assert!(
        TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop())
            .get(&source_task_id)
            .await
            .unwrap()
            .is_none(),
        "fixture source-task identity must be dangling"
    );
    let error = audit_repo
        .materialize_audit_task_atomic(
            &sel.id,
            &project_id,
            None,
            Some(&source_task_id),
            "Audit review: unavailable creator",
            "must roll back",
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("effective_creator_unavailable"));
    assert_eq!(
        TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop())
            .count_by_title("Audit review: unavailable creator")
            .await
            .unwrap(),
        0
    );
    assert!(
        audit_repo
            .get_selection_by_id(&sel.id)
            .await
            .unwrap()
            .unwrap()
            .audit_task_id
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn atomic_materialization_rolls_back_when_selection_link_fails() {
    let db = test_db();
    let project_id = uuid::Uuid::now_v7().to_string();
    djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;
    let sel = seed_selection(
        &db,
        &project_id,
        "sha-link-failure",
        "unflagged_merged",
        0,
        "2026-07-09T12:00:00Z",
    )
    .await;
    let audit_repo = AuditSamplerRepository::new(db.clone());
    let source_task_id = audit_repo
        .list_unmaterialized_selections()
        .await
        .unwrap()
        .into_iter()
        .find(|item| item.selection_id == sel.id)
        .and_then(|item| item.task_id)
        .unwrap();
    let error = audit_repo
        .materialize_audit_task_atomic(
            &uuid::Uuid::now_v7().to_string(),
            &project_id,
            None,
            Some(&source_task_id),
            "Audit review: link failure",
            "must roll back",
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("audit_selection_not_unmaterialized")
    );
    assert_eq!(
        TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop())
            .count_by_title("Audit review: link failure")
            .await
            .unwrap(),
        0
    );
    assert!(
        audit_repo
            .get_selection_by_id(&sel.id)
            .await
            .unwrap()
            .unwrap()
            .audit_task_id
            .is_none()
    );
}
