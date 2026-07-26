//! Resubmission and terminal-error behaviour after a non-stored completion
//! verification. Split out of `reply_loop_completion_intent_tests.rs` purely to
//! keep that file under the size guard; it shares the same harness via
//! `use super::*`.

use super::*;

#[tokio::test]
async fn ineligible_result_is_persisted_and_valid_resubmission_is_reverified() {
    let fixture = make_fixture(vec![ineligible("command failed"), stored()]).await;
    let provider = FakeProvider::script(vec![
        submit_turn("submit-failed", &fixture.task_id, "first attempt"),
        submit_turn("submit-stored", &fixture.task_id, "corrected attempt"),
    ]);
    let mut conversation = base_conversation();
    let (result, output, _, _, _, _) = run_with_provider(
        &provider,
        &[dummy_tool_schema("submit_work")],
        &mut conversation,
        &fixture.slot_ctx,
        &fixture.project_path,
        &fixture.task_id,
        &fixture.session_id,
        &fixture.cancel,
    )
    .await;

    assert!(result.is_ok());
    assert_eq!(fixture.callbacks.coordinator_count(), 2);
    assert_eq!(error_ids(&conversation), vec!["submit-failed"]);
    assert_eq!(
        output.finalize_payload.as_ref().unwrap()["summary"],
        "corrected attempt"
    );
    let persisted = djinn_db::SessionMessageRepository::new(
        fixture.slot_ctx.db.clone(),
        fixture.slot_ctx.event_bus.clone(),
    )
    .load_conversation(&fixture.session_id)
    .await
    .expect("load persisted conversation");
    assert_eq!(error_ids(&persisted), vec!["submit-failed"]);
}

#[tokio::test]
async fn terminal_error_exhausts_conversation_without_success_or_submission() {
    let fixture = make_fixture(vec![coordinator_error("persistence unavailable")]).await;
    let provider = FakeProvider::script_with_terminal_error(
        vec![submit_turn("submit-error", &fixture.task_id, "attempt")],
        "terminal provider failure after submit-error",
    );
    let mut conversation = base_conversation();
    let (result, output, _, _, _, _) = run_with_provider(
        &provider,
        &[dummy_tool_schema("submit_work")],
        &mut conversation,
        &fixture.slot_ctx,
        &fixture.project_path,
        &fixture.task_id,
        &fixture.session_id,
        &fixture.cancel,
    )
    .await;

    assert!(
        result.is_err(),
        "explicit provider failure terminates the real reply loop"
    );
    assert_eq!(provider.remaining(), 0, "terminal provider turn consumed");
    assert_eq!(fixture.callbacks.coordinator_count(), 1);
    assert!(output.finalize_payload.is_none());
    assert!(output.completion_intent.is_none());
    assert_eq!(error_ids(&conversation), vec!["submit-error"]);
}

#[tokio::test]
async fn three_non_stored_attempts_each_reach_verification_and_never_succeed() {
    let fixture = make_fixture(vec![
        ineligible("command one failed"),
        coordinator_error("writer failed"),
        ineligible("command three failed"),
    ])
    .await;
    let provider = FakeProvider::script_with_terminal_error(
        vec![
            submit_turn("submit-1", &fixture.task_id, "attempt one"),
            submit_turn("submit-2", &fixture.task_id, "attempt two"),
            submit_turn("submit-3", &fixture.task_id, "attempt three"),
        ],
        "terminal provider failure after three non-stored attempts",
    );
    let mut conversation = base_conversation();
    let (result, output, _, _, _, _) = run_with_provider(
        &provider,
        &[dummy_tool_schema("submit_work")],
        &mut conversation,
        &fixture.slot_ctx,
        &fixture.project_path,
        &fixture.task_id,
        &fixture.session_id,
        &fixture.cancel,
    )
    .await;

    assert!(result.is_err());
    assert_eq!(provider.remaining(), 0, "terminal provider turn consumed");
    assert_eq!(fixture.callbacks.coordinator_count(), 3);
    assert!(output.finalize_payload.is_none());
    assert!(output.completion_intent.is_none());
    assert_eq!(
        error_ids(&conversation),
        vec!["submit-1", "submit-2", "submit-3"]
    );
}
