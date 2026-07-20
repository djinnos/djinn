// djinn:allow-oversize — end-to-end operator explanation regressions and
// log-tail audit tests for the qum9 ledger/dossier surfaces.
//
// These tests prove the shipped ledger query (`ledger_for_task_since`) and
// the `TaskAttemptLedgerRow` contract explain the major retry/adoption/defer/
// failure outcomes from real `task_attempts` rows, including log-tail metadata.
//
// Acceptance criteria:
// AC1: Operator-readable ledger output explains spawn, defer, open-PR adoption,
//      in-flight wait, and terminal failure/rejection from real attempt rows.
// AC2: Log-tail audit tests cover present, absent, and fetch-failure/error-class
//      metadata and prove raw log-tail text is not exposed in default output.
// AC3: Stale CI evidence older than the newest submitted attempt cannot explain
//      a strike, while a concluded rejection/reopen on that head can.

use super::*;
use crate::dispatch::respawn_guard::{
    RespawnGuardDecision, record_adopted_pr_attempt, record_guard_deferred_attempt,
    run_respawn_guard,
};
use djinn_core::models::task_attempt::{
    GuardDecision, GuardReason, TaskAttemptLedgerRow, TaskAttemptOutcome,
};
use djinn_db::repositories::task_attempt::{
    CreateTaskAttemptParams, SubmitTaskAttemptParams, TaskAttemptRepository,
    TerminalTaskAttemptParams,
};

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Create a task with intervention_count=1 (post-intervention).
async fn make_post_intervention_task(
    db: &Database,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
) -> djinn_core::models::Task {
    let task = make_task_with_reopen_count(db, tx, REOPEN_INTERVENTION_THRESHOLD).await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(tx));
    repo.reset_intervention_counters(&task.id).await.unwrap();
    repo.get(&task.id).await.unwrap().unwrap()
}

/// Insert a submitted attempt row for the given task.  Returns (attempt_id, submitted_at).
async fn seed_submitted_attempt(
    db: &Database,
    task_id: &str,
    role: &str,
    summary: Option<&str>,
    log_tail: Option<&str>,
) -> (String, String) {
    let attempt_repo = TaskAttemptRepository::new(db.clone());
    let attempt_id = uuid::Uuid::now_v7().to_string();
    let dispatch_key = format!("zps8-submitted-{}", attempt_id);
    let attempt = attempt_repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &attempt_id,
            task_id,
            role,
            dispatch_key: &dispatch_key,
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();
    attempt_repo
        .advance_to_submitted(SubmitTaskAttemptParams {
            id: &attempt.id,
            submit_ref: Some("refs/heads/task/test"),
            checkpoint_ref: None,
            mirror_head_sha: Some("mirror-sha-1"),
            github_head_sha: Some("github-sha-1"),
            summary,
            summary_json: None,
            log_tail,
        })
        .await
        .unwrap();
    let row = attempt_repo.get(&attempt_id).await.unwrap().unwrap();
    (attempt_id, row.submitted_at.unwrap_or_default())
}

/// Insert a pending (in-flight dispatch-started) attempt row.
async fn seed_pending_attempt(db: &Database, task_id: &str, role: &str) -> String {
    let attempt_repo = TaskAttemptRepository::new(db.clone());
    let attempt_id = uuid::Uuid::now_v7().to_string();
    let dispatch_key = format!("zps8-pending-{}", attempt_id);
    attempt_repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &attempt_id,
            task_id,
            role,
            dispatch_key: &dispatch_key,
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap()
        .id
}

/// Advance the given attempt to a terminal outcome.
async fn terminalize_attempt(
    db: &Database,
    attempt_id: &str,
    outcome: TaskAttemptOutcome,
    summary: Option<&str>,
    summary_json: Option<&str>,
    log_tail: Option<&str>,
) {
    let attempt_repo = TaskAttemptRepository::new(db.clone());
    attempt_repo
        .advance_to_terminal(TerminalTaskAttemptParams {
            id: attempt_id,
            outcome,
            pr_url: None,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary,
            summary_json,
            log_tail,
        })
        .await
        .unwrap();
}

/// Query the operator ledger for a task since its last intervention.
async fn query_operator_ledger(
    db: &Database,
    task_id: &str,
    last_intervention_at: Option<&str>,
) -> Vec<TaskAttemptLedgerRow> {
    let repo = TaskAttemptRepository::new(db.clone());
    repo.ledger_for_task_since(task_id, None, last_intervention_at, 50)
        .await
        .unwrap()
}

