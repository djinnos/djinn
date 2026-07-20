use super::reply_loop::{ReplyLoopContext, run_reply_loop};
use crate::test_helpers::{
    FailingProvider, FakeProvider, agent_context_from_db, create_test_db, create_test_epic,
    create_test_project, create_test_task, test_path,
};
use djinn_provider::message::{ContentBlock, Conversation, Message, Role};
use djinn_provider::provider::StreamEvent;
use tokio_util::sync::CancellationToken;

fn dummy_tool_schema(name: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": { "name": name, "description": "test", "parameters": {"type": "object"} },
        "concurrent_safe": false
    })
}

fn dummy_tool_schema_with_safety(name: &str, concurrent_safe: bool) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": { "name": name, "description": "test", "parameters": {"type": "object"} },
        "readOnly": concurrent_safe,
        "destructive": false,
        "idempotent": concurrent_safe,
        "openWorld": false,
        "concurrent_safe": concurrent_safe
    })
}

async fn make_context() -> (
    crate::host::SlotContext,
    String,
    String,
    String,
    CancellationToken,
) {
    let cancel = CancellationToken::new();
    let db = create_test_db();
    let ctx = agent_context_from_db(db.clone(), cancel.clone());
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let task_run_id = uuid::Uuid::now_v7().to_string();
    djinn_db::repositories::task_run::TaskRunRepository::new(db.clone())
        .create(djinn_db::repositories::task_run::CreateTaskRunParams {
            id: &task_run_id,
            project_id: &project.id,
            task_id: &task.id,
            trigger_type: "dispatch",
            status: Some("running"),
            workspace_path: Some("/tmp"),
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .expect("create active task run");
    let session = djinn_db::SessionRepository::new(db.clone(), ctx.event_bus.clone())
        .create(djinn_db::repositories::session::CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task.id),
            model: "synthetic/test-model",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(&task_run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("create reply-loop smoke session");
    let project_path = djinn_core::paths::project_dir(&project.github_owner, &project.github_repo)
        .to_string_lossy()
        .into_owned();
    (ctx, project_path, task.id, session.id, cancel)
}

fn base_conversation() -> Conversation {
    let mut conversation = Conversation::new();
    conversation.push(Message::system("You are a worker."));
    conversation.push(Message::user("Do the task."));
    conversation
}

#[allow(clippy::too_many_arguments)]
async fn run_with_provider(
    provider: &dyn djinn_provider::provider::LlmProvider,
    tools: &[serde_json::Value],
    conversation: &mut Conversation,
    slot_ctx: &crate::host::SlotContext,
    project_path: &str,
    task_id: &str,
    session_id: &str,
    cancel: &CancellationToken,
) -> (
    anyhow::Result<()>,
    crate::output_parser::ParsedAgentOutput,
    i64,
    i64,
    i64,
    i64,
) {
    run_with_provider_and_model(
        provider,
        tools,
        conversation,
        slot_ctx,
        project_path,
        task_id,
        session_id,
        cancel,
        "synthetic/test-model",
    )
    .await
}

/// Like [`run_with_provider`] but accepts an explicit `model_id`, allowing
/// tests to exercise Codex-specific (or non-Codex) retry behaviour.
#[allow(clippy::too_many_arguments)]
async fn run_with_provider_and_model(
    provider: &dyn djinn_provider::provider::LlmProvider,
    tools: &[serde_json::Value],
    conversation: &mut Conversation,
    slot_ctx: &crate::host::SlotContext,
    project_path: &str,
    task_id: &str,
    session_id: &str,
    cancel: &CancellationToken,
    model_id: &str,
) -> (
    anyhow::Result<()>,
    crate::output_parser::ParsedAgentOutput,
    i64,
    i64,
    i64,
    i64,
) {
    let worktree = test_path("djinn-reply-loop-");
    let worktree_path = worktree.as_path();
    run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
            provider,
            tools,
            task_id,
            task_short_id: "t1",
            session_id,
            project_path,
            worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window: 10_000,
            model_id,
            cancel,
            global_cancel: cancel,
            ctx: slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        conversation,
        false,
    )
    .await
}

#[tokio::test]
async fn text_only_completion_path_ends_without_nudge_when_no_tools_exist() {
    let provider = FakeProvider::script(vec![vec![
        StreamEvent::Delta(ContentBlock::Text {
            text: "Completed the task.".into(),
        }),
        StreamEvent::Done,
    ]]);
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let mut conversation = base_conversation();
    let (result, output, _, _, _, _) = run_with_provider(
        &provider,
        &[],
        &mut conversation,
        &slot_ctx,
        &project_path,
        &task_id,
        &session_id,
        &cancel,
    )
    .await;
    assert!(
        result.is_ok(),
        "expected text-only completion to succeed: {result:?}"
    );
    assert!(output.finalize_payload.is_none());
    assert_eq!(provider.remaining(), 0);
    assert_eq!(conversation.messages.len(), 3);
    assert!(matches!(
        &conversation.messages[2],
        Message {
            role: Role::Assistant,
            content,
            ..
        } if matches!(content.as_slice(), [ContentBlock::Text { text }] if text == "Completed the task.")
    ));
}

#[tokio::test]
async fn tool_call_execution_adds_tool_result_and_continues_to_next_turn() {
    let tools = vec![dummy_tool_schema("output_view")];
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let provider = FakeProvider::script(vec![
        vec![
            StreamEvent::Delta(ContentBlock::ToolUse {
                id: "tool-1".into(),
                name: "output_view".into(),
                input: serde_json::json!({"tool_use_id": "missing", "limit": 5}),
            }),
            StreamEvent::Done,
        ],
        vec![
            StreamEvent::Delta(ContentBlock::ToolUse {
                id: "fin-1".into(),
                name: "submit_work".into(),
                input: serde_json::json!({
                    "task_id": task_id,
                    "commit_title": "complete tool-call test work",
                    "summary": "finished after tool call"
                }),
            }),
            StreamEvent::Done,
        ],
    ]);
    let mut conversation = base_conversation();
    let (result, _output, _, _, _, _) = run_with_provider(
        &provider,
        &tools,
        &mut conversation,
        &slot_ctx,
        &project_path,
        &task_id,
        &session_id,
        &cancel,
    )
    .await;
    assert!(result.is_ok(), "tool-call path should succeed: {result:?}");
    assert_eq!(
        provider.remaining(),
        0,
        "second provider turn should be consumed"
    );
    assert_eq!(conversation.messages.len(), 5);
    assert!(matches!(
        &conversation.messages[2].content[..],
        [ContentBlock::ToolUse { id, name, .. }] if id == "tool-1" && name == "output_view"
    ));
    assert!(matches!(
        &conversation.messages[3].content[..],
        [ContentBlock::ToolResult { tool_use_id, is_error, .. }] if tool_use_id == "tool-1" && *is_error
    ));
    assert_eq!(_output.finalize_tool_name.as_deref(), Some("submit_work"));
    assert_eq!(
        _output.finalize_payload.as_ref().unwrap()["summary"],
        "finished after tool call"
    );
}

#[tokio::test]
async fn finalize_tool_detection_ends_loop_without_extra_provider_turn() {
    let tools = vec![dummy_tool_schema("submit_work")];
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let provider = FakeProvider::script(vec![vec![
        StreamEvent::Delta(ContentBlock::ToolUse {
            id: "fin-1".into(),
            name: "submit_work".into(),
            input: serde_json::json!({
                "task_id": task_id,
                "commit_title": "complete finalize detection test",
                "summary": "done"
            }),
        }),
        StreamEvent::Done,
    ]]);
    let mut conversation = base_conversation();
    let (result, output, _, _, _, _) = run_with_provider(
        &provider,
        &tools,
        &mut conversation,
        &slot_ctx,
        &project_path,
        &task_id,
        &session_id,
        &cancel,
    )
    .await;
    assert!(
        result.is_ok(),
        "finalize tool turn should succeed: {result:?}"
    );
    assert_eq!(
        provider.remaining(),
        0,
        "reply loop should not request another provider turn"
    );
    assert_eq!(output.finalize_tool_name.as_deref(), Some("submit_work"));
    assert_eq!(output.finalize_payload.as_ref().unwrap()["summary"], "done");
    assert_eq!(
        conversation.messages.len(),
        3,
        "finalize should not append tool-result turn"
    );
}

#[tokio::test]
async fn empty_response_retries_then_injects_nudge_into_second_turn_history() {
    let tools = vec![dummy_tool_schema("submit_work")];
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let provider = FakeProvider::script(vec![
        vec![],
        vec![
            StreamEvent::Delta(ContentBlock::Text {
                text: "I think the work is done.".into(),
            }),
            StreamEvent::Done,
        ],
        vec![
            StreamEvent::Delta(ContentBlock::ToolUse {
                id: "fin-1".into(),
                name: "submit_work".into(),
                input: serde_json::json!({
                    "task_id": task_id,
                    "commit_title": "complete empty-response test",
                    "summary": "done after nudge"
                }),
            }),
            StreamEvent::Done,
        ],
    ]);
    let mut conversation = base_conversation();
    // Use a Codex-family model so that empty-stream retries are allowed
    // (non-Codex models now fail immediately on terminal empty turns).
    let (result, output, _, _, _, _) = run_with_provider_and_model(
        &provider,
        &tools,
        &mut conversation,
        &slot_ctx,
        &project_path,
        &task_id,
        &session_id,
        &cancel,
        "openai/test-model",
    )
    .await;
    assert!(
        result.is_ok(),
        "empty-turn retry + nudge path should succeed: {result:?}"
    );
    assert_eq!(provider.remaining(), 0);
    assert_eq!(output.finalize_tool_name.as_deref(), Some("submit_work"));
    assert!(conversation.messages.iter().any(|message| {
        message.role == Role::User
            && message.content.iter().any(|block| {
                matches!(block, ContentBlock::Text { text } if text.contains("You have not completed your session."))
            })
    }));
}

/// AC3 regression: a stream that emits partial assistant text but ends without
/// `StreamEvent::Done` must NOT be persisted as a complete assistant turn.
///
/// The in-flight flush still preserves observed content for resume, but the
/// reply loop must return a typed provider failure error so the truncated turn
/// is not finalized as a successful assistant message.
#[tokio::test]
async fn truncated_stream_with_partial_text_is_not_persisted_as_complete() {
    let tools = vec![dummy_tool_schema("submit_work")];
    // Provider returns partial text with no StreamEvent::Done — simulates a
    // truncated/early-ended provider stream.
    let provider = FakeProvider::script(vec![vec![
        StreamEvent::Delta(ContentBlock::Text {
            text: "partial assistant output that was cut short".into(),
        }),
        // No StreamEvent::Done — stream ends early.
    ]]);
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let mut conversation = base_conversation();
    let (result, _output, _, _, _, _) = run_with_provider(
        &provider,
        &tools,
        &mut conversation,
        &slot_ctx,
        &project_path,
        &task_id,
        &session_id,
        &cancel,
    )
    .await;
    assert!(
        result.is_err(),
        "truncated stream should produce an error, not succeed: {result:?}"
    );
    let err_msg = format!("{:?}", result.unwrap_err());
    assert!(
        err_msg.contains("ended early") || err_msg.contains("truncated"),
        "error should mention truncated/early stream: {err_msg}"
    );
    // The conversation must NOT contain a finalized assistant message with the
    // partial text.  Only the original system + user messages should remain.
    let has_assistant_msg = conversation
        .messages
        .iter()
        .any(|m| m.role == Role::Assistant);
    assert!(
        !has_assistant_msg,
        "truncated assistant output must not be finalized as a complete assistant message; \
         conversation messages: {:?}",
        conversation.messages.len()
    );
}

/// Regression: a non-Codex provider that returns an empty stream (no events)
/// must produce a typed provider failure on the very first occurrence — no
/// retries, no nudge path.
#[tokio::test]
async fn non_codex_empty_stream_fails_immediately_on_first_occurrence() {
    use djinn_provider::provider::ProviderError;

    let provider = FakeProvider::script(vec![vec![]]);
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let mut conversation = base_conversation();
    // Non-Codex model → immediate terminal failure.
    let (result, _output, _, _, _, _) = run_with_provider_and_model(
        &provider,
        &[],
        &mut conversation,
        &slot_ctx,
        &project_path,
        &task_id,
        &session_id,
        &cancel,
        "synthetic/kimi-k2.5",
    )
    .await;
    let err = result.expect_err("non-Codex empty stream must produce a terminal error");
    // Non-Codex providers get a transient ProviderInternal(500).
    assert!(
        err.downcast_ref::<ProviderError>().is_some(),
        "error must carry a typed ProviderError for failover classification: {err}"
    );
    assert!(
        err.to_string().contains("empty"),
        "error must mention empty for diagnostics: {err}"
    );
}

#[tokio::test]
async fn max_nudge_abort_returns_clean_error_path() {
    let tools = vec![dummy_tool_schema("submit_work")];
    let provider = FakeProvider::script(vec![
        vec![
            StreamEvent::Delta(ContentBlock::Text { text: "one".into() }),
            StreamEvent::Done,
        ],
        vec![
            StreamEvent::Delta(ContentBlock::Text { text: "two".into() }),
            StreamEvent::Done,
        ],
        vec![
            StreamEvent::Delta(ContentBlock::Text {
                text: "three".into(),
            }),
            StreamEvent::Done,
        ],
    ]);
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let mut conversation = base_conversation();
    let (result, _output, _, _, _, _) = run_with_provider(
        &provider,
        &tools,
        &mut conversation,
        &slot_ctx,
        &project_path,
        &task_id,
        &session_id,
        &cancel,
    )
    .await;
    let error = result.expect_err("expected clean nudge exhaustion error");
    assert!(
        error
            .to_string()
            .contains("consecutive text-only responses")
    );
    assert_eq!(provider.remaining(), 0);
}

#[tokio::test]
async fn provider_error_propagates_from_shared_failing_provider() {
    let provider = FailingProvider::new("scripted provider failure for reply loop");
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let mut conversation = base_conversation();
    let (result, _output, _, _, _, _) = run_with_provider(
        &provider,
        &[],
        &mut conversation,
        &slot_ctx,
        &project_path,
        &task_id,
        &session_id,
        &cancel,
    )
    .await;
    let error = result.expect_err("provider failure should propagate");
    assert!(
        error
            .to_string()
            .contains("scripted provider failure for reply loop")
    );
}

#[tokio::test]
async fn metadata_drives_streaming_dispatch_for_safe_tools() {
    let tools = vec![
        dummy_tool_schema_with_safety("output_view", true),
        dummy_tool_schema_with_safety("submit_work", false),
    ];
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let provider = FakeProvider::script(vec![
        vec![
            StreamEvent::Delta(ContentBlock::ToolUse {
                id: "tool-1".into(),
                name: "output_view".into(),
                input: serde_json::json!({"tool_use_id": "missing", "limit": 5}),
            }),
            StreamEvent::Done,
        ],
        vec![
            StreamEvent::Delta(ContentBlock::ToolUse {
                id: "fin-1".into(),
                name: "submit_work".into(),
                input: serde_json::json!({
                    "task_id": task_id,
                    "commit_title": "complete reply-loop fixture",
                    "summary": "done"
                }),
            }),
            StreamEvent::Done,
        ],
    ]);
    let mut conversation = base_conversation();
    let (result, output, _, _, _, _) = run_with_provider(
        &provider,
        &tools,
        &mut conversation,
        &slot_ctx,
        &project_path,
        &task_id,
        &session_id,
        &cancel,
    )
    .await;
    assert!(
        result.is_ok(),
        "metadata-driven dispatch should succeed: {result:?}"
    );
    assert_eq!(output.finalize_tool_name.as_deref(), Some("submit_work"));
    assert!(matches!(
        &conversation.messages[3].content[..],
        [ContentBlock::ToolResult { tool_use_id, .. }] if tool_use_id == "tool-1"
    ));
}

#[tokio::test]
async fn missing_metadata_defaults_to_unsafe_dispatch() {
    let tools = vec![
        serde_json::json!({
            "type": "function",
            "function": { "name": "output_view", "description": "test", "parameters": {"type": "object"} }
        }),
        dummy_tool_schema("submit_work"),
    ];
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let provider = FakeProvider::script(vec![
        vec![
            StreamEvent::Delta(ContentBlock::ToolUse {
                id: "tool-1".into(),
                name: "output_view".into(),
                input: serde_json::json!({"tool_use_id": "missing", "limit": 5}),
            }),
            StreamEvent::Done,
        ],
        vec![
            StreamEvent::Delta(ContentBlock::ToolUse {
                id: "fin-1".into(),
                name: "submit_work".into(),
                input: serde_json::json!({
                    "task_id": task_id,
                    "commit_title": "complete reply-loop fixture",
                    "summary": "done"
                }),
            }),
            StreamEvent::Done,
        ],
    ]);
    let mut conversation = base_conversation();
    let (result, output, _, _, _, _) = run_with_provider(
        &provider,
        &tools,
        &mut conversation,
        &slot_ctx,
        &project_path,
        &task_id,
        &session_id,
        &cancel,
    )
    .await;
    assert!(
        result.is_ok(),
        "default-unsafe dispatch should succeed: {result:?}"
    );
    assert_eq!(output.finalize_tool_name.as_deref(), Some("submit_work"));
    assert!(matches!(
        &conversation.messages[2].content[..],
        [ContentBlock::ToolUse { id, name, .. }] if id == "tool-1" && name == "output_view"
    ));
    assert!(matches!(
        &conversation.messages[3].content[..],
        [ContentBlock::ToolResult { tool_use_id, is_error, .. }] if tool_use_id == "tool-1" && *is_error
    ));
}

#[tokio::test]
async fn side_query_tools_share_normal_tool_result_turn_and_keep_order() {
    let tools = vec![
        dummy_tool_schema_with_safety("output_view", true),
        dummy_tool_schema_with_safety("shell", false),
        dummy_tool_schema("submit_work"),
    ];
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let provider = FakeProvider::script(vec![
        vec![
            StreamEvent::Delta(ContentBlock::Text {
                text: "Checking context before acting.".into(),
            }),
            StreamEvent::Delta(ContentBlock::ToolUse {
                id: "tool-1".into(),
                name: "output_view".into(),
                input: serde_json::json!({"tool_use_id": "missing", "limit": 5}),
            }),
            StreamEvent::Delta(ContentBlock::ToolUse {
                id: "tool-2".into(),
                name: "shell".into(),
                input: serde_json::json!({"command": "printf unsafe-tool"}),
            }),
            StreamEvent::Done,
        ],
        vec![
            StreamEvent::Delta(ContentBlock::ToolUse {
                id: "fin-1".into(),
                name: "submit_work".into(),
                input: serde_json::json!({
                    "task_id": task_id,
                    "commit_title": "complete reply-loop fixture",
                    "summary": "done"
                }),
            }),
            StreamEvent::Done,
        ],
    ]);
    let mut conversation = base_conversation();
    let (result, output, _, _, _, _) = run_with_provider(
        &provider,
        &tools,
        &mut conversation,
        &slot_ctx,
        &project_path,
        &task_id,
        &session_id,
        &cancel,
    )
    .await;
    assert!(result.is_ok(), "side-query path should succeed: {result:?}");
    assert_eq!(output.finalize_tool_name.as_deref(), Some("submit_work"));
    assert!(matches!(
        &conversation.messages[2],
        Message {
            role: Role::Assistant,
            content,
            ..
        } if matches!(content.as_slice(), [
            ContentBlock::Text { text },
            ContentBlock::ToolUse { id: first_id, name: first_name, .. },
            ContentBlock::ToolUse { id: second_id, name: second_name, .. }
        ] if text == "Checking context before acting." && first_id == "tool-1" && first_name == "output_view" && second_id == "tool-2" && second_name == "shell")
    ));
    assert!(matches!(
        &conversation.messages[3],
        Message {
            role: Role::User,
            content,
            ..
        } if matches!(content.as_slice(), [
            ContentBlock::ToolResult { tool_use_id: first_id, is_error: true, .. },
            ContentBlock::ToolResult { tool_use_id: second_id, is_error: false, .. }
        ] if first_id == "tool-1" && second_id == "tool-2")
    ));
}
