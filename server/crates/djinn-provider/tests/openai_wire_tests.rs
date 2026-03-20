use futures::TryStreamExt;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use djinn_provider::message::{ContentBlock, Conversation, Message};
use djinn_provider::provider::format::openai::OpenAIProvider;
use djinn_provider::provider::{
    AuthMethod, FormatFamily, LlmProvider, ProviderCapabilities, ProviderConfig, StreamEvent,
};

fn provider_config(base_url: &str) -> ProviderConfig {
    ProviderConfig {
        base_url: base_url.to_string(),
        auth: AuthMethod::BearerToken("test-token".to_string()),
        format_family: FormatFamily::OpenAI,
        model_id: "gpt-4o-mini".to_string(),
        context_window: 128_000,
        telemetry: None,
        session_affinity_key: None,
        provider_headers: Default::default(),
        capabilities: ProviderCapabilities::default(),
    }
}

fn one_turn_user_conversation() -> Conversation {
    let mut conversation = Conversation::new();
    conversation.push(Message::user("Hello"));
    conversation
}

fn sse_line_from_json(payload: &serde_json::Value) -> String {
    format!(
        "data: {}\n\n",
        serde_json::to_string(payload).expect("json payload")
    )
}

fn raw_data_line(payload: &str) -> String {
    format!("data: {}\n\n", payload)
}

fn done_sse_line() -> String {
    "data: [DONE]\n\n".to_string()
}

async fn mount_chat_completion_mock(status: u16, body: String) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(status)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    server
}

async fn collect_events(
    provider: &OpenAIProvider,
    conversation: &Conversation,
) -> anyhow::Result<Vec<StreamEvent>> {
    let stream = provider.stream(conversation, &[], None).await?;
    stream.try_collect().await
}

#[tokio::test]
async fn test_stream_emits_text_and_usage_events() {
    let body = [
        sse_line_from_json(&json!({
            "choices": [{
                "delta": {"content": "hello"},
                "index": 0,
                "finish_reason": null,
            }],
        })),
        sse_line_from_json(&json!({
            "choices": [{
                "delta": {"content": " world"},
                "index": 0,
                "finish_reason": null,
            }],
        })),
        sse_line_from_json(&json!({
            "usage": {"prompt_tokens": 13, "completion_tokens": 42}
        })),
    ]
    .join("")
        + &done_sse_line();

    let server = mount_chat_completion_mock(200, body).await;
    let provider = OpenAIProvider::new(provider_config(&server.uri()));
    let conversation = one_turn_user_conversation();

    let events = collect_events(&provider, &conversation)
        .await
        .expect("stream should complete");

    assert_eq!(events.len(), 4);
    match &events[0] {
        StreamEvent::Delta(ContentBlock::Text { text }) => assert_eq!(text, "hello"),
        other => panic!("expected first text delta, got {other:?}"),
    }
    match &events[1] {
        StreamEvent::Delta(ContentBlock::Text { text }) => assert_eq!(text, " world"),
        other => panic!("expected second text delta, got {other:?}"),
    }
    match &events[2] {
        StreamEvent::Usage(usage) => {
            assert_eq!(usage.input, 13);
            assert_eq!(usage.output, 42);
        }
        other => panic!("expected usage event, got {other:?}"),
    }
    assert!(matches!(events[3], StreamEvent::Done));

    let requests = server
        .received_requests()
        .await
        .expect("received requests should be available");
    assert_eq!(requests.len(), 1);
}

