use crate::*;
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_or_get_pending_creates_row_and_returns_record() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-1",
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    assert_eq!(attempt.id, id);
    assert_eq!(attempt.task_id, task_id);
    assert_eq!(attempt.role, "worker");
    assert_eq!(attempt.attempt_seq, 1);
    assert_eq!(attempt.dispatch_key, "dk-1");
    assert_eq!(attempt.outcome, "pending");
    assert!(attempt.session_id.is_none());
    assert!(attempt.summary.is_none());
    assert!(attempt.terminal_at.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_or_get_pending_is_idempotent_on_dispatch_key() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let a1 = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-idem",
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    let id2 = new_attempt_id();
    let a2 = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id2,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-idem",
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    assert_eq!(a1.id, a2.id);
    assert_eq!(a1.attempt_seq, a2.attempt_seq);
    let attempts = repo.list_for_task(&task_id).await.unwrap();
    assert_eq!(attempts.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attempt_seq_is_monotonic_per_task() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    for i in 1..=3 {
        let id = new_attempt_id();
        repo.create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: &format!("dk-{i}"),
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();
    }

    let attempts = repo.list_for_task(&task_id).await.unwrap();
    let seqs: Vec<i32> = attempts.iter().map(|a| a.attempt_seq).collect();
    assert_eq!(seqs, vec![3, 2, 1]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advance_to_submitted_moves_pending_forward() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-submit",
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    let submitted = repo
        .advance_to_submitted(SubmitTaskAttemptParams {
            id: &attempt.id,
            submit_ref: Some("submit-1"),
            checkpoint_ref: Some("cp-1"),
            mirror_head_sha: Some("mirror-sha"),
            github_head_sha: Some("github-sha"),
            summary: Some("summary"),
            summary_json: Some(r#"{"key": "value"}"#),
            log_tail: Some("log"),
        })
        .await
        .unwrap();

    assert_eq!(submitted.outcome, "submitted");
    assert!(submitted.submitted_at.is_some());
    assert_eq!(submitted.submit_ref.as_deref(), Some("submit-1"));
    assert_eq!(submitted.checkpoint_ref.as_deref(), Some("cp-1"));
    assert_eq!(submitted.summary.as_deref(), Some("summary"));
    // jsonb canonicalizes to a space after the colon on read-back.
    assert_eq!(
        submitted.summary_json.as_deref(),
        Some(r#"{"key": "value"}"#)
    );
    assert_eq!(submitted.log_tail.as_deref(), Some("log"));

    // Idempotent: same call again returns same row.
    let submitted2 = repo
        .advance_to_submitted(SubmitTaskAttemptParams {
            id: &attempt.id,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: None,
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();
    assert_eq!(submitted2.outcome, "submitted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advance_to_terminal_is_forward_only_and_idempotent() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-term",
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    repo.advance_to_submitted(SubmitTaskAttemptParams {
        id: &attempt.id,
        submit_ref: None,
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary: None,
        summary_json: None,
        log_tail: None,
    })
    .await
    .unwrap();

    let terminal = repo
        .advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: TaskAttemptOutcome::Completed,
            pr_url: Some("http://pr"),
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("done"),
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();

    assert_eq!(terminal.outcome, "completed");
    assert_eq!(terminal.pr_url.as_deref(), Some("http://pr"));
    assert_eq!(terminal.summary.as_deref(), Some("done"));
    assert!(terminal.terminal_at.is_some());
    let first_terminal_at = terminal.terminal_at.clone();

    // Idempotent: repeated terminal with same outcome is no-op and preserves
    // the original terminal_at and filled fields.
    let terminal2 = repo
        .advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: TaskAttemptOutcome::Completed,
            pr_url: None,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: None,
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();
    assert_eq!(terminal2.id, terminal.id);
    assert_eq!(terminal2.outcome, "completed");
    assert_eq!(terminal2.terminal_at, first_terminal_at);
    // pr_url should remain filled from first terminal call.
    assert_eq!(terminal2.pr_url.as_deref(), Some("http://pr"));

    // First-terminal-wins: once a row is terminal (`completed`), the SQL
    // predicate only permits an idempotent same-outcome move, so switching to a
    // *different* terminal outcome (`handoff`) is a no-op. The row keeps its
    // existing `completed` outcome, its original terminal_at, and its filled
    // fields (no overwrite, no rollback).
    let switched = repo
        .advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: TaskAttemptOutcome::Handoff,
            pr_url: None,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: None,
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();
    assert_eq!(switched.outcome, "completed");
    assert_eq!(switched.terminal_at, first_terminal_at);
    // Previously filled fields remain (no rollback).
    assert_eq!(switched.pr_url.as_deref(), Some("http://pr"));
    assert_eq!(switched.summary.as_deref(), Some("done"));

    // A second different terminal outcome (`force_closed`) is likewise a no-op:
    // the frozen `completed` outcome is preserved.
    let switched_again = repo
        .advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: TaskAttemptOutcome::ForceClosed,
            pr_url: None,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: None,
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();
    assert_eq!(switched_again.outcome, "completed");
    assert_eq!(switched_again.pr_url.as_deref(), Some("http://pr"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advance_to_submitted_does_not_roll_back_terminal() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-submit-on-terminal",
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    repo.advance_to_terminal(TerminalTaskAttemptParams {
        id: &attempt.id,
        outcome: TaskAttemptOutcome::Completed,
        pr_url: None,
        submit_ref: None,
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary: None,
        summary_json: None,
        log_tail: None,
    })
    .await
    .unwrap();

    let after_submit = repo
        .advance_to_submitted(SubmitTaskAttemptParams {
            id: &attempt.id,
            submit_ref: Some("submit-after-terminal"),
            checkpoint_ref: Some("cp-after-terminal"),
            mirror_head_sha: None,
            github_head_sha: None,
            summary: None,
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();

    assert_eq!(after_submit.outcome, "completed");
    assert!(after_submit.terminal_at.is_some());
    // Refs should not be filled on a terminal row by the submit helper.
    assert!(after_submit.submit_ref.is_none());
    assert!(after_submit.checkpoint_ref.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fill_nullable_fields_fills_without_rolling_back_outcome() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-fill",
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    repo.advance_to_terminal(TerminalTaskAttemptParams {
        id: &attempt.id,
        outcome: TaskAttemptOutcome::Completed,
        pr_url: None,
        submit_ref: None,
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary: None,
        summary_json: None,
        log_tail: None,
    })
    .await
    .unwrap();

    repo.fill_nullable_fields(FillTaskAttemptParams {
        id: &attempt.id,
        checkpoint_ref: Some("cp-fill"),
        submit_ref: Some("submit-fill"),
        pr_url: Some("pr-fill"),
        mirror_head_sha: Some("mirror-fill"),
        github_head_sha: Some("github-fill"),
        github_publication_error: None,
        summary: Some("summary-fill"),
        summary_json: Some(r#"{"filled": true}"#),
        log_tail: Some("tail-fill"),
    })
    .await
    .unwrap();

    let filled = repo.get(&attempt.id).await.unwrap().unwrap();
    assert_eq!(filled.outcome, "completed");
    assert_eq!(filled.checkpoint_ref.as_deref(), Some("cp-fill"));
    assert_eq!(filled.submit_ref.as_deref(), Some("submit-fill"));
    assert_eq!(filled.pr_url.as_deref(), Some("pr-fill"));
    assert_eq!(filled.mirror_head_sha.as_deref(), Some("mirror-fill"));
    assert_eq!(filled.github_head_sha.as_deref(), Some("github-fill"));
    assert_eq!(filled.summary.as_deref(), Some("summary-fill"));
    assert_eq!(filled.summary_json.as_deref(), Some(r#"{"filled": true}"#));
    assert_eq!(filled.log_tail.as_deref(), Some("tail-fill"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_deferred_row_has_no_session_and_is_terminal() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .insert_guard_deferred(GuardDeferTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "guard",
            dispatch_key: "dk-guard",
            decision: GuardDecision::Defer,
            reason: GuardReason::ParkRung,
            summary: Some("parked"),
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();

    assert_eq!(attempt.outcome, "deferred");
    assert_eq!(attempt.guard_decision.as_deref(), Some("defer"));
    assert_eq!(attempt.guard_reason.as_deref(), Some("park_rung"));
    assert!(attempt.session_id.is_none());
    assert!(attempt.terminal_at.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn latest_pending_or_submitted_and_lookups_work() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id1 = new_attempt_id();
    let a1 = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id1,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-latest-1",
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    let id2 = new_attempt_id();
    let a2 = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id2,
            task_id: &task_id,
            role: "planner",
            dispatch_key: "dk-latest-2",
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    repo.advance_to_submitted(SubmitTaskAttemptParams {
        id: &a2.id,
        submit_ref: None,
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary: None,
        summary_json: None,
        log_tail: None,
    })
    .await
    .unwrap();

    let latest = repo
        .latest_pending_or_submitted(&task_id, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.id, a2.id);

    let latest_worker = repo
        .latest_pending(&task_id, Some("worker"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest_worker.id, a1.id);

    let latest_planner = repo
        .latest_submitted(&task_id, Some("planner"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest_planner.id, a2.id);

    let by_key = repo
        .get_by_dispatch_key("dk-latest-2")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_key.id, a2.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_summaries_and_history_ordered_newest_first() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    for i in 1..=3 {
        let id = new_attempt_id();
        let attempt = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: &format!("dk-order-{i}"),
                session_id: None,
                attempt_seq: None,
                dispatch_owner_incarnation_id: None,
                dispatch_group_id: None,
            })
            .await
            .unwrap();
        repo.advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: TaskAttemptOutcome::Completed,
            pr_url: None,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some(&format!("summary {i}")),
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let summaries = repo
        .prompt_summaries_for_task(&task_id, None, 2)
        .await
        .unwrap();
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].attempt_seq, 3);
    assert_eq!(summaries[1].attempt_seq, 2);

    let history = repo.history_for_task(&task_id, None, 10).await.unwrap();
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].attempt_seq, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bounded_fields_rejected_when_too_large() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let big_summary = "x".repeat(TASK_ATTEMPT_SUMMARY_MAX_LEN + 1);
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-1",
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();
    assert!(attempt.summary.is_none());

    let err = repo
        .advance_to_submitted(SubmitTaskAttemptParams {
            id: &attempt.id,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some(&big_summary),
            summary_json: None,
            log_tail: None,
        })
        .await;
    assert!(err.is_err());

    let big_tail = "x".repeat(TASK_ATTEMPT_LOG_TAIL_MAX_LEN + 1);
    let err = repo
        .fill_nullable_fields(FillTaskAttemptParams {
            id: &attempt.id,
            checkpoint_ref: None,
            submit_ref: None,
            pr_url: None,
            mirror_head_sha: None,
            github_head_sha: None,
            github_publication_error: None,
            summary: None,
            summary_json: None,
            log_tail: Some(&big_tail),
        })
        .await;
    assert!(err.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_summary_json_rejected() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-json",
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    // Malformed JSON is rejected.
    let err = repo
        .advance_to_submitted(SubmitTaskAttemptParams {
            id: &attempt.id,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: None,
            summary_json: Some("not json"),
            log_tail: None,
        })
        .await;
    assert!(err.is_err());

    // Non-object JSON (array) is rejected.
    let err = repo
        .advance_to_submitted(SubmitTaskAttemptParams {
            id: &attempt.id,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: None,
            summary_json: Some("[1, 2, 3]"),
            log_tail: None,
        })
        .await;
    assert!(err.is_err());

    // Non-object JSON (scalar string) is rejected.
    let err = repo
        .fill_nullable_fields(FillTaskAttemptParams {
            id: &attempt.id,
            checkpoint_ref: None,
            submit_ref: None,
            pr_url: None,
            mirror_head_sha: None,
            github_head_sha: None,
            github_publication_error: None,
            summary: None,
            summary_json: Some("\"string\""),
            log_tail: None,
        })
        .await;
    assert!(err.is_err());

    // Valid JSON object is accepted.
    let submitted = repo
        .advance_to_submitted(SubmitTaskAttemptParams {
            id: &attempt.id,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: None,
            summary_json: Some(r#"{"ok": true}"#),
            log_tail: None,
        })
        .await
        .unwrap();
    assert_eq!(submitted.summary_json.as_deref(), Some(r#"{"ok": true}"#));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_key_length_bound_enforced() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let long_key = "k".repeat(TASK_ATTEMPT_DISPATCH_KEY_MAX_LEN + 1);
    let err = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: &long_key,
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await;
    assert!(err.is_err());

    // Exactly the max length is allowed.
    let max_key = "k".repeat(TASK_ATTEMPT_DISPATCH_KEY_MAX_LEN);
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &new_attempt_id(),
            task_id: &task_id,
            role: "worker",
            dispatch_key: &max_key,
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();
    assert_eq!(attempt.dispatch_key, max_key);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_historical_backfill_after_task_run_and_session_creation() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db.clone());

    // task_attempts starts empty for a newly-created task.
    let initial_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM task_attempts WHERE task_id = $1")
            .bind(&task_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(initial_count, 0);

    // Create a session and a task_run for the same task, mimicking the
    // pre-existing substrate. No task_attempts row should be created.
    let session_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO sessions (id, project_id, task_id, model_id, agent_type, status)
         VALUES ($1, $2, $3, 'model-1', 'worker', 'running')",
    )
    .bind(&session_id)
    .bind(&_pid)
    .bind(&task_id)
    .execute(db.pool())
    .await
    .unwrap();

    let run_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status)
         VALUES ($1, $2, $3, 'new_task', 'running')",
    )
    .bind(&run_id)
    .bind(&_pid)
    .bind(&task_id)
    .execute(db.pool())
    .await
    .unwrap();

    let after_preexisting_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM task_attempts WHERE task_id = $1")
            .bind(&task_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(after_preexisting_count, 0);

    // Only an explicit repository write populates the table.
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &new_attempt_id(),
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-backfill",
            session_id: Some(&session_id),
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    let final_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM task_attempts WHERE task_id = $1")
            .bind(&task_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(final_count, 1);
    assert_eq!(attempt.session_id.as_deref(), Some(session_id.as_str()));
}

// ── AC1: duplicate dispatch-key idempotency and per-task seq uniqueness ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_deferred_idempotent_on_dispatch_key() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db.clone());

    let id1 = new_attempt_id();
    let a1 = repo
        .insert_guard_deferred(GuardDeferTaskAttemptParams {
            id: &id1,
            task_id: &task_id,
            role: "guard",
            dispatch_key: "dk-guard-idem",
            decision: GuardDecision::Defer,
            reason: GuardReason::ParkRung,
            summary: Some("parked"),
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();

    let id2 = new_attempt_id();
    let a2 = repo
        .insert_guard_deferred(GuardDeferTaskAttemptParams {
            id: &id2,
            task_id: &task_id,
            role: "guard",
            dispatch_key: "dk-guard-idem",
            decision: GuardDecision::Defer,
            reason: GuardReason::LoopThreshold,
            summary: Some("loop"),
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();

    // Same dispatch_key → same row returned, no duplicate.
    assert_eq!(a1.id, a2.id);
    assert_eq!(a1.attempt_seq, a2.attempt_seq);
    // Original decision/reason preserved (ON CONFLICT DO NOTHING).
    assert_eq!(a2.guard_decision.as_deref(), Some("defer"));
    assert_eq!(a2.guard_reason.as_deref(), Some("park_rung"));

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM task_attempts WHERE task_id = $1")
        .bind(&task_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
}
