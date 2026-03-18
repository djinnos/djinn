use async_stream::stream;
use futures::StreamExt;
use reqwest::header::HeaderMap;
use serde::Deserialize;
use serde_json::{Value, json};
use std::pin::Pin;

use crate::message::{ContentBlock, Conversation};
use crate::provider::client::ApiClient;
use crate::provider::{LlmProvider, ProviderConfig, StreamEvent, TokenUsage, ToolChoice};

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

    fn build_request(
        &self,
        conversation: &Conversation,
        tools: &[Value],
        tool_choice: Option<ToolChoice>,
    ) -> Value {
        let (instructions, input_items) = conversation.to_openai_responses_input();

        let mut body = json!({
            "model": self.config.model_id,
            "input": input_items,
            "instructions": instructions.unwrap_or_default(),
            "store": false,
        });

        if self.config.capabilities.streaming {
            body["stream"] = json!(true);
        }

        if !tools.is_empty() {
            let tools_spec: Vec<Value> = tools
                .iter()
                .map(|tool| {
                    let name = tool
                        .get("name")
                        .or_else(|| tool.get("function").and_then(|f| f.get("name")));
                    let description = tool
                        .get("description")
                        .or_else(|| tool.get("function").and_then(|f| f.get("description")));
                    let parameters = tool
                        .get("inputSchema")
                        .or_else(|| tool.get("input_schema"))
                        .or_else(|| tool.get("function").and_then(|f| f.get("parameters")))
                        .cloned()
                        .map(super::openai::ensure_object_properties);
                    json!({
                        "type": "function",
                        "name": name,
                        "description": description,
                        "parameters": parameters,
                    })
                })
                .collect();
            body["tools"] = json!(tools_spec);

            match tool_choice.unwrap_or(ToolChoice::Auto) {
                ToolChoice::Auto => {}
                ToolChoice::Required => body["tool_choice"] = json!("required"),
                ToolChoice::None => body["tool_choice"] = json!("none"),
            }
        }

        body
    }

    fn effective_url(&self) -> String {
        format!("{}/responses", self.config.base_url.trim_end_matches('/'))
    }

    fn extra_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in &self.config.provider_headers {
            if let (Ok(n), Ok(v)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                reqwest::header::HeaderValue::from_str(value),
            ) {
                headers.insert(n, v);
            }
        }
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

// ─── SSE parsing ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ResponseMetadata {
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
    Reasoning {},
    Message {},
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
}

/// Parsed SSE event from the Responses API stream.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ResponsesStreamEvent {
    #[serde(rename = "response.created")]
    ResponseCreated {},
    #[serde(rename = "response.in_progress")]
    ResponseInProgress {},
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta { delta: String },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone { item: OutputItemInfo },
    #[serde(rename = "response.completed")]
    ResponseCompleted { response: ResponseMetadata },
    #[serde(rename = "response.failed")]
    ResponseFailed { error: Value },
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
    Error { error: Value },
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

/// Parsed line result: either zero or more stream events, or a provider error
/// that should be propagated through the stream.
enum ParsedLine {
    Events(Vec<StreamEvent>),
    ProviderError(String),
}

/// Extract a human-readable error message from an OpenAI error JSON value.
fn extract_error_message(error: &Value) -> String {
    // OpenAI errors: {"message": "...", "code": "..."}
    // or nested: {"error": {"message": "...", "code": "..."}}
    let msg = error
        .get("message")
        .or_else(|| error.get("error").and_then(|e| e.get("message")))
        .and_then(Value::as_str);
    let code = error
        .get("code")
        .or_else(|| error.get("error").and_then(|e| e.get("code")))
        .and_then(Value::as_str);
    match (code, msg) {
        (Some(c), Some(m)) => format!("{c}: {m}"),
        (None, Some(m)) => m.to_string(),
        (Some(c), None) => c.to_string(),
        (None, None) => error.to_string(),
    }
}

