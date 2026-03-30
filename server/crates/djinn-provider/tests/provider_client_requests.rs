use djinn_core::message::{CacheBreakpoint, Conversation, Message};
use djinn_provider::provider::client::ApiClient;
use djinn_provider::provider::format::anthropic::AnthropicProvider;
use djinn_provider::provider::format::openai::OpenAIProvider;
use djinn_provider::provider::{
    AuthMethod, FormatFamily, LlmProvider, ProviderCapabilities, ProviderConfig, ToolChoice,
};
use futures::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn openai_config(base_url: String, auth: AuthMethod) -> ProviderConfig {
    ProviderConfig {
        base_url,
        auth,
        format_family: FormatFamily::OpenAI,
        model_id: "gpt-4o-mini".to_string(),
        context_window: 128_000,
        telemetry: None,
        session_affinity_key: None,
        provider_headers: Default::default(),
        capabilities: ProviderCapabilities::default(),
    }
}

fn anthropic_config(base_url: String, auth: AuthMethod) -> ProviderConfig {
    ProviderConfig {
        base_url,
        auth,
        format_family: FormatFamily::Anthropic,
        model_id: "claude-3-5-sonnet".to_string(),
        context_window: 200_000,
        telemetry: None,
        session_affinity_key: None,
        provider_headers: Default::default(),
        capabilities: ProviderCapabilities {
            streaming: true,
            max_tokens_default: Some(8192),
        },
    }
}

fn tool_definition() -> Vec<Value> {
    vec![json!({
        "name": "shell",
        "description": "Run a shell command",
        "inputSchema": {
            "type": "object",
            "properties": {
                "cmd": {"type": "string"}
            },
            "required": ["cmd"]
        },
        "input_schema": {
            "type": "object",
            "properties": {
                "cmd": {"type": "string"}
            },
            "required": ["cmd"]
        }
    })]
}

fn conversation() -> Conversation {
    let mut conversation = Conversation::new();
    conversation.push(Message::system("You are a helpful assistant."));
    conversation.push(Message::user("List files"));
    conversation
}

fn anthropic_segmented_conversation(
    include_repo_map: bool,
    include_dynamic_tail: bool,
) -> Conversation {
    let mut conversation = Conversation::new();
    let mut system = Message::system_with_metadata(
        "Stable system prefix",
        djinn_core::message::MessageMeta {
            input_tokens: None,
            output_tokens: None,
            timestamp: None,
            provider_data: Some(json!({
                "anthropic_cache_breakpoint": CacheBreakpoint {
                    kind: Some("stable_prefix".to_string()),
                }
            })),
        },
    );
    if include_repo_map {
        system
            .content
            .push(djinn_core::message::ContentBlock::Text {
                text: "Repository map".to_string(),
            });
    }
    if include_dynamic_tail {
        system
            .content
            .push(djinn_core::message::ContentBlock::Text {
                text: "Task-specific volatile context".to_string(),
            });
    }
    conversation.push(system);
    conversation.push(Message::user("List files"));
    conversation
}

async fn drain_provider_stream_with_conversation(
    provider: &dyn LlmProvider,
    conversation: &Conversation,
    tools: &[Value],
    tool_choice: Option<ToolChoice>,
) {
    let mut stream = provider
        .stream(conversation, tools, tool_choice)
        .await
        .expect("provider stream should start");

    while let Some(item) = stream.next().await {
        item.expect("stream item should succeed");
    }
}

async fn drain_provider_stream(
    provider: &dyn LlmProvider,
    tools: &[Value],
    tool_choice: Option<ToolChoice>,
) {
    let conversation = conversation();
    drain_provider_stream_with_conversation(provider, &conversation, tools, tool_choice).await;
}

fn sse_template() -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string("data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":0}}\n\ndata: [DONE]\n\n")
}

fn anthropic_sse_template() -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n\
             data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":0}}\n\n\
             data: {\"type\":\"message_stop\"}\n\n",
        )
}

#[tokio::test]
async fn post_json_emits_json_post_without_auth_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/json"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{\"ok\":true}"))
        .mount(&server)
        .await;

    let body = json!({"hello": "world", "answer": 42});
    let response = ApiClient::new()
        .post_json(
            &format!("{}/json", server.uri()),
            body,
            &AuthMethod::NoAuth,
            HeaderMap::new(),
        )
        .await
        .expect("post_json should succeed");

    assert_eq!(response, "{\"ok\":true}");

    let requests = server.received_requests().await.expect("captured requests");
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method.as_str(), "POST");
    assert_eq!(request.url.path(), "/json");
    assert_eq!(
        request
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    assert!(request.headers.get("authorization").is_none());
    let parsed_body: Value = serde_json::from_slice(&request.body).expect("json request body");
    assert_eq!(parsed_body["hello"], "world");
    assert_eq!(parsed_body["answer"], 42);
}

