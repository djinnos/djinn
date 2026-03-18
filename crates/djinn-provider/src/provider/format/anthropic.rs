use async_stream::stream;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use std::pin::Pin;

use crate::message::{ContentBlock, Conversation};
use crate::provider::client::ApiClient;
use crate::provider::{LlmProvider, ProviderConfig, StreamEvent, TokenUsage};

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

    fn build_request(&self, conversation: &Conversation, tools: &[Value]) -> Value {
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
        let body = self.build_request(conversation, tools);
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

        // content_block_start with tool_use
        let start = r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"shell"}}"#;
        let e1 = parse_anthropic_event("content_block_start", start, &mut acc, &mut input_tokens);
        assert!(e1.is_empty());
        assert!(acc.is_some());

        // input_json_delta fragments
        let frag1 = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\""}}"#;
        parse_anthropic_event("content_block_delta", frag1, &mut acc, &mut input_tokens);

        let frag2 = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"ls\"}"}}"#;
        parse_anthropic_event("content_block_delta", frag2, &mut acc, &mut input_tokens);

        // content_block_stop emits tool use
        let stop = r#"{"type":"content_block_stop","index":0}"#;
        let events = parse_anthropic_event("content_block_stop", stop, &mut acc, &mut input_tokens);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }) => {
                assert_eq!(id, "toolu_01");
                assert_eq!(name, "shell");
                assert_eq!(input["cmd"], "ls");
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
    fn test_content_block_delta_input_json_without_active_tool_is_ignored() {
        let data = r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{}"}}"#;
        let mut acc = None;
        let mut input_tokens = 0u32;
        let events =
            parse_anthropic_event("content_block_delta", data, &mut acc, &mut input_tokens);
        assert!(events.is_empty());
        assert!(acc.is_none());
    }

    #[test]
    fn test_content_block_delta_unknown_variant_ignored() {
        let data = r#"{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"hmm"}}"#;
        let mut acc = None;
        let mut input_tokens = 0u32;
        let events =
            parse_anthropic_event("content_block_delta", data, &mut acc, &mut input_tokens);
        assert!(events.is_empty());
    }

    #[test]
    fn test_empty_text_delta_ignored() {
        let data = r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":""}}"#;
        let mut acc = None;
        let mut input_tokens = 0u32;
        let events =
            parse_anthropic_event("content_block_delta", data, &mut acc, &mut input_tokens);
        assert!(events.is_empty());
    }

    #[test]
    fn test_content_block_stop_without_active_tool_emits_nothing() {
        let data = r#"{"type":"content_block_stop","index":0}"#;
        let mut acc = None;
        let mut input_tokens = 0u32;
        let events = parse_anthropic_event("content_block_stop", data, &mut acc, &mut input_tokens);
        assert!(events.is_empty());
    }
}
