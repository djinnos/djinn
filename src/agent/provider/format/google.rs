use async_stream::stream;
use futures::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::{json, Value};
use std::pin::Pin;

use crate::agent::message::{ContentBlock, Conversation, Role};
use crate::agent::provider::{
    LlmProvider, ProviderConfig, StreamEvent, TokenUsage,
};
use crate::agent::provider::client::ApiClient;

pub struct GoogleProvider {
    config: ProviderConfig,
    client: ApiClient,
}

impl GoogleProvider {
    pub fn new(config: ProviderConfig) -> Self {
        Self {
            config,
            client: ApiClient::new(),
        }
    }

    fn build_request(&self, conversation: &Conversation, tools: &[Value]) -> Value {
        // Google uses "contents" with "parts", and role is "user"/"model"
        let contents: Vec<Value> = conversation
            .user_messages()
            .map(|msg| {
                let role = match msg.role {
                    Role::User => "user",
                    Role::Assistant => "model",
                    Role::System => "user",
                };

                let parts: Vec<Value> = msg
                    .content
                    .iter()
                    .flat_map(|block| match block {
                        ContentBlock::Text { text } => {
                            vec![json!({"text": text})]
                        }
                        ContentBlock::ToolUse { name, input, .. } => {
                            vec![json!({
                                "functionCall": {
                                    "name": name,
                                    "args": input
                                }
                            })]
                        }
                        ContentBlock::ToolResult {
                            tool_use_id: _,
                            content,
                            is_error: _,
                        } => content
                            .iter()
                            .filter_map(|c| {
                                if let ContentBlock::Text { text } = c {
                                    Some(json!({"text": text}))
                                } else {
                                    None
                                }
                            })
                            .collect(),
                    })
                    .collect();

                json!({"role": role, "parts": parts})
            })
            .collect();

        // System instruction as a separate field
        let mut body = json!({"contents": contents});

        if let Some(sys) = conversation.system_prompt() {
            body["systemInstruction"] = json!({
                "parts": [{"text": sys}]
            });
        }

        if !tools.is_empty() {
            body["tools"] = json!([{"functionDeclarations": tools}]);
        }

        body
    }

    fn effective_url(&self) -> String {
        let base = if let Some(proxy) = &self.config.dev_proxy {
            proxy.url.trim_end_matches('/').to_string()
        } else {
            self.config.base_url.trim_end_matches('/').to_string()
        };
        // Google AI Studio endpoint for streaming
        format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            base, self.config.model_id
        )
    }

    fn extra_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(proxy) = &self.config.dev_proxy {
            proxy.apply_headers(&mut headers);
        }
        headers
    }
}

// ─── SSE parsing helpers ──────────────────────────────────────────────────────

/// Parse a single Google AI Studio SSE data line.
/// Returns zero or more `StreamEvent`s produced by this chunk.
pub fn parse_google_line(line: &str) -> Vec<StreamEvent> {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let mut events = vec![];

    // Check for usage metadata
    if let Some(usage) = v.get("usageMetadata") {
        let input = usage
            .get("promptTokenCount")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u32;
        let output = usage
            .get("candidatesTokenCount")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u32;
        if input > 0 || output > 0 {
            events.push(StreamEvent::Usage(TokenUsage { input, output }));
        }
    }

    // Parse candidates
    if let Some(candidates) = v.get("candidates").and_then(|c| c.as_array()) {
        for candidate in candidates {
            if let Some(parts) = candidate
                .pointer("/content/parts")
                .and_then(|p| p.as_array())
            {
                for part in parts {
                    if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                        if !text.is_empty() {
                            events.push(StreamEvent::Delta(ContentBlock::Text {
                                text: text.to_string(),
                            }));
                        }
                    } else if let Some(fc) = part.get("functionCall") {
                        let name = fc
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string();
                        let input = fc.get("args").cloned().unwrap_or(Value::Null);
                        // Google doesn't provide a tool use id in streaming; generate a placeholder
                        let id = format!("google_fc_{}", name);
                        events.push(StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }));
                    }
                }
            }

            // Check finish reason for end of stream signal
            if let Some(reason) = candidate
                .get("finishReason")
                .and_then(|r| r.as_str())
                && !reason.is_empty()
                && reason != "FINISH_REASON_UNSPECIFIED"
            {
                // Will emit Done after the loop
            }
        }

        // If there are candidates with a finishReason, signal done
        let has_finish = candidates.iter().any(|c| {
            c.get("finishReason")
                .and_then(|r| r.as_str())
                .map(|r| !r.is_empty() && r != "FINISH_REASON_UNSPECIFIED")
                .unwrap_or(false)
        });
        if has_finish {
            events.push(StreamEvent::Done);
        }
    }

    events
}

