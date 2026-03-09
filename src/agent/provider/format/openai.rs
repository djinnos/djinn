use async_stream::stream;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::{json, Value};
use std::pin::Pin;

use crate::agent::message::{ContentBlock, Conversation, Role};
use crate::agent::provider::{
    LlmProvider, ProviderConfig, StreamEvent, TokenUsage,
};
use crate::agent::provider::client::ApiClient;

pub struct OpenAIProvider {
    config: ProviderConfig,
    client: ApiClient,
}

impl OpenAIProvider {
    pub fn new(config: ProviderConfig) -> Self {
        Self {
            config,
            client: ApiClient::new(),
        }
    }

    fn build_request(&self, conversation: &Conversation, tools: &[Value]) -> Value {
        // Convert messages
        let messages: Vec<Value> = conversation
            .messages
            .iter()
            .map(|msg| {
                let role = match msg.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };

                // Build content — for OpenAI, merge text blocks into a string for simple cases
                // or use array form for tool results
                let content_blocks: Vec<Value> = msg
                    .content
                    .iter()
                    .map(|block| match block {
                        ContentBlock::Text { text } => json!({"type": "text", "text": text}),
                        ContentBlock::ToolUse { id, name, input } => json!({
                            "type": "function",
                            "id": id,
                            "function": {"name": name, "arguments": input.to_string()}
                        }),
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error: _,
                        } => {
                            let text = content
                                .iter()
                                .filter_map(|c| {
                                    if let ContentBlock::Text { text } = c {
                                        Some(text.as_str())
                                    } else {
                                        None
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join("");
                            json!({"role": "tool", "tool_call_id": tool_use_id, "content": text})
                        }
                    })
                    .collect();

                // For tool results, the role is determined by the block itself
                if content_blocks.iter().any(|b| {
                    b.get("role")
                        .and_then(|r| r.as_str())
                        .map(|r| r == "tool")
                        .unwrap_or(false)
                }) {
                    // Return each tool result as a separate message
                    content_blocks.into_iter().next().unwrap_or(json!({}))
                } else {
                    json!({"role": role, "content": content_blocks})
                }
            })
            .collect();

        let mut body = json!({
            "model": self.config.model_id,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true}
        });

        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }

        body
    }

    fn effective_url(&self) -> String {
        if let Some(proxy) = &self.config.dev_proxy {
            format!("{}/chat/completions", proxy.url.trim_end_matches('/'))
        } else {
            format!("{}/chat/completions", self.config.base_url.trim_end_matches('/'))
        }
    }

    fn extra_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(proxy) = &self.config.dev_proxy
            && let Ok(val) = HeaderValue::from_str(&format!("Bearer {}", proxy.auth_key))
        {
            headers.insert(
                HeaderName::from_static("helicone-auth"),
                val,
            );
        }
        headers
    }
}

// ─── SSE parsing helpers ──────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct DeltaFunction {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct DeltaToolCall {
    #[allow(dead_code)]
    index: Option<u32>,
    id: Option<String>,
    function: Option<DeltaFunction>,
}

#[derive(Deserialize, Default)]
struct Delta {
    content: Option<String>,
    tool_calls: Option<Vec<DeltaToolCall>>,
}