/// Parse a single SSE data line from the OpenAI Responses streaming API.
///
/// `accumulated_items` collects OutputItemDone items across the stream.
/// Returns zero or more `StreamEvent`s, or a provider error.
fn parse_responses_line(line: &str, accumulated_items: &mut Vec<OutputItemInfo>) -> ParsedLine {
    let event = match parse_stream_event(line) {
        Ok(Some(e)) => e,
        Ok(None) => return ParsedLine::Events(vec![]),
        Err(e) => {
            tracing::debug!(error = %e, "failed to parse Responses SSE event");
            return ParsedLine::Events(vec![]);
        }
    };

    match event {
        ResponsesStreamEvent::OutputTextDelta { delta } => {
            if delta.is_empty() {
                ParsedLine::Events(vec![])
            } else {
                ParsedLine::Events(vec![StreamEvent::Delta(ContentBlock::Text { text: delta })])
            }
        }
        ResponsesStreamEvent::OutputItemDone { item } => {
            accumulated_items.push(item);
            ParsedLine::Events(vec![])
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

            ParsedLine::Events(events)
        }
        ResponsesStreamEvent::ResponseFailed { error } => {
            let msg = extract_error_message(&error);
            tracing::error!(error = %msg, "Responses API failed");
            ParsedLine::ProviderError(msg)
        }
        ResponsesStreamEvent::Error { error } => {
            let msg = extract_error_message(&error);
            tracing::error!(error = %msg, "Responses API error");
            ParsedLine::ProviderError(msg)
        }
        // Ignore all other event types
        _ => ParsedLine::Events(vec![]),
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
                    let mut accumulated_items: Vec<OutputItemInfo> = Vec::new();
                    let mut raw_stream = raw;
                    while let Some(result) = raw_stream.next().await {
                        match result {
                            Err(e) => { yield Err(e); return; }
                            Ok(line) => {
                                match parse_responses_line(&line, &mut accumulated_items) {
                                    ParsedLine::Events(events) => {
                                        for event in events {
                                            yield Ok(event);
                                        }
                                    }
                                    ParsedLine::ProviderError(msg) => {
                                        yield Err(anyhow::anyhow!("{}", msg));
                                        return;
                                    }
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
    use crate::message::{Message, Role};
    use crate::provider::{AuthMethod, FormatFamily, ProviderCapabilities};

    fn test_provider() -> OpenAIResponsesProvider {
        OpenAIResponsesProvider::new(ProviderConfig {
            base_url: "https://api.openai.com".to_string(),
            auth: AuthMethod::BearerToken("test".to_string()),
            format_family: FormatFamily::OpenAIResponses,
            model_id: "gpt-5.1-codex".to_string(),
            context_window: 128000,
            telemetry: None,
            session_affinity_key: None,
            provider_headers: Default::default(),
            capabilities: ProviderCapabilities::default(),
        })
    }

    #[test]
    fn test_build_request_simple() {
        let provider = test_provider();
        let mut conv = Conversation::new();
        conv.push(Message::system("You are helpful."));
        conv.push(Message::user("Hello"));

        let req = provider.build_request(&conv, &[], None);
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

        let req = provider.build_request(&conv, &[], None);
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
        assert_eq!(
            types,
            vec!["assistant", "function_call", "function_call_output"]
        );

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

        let req = provider.build_request(&conv, &[], None);
        let input = req["input"].as_array().unwrap();
        assert_eq!(input[0]["output"], "Error: not found");
    }

    #[test]
    fn test_parse_text_delta() {
        let line = r#"{"type":"response.output_text.delta","sequence_number":2,"item_id":"msg_1","output_index":0,"content_index":0,"delta":"Hello"}"#;
        let mut acc = Vec::new();
        let ParsedLine::Events(events) = parse_responses_line(line, &mut acc) else {
            panic!("expected events");
        };
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
        let ParsedLine::Events(events) = parse_responses_line(line, &mut acc) else {
            panic!("expected events");
        };
        assert!(events.is_empty());
    }

    #[test]
    fn test_parse_completed_with_function_call() {
        let line = r#"{"type":"response.completed","sequence_number":10,"response":{"id":"resp_1","object":"response","created_at":1737368310,"status":"completed","model":"gpt-5.1-codex","output":[{"type":"function_call","id":"fc_1","status":"completed","call_id":"call_abc","name":"bash","arguments":"{\"cmd\":\"ls\"}"}],"usage":{"input_tokens":100,"output_tokens":50}}}"#;
        let mut acc = Vec::new();
        let ParsedLine::Events(events) = parse_responses_line(line, &mut acc) else {
            panic!("expected events");
        };
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
    fn test_output_item_done_accumulates_until_completed() {
        let item_done = r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_acc","name":"bash","arguments":"{\"cmd\":\"pwd\"}"}}"#;
        let completed = r#"{"type":"response.completed","response":{"output":[],"usage":{"input_tokens":3,"output_tokens":4}}}"#;
        let mut acc = Vec::new();

        let ParsedLine::Events(events) = parse_responses_line(item_done, &mut acc) else {
            panic!("expected events");
        };
        assert!(events.is_empty());
        assert_eq!(acc.len(), 1);

        let ParsedLine::Events(events) = parse_responses_line(completed, &mut acc) else {
            panic!("expected events");
        };
        assert_eq!(events.len(), 2);
        match &events[0] {
            StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }) => {
                assert_eq!(id, "call_acc");
                assert_eq!(name, "bash");
                assert_eq!(input["cmd"], "pwd");
            }
            _ => panic!("expected tool use"),
        }
        match &events[1] {
            StreamEvent::Usage(u) => {
                assert_eq!(u.input, 3);
                assert_eq!(u.output, 4);
            }
            _ => panic!("expected usage"),
        }
    }

    #[test]
    fn test_output_item_done_non_function_call_ignored_on_completion() {
        let item_done = r#"{"type":"response.output_item.done","item":{"type":"message"}}"#;
        let completed = r#"{"type":"response.completed","response":{"output":[],"usage":{"input_tokens":1,"output_tokens":2}}}"#;
        let mut acc = Vec::new();

        let ParsedLine::Events(events) = parse_responses_line(item_done, &mut acc) else {
            panic!("expected events");
        };
        assert!(events.is_empty());

        let ParsedLine::Events(events) = parse_responses_line(completed, &mut acc) else {
            panic!("expected events");
        };
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::Usage(_)));
    }

    #[test]
    fn test_incomplete_function_call_item_is_ignored() {
        let line = r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_abc","name":"bash"}}"#;
        let mut acc = Vec::new();
        let ParsedLine::Events(events) = parse_responses_line(line, &mut acc) else {
            panic!("expected events");
        };
        assert!(events.is_empty());
        assert!(acc.is_empty());
    }

    #[test]
    fn test_parse_keepalive_ignored() {
        let line = r#"{"type":"keepalive"}"#;
        let mut acc = Vec::new();
        let ParsedLine::Events(events) = parse_responses_line(line, &mut acc) else {
            panic!("expected events");
        };
        assert!(events.is_empty());
    }

    #[test]
    fn test_parse_unknown_event_ignored() {
        let line = r#"{"type":"response.some_future_event","data":"foo"}"#;
        let mut acc = Vec::new();
        let ParsedLine::Events(events) = parse_responses_line(line, &mut acc) else {
            panic!("expected events");
        };
        assert!(events.is_empty());
    }

    #[test]
    fn test_parse_error_propagates() {
        let line = r#"{"type":"error","error":{"message":"context_length_exceeded: too many tokens","code":"context_length_exceeded"}}"#;
        let mut acc = Vec::new();
        let ParsedLine::ProviderError(msg) = parse_responses_line(line, &mut acc) else {
            panic!("expected provider error");
        };
        assert!(msg.contains("context_length_exceeded"));
    }

    #[test]
    fn test_parse_response_failed_propagates() {
        let line = r#"{"type":"response.failed","error":{"message":"server error","code":"server_error"}}"#;
        let mut acc = Vec::new();
        let ParsedLine::ProviderError(msg) = parse_responses_line(line, &mut acc) else {
            panic!("expected provider error");
        };
        assert!(msg.contains("server_error"));
    }

    #[test]
    fn test_build_request_with_tools_rmcp_format() {
        let provider = test_provider();
        let mut conv = Conversation::new();
        conv.push(Message::user("list files"));

        // rmcp::model::Tool format (name/description/inputSchema at top level)
        let tools = vec![json!({
            "name": "bash",
            "description": "Run a shell command",
            "inputSchema": {"type": "object", "properties": {"cmd": {"type": "string"}}}
        })];

        let req = provider.build_request(&conv, &tools, None);
        let tools_arr = req["tools"].as_array().unwrap();
        assert_eq!(tools_arr.len(), 1);
        assert_eq!(tools_arr[0]["type"], "function");
        assert_eq!(tools_arr[0]["name"], "bash");
        assert_eq!(tools_arr[0]["description"], "Run a shell command");
        assert!(tools_arr[0]["parameters"]["properties"]["cmd"].is_object());
    }

    #[test]
    fn test_build_request_with_tools_openai_function_format() {
        let provider = test_provider();
        let mut conv = Conversation::new();
        conv.push(Message::user("list files"));

        // OpenAI function-wrapped format (fallback)
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Run a shell command",
                "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}
            }
        })];

        let req = provider.build_request(&conv, &tools, None);
        let tools_arr = req["tools"].as_array().unwrap();
        assert_eq!(tools_arr.len(), 1);
        assert_eq!(tools_arr[0]["type"], "function");
        assert_eq!(tools_arr[0]["name"], "bash");
        assert_eq!(tools_arr[0]["description"], "Run a shell command");
    }

    #[test]
    fn test_build_request_sets_required_tool_choice_when_tools_present() {
        let provider = test_provider();
        let mut conv = Conversation::new();
        conv.push(Message::user("list files"));
        let tools = vec![json!({
            "name": "bash",
            "description": "Run a shell command",
            "inputSchema": {"type": "object"}
        })];

        let req = provider.build_request(&conv, &tools, Some(ToolChoice::Required));
        assert_eq!(req["tool_choice"], "required");
    }

    #[test]
    fn test_build_request_omits_tool_choice_when_tools_empty() {
        let provider = test_provider();
        let mut conv = Conversation::new();
        conv.push(Message::user("list files"));

        let req = provider.build_request(&conv, &[], Some(ToolChoice::Required));
        assert!(req.get("tool_choice").is_none());
    }

    #[test]
    fn test_effective_url() {
        let provider = test_provider();
        assert_eq!(provider.effective_url(), "https://api.openai.com/responses");
    }
}
