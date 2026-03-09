pub mod client;
pub mod format;

use std::pin::Pin;

use serde_json::Value;

use crate::agent::message::{ContentBlock, Conversation};

// ─── Token usage ──────────────────────────────────────────────────────────────

/// Token counts extracted from a provider API response.
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub input: u32,
    pub output: u32,
}

// ─── Stream events ────────────────────────────────────────────────────────────

/// Events yielded by the streaming response from an LLM provider.
pub enum StreamEvent {
    /// A content delta (text token or complete tool use block).
    Delta(ContentBlock),
    /// Token usage report from the provider.
    Usage(TokenUsage),
    /// End-of-stream sentinel.
    Done,
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
    /// Optional development proxy (Helicone, etc.).
    pub dev_proxy: Option<DevProxy>,
}

/// Development proxy configuration — routes all requests through a proxy URL.
#[derive(Clone)]
pub struct DevProxy {
    /// Proxy base URL (replaces provider base URL).
    pub url: String,
    /// Auth key sent as `Helicone-Auth: Bearer {key}`.
    pub auth_key: String,
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
    /// Anthropic Messages API.
    Anthropic,
    /// Google AI Studio / Vertex AI generateContent API.
    Google,
}

// ─── Provider trait ───────────────────────────────────────────────────────────

/// Abstraction over a single LLM provider endpoint.
pub trait LlmProvider: Send + Sync {
    /// Human-readable provider name for logging/diagnostics.
    fn name(&self) -> &str;

    /// Start a streaming completion request.
    ///
    /// Returns a future that resolves to a stream of `StreamEvent`s.
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
    >;
}

// ─── Factory ─────────────────────────────────────────────────────────────────

/// Create a concrete provider implementation from the given configuration.
pub fn create_provider(config: ProviderConfig) -> Box<dyn LlmProvider> {
    match config.format_family {
        FormatFamily::OpenAI => Box::new(format::openai::OpenAIProvider::new(config)),
        FormatFamily::Anthropic => Box::new(format::anthropic::AnthropicProvider::new(config)),
        FormatFamily::Google => Box::new(format::google::GoogleProvider::new(config)),
    }
}
