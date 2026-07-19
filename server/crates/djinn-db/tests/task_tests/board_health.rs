use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_report() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    // Create tasks: one open, one in_progress.
    let _t1 = open_task(&repo, &epic.id).await;
    let t2 = open_task(&repo, &epic.id).await;
    repo.transition(&t2.id, TransitionAction::Start, "", "system", None, None)
        .await
        .unwrap();

    let report = repo.board_health(24).await.unwrap();
    let epic_stats = report["epic_stats"].as_array().unwrap();
    assert_eq!(epic_stats.len(), 1);
    assert_eq!(epic_stats[0]["total"], 2);
    assert!(report.get("memory_health").is_none());

    // Backdate t2's updated_at to simulate staleness.
    let t2_id = t2.id.clone();
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00.000Z' WHERE id = $1")
        .bind(&t2_id)
        .execute(db.pool())
        .await
        .unwrap();

    let report2 = repo.board_health(24).await.unwrap();
    let stale = report2["stale_tasks"].as_array().unwrap();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0]["short_id"], t2.short_id.as_str());
    assert!(report2.get("memory_health").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_flags_repeated_reopen_role_tool_mismatch_candidates() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let task = repo
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            "Plan next wave after repeated worker churn",
            "Repeated reopen churn suggests this should create planning tasks instead of more worker implementation.",
            "Use task_create to split work and epic_update to refresh epic metadata.",
            "task",
            1,
            "planner",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET total_reopen_count = 3 WHERE id = $1")
        .bind(&task.id)
        .execute(db.pool())
        .await
        .unwrap();
    let _session = create_test_session(&db, &project.id, &task.id).await;

    let report = repo.board_health(24).await.unwrap();
    let mismatches = report
        .get("repeated_reopen_role_tool_mismatches")
        .and_then(|v| v.as_array())
        .expect("repeated_reopen_role_tool_mismatches field should exist");
    assert!(mismatches.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_ignores_repeated_reopen_tasks_without_role_tool_mismatch() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let task = repo
        .create_fixture_in_project(
            &epic.project_id,
            Some(&epic.id),
            "Implement worker-safe fix",
            "A normal implementation task with code changes only.",
            "Edit Rust code and update tests in the existing module.",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET total_reopen_count = 4 WHERE id = $1")
        .bind(&task.id)
        .execute(db.pool())
        .await
        .unwrap();

    let report = repo.board_health(24).await.unwrap();
    let mismatches = report
        .get("repeated_reopen_role_tool_mismatches")
        .and_then(|v| v.as_array())
        .expect("repeated_reopen_role_tool_mismatches field should exist");
    assert!(mismatches.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_returns_liveness_outcomes_section() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    // Create a task + session + liveness evidence row.
    let task = repo
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            "Liveness task",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    let session = create_test_session(&db, &project.id, &task.id).await;

    let evidence_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO liveness_evidence \
         (id, session_id, task_id, verdict, outcome_kind, evidence, created_at) \
         VALUES ($1, $2, $3, 'dead', 'dead_reclaimed', '{}', \
                 '2025-06-01T00:00:00.000Z')",
    )
    .bind(&evidence_id)
    .bind(&session.id)
    .bind(&task.id)
    .execute(db.pool())
    .await
    .unwrap();

    let report = repo.board_health(24).await.unwrap();
    let liveness = report
        .get("liveness_outcomes")
        .expect("liveness_outcomes field should exist");
    assert_eq!(liveness["total"], 1);
    let by_verdict = liveness["by_verdict"].as_object().unwrap();
    assert_eq!(by_verdict["dead"], 1);
    let recent = liveness["recent"].as_array().unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0]["verdict"], "dead");
    assert_eq!(recent[0]["outcome_kind"], "dead_reclaimed");
    assert_eq!(recent[0]["task_id"], task.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_returns_protocol_violations_section() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let task = repo
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            "Protocol violation task",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    let session = create_test_session(&db, &project.id, &task.id).await;

    let evidence_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO liveness_evidence \
         (id, session_id, task_id, verdict, outcome_kind, outcome_reason, evidence, created_at) \
         VALUES ($1, $2, $3, 'protocol_violation', 'protocol_violation', \
                 'clean_exit_nonterminal', '{}', '2025-06-01T00:00:00.000Z')",
    )
    .bind(&evidence_id)
    .bind(&session.id)
    .bind(&task.id)
    .execute(db.pool())
    .await
    .unwrap();

    let report = repo.board_health(24).await.unwrap();
    let pv = report
        .get("protocol_violations")
        .expect("protocol_violations field should exist");
    assert_eq!(pv["total"], 1);
    let recent = pv["recent"].as_array().unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0]["verdict"], "protocol_violation");
    assert_eq!(recent[0]["outcome_reason"], "clean_exit_nonterminal");
    assert_eq!(recent[0]["task_short_id"], task.short_id.as_str());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_stranded_ready_detects_stale_open_tasks() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    // Create an open task (no sessions) and backdate it so it exceeds the
    // 30-minute stranded-ready threshold.
    let task = repo
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            "Starved open task",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    backdate_task_updated_at(&db, &task.id, "60 minutes").await;

    let report = repo.board_health(24).await.unwrap();
    let sr = report
        .get("stranded_ready")
        .expect("stranded_ready field should exist");
    assert_eq!(sr["total"], 1);
    assert_eq!(sr["threshold_minutes"], 30);
    let findings = sr["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["id"], task.id);
    assert_eq!(findings[0]["short_id"], task.short_id.as_str());
    assert_eq!(findings[0]["severity"], "error"); // 60m ≥ 2×30 error threshold
    assert_eq!(findings[0]["unclaimed_since_confidence"], "low"); // no activity log → fallback
    assert_eq!(findings[0]["threshold"]["warning_minutes"], 30);
    assert_eq!(findings[0]["threshold"]["error_minutes"], 60);
    assert_eq!(findings[0]["threshold"]["critical_minutes"], 180);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_stranded_ready_excludes_tasks_with_active_sessions() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    // Create an open task with an active running session.
    let task = repo
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            "Active task",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    let session = create_test_session(&db, &project.id, &task.id).await;
    // Mark session as running.
    sqlx::query("UPDATE sessions SET status = 'running' WHERE id = $1")
        .bind(&session.id)
        .execute(db.pool())
        .await
        .unwrap();
    backdate_task_updated_at(&db, &task.id, "60 minutes").await;

    let report = repo.board_health(24).await.unwrap();
    let sr = report
        .get("stranded_ready")
        .expect("stranded_ready field should exist");
    let findings = sr["findings"].as_array().unwrap();
    assert!(
        findings.is_empty(),
        "task with active session should not appear in stranded_ready"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_stranded_ready_high_confidence_with_activity_log() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    // Create an open task and insert an activity_log entry showing it was
    // transitioned to 'open' long ago.
    let task = repo
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            "Task with activity log",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    // Insert a 'status_changed' → 'open' activity log entry backdated by 2 hours.
    let activity_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO activity_log \
         (id, task_id, actor_id, actor_role, event_type, payload, created_at) \
         VALUES ($1, $2, 'system', 'system', 'status_changed', \
                 '{\"from_status\": \"in_progress\", \"to_status\": \"open\"}', \
                 '2025-01-01T00:00:00.000Z')",
    )
    .bind(&activity_id)
    .bind(&task.id)
    .execute(db.pool())
    .await
    .unwrap();

    let report = repo.board_health(24).await.unwrap();
    let sr = report
        .get("stranded_ready")
        .expect("stranded_ready field should exist");
    let findings = sr["findings"].as_array().unwrap();
    // At least one finding for this task.
    let finding = findings
        .iter()
        .find(|f| f["id"] == task.id)
        .expect("task should appear in stranded_ready");
    assert_eq!(finding["unclaimed_since_confidence"], "high");
    assert_eq!(finding["unclaimed_since"], "2025-01-01T00:00:00.000Z");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_stranded_ready_severity_escalates_with_elapsed_time() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    // Task at 35 minutes: warning (≥1×30, <2×30).
    let t1 = repo
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            "Warning task",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    backdate_task_updated_at(&db, &t1.id, "35 minutes").await;

    // Task at 65 minutes: error (≥2×30, <6×30).
    let t2 = repo
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            "Error task",
            "desc",
            "design",
            "task",
            2,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    backdate_task_updated_at(&db, &t2.id, "65 minutes").await;

    // Task at 200 minutes: critical (≥6×30).
    let t3 = repo
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            "Critical task",
            "desc",
            "design",
            "task",
            3,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    backdate_task_updated_at(&db, &t3.id, "200 minutes").await;

    let report = repo.board_health(24).await.unwrap();
    let findings = report["stranded_ready"]["findings"].as_array().unwrap();

    let f1 = findings.iter().find(|f| f["id"] == t1.id).unwrap();
    assert_eq!(f1["severity"], "warning");

    let f2 = findings.iter().find(|f| f["id"] == t2.id).unwrap();
    assert_eq!(f2["severity"], "error");

    let f3 = findings.iter().find(|f| f["id"] == t3.id).unwrap();
    assert_eq!(f3["severity"], "critical");
}

