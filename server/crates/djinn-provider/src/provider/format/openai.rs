use async_stream::stream;
use futures::StreamExt;
use reqwest::header::HeaderMap;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::pin::Pin;

use crate::message::{ContentBlock, Conversation, Role};
use crate::provider::client::{ApiClient, SseFrame};
use crate::provider::{
    FormatFamily, LlmProvider, ProviderConfig, ProviderError, StreamEvent, TokenUsage, ToolChoice,
    ToolSchemaCompat,
};

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
        let mut messages: Vec<Value> = Vec::new();

        for msg in &conversation.messages {
            let role = openai_role(msg.role.clone());
            let mut acc = BlockAccumulator::new();

            for block in &msg.content {
                acc.accumulate(block);
            }

            append_openai_messages(role, acc, &mut messages);
        }

        let mut body = json!({
            "model": self.config.model_id,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true}
        });

        if !tools.is_empty() {
            body["tools"] = json!(convert_tools_to_openai(
                tools,
                self.config.tool_schema_compat,
            ));
            apply_tool_choice(&mut body, tool_choice);
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

// ─── Role mapping ───────────────────────────────────────────────────────────────

fn openai_role(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

// ─── Content-block accumulator ────────────────────────────────────────────────

struct BlockAccumulator {
    text_blocks: Vec<Value>,
    tool_calls: Vec<Value>,
    tool_results: Vec<Value>,
}

impl BlockAccumulator {
    fn new() -> Self {
        Self {
            text_blocks: Vec::new(),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
        }
    }

    fn accumulate(&mut self, block: &ContentBlock) {
        match block {
            ContentBlock::Text { text } => {
                self.text_blocks.push(json!({"type": "text", "text": text}));
            }
            ContentBlock::Image { media_type, data } => {
                self.text_blocks.push(json!({
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:{media_type};base64,{data}")
                    }
                }));
            }
            ContentBlock::Document {
                media_type,
                data,
                filename,
            } => {
                self.text_blocks.push(json!({
                    "type": "file",
                    "file": {
                        "filename": filename.as_deref().unwrap_or("document"),
                        "file_data": format!("data:{media_type};base64,{data}")
                    }
                }));
            }
            ContentBlock::ToolUse { id, name, input } => {
                self.tool_calls.push(json!({
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
                let text = collect_tool_result_text(content);
                self.tool_results.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": text
                }));
            }
            ContentBlock::Thinking { .. }
            | ContentBlock::RedactedThinking { .. }
            | ContentBlock::Unknown { .. }
            | ContentBlock::OpenAIReasoning { .. } => {}
        }
    }
}

// ─── Text extraction from tool-result content ─────────────────────────────────

fn collect_tool_result_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|c| {
            if let ContentBlock::Text { text } = c {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("")
}

// ─── Message assembly ─────────────────────────────────────────────────────────

fn append_openai_messages(role: &'static str, acc: BlockAccumulator, messages: &mut Vec<Value>) {
    if !acc.tool_results.is_empty() {
        for tr in acc.tool_results {
            messages.push(tr);
        }
    } else if !acc.tool_calls.is_empty() {
        let mut assistant_msg = json!({"role": role});
        if !acc.text_blocks.is_empty() {
            let text = acc
                .text_blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("");
            assistant_msg["content"] = json!(text);
        } else {
            assistant_msg["content"] = Value::Null;
        }
        assistant_msg["tool_calls"] = json!(acc.tool_calls);
        messages.push(assistant_msg);
    } else if acc.text_blocks.len() == 1 {
        let text = acc.text_blocks[0]
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("");
        messages.push(json!({"role": role, "content": text}));
    } else {
        messages.push(json!({"role": role, "content": acc.text_blocks}));
    }
}

// ─── Tool conversion ────────────────────────────────────────────────────────────

fn convert_tools_to_openai(tools: &[Value], compat: Option<ToolSchemaCompat>) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            if t.get("type").is_some() && t.get("function").is_some() {
                // Already in OpenAI format.
                let mut out = t.clone();
                if let Some(params) = out.pointer_mut("/function/parameters") {
                    *params = crate::provider::format::tool_projection::project(
                        params.clone(),
                        compat,
                        FormatFamily::OpenAI,
                    );
                }
                out
            } else {
                let schema = crate::provider::format::tool_projection::project(
                    t.get("inputSchema")
                        .cloned()
                        .unwrap_or(json!({"type": "object"})),
                    compat,
                    FormatFamily::OpenAI,
                );
                json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name").cloned().unwrap_or(json!("")),
                        "description": t.get("description").cloned().unwrap_or(json!("")),
                        "parameters": schema,
                    }
                })
            }
        })
        .collect()
}

