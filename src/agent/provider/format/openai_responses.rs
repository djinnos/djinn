use async_stream::stream;
use futures::StreamExt;
use reqwest::header::HeaderMap;
use serde::Deserialize;
use serde_json::{json, Value};
use std::pin::Pin;

use crate::agent::message::{ContentBlock, Conversation, Role};
use crate::agent::provider::client::ApiClient;
use crate::agent::provider::{LlmProvider, ProviderConfig, StreamEvent, TokenUsage};

// ─── Provider ─────────────────────────────────────────────────────────────────

pub struct OpenAIResponsesProvider {
    config: ProviderConfig,
    client: ApiClient,
}

impl OpenAIResponsesProvider {
    pub fn new(config: ProviderConfig) -> Self {
        Self {
            config,
            client: ApiClient::new(),
        }
    }

    fn build_request(&self, conversation: &Conversation, tools: &[Value]) -> Value {
        let mut input_items: Vec<Value> = Vec::new();
        let mut instructions: Option<String> = None;

        for msg in &conversation.messages {
            match msg.role {
                Role::System => {
                    // System messages become the top-level "instructions" field
                    let text = msg.text_content();
                    if !text.is_empty() {
                        match &mut instructions {
                            Some(existing) => {
                                existing.push_str("\n\n");
                                existing.push_str(&text);
                            }
                            None => {
                                instructions = Some(text);
                            }
                        }
                    }
                }
                Role::User => {
                    let mut text_items: Vec<Value> = Vec::new();

                    for block in &msg.content {
                        match block {
                            ContentBlock::Text { text } if !text.is_empty() => {
                                text_items.push(json!({"type": "input_text", "text": text}));
                            }
                            ContentBlock::ToolResult {
                                tool_use_id,
                                content,
                                is_error,
                            } => {
                                // Flush pending text
                                if !text_items.is_empty() {
                                    input_items.push(json!({
                                        "role": "user",
                                        "content": std::mem::take(&mut text_items)
                                    }));
                                }

                                let result_text: String = content
                                    .iter()
                                    .filter_map(|c| c.as_text())
                                    .collect::<Vec<_>>()
                                    .join("\n");

                                let output = if *is_error {
                                    format!("Error: {}", result_text)
                                } else {
                                    result_text
                                };

                                input_items.push(json!({
                                    "type": "function_call_output",
                                    "call_id": tool_use_id,
                                    "output": output
                                }));
                            }
                            _ => {}
                        }
                    }

                    if !text_items.is_empty() {
                        input_items.push(json!({
                            "role": "user",
                            "content": text_items
                        }));
                    }
                }
                Role::Assistant => {
                    let mut text_items: Vec<Value> = Vec::new();

                    for block in &msg.content {
                        match block {
                            ContentBlock::Text { text } if !text.is_empty() => {
                                text_items
                                    .push(json!({"type": "output_text", "text": text}));
                            }
                            ContentBlock::ToolUse { id, name, input } => {
                                // Flush pending text
                                if !text_items.is_empty() {
                                    input_items.push(json!({
                                        "role": "assistant",
                                        "content": std::mem::take(&mut text_items)
                                    }));
                                }

                                let arguments_str = serde_json::to_string(input)
                                    .unwrap_or_else(|_| "{}".to_string());

                                input_items.push(json!({
                                    "type": "function_call",
                                    "call_id": id,
                                    "name": name,
                                    "arguments": arguments_str
                                }));
                            }
                            _ => {}
                        }
                    }

                    if !text_items.is_empty() {
                        input_items.push(json!({
                            "role": "assistant",
                            "content": text_items
                        }));
                    }
                }
            }
        }

        let mut body = json!({
            "model": self.config.model_id,
            "input": input_items,
            "instructions": instructions.unwrap_or_default(),
            "store": false,
            "stream": true,
        });

        if !tools.is_empty() {
            let tools_spec: Vec<Value> = tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "name": tool["function"]["name"],
                        "description": tool["function"]["description"],
                        "parameters": tool["function"]["parameters"],
                    })
                })
                .collect();
            body["tools"] = json!(tools_spec);
        }

        body
    }

    fn effective_url(&self) -> String {
        if let Some(proxy) = &self.config.dev_proxy {
            format!("{}/responses", proxy.url.trim_end_matches('/'))
        } else {
            format!("{}/responses", self.config.base_url.trim_end_matches('/'))
        }
    }

    fn extra_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(proxy) = &self.config.dev_proxy {
            proxy.apply_headers(&mut headers);
        }
        headers
    }
}