/// A stale ready task whose dispatch backoff ladder has tripped
/// (`failure_streak >= 3`) is an explicit rate-limit gate, so it must be
/// excluded from stranded_ready rather than reported as starved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_stranded_ready_excludes_rate_limited_tasks() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let task = repo
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            "Rate-limited task",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    backdate_task_updated_at(&db, &task.id, "60 minutes").await;
    // Rate-limit backoff has tripped (>= 3 consecutive dispatch failures).
    sqlx::query("INSERT INTO dispatch_state (task_id, failure_streak) VALUES ($1, 3)")
        .bind(&task.id)
        .execute(db.pool())
        .await
        .unwrap();

    let report = repo.board_health(24).await.unwrap();
    let findings = report["stranded_ready"]["findings"].as_array().unwrap();
    assert!(
        findings.iter().all(|f| f["id"] != task.id),
        "rate-limited task (failure_streak >= 3) must be excluded from stranded_ready"
    );
}

/// A stale ready task with a future breaker cooldown is intentionally held, so
/// it must be excluded from stranded_ready.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_stranded_ready_excludes_cooldown_active_tasks() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let task = repo
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            "Cooldown task",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    backdate_task_updated_at(&db, &task.id, "60 minutes").await;
    // Breaker cooldown deadline one hour in the future.
    sqlx::query(
        "INSERT INTO dispatch_state (task_id, cooldown_until) \
         VALUES ($1, now() AT TIME ZONE 'utc' + interval '1 hour')",
    )
    .bind(&task.id)
    .execute(db.pool())
    .await
    .unwrap();

    let report = repo.board_health(24).await.unwrap();
    let findings = report["stranded_ready"]["findings"].as_array().unwrap();
    assert!(
        findings.iter().all(|f| f["id"] != task.id),
        "breaker-open (future cooldown) task must be excluded from stranded_ready"
    );
}

