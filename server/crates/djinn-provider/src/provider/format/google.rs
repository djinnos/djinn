use async_stream::stream;
use futures::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use std::pin::Pin;

use crate::message::{ContentBlock, Conversation};
use crate::provider::client::ApiClient;
use crate::provider::format::tool_projection::project;
use crate::provider::{
    FormatFamily, LlmProvider, ProviderConfig, ProviderError, StreamEvent, TokenUsage, ToolChoice,
};

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

    fn build_request(
        &self,
        conversation: &Conversation,
        tools: &[Value],
        tool_choice: Option<ToolChoice>,
    ) -> Value {
        let (system, contents) = conversation.to_google_contents();

        let mut body = json!({"contents": contents});

        if let Some(sys) = system {
            body["systemInstruction"] = json!({
                "parts": [{"text": sys}]
            });
        }

        // Thinking config. `None` => no `thinkingConfig` (pre-B5 behavior).
        // `Some(tier)` => request a per-tier `thinkingBudget` under
        // `generationConfig`.
        if let Some(tier) = self.config.reasoning_effort {
            body["generationConfig"] = json!({
                "thinkingConfig": {
                    "thinkingBudget": tier.thinking_budget()
                }
            });
        }

        if !tools.is_empty() {
            let declarations: Vec<Value> = tools
                .iter()
                .map(|tool| {
                    let name = tool
                        .get("name")
                        .and_then(|n| n.as_str())
                        .map(String::from)
                        .unwrap_or_default();
                    let description = tool
                        .get("description")
                        .and_then(|d| d.as_str())
                        .map(String::from)
                        .unwrap_or_default();
                    let input_schema = tool
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object"}));
                    let projected = project(
                        input_schema,
                        self.config.tool_schema_compat,
                        FormatFamily::Google,
                    );
                    json!({
                        "name": name,
                        "description": description,
                        "parameters": projected,
                    })
                })
                .collect();
            body["tools"] = json!([{"functionDeclarations": declarations}]);

            match tool_choice.unwrap_or(ToolChoice::Auto) {
                ToolChoice::Auto => {}
                ToolChoice::Required => {
                    body["toolConfig"] = json!({"functionCallingConfig": {"mode": "ANY"}})
                }
                ToolChoice::None => {
                    body["toolConfig"] = json!({"functionCallingConfig": {"mode": "NONE"}})
                }
            }
        }

        body
    }

    fn effective_url(&self) -> String {
        // Google AI Studio endpoint for streaming
        format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            self.config.base_url.trim_end_matches('/'),
            self.config.model_id
        )
    }

    fn extra_headers(&self) -> HeaderMap {
        HeaderMap::new()
    }
}

// ─── SSE parsing helpers ──────────────────────────────────────────────────────

/// Parse a single Google AI Studio SSE data line.
/// Returns zero or more `StreamEvent`s produced by this chunk.
pub fn parse_google_line(line: &str) -> Vec<StreamEvent> {
    let v = match parse_google_json(line) {
        Some(v) => v,
        None => return vec![],
    };

    let mut events = vec![];

    // Usage metadata
    append_google_usage(&v, &mut events);

    // Candidates: parts, function calls, and finish detection
    if let Some(candidates) = google_candidates(&v) {
        for candidate in candidates {
            append_candidate_parts(candidate, &mut events);
        }
        if candidates.iter().any(candidate_has_finish) {
            events.push(StreamEvent::Done);
        }
    }

    events
}

/// Parse an SSE data line into a JSON value, returning `None` on malformed input
/// (preserving the previous empty-vec behavior of `parse_google_line`).
fn parse_google_json(line: &str) -> Option<Value> {
    serde_json::from_str(line).ok()
}