// ─── SSE parsing ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ResponseMetadata {
    #[allow(dead_code)]
    id: String,
    #[allow(dead_code)]
    model: String,
    output: Vec<OutputItemInfo>,
    usage: Option<ResponseUsage>,
}

#[derive(Debug, Deserialize)]
struct ResponseUsage {
    input_tokens: u32,
    output_tokens: u32,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OutputItemInfo {
    Reasoning {
        #[allow(dead_code)]
        id: String,
    },
    Message {
        #[allow(dead_code)]
        id: String,
        #[allow(dead_code)]
        content: Vec<ContentPart>,
    },
    FunctionCall {
        #[allow(dead_code)]
        id: String,
        call_id: String,
        name: String,
        arguments: String,
    },
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentPart {
    OutputText {
        #[allow(dead_code)]
        text: String,
    },
    #[serde(other)]
    Unknown,
}

/// Parsed SSE event from the Responses API stream.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ResponsesStreamEvent {
    #[serde(rename = "response.created")]
    ResponseCreated {
        #[allow(dead_code)]
        response: ResponseMetadata,
    },
    #[serde(rename = "response.in_progress")]
    ResponseInProgress {
        #[allow(dead_code)]
        response: ResponseMetadata,
    },
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        delta: String,
    },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone {
        item: OutputItemInfo,
    },
    #[serde(rename = "response.completed")]
    ResponseCompleted {
        response: ResponseMetadata,
    },
    #[serde(rename = "response.failed")]
    ResponseFailed {
        error: Value,
    },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta {},
    #[serde(rename = "response.function_call_arguments.done")]
    FunctionCallArgumentsDone {},
    #[serde(rename = "response.output_text.done")]
    OutputTextDone {},
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded {},
    #[serde(rename = "response.content_part.added")]
    ContentPartAdded {},
    #[serde(rename = "response.content_part.done")]
    ContentPartDone {},
    #[serde(rename = "error")]
    Error {
        error: Value,
    },
    #[serde(rename = "keepalive")]
    Keepalive {},
}

const KNOWN_EVENT_TYPES: &[&str] = &[
    "response.created",
    "response.in_progress",
    "response.output_item.added",
    "response.content_part.added",
    "response.output_text.delta",
    "response.output_item.done",
    "response.content_part.done",
    "response.output_text.done",
    "response.completed",
    "response.failed",
    "response.function_call_arguments.delta",
    "response.function_call_arguments.done",
    "error",
    "keepalive",
];

fn parse_stream_event(data: &str) -> anyhow::Result<Option<ResponsesStreamEvent>> {
    let raw: Value = serde_json::from_str(data)?;

    let Some(event_type) = raw.get("type").and_then(Value::as_str) else {
        return Ok(None);
    };

    if !KNOWN_EVENT_TYPES.contains(&event_type) {
        return Ok(None);
    }

    let event: ResponsesStreamEvent = serde_json::from_value(raw)?;
    Ok(Some(event))
}