/// A stale ready task with a paused session is manually held, so it must be
/// excluded from stranded_ready.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_stranded_ready_excludes_paused_tasks() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let task = repo
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            "Paused task",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    let session = create_test_session(&db, &project.id, &task.id).await;
    sqlx::query("UPDATE sessions SET status = 'paused' WHERE id = $1")
        .bind(&session.id)
        .execute(db.pool())
        .await
        .unwrap();
    backdate_task_updated_at(&db, &task.id, "60 minutes").await;

    let report = repo.board_health(24).await.unwrap();
    let findings = report["stranded_ready"]["findings"].as_array().unwrap();
    assert!(
        findings.iter().all(|f| f["id"] != task.id),
        "manually paused task must be excluded from stranded_ready"
    );
}

/// A genuinely stranded task carries a fully-populated `dispatch_gate` object.
/// When the inflight model has no healthy model_health row, `image_ready` is
/// false, `no_eligible_model` is reported, and the gate verdict is `blocked`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_stranded_ready_gate_evidence_fields() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    // This fixture isolates an unhealthy inflight model, so give the task a
    // real creator and that creator's active private credential. A legacy NULL
    // creator correctly fails the credential gate and exercises another blocker.
    let user_id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, github_id, github_login) VALUES ($1, $2, $3)")
        .bind(&user_id)
        .bind(rand_github_id())
        .bind(format!("user-{user_id}"))
        .execute(db.pool())
        .await
        .unwrap();

    let task = repo
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            "Gate evidence task",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    repo.set_created_by_user_id(&task.id, &user_id)
        .await
        .unwrap();
    let cred_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO credentials \
         (id, provider_id, key_name, encrypted_value, owner_user_id) \
         VALUES ($1, 'anthropic', $2, '\\x00'::bytea, $3)",
    )
    .bind(&cred_id)
    .bind(format!("key-{cred_id}"))
    .bind(&user_id)
    .execute(db.pool())
    .await
    .unwrap();
    backdate_task_updated_at(&db, &task.id, "60 minutes").await;
    // An inflight model was chosen, but there is no model_health row for it.
    sqlx::query(
        "INSERT INTO dispatch_state (task_id, failure_streak, last_dispatched_role, inflight_model_id) \
         VALUES ($1, 0, 'worker', 'testprov/testmodel')",
    )
    .bind(&task.id)
    .execute(db.pool())
    .await
    .unwrap();

    let report = repo.board_health(24).await.unwrap();
    let findings = report["stranded_ready"]["findings"].as_array().unwrap();
    let finding = findings
        .iter()
        .find(|f| f["id"] == task.id)
        .expect("task should appear as stranded");
    let gate = &finding["dispatch_gate"];
    assert_eq!(gate["evaluated_role"], "worker");
    assert!(gate["toolset"].as_array().is_some_and(|t| !t.is_empty()));
    assert_eq!(gate["model_requirement"], "testprov/testmodel");
    assert_eq!(gate["image_ready"], false);
    assert_eq!(gate["breaker_open"], false);
    assert_eq!(gate["manually_paused"], false);
    assert_eq!(gate["rate_limited"], false);
    assert_eq!(gate["credential_available"], true);
    assert_eq!(gate["gate_verdict"], "blocked");
    let reasons = gate["reasons"].as_array().unwrap();
    assert!(
        reasons.iter().any(|r| r == "no_eligible_model"),
        "expected no_eligible_model in dispatch_gate reasons, got {reasons:?}"
    );
}

