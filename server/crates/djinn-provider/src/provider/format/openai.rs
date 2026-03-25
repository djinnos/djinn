use async_stream::stream;
use futures::StreamExt;
use reqwest::header::HeaderMap;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::pin::Pin;

use crate::message::{ContentBlock, Conversation, Role};
use crate::provider::client::ApiClient;
use crate::provider::{LlmProvider, ProviderConfig, StreamEvent, TokenUsage, ToolChoice};

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

    fn build_request(
        &self,
        conversation: &Conversation,
        tools: &[Value],
        tool_choice: Option<ToolChoice>,
    ) -> Value {
        // Convert messages — OpenAI Chat Completions format requires:
        // - Assistant tool calls in a separate `tool_calls` field (NOT in content)
        // - Tool results as standalone messages with role "tool"
        let mut messages: Vec<Value> = Vec::new();

        for msg in &conversation.messages {
            let role = match msg.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
            };

            // Separate content blocks by type
            let mut text_blocks: Vec<Value> = Vec::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            let mut tool_results: Vec<Value> = Vec::new();

            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        text_blocks.push(json!({"type": "text", "text": text}));
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": {"name": name, "arguments": input.to_string()}
                        }));
                    }
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
                        tool_results.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id,
                            "content": text
                        }));
                    }
                }
            }

            if !tool_results.is_empty() {
                // Tool results become standalone messages with role "tool"
                for tr in tool_results {
                    messages.push(tr);
                }
            } else if !tool_calls.is_empty() {
                // Assistant message with tool_calls
                let mut assistant_msg = json!({"role": role});
                if !text_blocks.is_empty() {
                    let text = text_blocks
                        .iter()
                        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("");
                    assistant_msg["content"] = json!(text);
                } else {
                    assistant_msg["content"] = Value::Null;
                }
                assistant_msg["tool_calls"] = json!(tool_calls);
                messages.push(assistant_msg);
            } else if text_blocks.len() == 1 {
                // Simple single text — use string content form
                let text = text_blocks[0]
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                messages.push(json!({"role": role, "content": text}));
            } else {
                // Multiple text blocks — use array content form
                messages.push(json!({"role": role, "content": text_blocks}));
            }
        }

        let mut body = json!({
            "model": self.config.model_id,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true}
        });

        if !tools.is_empty() {
            // Convert RMCP tool format to OpenAI function-calling format.
            // RMCP: {"name", "description", "inputSchema"}
            // OpenAI: {"type": "function", "function": {"name", "description", "parameters"}}
            let openai_tools: Vec<Value> = tools
                .iter()
                .map(|t| {
                    if t.get("type").is_some() && t.get("function").is_some() {
                        // Already in OpenAI format.
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
                .collect();
            body["tools"] = json!(openai_tools);

            match tool_choice.unwrap_or(ToolChoice::Auto) {
                ToolChoice::Auto => {}
                ToolChoice::Required => body["tool_choice"] = json!("required"),
                ToolChoice::None => body["tool_choice"] = json!("none"),
            }
        }

        if let Some(session_affinity_key) = &self.config.session_affinity_key
            && is_fireworks_base_url(&self.config.base_url)
        {
            body["user"] = json!(session_affinity_key);
        }

        body
    }

    fn effective_url(&self) -> String {
        format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        )
    }

    fn extra_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(session_affinity_key) = &self.config.session_affinity_key
            && is_fireworks_base_url(&self.config.base_url)
            && let Ok(value) = reqwest::header::HeaderValue::from_str(session_affinity_key)
        {
            headers.insert("x-session-affinity", value);
        }
        headers
    }
}

fn is_fireworks_base_url(base_url: &str) -> bool {
    base_url.contains("fireworks.ai")
}

// ─── Tool conversion ─────────────────────────────────────────────────────────

/// OpenAI requires `"properties"` on object schemas. Ensure it exists.
pub(super) fn ensure_object_properties(mut schema: Value) -> Value {
    if let Some(obj) = schema.as_object_mut()
        && obj.get("type").and_then(|v| v.as_str()) == Some("object")
    {
        obj.entry("properties").or_insert_with(|| json!({}));
    }
    schema
}

// ─── SSE parsing helpers ──────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct DeltaFunction {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct DeltaToolCall {
    index: Option<u32>,
    id: Option<String>,
    function: Option<DeltaFunction>,
}

#[derive(Deserialize, Default)]
struct Delta {
    content: Option<String>,
    /// Chain-of-thought tokens (Kimi K2.5, DeepSeek-R1, etc.)
    reasoning_content: Option<String>,
    /// Chain-of-thought tokens (GLM-4.7, Minimax, etc.)
    reasoning_details: Option<String>,
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
    tool_acc: &mut BTreeMap<u32, (String, String, String)>, // index -> (id, name, arguments)
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