// ═════════════════════════════════════════════════════════════════════════════
// AC1: Operator-readable ledger output explains major outcomes from real rows.
// ═════════════════════════════════════════════════════════════════════════════

/// AC1a: A freshly spawned (pending) attempt appears in the ledger with
/// `outcome=pending` and a non-empty `dispatch_key`, explaining the spawn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zps8_ledger_explains_spawn_from_pending_attempt() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let _actor = coordinator_actor_for_tests(&db, &tx);

    let task = make_post_intervention_task(&db, &tx).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let _attempt_id = seed_pending_attempt(&db, &task.id, "worker").await;

    let ledger = query_operator_ledger(&db, &task.id, task.last_intervention_at.as_deref()).await;
    assert_eq!(ledger.len(), 1, "ledger must have exactly one row");

    let row = &ledger[0];
    assert_eq!(row.outcome, "pending", "spawned attempt must show pending");
    assert_eq!(row.role, "worker");
    assert!(
        !row.dispatch_key.is_empty(),
        "dispatch_key must explain the spawn"
    );
    assert_eq!(row.guard_decision, None, "spawned attempt has no guard");
    assert_eq!(
        row.guard_reason, None,
        "spawned attempt has no guard reason"
    );
    assert!(
        row.terminal_at.is_none(),
        "pending attempt must not be terminal"
    );
}

/// AC1b: A guard-deferred attempt appears in the ledger with `outcome=deferred`
/// and `guard_reason=respawn_guard`, explaining why dispatch was deferred
/// (another attempt is already in-flight).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zps8_ledger_explains_defer_from_guard_decision() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let _actor = coordinator_actor_for_tests(&db, &tx);

    let task = make_post_intervention_task(&db, &tx).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    // Record a guard-deferred audit row (second spawn blocked by first).
    let defer_id = record_guard_deferred_attempt(
        &db,
        &task.id,
        "worker",
        GuardReason::RespawnGuard,
        Some("duplicate spawn blocked: pending attempt already in-flight"),
    )
    .await
    .expect("guard deferred row should insert");

    let ledger = query_operator_ledger(&db, &task.id, task.last_intervention_at.as_deref()).await;
    assert_eq!(ledger.len(), 1, "ledger must have exactly one row");

    let row = &ledger[0];
    assert_eq!(row.id, defer_id);
    assert_eq!(row.outcome, "deferred", "deferred guard must show deferred");
    assert_eq!(
        row.guard_decision.as_deref(),
        Some(GuardDecision::Defer.as_str()),
        "guard_decision must be defer"
    );
    assert_eq!(
        row.guard_reason.as_deref(),
        Some(GuardReason::RespawnGuard.as_str()),
        "guard_reason must explain the defer as respawn_guard"
    );
    assert!(
        row.terminal_at.is_some(),
        "deferred guard rows are terminal"
    );
    assert_eq!(row.role, "worker");
}

/// AC1c: An adopted open-PR attempt appears in the ledger with
/// `outcome=adopted_pr`, `guard_reason=open_pr_adoption`, and the `pr_url`,
/// explaining the adoption.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zps8_ledger_explains_adoption_from_open_pr() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let _actor = coordinator_actor_for_tests(&db, &tx);

    let task = make_post_intervention_task(&db, &tx).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    let adopt_id = record_adopted_pr_attempt(
        &db,
        &task.id,
        "worker",
        "https://github.example/owner/repo/pull/99",
        Some("adopted existing open PR"),
    )
    .await
    .expect("adopted PR row should insert");

    let ledger = query_operator_ledger(&db, &task.id, task.last_intervention_at.as_deref()).await;
    assert_eq!(ledger.len(), 1);

    let row = &ledger[0];
    assert_eq!(row.id, adopt_id);
    assert_eq!(row.outcome, "adopted_pr");
    assert_eq!(
        row.guard_reason.as_deref(),
        Some(GuardReason::OpenPrAdoption.as_str()),
        "guard_reason must explain adoption as open_pr_adoption"
    );
    assert_eq!(
        row.pr_url.as_deref(),
        Some("https://github.example/owner/repo/pull/99"),
        "adopted PR must carry the pr_url for operator follow-up"
    );
    assert!(row.terminal_at.is_some());
}