/// Parse a single SSE data line from the OpenAI Responses streaming API.
///
/// `accumulated_items` collects OutputItemDone items across the stream.
/// Returns zero or more `StreamEvent`s.
fn parse_responses_line(
    line: &str,
    accumulated_items: &mut Vec<OutputItemInfo>,
) -> Vec<StreamEvent> {
    let event = match parse_stream_event(line) {
        Ok(Some(e)) => e,
        Ok(None) => return vec![],
        Err(e) => {
            tracing::debug!(error = %e, "failed to parse Responses SSE event");
            return vec![];
        }
    };

    match event {
        ResponsesStreamEvent::OutputTextDelta { delta } => {
            if delta.is_empty() {
                vec![]
            } else {
                vec![StreamEvent::Delta(ContentBlock::Text { text: delta })]
            }
        }
        ResponsesStreamEvent::OutputItemDone { item } => {
            accumulated_items.push(item);
            vec![]
        }
        ResponsesStreamEvent::ResponseCompleted { response } => {
            let mut events = Vec::new();

            // Emit tool uses from accumulated items (text was already streamed as deltas)
            let final_items = if response.output.is_empty() {
                accumulated_items.as_slice()
            } else {
                response.output.as_slice()
            };

            for item in final_items {
                if let OutputItemInfo::FunctionCall {
                    call_id,
                    name,
                    arguments,
                    ..
                } = item
                {
                    let input: Value = if arguments.is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(arguments).unwrap_or(json!({}))
                    };
                    events.push(StreamEvent::Delta(ContentBlock::ToolUse {
                        id: call_id.clone(),
                        name: name.clone(),
                        input,
                    }));
                }
            }

            // Emit usage
            if let Some(usage) = response.usage {
                events.push(StreamEvent::Usage(TokenUsage {
                    input: usage.input_tokens,
                    output: usage.output_tokens,
                }));
            }

            events
        }
        ResponsesStreamEvent::ResponseFailed { error } => {
            tracing::error!(?error, "Responses API failed");
            vec![]
        }
        ResponsesStreamEvent::Error { error } => {
            tracing::error!(?error, "Responses API error");
            vec![]
        }
        // Ignore all other event types
        _ => vec![],
    }
}

// ─── LlmProvider impl ────────────────────────────────────────────────────────