        // Reasoning/thinking content (e.g. Kimi K2.5, DeepSeek-R1, GLM-4.7)
        let thinking = delta
            .reasoning_content
            .or(delta.reasoning_details)
            .filter(|s| !s.is_empty());
        if let Some(text) = thinking {
            events.push(StreamEvent::Thinking(text));
        }

        // Text content
        if let Some(text) = delta.content
            && !text.is_empty()
        {
            events.push(StreamEvent::Delta(ContentBlock::Text { text }));
        }

        // Tool calls — accumulate across chunks, keyed by index
        if let Some(tool_calls) = delta.tool_calls {
            for tc in tool_calls {
                let idx = tc.index.unwrap_or(0);
                let func = tc.function.unwrap_or_default();
                if let Some(entry) = tool_acc.get_mut(&idx) {
                    // Existing entry — append fragments
                    if let Some(id) = tc.id
                        && !id.is_empty()
                    {
                        entry.0 = id;
                    }
                    if let Some(name) = func.name
                        && !name.is_empty()
                    {
                        entry.1 = name;
                    }
                    if let Some(frag) = func.arguments {
                        entry.2.push_str(&frag);
                    }
                } else {
                    // New entry for this index
                    tool_acc.insert(idx, (
                        tc.id.unwrap_or_default(),
                        func.name.unwrap_or_default(),
                        func.arguments.unwrap_or_default(),
                    ));
                }
            }
        }

        // On finish_reason="tool_calls", emit all accumulated tool uses
        if choice
            .finish_reason
            .as_deref()
            .map(|r| r == "tool_calls")
            .unwrap_or(false)
        {
            // Drain all entries sorted by index (BTreeMap iterates in order)
            let entries: Vec<_> = tool_acc.keys().cloned().collect();
            for idx in entries {
                if let Some((id, name, args)) = tool_acc.remove(&idx) {
                    let input = serde_json::from_str(&args).unwrap_or(Value::Null);
                    events.push(StreamEvent::Delta(ContentBlock::ToolUse {
                        id,
                        name,
                        input,
                    }));
                }
            }
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
            if let Some(text) = msg.get("content").and_then(|c| c.as_str())
                && !text.is_empty()
            {
                events.push(StreamEvent::Delta(ContentBlock::Text {
                    text: text.to_string(),
                }));
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
                    events.push(StreamEvent::Delta(ContentBlock::ToolUse {
                        id,
                        name,
                        input,
                    }));
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
        let auth = self.config.auth.clone();
        let extra_headers = self.extra_headers();

        Box::pin(async move {
            let raw = self.client.stream_sse(&url, body, &auth, extra_headers);
            let out: Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>> =
                Box::pin(stream! {
                    let mut tool_acc: BTreeMap<u32, (String, String, String)> = BTreeMap::new();
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
    use crate::message::Message;
    use crate::provider::{AuthMethod, FormatFamily, ProviderCapabilities, ProviderConfig};

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
        }
    }

    #[test]
    fn test_build_request_sets_fireworks_user_field() {
        let provider = OpenAIProvider::new(ProviderConfig {
            base_url: "https://api.fireworks.ai/inference/v1".to_string(),
            auth: AuthMethod::BearerToken("test".to_string()),
            format_family: FormatFamily::OpenAI,
            model_id: "accounts/fireworks/models/deepseek-v3p2".to_string(),
            context_window: 128_000,
            telemetry: None,
            session_affinity_key: Some("session-123".to_string()),
            provider_headers: Default::default(),
            capabilities: ProviderCapabilities::default(),
        });
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));

        let req = provider.build_request(&conv, &[], None);
        assert_eq!(req["user"], "session-123");
    }

    #[test]
    fn test_extra_headers_sets_fireworks_session_affinity() {
        let provider = OpenAIProvider::new(ProviderConfig {
            base_url: "https://api.fireworks.ai/inference/v1".to_string(),
            auth: AuthMethod::BearerToken("test".to_string()),
            format_family: FormatFamily::OpenAI,
            model_id: "accounts/fireworks/models/deepseek-v3p2".to_string(),
            context_window: 128_000,
            telemetry: None,
            session_affinity_key: Some("session-123".to_string()),
            provider_headers: Default::default(),
            capabilities: ProviderCapabilities::default(),
        });

        let headers = provider.extra_headers();
        assert_eq!(
            headers
                .get("x-session-affinity")
                .and_then(|v| v.to_str().ok()),
            Some("session-123")
        );
    }

    #[test]
    fn test_non_fireworks_provider_omits_session_affinity() {
        let provider = OpenAIProvider::new(test_openai_config());
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));

        let req = provider.build_request(&conv, &[], None);
        assert!(req.get("user").is_none());
        assert!(provider.extra_headers().get("x-session-affinity").is_none());
    }