#[tokio::test]
async fn test_stream_tool_call_argument_accumulation_from_split_chunks() {
    let mut args_start = String::from("{\"name\":\"");
    args_start.push_str("");
    let mut args_end = String::from("\"");
    args_end.push('}');

    let mut args_full = args_start.clone();
    args_full.push_str("alice");
    args_full.push_str(&args_end);
    let expected_args: serde_json::Value =
        serde_json::from_str(&args_full).expect("tool call args should be valid JSON");

    let body = [
        sse_line_from_json(&json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_001",
                        "function": {
                            "name": "lookup",
                            "arguments": args_start,
                        },
                    }]
                },
                "finish_reason": null,
                "index": 0,
            }],
        })),
        sse_line_from_json(&json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": {
                            "arguments": "alice",
                        },
                    }]
                },
                "finish_reason": null,
                "index": 0,
            }],
        })),
        sse_line_from_json(&json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": {
                            "arguments": args_end,
                        },
                    }]
                },
                "finish_reason": null,
                "index": 0,
            }],
        })),
        sse_line_from_json(&json!({
            "choices": [{
                "delta": {},
                "finish_reason": "tool_calls",
                "index": 0,
            }],
        })),
    ]
    .join("")
        + &done_sse_line();

    let server = mount_chat_completion_mock(200, body).await;
    let provider = OpenAIProvider::new(provider_config(&server.uri()));
    let conversation = one_turn_user_conversation();

    let events = collect_events(&provider, &conversation)
        .await
        .expect("stream should complete");

    assert_eq!(events.len(), 2);
    match &events[0] {
        StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }) => {
            assert_eq!(id, "call_001");
            assert_eq!(name, "lookup");
            assert_eq!(input, &expected_args);
        }
        other => panic!("expected tool use event, got {other:?}"),
    }
    assert!(matches!(events[1], StreamEvent::Done));

    assert_eq!(
        server
            .received_requests()
            .await
            .expect("received requests should be available")
            .len(),
        1
    );
}

#[tokio::test]
async fn test_stream_finish_reason_tool_calls_emits_tool_use() {
    let body = [
        sse_line_from_json(&json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_weather",
                        "function": {
                            "name": "weather",
                            "arguments": r#"{"city":"Paris"}"#,
                        },
                    }]
                },
                "finish_reason": null,
                "index": 0,
            }],
        })),
        sse_line_from_json(&json!({
            "choices": [{
                "delta": {},
                "finish_reason": "tool_calls",
                "index": 0,
            }],
        })),
    ]
    .join("")
        + &done_sse_line();

    let server = mount_chat_completion_mock(200, body).await;
    let provider = OpenAIProvider::new(provider_config(&server.uri()));
    let conversation = one_turn_user_conversation();

    let events = collect_events(&provider, &conversation)
        .await
        .expect("stream should complete");

    assert_eq!(events.len(), 2);
    match &events[0] {
        StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }) => {
            assert_eq!(id, "call_weather");
            assert_eq!(name, "weather");
            assert_eq!(input, &json!({"city": "Paris"}));
        }
        other => panic!("expected tool use event, got {other:?}"),
    }
    assert!(matches!(events[1], StreamEvent::Done));

    assert_eq!(
        server
            .received_requests()
            .await
            .expect("received requests should be available")
            .len(),
        1
    );
}

#[tokio::test]
async fn test_stream_finish_reason_stop_keeps_text() {
    let body = sse_line_from_json(&json!({
        "choices": [{
            "delta": {"content": "final message"},
            "finish_reason": "stop",
            "index": 0,
        }],
    })) + &done_sse_line();

    let server = mount_chat_completion_mock(200, body).await;
    let provider = OpenAIProvider::new(provider_config(&server.uri()));
    let conversation = one_turn_user_conversation();

    let events = collect_events(&provider, &conversation)
        .await
        .expect("stream should complete");

    assert_eq!(events.len(), 2);
    match &events[0] {
        StreamEvent::Delta(ContentBlock::Text { text }) => assert_eq!(text, "final message"),
        other => panic!("expected final text delta, got {other:?}"),
    }
    assert!(matches!(events[1], StreamEvent::Done));

    let requests = server
        .received_requests()
        .await
        .expect("received requests should be available");
    assert_eq!(requests.len(), 1);
}

