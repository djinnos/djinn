use async_stream::stream;
use futures::StreamExt;
use serde_json::Value;
use std::pin::Pin;

use crate::message::{ContentBlock, Conversation};
#[allow(unused_imports)]
use crate::provider::client::ApiClient;
use crate::provider::{
    LlmProvider, ProviderConfig, ProviderError, StreamEvent, TokenUsage, ToolChoice,
};

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

        // `error` is handled out-of-band in the stream loop (it must surface as
        // a typed `Err(ProviderError)`, not a `StreamEvent`); see
        // `classify_anthropic_error_event`. `ping` and any unknown event types
        // carry no usable completion content and are dropped.
        _ => {}
    }

    events
}

/// Classify an Anthropic **in-stream** `error` SSE event into a typed
/// [`ProviderError`] plus a human-readable message.
///
/// Anthropic signals rate-limit / overload / refusal as an HTTP `200` SSE
/// stream carrying `event: error\ndata: {"type":"error","error":{"type":
/// "rate_limit_error","message":"..."}}` rather than a 4xx/5xx status. Dropping
/// it (the previous `_ => {}` behaviour) yielded zero `StreamEvent`s, so the
/// turn looked like an "empty/no-event turn" and the real failure never reached
/// `classify_provider_failure` / the per-(scope,model) health breaker.
///
/// We reuse [`ProviderError::from_stream_error`] — the same mapping the OpenAI
/// Responses streaming path uses for its mid-stream `error`/`response.failed`
/// events — passing Anthropic's `error.type` as the `code`. The substring
/// classifier covers the Anthropic vocabulary directly:
/// `rate_limit_error` → `RateLimit` (retryable), `overloaded_error`/`api_error`
/// → `ProviderInternal{500}` (retryable, server-side), `authentication_error`/
/// `permission_error` → `Authentication` (terminal), `invalid_request_error` →
/// `InvalidRequest` (terminal). A `retry-after` (ms) is folded in when present.
pub(crate) fn classify_anthropic_error_event(data: &str) -> (ProviderError, String) {
    let v: Value = serde_json::from_str(data).unwrap_or(Value::Null);
    let error = v.get("error");
    let err_type = error
        .and_then(|e| e.get("type"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let message = error
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("provider returned an error event")
        .to_string();

    let class = ProviderError::from_stream_error(err_type.as_deref(), &message);

    // Anthropic may include `error.retry_after` (seconds) on rate-limit events;
    // carry it through to the typed RateLimit so backoff can honour it.
    let class = match class {
        ProviderError::RateLimit { .. } => {
            let retry_after_ms = error
                .and_then(|e| e.get("retry_after"))
                .and_then(Value::as_f64)
                .map(|secs| (secs * 1000.0) as u64);
            class.with_retry_after(retry_after_ms)
        }
        other => other,
    };

    // Human-readable message rides along as anyhow context; prefix the
    // Anthropic error type when we have one (mirrors OpenAI's `code: message`).
    let display = match err_type {
        Some(t) => format!("{t}: {message}"),
        None => message,
    };

    (class, display)
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
                                    if event_type == "error" {
                                        // Anthropic signals rate-limit/overload/refusal as a
                                        // 200 SSE `error` event (not a 4xx/5xx status). Surface
                                        // it as a TYPED `ProviderError` so the reply loop reports
                                        // the real reason and the health breaker gets fed —
                                        // dropping it (old `_ => {}`) made the turn look "empty".
                                        // Preserve the typed error as the anyhow *source* (via
                                        // `.context`) so `consume_provider_stream`'s
                                        // `Err(e) => Err(e.context(..))` keeps it downcastable;
                                        // do NOT stringify it (that erases the type the breaker
                                        // classifies on).
                                        let (class, msg) = classify_anthropic_error_event(&line);
                                        tracing::error!(target: "djinn_provider::request", error = %msg, ?class, "anthropic stream error event");
                                        yield Err(anyhow::Error::new(class).context(msg));
                                        return;
                                    }
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