// ─── Tool-choice application ────────────────────────────────────────────────────

fn apply_tool_choice(body: &mut Value, tool_choice: Option<ToolChoice>) {
    match tool_choice.unwrap_or(ToolChoice::Auto) {
        ToolChoice::Auto => {}
        ToolChoice::Required => body["tool_choice"] = json!("required"),
        ToolChoice::None => body["tool_choice"] = json!("none"),
    }
}

// ─── Fireworks session affinity ─────────────────────────────────────────────────

fn is_fireworks_base_url(base_url: &str) -> bool {
    base_url.contains("fireworks.ai")
}

// ─── Schema helper ────────────────────────────────────────────────────────────

/// OpenAI requires `"properties"` on object schemas. Ensure it exists.
///
/// This is a thin compatibility wrapper over the shared projection core so the
/// existing `openai_responses.rs` call site keeps compiling.  The full
/// OpenAI-family rewrite (deep properties enforcement and top-level anyOf
/// flattening) lives in `tool_projection.rs`.
#[allow(dead_code)]
pub(super) fn ensure_object_properties(schema: Value) -> Value {
    crate::provider::format::tool_projection::project(
        schema,
        Some(crate::provider::ToolSchemaCompat::OpenAi),
        crate::provider::FormatFamily::OpenAI,
    )
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
    #[serde(default)]
    prompt_tokens_details: Option<TokenDetails>,
    #[serde(default)]
    completion_tokens_details: Option<TokenDetails>,
}

/// Chat-completions token-detail breakdown. Providers spell the cache field
/// differently (`cached_tokens` vs `cached_input_tokens`); reasoning models
/// expose `reasoning_tokens`. All optional — default missing fields to 0.
#[derive(Deserialize, Default)]
struct TokenDetails {
    #[serde(default)]
    cached_tokens: Option<u32>,
    #[serde(default)]
    cached_input_tokens: Option<u32>,
    #[serde(default)]
    reasoning_tokens: Option<u32>,
}

impl TokenDetails {
    fn cached(&self) -> u32 {
        self.cached_tokens.or(self.cached_input_tokens).unwrap_or(0)
    }
    fn reasoning(&self) -> u32 {
        self.reasoning_tokens.unwrap_or(0)
    }
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
        events.push(usage_event(usage));
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

        append_reasoning_delta(
            delta.reasoning_content,
            delta.reasoning_details,
            &mut events,
        );

        append_text_delta(delta.content, &mut events);

        // Tool calls — accumulate across chunks, keyed by index
        if let Some(tool_calls) = delta.tool_calls {
            for tc in tool_calls {
                accumulate_tool_call(tc, tool_acc);
            }
        }

        // On finish_reason="tool_calls", emit all accumulated tool uses
        if is_tool_calls_finish(choice.finish_reason.as_deref()) {
            append_finished_tool_calls(tool_acc, &mut events);
        }
    }

    events
}

// ─── Stream-parsing helpers ──────────────────────────────────────────────────