#[tokio::test]
async fn test_stream_usage_only_chunk_emits_usage_event() {
    let body = sse_line_from_json(&json!({
        "usage": {
            "prompt_tokens": 7,
            "completion_tokens": 0,
        },
    })) + &done_sse_line();

    let server = mount_chat_completion_mock(200, body).await;
    let provider = OpenAIProvider::new(provider_config(&server.uri()));
    let conversation = one_turn_user_conversation();

    let events = collect_events(&provider, &conversation)
        .await
        .expect("stream should complete");

    assert_eq!(events.len(), 2);
    match &events[0] {
        StreamEvent::Usage(usage) => {
            assert_eq!(usage.input, 7);
            assert_eq!(usage.output, 0);
        }
        other => panic!("expected usage event, got {other:?}"),
    }
    assert!(matches!(events[1], StreamEvent::Done));
}

#[tokio::test]
async fn test_stream_ignores_malformed_sse_lines_and_continues() {
    let body = [
        sse_line_from_json(&json!({
            "choices": [{
                "delta": {"content": "ok"},
                "finish_reason": null,
                "index": 0,
            }],
        })),
        "data: {not-json\n\n".to_string(),
        "event: ping\n\n".to_string(),
        raw_data_line(
            r#"{"choices":[{"delta":{"content":" recovered"},"finish_reason":null,"index":0}]}"#,
        ),
    ]
    .join("")
        + &done_sse_line();

    let server = mount_chat_completion_mock(200, body).await;
    let provider = OpenAIProvider::new(provider_config(&server.uri()));
    let conversation = one_turn_user_conversation();

    let events = collect_events(&provider, &conversation)
        .await
        .expect("stream should complete");

    assert_eq!(events.len(), 3);
    match &events[0] {
        StreamEvent::Delta(ContentBlock::Text { text }) => assert_eq!(text, "ok"),
        other => panic!("expected first text delta, got {other:?}"),
    }
    match &events[1] {
        StreamEvent::Delta(ContentBlock::Text { text }) => assert_eq!(text, " recovered"),
        other => panic!("expected recovered text delta, got {other:?}"),
    }
    assert!(matches!(events[2], StreamEvent::Done));

    let requests = server
        .received_requests()
        .await
        .expect("received requests should be available");
    assert_eq!(requests[0].url.path(), "/chat/completions");
    assert_eq!(requests.len(), 1);
}

#[tokio::test]
async fn test_stream_http_error_includes_status_and_body() {
    let server = mount_chat_completion_mock(401, "unauthorized".to_string()).await;
    let provider = OpenAIProvider::new(provider_config(&server.uri()));
    let conversation = one_turn_user_conversation();

    let stream = provider.stream(&conversation, &[], None).await.unwrap();
    let err = stream
        .try_collect::<Vec<_>>()
        .await
        .expect_err("expected stream error");

    let msg = err.to_string();
    assert!(msg.contains("provider API error 401"));
    assert!(msg.contains("unauthorized"));
}

#[tokio::test]
async fn test_stream_targets_local_mock_without_real_network_calls() {
    let body = sse_line_from_json(&json!({
        "choices": [{
            "delta": {"content": "ok"},
            "index": 0,
            "finish_reason": null,
        }],
    })) + &done_sse_line();

    let server = mount_chat_completion_mock(200, body).await;
    let provider = OpenAIProvider::new(provider_config(&server.uri()));
    let conversation = one_turn_user_conversation();

    let events = collect_events(&provider, &conversation)
        .await
        .expect("stream should complete");

    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], StreamEvent::Delta(ContentBlock::Text { text }) if text == "ok"));

    let requests = server
        .received_requests()
        .await
        .expect("received requests should be available");
    assert_eq!(requests.len(), 1);
    let req = &requests[0];
    assert_eq!(req.url.path(), "/chat/completions");
    assert_eq!(req.url.host_str(), Some("localhost"));
    let authorization = req
        .headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .expect("authorization header");
    assert_eq!(authorization, "Bearer test-token");
}