impl LlmProvider for GoogleProvider {
    fn name(&self) -> &str {
        "google"
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
                    let mut emitted_done = false;
                    let mut raw_stream = raw;
                    while let Some(result) = raw_stream.next().await {
                        match result {
                            Err(e) => { yield Err(e); return; }
                            Ok(line) => {
                                for event in parse_google_line(&line) {
                                    let is_done = matches!(event, StreamEvent::Done);
                                    yield Ok(event);
                                    if is_done {
                                        emitted_done = true;
                                    }
                                }
                            }
                        }
                    }
                    if !emitted_done {
                        yield Ok(StreamEvent::Done);
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
    fn test_parse_text_part() {
        let line = r#"{"candidates":[{"content":{"parts":[{"text":"Hello world"}],"role":"model"},"finishReason":"STOP","index":0}]}"#;
        let events = parse_google_line(line);
        // Should have text delta and done
        let text_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Delta(ContentBlock::Text { .. })))
            .collect();
        assert_eq!(text_events.len(), 1);
        match &text_events[0] {
            StreamEvent::Delta(ContentBlock::Text { text }) => assert_eq!(text, "Hello world"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn test_parse_function_call() {
        let line = r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"shell","args":{"cmd":"ls"}}}],"role":"model"},"finishReason":"STOP"}]}"#;
        let events = parse_google_line(line);
        let tool_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Delta(ContentBlock::ToolUse { .. })))
            .collect();
        assert_eq!(tool_events.len(), 1);
        match &tool_events[0] {
            StreamEvent::Delta(ContentBlock::ToolUse { name, input, .. }) => {
                assert_eq!(name, "shell");
                assert_eq!(input["cmd"], "ls");
            }
            _ => panic!("expected tool use"),
        }
    }

    #[test]
    fn test_parse_usage_metadata() {
        let line = r#"{"candidates":[{"content":{"parts":[],"role":"model"},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":15,"candidatesTokenCount":30,"totalTokenCount":45}}"#;
        let events = parse_google_line(line);
        let usage_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Usage(_)))
            .collect();
        assert_eq!(usage_events.len(), 1);
        match &usage_events[0] {
            StreamEvent::Usage(u) => {
                assert_eq!(u.input, 15);
                assert_eq!(u.output, 30);
            }
            _ => panic!("expected usage"),
        }
    }

    #[test]
    fn test_finish_reason_emits_done() {
        let line = r#"{"candidates":[{"content":{"parts":[],"role":"model"},"finishReason":"STOP"}]}"#;
        let events = parse_google_line(line);
        assert!(events.iter().any(|e| matches!(e, StreamEvent::Done)));
    }

    #[test]
    fn test_streaming_chunk_no_finish_no_done() {
        // Intermediate chunk without finishReason shouldn't emit Done
        let line = r#"{"candidates":[{"content":{"parts":[{"text":"hello"}],"role":"model"}}]}"#;
        let events = parse_google_line(line);
        assert!(!events.iter().any(|e| matches!(e, StreamEvent::Done)));
    }
}
