use crate::*;
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attempt_seq_independent_across_tasks() {
    let db = test_db();
    let (_pid1, task_a) = create_task(&db).await;
    let (_pid2, task_b) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    // Three attempts on task A.
    for i in 1..=3 {
        let id = new_attempt_id();
        repo.create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_a,
            role: "worker",
            dispatch_key: &format!("dk-a-{i}"),
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();
    }

    // First attempt on task B should be seq=1, not seq=4.
    let id_b = new_attempt_id();
    let b = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id_b,
            task_id: &task_b,
            role: "worker",
            dispatch_key: "dk-b-1",
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();
    assert_eq!(b.attempt_seq, 1);
    assert_eq!(b.task_id, task_b);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_key_unique_constraint_prevents_cross_id_collision() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    // First insert succeeds.
    let id1 = new_attempt_id();
    repo.create_or_get_pending(CreateTaskAttemptParams {
        id: &id1,
        task_id: &task_id,
        role: "worker",
        dispatch_key: "dk-unique",
        session_id: None,
        attempt_seq: None,
        dispatch_owner_incarnation_id: None,
        dispatch_group_id: None,
    })
    .await
    .unwrap();

    // Second insert with same dispatch_key but different id and attempt_seq
    // still returns the original row (ON CONFLICT DO NOTHING).
    let id2 = new_attempt_id();
    let a2 = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id2,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-unique",
            session_id: None,
            attempt_seq: Some(999),
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();
    assert_eq!(a2.id, id1);
    assert_eq!(a2.attempt_seq, 1); // original seq, not 999
}

