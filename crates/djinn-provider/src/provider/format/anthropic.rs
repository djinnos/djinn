use async_stream::stream;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use std::pin::Pin;

use crate::message::{ContentBlock, Conversation};
use crate::provider::client::ApiClient;
use crate::provider::{LlmProvider, ProviderConfig, StreamEvent, TokenUsage, ToolChoice};

pub struct AnthropicProvider {
    config: ProviderConfig,
    client: ApiClient,
}

impl AnthropicProvider {
    pub fn new(config: ProviderConfig) -> Self {
        Self {
            config,
            client: ApiClient::new(),
        }
    }

    fn build_request(
        &self,
        conversation: &Conversation,
        tools: &[Value],
        tool_choice: Option<ToolChoice>,
    ) -> Value {
        let (system, messages) = conversation.to_anthropic_messages();

        let max_tokens = self.config.capabilities.max_tokens_default.unwrap_or(8192);

        let mut body = json!({
            "model": self.config.model_id,
            "system": system.unwrap_or_default(),
            "messages": messages,
            "max_tokens": max_tokens
        });

        if self.config.capabilities.streaming {
            body["stream"] = json!(true);
        }

        if !tools.is_empty() {
            body["tools"] = json!(tools);

            let thinking_enabled = body
                .get("thinking")
                .and_then(|thinking| thinking.get("type"))
                .and_then(Value::as_str)
                == Some("enabled");

            if !thinking_enabled {
                match tool_choice.unwrap_or(ToolChoice::Auto) {
                    ToolChoice::Auto => {}
                    ToolChoice::Required => body["tool_choice"] = json!({"type": "any"}),
                    ToolChoice::None => body["tool_choice"] = json!({"type": "none"}),
                }
            }
        }

        body
    }

    fn effective_url(&self) -> String {
        format!("{}/v1/messages", self.config.base_url.trim_end_matches('/'))
    }

    fn extra_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();

        // Anthropic version header (always required)
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static("2023-06-01"),
        );

        headers
    }
}

// ─── SSE parsing helpers ──────────────────────────────────────────────────────

/// State machine for accumulating a streaming tool use block.
#[derive(Default)]
pub(crate) struct ToolAcc {
    id: String,
    name: String,
    input_json: String,
}

/// Parse a single Anthropic SSE event (event_type + data JSON).
/// Mutates `tool_acc` in place; caller owns it across calls.
pub(crate) fn parse_anthropic_event(
    event_type: &str,
    data: &str,
    tool_acc: &mut Option<ToolAcc>,
    input_tokens: &mut u32,
) -> Vec<StreamEvent> {
    let mut events = vec![];

    match event_type {
        "message_start" => {
            // {"type":"message_start","message":{"usage":{"input_tokens":N,...}}}
            if let Ok(v) = serde_json::from_str::<Value>(data)
                && let Some(n) = v
                    .pointer("/message/usage/input_tokens")
                    .and_then(|x| x.as_u64())
            {
                *input_tokens = n as u32;
            }
        }

        "content_block_start" => {
            // {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"...","name":"..."}}
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                let block_type = v
                    .pointer("/content_block/type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                if block_type == "tool_use" {
                    let id = v
                        .pointer("/content_block/id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = v
                        .pointer("/content_block/name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    *tool_acc = Some(ToolAcc {
                        id,
                        name,
                        input_json: String::new(),
                    });
                }
            }
        }

        "content_block_delta" => {
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                let delta_type = v
                    .pointer("/delta/type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("");

                match delta_type {
                    "text_delta" => {
                        let text = v
                            .pointer("/delta/text")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !text.is_empty() {
                            events.push(StreamEvent::Delta(ContentBlock::Text { text }));
                        }
                    }
                    "input_json_delta" => {
                        if let Some(acc) = tool_acc.as_mut()
                            && let Some(frag) =
                                v.pointer("/delta/partial_json").and_then(|x| x.as_str())
                        {
                            acc.input_json.push_str(frag);
                        }
                    }
                    _ => {}
                }
            }
        }

        "content_block_stop" => {
            // If we were accumulating a tool use, emit it now
            if let Some(acc) = tool_acc.take() {
                let input = serde_json::from_str(&acc.input_json)
                    .unwrap_or(Value::Object(Default::default()));
                events.push(StreamEvent::Delta(ContentBlock::ToolUse {
                    id: acc.id,
                    name: acc.name,
                    input,
                }));
            }
        }

        "message_delta" => {
            // {"type":"message_delta","usage":{"output_tokens":N}}
            if let Ok(v) = serde_json::from_str::<Value>(data)
                && let Some(n) = v.pointer("/usage/output_tokens").and_then(|x| x.as_u64())
            {
                events.push(StreamEvent::Usage(TokenUsage {
                    input: *input_tokens,
                    output: n as u32,
                }));
            }
        }

        "message_stop" => {
            events.push(StreamEvent::Done);
        }

        _ => {} // ping, error, etc.
    }

    events
}

impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn stream<'a>(
        &'a self,
        conversation: &'a Conversation,
        tools: &'a [Value],
        tool_choice: Option<ToolChoice>,
    ) -> Pin<
        Box<
            dyn futures::Future<
                    Output = anyhow::Result<
                        Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>>,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let body = self.build_request(conversation, tools, tool_choice);
        let url = self.effective_url();
        let extra_headers = self.extra_headers();

        // For Anthropic, auth is via x-api-key header; we pass NoAuth here and
        // rely on the ApiKeyHeader auth being set in config.auth which is passed through.
        let auth = self.config.auth.clone();

        Box::pin(async move {
            let raw = self.client.stream_sse(&url, body, &auth, extra_headers);

            let out: Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>> =
                Box::pin(stream! {
                    let mut tool_acc: Option<ToolAcc> = None;
                    let mut input_tokens: u32 = 0;

                    // Anthropic SSE uses event: / data: pairs
                    // Our client currently yields only data: lines.
                    // We need to track event: lines too. Since ApiClient only yields data lines,
                    // we handle this by parsing event type from the data itself for Anthropic.
                    // The data JSON always has a "type" field.
                    let mut raw_stream = raw;
                    while let Some(result) = raw_stream.next().await {
                        match result {
                            Err(e) => { yield Err(e); return; }
                            Ok(line) => {
                                // Anthropic data lines contain the event type in the JSON
                                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                                    let event_type = v["type"].as_str().unwrap_or("").to_string();
                                    for event in parse_anthropic_event(&event_type, &line, &mut tool_acc, &mut input_tokens) {
                                        yield Ok(event);
                                    }
                                }
                            }
                        }
                    }
                });
            Ok(out)
        })
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Conversation, Message};
    use crate::provider::{AuthMethod, FormatFamily, ProviderCapabilities, ProviderConfig};
    use axum::{Router, routing::post};
    use futures::TryStreamExt;

    fn spawn_sse_server(status: u16, body: &'static str) -> String {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("bind local tcp listener");
        let addr = listener.local_addr().expect("local addr");
        listener.set_nonblocking(true).expect("set nonblocking");

        let rt = tokio::runtime::Handle::current();
        rt.spawn(async move {
            let app = Router::new().route(
                "/v1/messages",
                post(move |_req: axum::extract::Request| async move {
                    (
                        axum::http::StatusCode::from_u16(status).expect("status"),
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        body,
                    )
                }),
            );

            let tokio_listener =
                tokio::net::TcpListener::from_std(listener).expect("convert to tokio listener");
            axum::serve(tokio_listener, app).await.ok();
        });

        format!("http://{}:{}", addr.ip(), addr.port())
    }

    fn test_anthropic_config() -> ProviderConfig {
        ProviderConfig {
            base_url: "https://example.com".to_string(),
            auth: AuthMethod::NoAuth,
            format_family: FormatFamily::Anthropic,
            model_id: "claude-3-5-sonnet".to_string(),
            context_window: 200_000,
            telemetry: None,
            session_affinity_key: None,
            provider_headers: std::collections::HashMap::new(),
            capabilities: ProviderCapabilities {
                streaming: true,
                max_tokens_default: Some(8192),
            },
        }
    }

    fn test_provider() -> AnthropicProvider {
        AnthropicProvider::new(test_anthropic_config())
    }

    #[test]
    fn test_message_start_extracts_input_tokens() {
        let data = r#"{"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"claude-3-5-sonnet","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"output_tokens":1}}}"#;
        let mut acc = None;
        let mut input_tokens = 0u32;
        let events = parse_anthropic_event("message_start", data, &mut acc, &mut input_tokens);
        assert!(events.is_empty());
        assert_eq!(input_tokens, 25);
    }

    #[test]
    fn test_text_delta_event() {
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
        let mut acc = None;
        let mut input_tokens = 0u32;
        let events =
            parse_anthropic_event("content_block_delta", data, &mut acc, &mut input_tokens);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Delta(ContentBlock::Text { text }) => assert_eq!(text, "Hello"),
            _ => panic!("expected text delta"),
        }
    }

    #[test]
    fn test_tool_use_accumulation() {
        let mut acc = None;
        let mut input_tokens = 0u32;

        let start = r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"shell"}}"#;
        let e1 = parse_anthropic_event("content_block_start", start, &mut acc, &mut input_tokens);
        assert!(e1.is_empty());
        assert!(acc.is_some());

        let frag1 = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"l"}}"#;
        parse_anthropic_event("content_block_delta", frag1, &mut acc, &mut input_tokens);

        let frag2 = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"s\",\"dir\":\"/tmp\"}"}}"#;
        parse_anthropic_event("content_block_delta", frag2, &mut acc, &mut input_tokens);

        let stop = r#"{"type":"content_block_stop","index":0}"#;
        let events = parse_anthropic_event("content_block_stop", stop, &mut acc, &mut input_tokens);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }) => {
                assert_eq!(id.as_str(), "toolu_01");
                assert_eq!(name.as_str(), "shell");
                assert_eq!(input["cmd"].as_str(), Some("ls"));
                assert_eq!(input["dir"].as_str(), Some("/tmp"));
            }
            _ => panic!("expected tool use"),
        }
        assert!(acc.is_none());
    }

    #[test]
    fn test_message_delta_emits_usage() {
        let data = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}"#;
        let mut acc = None;
        let mut input_tokens = 10u32;
        let events = parse_anthropic_event("message_delta", data, &mut acc, &mut input_tokens);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Usage(u) => {
                assert_eq!(u.input, 10);
                assert_eq!(u.output, 42);
            }
            _ => panic!("expected usage"),
        }
    }

    #[test]
    fn test_message_stop_emits_done() {
        let data = r#"{"type":"message_stop"}"#;
        let mut acc = None;
        let mut input_tokens = 0u32;
        let events = parse_anthropic_event("message_stop", data, &mut acc, &mut input_tokens);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::Done));
    }

    #[test]
    fn test_build_request_always_populates_system_field() {
        let provider = test_provider();
        let mut conv = Conversation::default();
        conv.push(crate::message::Message::system("system prompt"));
        conv.push(crate::message::Message::user("first user"));
        conv.push(crate::message::Message::assistant("first assistant"));
        conv.push(crate::message::Message::user("second user"));

        let req = provider.build_request(&conv, &[], None);
        assert_eq!(req["system"], "system prompt");
        let messages = req["messages"].as_array().expect("messages array");
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["text"], "first user");
    }

    #[test]
    fn test_content_block_delta_input_json_without_active_tool_is_ignored() {
        let data = r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{}"}}"#;
        let mut acc = None;
        let mut input_tokens = 0u32;
        let events =
            parse_anthropic_event("content_block_delta", data, &mut acc, &mut input_tokens);
        assert!(events.is_empty());
        assert!(acc.is_none());
    }

    #[tokio::test]
    async fn test_stream_uses_payload_type_over_sse_event_name() {
        let body = concat!(
            "event: nope\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n\n",
            "event: wrong-name\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello from payload\"}}\n\n",
            "event: definitely-not-message-delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n\n",
            "event: not-message-stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let mut config = test_anthropic_config();
        config.base_url = spawn_sse_server(200, body);
        let provider = AnthropicProvider::new(config);
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));

        let events = provider
            .stream(&conv, &[], None)
            .await
            .expect("stream")
            .try_collect::<Vec<_>>()
            .await
            .expect("collect events");

        assert_eq!(events.len(), 3);
        match &events[0] {
            StreamEvent::Delta(ContentBlock::Text { text }) => {
                assert_eq!(text, "Hello from payload")
            }
            _ => panic!("expected text delta"),
        }
        match &events[1] {
            StreamEvent::Usage(u) => {
                assert_eq!(u.input, 7);
                assert_eq!(u.output, 9);
            }
            _ => panic!("expected usage"),
        }
        assert!(matches!(events[2], StreamEvent::Done));
    }

    #[tokio::test]
    async fn test_streamed_error_event_is_ignored_but_http_error_shape_surfaces() {
        let body = concat!(
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"try again later\"}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let mut config = test_anthropic_config();
        config.base_url = spawn_sse_server(200, body);
        let provider = AnthropicProvider::new(config);
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));

        let events = provider
            .stream(&conv, &[], None)
            .await
            .expect("stream")
            .try_collect::<Vec<_>>()
            .await
            .expect("collect events");
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::Done));

        let error_body = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        let mut error_config = test_anthropic_config();
        error_config.base_url = spawn_sse_server(401, error_body);
        let provider = AnthropicProvider::new(error_config);
        let err = provider
            .stream(&conv, &[], None)
            .await
            .expect("stream")
            .try_collect::<Vec<_>>()
            .await
            .expect_err("expected anthropic http error");
        let err_text = err.to_string();
        assert!(err_text.contains("provider API error 401 Unauthorized"));
        assert!(err_text.contains("authentication_error"));
        assert!(err_text.contains("invalid x-api-key"));
    }

    #[test]
    fn test_build_request_sets_required_tool_choice_when_tools_present() {
        let provider = test_provider();
        let mut conv = Conversation::new();
        conv.push(crate::message::Message::user("Hello"));
        let tools = vec![json!({
            "name": "shell",
            "description": "Run shell",
            "input_schema": {"type": "object"}
        })];

        let req = provider.build_request(&conv, &tools, Some(ToolChoice::Required));
        assert_eq!(req["tool_choice"]["type"], "any");
    }
}