#[tokio::test]
async fn stream_sse_sends_bearer_auth_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/sse"))
        .and(header("authorization", "Bearer secret-token"))
        .respond_with(sse_template())
        .mount(&server)
        .await;

    let mut stream = ApiClient::new().stream_sse(
        &format!("{}/sse", server.uri()),
        json!({"message": "ping"}),
        &AuthMethod::BearerToken("secret-token".to_string()),
        HeaderMap::new(),
    );

    while let Some(item) = stream.next().await {
        item.expect("stream item should succeed");
    }

    let requests = server.received_requests().await.expect("captured requests");
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(
        request
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok()),
        Some("Bearer secret-token")
    );
    assert_eq!(
        request
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    let parsed_body: Value = serde_json::from_slice(&request.body).expect("json request body");
    assert_eq!(parsed_body["message"], "ping");
}

#[tokio::test]
async fn stream_sse_sends_custom_api_key_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/sse"))
        .and(header("x-api-key", "anthropic-secret"))
        .respond_with(sse_template())
        .mount(&server)
        .await;

    let mut stream = ApiClient::new().stream_sse(
        &format!("{}/sse", server.uri()),
        json!({"message": "ping"}),
        &AuthMethod::ApiKeyHeader {
            header: "x-api-key".to_string(),
            key: "anthropic-secret".to_string(),
        },
        HeaderMap::new(),
    );

    while let Some(item) = stream.next().await {
        item.expect("stream item should succeed");
    }

    let requests = server.received_requests().await.expect("captured requests");
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(
        request
            .headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok()),
        Some("anthropic-secret")
    );
    assert!(request.headers.get("authorization").is_none());
    assert_eq!(
        request
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
}

#[tokio::test]
async fn openai_provider_serializes_required_tool_choice_in_request_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_template())
        .mount(&server)
        .await;

    let provider = OpenAIProvider::new(openai_config(
        server.uri(),
        AuthMethod::BearerToken("provider-token".to_string()),
    ));
    let tools = tool_definition();
    drain_provider_stream(&provider, &tools, Some(ToolChoice::Required)).await;

    let requests = server.received_requests().await.expect("captured requests");
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(
        request
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok()),
        Some("Bearer provider-token")
    );
    assert_eq!(
        request
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );

    let body: Value = serde_json::from_slice(&request.body).expect("json body");
    assert_eq!(body["model"], "gpt-4o-mini");
    assert_eq!(body["stream"], true);
    assert_eq!(body["tool_choice"], "required");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(
        body["messages"][0]["content"],
        "You are a helpful assistant."
    );
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(body["messages"][1]["content"], "List files");
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "shell");
    assert_eq!(
        body["tools"][0]["function"]["description"],
        "Run a shell command"
    );
    assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    assert_eq!(
        body["tools"][0]["function"]["parameters"]["properties"]["cmd"]["type"],
        "string"
    );
}

#[tokio::test]
async fn openai_provider_serializes_none_tool_choice_in_request_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_template())
        .mount(&server)
        .await;

    let provider = OpenAIProvider::new(openai_config(server.uri(), AuthMethod::NoAuth));
    let tools = tool_definition();
    drain_provider_stream(&provider, &tools, Some(ToolChoice::None)).await;

    let requests = server.received_requests().await.expect("captured requests");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("json body");
    assert_eq!(body["tool_choice"], "none");
    assert_eq!(body["tools"][0]["function"]["name"], "shell");
}

#[tokio::test]
async fn anthropic_provider_serializes_required_tool_choice_and_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(anthropic_sse_template())
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(anthropic_config(
        server.uri(),
        AuthMethod::ApiKeyHeader {
            header: "x-api-key".to_string(),
            key: "anthropic-key".to_string(),
        },
    ));
    let tools = tool_definition();
    drain_provider_stream(&provider, &tools, Some(ToolChoice::Required)).await;

    let requests = server.received_requests().await.expect("captured requests");
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(
        request
            .headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok()),
        Some("anthropic-key")
    );
    assert_eq!(
        request
            .headers
            .get("anthropic-version")
            .and_then(|v| v.to_str().ok()),
        Some("2023-06-01")
    );
    assert_eq!(
        request
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );

    let body: Value = serde_json::from_slice(&request.body).expect("json body");
    assert_eq!(body["model"], "claude-3-5-sonnet");
    assert_eq!(body["system"], "You are a helpful assistant.");
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"][0]["type"], "text");
    assert_eq!(body["messages"][0]["content"][0]["text"], "List files");
    assert_eq!(body["tool_choice"]["type"], "any");
    assert_eq!(body["tools"][0]["name"], "shell");
    assert_eq!(body["tools"][0]["description"], "Run a shell command");
}