/// Convert an OpenAI `UsageChunk` into a `StreamEvent::Usage`.
///
/// Preserves the convention that `context_total` equals `prompt_tokens`
/// (which already includes cached tokens, so adding `cache_read` would
/// double-count).
fn usage_event(usage: UsageChunk) -> StreamEvent {
    let cache_read = usage
        .prompt_tokens_details
        .as_ref()
        .map(|d| d.cached())
        .unwrap_or(0);
    let reasoning_output = usage
        .completion_tokens_details
        .as_ref()
        .map(|d| d.reasoning())
        .unwrap_or(0);
    let input = usage.prompt_tokens.unwrap_or(0);
    StreamEvent::Usage(TokenUsage {
        input,
        output: usage.completion_tokens.unwrap_or(0),
        cache_read,
        cache_write: 0,
        reasoning_output,
        context_total: input,
    })
}

/// Emit a `StreamEvent::Thinking` for reasoning/thinking content from
/// models like Kimi K2.5, DeepSeek-R1, or GLM-4.7. Preserves the
/// `reasoning_content`-first precedence and empty-string suppression.
fn append_reasoning_delta(
    reasoning_content: Option<String>,
    reasoning_details: Option<String>,
    events: &mut Vec<StreamEvent>,
) {
    let thinking = reasoning_content
        .or(reasoning_details)
        .filter(|s| !s.is_empty());
    if let Some(text) = thinking {
        events.push(StreamEvent::Thinking(text));
    }
}

/// Emit a `StreamEvent::Delta(Text)` for non-empty text content.
fn append_text_delta(content: Option<String>, events: &mut Vec<StreamEvent>) {
    if let Some(text) = content
        && !text.is_empty()
    {
        events.push(StreamEvent::Delta(ContentBlock::Text { text }));
    }
}

/// Accumulate a single `DeltaToolCall` into the tool-call accumulator.
///
/// Preserves current behavior: default index to 0, non-empty `id`/`name`
/// replacement for existing entries, and argument fragment append.
fn accumulate_tool_call(tc: DeltaToolCall, tool_acc: &mut BTreeMap<u32, (String, String, String)>) {
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
        tool_acc.insert(
            idx,
            (
                tc.id.unwrap_or_default(),
                func.name.unwrap_or_default(),
                func.arguments.unwrap_or_default(),
            ),
        );
    }
}

/// Check whether a finish-reason signals tool-call completion.
fn is_tool_calls_finish(reason: Option<&str>) -> bool {
    matches!(reason, Some("tool_calls"))
}