/// AC1d: A submitted (in-flight) attempt appears in the ledger as
/// `outcome=submitted`, showing the operator that an attempt is being waited
/// on rather than counting as a failed strike.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zps8_ledger_explains_inflight_wait_from_submitted() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let _actor = coordinator_actor_for_tests(&db, &tx);

    let task = make_post_intervention_task(&db, &tx).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let (attempt_id, _submitted_at) =
        seed_submitted_attempt(&db, &task.id, "worker", Some("awaiting review"), None).await;

    let ledger = query_operator_ledger(&db, &task.id, task.last_intervention_at.as_deref()).await;
    assert_eq!(ledger.len(), 1);

    let row = &ledger[0];
    assert_eq!(row.id, attempt_id);
    assert_eq!(
        row.outcome, "submitted",
        "in-flight attempt must show submitted, not a terminal failure"
    );
    assert!(
        row.submitted_at.is_some(),
        "submitted attempt must carry submission timestamp"
    );
    assert!(
        row.terminal_at.is_none(),
        "submitted attempt must NOT be terminal"
    );
    assert!(
        row.submit_ref.is_some(),
        "submitted attempt must carry submit_ref for PR tracking"
    );
    assert_eq!(row.guard_reason, None, "no guard on a normal submission");
}

/// AC1e: A terminal rejection (review failure) appears in the ledger with
/// `outcome=reopened`, explaining the genuine failure evidence that triggered
/// a reopen/park.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zps8_ledger_explains_terminal_failure_rejection() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let _actor = coordinator_actor_for_tests(&db, &tx);

    let task = make_post_intervention_task(&db, &tx).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let (attempt_id, _submitted_at) =
        seed_submitted_attempt(&db, &task.id, "worker", None, None).await;

    // Terminalize as reopened (review rejection).
    // The terminal summary is set here since the submit phase had none;
    // advance_to_terminal uses COALESCE so this summary is preserved.
    terminalize_attempt(
        &db,
        &attempt_id,
        TaskAttemptOutcome::Reopened,
        Some("reviewer: acceptance criteria not met"),
        None,
        None,
    )
    .await;

    let ledger = query_operator_ledger(&db, &task.id, task.last_intervention_at.as_deref()).await;
    assert_eq!(ledger.len(), 1);

    let row = &ledger[0];
    assert_eq!(row.id, attempt_id);
    assert_eq!(
        row.outcome, "reopened",
        "terminal rejection must show reopened outcome"
    );
    assert!(row.terminal_at.is_some(), "rejection must be terminal");
    assert!(
        row.summary
            .as_deref()
            .unwrap()
            .contains("acceptance criteria"),
        "summary must carry the rejection evidence; got: {:?}",
        row.summary
    );
}

/// AC1f: A terminal crash (infra failure) appears in the ledger as `outcome=crashed`,
/// explaining genuine terminal failure without a review rejection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zps8_ledger_explains_terminal_crash() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let _actor = coordinator_actor_for_tests(&db, &tx);

    let task = make_post_intervention_task(&db, &tx).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let attempt_id = seed_pending_attempt(&db, &task.id, "worker").await;

    terminalize_attempt(
        &db,
        &attempt_id,
        TaskAttemptOutcome::Crashed,
        Some("worker session lost: OOM killer"),
        None,
        None,
    )
    .await;

    let ledger = query_operator_ledger(&db, &task.id, task.last_intervention_at.as_deref()).await;
    assert_eq!(ledger.len(), 1);

    let row = &ledger[0];
    assert_eq!(row.outcome, "crashed");
    assert!(row.terminal_at.is_some());
    assert!(
        row.summary.as_deref().unwrap().contains("OOM"),
        "crash summary must carry the crash evidence"
    );
}