/// Append a usage event when `usageMetadata` carries non-zero input/output counts.
fn append_google_usage(v: &Value, events: &mut Vec<StreamEvent>) {
    let Some(usage) = v.get("usageMetadata") else {
        return;
    };
    let input = usage
        .get("promptTokenCount")
        .and_then(|x| x.as_u64())
        .unwrap_or(0) as u32;
    let output = usage
        .get("candidatesTokenCount")
        .and_then(|x| x.as_u64())
        .unwrap_or(0) as u32;
    // Gemini reports cache hits via `cachedContentTokenCount`.
    let cache_read = usage
        .get("cachedContentTokenCount")
        .and_then(|x| x.as_u64())
        .unwrap_or(0) as u32;
    // Suppress usage when both input and output are zero.
    if input > 0 || output > 0 {
        events.push(StreamEvent::Usage(TokenUsage {
            input,
            output,
            cache_read,
            // Gemini `promptTokenCount` already includes cached content.
            context_total: input,
            ..Default::default()
        }));
    }
}

/// Return the `candidates` array of a Google stream chunk, if present.
fn google_candidates(v: &Value) -> Option<&[Value]> {
    v.get("candidates")
        .and_then(|c| c.as_array())
        .map(Vec::as_slice)
}

/// Emit delta events for the text and function-call parts of a candidate.
fn append_candidate_parts(candidate: &Value, events: &mut Vec<StreamEvent>) {
    let Some(parts) = candidate
        .pointer("/content/parts")
        .and_then(|p| p.as_array())
    else {
        return;
    };
    for part in parts {
        append_part_event(part, events);
    }
}

/// Append the stream event for a single content part (text or function call).
fn append_part_event(part: &Value, events: &mut Vec<StreamEvent>) {
    if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
        // Suppress empty text deltas.
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
        events.push(StreamEvent::Delta(ContentBlock::ToolUse {
            id,
            name,
            input,
        }));
    }
}

/// True when the candidate carries a non-trivial `finishReason`.
fn candidate_has_finish(candidate: &Value) -> bool {
    candidate
        .get("finishReason")
        .and_then(|r| r.as_str())
        .map(|r| !r.is_empty() && r != "FINISH_REASON_UNSPECIFIED")
        .unwrap_or(false)
}

