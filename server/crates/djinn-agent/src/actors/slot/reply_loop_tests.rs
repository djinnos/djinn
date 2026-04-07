use super::reply_loop::{ReplyLoopContext, run_reply_loop};
use crate::message::{ContentBlock, Conversation, Message, Role};
use crate::provider::StreamEvent;
use crate::test_helpers::{
    FailingProvider, FakeProvider, agent_context_from_db, create_test_db, create_test_epic,
    create_test_project, create_test_task, test_path,
};
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
        "concurrent_safe": concurrent_safe
    })
}

async fn make_context() -> (
    crate::context::AgentContext,
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
    (ctx, project.path, task.id, cancel)
}

fn base_conversation() -> Conversation {
    let mut conversation = Conversation::new();
    conversation.push(Message::system("You are a worker."));
    conversation.push(Message::user("Do the task."));
    conversation
}

async fn run_with_provider(
    provider: &dyn crate::provider::LlmProvider,
    tools: &[serde_json::Value],
    conversation: &mut Conversation,
    app_state: &crate::context::AgentContext,
    project_path: &str,
    task_id: &str,
    cancel: &CancellationToken,
) -> (
    anyhow::Result<()>,
    crate::output_parser::ParsedAgentOutput,
    i64,
    i64,
) {
    let worktree = test_path("djinn-reply-loop-");
    let worktree_path = worktree.as_path();
    run_reply_loop(
        ReplyLoopContext {
            provider,
            tools,
            task_id,
            task_short_id: "t1",
            session_id: "session-1",
            project_path,
            worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_lead"],
            context_window: 10_000,
            model_id: "synthetic/test-model",
            cancel,
            global_cancel: cancel,
            app_state,
            mcp_registry: None,
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
    let (app_state, project_path, task_id, cancel) = make_context().await;
    let mut conversation = base_conversation();

    let (result, output, _, _) = run_with_provider(
        &provider,
        &[],
        &mut conversation,
        &app_state,
        &project_path,
        &task_id,
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
                input: serde_json::json!({"task_id": "t1", "summary": "finished after tool call"}),
            }),
            StreamEvent::Done,
        ],
    ]);
    let (app_state, project_path, task_id, cancel) = make_context().await;
    let mut conversation = base_conversation();

    let (result, _output, _, _) = run_with_provider(
        &provider,
        &tools,
        &mut conversation,
        &app_state,
        &project_path,
        &task_id,
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
    let provider = FakeProvider::script(vec![vec![
        StreamEvent::Delta(ContentBlock::ToolUse {
            id: "fin-1".into(),
            name: "submit_work".into(),
            input: serde_json::json!({"task_id": "t1", "summary": "done"}),
        }),
        StreamEvent::Done,
    ]]);
    let (app_state, project_path, task_id, cancel) = make_context().await;
    let mut conversation = base_conversation();

    let (result, output, _, _) = run_with_provider(
        &provider,
        &tools,
        &mut conversation,
        &app_state,
        &project_path,
        &task_id,
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
                input: serde_json::json!({"task_id": "t1", "summary": "done after nudge"}),
            }),
            StreamEvent::Done,
        ],
    ]);
    let (app_state, project_path, task_id, cancel) = make_context().await;
    let mut conversation = base_conversation();

    let (result, output, _, _) = run_with_provider(
        &provider,
        &tools,
        &mut conversation,
        &app_state,
        &project_path,
        &task_id,
        &cancel,
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
    let (app_state, project_path, task_id, cancel) = make_context().await;
    let mut conversation = base_conversation();

    let (result, _output, _, _) = run_with_provider(
        &provider,
        &tools,
        &mut conversation,
        &app_state,
        &project_path,
        &task_id,
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
    let (app_state, project_path, task_id, cancel) = make_context().await;
    let mut conversation = base_conversation();

    let (result, _output, _, _) = run_with_provider(
        &provider,
        &[],
        &mut conversation,
        &app_state,
        &project_path,
        &task_id,
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
                input: serde_json::json!({"task_id": "t1", "summary": "done"}),
            }),
            StreamEvent::Done,
        ],
    ]);
    let (app_state, project_path, task_id, cancel) = make_context().await;
    let mut conversation = base_conversation();

    let (result, output, _, _) = run_with_provider(
        &provider,
        &tools,
        &mut conversation,
        &app_state,
        &project_path,
        &task_id,
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
                input: serde_json::json!({"task_id": "t1", "summary": "done"}),
            }),
            StreamEvent::Done,
        ],
    ]);
    let (app_state, project_path, task_id, cancel) = make_context().await;
    let mut conversation = base_conversation();

    let (result, output, _, _) = run_with_provider(
        &provider,
        &tools,
        &mut conversation,
        &app_state,
        &project_path,
        &task_id,
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
