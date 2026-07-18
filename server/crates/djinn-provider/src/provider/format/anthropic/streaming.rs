use async_stream::stream;
use futures::StreamExt;
use serde_json::Value;
use std::collections::BTreeMap;
use std::pin::Pin;

use crate::message::{ContentBlock, Conversation};
#[allow(unused_imports)]
use crate::provider::client::ApiClient;
use crate::provider::{
    LlmProvider, ProviderConfig, ProviderError, StreamEvent, TokenUsage, ToolChoice,
};

use super::request::AnthropicProvider;

// ─── SSE parsing helpers ──────────────────────────────────────────────────────

/// State for content blocks that complete at a later `content_block_stop`.
///
/// Anthropic may interleave deltas for different content indices, so all
/// pending blocks are keyed by their wire index rather than by "last block".
#[derive(Default)]
pub(crate) struct ContentBlockAcc {
    blocks: BTreeMap<u64, PendingContentBlock>,
}

#[cfg(test)]
impl ContentBlockAcc {
    pub(crate) fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

enum PendingContentBlock {
    ToolUse {
        id: String,
        name: String,
        input_json: String,
    },
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
    Unknown {
        content_type: String,
        extra: serde_json::Map<String, Value>,
    },
}

/// Parse a single Anthropic SSE event (event_type + data JSON).
/// Mutates `block_acc` in place; caller owns it across calls.
pub(crate) fn parse_anthropic_event(
    event_type: &str,
    data: &str,
    block_acc: &mut ContentBlockAcc,
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
                let (Some(index), Some(content_block)) = (
                    v.get("index").and_then(Value::as_u64),
                    v.get("content_block").and_then(Value::as_object),
                ) else {
                    return events;
                };
                let pending = match content_block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                {
                    "text" => {
                        // Text content streams via `text_delta` events; the start
                        // frame carries at most an initial fragment. Never track a
                        // text block as pending Unknown state: the captured
                        // `{"type":"text","text":""}` would be replayed verbatim on
                        // the next request, which strict Anthropic-compatible
                        // endpoints reject with 400 "text content is empty".
                        if let Some(initial) = content_block.get("text").and_then(Value::as_str)
                            && !initial.is_empty()
                        {
                            events.push(StreamEvent::Delta(ContentBlock::Text {
                                text: initial.to_string(),
                            }));
                        }
                        return events;
                    }
                    "tool_use" => PendingContentBlock::ToolUse {
                        id: content_block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        name: content_block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        input_json: String::new(),
                    },
                    "thinking" => PendingContentBlock::Thinking {
                        thinking: content_block
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        signature: content_block
                            .get("signature")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    },
                    "redacted_thinking" => PendingContentBlock::RedactedThinking {
                        data: content_block
                            .get("data")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    },
                    content_type => PendingContentBlock::Unknown {
                        content_type: content_type.to_string(),
                        extra: content_block
                            .iter()
                            .filter(|(key, _)| key.as_str() != "type")
                            .map(|(key, value)| (key.clone(), value.clone()))
                            .collect(),
                    },
                };
                block_acc.blocks.insert(index, pending);
            }
        }

        "content_block_delta" => {
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                let index = v.get("index").and_then(Value::as_u64).unwrap_or(u64::MAX);
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
                            events.push(StreamEvent::ThinkingDelta {
                                id: index,
                                text: thinking.clone(),
                            });
                        }
                        if let Some(PendingContentBlock::Thinking {
                            thinking: accumulated,
                            ..
                        }) = block_acc.blocks.get_mut(&index)
                        {
                            accumulated.push_str(&thinking);
                        }
                    }
                    "signature_delta" => {
                        if let Some(PendingContentBlock::Thinking { signature, .. }) =
                            block_acc.blocks.get_mut(&index)
                            && let Some(delta) =
                                v.pointer("/delta/signature").and_then(Value::as_str)
                        {
                            signature.get_or_insert_with(String::new).push_str(delta);
                        }
                    }
                    "input_json_delta" => {
                        if let Some(PendingContentBlock::ToolUse { input_json, .. }) =
                            block_acc.blocks.get_mut(&index)
                            && let Some(frag) =
                                v.pointer("/delta/partial_json").and_then(|x| x.as_str())
                        {
                            input_json.push_str(frag);
                        }
                    }
                    _ => {
                        // Unknown block schemas have no typed delta representation.
                        // Keep provider-owned fields without overwriting start data.
                        if let Some(PendingContentBlock::Unknown { extra, .. }) =
                            block_acc.blocks.get_mut(&index)
                            && let Some(delta) = v.get("delta").and_then(Value::as_object)
                        {
                            for (key, value) in delta {
                                if key != "type" {
                                    extra.entry(key.clone()).or_insert_with(|| value.clone());
                                }
                            }
                        }
                    }
                }
            }
        }

        "content_block_stop" => {
            if let Ok(v) = serde_json::from_str::<Value>(data)
                && let Some(index) = v.get("index").and_then(Value::as_u64)
                && let Some(pending) = block_acc.blocks.remove(&index)
            {
                let block = match pending {
                    PendingContentBlock::ToolUse {
                        id,
                        name,
                        input_json,
                    } => ContentBlock::ToolUse {
                        id,
                        name,
                        input: serde_json::from_str(&input_json)
                            .unwrap_or(Value::Object(Default::default())),
                    },
                    PendingContentBlock::Thinking {
                        thinking,
                        signature,
                    } => {
                        // Completion is the single load-bearing representation
                        // of this block. Consumers atomically materialize its
                        // payload and record its ID; emitting a second
                        // Delta(Thinking) would create an exact-once gap.
                        events.push(StreamEvent::ThinkingBlockComplete {
                            id: index,
                            thinking,
                            signature,
                        });
                        return events;
                    }
                    PendingContentBlock::RedactedThinking { data } => {
                        ContentBlock::RedactedThinking { data }
                    }
                    PendingContentBlock::Unknown {
                        content_type,
                        extra,
                    } => ContentBlock::Unknown {
                        content_type,
                        extra,
                    },
                };
                events.push(StreamEvent::Delta(block));
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
                    let mut block_acc = ContentBlockAcc::default();
                    let mut input_tokens: u32 = 0;
                    let mut cache_read: u32 = 0;
                    let mut cache_write: u32 = 0;

                    // Anthropic SSE uses event: / data: pairs
                    // Our client currently yields only data: lines.
                    // We need to track event: lines too. Since ApiClient only yields data lines,
                    // we handle this by parsing event type from the data itself for Anthropic.
                    // The data JSON always has a "type" field.
                    let mut raw_stream = raw;
                    let mut seen_message_stop = false;
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
                                    for event in parse_anthropic_event(&event_type, &line, &mut block_acc, &mut input_tokens, &mut cache_read, &mut cache_write) {
                                        if matches!(event, StreamEvent::Done) {
                                            seen_message_stop = true;
                                        }
                                        yield Ok(event);
                                    }
                                }
                            }
                        }
                    }
                    // Raw EOF before the Anthropic terminal `message_stop` frame
                    // is a truncated / stalled stream, not a complete turn. Yield
                    // a typed retryable failure so the breaker and failover logic
                    // can react.
                    if !seen_message_stop {
                        tracing::warn!("anthropic stream ended before message_stop");
                        yield Err(anyhow::Error::new(ProviderError::Transport)
                            .context("stream ended before message_stop"));
                    }
                });
            Ok(out)
        })
    }
}