/// A stale ready task whose creator has credentials that are all revoked (and
/// no org-shared fallback) is owner-credential-blocked and must be excluded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_stranded_ready_excludes_credential_revoked_tasks() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let task = repo
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            "Credential-blocked task",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    backdate_task_updated_at(&db, &task.id, "60 minutes").await;

    // Attribute the task to a user whose only credential is revoked.
    let user_id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, github_id, github_login) VALUES ($1, $2, $3)")
        .bind(&user_id)
        .bind(rand_github_id())
        .bind(format!("user-{user_id}"))
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET created_by_user_id = $1 WHERE id = $2")
        .bind(&user_id)
        .bind(&task.id)
        .execute(db.pool())
        .await
        .unwrap();
    let cred_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO credentials \
         (id, provider_id, key_name, encrypted_value, owner_user_id, revoked_at) \
         VALUES ($1, 'anthropic', $2, '\\x00'::bytea, $3, '2025-01-01T00:00:00.000Z')",
    )
    .bind(&cred_id)
    .bind(format!("key-{cred_id}"))
    .bind(&user_id)
    .execute(db.pool())
    .await
    .unwrap();

    let report = repo.board_health(24).await.unwrap();
    let findings = report["stranded_ready"]["findings"].as_array().unwrap();
    assert!(
        findings.iter().all(|f| f["id"] != task.id),
        "owner-credential-revoked task must be excluded from stranded_ready"
    );
}

/// Small unique-ish github_id generator for seeding test users without
/// colliding on the `uq_users_github_id` constraint within a test DB. Derived
/// from a fresh UUIDv7 (monotonic, high-entropy) to stay off wall-clock APIs.
fn rand_github_id() -> i64 {
    (uuid::Uuid::now_v7().as_u128() & 0x7fff_ffff_ffff) as i64
}

