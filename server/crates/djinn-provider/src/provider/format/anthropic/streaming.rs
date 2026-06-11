use async_stream::stream;
use futures::StreamExt;
use serde_json::Value;
use std::pin::Pin;

use crate::message::{ContentBlock, Conversation};
#[allow(unused_imports)]
use crate::provider::client::ApiClient;
use crate::provider::{LlmProvider, ProviderConfig, StreamEvent, TokenUsage, ToolChoice};

use super::request::AnthropicProvider;

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
    cache_read: &mut u32,
    cache_write: &mut u32,
) -> Vec<StreamEvent> {
    let mut events = vec![];

    match event_type {
        "message_start" => {
            // {"type":"message_start","message":{"usage":{"input_tokens":N,
            //  "cache_read_input_tokens":N,"cache_creation_input_tokens":N,...}}}
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                if let Some(n) = v
                    .pointer("/message/usage/input_tokens")
                    .and_then(|x| x.as_u64())
                {
                    *input_tokens = n as u32;
                }
                if let Some(n) = v
                    .pointer("/message/usage/cache_read_input_tokens")
                    .and_then(|x| x.as_u64())
                {
                    *cache_read = n as u32;
                }
                if let Some(n) = v
                    .pointer("/message/usage/cache_creation_input_tokens")
                    .and_then(|x| x.as_u64())
                {
                    *cache_write = n as u32;
                }
            }
        }

        "content_block_start" => {
            // {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"...","name":"..."}}
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                let block_type = v
                    .pointer("/content_block/type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                match block_type {
                    "tool_use" => {
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
                    "thinking" => {
                        // Extended thinking block — nothing to accumulate at start.
                    }
                    _ => {}
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
                    "thinking_delta" => {
                        let thinking = v
                            .pointer("/delta/thinking")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !thinking.is_empty() {
                            events.push(StreamEvent::Thinking(thinking));
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
            // {"type":"message_delta","usage":{"output_tokens":N,
            //  "cache_read_input_tokens":N,"cache_creation_input_tokens":N}}
            // Anthropic may also restate cache counts here — fold them in if present.
            if let Ok(v) = serde_json::from_str::<Value>(data)
                && let Some(n) = v.pointer("/usage/output_tokens").and_then(|x| x.as_u64())
            {
                if let Some(c) = v
                    .pointer("/usage/cache_read_input_tokens")
                    .and_then(|x| x.as_u64())
                {
                    *cache_read = c as u32;
                }
                if let Some(c) = v
                    .pointer("/usage/cache_creation_input_tokens")
                    .and_then(|x| x.as_u64())
                {
                    *cache_write = c as u32;
                }
                events.push(StreamEvent::Usage(TokenUsage {
                    input: *input_tokens,
                    output: n as u32,
                    cache_read: *cache_read,
                    cache_write: *cache_write,
                    reasoning_output: 0,
                    // Anthropic `input_tokens` excludes cached reads/writes; the
                    // real context the model saw is the sum of all three.
                    context_total: input_tokens
                        .saturating_add(*cache_read)
                        .saturating_add(*cache_write),
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

    fn config_snapshot(&self) -> Option<ProviderConfig> {
        Some(self.config.clone())
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
                    let mut cache_read: u32 = 0;
                    let mut cache_write: u32 = 0;

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
                                    for event in parse_anthropic_event(&event_type, &line, &mut tool_acc, &mut input_tokens, &mut cache_read, &mut cache_write) {
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