impl LlmProvider for OpenAIResponsesProvider {
    fn name(&self) -> &str {
        "openai_responses"
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
                    let mut accumulated_items: Vec<OutputItemInfo> = Vec::new();
                    let mut raw_stream = raw;
                    while let Some(result) = raw_stream.next().await {
                        match result {
                            Err(e) => { yield Err(e); return; }
                            Ok(line) => {
                                for event in parse_responses_line(&line, &mut accumulated_items) {
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
    use crate::agent::message::Message;
    use crate::agent::provider::{AuthMethod, FormatFamily};

    fn test_provider() -> OpenAIResponsesProvider {
        OpenAIResponsesProvider::new(ProviderConfig {
            base_url: "https://api.openai.com".to_string(),
            auth: AuthMethod::BearerToken("test".to_string()),
            format_family: FormatFamily::OpenAIResponses,
            model_id: "gpt-5.1-codex".to_string(),
            context_window: 128000,
            dev_proxy: None,
        })
    }

    #[test]
    fn test_build_request_simple() {
        let provider = test_provider();
        let mut conv = Conversation::new();
        conv.push(Message::system("You are helpful."));
        conv.push(Message::user("Hello"));

        let req = provider.build_request(&conv, &[]);
        assert_eq!(req["model"], "gpt-5.1-codex");
        assert_eq!(req["store"], false);
        assert_eq!(req["stream"], true);

        // System message becomes top-level instructions
        assert_eq!(req["instructions"], "You are helpful.");

        let input = req["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        // User message
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][0]["text"], "Hello");
    }

    #[test]
    fn test_build_request_tool_use_and_result() {
        let provider = test_provider();
        let mut conv = Conversation::new();
        conv.push(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "Let me check.".into(),
                },
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "bash".into(),
                    input: json!({"cmd": "ls"}),
                },
            ],
            metadata: None,
        });
        conv.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: vec![ContentBlock::text("file1.txt")],
                is_error: false,
            }],
            metadata: None,
        });

        let req = provider.build_request(&conv, &[]);
        let input = req["input"].as_array().unwrap();

        // Should be: assistant text, function_call, function_call_output
        let types: Vec<&str> = input
            .iter()
            .map(|item| {
                item.get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or_else(|| item["role"].as_str().unwrap())
            })
            .collect();
        assert_eq!(types, vec!["assistant", "function_call", "function_call_output"]);

        // Verify function_call fields
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[1]["name"], "bash");

        // Verify function_call_output
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["output"], "file1.txt");
    }

    #[test]
    fn test_build_request_error_tool_result() {
        let provider = test_provider();
        let mut conv = Conversation::new();
        conv.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: vec![ContentBlock::text("not found")],
                is_error: true,
            }],
            metadata: None,
        });

        let req = provider.build_request(&conv, &[]);
        let input = req["input"].as_array().unwrap();
        assert_eq!(input[0]["output"], "Error: not found");
    }

    #[test]
    fn test_parse_text_delta() {
        let line = r#"{"type":"response.output_text.delta","sequence_number":2,"item_id":"msg_1","output_index":0,"content_index":0,"delta":"Hello"}"#;
        let mut acc = Vec::new();
        let events = parse_responses_line(line, &mut acc);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Delta(ContentBlock::Text { text }) => assert_eq!(text, "Hello"),
            _ => panic!("expected text delta"),
        }
    }

    #[test]
    fn test_parse_empty_delta_skipped() {
        let line = r#"{"type":"response.output_text.delta","sequence_number":2,"item_id":"msg_1","output_index":0,"content_index":0,"delta":""}"#;
        let mut acc = Vec::new();
        let events = parse_responses_line(line, &mut acc);
        assert!(events.is_empty());
    }

    #[test]
    fn test_parse_completed_with_function_call() {
        let line = r#"{"type":"response.completed","sequence_number":10,"response":{"id":"resp_1","object":"response","created_at":1737368310,"status":"completed","model":"gpt-5.1-codex","output":[{"type":"function_call","id":"fc_1","status":"completed","call_id":"call_abc","name":"bash","arguments":"{\"cmd\":\"ls\"}"}],"usage":{"input_tokens":100,"output_tokens":50}}}"#;
        let mut acc = Vec::new();
        let events = parse_responses_line(line, &mut acc);
        // Should have tool use + usage
        assert_eq!(events.len(), 2);
        match &events[0] {
            StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }) => {
                assert_eq!(id, "call_abc");
                assert_eq!(name, "bash");
                assert_eq!(input["cmd"], "ls");
            }
            _ => panic!("expected tool use"),
        }
        match &events[1] {
            StreamEvent::Usage(u) => {
                assert_eq!(u.input, 100);
                assert_eq!(u.output, 50);
            }
            _ => panic!("expected usage"),
        }
    }

    #[test]
    fn test_parse_keepalive_ignored() {
        let line = r#"{"type":"keepalive"}"#;
        let mut acc = Vec::new();
        let events = parse_responses_line(line, &mut acc);
        assert!(events.is_empty());
    }

    #[test]
    fn test_parse_unknown_event_ignored() {
        let line = r#"{"type":"response.some_future_event","data":"foo"}"#;
        let mut acc = Vec::new();
        let events = parse_responses_line(line, &mut acc);
        assert!(events.is_empty());
    }

    #[test]
    fn test_build_request_with_tools() {
        let provider = test_provider();
        let mut conv = Conversation::new();
        conv.push(Message::user("list files"));

        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Run a shell command",
                "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}
            }
        })];

        let req = provider.build_request(&conv, &tools);
        let tools_arr = req["tools"].as_array().unwrap();
        assert_eq!(tools_arr.len(), 1);
        assert_eq!(tools_arr[0]["type"], "function");
        assert_eq!(tools_arr[0]["name"], "bash");
        assert_eq!(tools_arr[0]["description"], "Run a shell command");
    }

    #[test]
    fn test_effective_url() {
        let provider = test_provider();
        assert_eq!(provider.effective_url(), "https://api.openai.com/responses");
    }
}
