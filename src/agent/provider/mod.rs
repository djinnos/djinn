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
    /// The real provider base URL (sent as `Helicone-Target-Url` so the proxy
    /// knows where to forward). This lets any provider work without needing
    /// provider-specific gateway paths.
    pub target_url: String,
    /// Auth key sent as `Helicone-Auth: Bearer {key}`.
    pub auth_key: String,
    /// Task ID for tracing (sent as `Helicone-Property-TaskId`).
    pub task_id: Option<String>,
    /// Agent type for tracing (sent as `Helicone-Property-AgentType`).
    pub agent_type: Option<String>,
    /// Session ID for tracing (sent as `Helicone-Session-Id`).
    pub session_id: Option<String>,
}

impl DevProxy {
    /// Inject Helicone auth + metadata headers into a `HeaderMap`.
    pub fn apply_headers(&self, headers: &mut reqwest::header::HeaderMap) {
        use reqwest::header::{HeaderName, HeaderValue};
        if let Ok(val) = HeaderValue::from_str(&format!("Bearer {}", self.auth_key)) {
            headers.insert(HeaderName::from_static("helicone-auth"), val);
        }
        if let Ok(val) = HeaderValue::from_str(&self.target_url) {
            headers.insert(HeaderName::from_static("helicone-target-url"), val);
        }
        if let Some(ref tid) = self.task_id {
            if let Ok(val) = HeaderValue::from_str(tid) {
                headers.insert(HeaderName::from_static("helicone-property-taskid"), val);
            }
        }
        if let Some(ref at) = self.agent_type {
            if let Ok(val) = HeaderValue::from_str(at) {
                headers.insert(HeaderName::from_static("helicone-property-agenttype"), val);
            }
        }
        if let Some(ref sid) = self.session_id {
            if let Ok(val) = HeaderValue::from_str(sid) {
                headers.insert(HeaderName::from_static("helicone-session-id"), val);
            }
        }
    }
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
        FormatFamily::OpenAIResponses => {
            Box::new(format::openai_responses::OpenAIResponsesProvider::new(config))
        }
        FormatFamily::Anthropic => Box::new(format::anthropic::AnthropicProvider::new(config)),
        FormatFamily::Google => Box::new(format::google::GoogleProvider::new(config)),
    }
}
