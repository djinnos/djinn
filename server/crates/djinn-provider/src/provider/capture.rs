//! Safe raw Anthropic-compatible SSE capture helpers.
//!
//! This module is intentionally small and operator-facing: it assembles the same
//! Anthropic `/v1/messages` request body as the normal provider path, streams raw
//! `data:` frames through [`crate::provider::client::ApiClient`], and writes only
//! sanitized request metadata plus redacted frame payloads to an artifact.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, anyhow};
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::message::{Conversation, Message};
use crate::provider::error::redact_secrets;
use crate::provider::format::anthropic::AnthropicProvider;
use crate::provider::{
    AuthMethod, FormatFamily, ProviderCapabilities, ProviderConfig, ReasoningEffort,
};

const REDACTED: &str = "[REDACTED]";

/// Inputs for an Anthropic-compatible raw SSE capture.
#[derive(Clone)]
pub struct AnthropicSseCaptureConfig {
    pub base_url: String,
    pub model: String,
    pub auth: AuthMethod,
    pub prompt: String,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub max_tokens: u32,
    pub provider_headers: BTreeMap<String, String>,
}

/// The sanitized local capture artifact written by this utility.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnthropicSseCaptureArtifact {
    pub artifact_version: u32,
    pub created_at: String,
    pub request: SanitizedRequestMetadata,
    /// Redacted raw payloads yielded from SSE `data:` lines, excluding `[DONE]`.
    pub data_frames: Vec<CapturedDataFrame>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SanitizedRequestMetadata {
    pub provider_format: String,
    pub model: String,
    pub base_url: String,
    pub path: String,
    pub max_tokens: u32,
    pub stream: bool,
    pub thinking_requested: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_budget_tokens: Option<u32>,
    pub auth: SanitizedAuthMetadata,
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SanitizedAuthMetadata {
    pub kind: String,
    pub redacted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapturedDataFrame {
    pub index: usize,
    pub data: String,
}

/// Offline classification of whether a raw Anthropic-compatible capture
/// surfaced model reasoning, and if so whether it used Anthropic's structured
/// thinking stream shape or leaked inline `<think>` tags as ordinary text.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AnthropicThinkingStreamClassification {
    /// At least one frame used Anthropic's structured thinking block/delta
    /// shape (`content_block.type = "thinking"` or
    /// `delta.type = "thinking_delta"`).
    StructuredThinking,
    /// No structured thinking frame was observed, but a text delta contained
    /// inline `<think>`/`</think>` tags.
    InlineThinkTags,
    /// No structured thinking or inline think-tag evidence was observed.
    NoReasoningObserved,
}

/// Classify a sanitized raw Anthropic/MiniMax SSE capture artifact.
///
/// The input is the exact artifact/frame shape emitted by
/// [`capture_anthropic_sse`] and [`dry_run_anthropic_sse_capture`]: each
/// [`CapturedDataFrame::data`] value is a raw SSE `data:` JSON payload after
/// redaction. This helper intentionally does not alter runtime completion
/// behavior; it is for fixture-backed tests, runbook guidance, and future
/// planner decisions about whether an inline think-tag fallback is required.
pub fn classify_anthropic_thinking_stream(
    artifact: &AnthropicSseCaptureArtifact,
) -> AnthropicThinkingStreamClassification {
    let mut saw_inline_think_tags = false;

    for frame in &artifact.data_frames {
        let Ok(value) = serde_json::from_str::<Value>(&frame.data) else {
            continue;
        };

        let content_block_type = value.pointer("/content_block/type").and_then(Value::as_str);
        let delta_type = value.pointer("/delta/type").and_then(Value::as_str);
        if content_block_type == Some("thinking") || delta_type == Some("thinking_delta") {
            return AnthropicThinkingStreamClassification::StructuredThinking;
        }

        if delta_type == Some("text_delta")
            && value
                .pointer("/delta/text")
                .and_then(Value::as_str)
                .is_some_and(contains_inline_think_tag)
        {
            saw_inline_think_tags = true;
        }
    }

    if saw_inline_think_tags {
        AnthropicThinkingStreamClassification::InlineThinkTags
    } else {
        AnthropicThinkingStreamClassification::NoReasoningObserved
    }
}

fn contains_inline_think_tag(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("<think>") || text.contains("</think>")
}

impl AnthropicSseCaptureConfig {
    pub fn provider_config(&self) -> ProviderConfig {
        ProviderConfig {
            base_url: self.base_url.clone(),
            auth: self.auth.clone(),
            format_family: FormatFamily::Anthropic,
            model_id: self.model.clone(),
            context_window: 200_000,
            telemetry: None,
            session_affinity_key: None,
            provider_headers: self.provider_headers.clone().into_iter().collect(),
            capabilities: ProviderCapabilities {
                streaming: true,
                max_tokens_default: Some(self.max_tokens),
            },
            reasoning_effort: self.reasoning_effort,
            tool_schema_compat: None,
        }
    }
}

/// Capture raw Anthropic-compatible SSE `data:` frames and return a sanitized
/// artifact. The artifact never includes bearer/API-key values.
pub async fn capture_anthropic_sse(
    config: AnthropicSseCaptureConfig,
) -> anyhow::Result<AnthropicSseCaptureArtifact> {
    let (provider, body, url, headers) = build_request_parts(&config)?;
    let mut stream = provider
        .client
        .stream_sse(&url, body.clone(), &config.auth, headers.clone());

    let secret_values = secret_values(&config.auth, &headers);
    let secret_refs: Vec<&str> = secret_values.iter().map(String::as_str).collect();
    let mut data_frames = Vec::new();
    while let Some(frame) = stream.next().await {
        let frame = frame?;
        data_frames.push(CapturedDataFrame {
            index: data_frames.len(),
            data: redact_secrets(&frame, &secret_refs),
        });
    }

    Ok(AnthropicSseCaptureArtifact {
        artifact_version: 1,
        created_at: OffsetDateTime::now_utc().format(&Rfc3339)?,
        request: sanitized_request_metadata(&config, &url, &body, &headers),
        data_frames,
    })
}

/// Capture and write a pretty-printed JSON artifact to `output_path`.
pub async fn capture_anthropic_sse_to_file(
    config: AnthropicSseCaptureConfig,
    output_path: impl AsRef<Path>,
) -> anyhow::Result<AnthropicSseCaptureArtifact> {
    let artifact = capture_anthropic_sse(config).await?;
    let json = serde_json::to_string_pretty(&artifact)?;
    tokio::fs::write(output_path.as_ref(), json)
        .await
        .with_context(|| {
            format!(
                "write capture artifact to {}",
                output_path.as_ref().display()
            )
        })?;
    Ok(artifact)
}

/// Build the request and sanitized metadata without performing network I/O.
/// Useful for tests and for operators validating the request shape before using
/// real credentials.
pub fn dry_run_anthropic_sse_capture(
    config: &AnthropicSseCaptureConfig,
) -> anyhow::Result<AnthropicSseCaptureArtifact> {
    let (_provider, body, url, headers) = build_request_parts(config)?;
    Ok(AnthropicSseCaptureArtifact {
        artifact_version: 1,
        created_at: OffsetDateTime::now_utc().format(&Rfc3339)?,
        request: sanitized_request_metadata(config, &url, &body, &headers),
        data_frames: Vec::new(),
    })
}

fn build_request_parts(
    config: &AnthropicSseCaptureConfig,
) -> anyhow::Result<(AnthropicProvider, Value, String, HeaderMap)> {
    if config.base_url.trim().is_empty() {
        return Err(anyhow!("base URL is required"));
    }
    if config.model.trim().is_empty() {
        return Err(anyhow!("model is required"));
    }
    if config.prompt.trim().is_empty() {
        return Err(anyhow!("prompt is required"));
    }
    if config.max_tokens < 2 {
        return Err(anyhow!(
            "max_tokens must be at least 2 when thinking may be requested"
        ));
    }

    let provider = AnthropicProvider::new(config.provider_config());
    let mut conversation = Conversation::new();
    conversation.push(Message::user(config.prompt.clone()));
    let body = provider.build_request(&conversation, &[], None);
    let url = provider.effective_url();
    let headers = provider.extra_headers();
    Ok((provider, body, url, headers))
}

fn sanitized_request_metadata(
    config: &AnthropicSseCaptureConfig,
    url: &str,
    body: &Value,
    headers: &HeaderMap,
) -> SanitizedRequestMetadata {
    let thinking = body.get("thinking");
    SanitizedRequestMetadata {
        provider_format: "anthropic_messages".to_string(),
        model: config.model.clone(),
        base_url: config.base_url.clone(),
        path: reqwest::Url::parse(url)
            .map(|url| url.path().to_string())
            .unwrap_or_else(|_| "/v1/messages".to_string()),
        max_tokens: body
            .get("max_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(config.max_tokens as u64) as u32,
        stream: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        thinking_requested: thinking
            .and_then(|thinking| thinking.get("type"))
            .and_then(Value::as_str)
            == Some("enabled"),
        reasoning_effort: config.reasoning_effort,
        thinking_budget_tokens: thinking
            .and_then(|thinking| thinking.get("budget_tokens"))
            .and_then(Value::as_u64)
            .map(|budget| budget as u32),
        auth: sanitized_auth_metadata(&config.auth),
        headers: sanitized_headers(&config.auth, headers),
    }
}

fn sanitized_auth_metadata(auth: &AuthMethod) -> SanitizedAuthMetadata {
    match auth {
        AuthMethod::BearerToken(_) => SanitizedAuthMetadata {
            kind: "bearer".to_string(),
            redacted: true,
        },
        AuthMethod::ApiKeyHeader { header, .. } => SanitizedAuthMetadata {
            kind: format!("api_key_header:{header}"),
            redacted: true,
        },
        AuthMethod::NoAuth => SanitizedAuthMetadata {
            kind: "none".to_string(),
            redacted: false,
        },
    }
}

fn sanitized_headers(auth: &AuthMethod, headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut sanitized = BTreeMap::new();
    match auth {
        AuthMethod::BearerToken(_) => {
            sanitized.insert("authorization".to_string(), REDACTED.to_string());
        }
        AuthMethod::ApiKeyHeader { header, .. } => {
            sanitized.insert(header.to_ascii_lowercase(), REDACTED.to_string());
        }
        AuthMethod::NoAuth => {}
    }

    for (name, value) in headers {
        let key = name.as_str().to_ascii_lowercase();
        let value = value.to_str().unwrap_or("<non-utf8>");
        let value = if is_secret_header_name(&key) {
            REDACTED.to_string()
        } else {
            value.to_string()
        };
        sanitized.insert(key, value);
    }
    sanitized
}

fn secret_values(auth: &AuthMethod, headers: &HeaderMap) -> Vec<String> {
    let mut secrets = match auth {
        AuthMethod::BearerToken(token) => vec![token.clone(), format!("Bearer {token}")],
        AuthMethod::ApiKeyHeader { key, .. } => vec![key.clone()],
        AuthMethod::NoAuth => Vec::new(),
    };

    for (name, value) in headers {
        if is_secret_header_name(name.as_str())
            && let Ok(value) = value.to_str()
        {
            secrets.push(value.to_string());
        }
    }
    secrets
}

fn is_secret_header_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "authorization"
            | "proxy-authorization"
            | "x-api-key"
            | "api-key"
            | "anthropic-api-key"
            | "x-minimax-api-key"
            | "helicone-auth"
    ) || name.contains("secret")
        || name.contains("token")
        || (name.contains("key") && name != "idempotency-key")
}

