use super::*;

#[rstest]
// close is valid from every non-closed state
#[case("open", TransitionAction::Close, "closed", None)]
#[case("in_progress", TransitionAction::Close, "closed", None)]
#[case("verifying", TransitionAction::Close, "closed", None)]
#[case("needs_task_review", TransitionAction::Close, "closed", None)]
#[case("in_task_review", TransitionAction::Close, "closed", None)]
#[case("approved", TransitionAction::Close, "closed", None)]
#[case("pr_draft", TransitionAction::Close, "closed", None)]
#[case("pr_review", TransitionAction::Close, "closed", None)]
// force_close is valid from every non-closed state (requires a reason)
#[case("open", TransitionAction::ForceClose, "closed", Some("testing"))]
#[case("in_progress", TransitionAction::ForceClose, "closed", Some("testing"))]
// reopen is valid from closed
#[case("closed", TransitionAction::Reopen, "open", Some("needed again"))]
// submit_task_review is valid from in_progress
#[case(
    "in_progress",
    TransitionAction::SubmitTaskReview,
    "needs_task_review",
    None
)]
// task_review_start is valid from needs_task_review
#[case(
    "needs_task_review",
    TransitionAction::TaskReviewStart,
    "in_task_review",
    None
)]
// task_review_approve moves to approved
#[case(
    "in_task_review",
    TransitionAction::TaskReviewApprove,
    "approved",
    None
)]
// task_review_reject returns to open (requires reason)
#[case(
    "in_task_review",
    TransitionAction::TaskReviewReject,
    "open",
    Some("needs more work")
)]
// release returns in_progress to open (requires reason)
#[case(
    "in_progress",
    TransitionAction::Release,
    "open",
    Some("releasing slot")
)]
// release_task_review returns in_task_review to needs_task_review (requires reason)
#[case(
    "in_task_review",
    TransitionAction::ReleaseTaskReview,
    "needs_task_review",
    Some("releasing review")
)]
// pr_created transitions approved → pr_draft
#[case("approved", TransitionAction::PrCreated, "pr_draft", None)]
// pr_undraft transitions pr_draft → pr_review
#[case("pr_draft", TransitionAction::PrUndraft, "pr_review", None)]
// pr_ci_failed transitions pr_draft → open
#[case("pr_draft", TransitionAction::PrCiFailed, "open", None)]
// pr_conflict transitions approved → open
#[case("approved", TransitionAction::PrConflict, "open", None)]
// pr_conflict transitions pr_draft → open
#[case("pr_draft", TransitionAction::PrConflict, "open", None)]
// pr_conflict transitions pr_review → open
#[case("pr_review", TransitionAction::PrConflict, "open", None)]
// pr_merge transitions pr_review → closed
#[case("pr_review", TransitionAction::PrMerge, "closed", None)]
// pr_changes_requested transitions pr_review → open (requires reason)
#[case(
    "pr_review",
    TransitionAction::PrChangesRequested,
    "open",
    Some("changes requested by reviewer")
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_transition(
    #[case] from_status: &str,
    #[case] action: TransitionAction,
    #[case] expected_to: &str,
    #[case] reason: Option<&str>,
) {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    // Create with AC so Start works if needed; set_status bypasses AC guard.
    let task = repo
        .create(&epic.id, "T", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    repo.update(
        &task.id,
        "T",
        "",
        "",
        0,
        "",
        "",
        r#"[{"description":"default","met":false}]"#,
    )
    .await
    .unwrap();

    // Put the task in the requested starting state.
    repo.set_status(&task.id, from_status).await.unwrap();

    let result = repo
        .transition(&task.id, action, "", "system", reason, None)
        .await
        .expect("expected valid transition to succeed");

    assert_eq!(
        result.status, expected_to,
        "transition produced unexpected status"
    );
}

// ── rstest parametrized: invalid state machine transitions ────────────────
//
// Each case is a (from_status, action) pair that must be rejected with
// InvalidTransition.  A reason is always supplied to avoid the reason-required
// guard masking the state-machine error.

#[rstest]
#[case("open", TransitionAction::SubmitVerification)]
#[case("open", TransitionAction::SubmitTaskReview)]
#[case("open", TransitionAction::TaskReviewStart)]
#[case("open", TransitionAction::TaskReviewApprove)]
#[case("open", TransitionAction::TaskReviewReject)]
#[case("open", TransitionAction::Reopen)]
#[case("open", TransitionAction::Release)]
#[case("in_progress", TransitionAction::TaskReviewStart)]
#[case("in_progress", TransitionAction::TaskReviewApprove)]
#[case("in_progress", TransitionAction::Reopen)]
#[case("needs_task_review", TransitionAction::TaskReviewApprove)]
#[case("needs_task_review", TransitionAction::TaskReviewReject)]
#[case("needs_task_review", TransitionAction::Reopen)]
#[case("in_task_review", TransitionAction::SubmitTaskReview)]
#[case("closed", TransitionAction::Close)]
#[case("closed", TransitionAction::ForceClose)]
#[case("closed", TransitionAction::TaskReviewApprove)]
// approved invalid transitions
#[case("approved", TransitionAction::Start)]
#[case("approved", TransitionAction::TaskReviewApprove)]
#[case("approved", TransitionAction::PrUndraft)]
// pr_created only from approved
#[case("open", TransitionAction::PrCreated)]
#[case("in_progress", TransitionAction::PrCreated)]
#[case("pr_draft", TransitionAction::PrCreated)]
// pr_undraft only from pr_draft
#[case("open", TransitionAction::PrUndraft)]
#[case("approved", TransitionAction::PrUndraft)]
// pr_ci_failed only from pr_draft
#[case("open", TransitionAction::PrCiFailed)]
#[case("approved", TransitionAction::PrCiFailed)]
#[case("pr_review", TransitionAction::PrCiFailed)]
// pr_conflict only from approved, pr_draft, or pr_review
#[case("open", TransitionAction::PrConflict)]
#[case("in_progress", TransitionAction::PrConflict)]
// pr_merge only from pr_review
#[case("open", TransitionAction::PrMerge)]
#[case("in_task_review", TransitionAction::PrMerge)]
#[case("pr_draft", TransitionAction::PrMerge)]
// pr_changes_requested only from pr_review
#[case("open", TransitionAction::PrChangesRequested)]
#[case("in_task_review", TransitionAction::PrChangesRequested)]
#[case("pr_draft", TransitionAction::PrChangesRequested)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_transition(#[case] from_status: &str, #[case] action: TransitionAction) {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = repo
        .create(&epic.id, "T", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    repo.update(
        &task.id,
        "T",
        "",
        "",
        0,
        "",
        "",
        r#"[{"description":"default","met":false}]"#,
    )
    .await
    .unwrap();

    repo.set_status(&task.id, from_status).await.unwrap();

    // Supply a reason so the requires_reason guard never fires first.
    let err = repo
        .transition(&task.id, action, "", "system", Some("stub-reason"), None)
        .await
        .expect_err("expected InvalidTransition");

    assert!(
        matches!(err, Error::InvalidTransition(_)),
        "expected InvalidTransition, got {err:?}"
    );
}

// ── rstest parametrized: task_count group_by ─────────────────────────────
//
// Verifies that count_grouped returns a { "groups": [...] } object (not an
// error) for each valid group_by value, and that the group key values match
// the kind of field being grouped on.

#[rstest]
#[case("status")]
#[case("priority")]
#[case("issue_type")]
#[case("epic")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_count_group_by(#[case] group_by: &str) {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    // Create a mix of tasks so grouping has something to return.
    repo.create(&epic.id, "A", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    repo.create(&epic.id, "B", "", "", "feature", 1, "", Some("open"))
        .await
        .unwrap();

    let result = repo
        .count_grouped(CountQuery {
            group_by: Some(group_by.to_owned()),
            project_id: None,
            status: None,
            issue_type: None,
            priority: None,
            label: None,
            text: None,
            parent: None,
        })
        .await
        .expect("count_grouped should succeed");

    assert!(
        result["groups"].is_array(),
        "expected {{groups: [...]}} for group_by={group_by}"
    );
    assert!(
        !result["groups"].as_array().unwrap().is_empty(),
        "expected at least one group for group_by={group_by}"
    );
}

// ── rstest parametrized: task_list filters ───────────────────────────────
//
// Verifies that list_filtered returns correctly filtered results for each
// filter dimension independently.

#[rstest]
#[case("status")]
#[case("priority")]
#[case("label")]
#[case("text")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_list_filter(#[case] filter_kind: &str) {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    // Create two tasks; the filter should pick exactly one of them.
    repo.create(&epic.id, "alpha task", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    let beta = repo
        .create(
            &epic.id,
            "beta feature",
            "",
            "",
            "feature",
            1,
            "",
            Some("open"),
        )
        .await
        .unwrap();

    // Apply a label to the second task.
    repo.update(&beta.id, "beta feature", "", "", 1, "", r#"["urgent"]"#, "")
        .await
        .unwrap();

    // Transition beta to in_progress (bypassing AC check via set_status).
    repo.set_status(&beta.id, "in_progress").await.unwrap();

    let query = match filter_kind {
        "status" => ListQuery {
            status: Some("in_progress".to_owned()),
            ..Default::default()
        },
        "priority" => ListQuery {
            priority: Some(1),
            ..Default::default()
        },
        "label" => ListQuery {
            label: Some("urgent".to_owned()),
            ..Default::default()
        },
        "text" => ListQuery {
            text: Some("beta".to_owned()),
            ..Default::default()
        },
        _ => unreachable!(),
    };

    let result = repo
        .list_filtered(query)
        .await
        .expect("list_filtered should succeed");

    assert_eq!(
        result.total_count, 1,
        "filter_kind={filter_kind} should match exactly one task"
    );
    assert_eq!(result.tasks[0].id, beta.id);
}