/// Comprehensive backward-compatibility regression: legacy coarse board_health
/// fields must remain present and deserializable while all additive liveness,
/// protocol-violation, stranded-ready, classifier outcome/evidence, and
/// dispatch-gate fields are also available in a single response.
///
/// This test seeds all three categories of additive data (liveness evidence,
/// protocol-violation evidence, and a stranded-ready task) and verifies that
/// `board_health` returns every field without omission.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_legacy_and_additive_fields_coexist() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    // ── Seed legacy stale task ──────────────────────────────────────────
    let stale_task = repo
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            "Stale in_progress task",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            Some(r#"[{"description":"default","met":false}]"#),
        )
        .await
        .unwrap();
    repo.transition(
        &stale_task.id,
        TransitionAction::Start,
        "",
        "system",
        None,
        None,
    )
    .await
    .unwrap();
    // Backdate updated_at so it appears as a stale in_progress task.
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00.000Z' WHERE id = $1")
        .bind(&stale_task.id)
        .execute(db.pool())
        .await
        .unwrap();

    // ── Seed liveness evidence (dead verdict) ──────────────────────────
    let liveness_task = repo
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            "Liveness evidence task",
            "desc",
            "design",
            "task",
            2,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    let liveness_session = create_test_session(&db, &project.id, &liveness_task.id).await;
    let evidence_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO liveness_evidence \
         (id, session_id, task_id, verdict, outcome_kind, outcome_reason, evidence, created_at) \
         VALUES ($1, $2, $3, 'dead', 'dead_reclaimed', 'hard_runtime_exceeded', \
                 '{\"pod_phase\":\"Succeeded\",\"claim_ttl_expired\":true}', \
                 '2025-06-01T00:00:00.000Z')",
    )
    .bind(&evidence_id)
    .bind(&liveness_session.id)
    .bind(&liveness_task.id)
    .execute(db.pool())
    .await
    .unwrap();

    // ── Seed protocol-violation evidence ───────────────────────────────
    let pv_session_id = liveness_session.id.clone();
    let pv_task_id = liveness_task.id.clone();
    let pv_evidence_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO liveness_evidence \
         (id, session_id, task_id, verdict, outcome_kind, outcome_reason, evidence, created_at) \
         VALUES ($1, $2, $3, 'protocol_violation', 'protocol_violation', \
                 'clean_exit_nonterminal', '{\"reason\":\"unexpected\"}', \
                 '2025-06-01T00:00:01.000Z')",
    )
    .bind(&pv_evidence_id)
    .bind(&pv_session_id)
    .bind(&pv_task_id)
    .execute(db.pool())
    .await
    .unwrap();

    // ── Seed stranded-ready task ───────────────────────────────────────
    let stranded_task = repo
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            "Stranded open task",
            "desc",
            "design",
            "task",
            3,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    backdate_task_updated_at(&db, &stranded_task.id, "90 minutes").await;

    // ── Single board_health call ───────────────────────────────────────
    let report = repo.board_health(24).await.unwrap();

    // ── Legacy coarse fields must remain present ───────────────────────
    assert!(
        report.get("stale_tasks").is_some(),
        "legacy stale_tasks must remain present"
    );
    assert!(
        report.get("epic_stats").is_some(),
        "legacy epic_stats must remain present"
    );
    assert!(
        report.get("review_queue").is_some(),
        "legacy review_queue must remain present"
    );
    assert_eq!(
        report.get("stale_threshold_hours").and_then(|v| v.as_i64()),
        Some(24),
        "legacy stale_threshold_hours must remain present"
    );

    // Verify the stale task actually surfaces.
    let stale = report["stale_tasks"].as_array().unwrap();
    assert!(
        stale.iter().any(|t| t["id"] == stale_task.id),
        "stale in_progress task must appear in stale_tasks"
    );

    // ── Additive liveness_outcomes section ─────────────────────────────
    let liveness = report
        .get("liveness_outcomes")
        .expect("liveness_outcomes section must be present");
    let l_total = liveness
        .get("total")
        .and_then(|v| v.as_i64())
        .expect("liveness_outcomes.total must be a number");
    assert!(
        l_total >= 2,
        "must surface at least 2 liveness outcomes (dead + protocol_violation), got {l_total}"
    );
    let by_verdict = liveness
        .get("by_verdict")
        .and_then(|v| v.as_object())
        .expect("liveness_outcomes.by_verdict must be an object");
    assert!(
        by_verdict.contains_key("dead"),
        "by_verdict must contain dead count"
    );
    let recent = liveness
        .get("recent")
        .and_then(|v| v.as_array())
        .expect("liveness_outcomes.recent must be an array");
    // The dead outcome must carry classifier outcome/evidence fields.
    let dead_item = recent
        .iter()
        .find(|i| {
            i.get("verdict").and_then(|v| v.as_str()) == Some("dead")
                && i.get("task_id").and_then(|v| v.as_str()) == Some(&liveness_task.id)
        })
        .expect("dead verdict for liveness_task must be present");
    assert_eq!(
        dead_item.get("outcome_kind").and_then(|v| v.as_str()),
        Some("dead_reclaimed"),
        "classifier outcome_kind must be present"
    );
    assert_eq!(
        dead_item.get("outcome_reason").and_then(|v| v.as_str()),
        Some("hard_runtime_exceeded"),
        "classifier outcome_reason must be present"
    );

    // ── Additive protocol_violations section ───────────────────────────
    let pv = report
        .get("protocol_violations")
        .expect("protocol_violations section must be present");
    assert!(
        pv.get("total").and_then(|v| v.as_i64()).unwrap_or(0) >= 1,
        "must surface at least 1 protocol violation"
    );
    let pv_recent = pv
        .get("recent")
        .and_then(|v| v.as_array())
        .expect("protocol_violations.recent must be an array");
    let pv_item = pv_recent
        .iter()
        .find(|i| i.get("task_id").and_then(|v| v.as_str()) == Some(&liveness_task.id))
        .expect("protocol violation for liveness_task must be present");
    assert_eq!(
        pv_item.get("verdict").and_then(|v| v.as_str()),
        Some("protocol_violation"),
        "protocol violation verdict must match"
    );
    assert_eq!(
        pv_item.get("outcome_reason").and_then(|v| v.as_str()),
        Some("clean_exit_nonterminal"),
        "protocol violation outcome_reason must match"
    );

    // ── Additive stranded_ready section ────────────────────────────────
    let sr = report
        .get("stranded_ready")
        .expect("stranded_ready section must be present");
    assert_eq!(
        sr.get("threshold_minutes").and_then(|v| v.as_i64()),
        Some(30),
        "must echo the base 30-minute threshold"
    );
    let findings = sr
        .get("findings")
        .and_then(|v| v.as_array())
        .expect("stranded_ready.findings must be an array");
    let stranded_finding = findings
        .iter()
        .find(|f| f.get("id").and_then(|v| v.as_str()) == Some(&stranded_task.id))
        .expect("stranded_task must appear in stranded_ready findings");
    assert_eq!(
        stranded_finding.get("severity").and_then(|v| v.as_str()),
        Some("error"),
        "90-minute backdate must produce error severity"
    );
    // Dispatch-gate evidence must be present.
    let gate = stranded_finding
        .get("dispatch_gate")
        .expect("dispatch_gate evidence must be present on stranded finding");
    assert!(
        gate.get("evaluated_role").is_some(),
        "dispatch_gate.evaluated_role must be present"
    );
    assert!(
        gate.get("gate_verdict").is_some(),
        "dispatch_gate.gate_verdict must be present"
    );
    assert!(
        gate.get("breaker_open").is_some(),
        "dispatch_gate.breaker_open must be present"
    );
    assert!(
        gate.get("manually_paused").is_some(),
        "dispatch_gate.manually_paused must be present"
    );
    assert!(
        gate.get("rate_limited").is_some(),
        "dispatch_gate.rate_limited must be present"
    );
    assert!(
        gate.get("credential_available").is_some(),
        "dispatch_gate.credential_available must be present"
    );
    assert!(
        gate.get("reasons").is_some(),
        "dispatch_gate.reasons must be present"
    );
    // Threshold ladder.
    let threshold = stranded_finding
        .get("threshold")
        .expect("threshold ladder must be present");
    assert_eq!(
        threshold.get("warning_minutes").and_then(|v| v.as_i64()),
        Some(30)
    );
    assert_eq!(
        threshold.get("error_minutes").and_then(|v| v.as_i64()),
        Some(60)
    );
    assert_eq!(
        threshold.get("critical_minutes").and_then(|v| v.as_i64()),
        Some(180)
    );
}
