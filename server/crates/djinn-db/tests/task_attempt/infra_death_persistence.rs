use crate::*;

// ── AC3: Duplicate infra-death persistence ──────────────────────────────────
//
// When an infra-death capture runs concurrently with (or after) a real terminal
// report, the repository must:
//   - Preserve the first non-null `log_tail` (COALESCE semantics).
//   - Allow repeated calls to update `summary_json` (fetch metadata).
//   - Never create duplicate attempt rows.
//   - Never move a terminal row backward to a non-terminal outcome.

/// Compare two JSON strings by parsing and comparing values, ignoring key
/// ordering.  Postgres `jsonb` canonicalizes key order so raw string
/// comparison is unreliable.
fn json_eq(actual: &str, expected: &str) -> bool {
    let a: serde_json::Value = serde_json::from_str(actual).unwrap_or_default();
    let b: serde_json::Value = serde_json::from_str(expected).unwrap_or_default();
    a == b
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_terminal_preserves_first_log_tail() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-dup-term-1",
            session_id: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();

    // First terminal call carries a log_tail (infra-death capture).
    repo.advance_to_terminal(TerminalTaskAttemptParams {
        id: &attempt.id,
        outcome: TaskAttemptOutcome::Crashed,
        pr_url: None,
        submit_ref: None,
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary: Some("infra-death: OOMKilled"),
        summary_json: Some(r#"{"fetch_error": "none"}"#),
        log_tail: Some("first-infra-death-log-tail-data"),
    })
    .await
    .unwrap();

    // Second terminal call with the SAME outcome tries to write a different
    // log_tail.  The SQL uses COALESCE(log_tail, $10) so the first value is
    // preserved.
    let second = repo
        .advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: TaskAttemptOutcome::Crashed,
            pr_url: None,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: None,
            summary_json: Some(r#"{"fetch_error": "timeout"}"#),
            log_tail: Some("second-log-tail-should-not-overwrite"),
        })
        .await
        .unwrap();

    // log_tail preserved from the first call.
    assert_eq!(
        second.log_tail.as_deref(),
        Some("first-infra-death-log-tail-data"),
        "first non-null log_tail must be preserved"
    );

    // Only one attempt row exists.
    let attempts = repo.list_for_task(&task_id).await.unwrap();
    assert_eq!(attempts.len(), 1, "no duplicate attempt rows");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fill_nullable_preserves_first_log_tail_across_repeated_calls() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-fill-log-1",
            session_id: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();

    // Advance to terminal so fill_nullable_fields is safe on a terminal row.
    repo.advance_to_terminal(TerminalTaskAttemptParams {
        id: &attempt.id,
        outcome: TaskAttemptOutcome::Crashed,
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

    // First fill: sets log_tail.
    repo.fill_nullable_fields(FillTaskAttemptParams {
        id: &attempt.id,
        checkpoint_ref: None,
        submit_ref: None,
        pr_url: None,
        mirror_head_sha: None,
        github_head_sha: None,
        github_publication_error: None,
        summary: None,
        summary_json: Some(r#"{"capture": "ok"}"#),
        log_tail: Some("first-captured-log"),
    })
    .await
    .unwrap();

    // Second fill: tries to set a different log_tail.
    repo.fill_nullable_fields(FillTaskAttemptParams {
        id: &attempt.id,
        checkpoint_ref: None,
        submit_ref: None,
        pr_url: None,
        mirror_head_sha: None,
        github_head_sha: None,
        github_publication_error: None,
        summary: None,
        summary_json: Some(r#"{"capture": "retry"}"#),
        log_tail: Some("second-captured-log-should-not-overwrite"),
    })
    .await
    .unwrap();

    let filled = repo.get(&attempt.id).await.unwrap().unwrap();
    // First log_tail preserved.
    assert_eq!(
        filled.log_tail.as_deref(),
        Some("first-captured-log"),
        "fill_nullable_fields must preserve first non-null log_tail"
    );
    // summary_json was also set on first call and preserved on second.
    assert!(
        json_eq(
            filled.summary_json.as_deref().unwrap(),
            r#"{"capture": "ok"}"#
        ),
        "fill_nullable_fields must preserve first non-null summary_json"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn infra_death_fill_does_not_create_duplicate_rows() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let _attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-no-dup-fill",
            session_id: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();

    // Simulate multiple concurrent fill attempts.
    for i in 0..3 {
        repo.fill_nullable_fields(FillTaskAttemptParams {
            id: &id,
            checkpoint_ref: None,
            submit_ref: None,
            pr_url: None,
            mirror_head_sha: None,
            github_head_sha: None,
            github_publication_error: None,
            summary: None,
            summary_json: Some(&format!(r#"{{"attempt": {i}}}"#)),
            log_tail: Some(&format!("log-tail-attempt-{i}")),
        })
        .await
        .unwrap();
    }

    // Still exactly one row.
    let attempts = repo.list_for_task(&task_id).await.unwrap();
    assert_eq!(
        attempts.len(),
        1,
        "repeated fills must not create duplicate rows"
    );

    // The first fill's values are preserved.
    let row = &attempts[0];
    assert_eq!(
        row.log_tail.as_deref(),
        Some("log-tail-attempt-0"),
        "first non-null log_tail wins"
    );
    assert!(
        json_eq(row.summary_json.as_deref().unwrap(), r#"{"attempt": 0}"#),
        "first non-null summary_json wins"
    );
}

// ── AC4: Terminal-report precedence ─────────────────────────────────────────
//
// When a real terminal report arrives before or after an infra-death capture,
// the outcome must reflect the real terminal report, not be moved backward.
// The infra-death log_tail capture is purely diagnostic — it never overrides
// the outcome.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_terminal_report_wins_over_infra_death_outcome() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-report-wins",
            session_id: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();

    // Advance to submitted (worker is running).
    repo.advance_to_submitted(SubmitTaskAttemptParams {
        id: &attempt.id,
        submit_ref: Some("submit-1"),
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary: None,
        summary_json: None,
        log_tail: None,
    })
    .await
    .unwrap();

    // Real terminal report arrives first: the attempt is completed.
    let terminal = repo
        .advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: TaskAttemptOutcome::Completed,
            pr_url: Some("http://example.com/pr/42"),
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("task completed successfully"),
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();

    assert_eq!(terminal.outcome, "completed");

    // Infra-death capture arrives later, trying to set "crashed" via
    // `advance_to_terminal`.  The SQL WHERE clause does not match a
    // `completed` row when the requested outcome is `crashed`, so the
    // entire UPDATE is a no-op.
    let after_infra = repo
        .advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: TaskAttemptOutcome::Crashed,
            pr_url: None,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("infra-death: OOM"),
            summary_json: Some(r#"{"fetch_error": "none"}"#),
            log_tail: Some("oom-crash-log-tail"),
        })
        .await
        .unwrap();

    // The outcome must NOT have changed to "crashed".
    assert_eq!(
        after_infra.outcome, "completed",
        "real terminal report (completed) must not be overwritten by infra-death (crashed)"
    );
    // The advance_to_terminal with a different outcome is a full no-op:
    // the WHERE clause doesn't match, so log_tail is still null.
    assert!(
        after_infra.log_tail.is_none(),
        "advance_to_terminal with different terminal outcome is a full no-op (no field updates)"
    );
    // The real summary is preserved.
    assert_eq!(
        after_infra.summary.as_deref(),
        Some("task completed successfully"),
        "real terminal summary must be preserved"
    );

    // Use fill_nullable_fields to capture the infra-death diagnostic data
    // without changing the outcome.
    repo.fill_nullable_fields(FillTaskAttemptParams {
        id: &attempt.id,
        checkpoint_ref: None,
        submit_ref: None,
        pr_url: None,
        mirror_head_sha: None,
        github_head_sha: None,
        github_publication_error: None,
        summary: None,
        summary_json: Some(r#"{"fetch_error": "none"}"#),
        log_tail: Some("oom-crash-log-tail"),
    })
    .await
    .unwrap();

    let final_row = repo.get(&attempt.id).await.unwrap().unwrap();
    assert_eq!(final_row.outcome, "completed", "outcome unchanged by fill");
    assert_eq!(
        final_row.log_tail.as_deref(),
        Some("oom-crash-log-tail"),
        "infra-death log_tail captured via fill_nullable_fields as diagnostic"
    );
    assert_eq!(
        final_row.summary.as_deref(),
        Some("task completed successfully"),
        "real summary preserved after fill"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn infra_death_then_real_terminal_preserves_first_log_tail() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-infra-then-real",
            session_id: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();

    // Infra-death arrives first (crashed).
    repo.advance_to_terminal(TerminalTaskAttemptParams {
        id: &attempt.id,
        outcome: TaskAttemptOutcome::Crashed,
        pr_url: None,
        submit_ref: None,
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary: Some("infra-death: OOMKilled"),
        summary_json: Some(r#"{"fetch_error": "none", "capture": "ok"}"#),
        log_tail: Some("infra-log-tail-data"),
    })
    .await
    .unwrap();

    // Real terminal report arrives later with same outcome (idempotent).
    let after_real = repo
        .advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: TaskAttemptOutcome::Crashed,
            pr_url: Some("http://example.com/pr/99"),
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("worker reported crash reason"),
            summary_json: Some(r#"{"fetch_error": "none", "source": "report"}"#),
            log_tail: Some("real-report-log-tail-should-not-overwrite"),
        })
        .await
        .unwrap();

    // First log_tail preserved.
    assert_eq!(
        after_real.log_tail.as_deref(),
        Some("infra-log-tail-data"),
        "first non-null log_tail preserved even when real report arrives later"
    );
    // First summary preserved too.
    assert_eq!(
        after_real.summary.as_deref(),
        Some("infra-death: OOMKilled"),
        "first non-null summary preserved"
    );
    // First summary_json preserved.
    assert!(
        json_eq(
            after_real.summary_json.as_deref().unwrap(),
            r#"{"fetch_error": "none", "capture": "ok"}"#
        ),
        "first non-null summary_json preserved"
    );
    // pr_url from real report is filled (was null before).
    assert_eq!(
        after_real.pr_url.as_deref(),
        Some("http://example.com/pr/99"),
        "pr_url from real terminal report fills the null"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn different_terminal_outcome_is_full_noop() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-diff-term-noop",
            session_id: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();

    // Worker submits work.
    repo.advance_to_submitted(SubmitTaskAttemptParams {
        id: &attempt.id,
        submit_ref: Some("ref-1"),
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary: None,
        summary_json: None,
        log_tail: None,
    })
    .await
    .unwrap();

    // Infra-death sets crashed.
    repo.advance_to_terminal(TerminalTaskAttemptParams {
        id: &attempt.id,
        outcome: TaskAttemptOutcome::Crashed,
        pr_url: None,
        submit_ref: None,
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary: Some("infra-death"),
        summary_json: None,
        log_tail: Some("crash-log"),
    })
    .await
    .unwrap();

    // A "completed" terminal report cannot change outcome because
    // `completed` != `crashed` and the SQL only allows idempotent
    // same-outcome re-writes for terminal rows.  The entire UPDATE
    // is a no-op when the WHERE clause doesn't match.
    let result = repo
        .advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: TaskAttemptOutcome::Completed,
            pr_url: Some("http://example.com/pr/1"),
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("completed successfully"),
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();

    assert_eq!(
        result.outcome, "crashed",
        "completed cannot overwrite crashed — WHERE clause rejects different terminal outcome"
    );
    // The entire UPDATE is a no-op — pr_url is NOT filled because the
    // WHERE clause rejected the row.
    assert!(
        result.pr_url.is_none(),
        "advance_to_terminal with different terminal outcome is a full no-op"
    );
    // Log tail from the first call is preserved.
    assert_eq!(
        result.log_tail.as_deref(),
        Some("crash-log"),
        "log_tail from first terminal call preserved"
    );

    // Use fill_nullable_fields to capture the pr_url from the real report
    // (diagnostic fill).
    repo.fill_nullable_fields(FillTaskAttemptParams {
        id: &attempt.id,
        checkpoint_ref: None,
        submit_ref: None,
        pr_url: Some("http://example.com/pr/1"),
        mirror_head_sha: None,
        github_head_sha: None,
        github_publication_error: None,
        summary: None,
        summary_json: None,
        log_tail: None,
    })
    .await
    .unwrap();

    let filled = repo.get(&attempt.id).await.unwrap().unwrap();
    assert_eq!(
        filled.pr_url.as_deref(),
        Some("http://example.com/pr/1"),
        "pr_url captured via fill_nullable_fields"
    );
    assert_eq!(filled.outcome, "crashed", "outcome unchanged");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fill_nullable_on_terminal_row_captures_infra_death_data() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-fill-on-terminal",
            session_id: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();

    // Worker completes normally — real terminal report.
    repo.advance_to_terminal(TerminalTaskAttemptParams {
        id: &attempt.id,
        outcome: TaskAttemptOutcome::Completed,
        pr_url: Some("http://example.com/pr/7"),
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

    // Infra-death capture fills log_tail via fill_nullable_fields (diagnostic only).
    repo.fill_nullable_fields(FillTaskAttemptParams {
        id: &attempt.id,
        checkpoint_ref: None,
        submit_ref: None,
        pr_url: None,
        mirror_head_sha: None,
        github_head_sha: None,
        github_publication_error: None,
        summary: None,
        summary_json: Some(r#"{"infra_death": "OOMKilled", "capture": "ok"}"#),
        log_tail: Some("last-200-lines-of-pod-log"),
    })
    .await
    .unwrap();

    let row = repo.get(&attempt.id).await.unwrap().unwrap();
    assert_eq!(row.outcome, "completed", "outcome unchanged by fill");
    assert_eq!(
        row.summary.as_deref(),
        Some("done"),
        "real summary preserved"
    );
    assert_eq!(
        row.log_tail.as_deref(),
        Some("last-200-lines-of-pod-log"),
        "infra-death log_tail captured as diagnostic"
    );
    assert!(
        json_eq(
            row.summary_json.as_deref().unwrap(),
            r#"{"infra_death": "OOMKilled", "capture": "ok"}"#
        ),
        "infra-death metadata captured"
    );
    // Still exactly one row.
    let attempts = repo.list_for_task(&task_id).await.unwrap();
    assert_eq!(attempts.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idempotent_same_outcome_terminal_is_noop() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-idem-same",
            session_id: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();

    // First terminal call.
    repo.advance_to_terminal(TerminalTaskAttemptParams {
        id: &attempt.id,
        outcome: TaskAttemptOutcome::Crashed,
        pr_url: None,
        submit_ref: None,
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary: Some("first summary"),
        summary_json: Some(r#"{"first": true}"#),
        log_tail: Some("first-log"),
    })
    .await
    .unwrap();

    // Second call with the same outcome — idempotent.
    let second = repo
        .advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: TaskAttemptOutcome::Crashed,
            pr_url: Some("http://pr/late"),
            submit_ref: Some("late-submit"),
            checkpoint_ref: Some("late-cp"),
            mirror_head_sha: Some("late-mirror"),
            github_head_sha: Some("late-github"),
            summary: Some("second summary"),
            summary_json: Some(r#"{"second": true}"#),
            log_tail: Some("second-log"),
        })
        .await
        .unwrap();

    // All COALESCE fields preserve the first value.
    assert_eq!(second.outcome, "crashed");
    assert_eq!(second.summary.as_deref(), Some("first summary"));
    assert_eq!(second.log_tail.as_deref(), Some("first-log"));
    assert!(
        json_eq(
            second.summary_json.as_deref().unwrap(),
            r#"{"first": true}"#
        ),
        "first summary_json preserved"
    );
    // Fields that were null are filled.
    assert_eq!(second.pr_url.as_deref(), Some("http://pr/late"));
    assert_eq!(second.submit_ref.as_deref(), Some("late-submit"));
    assert_eq!(second.checkpoint_ref.as_deref(), Some("late-cp"));
    assert_eq!(second.mirror_head_sha.as_deref(), Some("late-mirror"));
    assert_eq!(second.github_head_sha.as_deref(), Some("late-github"));
}

// ── AC3b: persist_infra_death_log_tail (production API) ────────────────────
//
// These tests exercise the actual `persist_infra_death_log_tail` repository
// method used by the supervisor path, rather than the lower-level
// `advance_to_terminal` / `fill_nullable_fields` helpers.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persist_infra_death_preserves_first_log_tail() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-persist-lt-1",
            session_id: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();

    // First persist: sets log_tail.
    let meta1 = r#"{"infra_death_log_tail":{"fetched":true,"line_count":42}}"#;
    let first = repo
        .persist_infra_death_log_tail(&attempt.id, Some("first captured log lines"), meta1)
        .await
        .unwrap();
    assert_eq!(
        first.log_tail.as_deref(),
        Some("first captured log lines"),
        "first persist must set log_tail"
    );

    // Second persist with a different log_tail: must NOT overwrite.
    let meta2 = r#"{"infra_death_log_tail":{"fetched":false,"fetch_error_class":"timeout"}}"#;
    let second = repo
        .persist_infra_death_log_tail(
            &attempt.id,
            Some("second log tail should not overwrite"),
            meta2,
        )
        .await
        .unwrap();
    assert_eq!(
        second.log_tail.as_deref(),
        Some("first captured log lines"),
        "persist_infra_death_log_tail must preserve first non-null log_tail (COALESCE)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persist_infra_death_null_then_nonnull_log_tail() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-persist-null-lt",
            session_id: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();

    // First persist with null log_tail (fetch failed).
    let meta1 = r#"{"infra_death_log_tail":{"fetched":false,"fetch_error_class":"no_pod_found"}}"#;
    let first = repo
        .persist_infra_death_log_tail(&attempt.id, None, meta1)
        .await
        .unwrap();
    assert!(
        first.log_tail.is_none(),
        "first persist with null log_tail leaves it null"
    );

    // Second persist with non-null log_tail: COALESCE fills the null.
    let meta2 = r#"{"infra_death_log_tail":{"fetched":true,"line_count":100}}"#;
    let second = repo
        .persist_infra_death_log_tail(&attempt.id, Some("captured on retry"), meta2)
        .await
        .unwrap();
    assert_eq!(
        second.log_tail.as_deref(),
        Some("captured on retry"),
        "COALESCE fills null log_tail with the first non-null value"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persist_infra_death_merges_fetch_metadata_on_first_call() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-persist-meta-1",
            session_id: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();

    // summary_json is initially NULL; persist should merge the infra_death_log_tail object.
    let meta =
        r#"{"infra_death_log_tail":{"fetched":true,"line_count":42,"death_reason":"OOMKilled"}}"#;
    let updated = repo
        .persist_infra_death_log_tail(&attempt.id, Some("tail"), meta)
        .await
        .unwrap();

    let sj = updated
        .summary_json
        .as_deref()
        .expect("summary_json must be set");
    let parsed: serde_json::Value = serde_json::from_str(sj).unwrap();
    let idlt = parsed
        .get("infra_death_log_tail")
        .expect("infra_death_log_tail key must be present");
    assert_eq!(idlt["fetched"], serde_json::Value::Bool(true));
    assert_eq!(idlt["line_count"], serde_json::json!(42));
    assert_eq!(idlt["death_reason"], serde_json::json!("OOMKilled"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persist_infra_death_merges_into_existing_summary_json() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-persist-merge",
            session_id: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();

    // Pre-populate summary_json with some existing key.
    repo.fill_nullable_fields(FillTaskAttemptParams {
        id: &attempt.id,
        summary_json: Some(r#"{"existing_key":"existing_value"}"#),
        ..Default::default()
    })
    .await
    .unwrap();

    // Now persist infra-death log tail: should merge the new key into existing JSON.
    let meta = r#"{"infra_death_log_tail":{"fetched":true,"line_count":100}}"#;
    let updated = repo
        .persist_infra_death_log_tail(&attempt.id, Some("tail content"), meta)
        .await
        .unwrap();

    let sj = updated.summary_json.as_deref().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(sj).unwrap();
    assert!(
        parsed.get("existing_key").is_some(),
        "existing_key must be preserved after merge"
    );
    assert!(
        parsed.get("infra_death_log_tail").is_some(),
        "infra_death_log_tail key must be merged in"
    );
    assert_eq!(updated.log_tail.as_deref(), Some("tail content"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persist_infra_death_no_overwrite_existing_metadata() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-persist-no-ow",
            session_id: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();

    // First persist: sets both log_tail and summary_json.
    let meta1 = r#"{"infra_death_log_tail":{"fetched":true,"line_count":42}}"#;
    let first = repo
        .persist_infra_death_log_tail(&attempt.id, Some("first tail"), meta1)
        .await
        .unwrap();
    let first_sj = first.summary_json.clone();

    // Second persist with different metadata: summary_json should be
    // preserved because the infra_death_log_tail key already exists.
    let meta2 = r#"{"infra_death_log_tail":{"fetched":false,"fetch_error_class":"timeout"}}"#;
    let second = repo
        .persist_infra_death_log_tail(&attempt.id, Some("second tail"), meta2)
        .await
        .unwrap();

    assert_eq!(
        second.log_tail.as_deref(),
        Some("first tail"),
        "log_tail preserved from first call"
    );
    assert_eq!(
        second.summary_json, first_sj,
        "summary_json preserved when infra_death_log_tail key already exists"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persist_infra_death_no_duplicate_rows() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let _attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-persist-no-dup",
            session_id: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();

    // Simulate multiple concurrent persist_infra_death_log_tail calls.
    for i in 0..3 {
        let meta = format!(r#"{{"infra_death_log_tail":{{"fetched":true,"attempt":{i}}}}}"#);
        repo.persist_infra_death_log_tail(&id, Some(&format!("log-tail-attempt-{i}")), &meta)
            .await
            .unwrap();
    }

    // Still exactly one row.
    let attempts = repo.list_for_task(&task_id).await.unwrap();
    assert_eq!(
        attempts.len(),
        1,
        "repeated persist_infra_death_log_tail must not create duplicate rows"
    );

    // The first call's log_tail is preserved.
    let row = &attempts[0];
    assert_eq!(
        row.log_tail.as_deref(),
        Some("log-tail-attempt-0"),
        "first non-null log_tail wins across repeated persist calls"
    );

    // summary_json was set by the first call and not overwritten.
    let sj = row.summary_json.as_deref().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(sj).unwrap();
    let idlt = parsed.get("infra_death_log_tail").unwrap();
    assert_eq!(
        idlt["attempt"],
        serde_json::json!(0),
        "first call's metadata preserved"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persist_infra_death_does_not_change_outcome_on_terminal_row() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-persist-term",
            session_id: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();

    // Terminalize the attempt as completed (real terminal report).
    repo.advance_to_terminal(TerminalTaskAttemptParams {
        id: &attempt.id,
        outcome: TaskAttemptOutcome::Completed,
        pr_url: Some("http://example.com/pr/1"),
        submit_ref: None,
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary: Some("task completed successfully"),
        summary_json: None,
        log_tail: None,
    })
    .await
    .unwrap();

    // Now persist infra-death log tail on the terminal row.
    let meta = r#"{"infra_death_log_tail":{"fetched":true,"death_reason":"OOMKilled"}}"#;
    let updated = repo
        .persist_infra_death_log_tail(&attempt.id, Some("oom-crash-log"), meta)
        .await
        .unwrap();

    // Outcome must NOT change — persist_infra_death_log_tail does not touch outcome.
    assert_eq!(
        updated.outcome, "completed",
        "persist_infra_death_log_tail must not change the outcome"
    );
    // log_tail captured as diagnostic data.
    assert_eq!(
        updated.log_tail.as_deref(),
        Some("oom-crash-log"),
        "log_tail captured on terminal row"
    );
    // summary_json merged in.
    let sj = updated.summary_json.as_deref().unwrap();
    assert!(
        sj.contains("infra_death_log_tail"),
        "infra_death_log_tail metadata merged into summary_json"
    );
    // Real summary preserved.
    assert_eq!(
        updated.summary.as_deref(),
        Some("task completed successfully"),
        "real terminal summary preserved"
    );
    // Real pr_url preserved.
    assert_eq!(
        updated.pr_url.as_deref(),
        Some("http://example.com/pr/1"),
        "real pr_url preserved"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persist_infra_death_on_pending_row_then_terminal_wins() {
    let db = test_db();
    let (_pid, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db);

    let id = new_attempt_id();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-persist-then-term",
            session_id: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();

    // Infra-death capture persists on pending row.
    let meta =
        r#"{"infra_death_log_tail":{"fetched":true,"line_count":200,"death_reason":"evicted"}}"#;
    let after_persist = repo
        .persist_infra_death_log_tail(&attempt.id, Some("pod-log-before-eviction"), meta)
        .await
        .unwrap();
    assert_eq!(after_persist.outcome, "pending");
    assert_eq!(
        after_persist.log_tail.as_deref(),
        Some("pod-log-before-eviction")
    );

    // Real terminal report arrives and terminalizes the row.
    let terminal = repo
        .advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: TaskAttemptOutcome::Crashed,
            pr_url: None,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("worker reported: evicted by node"),
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();

    assert_eq!(terminal.outcome, "crashed");
    assert_eq!(
        terminal.log_tail.as_deref(),
        Some("pod-log-before-eviction"),
        "log_tail from infra-death persist preserved after terminal report"
    );
    // summary from terminal report is set (was null).
    assert_eq!(
        terminal.summary.as_deref(),
        Some("worker reported: evicted by node")
    );
    // infra-death metadata persisted earlier is still present.
    let sj = terminal.summary_json.as_deref().unwrap();
    assert!(
        sj.contains("infra_death_log_tail"),
        "infra-death metadata survives terminal advance"
    );
}