/// AC1g: Multiple attempt lifecycle stages appear together in the ledger,
/// showing the complete operator story for a retry sequence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zps8_ledger_shows_complete_retry_sequence() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let _actor = coordinator_actor_for_tests(&db, &tx);

    let task = make_post_intervention_task(&db, &tx).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    // 1. First attempt: crash.
    let attempt1_id = seed_pending_attempt(&db, &task.id, "worker").await;
    terminalize_attempt(
        &db,
        &attempt1_id,
        TaskAttemptOutcome::Crashed,
        Some("session lost"),
        None,
        None,
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    // 2. Guard defers second spawn (first still terminal — but we simulate a
    //    race where another in-flight attempt exists).
    //    Instead, record a deferred guard audit row.
    record_guard_deferred_attempt(
        &db,
        &task.id,
        "worker",
        GuardReason::RespawnGuard,
        Some("non-terminal attempt blocks respawn"),
    )
    .await
    .expect("deferred row");

    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    // 3. Third attempt: submitted successfully.
    let (attempt3_id, _) =
        seed_submitted_attempt(&db, &task.id, "worker", Some("second try submit"), None).await;

    let ledger = query_operator_ledger(&db, &task.id, task.last_intervention_at.as_deref()).await;
    assert_eq!(ledger.len(), 3, "ledger must show all three attempts");

    // Ledger is newest-first.  attempt3 (submitted) is newest.
    let newest = &ledger[0];
    assert_eq!(newest.id, attempt3_id);
    assert_eq!(newest.outcome, "submitted");

    // Middle is the deferred guard.
    let middle = &ledger[1];
    assert_eq!(middle.outcome, "deferred");
    assert_eq!(
        middle.guard_reason.as_deref(),
        Some(GuardReason::RespawnGuard.as_str())
    );

    // Oldest is the crashed attempt.
    let oldest = &ledger[2];
    assert_eq!(oldest.id, attempt1_id);
    assert_eq!(oldest.outcome, "crashed");
}

// ═════════════════════════════════════════════════════════════════════════════
// AC2: Log-tail audit tests cover present, absent, and fetch-failure/error-class
//      metadata; raw log-tail text is NOT exposed in ledger output.
// ═════════════════════════════════════════════════════════════════════════════

/// AC2a: An attempt with a captured log_tail reports `log_tail_present=true`
/// in the ledger row.  The raw log_tail text is NOT present as a field.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zps8_log_tail_present_metadata_in_ledger() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let _actor = coordinator_actor_for_tests(&db, &tx);

    let task = make_post_intervention_task(&db, &tx).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let (attempt_id, _) = seed_submitted_attempt(
        &db,
        &task.id,
        "worker",
        Some("submit with logs"),
        Some("ERROR: connection refused at port 5432\nTRACE: retrying..."),
    )
    .await;

    terminalize_attempt(
        &db,
        &attempt_id,
        TaskAttemptOutcome::Crashed,
        Some("DB connection failure"),
        None,
        None,
    )
    .await;

    let ledger = query_operator_ledger(&db, &task.id, task.last_intervention_at.as_deref()).await;
    assert_eq!(ledger.len(), 1);

    let row = &ledger[0];
    assert!(
        row.log_tail_present,
        "log_tail_present must be true when log_tail was captured"
    );
    // The raw text must NOT be in the ledger row (the struct has no `log_tail` field).
    // Verify via serialization: the JSON output must not contain the raw text.
    let json = serde_json::to_string(row).unwrap();
    assert!(
        !json.contains("connection refused at port 5432"),
        "raw log-tail text must NOT appear in serialized ledger output; got: {json}"
    );
    assert!(
        !json.contains("retrying"),
        "raw log-tail text must NOT appear in serialized ledger output"
    );
}

/// AC2b: An attempt without a log_tail reports `log_tail_present=false` in the
/// ledger row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zps8_log_tail_absent_metadata_in_ledger() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let _actor = coordinator_actor_for_tests(&db, &tx);

    let task = make_post_intervention_task(&db, &tx).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let (attempt_id, _) = seed_submitted_attempt(
        &db,
        &task.id,
        "worker",
        Some("submit without logs"),
        None, // no log_tail
    )
    .await;

    terminalize_attempt(
        &db,
        &attempt_id,
        TaskAttemptOutcome::Crashed,
        Some("worker session lost"),
        None,
        None,
    )
    .await;

    let ledger = query_operator_ledger(&db, &task.id, task.last_intervention_at.as_deref()).await;
    assert_eq!(ledger.len(), 1);

    let row = &ledger[0];
    assert!(
        !row.log_tail_present,
        "log_tail_present must be false when no log_tail was captured"
    );
    assert_eq!(
        row.log_tail_error_class, None,
        "no error class when no infra-death metadata"
    );
}