#[derive(Deserialize)]
struct Choice {
    delta: Option<Delta>,
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct UsageChunk {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct StreamChunk {
    choices: Option<Vec<Choice>>,
    usage: Option<UsageChunk>,
}

/// Parse a single SSE data line from the OpenAI streaming API.
/// Returns zero or more `StreamEvent`s produced by this line.
pub fn parse_openai_line(
    line: &str,
    tool_acc: &mut Option<(String, String, String)>, // (id, name, arguments)
) -> Vec<StreamEvent> {
    let chunk: StreamChunk = match serde_json::from_str(line) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let mut events = vec![];

    // Usage field (appears in final chunk when stream_options.include_usage=true)
    if let Some(usage) = chunk.usage {
        events.push(StreamEvent::Usage(TokenUsage {
            input: usage.prompt_tokens.unwrap_or(0),
            output: usage.completion_tokens.unwrap_or(0),
        }));
    }

    let choices = match chunk.choices {
        Some(c) => c,
        None => return events,
    };

    for choice in choices {
        let delta = match choice.delta {
            Some(d) => d,
            None => continue,
        };

        // Text content
        if let Some(text) = delta.content && !text.is_empty() {
            events.push(StreamEvent::Delta(ContentBlock::Text { text }));
        }

        // Tool calls — accumulate across chunks
        if let Some(tool_calls) = delta.tool_calls {
            for tc in tool_calls {
                let func = tc.function.unwrap_or_default();
                match tool_acc {
                    None => {
                        // First chunk for this tool call
                        *tool_acc = Some((
                            tc.id.unwrap_or_default(),
                            func.name.unwrap_or_default(),
                            func.arguments.unwrap_or_default(),
                        ));
                    }
                    Some((_, _, args)) => {
                        // Append arguments fragment
                        if let Some(frag) = func.arguments {
                            args.push_str(&frag);
                        }
                    }
                }
            }
        }

        // On finish_reason="tool_calls", emit the accumulated tool use
        if choice
            .finish_reason
            .as_deref()
            .map(|r| r == "tool_calls")
            .unwrap_or(false)
            && let Some((id, name, args)) = tool_acc.take()
        {
            let input = serde_json::from_str(&args).unwrap_or(Value::Null);
            events.push(StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }));
        }
    }

    events
}

impl LlmProvider for OpenAIProvider {
    fn name(&self) -> &str {
        "openai"
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
        let auth = self.config.auth.clone();
        let extra_headers = self.extra_headers();

        Box::pin(async move {
            let raw = self.client.stream_sse(&url, body, &auth, extra_headers);
            let out: Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>> =
                Box::pin(stream! {
                    let mut tool_acc: Option<(String, String, String)> = None;
                    let mut raw_stream = raw;
                    while let Some(result) = raw_stream.next().await {
                        match result {
                            Err(e) => { yield Err(e); return; }
                            Ok(line) => {
                                for event in parse_openai_line(&line, &mut tool_acc) {
                                    yield Ok(event);
                                }
                            }
                        }
                    }
                    yield Ok(StreamEvent::Done);
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
    fn test_parse_text_delta() {
        let line = r#"{"choices":[{"delta":{"content":"hello"},"finish_reason":null,"index":0}]}"#;
        let mut acc = None;
        let events = parse_openai_line(line, &mut acc);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Delta(ContentBlock::Text { text }) => assert_eq!(text, "hello"),
            _ => panic!("expected text delta"),
        }
    }

    #[test]
    fn test_parse_empty_content_skipped() {
        let line = r#"{"choices":[{"delta":{"content":""},"finish_reason":null,"index":0}]}"#;
        let mut acc = None;
        let events = parse_openai_line(line, &mut acc);
        assert!(events.is_empty());
    }

    #[test]
    fn test_parse_tool_call_accumulated() {
        // First chunk: tool call start
        let line1 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_abc","function":{"name":"shell","arguments":""}}]},"finish_reason":null}]}"#;
        // Second chunk: arguments fragment
        let line2 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\":\"ls\"}"}}]},"finish_reason":null}]}"#;
        // Final chunk: finish_reason=tool_calls
        let line3 = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;

        let mut acc = None;
        let e1 = parse_openai_line(line1, &mut acc);
        assert!(e1.is_empty(), "no events on first tool chunk");
        assert!(acc.is_some());

        let e2 = parse_openai_line(line2, &mut acc);
        assert!(e2.is_empty(), "no events while accumulating");

        let e3 = parse_openai_line(line3, &mut acc);
        assert_eq!(e3.len(), 1);
        match &e3[0] {
            StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }) => {
                assert_eq!(id, "call_abc");
                assert_eq!(name, "shell");
                assert_eq!(input["cmd"], "ls");
            }
            _ => panic!("expected tool use"),
        }
        assert!(acc.is_none(), "accumulator cleared after emit");
    }

    #[test]
    fn test_parse_usage() {
        let line = r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":20}}"#;
        let mut acc = None;
        let events = parse_openai_line(line, &mut acc);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Usage(u) => {
                assert_eq!(u.input, 10);
                assert_eq!(u.output, 20);
            }
            _ => panic!("expected usage"),
        }
    }
}