// ── AC2: lifecycle forward-only, terminal→nonterminal rejected ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_to_terminal_direct_skipping_submitted() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-direct-term",
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();
    assert_eq!(attempt.outcome, "pending");

    // Advance directly from pending to terminal (skip submitted).
    let terminal = repo
        .advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: TaskAttemptOutcome::Crashed,
            pr_url: None,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("crashed early"),
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();

    assert_eq!(terminal.outcome, "crashed");
    assert!(terminal.terminal_at.is_some());
    assert_eq!(terminal.summary.as_deref(), Some("crashed early"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advance_to_terminal_rejects_non_terminal_outcome() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-nonterm",
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    let err = repo
        .advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: TaskAttemptOutcome::Pending,
            pr_url: None,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: None,
            summary_json: None,
            log_tail: None,
        })
        .await;
    assert!(err.is_err());

    let err = repo
        .advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: TaskAttemptOutcome::Submitted,
            pr_url: None,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: None,
            summary_json: None,
            log_tail: None,
        })
        .await;
    assert!(err.is_err());

    // Row unchanged (still pending).
    let row = repo.get(&attempt.id).await.unwrap().unwrap();
    assert_eq!(row.outcome, "pending");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_outcome_is_frozen_after_first_terminal() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-frozen",
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    // First terminal wins: move to completed.
    repo.advance_to_terminal(TerminalTaskAttemptParams {
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
    let after_completed = repo.get(&attempt.id).await.unwrap().unwrap();
    let frozen_terminal_at = after_completed.terminal_at.clone();
    assert!(frozen_terminal_at.is_some());

    // A different terminal outcome (`force_closed`) is a no-op once terminal:
    // the SQL predicate only matches non-terminal rows or an identical terminal
    // outcome, so the frozen `completed` outcome is preserved (no rank ordering).
    repo.advance_to_terminal(TerminalTaskAttemptParams {
        id: &attempt.id,
        outcome: TaskAttemptOutcome::ForceClosed,
        pr_url: Some("http://other-pr"),
        submit_ref: None,
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary: Some("clobber"),
        summary_json: None,
        log_tail: None,
    })
    .await
    .unwrap();

    let after_force = repo.get(&attempt.id).await.unwrap().unwrap();
    assert_eq!(
        after_force.outcome, "completed",
        "a different terminal outcome must not overwrite the first terminal"
    );
    // No fields were touched: neither the outcome, terminal_at, nor the
    // already-filled pr_url/summary changed.
    assert_eq!(after_force.terminal_at, frozen_terminal_at);
    assert_eq!(after_force.pr_url.as_deref(), Some("http://pr"));
    assert_eq!(after_force.summary.as_deref(), Some("done"));

    // Another different terminal (`handoff`) is likewise a no-op.
    let handoff = repo
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
    assert_eq!(
        handoff.outcome, "completed",
        "the first terminal outcome stays frozen across further terminal calls"
    );

    // The same terminal outcome remains idempotent (no error, no change).
    let idem = repo
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
    assert_eq!(idem.outcome, "completed");
    assert_eq!(idem.terminal_at, frozen_terminal_at);
}

// ── AC3: nullable guard-only rows, fill-forward, lookups, ordering ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_rows_start_with_nullable_refs() {
    let db = test_db();
    let (project_id, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db.clone());

    // Create a session so the FK constraint is satisfied.
    let session_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO sessions (id, project_id, task_id, model_id, agent_type, status)
         VALUES ($1, $2, $3, 'model-1', 'worker', 'running')",
    )
    .bind(&session_id)
    .bind(&project_id)
    .bind(&task_id)
    .execute(db.pool())
    .await
    .unwrap();

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-nullable",
            session_id: Some(&session_id),
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    // Session_id is set, but refs/summaries are null.
    assert_eq!(attempt.session_id.as_deref(), Some(session_id.as_str()));
    assert!(attempt.summary.is_none());
    assert!(attempt.summary_json.is_none());
    assert!(attempt.log_tail.is_none());
    assert!(attempt.checkpoint_ref.is_none());
    assert!(attempt.submit_ref.is_none());
    assert!(attempt.pr_url.is_none());
    assert!(attempt.mirror_head_sha.is_none());
    assert!(attempt.github_head_sha.is_none());
    assert!(attempt.submitted_at.is_none());
    assert!(attempt.terminal_at.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_only_row_without_session_id() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-no-session",
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    assert!(attempt.session_id.is_none());
    assert_eq!(attempt.outcome, "pending");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fill_forward_preserves_existing_values() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-fill-preserve",
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    // Set initial values via submit.
    repo.advance_to_submitted(SubmitTaskAttemptParams {
        id: &attempt.id,
        submit_ref: Some("original-submit"),
        checkpoint_ref: Some("original-cp"),
        mirror_head_sha: None,
        github_head_sha: None,
        summary: Some("original-summary"),
        summary_json: None,
        log_tail: Some("original-tail"),
    })
    .await
    .unwrap();

    // Fill-nullable should NOT overwrite existing values.
    repo.fill_nullable_fields(FillTaskAttemptParams {
        id: &attempt.id,
        checkpoint_ref: Some("new-cp"),
        submit_ref: Some("new-submit"),
        pr_url: Some("new-pr"),
        mirror_head_sha: Some("new-mirror"),
        github_head_sha: Some("new-github"),
        github_publication_error: None,
        summary: Some("new-summary"),
        summary_json: Some(r#"{"new": true}"#),
        log_tail: Some("new-tail"),
    })
    .await
    .unwrap();

    let filled = repo.get(&attempt.id).await.unwrap().unwrap();
    // Previously-set values are preserved (COALESCE behavior).
    assert_eq!(filled.submit_ref.as_deref(), Some("original-submit"));
    assert_eq!(filled.checkpoint_ref.as_deref(), Some("original-cp"));
    assert_eq!(filled.summary.as_deref(), Some("original-summary"));
    assert_eq!(filled.log_tail.as_deref(), Some("original-tail"));
    // Previously-null values are filled.
    assert_eq!(filled.pr_url.as_deref(), Some("new-pr"));
    assert_eq!(filled.mirror_head_sha.as_deref(), Some("new-mirror"));
    assert_eq!(filled.github_head_sha.as_deref(), Some("new-github"));
    assert_eq!(filled.summary_json.as_deref(), Some(r#"{"new": true}"#));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn latest_pending_or_submitted_returns_none_when_all_terminal() {
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
                dispatch_key: &format!("dk-all-term-{i}"),
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
    }

    let latest = repo
        .latest_pending_or_submitted(&task_id, None)
        .await
        .unwrap();
    assert!(latest.is_none(), "all attempts are terminal");

    let latest_pending = repo.latest_pending(&task_id, None).await.unwrap();
    assert!(latest_pending.is_none());

    let latest_submitted = repo.latest_submitted(&task_id, None).await.unwrap();
    assert!(latest_submitted.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_for_task_with_role_filter() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    // Worker attempt.
    let w_id = new_attempt_id();
    let w = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &w_id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-role-w",
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();
    repo.advance_to_terminal(TerminalTaskAttemptParams {
        id: &w.id,
        outcome: TaskAttemptOutcome::Completed,
        pr_url: None,
        submit_ref: None,
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary: Some("worker done"),
        summary_json: None,
        log_tail: None,
    })
    .await
    .unwrap();

    // Planner attempt.
    let p_id = new_attempt_id();
    let p = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &p_id,
            task_id: &task_id,
            role: "planner",
            dispatch_key: "dk-role-p",
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();
    repo.advance_to_terminal(TerminalTaskAttemptParams {
        id: &p.id,
        outcome: TaskAttemptOutcome::Completed,
        pr_url: None,
        submit_ref: None,
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary: Some("planner done"),
        summary_json: None,
        log_tail: None,
    })
    .await
    .unwrap();

    // Unfiltered history returns both.
    let all = repo.history_for_task(&task_id, None, 100).await.unwrap();
    assert_eq!(all.len(), 2);

    // Role-filtered history returns only worker.
    let worker_only = repo
        .history_for_task(&task_id, Some("worker"), 100)
        .await
        .unwrap();
    assert_eq!(worker_only.len(), 1);
    assert_eq!(worker_only[0].role, "worker");
    assert_eq!(worker_only[0].summary.as_deref(), Some("worker done"));

    // Role-filtered history returns only planner.
    let planner_only = repo
        .history_for_task(&task_id, Some("planner"), 100)
        .await
        .unwrap();
    assert_eq!(planner_only.len(), 1);
    assert_eq!(planner_only[0].role, "planner");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_summaries_zero_limit_returns_empty() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    // Create an attempt so there's data.
    let id = new_attempt_id();
    repo.create_or_get_pending(CreateTaskAttemptParams {
        id: &id,
        task_id: &task_id,
        role: "worker",
        dispatch_key: "dk-zero",
        session_id: None,
        attempt_seq: None,
        dispatch_owner_incarnation_id: None,
        dispatch_group_id: None,
    })
    .await
    .unwrap();

    let summaries = repo
        .prompt_summaries_for_task(&task_id, None, 0)
        .await
        .unwrap();
    assert!(summaries.is_empty());

    let history = repo.history_for_task(&task_id, None, 0).await.unwrap();
    assert!(history.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_for_task_returns_empty_for_nonexistent_task() {
    let db = test_db();
    let repo = TaskAttemptRepository::new(db);

    let attempts = repo.list_for_task("nonexistent-task-id").await.unwrap();
    assert!(attempts.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_row_shape_includes_all_expected_fields() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-shape",
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    repo.advance_to_submitted(SubmitTaskAttemptParams {
        id: &attempt.id,
        submit_ref: Some("submit-ref-val"),
        checkpoint_ref: Some("cp-val"),
        mirror_head_sha: Some("mirror-sha-val"),
        github_head_sha: Some("github-sha-val"),
        summary: Some("summary-val"),
        summary_json: Some(r#"{"status": "ok"}"#),
        log_tail: Some("tail-val"),
    })
    .await
    .unwrap();

    repo.advance_to_terminal(TerminalTaskAttemptParams {
        id: &attempt.id,
        outcome: TaskAttemptOutcome::Completed,
        pr_url: Some("http://example.com/pr/42"),
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

    let history = repo.history_for_task(&task_id, None, 10).await.unwrap();
    assert_eq!(history.len(), 1);
    let h = &history[0];

    assert_eq!(h.id, id);
    assert_eq!(h.task_id, task_id);
    assert_eq!(h.role, "worker");
    assert_eq!(h.attempt_seq, 1);
    assert_eq!(h.dispatch_key, "dk-shape");
    assert_eq!(h.outcome, "completed");
    assert!(h.session_id.is_none());
    assert!(h.guard_decision.is_none());
    assert!(h.guard_reason.is_none());
    assert_eq!(h.summary.as_deref(), Some("summary-val"));
    assert_eq!(h.checkpoint_ref.as_deref(), Some("cp-val"));
    assert_eq!(h.submit_ref.as_deref(), Some("submit-ref-val"));
    assert_eq!(h.pr_url.as_deref(), Some("http://example.com/pr/42"));
    assert_eq!(h.mirror_head_sha.as_deref(), Some("mirror-sha-val"));
    assert_eq!(h.github_head_sha.as_deref(), Some("github-sha-val"));
    assert!(!h.created_at.is_empty());
    assert!(h.submitted_at.is_some());
    assert!(h.terminal_at.is_some());
}

// ── AC4: bounded fields and JSON validity ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_dispatch_key_rejected() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let err = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &new_attempt_id(),
            task_id: &task_id,
            role: "worker",
            dispatch_key: "",
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await;
    assert!(err.is_err());

    let err = repo
        .insert_guard_deferred(GuardDeferTaskAttemptParams {
            id: &new_attempt_id(),
            task_id: &task_id,
            role: "guard",
            dispatch_key: "",
            decision: GuardDecision::Defer,
            reason: GuardReason::ParkRung,
            summary: None,
            summary_json: None,
            log_tail: None,
        })
        .await;
    assert!(err.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn negative_attempt_seq_rejected() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let err = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &new_attempt_id(),
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-neg-seq",
            session_id: None,
            attempt_seq: Some(-1),
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await;
    assert!(err.is_err());

    let err = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &new_attempt_id(),
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-zero-seq",
            session_id: None,
            attempt_seq: Some(0),
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await;
    assert!(err.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_deferred_rejects_oversize_summary_and_log_tail() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let big_summary = "x".repeat(TASK_ATTEMPT_SUMMARY_MAX_LEN + 1);
    let err = repo
        .insert_guard_deferred(GuardDeferTaskAttemptParams {
            id: &new_attempt_id(),
            task_id: &task_id,
            role: "guard",
            dispatch_key: "dk-guard-big-sum",
            decision: GuardDecision::Defer,
            reason: GuardReason::ParkRung,
            summary: Some(&big_summary),
            summary_json: None,
            log_tail: None,
        })
        .await;
    assert!(err.is_err());

    let big_tail = "x".repeat(TASK_ATTEMPT_LOG_TAIL_MAX_LEN + 1);
    let err = repo
        .insert_guard_deferred(GuardDeferTaskAttemptParams {
            id: &new_attempt_id(),
            task_id: &task_id,
            role: "guard",
            dispatch_key: "dk-guard-big-tail",
            decision: GuardDecision::Defer,
            reason: GuardReason::ParkRung,
            summary: None,
            summary_json: None,
            log_tail: Some(&big_tail),
        })
        .await;
    assert!(err.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_rejects_invalid_summary_json() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-term-json",
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    // Malformed JSON rejected.
    let err = repo
        .advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: TaskAttemptOutcome::Completed,
            pr_url: None,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: None,
            summary_json: Some("{not valid"),
            log_tail: None,
        })
        .await;
    assert!(err.is_err());

    // Array JSON rejected.
    let err = repo
        .advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: TaskAttemptOutcome::Completed,
            pr_url: None,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: None,
            summary_json: Some("[1, 2]"),
            log_tail: None,
        })
        .await;
    assert!(err.is_err());

    // Row unchanged.
    let row = repo.get(&attempt.id).await.unwrap().unwrap();
    assert_eq!(row.outcome, "pending");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_rejects_oversize_summary_and_log_tail() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-term-oversize",
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    let big_summary = "x".repeat(TASK_ATTEMPT_SUMMARY_MAX_LEN + 1);
    let err = repo
        .advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: TaskAttemptOutcome::Completed,
            pr_url: None,
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
            log_tail: Some(&big_tail),
        })
        .await;
    assert!(err.is_err());

    // Row unchanged.
    let row = repo.get(&attempt.id).await.unwrap().unwrap();
    assert_eq!(row.outcome, "pending");
}
