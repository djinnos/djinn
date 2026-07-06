use futures::TryStreamExt;

use djinn_provider::message::{ContentBlock, Conversation, Message};
use djinn_provider::provider::format::openai::OpenAIProvider;
use djinn_provider::provider::{
    AuthMethod, FormatFamily, LlmProvider, ProviderCapabilities, ProviderConfig, ProviderError,
    StreamEvent,
};

fn test_openai_config() -> ProviderConfig {
    ProviderConfig {
        base_url: "https://api.openai.com".to_string(),
        auth: AuthMethod::BearerToken("test".to_string()),
        format_family: FormatFamily::OpenAI,
        model_id: "gpt-4o-mini".to_string(),
        context_window: 128_000,
        telemetry: None,
        session_affinity_key: None,
        provider_headers: std::collections::HashMap::new(),
        capabilities: ProviderCapabilities::default(),
        reasoning_effort: None,
        tool_schema_compat: None,
    }
}

/// Spawn a local axum server that serves a static SSE body.
/// Returns the base URL (e.g. `http://127.0.0.1:PORT`).
fn spawn_sse_server(body: &'static str) -> String {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind local tcp listener");
    let addr = listener.local_addr().expect("local addr");
    listener.set_nonblocking(true).expect("set nonblocking");

    let app = axum::Router::new().route(
        "/chat/completions",
        axum::routing::post(move |_req: axum::extract::Request| async move {
            (
                axum::http::StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                body,
            )
        }),
    );

    let tokio_listener =
        tokio::net::TcpListener::from_std(listener).expect("convert to tokio listener");
    tokio::spawn(async move {
        axum::serve(tokio_listener, app).await.ok();
    });

    format!("http://{}:{}", addr.ip(), addr.port())
}

#[tokio::test]
async fn test_stream_done_propagation() {
    // A stream that emits a text delta followed by [DONE] should yield
    // the text delta and StreamEvent::Done (in that order).
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
        "data: [DONE]\n\n"
    );
    let base_url = spawn_sse_server(body);
    let provider = OpenAIProvider::new(ProviderConfig {
        base_url,
        ..test_openai_config()
    });
    let mut conv = Conversation::new();
    conv.push(Message::user("Hello"));

    let stream = provider
        .stream(&conv, &[], None)
        .await
        .expect("stream start");
    let events: Vec<_> = stream.try_collect().await.expect("stream events");

    assert!(
        events.len() >= 2,
        "expected at least 2 events, got {}",
        events.len()
    );
    assert!(
        matches!(
            &events[0],
            StreamEvent::Delta(ContentBlock::Text { text }) if text == "hi"
        ),
        "first event should be text delta, got {:?}",
        &events[0]
    );
    assert!(
        matches!(events.last().unwrap(), StreamEvent::Done),
        "last event should be Done, got {:?}",
        events.last().unwrap()
    );
}

#[tokio::test]
async fn test_stream_raw_eof_before_done_yields_error() {
    // A stream that emits a text delta but ends (raw EOF) before
    // `data: [DONE]` must yield a typed retryable Transport error,
    // not a synthesized StreamEvent::Done.
    let body: &'static str =
        "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n";
    let base_url = spawn_sse_server(body);
    let provider = OpenAIProvider::new(ProviderConfig {
        base_url,
        ..test_openai_config()
    });
    let mut conv = Conversation::new();
    conv.push(Message::user("Hello"));

    let stream = provider
        .stream(&conv, &[], None)
        .await
        .expect("stream start");
    let err = stream
        .try_collect::<Vec<_>>()
        .await
        .expect_err("raw EOF before [DONE] must yield Err");

    let pe = err
        .downcast_ref::<ProviderError>()
        .expect("typed ProviderError must be downcastable");
    assert_eq!(*pe, ProviderError::Transport);
    assert!(pe.retryable(), "truncated stream must be retryable");
    assert!(
        err.to_string().contains("[DONE]"),
        "error message must mention [DONE]: {}",
        err
    );
}

#[tokio::test]
async fn test_stream_truncated_tool_accumulator_fails_typed() {
    // A stream that starts accumulating tool call deltas but then sends
    // [DONE] without ever emitting finish_reason:"tool_calls" must fail
    // typed with a Transport error and discard partial tool state.
    let body = concat!(
        // Start a tool call (accumulate index 0 with name + partial args)
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"shell\",\"arguments\":\"{\\\"cmd\\\":\\\"ls\"}}]},\"finish_reason\":null}]}\n\n",
        // Provider sends [DONE] without ever finishing the tool call
        "data: [DONE]\n\n"
    );
    let base_url = spawn_sse_server(body);
    let provider = OpenAIProvider::new(ProviderConfig {
        base_url,
        ..test_openai_config()
    });
    let mut conv = Conversation::new();
    conv.push(Message::user("Hello"));

    let stream = provider
        .stream(&conv, &[], None)
        .await
        .expect("stream start");
    let err = stream
        .try_collect::<Vec<_>>()
        .await
        .expect_err("truncated tool accumulator at [DONE] must yield Err");

    let pe = err
        .downcast_ref::<ProviderError>()
        .expect("typed ProviderError must be downcastable");
    assert_eq!(*pe, ProviderError::Transport);
    assert!(pe.retryable(), "truncated tool acc must be retryable");
    assert!(
        err.to_string().contains("incomplete tool"),
        "error message must mention incomplete tool: {}",
        err
    );
}