/// Drain all accumulated tool calls (sorted by index via `BTreeMap`) and
/// emit each as a `StreamEvent::Delta(ToolUse)`.
fn append_finished_tool_calls(
    tool_acc: &mut BTreeMap<u32, (String, String, String)>,
    events: &mut Vec<StreamEvent>,
) {
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
        // Tolerant multi-spelling fallback: providers differ on the cache key
        // (`cached_tokens` vs `cached_input_tokens`); missing → 0.
        let cache_read = usage
            .get("prompt_tokens_details")
            .and_then(|d| {
                d.get("cached_tokens")
                    .or_else(|| d.get("cached_input_tokens"))
            })
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u32;
        let reasoning_output = usage
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u32;
        events.push(StreamEvent::Usage(TokenUsage {
            input,
            output,
            cache_read,
            cache_write: 0,
            reasoning_output,
            // `prompt_tokens` already includes cached tokens (see streaming path).
            context_total: input,
        }));
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
                    let mut tool_acc: BTreeMap<u32, (String, String, String)> = BTreeMap::new();
                    let mut raw_stream = raw;
                    let mut seen_done = false;
                    while let Some(result) = raw_stream.next().await {
                        match result {
                            Err(e) => { yield Err(e); return; }
                            Ok(frame) => match frame {
                                SseFrame::Data(line) => {
                                    for event in parse_openai_line(&line, &mut tool_acc) {
                                        yield Ok(event);
                                    }
                                }
                                SseFrame::Done => {
                                    seen_done = true;
                                    // Truncated tool/function accumulator: the
                                    // provider sent [DONE] but we still have
                                    // incomplete tool calls. Fail typed rather
                                    // than emitting partial tool output.
                                    if !tool_acc.is_empty() {
                                        tracing::warn!(
                                            tool_count = tool_acc.len(),
                                            "openai stream [DONE] with incomplete tool calls"
                                        );
                                        tool_acc.clear();
                                        yield Err(anyhow::Error::new(ProviderError::Transport)
                                            .context("openai stream ended with incomplete tool/function call accumulator"));
                                        return;
                                    }
                                    yield Ok(StreamEvent::Done);
                                }
                            }
                        }
                    }
                    // Raw EOF before [DONE] is a truncated / stalled stream.
                    // Yield a typed retryable failure.
                    if !seen_done {
                        // Discard any partial tool accumulator state.
                        if !tool_acc.is_empty() {
                            tracing::warn!(
                                tool_count = tool_acc.len(),
                                "openai stream EOF with incomplete tool calls (no [DONE])"
                            );
                            tool_acc.clear();
                        }
                        tracing::warn!("openai stream ended before [DONE]");
                        yield Err(anyhow::Error::new(ProviderError::Transport)
                            .context("openai stream ended before [DONE]"));
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
            reasoning_effort: None,
            tool_schema_compat: None,
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
            reasoning_effort: None,
            tool_schema_compat: None,
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
            reasoning_effort: None,
            tool_schema_compat: None,
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
    fn test_build_request_native_no_quirk_preserves_chat_completions_envelope() {
        // Captures the wire-shape OpenAI chat-completions emits for a native
        // (`tool_schema_compat: None`) provider config, by observing the
        // **actual generated JSON body** (`build_request` is exactly the
        // JSON OpenAI receives). The companion integration test in
        // `tests/provider_client_requests.rs::openai_chat_completions_native_*`
        // does the full request round-trip; this in-crate test pins the
        // `build_request` shape independently so a seam regression surfaces
        // here even when the integration test harness is unavailable.
        let provider = OpenAIProvider::new(test_openai_config());
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
        let tool = &req["tools"][0];
        // OpenAI Chat Completions envelope: nested `function` with the
        // parameters schema. NO `strict` flag — that's a Responses-only
        // concern. NO Anthropic-style `input_schema` alias.
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], "shell");
        assert_eq!(tool["function"]["description"], "Run a shell command");
        assert_eq!(tool["function"]["parameters"]["type"], "object");
        assert_eq!(
            tool["function"]["parameters"]["properties"]["cmd"]["type"],
            "string"
        );
        assert_eq!(tool["function"]["parameters"]["required"][0], "cmd");
        assert!(tool["function"].get("strict").is_none());
        assert!(tool.get("input_schema").is_none());

        // Byte-determinism: the whole body must serialize to identical
        // strings across repeated builds. A drift here would also break the
        // implicit cache-friendly contract on the OpenAI side and would
        // mean a non-deterministic value (timestamp, hash order, id) leaked
        // into the native path that the mpen AC3 says must stay stable.
        let body_a = serde_json::to_string(&req).expect("serialize body once");
        let body_b = serde_json::to_string(&req).expect("serialize body twice");
        assert_eq!(
            body_a, body_b,
            "OpenAI chat-completions native no-quirk request must be byte-deterministic across builds"
        );
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

    // ─── E2E: OpenAI formatting ignores Anthropic cache metadata ──────────────

    /// E2E: when a system message carries Anthropic-style cache metadata and
    /// multi-block content (as produced by build_system_message for an Anthropic
    /// model), OpenAI formatting flattens it into a single system message with
    /// concatenated text — no cache_control, no array blocks.
    #[test]
    fn e2e_openai_ignores_anthropic_cache_metadata_on_system_message() {
        use crate::message::{CacheBreakpoint, ContentBlock, MessageMeta};

        let provider = OpenAIProvider::new(test_openai_config());

        // Build a system message with the same structure that build_system_message
        // would produce for an Anthropic model with repo map present.
        let sys_msg = Message {
            role: Role::System,
            content: vec![
                ContentBlock::Text {
                    text: "You are a helpful assistant.".to_string(),
                },
                ContentBlock::Text {
                    text: "## Current Project\nDemo project".to_string(),
                },
                ContentBlock::Text {
                    text: "## Repository Map\nsrc/lib.rs\n  pub fn run()".to_string(),
                },
                ContentBlock::Text {
                    text: "Be concise.".to_string(),
                },
            ],
            metadata: Some(MessageMeta {
                input_tokens: None,
                output_tokens: None,
                timestamp: None,
                provider_data: Some(serde_json::json!({
                    "anthropic_cache_breakpoint": CacheBreakpoint {
                        kind: Some("stable_prefix".to_string()),
                    }
                })),
            }),
        };

        let mut conv = Conversation::new();
        conv.push(sys_msg);
        conv.push(Message::user("Hello"));

        let req = provider.build_request(&conv, &[], None);
        let messages = req["messages"].as_array().expect("messages array");

        // OpenAI format: system message is a regular message, no cache_control anywhere
        let system_msg = &messages[0];
        assert_eq!(system_msg["role"], "system");

        // Content should be a plain array of text blocks without cache_control
        let content = system_msg["content"].as_array().expect("content array");
        for block in content {
            assert!(
                block.get("cache_control").is_none(),
                "OpenAI system blocks must not contain cache_control: {block}"
            );
        }

        // The text content is preserved
        let texts: Vec<&str> = content.iter().filter_map(|b| b["text"].as_str()).collect();
        assert!(texts.iter().any(|t| t.contains("helpful assistant")));
        assert!(texts.iter().any(|t| t.contains("Repository Map")));
    }

    // ─── Tool-schema projection at serialization seam ────────────────────

    /// A config with `tool_schema_compat: Some(OpenAi)` to exercise the
    /// shared projection path.
    fn openai_compat_config() -> ProviderConfig {
        ProviderConfig {
            tool_schema_compat: Some(ToolSchemaCompat::OpenAi),
            ..test_openai_config()
        }
    }

    /// A config with `tool_schema_compat: Some(Moonshot)` for
    /// Moonshot-via-OpenAI-format provider.
    fn moonshot_compat_config() -> ProviderConfig {
        ProviderConfig {
            tool_schema_compat: Some(ToolSchemaCompat::Moonshot),
            ..test_openai_config()
        }
    }

    /// When `tool_schema_compat` is `None`, the RMCP inputSchema is forwarded
    /// verbatim — no `properties` key is injected.
    #[test]
    fn native_none_compat_preserves_rmcp_schema_verbatim() {
        let provider = OpenAIProvider::new(test_openai_config()); // compat = None
        let mut conv = Conversation::new();
        conv.push(Message::user("hi"));

        // An RMCP-shaped tool whose inputSchema is a bare object without `properties`.
        let tools = vec![json!({
            "name": "shell",
            "description": "Run shell",
            "inputSchema": {"type": "object"}
        })];

        let req = provider.build_request(&conv, &tools, None);
        let tool = &req["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], "shell");

        // No `properties` key should be added under None compat.
        let params = &tool["function"]["parameters"];
        assert!(
            params.get("properties").is_none(),
            "None compat must not inject properties: {params}"
        );
    }

    /// When `tool_schema_compat` is `None`, an already-OpenAI-shaped tool is
    /// forwarded verbatim (no projection changes).
    #[test]
    fn native_none_compat_preserves_already_openai_tool_verbatim() {
        let provider = OpenAIProvider::new(test_openai_config()); // compat = None
        let mut conv = Conversation::new();
        conv.push(Message::user("hi"));

        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "read",
                "description": "Read a file",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }
            }
        })];

        let req = provider.build_request(&conv, &tools, None);
        let params = &req["tools"][0]["function"]["parameters"];
        // Must be exactly what was passed in.
        assert_eq!(
            params,
            &json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            })
        );
    }

    /// With `tool_schema_compat: Some(OpenAi)`, the shared projection
    /// enforces `properties` on object schemas for RMCP-shaped tools.
    #[test]
    fn openai_compat_projects_rmcp_input_schema() {
        let provider = OpenAIProvider::new(openai_compat_config());
        let mut conv = Conversation::new();
        conv.push(Message::user("hi"));

        let tools = vec![json!({
            "name": "shell",
            "description": "Run shell",
            "inputSchema": {"type": "object"}
        })];

        let req = provider.build_request(&conv, &tools, None);
        let params = &req["tools"][0]["function"]["parameters"];
        // OpenAI compat injects `properties: {}` on bare object schemas.
        assert_eq!(
            params,
            &json!({"type": "object", "properties": {}}),
            "OpenAI compat must enforce properties on object schemas"
        );
    }

    /// With `tool_schema_compat: Some(OpenAi)`, the shared projection
    /// enforces `properties` on already-OpenAI-shaped tool parameters.
    #[test]
    fn openai_compat_projects_already_openai_tool_parameters() {
        let provider = OpenAIProvider::new(openai_compat_config());
        let mut conv = Conversation::new();
        conv.push(Message::user("hi"));

        // Already-OpenAI tool with a nested object missing properties.
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "search",
                "description": "Search",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "filter": {
                            "type": "object"
                            // Missing `properties` — should be injected.
                        }
                    }
                }
            }
        })];

        let req = provider.build_request(&conv, &tools, None);
        let params = &req["tools"][0]["function"]["parameters"];
        let filter = &params["properties"]["filter"];
        assert!(
            filter.get("properties").is_some(),
            "Deep object must gain properties: {filter}"
        );
    }

    /// A non-OpenAI compat (Moonshot) is also threaded through the
    /// projection path for RMCP tools via the same seam.
    #[test]
    fn moonshot_compat_applied_via_openai_seam() {
        let provider = OpenAIProvider::new(moonshot_compat_config());
        let mut conv = Conversation::new();
        conv.push(Message::user("hi"));

        // An RMCP tool with prefixItems (Moonshot collapses these).
        let tools = vec![json!({
            "name": "pick",
            "description": "Pick items",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "coords": {
                        "type": "array",
                        "prefixItems": [
                            {"type": "number"},
                            {"type": "number"}
                        ]
                    }
                }
            }
        })];

        let req = provider.build_request(&conv, &tools, None);
        let params = &req["tools"][0]["function"]["parameters"];
        let coords = &params["properties"]["coords"];
        // Moonshot compat collapses prefixItems → items.
        assert!(
            coords.get("prefixItems").is_none(),
            "Moonshot must collapse prefixItems: {coords}"
        );
        assert!(
            coords.get("items").is_some(),
            "Moonshot must produce items: {coords}"
        );
    }

    // ─── Provider-internal thinking block skip guards for OpenAI format ───────
    // Anthropic signed thinking, redacted thinking, and unknown passthrough
    // blocks must not leak into OpenAI-style request serialization as empty text
    // or any other representation. (Native Anthropic replay is owned by `xw13`.)

    #[test]
    fn test_build_request_drops_anthropic_thinking_and_unknown_blocks() {
        let provider = OpenAIProvider::new(test_openai_config());
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
        let messages = req["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 1);
        // OpenAI chat-completions collapses a single visible text block into a string.
        assert_eq!(messages[0]["content"], "visible output");
    }

    #[test]
    fn test_build_request_all_internal_blocks_dropped_with_empty_assistant() {
        let provider = OpenAIProvider::new(test_openai_config());
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
        let content = &req["messages"][0]["content"];
        assert_eq!(content.as_array().map(|a| a.len()).unwrap_or(usize::MAX), 0);
        assert!(content.as_str() != Some(""));
    }
}