/// AC2c: An attempt with infra-death log-tail fetch failure reports
/// `log_tail_error_class` in the ledger row.  The error class comes from
/// `summary_json->'infra_death_log_tail'->>'fetch_error_class'`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zps8_log_tail_fetch_failure_error_class_in_ledger() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let _actor = coordinator_actor_for_tests(&db, &tx);

    let task = make_post_intervention_task(&db, &tx).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let attempt_id = seed_pending_attempt(&db, &task.id, "worker").await;

    let summary_json =
        r#"{"infra_death_log_tail":{"fetched":false,"fetch_error_class":"timeout"}}"#;
    terminalize_attempt(
        &db,
        &attempt_id,
        TaskAttemptOutcome::Crashed,
        Some("infra death: pod evicted"),
        Some(summary_json),
        Some("partial log before eviction..."),
    )
    .await;

    let ledger = query_operator_ledger(&db, &task.id, task.last_intervention_at.as_deref()).await;
    assert_eq!(ledger.len(), 1);

    let row = &ledger[0];
    assert!(
        row.log_tail_present,
        "log_tail_present must be true when partial log was captured"
    );
    assert_eq!(
        row.log_tail_error_class.as_deref(),
        Some("timeout"),
        "log_tail_error_class must be extracted from summary_json infra_death_log_tail.fetch_error_class"
    );

    // Verify raw text is still not exposed.
    let json = serde_json::to_string(row).unwrap();
    assert!(
        !json.contains("partial log before eviction"),
        "raw log-tail text must NOT appear even when fetch_error_class is present"
    );
}

/// AC2d: An attempt with successful infra-death log capture reports
/// `log_tail_error_class=None` (fetched=true means no error).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zps8_log_tail_successful_fetch_no_error_class() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let _actor = coordinator_actor_for_tests(&db, &tx);

    let task = make_post_intervention_task(&db, &tx).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let attempt_id = seed_pending_attempt(&db, &task.id, "worker").await;

    let summary_json = r#"{"infra_death_log_tail":{"fetched":true,"line_count":42}}"#;
    terminalize_attempt(
        &db,
        &attempt_id,
        TaskAttemptOutcome::Crashed,
        Some("infra death: pod evicted"),
        Some(summary_json),
        Some("full captured log tail content"),
    )
    .await;

    let ledger = query_operator_ledger(&db, &task.id, task.last_intervention_at.as_deref()).await;
    assert_eq!(ledger.len(), 1);

    let row = &ledger[0];
    assert!(row.log_tail_present, "log was captured");
    assert_eq!(
        row.log_tail_error_class, None,
        "no error class when fetch succeeded (fetched=true)"
    );
}

/// AC2e: A guard-deferred audit row with no log_tail reports
/// `log_tail_present=false` in the ledger, proving guard-only rows are clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zps8_guard_deferred_row_has_no_log_tail() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let _actor = coordinator_actor_for_tests(&db, &tx);

    let task = make_post_intervention_task(&db, &tx).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    record_guard_deferred_attempt(
        &db,
        &task.id,
        "worker",
        GuardReason::RespawnGuard,
        Some("blocked by in-flight attempt"),
    )
    .await
    .expect("deferred row");

    let ledger = query_operator_ledger(&db, &task.id, task.last_intervention_at.as_deref()).await;
    assert_eq!(ledger.len(), 1);

    let row = &ledger[0];
    assert!(
        !row.log_tail_present,
        "guard-deferred rows have no log_tail"
    );
    assert_eq!(row.log_tail_error_class, None);
    assert_eq!(
        row.summary_json, None,
        "guard-deferred rows have no summary_json"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// AC3: i3mv pending/submitted-head behavior preserved via ledger.
//      Stale CI evidence older than the newest submitted attempt cannot
//      explain a strike; concluded rejection/reopen on that head CAN.
// ═════════════════════════════════════════════════════════════════════════════

/// AC3a: When the newest post-intervention attempt is `submitted` (pending
/// review), the ledger shows it as non-terminal.  A PostInterventionHistory
/// computed from this state reports `submission_pending_review=true`, proving
/// stale CI evidence cannot explain a strike.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zps8_submitted_head_in_ledger_blocks_stale_ci_strike() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let actor = coordinator_actor_for_tests(&db, &tx);

    let task = make_post_intervention_task(&db, &tx).await;

    // Seed stale CI evidence (from a prior head).
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    repo.upsert_ci_snapshot(djinn_core::models::TaskPrCiSnapshotInput {
        task_id: task.id.clone(),
        pr_number: 42,
        head_sha: "stale-head-sha".to_string(),
        ci_status: djinn_core::models::CiStatus::Failing,
        blocking_required_check_names: vec!["CI Test".to_string()],
        failure_fingerprint: Some("fp:stale-ci".to_string()),
        same_signature_count: 1,
        last_remediation_base_sha: None,
    })
    .await
    .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Seed a submitted attempt AFTER the stale CI evidence.
    let (_attempt_id, _submitted_at) =
        seed_submitted_attempt(&db, &task.id, "worker", Some("post-ci submit"), None).await;

    // Verify the ledger shows the submitted attempt.
    let ledger = query_operator_ledger(&db, &task.id, task.last_intervention_at.as_deref()).await;
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].outcome, "submitted");
    assert!(ledger[0].submitted_at.is_some());
    assert!(ledger[0].terminal_at.is_none());

    // Verify the history confirms submission_pending_review, blocking stale CI.
    repo.set_status(&task.id, "needs_task_review")
        .await
        .unwrap();
    let task = repo.get(&task.id).await.unwrap().unwrap();
    let history = actor.post_intervention_history(&task).await;

    assert!(
        history.submission_pending_review,
        "submitted head must block stale CI from explaining a strike"
    );
    assert!(history.any_submitted);
    assert!(history.non_attempt_models.is_empty());
}

