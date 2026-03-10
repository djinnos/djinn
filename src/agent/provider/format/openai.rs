use async_stream::stream;
use futures::StreamExt;
use reqwest::header::HeaderMap;
use serde::Deserialize;
use serde_json::{json, Value};
use std::pin::Pin;

use crate::agent::message::{ContentBlock, Conversation};
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
        let messages = conversation.to_openai_messages();

        let mut body = json!({
            "model": self.config.model_id,
            "messages": messages,
        });

        // Only request streaming when the provider supports it.
        if self.config.capabilities.streaming {
            body["stream"] = json!(true);
            body["stream_options"] = json!({"include_usage": true});
        }

        if !tools.is_empty() {
            body["tools"] = json!(convert_tools_to_openai(tools));
        }

        body
    }

    fn effective_url(&self) -> String {
        format!("{}/chat/completions", self.config.base_url.trim_end_matches('/'))
    }

    fn extra_headers(&self) -> HeaderMap {
        HeaderMap::new()
    }
}

// ─── Tool conversion ─────────────────────────────────────────────────────────

/// Convert RMCP tool format to OpenAI function-calling format.
/// RMCP: `{"name", "description", "inputSchema"}`
/// OpenAI: `{"type": "function", "function": {"name", "description", "parameters"}}`
fn convert_tools_to_openai(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            if t.get("type").is_some() && t.get("function").is_some() {
                t.clone()
            } else {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name").cloned().unwrap_or(json!("")),
                        "description": t.get("description").cloned().unwrap_or(json!("")),
                        "parameters": t.get("inputSchema").cloned().unwrap_or(json!({"type": "object"})),
                    }
                })
            }
        })
        .collect()
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

/// Parse a complete (non-streaming) OpenAI chat completion response into
/// `StreamEvent`s. Used when `capabilities.streaming` is `false`.
pub fn parse_openai_response(body: &str) -> Vec<StreamEvent> {
    let v: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let mut events = vec![];

    // Usage
    if let Some(usage) = v.get("usage") {
        let input = usage
            .get("prompt_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u32;
        let output = usage
            .get("completion_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u32;
        events.push(StreamEvent::Usage(TokenUsage { input, output }));
    }

    // choices[0].message
    if let Some(choices) = v.get("choices").and_then(|c| c.as_array()) {
        for choice in choices {
            let msg = match choice.get("message") {
                Some(m) => m,
                None => continue,
            };

            // Text content
            if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
                if !text.is_empty() {
                    events.push(StreamEvent::Delta(ContentBlock::Text {
                        text: text.to_string(),
                    }));
                }
            }

            // Tool calls
            if let Some(tool_calls) = msg.get("tool_calls").and_then(|tc| tc.as_array()) {
                for tc in tool_calls {
                    let id = tc
                        .get("id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = tc
                        .pointer("/function/name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args_str = tc
                        .pointer("/function/arguments")
                        .and_then(|x| x.as_str())
                        .unwrap_or("{}");
                    let input = serde_json::from_str(args_str).unwrap_or(Value::Null);
                    events.push(StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }));
                }
            }
        }
    }

    events.push(StreamEvent::Done);
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
        let streaming = self.config.capabilities.streaming;

        Box::pin(async move {
            if streaming {
                // Streaming path: SSE
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
            } else {
                // Non-streaming path: single POST, parse complete response
                let response_body = self.client.post_json(&url, body, &auth, extra_headers).await?;
                let events = parse_openai_response(&response_body);
                let out: Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>> =
                    Box::pin(futures::stream::iter(events.into_iter().map(Ok)));
                Ok(out)
            }
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
