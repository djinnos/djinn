pub mod client;
pub mod format;
pub mod telemetry;

use std::pin::Pin;

use serde_json::Value;

use crate::message::{ContentBlock, Conversation};

// ─── Token usage ──────────────────────────────────────────────────────────────

/// Token counts extracted from a provider API response.
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub input: u32,
    pub output: u32,
}

// ─── Stream events ────────────────────────────────────────────────────────────

/// Events yielded by the streaming response from an LLM provider.
#[derive(Debug)]
pub enum StreamEvent {
    /// A content delta (text token or complete tool use block).
    Delta(ContentBlock),
    /// Reasoning/thinking token from models that stream their chain-of-thought
    /// (e.g. Kimi K2.5 `reasoning_content`, GLM `reasoning_details`).
    Thinking(String),
    /// Token usage report from the provider.
    Usage(TokenUsage),
    /// End-of-stream sentinel.
    Done,
}

// ─── Provider capabilities ───────────────────────────────────────────────────

/// Provider-level capabilities that affect request building and response parsing.
#[derive(Clone, Debug)]
pub struct ProviderCapabilities {
    /// Whether the provider supports SSE streaming. When `false`, the provider
    /// performs a single POST and parses the complete JSON response.
    pub streaming: bool,
    /// Default max_tokens to send in the request (e.g. Anthropic requires this).
    pub max_tokens_default: Option<u32>,
}

impl Default for ProviderCapabilities {
    fn default() -> Self {
        Self {
            streaming: true,
            max_tokens_default: None,
        }
    }
}

// ─── Provider configuration ───────────────────────────────────────────────────

/// Configuration for a single provider instance.
#[derive(Clone)]
pub struct ProviderConfig {
    /// Base URL for the provider API (e.g. `https://api.openai.com`).
    pub base_url: String,
    /// Authentication method for this provider.
    pub auth: AuthMethod,
    /// Wire format family.
    pub format_family: FormatFamily,
    /// Model ID to request (e.g. `gpt-4o`, `claude-3-5-sonnet-20241022`).
    pub model_id: String,
    /// Context window size in tokens (informational, used for compaction checks).
    pub context_window: u32,
    /// Telemetry metadata for OTel span instrumentation.
    pub telemetry: Option<TelemetryMeta>,
    /// Stable session identifier for provider-specific request affinity/caching.
    pub session_affinity_key: Option<String>,
    /// Extra headers to include on every request (e.g. `chatgpt-account-id` for Codex).
    pub provider_headers: std::collections::HashMap<String, String>,
    /// Provider-level capabilities.
    pub capabilities: ProviderCapabilities,
}

/// Metadata attached to each provider call for OTel tracing.
#[derive(Clone)]
pub struct TelemetryMeta {
    /// Task ID for correlation.
    pub task_id: Option<String>,
    /// Agent type (e.g. "worker", "reviewer").
    pub agent_type: Option<String>,
    /// Session ID for grouping.
    pub session_id: Option<String>,
}

/// Authentication method for provider API requests.
#[derive(Clone)]
pub enum AuthMethod {
    /// Standard `Authorization: Bearer <token>` header.
    BearerToken(String),
    /// Custom header name + key (e.g. Anthropic's `x-api-key`).
    ApiKeyHeader { header: String, key: String },
    /// No authentication (e.g. local models, Google API-key-in-URL).
    NoAuth,
}

/// Wire format family — determines request/response serialization.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FormatFamily {
    /// OpenAI chat completions API (also used by compatible providers).
    OpenAI,
    /// OpenAI Responses API (used by ChatGPT Codex and newer OpenAI endpoints).
    OpenAIResponses,
    /// Anthropic Messages API.
    Anthropic,
    /// Google AI Studio / Vertex AI generateContent API.
    Google,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToolChoice {
    Auto,
    Required,
    None,
}

// ─── Provider trait ───────────────────────────────────────────────────────────

/// Abstraction over a single LLM provider endpoint.
pub trait LlmProvider: Send + Sync {
    /// Human-readable provider name for logging/diagnostics.
    fn name(&self) -> &str;

    /// Start a streaming completion request.
    ///
    /// Returns a future that resolves to a stream of `StreamEvent`s.
    #[allow(clippy::type_complexity)]
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
    >;
}

// ─── Factory ─────────────────────────────────────────────────────────────────

/// Create a concrete provider implementation from the given configuration.
pub fn create_provider(config: ProviderConfig) -> Box<dyn LlmProvider> {
    match config.format_family {
        FormatFamily::OpenAI => Box::new(format::openai::OpenAIProvider::new(config)),
        FormatFamily::OpenAIResponses => Box::new(
            format::openai_responses::OpenAIResponsesProvider::new(config),
        ),
        FormatFamily::Anthropic => Box::new(format::anthropic::AnthropicProvider::new(config)),
        FormatFamily::Google => Box::new(format::google::GoogleProvider::new(config)),
    }
}