/// AC3b: When the submitted attempt is terminalized as `reopened` (concluded
/// rejection), the ledger shows it as terminal.  The PostInterventionHistory
/// reports `submission_pending_review=false`, allowing the rejection to
/// explain a park/strike.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zps8_concluded_rejection_in_ledger_explains_strike() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let actor = coordinator_actor_for_tests(&db, &tx);

    let task = make_post_intervention_task(&db, &tx).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let (attempt_id, _) =
        seed_submitted_attempt(&db, &task.id, "worker", Some("submit for review"), None).await;

    // Terminalize as reopened (concluded rejection).
    terminalize_attempt(
        &db,
        &attempt_id,
        TaskAttemptOutcome::Reopened,
        Some("reviewer rejected: AC not met"),
        None,
        None,
    )
    .await;

    // Verify ledger shows terminal rejection.
    let ledger = query_operator_ledger(&db, &task.id, task.last_intervention_at.as_deref()).await;
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].outcome, "reopened");
    assert!(ledger[0].terminal_at.is_some());

    // Verify history confirms submission_pending_review=false, explaining strike.
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = repo.get(&task.id).await.unwrap().unwrap();
    let history = actor.post_intervention_history(&task).await;

    assert!(
        !history.submission_pending_review,
        "concluded rejection must allow the strike to be explained"
    );
    assert!(
        history.any_submitted,
        "the attempt did submit before rejection"
    );
    assert_eq!(
        history.most_recent_reopen_class,
        djinn_core::models::ReopenClass::ReviewRejected,
        "reopen class must be ReviewRejected"
    );
}

/// AC3c: A pending attempt (not yet submitted) also blocks stale CI.  The
/// ledger shows it as pending (non-terminal), and the guard defers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zps8_pending_head_in_ledger_is_inflight_not_strike() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let _actor = coordinator_actor_for_tests(&db, &tx);

    let task = make_post_intervention_task(&db, &tx).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    seed_pending_attempt(&db, &task.id, "worker").await;

    // Ledger shows the pending attempt.
    let ledger = query_operator_ledger(&db, &task.id, task.last_intervention_at.as_deref()).await;
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].outcome, "pending");
    assert!(ledger[0].terminal_at.is_none());

    // Guard defers for this task+role.
    let decision = run_respawn_guard(&db, &task.id, "worker", None, None).await;
    assert_eq!(
        decision,
        RespawnGuardDecision::Defer(GuardReason::RespawnGuard),
        "guard must defer when a pending in-flight attempt exists in ledger"
    );
}

/// AC3d: After a concluded rejection, the park reason uses the correct
/// "acceptance criteria" phrasing, derived from the ledger's terminal
/// rejection evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zps8_concluded_rejection_park_reason_from_ledger_evidence() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let actor = coordinator_actor_for_tests(&db, &tx);

    let task = make_post_intervention_task(&db, &tx).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let (attempt_id, _) =
        seed_submitted_attempt(&db, &task.id, "worker", Some("submit"), None).await;
    terminalize_attempt(
        &db,
        &attempt_id,
        TaskAttemptOutcome::Reopened,
        Some("review rejected"),
        None,
        None,
    )
    .await;

    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = repo.get(&task.id).await.unwrap().unwrap();
    let history = actor.post_intervention_history(&task).await;

    let reason = CoordinatorActor::compute_park_reason(&task, &history);
    assert!(
        reason.contains("acceptance criteria"),
        "park reason must cite acceptance criteria from rejection evidence; got: {reason}"
    );

    // Verify the ledger provides the supporting evidence.
    let ledger = query_operator_ledger(&db, &task.id, task.last_intervention_at.as_deref()).await;
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].outcome, "reopened");
    assert!(ledger[0].terminal_at.is_some());
}