#[tokio::test]
async fn anthropic_provider_serializes_cache_control_for_stable_system_prefix() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(anthropic_sse_template())
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(anthropic_config(server.uri(), AuthMethod::NoAuth));
    let tools = tool_definition();
    let conversation = anthropic_segmented_conversation(false, false);
    drain_provider_stream_with_conversation(
        &provider,
        &conversation,
        &tools,
        Some(ToolChoice::Required),
    )
    .await;

    let requests = server.received_requests().await.expect("captured requests");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("json body");
    assert_eq!(body["system"], "Stable system prefix");
    assert_eq!(body["tools"][0]["name"], "shell");
    assert_eq!(body["tools"][0]["cache_control"]["kind"], "stable_prefix");
}

#[tokio::test]
async fn anthropic_provider_applies_cache_control_only_to_stable_prefix_blocks() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(anthropic_sse_template())
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(anthropic_config(server.uri(), AuthMethod::NoAuth));
    let tools = tool_definition();
    let conversation = anthropic_segmented_conversation(true, true);

    drain_provider_stream_with_conversation(
        &provider,
        &conversation,
        &tools,
        Some(ToolChoice::Required),
    )
    .await;

    let requests = server.received_requests().await.expect("captured requests");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("json body");
    let system = body["system"].as_array().expect("system block array");
    assert_eq!(system.len(), 3);
    assert_eq!(system[0]["text"], "Stable system prefix");
    assert_eq!(system[1]["text"], "Repository map");
    assert_eq!(system[2]["text"], "Task-specific volatile context");
    assert_eq!(system[0]["cache_control"]["kind"], "stable_prefix");
    assert_eq!(system[1]["cache_control"]["kind"], "stable_prefix");
    assert!(system[2].get("cache_control").is_none());
    assert_eq!(body["tools"][0]["cache_control"]["kind"], "stable_prefix");
}

#[tokio::test]
async fn anthropic_provider_preserves_order_when_repo_map_is_absent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(anthropic_sse_template())
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(anthropic_config(server.uri(), AuthMethod::NoAuth));
    let tools = tool_definition();
    let conversation = anthropic_segmented_conversation(false, true);

    drain_provider_stream_with_conversation(
        &provider,
        &conversation,
        &tools,
        Some(ToolChoice::Required),
    )
    .await;

    let requests = server.received_requests().await.expect("captured requests");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("json body");
    let system = body["system"].as_array().expect("system block array");
    assert_eq!(system.len(), 2);
    assert_eq!(system[0]["text"], "Stable system prefix");
    assert_eq!(system[1]["text"], "Task-specific volatile context");
    assert_eq!(system[0]["cache_control"]["kind"], "stable_prefix");
    assert!(system[1].get("cache_control").is_none());
    assert_eq!(body["tools"][0]["cache_control"]["kind"], "stable_prefix");
}

#[tokio::test]
async fn anthropic_provider_preserves_order_when_dynamic_tail_is_absent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(anthropic_sse_template())
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(anthropic_config(server.uri(), AuthMethod::NoAuth));
    let tools = tool_definition();
    let conversation = anthropic_segmented_conversation(true, false);

    drain_provider_stream_with_conversation(
        &provider,
        &conversation,
        &tools,
        Some(ToolChoice::Required),
    )
    .await;

    let requests = server.received_requests().await.expect("captured requests");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("json body");
    let system = body["system"].as_array().expect("system block array");
    assert_eq!(system.len(), 2);
    assert_eq!(system[0]["text"], "Stable system prefix");
    assert_eq!(system[1]["text"], "Repository map");
    assert_eq!(system[0]["cache_control"]["kind"], "stable_prefix");
    assert!(system[1].get("cache_control").is_none());
    assert_eq!(body["tools"][0]["cache_control"]["kind"], "stable_prefix");
}

#[tokio::test]
async fn anthropic_provider_serializes_none_tool_choice_in_request_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(anthropic_sse_template())
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(anthropic_config(server.uri(), AuthMethod::NoAuth));
    let tools = tool_definition();
    drain_provider_stream(&provider, &tools, Some(ToolChoice::None)).await;

    let requests = server.received_requests().await.expect("captured requests");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("json body");
    assert_eq!(body["tool_choice"]["type"], "none");
    assert_eq!(body["tools"][0]["name"], "shell");
}