    #[test]
    fn test_build_request_sets_required_tool_choice_when_tools_present() {
        let provider = OpenAIProvider::new(test_openai_config());
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));
        let tools = vec![json!({
            "name": "shell",
            "description": "Run shell",
            "inputSchema": {"type": "object"}
        })];

        let req = provider.build_request(&conv, &tools, Some(ToolChoice::Required));
        assert_eq!(req["tool_choice"], "required");
    }

    #[test]
    fn test_build_request_omits_tool_choice_when_tools_empty() {
        let provider = OpenAIProvider::new(test_openai_config());
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));

        let req = provider.build_request(&conv, &[], Some(ToolChoice::Required));
        assert!(req.get("tool_choice").is_none());
    }

    #[test]
    fn test_parse_text_delta() {
        let line = r#"{"choices":[{"delta":{"content":"hello"},"finish_reason":null,"index":0}]}"#;
        let mut acc = BTreeMap::new();
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
        let mut acc = BTreeMap::new();
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

        let mut acc = BTreeMap::new();
        let e1 = parse_openai_line(line1, &mut acc);
        assert!(e1.is_empty(), "no events on first tool chunk");
        assert!(!acc.is_empty());

        let e2 = parse_openai_line(line2, &mut acc);
        assert!(e2.is_empty(), "no events while accumulating");

        let e3 = parse_openai_line(line3, &mut acc);
        assert_eq!(e3.len(), 1);
        match &e3[0] {
            StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }) => {
                assert_eq!(id.as_str(), "call_abc");
                assert_eq!(name.as_str(), "shell");
                assert_eq!(input["cmd"].as_str(), Some("ls"));
            }
            _ => panic!("expected tool use"),
        }
        assert!(acc.is_empty(), "accumulator cleared after emit");
    }

    #[test]
    fn test_build_request_keeps_system_message_first() {
        let provider = OpenAIProvider::new(test_openai_config());
        let mut conv = Conversation::default();
        conv.push(crate::message::Message::system("system prompt"));
        conv.push(crate::message::Message::user("first user"));
        conv.push(crate::message::Message::assistant("first assistant"));
        conv.push(crate::message::Message::user("second user"));

        let req = provider.build_request(&conv, &[], None);
        let messages = req["messages"].as_array().expect("messages array");
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "system prompt");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "first user");
    }

    #[test]
    fn test_parse_invalid_json_ignored() {
        let mut acc = BTreeMap::new();
        let events = parse_openai_line("{not-json", &mut acc);
        assert!(events.is_empty());
        assert!(acc.is_empty());
    }

    #[test]
    fn test_parse_missing_choices_with_usage_only() {
        let line = r#"{"usage":{"prompt_tokens":7}}"#;
        let mut acc = BTreeMap::new();
        let events = parse_openai_line(line, &mut acc);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Usage(u) => {
                assert_eq!(u.input, 7);
                assert_eq!(u.output, 0);
            }
            _ => panic!("expected usage"),
        }
    }
    #[test]
    fn test_parse_reasoning_content() {
        let line = r#"{"choices":[{"delta":{"reasoning_content":"let me think..."},"finish_reason":null,"index":0}]}"#;
        let mut acc = BTreeMap::new();
        let events = parse_openai_line(line, &mut acc);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Thinking(text) => assert_eq!(text, "let me think..."),
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_reasoning_details() {
        let line = r#"{"choices":[{"delta":{"reasoning_details":"step 1: analyze"},"finish_reason":null,"index":0}]}"#;
        let mut acc = BTreeMap::new();
        let events = parse_openai_line(line, &mut acc);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Thinking(text) => assert_eq!(text, "step 1: analyze"),
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_reasoning_content_with_text() {
        // Some models send both reasoning and content in same chunk
        let line = r#"{"choices":[{"delta":{"reasoning_content":"thinking","content":"hello"},"finish_reason":null,"index":0}]}"#;
        let mut acc = BTreeMap::new();
        let events = parse_openai_line(line, &mut acc);
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], StreamEvent::Thinking(t) if t == "thinking"));
        assert!(
            matches!(&events[1], StreamEvent::Delta(ContentBlock::Text { text }) if text == "hello")
        );
    }

    #[test]
    fn test_parse_empty_reasoning_content_skipped() {
        let line =
            r#"{"choices":[{"delta":{"reasoning_content":""},"finish_reason":null,"index":0}]}"#;
        let mut acc = BTreeMap::new();
        let events = parse_openai_line(line, &mut acc);
        assert!(events.is_empty());
    }

    #[test]
    fn test_parse_done_sentinel_ignored() {
        let mut acc = BTreeMap::new();
        let events = parse_openai_line("[DONE]", &mut acc);
        assert!(events.is_empty());
        assert!(acc.is_empty());
    }

    #[test]
    fn test_parse_parallel_tool_calls() {
        let mut acc = BTreeMap::new();

        // Chunk 1: two tool calls start
        let line1 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"shell","arguments":""}},{"index":1,"id":"call_2","function":{"name":"read","arguments":""}}]},"finish_reason":null}]}"#;
        let e1 = parse_openai_line(line1, &mut acc);
        assert!(e1.is_empty(), "no events on first tool chunk");
        assert_eq!(acc.len(), 2);

        // Chunk 2: arguments for index 0
        let line2 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\":\"ls\"}"}}]},"finish_reason":null}]}"#;
        let e2 = parse_openai_line(line2, &mut acc);
        assert!(e2.is_empty());

        // Chunk 3: arguments for index 1
        let line3 = r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"path\":\"/tmp\"}"}}]},"finish_reason":null}]}"#;
        let e3 = parse_openai_line(line3, &mut acc);
        assert!(e3.is_empty());

        // Chunk 4: finish
        let line4 = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;
        let e4 = parse_openai_line(line4, &mut acc);
        assert_eq!(e4.len(), 2);

        match &e4[0] {
            StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }) => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "shell");
                assert_eq!(input["cmd"].as_str(), Some("ls"));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        match &e4[1] {
            StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }) => {
                assert_eq!(id, "call_2");
                assert_eq!(name, "read");
                assert_eq!(input["path"].as_str(), Some("/tmp"));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        assert!(acc.is_empty(), "accumulator cleared after emit");
    }

    #[test]
    fn test_parse_parallel_tool_calls_with_thinking() {
        let mut acc = BTreeMap::new();

        // Chunk 1: reasoning + two tool calls start
        let line1 = r#"{"choices":[{"delta":{"reasoning_content":"Let me run these commands","tool_calls":[{"index":0,"id":"call_1","function":{"name":"shell","arguments":""}},{"index":1,"id":"call_2","function":{"name":"read","arguments":""}}]},"finish_reason":null}]}"#;
        let e1 = parse_openai_line(line1, &mut acc);
        assert_eq!(e1.len(), 1);
        match &e1[0] {
            StreamEvent::Thinking(text) => assert_eq!(text, "Let me run these commands"),
            other => panic!("expected Thinking, got {other:?}"),
        }

        // Chunk 2: arguments for index 0
        let line2 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\":\"ls\"}"}}]},"finish_reason":null}]}"#;
        parse_openai_line(line2, &mut acc);

        // Chunk 3: arguments for index 1
        let line3 = r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"path\":\"/tmp\"}"}}]},"finish_reason":null}]}"#;
        parse_openai_line(line3, &mut acc);

        // Chunk 4: finish
        let line4 = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;
        let e4 = parse_openai_line(line4, &mut acc);
        assert_eq!(e4.len(), 2);

        match &e4[0] {
            StreamEvent::Delta(ContentBlock::ToolUse { id, name, .. }) => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "shell");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        match &e4[1] {
            StreamEvent::Delta(ContentBlock::ToolUse { id, name, .. }) => {
                assert_eq!(id, "call_2");
                assert_eq!(name, "read");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_thinking_before_tool_call() {
        let mut acc = BTreeMap::new();

        // Chunk 1: reasoning token
        let line1 = r#"{"choices":[{"delta":{"reasoning_content":"thinking step 1"},"finish_reason":null}]}"#;
        let e1 = parse_openai_line(line1, &mut acc);
        assert_eq!(e1.len(), 1);
        assert!(matches!(&e1[0], StreamEvent::Thinking(t) if t == "thinking step 1"));

        // Chunk 2: more reasoning
        let line2 = r#"{"choices":[{"delta":{"reasoning_content":"thinking step 2"},"finish_reason":null}]}"#;
        let e2 = parse_openai_line(line2, &mut acc);
        assert_eq!(e2.len(), 1);
        assert!(matches!(&e2[0], StreamEvent::Thinking(t) if t == "thinking step 2"));

        // Chunk 3: tool call
        let line3 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"shell","arguments":"{\"cmd\":\"ls\"}"}}]},"finish_reason":null}]}"#;
        let e3 = parse_openai_line(line3, &mut acc);
        assert!(e3.is_empty());

        // Chunk 4: finish
        let line4 = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;
        let e4 = parse_openai_line(line4, &mut acc);
        assert_eq!(e4.len(), 1);
        match &e4[0] {
            StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }) => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "shell");
                assert_eq!(input["cmd"].as_str(), Some("ls"));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }
}
