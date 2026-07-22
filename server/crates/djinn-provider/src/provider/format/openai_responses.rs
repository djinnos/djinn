// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
use async_stream::stream;
use futures::StreamExt;
use reqwest::header::HeaderMap;
use serde::Deserialize;
use serde_json::{Value, json};
use std::pin::Pin;

use crate::message::{ContentBlock, Conversation};
use crate::provider::FormatFamily;
use crate::provider::client::{ApiClient, SseFrame};
use crate::provider::error::ProviderError;
use crate::provider::format::tool_projection::project;
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
            "include": ["reasoning.encrypted_content"],
            "store": false,
        });

        if self.config.capabilities.streaming {
            body["stream"] = json!(true);
        }

        // Enable reasoning with summaries for models that support it.
        // GPT-5.x defaults to effort=none; we raise it and request summaries
        // so the thinking content is captured and persisted.
        if is_reasoning_capable_model(&self.config.model_id) {
            // `None` preserves the pre-B5 default (effort=medium); `Some(tier)`
            // maps the normalized effort onto the Responses `reasoning.effort`
            // token. `summary:"detailed"` is unconditional either way.
            let effort = self
                .config
                .reasoning_effort
                .map(|tier| tier.openai_effort())
                .unwrap_or("medium");
            body["reasoning"] = json!({
                "effort": effort,
                "summary": "detailed"
            });
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
                        .map(|schema| {
                            project(schema, self.config.tool_schema_compat, FormatFamily::OpenAI)
                        });
                    json!({
                        "type": "function",
                        "name": name,
                        "description": description,
                        "parameters": parameters,
                        "strict": false,
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

        // Diagnostic: surface exactly what each request sends so worker vs chat
        // divergence (model, tool_choice, base_url) is visible under
        // `djinn=debug` instead of having to guess from a masked "empty
        // assistant turn". Kept at debug to avoid per-turn info-log noise.
        tracing::debug!(
            target: "djinn_provider::request",
            model = %self.config.model_id,
            base_url = %self.config.base_url,
            tools = tools.len(),
            tool_choice = ?tool_choice,
            reasoning = is_reasoning_capable_model(&self.config.model_id),
            "openai_responses: building request"
        );

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

/// Returns true for OpenAI models that support the Responses API `reasoning`
/// parameter (effort + summary).  This covers the GPT-5 family and o-series
/// reasoning models.
fn is_reasoning_capable_model(model_id: &str) -> bool {
    let lower = model_id.to_lowercase();
    lower.starts_with("gpt-5")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
}

// ─── SSE parsing ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ResponseMetadata {
    #[serde(default)]
    output: Vec<OutputItemInfo>,
    usage: Option<ResponseUsage>,
}

#[derive(Debug, Deserialize)]
struct ResponseUsage {
    input_tokens: u32,
    output_tokens: u32,
    #[serde(default)]
    input_tokens_details: Option<InputTokensDetails>,
    #[serde(default)]
    output_tokens_details: Option<OutputTokensDetails>,
}

#[derive(Debug, Deserialize, Default)]
struct InputTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
}

#[derive(Debug, Deserialize, Default)]
struct OutputTokensDetails {
    #[serde(default)]
    reasoning_tokens: u32,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OutputItemInfo {
    Reasoning {
        id: Option<String>,
        encrypted_content: Option<String>,
        summary: Option<Value>,
        status: Option<String>,
    },
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
    OutputItemAdded {
        #[serde(default)]
        item: Option<OutputItemInfo>,
    },
    #[serde(rename = "response.content_part.added")]
    ContentPartAdded {},
    #[serde(rename = "response.content_part.done")]
    ContentPartDone {},
    /// Reasoning summary text delta (streamed when `reasoning.summary` is set).
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryTextDelta { delta: String },
    #[serde(rename = "response.reasoning_summary_text.done")]
    ReasoningSummaryTextDone {},
    #[serde(rename = "response.reasoning_summary_part.added")]
    ReasoningSummaryPartAdded {},
    #[serde(rename = "response.reasoning_summary_part.done")]
    ReasoningSummaryPartDone {},
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
    "response.reasoning_summary_text.delta",
    "response.reasoning_summary_text.done",
    "response.reasoning_summary_part.added",
    "response.reasoning_summary_part.done",
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
    /// The OpenAI Responses family terminal frame (`response.completed`).
    /// Carries the terminal events (final output items, usage) that should be
    /// yielded to the consumer, and signals to the stream loop that the
    /// provider-family terminal frame has been observed.
    Terminal(Vec<StreamEvent>),
    /// A typed provider error parsed from a mid-stream `response.failed` /
    /// `error` event, plus the human-readable message. The typed
    /// [`ProviderError`] is preserved (not stringified) so the supervisor's
    /// `classify_provider_failure` can `downcast_ref` it and feed the host
    /// breaker — a `server_error` here is a real provider-side 5xx, not an
    /// untyped blip.
    ProviderError(ProviderError, String),
}

/// Extract the OpenAI error `code` (top-level or nested under `error`) for
/// typed classification, separate from the human-readable message.
fn extract_error_code(error: &Value) -> Option<String> {
    error
        .get("code")
        .or_else(|| error.get("error").and_then(|e| e.get("code")))
        .and_then(Value::as_str)
        .map(str::to_string)
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
/// `in_flight_function_calls` tracks function call items that have been added
/// but whose arguments haven't completed yet (for truncation detection).
/// Returns zero or more `StreamEvent`s, a terminal signal, or a provider error.
fn parse_responses_line(
    line: &str,
    accumulated_items: &mut Vec<OutputItemInfo>,
    in_flight_function_calls: &mut usize,
) -> ParsedLine {
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
        ResponsesStreamEvent::ReasoningSummaryTextDelta { delta } => {
            if delta.is_empty() {
                ParsedLine::Events(vec![])
            } else {
                ParsedLine::Events(vec![StreamEvent::Thinking(delta)])
            }
        }
        ResponsesStreamEvent::OutputItemDone { item } => {
            accumulated_items.push(item);
            ParsedLine::Events(vec![])
        }
        ResponsesStreamEvent::ResponseCompleted { response } => {
            // Detect truncated function call accumulators: function call items
            // were added but their arguments never completed. Fail typed rather
            // than emitting a false complete response.
            if *in_flight_function_calls > 0 {
                tracing::warn!(
                    in_flight = *in_flight_function_calls,
                    "openai_responses stream response.completed with in-flight function calls"
                );
                accumulated_items.clear();
                *in_flight_function_calls = 0;
                return ParsedLine::ProviderError(
                    ProviderError::Transport,
                    "openai_responses stream ended with incomplete function call accumulator"
                        .to_string(),
                );
            }

            let mut events = Vec::new();

            // Emit tool uses from accumulated items (text was already streamed as deltas)
            let final_items = if response.output.is_empty() {
                accumulated_items.as_slice()
            } else {
                response.output.as_slice()
            };

            for item in final_items {
                match item {
                    OutputItemInfo::Reasoning {
                        id,
                        encrypted_content: Some(encrypted_content),
                        summary,
                        status,
                    } if !encrypted_content.is_empty() => {
                        events.push(StreamEvent::Delta(ContentBlock::OpenAIReasoning {
                            id: id.clone(),
                            encrypted_content: encrypted_content.clone(),
                            summary: summary.clone(),
                            status: status.clone(),
                        }));
                    }
                    OutputItemInfo::FunctionCall {
                        call_id,
                        name,
                        arguments,
                        ..
                    } => {
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
                    _ => {}
                }
            }

            // Emit usage
            if let Some(usage) = response.usage {
                let cache_read = usage
                    .input_tokens_details
                    .as_ref()
                    .map(|d| d.cached_tokens)
                    .unwrap_or(0);
                let reasoning_output = usage
                    .output_tokens_details
                    .as_ref()
                    .map(|d| d.reasoning_tokens)
                    .unwrap_or(0);
                events.push(StreamEvent::Usage(TokenUsage {
                    input: usage.input_tokens,
                    output: usage.output_tokens,
                    cache_read,
                    cache_write: 0,
                    reasoning_output,
                    // Responses API `input_tokens` already includes cached input.
                    context_total: usage.input_tokens,
                }));
            }

            // Diagnostic: a `response.completed` carrying NO message and NO
            // function_call (reasoning-only or empty output) is the source of
            // the masked "empty assistant turn". Surface what the model
            // actually returned so we stop guessing.
            let n_msg = final_items
                .iter()
                .filter(|i| matches!(i, OutputItemInfo::Message {}))
                .count();
            let n_call = final_items
                .iter()
                .filter(|i| matches!(i, OutputItemInfo::FunctionCall { .. }))
                .count();
            let n_reasoning = final_items
                .iter()
                .filter(|i| matches!(i, OutputItemInfo::Reasoning { .. }))
                .count();
            if n_msg == 0 && n_call == 0 {
                // Idea 5: distinguish the two empty-output modes for downstream
                // triage. `n_reasoning > 0` = reasoning-only stall (the model
                // spent reasoning but emitted nothing usable; the reply loop
                // nudges it). All-zero = a genuine empty-200, which on the
                // Codex/OpenAI consumer backend is the over-quota account
                // answering with an empty `response.completed` (a throttle that
                // drives failover). The reply loop's authoritative discriminator
                // is the reasoning-token count from the usage frame, not these
                // final-item counts (encrypted/summary-off reasoning may surface
                // no reasoning item), but logging both aids diagnosis.
                let mode = if n_reasoning > 0 {
                    "reasoning-only stall"
                } else {
                    "all-zero (likely Codex/OpenAI quota empty-200 throttle)"
                };
                tracing::warn!(
                    target: "djinn_provider::request",
                    total_items = final_items.len(),
                    reasoning_items = n_reasoning,
                    mode,
                    "openai_responses: response.completed with NO message and NO function_call — surfaces upstream as an empty assistant turn"
                );
            }

            ParsedLine::Terminal(events)
        }
        ResponsesStreamEvent::ResponseFailed { error } => {
            let msg = extract_error_message(&error);
            let class =
                ProviderError::from_stream_error(extract_error_code(&error).as_deref(), &msg);
            tracing::error!(error = %msg, ?class, "Responses API failed");
            ParsedLine::ProviderError(class, msg)
        }
        ResponsesStreamEvent::Error { error } => {
            let msg = extract_error_message(&error);
            let class =
                ProviderError::from_stream_error(extract_error_code(&error).as_deref(), &msg);
            tracing::error!(error = %msg, ?class, "Responses API error");
            ParsedLine::ProviderError(class, msg)
        }
        // Track function call items that have been added to the stream so we
        // can detect truncation (added but never completed).
        ResponsesStreamEvent::OutputItemAdded { item } => {
            if let Some(OutputItemInfo::FunctionCall { .. }) = item {
                *in_flight_function_calls += 1;
            }
            ParsedLine::Events(vec![])
        }
        // A completed function call arguments event: the function call is no
        // longer in-flight (it will arrive as OutputItemDone next).
        ResponsesStreamEvent::FunctionCallArgumentsDone {} => {
            if *in_flight_function_calls > 0 {
                *in_flight_function_calls -= 1;
            }
            ParsedLine::Events(vec![])
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

    fn config_snapshot(&self) -> Option<ProviderConfig> {
        Some(self.config.clone())
    }

    fn stream_request_body_bytes(
        &self,
        conversation: &Conversation,
        tools: &[Value],
        tool_choice: Option<ToolChoice>,
    ) -> Option<usize> {
        serde_json::to_vec(&self.build_request(conversation, tools, tool_choice))
            .ok()
            .map(|body| body.len())
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
            let raw = self
                .client
                .stream_sse_frames(&url, body, &auth, extra_headers);
            let out: Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>> =
                Box::pin(stream! {
                    let mut accumulated_items: Vec<OutputItemInfo> = Vec::new();
                    let mut in_flight_function_calls: usize = 0;
                    let mut raw_stream = raw;
                    let mut seen_done = false;
                    while let Some(result) = raw_stream.next().await {
                        match result {
                            Err(e) => { yield Err(e); return; }
                            Ok(frame) => match frame {
                                SseFrame::Data(line) => {
                                    match parse_responses_line(
                                        &line,
                                        &mut accumulated_items,
                                        &mut in_flight_function_calls,
                                    ) {
                                        ParsedLine::Events(events) => {
                                            for event in events {
                                                yield Ok(event);
                                            }
                                        }
                                        ParsedLine::Terminal(events) => {
                                            // The family terminal frame
                                            // (response.completed) has been
                                            // observed. Yield its events, then
                                            // mark terminal so [DONE] or EOF
                                            // handling is correct.
                                            for event in events {
                                                yield Ok(event);
                                            }
                                            seen_done = true;
                                        }
                                        ParsedLine::ProviderError(class, msg) => {
                                            // Preserve the typed ProviderError as the
                                            // source so downstream `downcast_ref` can
                                            // classify it; the human message rides as
                                            // anyhow context (so `to_string()` is
                                            // unchanged for logs/tests).
                                            yield Err(anyhow::Error::new(class).context(msg));
                                            return;
                                        }
                                    }
                                }
                                SseFrame::Done => {
                                    // The OpenAI [DONE] transport sentinel.
                                    // If we already saw the family terminal
                                    // frame, this is expected. If not, the
                                    // stream ended before the expected terminal
                                    // — fail typed.
                                    if seen_done {
                                        yield Ok(StreamEvent::Done);
                                    } else {
                                        // Discard any partial accumulator state.
                                        accumulated_items.clear();
                                        tracing::warn!("openai_responses stream [DONE] before response.completed");
                                        yield Err(anyhow::Error::new(ProviderError::Transport)
                                            .context("openai_responses stream ended before response.completed"));
                                    }
                                    return;
                                }
                            }
                        }
                    }
                    // Raw EOF. The Responses API's authoritative terminal frame
                    // is `response.completed`, and the connection normally
                    // closes right after it WITHOUT an OpenAI `[DONE]`
                    // transport sentinel (see the `openai_responses_sse_template`
                    // fixture in tests/provider_client_requests.rs, which ends
                    // at `response.completed`). If the terminal frame was
                    // observed, this EOF is a clean end of stream — yield Done.
                    // (When a `[DONE]` sentinel does arrive, the SseFrame::Done
                    // branch above returns first, so Done is emitted exactly
                    // once either way.)
                    if seen_done {
                        yield Ok(StreamEvent::Done);
                    } else {
                        // Raw EOF before the OpenAI Responses terminal frame.
                        // Yield a typed retryable failure.
                        // Discard any partial accumulator state.
                        if !accumulated_items.is_empty() || in_flight_function_calls > 0 {
                            tracing::warn!(
                                accumulated = accumulated_items.len(),
                                in_flight = in_flight_function_calls,
                                "openai_responses stream EOF with incomplete accumulators"
                            );
                            accumulated_items.clear();
                        }
                        tracing::warn!("openai_responses stream ended before response.completed");
                        yield Err(anyhow::Error::new(ProviderError::Transport)
                            .context("openai_responses stream ended before response.completed"));
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
    use crate::message::{Message, Role};
    use crate::provider::{AuthMethod, FormatFamily, ProviderCapabilities};
    use axum::{Router, routing::post};
    use futures::TryStreamExt;
    use std::sync::{Arc, Mutex};

    fn spawn_sse_server(
        status: u16,
        body: &'static str,
        seen_auth: Arc<Mutex<Option<String>>>,
    ) -> String {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("bind local tcp listener");
        let addr = listener.local_addr().expect("local addr");
        listener.set_nonblocking(true).expect("set nonblocking");

        let app = Router::new().route(
            "/responses",
            post(move |req: axum::extract::Request| async move {
                let auth = req
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                *seen_auth.lock().expect("lock seen auth") = auth;
                (
                    axum::http::StatusCode::from_u16(status).expect("status"),
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    body,
                )
            }),
        );

        let tokio_listener =
            tokio::net::TcpListener::from_std(listener).expect("convert to tokio listener");
        tokio::spawn(async move {
            axum::serve(tokio_listener, app).await.ok();
        });

        format!("http://{}:{}", addr.ip(), addr.port())
    }

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
            reasoning_effort: None,
            tool_schema_compat: None,
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
        assert_eq!(req["include"], json!(["reasoning.encrypted_content"]));

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
    fn test_build_request_preserves_encrypted_reasoning_before_tool_output() {
        let provider = test_provider();
        let mut conv = Conversation::new();
        conv.push(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::OpenAIReasoning {
                    id: Some("rs_1".into()),
                    encrypted_content: "enc".into(),
                    summary: Some(json!([])),
                    status: Some("completed".into()),
                },
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "bash".into(),
                    input: json!({"cmd": "pwd"}),
                },
            ],
            metadata: None,
        });
        conv.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: vec![ContentBlock::text("/repo")],
                is_error: false,
            }],
            metadata: None,
        });

        let req = provider.build_request(&conv, &[], None);
        let input = req["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["id"], "rs_1");
        assert_eq!(input[0]["encrypted_content"], "enc");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[2]["type"], "function_call_output");
    }

    #[test]
    fn test_build_request_error_tool_result() {
        let provider = test_provider();
        let mut conv = Conversation::new();
        // A tool result must be preceded by its matching function call, or the
        // serializer drops it as an orphan (the Responses API rejects a
        // function_call_output with no matching call). Pair it so we exercise
        // the is_error "Error: " prefix rendering on a real tool result.
        conv.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "read".into(),
                input: json!({}),
            }],
            metadata: None,
        });
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
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["output"], "Error: not found");
    }

    #[test]
    fn test_parse_text_delta() {
        let line = r#"{"type":"response.output_text.delta","sequence_number":2,"item_id":"msg_1","output_index":0,"content_index":0,"delta":"Hello"}"#;
        let mut acc = Vec::new();
        let ParsedLine::Events(events) = parse_responses_line(line, &mut acc, &mut 0) else {
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
        let ParsedLine::Events(events) = parse_responses_line(line, &mut acc, &mut 0) else {
            panic!("expected events");
        };
        assert!(events.is_empty());
    }

    #[test]
    fn test_parse_completed_with_function_call() {
        let line = r#"{"type":"response.completed","sequence_number":10,"response":{"id":"resp_1","object":"response","created_at":1737368310,"status":"completed","model":"gpt-5.1-codex","output":[{"type":"function_call","id":"fc_1","status":"completed","call_id":"call_abc","name":"bash","arguments":"{\"cmd\":\"ls\"}"}],"usage":{"input_tokens":100,"output_tokens":50}}}"#;
        let mut acc = Vec::new();
        let ParsedLine::Terminal(events) = parse_responses_line(line, &mut acc, &mut 0) else {
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
    fn test_parse_completed_with_reasoning_and_function_call() {
        let line = r#"{"type":"response.completed","response":{"output":[{"type":"reasoning","id":"rs_1","summary":[],"status":"completed","encrypted_content":"enc"},{"type":"function_call","call_id":"call_abc","name":"bash","arguments":"{\"cmd\":\"ls\"}"}],"usage":{"input_tokens":10,"output_tokens":5}}}"#;
        let mut acc = Vec::new();
        let ParsedLine::Terminal(events) = parse_responses_line(line, &mut acc, &mut 0) else {
            panic!("expected events");
        };

        assert_eq!(events.len(), 3);
        match &events[0] {
            StreamEvent::Delta(ContentBlock::OpenAIReasoning {
                id,
                encrypted_content,
                ..
            }) => {
                assert_eq!(id.as_deref(), Some("rs_1"));
                assert_eq!(encrypted_content, "enc");
            }
            _ => panic!("expected reasoning item"),
        }
        assert!(matches!(
            &events[1],
            StreamEvent::Delta(ContentBlock::ToolUse { id, .. }) if id == "call_abc"
        ));
    }

    #[test]
    fn test_output_item_done_accumulates_until_completed() {
        let item_done = r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_acc","name":"bash","arguments":"{\"cmd\":\"pwd\"}"}}"#;
        let completed = r#"{"type":"response.completed","response":{"output":[],"usage":{"input_tokens":3,"output_tokens":4}}}"#;
        let mut acc = Vec::new();

        let ParsedLine::Events(events) = parse_responses_line(item_done, &mut acc, &mut 0) else {
            panic!("expected events");
        };
        assert!(events.is_empty());
        assert_eq!(acc.len(), 1);

        let ParsedLine::Terminal(events) = parse_responses_line(completed, &mut acc, &mut 0) else {
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

        let ParsedLine::Events(events) = parse_responses_line(item_done, &mut acc, &mut 0) else {
            panic!("expected events");
        };
        assert!(events.is_empty());

        let ParsedLine::Terminal(events) = parse_responses_line(completed, &mut acc, &mut 0) else {
            panic!("expected events");
        };
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::Usage(_)));
    }

    #[test]
    fn test_completed_missing_output_is_treated_as_empty_response() {
        let completed = r#"{"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":2}}}"#;
        let mut acc = Vec::new();

        let ParsedLine::Terminal(events) = parse_responses_line(completed, &mut acc, &mut 0) else {
            panic!("expected events");
        };
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::Usage(_)));
    }

    #[test]
    fn test_incomplete_function_call_item_is_ignored() {
        let line = r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_abc","name":"bash"}}"#;
        let mut acc = Vec::new();
        let ParsedLine::Events(events) = parse_responses_line(line, &mut acc, &mut 0) else {
            panic!("expected events");
        };
        assert!(events.is_empty());
        assert!(acc.is_empty());
    }

    #[test]
    fn test_parse_keepalive_ignored() {
        let line = r#"{"type":"keepalive"}"#;
        let mut acc = Vec::new();
        let ParsedLine::Events(events) = parse_responses_line(line, &mut acc, &mut 0) else {
            panic!("expected events");
        };
        assert!(events.is_empty());
    }

    #[test]
    fn test_parse_unknown_event_ignored() {
        let line = r#"{"type":"response.some_future_event","data":"foo"}"#;
        let mut acc = Vec::new();
        let ParsedLine::Events(events) = parse_responses_line(line, &mut acc, &mut 0) else {
            panic!("expected events");
        };
        assert!(events.is_empty());
    }

    #[test]
    fn test_parse_error_propagates() {
        let line = r#"{"type":"error","error":{"message":"context_length_exceeded: too many tokens","code":"context_length_exceeded"}}"#;
        let mut acc = Vec::new();
        let ParsedLine::ProviderError(class, msg) = parse_responses_line(line, &mut acc, &mut 0)
        else {
            panic!("expected provider error");
        };
        assert!(msg.contains("context_length_exceeded"));
        assert_eq!(class, ProviderError::ContextOverflow);
    }

    #[test]
    fn test_parse_response_failed_propagates() {
        let line = r#"{"type":"response.failed","error":{"message":"server error","code":"server_error"}}"#;
        let mut acc = Vec::new();
        let ParsedLine::ProviderError(class, msg) = parse_responses_line(line, &mut acc, &mut 0)
        else {
            panic!("expected provider error");
        };
        assert!(msg.contains("server_error"));
        // A mid-stream server_error must be typed as a provider-internal 5xx so
        // the supervisor breaker can act on it (was previously an untyped string
        // → provider_failure: None → breaker never fed).
        assert_eq!(class, ProviderError::ProviderInternal { status: 500 });
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
        assert_eq!(tools_arr[0]["strict"], false);
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
        assert_eq!(tools_arr[0]["strict"], false);
    }

    #[test]
    fn test_build_request_with_tools_moonshot_compat_projects_schema() {
        let mut provider = test_provider();
        provider.config.tool_schema_compat = Some(crate::provider::ToolSchemaCompat::Moonshot);
        let mut conv = Conversation::new();
        conv.push(Message::user("list files"));

        // Schema uses Draft-2020-12 prefixItems that Moonshot rejects.
        let tools = vec![json!({
            "name": "tuple_tool",
            "description": "Takes a tuple",
            "inputSchema": {
                "type": "array",
                "prefixItems": [{ "type": "string" }, { "type": "integer" }]
            }
        })];

        let req = provider.build_request(&conv, &tools, None);
        let tools_arr = req["tools"].as_array().unwrap();
        assert_eq!(tools_arr[0]["name"], "tuple_tool");
        assert_eq!(tools_arr[0]["strict"], false);
        // Moonshot projection collapses prefixItems into items array.
        let params = &tools_arr[0]["parameters"];
        assert!(params.get("prefixItems").is_none());
        let items = params["items"].as_array().expect("items should be array");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["type"], "string");
        assert_eq!(items[1]["type"], "integer");
    }

    #[test]
    fn test_build_request_native_no_quirk_emits_strict_false_per_tool() {
        // Captures the wire-shape OpenAI Responses emits for a native
        // (`tool_schema_compat: None`) provider config, by observing the
        // **actual generated JSON body** (`build_request` is exactly the
        // JSON OpenAI Responses receives). The companion integration test
        // in `tests/provider_client_requests.rs::openai_responses_native_*`
        // does the full request round-trip; this in-crate test pins the
        // `build_request` shape independently so a seam regression surfaces
        // here even when the integration test harness is unavailable.
        //
        // The Responses path is the documented exception to the no-quirk
        // identity rule: every emitted function tool spec MUST carry
        // `"strict": false` because the RMCP-emitted input schemas do not
        // satisfy OpenAI's strict-schema requirements. Native=no-quirk
        // everywhere else — only `strict` is intentionally non-native.
        let provider = test_provider();
        let mut conv = Conversation::new();
        conv.push(Message::user("list files"));
        let tools = vec![
            json!({
                "name": "bash",
                "description": "Run a shell command",
                "inputSchema": {
                    "type": "object",
                    "properties": {"cmd": {"type": "string"}},
                    "required": ["cmd"]
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "read",
                    "description": "Read a file",
                    "parameters": {
                        "type": "object",
                        "properties": {"path": {"type": "string"}}
                    }
                }
            }),
        ];

        let req = provider.build_request(&conv, &tools, Some(ToolChoice::Auto));
        let tools_arr = req["tools"].as_array().expect("tools array");
        assert_eq!(tools_arr.len(), 2);

        for tool in tools_arr {
            assert_eq!(tool["type"], "function");
            // Responses hoists `name`/`description`/`parameters` to the top
            // level; the chat-completions `function` wrapper is gone.
            assert!(tool["name"].is_string());
            assert!(tool["description"].is_string());
            assert!(tool["function"].is_null());
            assert!(tool["parameters"].is_object());
            // The intended non-native-shape exception: every function tool
            // spec carries `strict: false` regardless of compat.
            assert_eq!(tool["strict"], false);
        }

        // Byte-determinism: the whole body must serialize to identical
        // strings across repeated builds. A drift here would also break the
        // implicit OpenAI Responses cache and rate-limit contract on the
        // and would mean a non-deterministic value (timestamp, hash order,
        // id) leaked into the native path that mpen AC3 says must stay
        // stable. This guards the same invariant as the Anthropic /
        // OpenAI-ChatCompletions / Google native tests do for their seams.
        let body_a = serde_json::to_string(&req).expect("serialize body once");
        let body_b = serde_json::to_string(&req).expect("serialize body twice");
        assert_eq!(
            body_a, body_b,
            "OpenAI Responses native no-quirk request must be byte-deterministic across builds"
        );
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

    #[tokio::test]
    async fn test_stream_dispatches_text_and_completed_tool_call_from_data_lines() {
        let seen_auth = Arc::new(Mutex::new(None));
        let body = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_stream\",\"name\":\"bash\",\"arguments\":\"{\\\"cmd\\\":\\\"pwd\\\"}\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[],\"usage\":{\"input_tokens\":11,\"output_tokens\":13}}}\n\n",
            "data: [DONE]\n\n"
        );
        let mut config = test_provider().config.clone();
        config.base_url = spawn_sse_server(200, body, seen_auth.clone());
        let provider = OpenAIResponsesProvider::new(config);
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));

        let stream = provider
            .stream(&conv, &[], None)
            .await
            .expect("stream start");
        let events: Vec<_> = stream.try_collect().await.expect("stream events");

        assert_eq!(
            seen_auth.lock().expect("seen auth").as_deref(),
            Some("Bearer test")
        );
        assert!(
            matches!(&events[0], StreamEvent::Delta(ContentBlock::Text { text }) if text == "Hello")
        );
        assert!(
            matches!(&events[1], StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }) if id == "call_stream" && name == "bash" && input["cmd"] == "pwd")
        );
        assert!(matches!(
            &events[2],
            StreamEvent::Usage(TokenUsage {
                input: 11,
                output: 13,
                ..
            })
        ));
        assert!(matches!(&events[3], StreamEvent::Done));
    }

    #[tokio::test]
    async fn test_stream_http_error_is_propagated() {
        let seen_auth = Arc::new(Mutex::new(None));
        let mut config = test_provider().config.clone();
        config.base_url = spawn_sse_server(401, "unauthorized", seen_auth);
        let provider = OpenAIResponsesProvider::new(config);
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));

        let stream = provider
            .stream(&conv, &[], None)
            .await
            .expect("stream start");
        let result: Result<Vec<_>, _> = stream.try_collect().await;
        let err = match result {
            Ok(_) => panic!("expected provider error"),
            Err(err) => err,
        };
        let msg = err.to_string();
        assert!(msg.contains("provider API error 401"));
        assert!(msg.contains("unauthorized"));
    }

    #[tokio::test]
    async fn test_stream_response_failed_event_is_propagated() {
        let seen_auth = Arc::new(Mutex::new(None));
        let body = "data: {\"type\":\"response.failed\",\"error\":{\"message\":\"quota exceeded\",\"code\":\"insufficient_quota\"}}\n\n";
        let mut config = test_provider().config.clone();
        config.base_url = spawn_sse_server(200, body, seen_auth);
        let provider = OpenAIResponsesProvider::new(config);
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));

        let stream = provider
            .stream(&conv, &[], None)
            .await
            .expect("stream start");
        let result: Result<Vec<_>, _> = stream.try_collect().await;
        let err = match result {
            Ok(_) => panic!("expected provider error"),
            Err(err) => err,
        };
        let msg = err.to_string();
        assert!(msg.contains("insufficient_quota"));
        assert!(msg.contains("quota exceeded"));
    }

    #[tokio::test]
    async fn test_stream_raw_eof_before_terminal_yields_retryable_error() {
        let seen_auth = Arc::new(Mutex::new(None));
        // SSE body with data events but NO response.completed and NO [DONE]
        let body = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n"
        );
        let mut config = test_provider().config.clone();
        config.base_url = spawn_sse_server(200, body, seen_auth);
        let provider = OpenAIResponsesProvider::new(config);
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));

        let stream = provider
            .stream(&conv, &[], None)
            .await
            .expect("stream start");
        let result: Result<Vec<_>, _> = stream.try_collect().await;
        let err = match result {
            Ok(_) => panic!("expected provider error for EOF before terminal"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("ended before response.completed"));
        // Must be a downcastable retryable ProviderError::Transport
        assert!(
            err.downcast_ref::<ProviderError>()
                .is_some_and(|e| e.retryable()),
            "expected retryable ProviderError::Transport, got: {err:#}"
        );
    }

    #[tokio::test]
    async fn test_stream_response_completed_before_done_is_terminal() {
        let seen_auth = Arc::new(Mutex::new(None));
        // response.completed followed by [DONE] — the expected happy path
        let body = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[],\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n\n",
            "data: [DONE]\n\n"
        );
        let mut config = test_provider().config.clone();
        config.base_url = spawn_sse_server(200, body, seen_auth);
        let provider = OpenAIResponsesProvider::new(config);
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));

        let stream = provider
            .stream(&conv, &[], None)
            .await
            .expect("stream start");
        let events: Vec<_> = stream.try_collect().await.expect("stream events");

        assert!(matches!(
            &events[0],
            StreamEvent::Delta(ContentBlock::Text { text }) if text == "Hi"
        ));
        assert!(matches!(&events[1], StreamEvent::Usage(_)));
        assert!(matches!(&events[2], StreamEvent::Done));
    }

    #[tokio::test]
    async fn test_stream_eof_after_completed_without_done_sentinel_tool_call() {
        let seen_auth = Arc::new(Mutex::new(None));
        // The Responses API's authoritative terminal frame is
        // `response.completed`; the connection then closes with NO `[DONE]`
        // transport sentinel. This is the exact production shape for
        // tool-call-only turns (gpt-5.5 via Codex OAuth): the stream must end
        // with a clean StreamEvent::Done, not an EOF-truncation error.
        let body = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_eof\",\"name\":\"bash\",\"arguments\":\"\",\"status\":\"in_progress\"}}\n\n",
            "event: response.function_call_arguments.done\n",
            "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_1\",\"output_index\":0,\"arguments\":\"{\\\"cmd\\\":\\\"ls\\\"}\"}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_eof\",\"name\":\"bash\",\"arguments\":\"{\\\"cmd\\\":\\\"ls\\\"}\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[],\"usage\":{\"input_tokens\":7,\"output_tokens\":9}}}\n\n" // NO [DONE] sentinel — the server closes the connection here.
        );
        let mut config = test_provider().config.clone();
        config.base_url = spawn_sse_server(200, body, seen_auth);
        let provider = OpenAIResponsesProvider::new(config);
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));

        let stream = provider
            .stream(&conv, &[], None)
            .await
            .expect("stream start");
        let events: Vec<_> = stream.try_collect().await.expect("stream events");

        assert!(
            matches!(&events[0], StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }) if id == "call_eof" && name == "bash" && input["cmd"] == "ls")
        );
        assert!(matches!(
            &events[1],
            StreamEvent::Usage(TokenUsage {
                input: 7,
                output: 9,
                ..
            })
        ));
        assert!(matches!(&events[2], StreamEvent::Done));
        assert_eq!(events.len(), 3);
    }

    #[tokio::test]
    async fn test_stream_eof_after_completed_without_done_sentinel_text() {
        let seen_auth = Arc::new(Mutex::new(None));
        // Text-only stream ending at `response.completed` with NO `[DONE]`
        // sentinel: must terminate with StreamEvent::Done, not an error.
        let body = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[],\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n\n" // NO [DONE] sentinel — the server closes the connection here.
        );
        let mut config = test_provider().config.clone();
        config.base_url = spawn_sse_server(200, body, seen_auth);
        let provider = OpenAIResponsesProvider::new(config);
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));

        let stream = provider
            .stream(&conv, &[], None)
            .await
            .expect("stream start");
        let events: Vec<_> = stream.try_collect().await.expect("stream events");

        assert!(matches!(
            &events[0],
            StreamEvent::Delta(ContentBlock::Text { text }) if text == "Hi"
        ));
        assert!(matches!(&events[1], StreamEvent::Usage(_)));
        assert!(matches!(&events[2], StreamEvent::Done));
        assert_eq!(events.len(), 3);
    }

    #[tokio::test]
    async fn test_stream_truncated_function_call_accumulator_fails_typed() {
        let seen_auth = Arc::new(Mutex::new(None));
        // A function call item is added (output_item.added) but
        // function_call_arguments.done never arrives, and
        // response.completed is emitted. The adapter should fail typed.
        let body = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Let me check\"}\n\n",
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_trunc\",\"name\":\"bash\",\"arguments\":\"\",\"status\":\"in_progress\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[],\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n\n",
            "data: [DONE]\n\n"
        );
        let mut config = test_provider().config.clone();
        config.base_url = spawn_sse_server(200, body, seen_auth);
        let provider = OpenAIResponsesProvider::new(config);
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));

        let stream = provider
            .stream(&conv, &[], None)
            .await
            .expect("stream start");
        let result: Result<Vec<_>, _> = stream.try_collect().await;
        let err = match result {
            Ok(_) => panic!("expected error for truncated function call accumulator"),
            Err(err) => err,
        };
        assert!(
            err.to_string()
                .contains("incomplete function call accumulator"),
            "unexpected error: {err:#}"
        );
        // Must be downcastable retryable ProviderError::Transport
        assert!(
            err.downcast_ref::<ProviderError>()
                .is_some_and(|e| e.retryable()),
            "expected retryable ProviderError::Transport, got: {err:#}"
        );
    }

    #[test]
    fn test_parse_reasoning_summary_delta() {
        let mut acc = Vec::new();
        let line = r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs_abc","output_index":0,"summary_index":0,"delta":"The user wants to","sequence_number":4}"#;
        match parse_responses_line(line, &mut acc, &mut 0) {
            ParsedLine::Events(events) => {
                assert_eq!(events.len(), 1);
                assert!(matches!(&events[0], StreamEvent::Thinking(t) if t == "The user wants to"));
            }
            ParsedLine::Terminal(_) => panic!("unexpected terminal"),
            ParsedLine::ProviderError(_, e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn test_parse_reasoning_summary_empty_delta_skipped() {
        let mut acc = Vec::new();
        let line = r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs_abc","output_index":0,"summary_index":0,"delta":"","sequence_number":4}"#;
        match parse_responses_line(line, &mut acc, &mut 0) {
            ParsedLine::Events(events) => assert!(events.is_empty()),
            ParsedLine::Terminal(_) => panic!("unexpected terminal"),
            ParsedLine::ProviderError(_, e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn test_reasoning_summary_lifecycle_events_ignored() {
        let mut acc = Vec::new();
        // These lifecycle events should be silently consumed without error
        for line in [
            r#"{"type":"response.reasoning_summary_part.added","item_id":"rs_abc","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""},"sequence_number":3}"#,
            r#"{"type":"response.reasoning_summary_text.done","item_id":"rs_abc","output_index":0,"summary_index":0,"text":"Full summary.","sequence_number":10}"#,
            r#"{"type":"response.reasoning_summary_part.done","item_id":"rs_abc","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":"Full summary."},"sequence_number":11}"#,
        ] {
            match parse_responses_line(line, &mut acc, &mut 0) {
                ParsedLine::Events(events) => {
                    assert!(events.is_empty(), "expected no events for lifecycle event")
                }
                ParsedLine::Terminal(_) => panic!("unexpected terminal"),
                ParsedLine::ProviderError(_, e) => panic!("unexpected error: {e}"),
            }
        }
    }

    #[test]
    fn test_build_request_includes_reasoning_for_gpt5() {
        let mut config = test_provider().config.clone();
        config.model_id = "gpt-5.4".to_string();
        let provider = OpenAIResponsesProvider::new(config);
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));
        let req = provider.build_request(&conv, &[], None);
        assert_eq!(req["reasoning"]["effort"], "medium");
        assert_eq!(req["reasoning"]["summary"], "detailed");
    }

    #[test]
    fn test_build_request_no_reasoning_for_codex() {
        let provider = test_provider(); // model_id = gpt-5.1-codex
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));
        let req = provider.build_request(&conv, &[], None);
        // codex models don't match is_reasoning_capable_model (they contain "codex")
        // but gpt-5.1-codex starts with "gpt-5" so it should get reasoning
        assert_eq!(req["reasoning"]["effort"], "medium");
    }

    #[test]
    fn test_build_request_includes_reasoning_for_o_series() {
        let mut config = test_provider().config.clone();
        config.model_id = "o3".to_string();
        let provider = OpenAIResponsesProvider::new(config);
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));
        let req = provider.build_request(&conv, &[], None);
        assert_eq!(req["reasoning"]["effort"], "medium");
        assert_eq!(req["reasoning"]["summary"], "detailed");
    }

    #[test]
    fn test_build_request_no_reasoning_for_non_reasoning_model() {
        let mut config = test_provider().config.clone();
        config.model_id = "some-custom-model".to_string();
        let provider = OpenAIResponsesProvider::new(config);
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));
        let req = provider.build_request(&conv, &[], None);
        assert!(req.get("reasoning").is_none());
    }

    // ─── B5: reasoning-effort tier mapping ──────────────────────────────────

    #[test]
    fn test_reasoning_effort_none_is_medium_default() {
        // None must be byte-identical to the pre-B5 default.
        let mut config = test_provider().config.clone();
        config.model_id = "gpt-5.4".to_string();
        config.reasoning_effort = None;
        let provider = OpenAIResponsesProvider::new(config);
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));
        let req = provider.build_request(&conv, &[], None);
        assert_eq!(req["reasoning"]["effort"], "medium");
        assert_eq!(req["reasoning"]["summary"], "detailed");
    }

    #[test]
    fn test_default_policy_preserves_openai_responses_request_bytes() {
        use crate::provider::default_reasoning_effort_for_model;

        let mut conv = Conversation::new();
        conv.push(Message::system("You are helpful."));
        conv.push(Message::user("Hello"));

        let mut baseline_config = test_provider().config.clone();
        baseline_config.model_id = "gpt-5.1-codex".to_string();
        baseline_config.reasoning_effort = None;
        let baseline =
            OpenAIResponsesProvider::new(baseline_config.clone()).build_request(&conv, &[], None);

        let mut policy_config = baseline_config;
        policy_config.reasoning_effort = default_reasoning_effort_for_model(
            true,
            FormatFamily::OpenAIResponses,
            &policy_config.model_id,
        );
        let with_policy =
            OpenAIResponsesProvider::new(policy_config).build_request(&conv, &[], None);

        assert_eq!(with_policy, baseline);
        assert_eq!(with_policy["reasoning"]["effort"], "medium");
        assert_eq!(with_policy["reasoning"]["summary"], "detailed");
    }

    #[test]
    fn test_reasoning_effort_high_maps_effort() {
        use crate::provider::ReasoningEffort;
        let mut config = test_provider().config.clone();
        config.model_id = "gpt-5.4".to_string();
        config.reasoning_effort = Some(ReasoningEffort::High);
        let provider = OpenAIResponsesProvider::new(config);
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));
        let req = provider.build_request(&conv, &[], None);
        assert_eq!(req["reasoning"]["effort"], "high");
        // summary stays "detailed" regardless of tier.
        assert_eq!(req["reasoning"]["summary"], "detailed");
    }

    #[test]
    fn test_reasoning_effort_minimal_maps_effort() {
        use crate::provider::ReasoningEffort;
        let mut config = test_provider().config.clone();
        config.model_id = "gpt-5.4".to_string();
        config.reasoning_effort = Some(ReasoningEffort::Minimal);
        let provider = OpenAIResponsesProvider::new(config);
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));
        let req = provider.build_request(&conv, &[], None);
        // `Minimal` maps to `"low"`: gpt-5.5+ rejects `"minimal"`, and `low` is
        // the weakest tier the whole gpt-5.x family accepts (see
        // `ReasoningEffort::openai_effort`).
        assert_eq!(req["reasoning"]["effort"], "low");
        assert_eq!(req["reasoning"]["summary"], "detailed");
    }

    #[tokio::test]
    async fn test_stream_reasoning_summary_with_text() {
        let seen_auth = Arc::new(Mutex::new(None));
        let body = "\
data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n\
data: {\"type\":\"response.in_progress\",\"response\":{\"id\":\"resp_1\"}}\n\n\
data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"summary\":[],\"status\":\"in_progress\"}}\n\n\
data: {\"type\":\"response.reasoning_summary_part.added\",\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":0,\"part\":{\"type\":\"summary_text\",\"text\":\"\"}}\n\n\
data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":0,\"delta\":\"Thinking about \"}\n\n\
data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":0,\"delta\":\"the problem.\"}\n\n\
data: {\"type\":\"response.reasoning_summary_text.done\",\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":0,\"text\":\"Thinking about the problem.\"}\n\n\
data: {\"type\":\"response.reasoning_summary_part.done\",\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":0,\"part\":{\"type\":\"summary_text\",\"text\":\"Thinking about the problem.\"}}\n\n\
data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"Thinking about the problem.\"}],\"status\":\"completed\"}}\n\n\
data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}\n\n\
data: {\"type\":\"response.content_part.added\",\"item_id\":\"msg_1\",\"output_index\":1,\"content_index\":0,\"part\":{\"type\":\"output_text\",\"text\":\"\"}}\n\n\
data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":1,\"content_index\":0,\"delta\":\"The answer is 42.\"}\n\n\
data: {\"type\":\"response.output_text.done\",\"item_id\":\"msg_1\",\"output_index\":1,\"content_index\":0,\"text\":\"The answer is 42.\"}\n\n\
data: {\"type\":\"response.content_part.done\",\"item_id\":\"msg_1\",\"output_index\":1,\"content_index\":0,\"part\":{\"type\":\"output_text\",\"text\":\"The answer is 42.\"}}\n\n\
data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"The answer is 42.\"}],\"status\":\"completed\"}}\n\n\
data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[{\"type\":\"reasoning\"},{\"type\":\"message\"}],\"usage\":{\"input_tokens\":50,\"output_tokens\":30}}}\n\n\
data: [DONE]\n\n";

        let mut config = test_provider().config.clone();
        config.base_url = spawn_sse_server(200, body, seen_auth);
        let provider = OpenAIResponsesProvider::new(config);
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));

        let stream = provider
            .stream(&conv, &[], None)
            .await
            .expect("stream start");
        let events: Vec<StreamEvent> = stream.try_collect().await.expect("stream events");

        // Should have: Thinking("Thinking about "), Thinking("the problem."),
        //              Delta(Text("The answer is 42.")), Usage, Done
        let thinking_events: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Thinking(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking_events, vec!["Thinking about ", "the problem."]);

        let text_events: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(ContentBlock::Text { text }) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text_events, vec!["The answer is 42."]);

        assert!(events.iter().any(|e| matches!(e, StreamEvent::Usage(_))));
        assert!(events.iter().any(|e| matches!(e, StreamEvent::Done)));
    }

    // ── Shared Anthropic-oriented variants do not leak into Responses requests ──

    /// The Responses request (`build_request`) must skip the shared
    /// Anthropic-oriented `ContentBlock` variants — Thinking, RedactedThinking,
    /// and Unknown — rather than serializing them as empty `output_text` items
    /// or otherwise altering the established Responses input shape. Guards the
    /// shared `ContentBlock` expansion from regressing non-Anthropic provider
    /// request content.
    #[test]
    fn test_build_request_drops_anthropic_thinking_and_unknown_blocks() {
        let provider = test_provider();
        let mut conv = Conversation::new();
        conv.push(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "internal reasoning".to_string(),
                    signature: Some("sig_abc".to_string()),
                },
                ContentBlock::RedactedThinking {
                    data: "opaque_data_blob".to_string(),
                },
                ContentBlock::Unknown {
                    content_type: "custom_provider_block".to_string(),
                    extra: {
                        let mut m = serde_json::Map::new();
                        m.insert("foo".to_string(), json!("bar"));
                        m
                    },
                },
                ContentBlock::text("visible output"),
            ],
            metadata: None,
        });

        let req = provider.build_request(&conv, &[], None);
        let input = req["input"].as_array().expect("input array");
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "assistant");
        let content = input[0]["content"].as_array().unwrap();
        // Only the visible text survives as output_text.
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "output_text");
        assert_eq!(content[0]["text"], "visible output");
        // No empty-text placeholder anywhere in the input.
        assert!(input.iter().all(|item| {
            item.get("content")
                .and_then(|c| c.as_array())
                .map(|arr| {
                    !arr.iter().any(|b| {
                        b.get("type") == Some(&json!("output_text"))
                            && b.get("text") == Some(&json!(""))
                    })
                })
                .unwrap_or(true)
        }));
    }

    /// When an assistant message contains only provider-internal Anthropic
    /// variants, the Responses request must not emit any item for it — no
    /// empty-text fallback and no spurious reasoning/function items.
    #[test]
    fn test_build_request_all_anthropic_internal_blocks_dropped() {
        let provider = test_provider();
        let mut conv = Conversation::new();
        conv.push(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "internal reasoning".to_string(),
                    signature: Some("sig_abc".to_string()),
                },
                ContentBlock::RedactedThinking {
                    data: "opaque".to_string(),
                },
            ],
            metadata: None,
        });

        let req = provider.build_request(&conv, &[], None);
        let input = req["input"].as_array().expect("input array");
        // No input items emitted for the all-internal assistant message.
        assert_eq!(input.len(), 0);
    }

    /// Unsigned thinking (historical JSON shape lacking `signature`) is also
    /// skipped by the Responses path, confirming backward-compatible handling
    /// does not surface as content in non-Anthropic requests.
    #[test]
    fn test_build_request_drops_unsigned_thinking_block() {
        let provider = test_provider();
        // Simulate a historical unsigned thinking block from DB storage.
        let block: ContentBlock =
            serde_json::from_value(json!({"type": "thinking", "thinking": "legacy"})).unwrap();
        let mut conv = Conversation::new();
        conv.push(Message {
            role: Role::Assistant,
            content: vec![block, ContentBlock::text("visible")],
            metadata: None,
        });

        let req = provider.build_request(&conv, &[], None);
        let input = req["input"].as_array().expect("input array");
        assert_eq!(input.len(), 1);
        let content = input[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "output_text");
        assert_eq!(content[0]["text"], "visible");
    }
}