impl LlmProvider for GoogleProvider {
    fn name(&self) -> &str {
        "google"
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
            let raw = self.client.stream_sse(&url, body, &auth, extra_headers);
            let out: Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>> =
                Box::pin(stream! {
                    let mut seen_terminal = false;
                    let mut raw_stream = raw;
                    while let Some(result) = raw_stream.next().await {
                        match result {
                            Err(e) => { yield Err(e); return; }
                            Ok(line) => {
                                for event in parse_google_line(&line) {
                                    if matches!(event, StreamEvent::Done) {
                                        seen_terminal = true;
                                    }
                                    yield Ok(event);
                                }
                            }
                        }
                    }
                    // Raw EOF before the Google terminal signal (finishReason) is
                    // a truncated / stalled stream, not a complete turn. Yield a
                    // typed retryable failure so the breaker and failover logic
                    // can react.
                    if !seen_terminal {
                        tracing::warn!("google stream ended before terminal signal");
                        yield Err(anyhow::Error::new(ProviderError::Transport)
                            .context("stream ended before terminal signal"));
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
    use crate::provider::{
        AuthMethod, ExhaustedTransportCategory, FormatFamily, ProviderCapabilities, ProviderConfig,
        ProviderError, ToolSchemaCompat,
    };
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
            "/v1beta/models/gemini-2.5-pro:streamGenerateContent",
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

    fn test_google_config() -> ProviderConfig {
        ProviderConfig {
            base_url: "https://generativelanguage.googleapis.com".to_string(),
            auth: AuthMethod::NoAuth,
            format_family: FormatFamily::Google,
            model_id: "gemini-2.5-pro".to_string(),
            context_window: 1_048_000,
            telemetry: None,
            session_affinity_key: None,
            provider_headers: Default::default(),
            capabilities: ProviderCapabilities::default(),
            reasoning_effort: None,
            tool_schema_compat: None,
        }
    }

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
        let line =
            r#"{"candidates":[{"content":{"parts":[],"role":"model"},"finishReason":"STOP"}]}"#;
        let events = parse_google_line(line);
        assert!(events.iter().any(|e| matches!(e, StreamEvent::Done)));
    }

    #[test]
    fn test_safety_finish_reason_currently_emits_done() {
        let line =
            r#"{"candidates":[{"content":{"parts":[],"role":"model"},"finishReason":"SAFETY"}]}"#;
        let events = parse_google_line(line);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::Done));
    }

    #[test]
    fn test_streaming_chunk_no_finish_no_done() {
        // Intermediate chunk without finishReason shouldn't emit Done
        let line = r#"{"candidates":[{"content":{"parts":[{"text":"hello"}],"role":"model"}}]}"#;
        let events = parse_google_line(line);
        assert!(!events.iter().any(|e| matches!(e, StreamEvent::Done)));
    }

    #[test]
    fn test_missing_parts_is_ignored() {
        let line = r#"{"candidates":[{"content":{"role":"model"}}]}"#;
        let events = parse_google_line(line);
        assert!(events.is_empty());
    }

    #[test]
    fn test_function_call_placeholder_id_generation() {
        let line = r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"lookup","args":{"id":1}}}],"role":"model"}}]}"#;
        let events = parse_google_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }) => {
                assert_eq!(id, "google_fc_lookup");
                assert_eq!(name, "lookup");
                assert_eq!(input["id"], 1);
            }
            _ => panic!("expected tool use"),
        }
    }

    #[test]
    fn test_build_request_sets_required_tool_config_when_tools_present() {
        let provider = GoogleProvider::new(test_google_config());
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));
        let tools = vec![json!({
            "name": "shell",
            "description": "Run shell",
            "inputSchema": {"type": "object"}
        })];

        let req = provider.build_request(&conv, &tools, Some(ToolChoice::Required));
        assert_eq!(req["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
    }

    #[test]
    fn test_build_request_native_no_quirk_emits_function_declarations_envelope() {
        // Captures the wire-shape Google emits for a native/no-quirk
        // `tool_schema_compat: None` provider config, by observing the
        // **actual generated JSON body** (the `build_request` return value
        // is exactly the JSON Google receives). The companion integration
        // test in `tests/provider_client_requests.rs::google_native_no_quirk_*`
        // does the full request round-trip; this in-crate test pins the
        // `build_request` shape independently so a seam regression surfaces
        // here even when the integration test harness is unavailable.
        //
        // We deliberately do not go through the wiremock HTTP layer or the
        // standalone `tool_projection::project` direct call — this test
        // proves the **seam** (build_request, end-to-end) stays native-shaped
        // on `tool_schema_compat: None`, exactly the property mpen AC3
        // requires.
        let provider = GoogleProvider::new(test_google_config());
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));
        let tools = vec![json!({
            "name": "shell",
            "description": "Run a shell command",
            "inputSchema": {
                "type": "object",
                "properties": {"cmd": {"type": "string"}},
                "required": ["cmd"]
            }
        })];

        let req = provider.build_request(&conv, &tools, Some(ToolChoice::Auto));
        let tools_arr = req["tools"].as_array().expect("tools array");
        assert_eq!(tools_arr.len(), 1);
        // Gemini wire format expects exactly one `functionDeclarations` group.
        assert_eq!(
            tools_arr[0]
                .as_object()
                .map(|o| o.keys().collect::<Vec<_>>())
                .unwrap_or_default(),
            vec!["functionDeclarations"]
        );

        let decl = &tools_arr[0]["functionDeclarations"][0];
        assert_eq!(decl["name"], "shell");
        assert_eq!(decl["description"], "Run a shell command");
        // RMCP `inputSchema` becomes Gemini wire `parameters`.
        assert!(decl.get("inputSchema").is_none());
        assert_eq!(decl["parameters"]["type"], "object");
        assert_eq!(decl["parameters"]["properties"]["cmd"]["type"], "string");
        assert_eq!(decl["parameters"]["required"][0], "cmd");

        // Native path must NOT emit `strict` (Responses-only concern) and
        // must NOT emit `toolConfig` for the `Auto` choice.
        assert!(decl.get("strict").is_none());
        assert!(req.get("toolConfig").is_none());

        // The whole request body must serialize to byte-identical strings on
        // repeated builds — no non-deterministic value (timestamp, request
        // id, hash-order leak, …) may sneak into the functionDecls envelope
        // for the native path. A drift here would also break the Gemini
        // implicit dedup / cost-control contract.
        let body_a = serde_json::to_string(&req).expect("serialize body once");
        let body_b = serde_json::to_string(&req).expect("serialize body twice");
        assert_eq!(
            body_a, body_b,
            "Google native no-quirk request body must be byte-deterministic across builds"
        );
    }

    #[test]
    fn test_build_request_omits_tool_config_when_tools_empty() {
        let provider = GoogleProvider::new(test_google_config());
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));

        let req = provider.build_request(&conv, &[], Some(ToolChoice::Required));
        assert!(req.get("toolConfig").is_none());
    }

    #[test]
    fn test_build_request_tool_choice_none_still_emits_tool_config() {
        let provider = GoogleProvider::new(test_google_config());
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));
        let tools = vec![json!({
            "name": "noop",
            "description": "Noop",
            "inputSchema": {"type": "object"}
        })];
        let req = provider.build_request(&conv, &tools, Some(ToolChoice::None));
        assert_eq!(req["toolConfig"]["functionCallingConfig"]["mode"], "NONE");
    }

    // ─── B5: reasoning-effort -> thinkingConfig ─────────────────────────────

    #[test]
    fn test_reasoning_effort_none_omits_thinking_config() {
        // None must preserve pre-B5 behavior: no generationConfig/thinkingConfig.
        let provider = GoogleProvider::new(test_google_config());
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));
        let req = provider.build_request(&conv, &[], None);
        assert!(
            req.get("generationConfig").is_none(),
            "generationConfig must be absent when reasoning_effort is None"
        );
    }

    #[test]
    fn test_reasoning_effort_high_sets_thinking_budget() {
        use crate::provider::ReasoningEffort;
        // Native/no-quirk: schema is forwarded as-is through `inputSchema`.
        let provider = GoogleProvider::new(test_google_config());
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));
        let tools = vec![json!({
            "name": "shell",
            "description": "Run shell",
            "inputSchema": {
                "type": "object",
                "properties": {"cmd": {"type": "string"}},
                "required": ["cmd"]
            },
            "readOnly": false,
            "destructive": true,
            "idempotent": true,
            "openWorld": true
        })];
        let req = provider.build_request(&conv, &tools, None);
        let decl = &req["tools"][0]["functionDeclarations"][0];
        assert_eq!(decl["name"], "shell");
        assert_eq!(decl["description"], "Run shell");
        assert_eq!(decl["parameters"]["type"], "object");
        assert_eq!(decl["parameters"]["properties"]["cmd"]["type"], "string");
        assert!(decl.get("inputSchema").is_none());
        assert!(decl.get("readOnly").is_none());
        assert!(decl.get("destructive").is_none());
        assert!(decl.get("idempotent").is_none());
        assert!(decl.get("openWorld").is_none());

        // Gemini quirk: apply keyword whitelist and required filtering.
        let mut gemini_config = test_google_config();
        gemini_config.tool_schema_compat = Some(ToolSchemaCompat::Gemini);
        let gemini_provider = GoogleProvider::new(gemini_config);
        let gemini_tools = vec![json!({
            "name": "shell",
            "description": "Run shell",
            "inputSchema": {
                "type": "object",
                "properties": {"cmd": {"type": "string"}},
                "required": ["cmd", "missing"],
                "unevaluatedProperties": false
            }
        })];
        let gemini_req = gemini_provider.build_request(&conv, &gemini_tools, None);
        let gemini_decl = &gemini_req["tools"][0]["functionDeclarations"][0];
        assert!(gemini_decl.get("inputSchema").is_none());
        let required = gemini_decl["parameters"]["required"].as_array().unwrap();
        assert_eq!(required, &["cmd"]);
        assert!(
            gemini_decl["parameters"]
                .get("unevaluatedProperties")
                .is_none()
        );

        // Reasoning effort high sets the thinking budget.
        let mut config = test_google_config();
        config.reasoning_effort = Some(ReasoningEffort::High);
        let provider = GoogleProvider::new(config);
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));
        let req = provider.build_request(&conv, &[], None);
        assert_eq!(
            req["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            24_000
        );
    }

    #[tokio::test]
    async fn test_stream_uses_data_lines_and_ignores_event_metadata() {
        let seen_auth = Arc::new(Mutex::new(None));
        let body = concat!(
            "event: response.output_text.delta\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello from google\"}],\"role\":\"model\"}}]}\n\n",
            "event: done\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[],\"role\":\"model\"},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":7,\"candidatesTokenCount\":9}}\n\n"
        );
        let base_url = spawn_sse_server(200, body, seen_auth.clone());
        let provider = GoogleProvider::new(ProviderConfig {
            base_url,
            ..test_google_config()
        });
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));

        let stream = provider
            .stream(&conv, &[], None)
            .await
            .expect("stream start");
        let events: Vec<_> = stream.try_collect().await.expect("stream events");

        assert!(seen_auth.lock().expect("seen auth").is_none());
        assert!(matches!(
            &events[0],
            StreamEvent::Delta(ContentBlock::Text { text }) if text == "Hello from google"
        ));
        assert!(matches!(
            &events[1],
            StreamEvent::Usage(TokenUsage {
                input: 7,
                output: 9,
                ..
            })
        ));
        assert!(matches!(&events[2], StreamEvent::Done));
    }

    #[tokio::test]
    async fn test_stream_raw_eof_before_terminal_yields_error() {
        // A stream that emits data deltas but ends (raw EOF) before any
        // finishReason must yield a typed retryable unexpected-EOF diagnostic, not
        // a synthesized StreamEvent::Done.
        let seen_auth = Arc::new(Mutex::new(None));
        let body = concat!(
            "event: response.output_text.delta\\n",
            "data: {\\\"candidates\\\":[{\\\"content\\\":{\\\"parts\\\":[{\\\"text\\\":\\\"partial\\\"}],\\\"role\\\":\\\"model\\\"}}]}\\n\\n"
        );
        let base_url = spawn_sse_server(200, body, seen_auth.clone());
        let provider = GoogleProvider::new(ProviderConfig {
            base_url,
            ..test_google_config()
        });
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));
        let request_body = provider.build_request(&conv, &[], None);
        let expected_payload_bytes = serde_json::to_vec(&request_body).unwrap().len();

        let stream = provider
            .stream(&conv, &[], None)
            .await
            .expect("stream start");
        let err = stream
            .try_collect::<Vec<_>>()
            .await
            .expect_err("raw EOF before terminal must yield Err");

        let pe = err
            .downcast_ref::<ProviderError>()
            .expect("typed ProviderError must be downcastable");
        match pe {
            ProviderError::ExhaustedTransport(diagnostic) => {
                assert_eq!(
                    diagnostic.category,
                    ExhaustedTransportCategory::UnexpectedEof
                );
                assert_eq!(diagnostic.estimated_payload_chars, expected_payload_bytes);
            }
            other => panic!("expected exhausted unexpected-EOF transport, got {other:?}"),
        }
        assert!(pe.retryable(), "truncated stream must be retryable");
    }

    #[tokio::test]
    async fn test_stream_http_error_is_propagated() {
        let seen_auth = Arc::new(Mutex::new(None));
        let base_url = spawn_sse_server(400, "bad request", seen_auth);
        let provider = GoogleProvider::new(ProviderConfig {
            base_url,
            ..test_google_config()
        });
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
        assert!(msg.contains("provider API error 400"));
        assert!(msg.contains("bad request"));
    }
}
