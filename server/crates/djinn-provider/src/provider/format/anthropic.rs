use async_stream::stream;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use std::pin::Pin;

use crate::message::{CacheBreakpoint, ContentBlock, Conversation};
use crate::provider::client::ApiClient;
use crate::provider::{LlmProvider, ProviderConfig, StreamEvent, TokenUsage, ToolChoice};

const ANTHROPIC_CACHE_BREAKPOINT_KEY: &str = "anthropic_cache_breakpoint";

#[derive(Debug, Clone, PartialEq)]
struct AnthropicSystemBlock {
    text: String,
    cache_control: Option<Value>,
}

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

    fn maybe_cache_control(message: &djinn_core::message::Message) -> Option<Value> {
        message
            .metadata
            .as_ref()
            .and_then(|meta| meta.provider_data.as_ref())
            .and_then(|data| data.get(ANTHROPIC_CACHE_BREAKPOINT_KEY))
            .and_then(|value| serde_json::from_value::<CacheBreakpoint>(value.clone()).ok())
            .map(|breakpoint| {
                let mut obj = serde_json::Map::new();
                obj.insert("type".to_string(), json!("ephemeral"));
                if let Some(kind) = breakpoint.kind {
                    obj.insert("kind".to_string(), json!(kind));
                }
                Value::Object(obj)
            })
    }

    fn system_blocks(conversation: &Conversation) -> Vec<AnthropicSystemBlock> {
        conversation
            .messages
            .iter()
            .filter(|message| message.role == djinn_core::message::Role::System)
            .flat_map(|message| {
                let cache_control = Self::maybe_cache_control(message);
                let content_len = message.content.len();
                message
                    .content
                    .iter()
                    .enumerate()
                    .filter_map(move |(index, block)| match block {
                        ContentBlock::Text { text } if !text.trim().is_empty() => {
                            let cache_control =
                                if cache_control.is_some() && index + 1 < content_len {
                                    cache_control.clone()
                                } else {
                                    None
                                };
                            Some(AnthropicSystemBlock {
                                text: text.clone(),
                                cache_control,
                            })
                        }
                        _ => None,
                    })
            })
            .collect()
    }

    fn serialize_system_blocks(blocks: &[AnthropicSystemBlock]) -> Value {
        if blocks.len() == 1 && blocks[0].cache_control.is_none() {
            return Value::String(blocks[0].text.clone());
        }

        Value::Array(
            blocks
                .iter()
                .map(|block| {
                    let mut obj = serde_json::Map::new();
                    obj.insert("type".to_string(), json!("text"));
                    obj.insert("text".to_string(), json!(block.text));
                    if let Some(cache_control) = &block.cache_control {
                        obj.insert("cache_control".to_string(), cache_control.clone());
                    }
                    Value::Object(obj)
                })
                .collect(),
        )
    }

    fn build_request(
        &self,
        conversation: &Conversation,
        tools: &[Value],
        tool_choice: Option<ToolChoice>,
    ) -> Value {
        let (_system, messages) = conversation.to_anthropic_messages();
        let system_blocks = Self::system_blocks(conversation);

        let max_tokens = self.config.capabilities.max_tokens_default.unwrap_or(8192);

        let mut body = json!({
            "model": self.config.model_id,
            "system": Self::serialize_system_blocks(&system_blocks),
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
    fn test_build_request_preserves_separate_system_blocks_with_cache_control() {
        let provider = test_provider();
        let mut conv = Conversation::default();
        conv.push(crate::message::Message::system_with_metadata(
            "base prompt",
            crate::message::MessageMeta {
                input_tokens: None,
                output_tokens: None,
                timestamp: None,
                provider_data: Some(json!({
                    ANTHROPIC_CACHE_BREAKPOINT_KEY: CacheBreakpoint {
                        kind: Some("stable_prefix".to_string()),
                    }
                })),
            },
        ));
        conv.messages[0].content.push(ContentBlock::Text {
            text: "tool definitions".to_string(),
        });
        conv.messages[0].content.push(ContentBlock::Text {
            text: "repo map".to_string(),
        });
        conv.push(crate::message::Message::user("hello"));

        let req = provider.build_request(&conv, &[], None);
        let system = req["system"].as_array().expect("system block array");
        assert_eq!(system.len(), 3);
        assert_eq!(system[0]["text"], "base prompt");
        assert_eq!(system[1]["text"], "tool definitions");
        assert_eq!(system[2]["text"], "repo map");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(system[0]["cache_control"]["kind"], "stable_prefix");
        assert_eq!(system[1]["cache_control"]["kind"], "stable_prefix");
        assert!(system[2].get("cache_control").is_none());
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

    // ─── End-to-end prompt assembly → Anthropic request coverage ──────────────

    /// Helper that replicates the chat-layer `build_system_message` logic:
    /// stable-prefix segments become individual ContentBlocks in a single System
    /// message with `cache_control` metadata, while dynamic segments are merged
    /// into a trailing block without cache metadata.
    fn build_system_message_for_test(
        base_prompt: &str,
        project_context: Option<&str>,
        repo_map: Option<&str>,
        client_system: Option<&str>,
        is_anthropic: bool,
    ) -> Message {
        // Stable segments: base, project, repo-map
        let mut stable_texts: Vec<String> = vec![base_prompt.trim().to_string()];
        if let Some(ctx) = project_context.filter(|s| !s.trim().is_empty()) {
            stable_texts.push(ctx.to_string());
        }
        if let Some(rm) = repo_map.filter(|s| !s.trim().is_empty()) {
            stable_texts.push(rm.to_string());
        }

        // Dynamic tail
        let dynamic_tail = client_system
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string());

        let has_stable_prefix = true; // base prompt always present
        let metadata = if is_anthropic && has_stable_prefix {
            Some(crate::message::MessageMeta {
                input_tokens: None,
                output_tokens: None,
                timestamp: None,
                provider_data: Some(json!({
                    ANTHROPIC_CACHE_BREAKPOINT_KEY: CacheBreakpoint {
                        kind: Some("stable_prefix".to_string()),
                    }
                })),
            })
        } else {
            None
        };

        let mut content: Vec<ContentBlock> = stable_texts
            .into_iter()
            .map(|text| ContentBlock::Text { text })
            .collect();
        if let Some(tail) = dynamic_tail {
            content.push(ContentBlock::Text { text: tail });
        }

        Message {
            role: crate::message::Role::System,
            content,
            metadata,
        }
    }

    /// E2E: with repo map present, Anthropic system blocks preserve ordering
    /// (base → project → repo map → dynamic tail) and stable-prefix
    /// `cache_control` appears only on the non-final stable blocks.
    #[test]
    fn e2e_repo_map_present_system_blocks_ordered_with_cache_control() {
        let provider = test_provider();
        let base = "You are a helpful assistant.";
        let project = "## Current Project\n**Name**: Demo\n**Path**: /tmp/demo";
        let repo_map = "## Repository Map\nsrc/lib.rs\n  pub fn run()";
        let client = "Be concise.";

        let sys_msg =
            build_system_message_for_test(base, Some(project), Some(repo_map), Some(client), true);

        let mut conv = Conversation::new();
        conv.push(sys_msg);
        conv.push(Message::user("What does this project do?"));

        let req = provider.build_request(&conv, &[], None);
        let system = req["system"]
            .as_array()
            .expect("system should be an array when cache_control is present");

        // Ordering: base, project, repo_map, dynamic tail
        assert_eq!(system.len(), 4, "expected 4 system blocks");
        assert_eq!(system[0]["text"], base.trim());
        assert_eq!(system[1]["text"], project);
        assert_eq!(system[2]["text"], repo_map);
        assert_eq!(system[3]["text"], client);

        // cache_control on all stable-prefix blocks except the last content block
        assert!(
            system[0].get("cache_control").is_some(),
            "base prompt block should have cache_control"
        );
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(system[0]["cache_control"]["kind"], "stable_prefix");

        assert!(
            system[1].get("cache_control").is_some(),
            "project context block should have cache_control"
        );
        assert_eq!(system[1]["cache_control"]["kind"], "stable_prefix");

        assert!(
            system[2].get("cache_control").is_some(),
            "repo map block should have cache_control"
        );
        assert_eq!(system[2]["cache_control"]["kind"], "stable_prefix");

        // Dynamic tail (last block) has no cache_control
        assert!(
            system[3].get("cache_control").is_none(),
            "dynamic tail block must not have cache_control"
        );
    }

    /// E2E: without repo map or project context, a single non-cacheable system
    /// block collapses to a plain string (no array, no cache_control).
    #[test]
    fn e2e_no_repo_map_single_block_no_cache_control() {
        let provider = test_provider();
        let base = "You are a helpful assistant.";

        // No project context, no repo map, no client system — just the base prompt.
        // When there is only one content block, the chat layer omits cache metadata
        // when the model is non-Anthropic OR when the prompt is trivially simple.
        let sys_msg = build_system_message_for_test(base, None, None, None, false);

        let mut conv = Conversation::new();
        conv.push(sys_msg);
        conv.push(Message::user("Hello"));

        let req = provider.build_request(&conv, &[], None);

        // Single block without cache_control → serialized as plain string
        assert!(
            req["system"].is_string(),
            "single-block system without cache_control should serialize as a plain string"
        );
        assert_eq!(req["system"], base.trim());
    }

    /// E2E: Anthropic model with base prompt only (no optional contexts) still
    /// uses array format when cache metadata is attached, and cache_control
    /// is on the non-final block only (here: there is only one block which is
    /// also the final block, so no cache_control on it).
    #[test]
    fn e2e_anthropic_base_only_with_cache_metadata_formats_as_single_block() {
        let provider = test_provider();
        let base = "You are a helpful assistant.";

        // Anthropic model, base prompt only — metadata says stable_prefix but
        // there is only 1 content block. The cache_control logic attaches it to
        // all blocks except the last, so with 1 block there is no cache_control.
        let sys_msg = build_system_message_for_test(base, None, None, None, true);

        let mut conv = Conversation::new();
        conv.push(sys_msg);
        conv.push(Message::user("Hello"));

        let req = provider.build_request(&conv, &[], None);

        // Single content block (even with metadata present) → last-block rule
        // means no cache_control → serialized as plain string.
        assert!(
            req["system"].is_string(),
            "single-block anthropic system should still be a plain string \
             when cache_control is absent on the only block"
        );
        assert_eq!(req["system"], base.trim());
    }

    /// E2E: repo-map session with tools verifies that system blocks and tool
    /// definitions coexist correctly in the request body.
    #[test]
    fn e2e_repo_map_with_tools_preserves_both_system_and_tools() {
        let provider = test_provider();
        let base = "You are a helpful assistant.";
        let repo_map = "## Repository Map\nsrc/main.rs\n  fn main()";

        let sys_msg =
            build_system_message_for_test(base, None, Some(repo_map), Some("be brief"), true);

        let mut conv = Conversation::new();
        conv.push(sys_msg);
        conv.push(Message::user("List files"));

        let tools = vec![json!({
            "name": "shell",
            "description": "Run a shell command",
            "input_schema": {"type": "object", "properties": {"cmd": {"type": "string"}}}
        })];

        let req = provider.build_request(&conv, &tools, None);
        let system = req["system"]
            .as_array()
            .expect("system should be array with cache_control");
        assert_eq!(system.len(), 3); // base, repo_map, dynamic tail

        // Tools appear in the request body alongside system blocks
        let req_tools = req["tools"].as_array().expect("tools array");
        assert_eq!(req_tools.len(), 1);
        assert_eq!(req_tools[0]["name"], "shell");
    }
}