/// Parse a `Name: value` CLI/env header pair.
pub fn parse_header_pair(header: &str) -> anyhow::Result<(String, String)> {
    let (name, value) = header
        .split_once(':')
        .ok_or_else(|| anyhow!("header must be in 'Name: value' form"))?;
    HeaderName::from_bytes(name.trim().as_bytes()).with_context(|| {
        format!(
            "invalid header name '{}': not an HTTP header token",
            name.trim()
        )
    })?;
    HeaderValue::from_str(value.trim()).with_context(|| {
        format!(
            "invalid value for header '{}': not an HTTP header value",
            name.trim()
        )
    })?;
    Ok((name.trim().to_string(), value.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::post};

    fn test_config(base_url: String) -> AnthropicSseCaptureConfig {
        AnthropicSseCaptureConfig {
            base_url,
            model: "MiniMax-M3".to_string(),
            auth: AuthMethod::BearerToken("sk-live-secret".to_string()),
            prompt: "Say hello in one short sentence.".to_string(),
            reasoning_effort: Some(ReasoningEffort::Low),
            max_tokens: 4097,
            provider_headers: BTreeMap::from([
                ("x-safe-header".to_string(), "ok".to_string()),
                (
                    "helicone-auth".to_string(),
                    "Bearer helicone-secret".to_string(),
                ),
            ]),
        }
    }

    fn fixture_artifact(data_frames: &[&str]) -> AnthropicSseCaptureArtifact {
        AnthropicSseCaptureArtifact {
            artifact_version: 1,
            created_at: "2026-06-15T00:00:00Z".to_string(),
            request: SanitizedRequestMetadata {
                provider_format: "anthropic_messages".to_string(),
                model: "MiniMax-M3".to_string(),
                base_url: "https://api.minimax.io/anthropic/v1".to_string(),
                path: "/anthropic/v1/messages".to_string(),
                max_tokens: 4097,
                stream: true,
                thinking_requested: true,
                reasoning_effort: Some(ReasoningEffort::Low),
                thinking_budget_tokens: Some(4096),
                auth: SanitizedAuthMetadata {
                    kind: "bearer".to_string(),
                    redacted: true,
                },
                headers: BTreeMap::from([("authorization".to_string(), REDACTED.to_string())]),
            },
            data_frames: data_frames
                .iter()
                .enumerate()
                .map(|(index, data)| CapturedDataFrame {
                    index,
                    data: (*data).to_string(),
                })
                .collect(),
        }
    }

    fn spawn_sse_server() -> String {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("bind local tcp listener");
        let addr = listener.local_addr().expect("local addr");
        listener.set_nonblocking(true).expect("set nonblocking");

        tokio::runtime::Handle::current().spawn(async move {
            let app = Router::new().route(
                "/anthropic/v1/messages",
                post(move |_req: axum::extract::Request| async move {
                    (
                        axum::http::StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        "data: {\"type\":\"content_block_delta\",\"echo\":\"sk-live-secret\"}\n\n\
                         data: {\"type\":\"message_stop\",\"other\":\"Bearer helicone-secret\"}\n\n\
                         data: [DONE]\n\n",
                    )
                }),
            );

            let listener = tokio::net::TcpListener::from_std(listener).expect("tokio listener");
            axum::serve(listener, app).await.ok();
        });

        format!("http://{}:{}/anthropic/v1", addr.ip(), addr.port())
    }

    #[tokio::test]
    async fn capture_output_shape_redacts_secrets_without_external_network() {
        let artifact = capture_anthropic_sse(test_config(spawn_sse_server()))
            .await
            .expect("capture from local mock server");

        assert_eq!(artifact.artifact_version, 1);
        assert_eq!(artifact.request.model, "MiniMax-M3");
        assert_eq!(artifact.request.path, "/anthropic/v1/messages");
        assert!(artifact.request.stream);
        assert!(artifact.request.thinking_requested);
        assert_eq!(
            artifact.request.reasoning_effort,
            Some(ReasoningEffort::Low)
        );
        assert_eq!(artifact.request.thinking_budget_tokens, Some(4096));
        assert_eq!(artifact.request.headers["authorization"], REDACTED);
        assert_eq!(artifact.request.headers["helicone-auth"], REDACTED);
        assert_eq!(artifact.request.headers["x-safe-header"], "ok");
        assert_eq!(artifact.data_frames.len(), 2);

        let serialized = serde_json::to_string(&artifact).expect("serialize artifact");
        assert!(!serialized.contains("sk-live-secret"));
        assert!(!serialized.contains("helicone-secret"));
        assert!(serialized.contains(REDACTED));
    }

    #[test]
    fn dry_run_surfaces_thinking_metadata_and_no_frames() {
        let artifact = dry_run_anthropic_sse_capture(&test_config(
            "https://api.minimax.io/anthropic/v1".to_string(),
        ))
        .expect("dry run");

        assert!(artifact.data_frames.is_empty());
        assert_eq!(artifact.request.path, "/anthropic/v1/messages");
        assert!(artifact.request.thinking_requested);
        assert_eq!(artifact.request.thinking_budget_tokens, Some(4096));
    }

    #[test]
    fn parse_header_pair_rejects_bad_header_name() {
        assert!(parse_header_pair("x extra: value").is_err());
        assert_eq!(
            parse_header_pair("X-Trace: abc").expect("valid header"),
            ("X-Trace".to_string(), "abc".to_string())
        );
    }

    #[test]
    fn classifies_structured_anthropic_thinking_delta_fixture() {
        let artifact = fixture_artifact(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":12,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Synthetic private reasoning summary."}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Final answer."}}"#,
            r#"{"type":"message_stop"}"#,
        ]);

        assert_eq!(
            classify_anthropic_thinking_stream(&artifact),
            AnthropicThinkingStreamClassification::StructuredThinking
        );
    }

    #[test]
    fn classifies_inline_think_tags_in_text_delta_fixture() {
        let artifact = fixture_artifact(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":12,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"<think>synthetic hidden reasoning</think> Visible answer."}}"#,
            r#"{"type":"message_stop"}"#,
        ]);

        assert_eq!(
            classify_anthropic_thinking_stream(&artifact),
            AnthropicThinkingStreamClassification::InlineThinkTags
        );
    }

    #[test]
    fn classifies_plain_text_fixture_as_no_reasoning_observed() {
        let artifact = fixture_artifact(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":12,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"A short ordinary answer with no reasoning markers."}}"#,
            r#"{"type":"message_stop"}"#,
        ]);

        assert_eq!(
            classify_anthropic_thinking_stream(&artifact),
            AnthropicThinkingStreamClassification::NoReasoningObserved
        );
    }
}
