pub mod client;
pub mod error;
pub mod format;
pub mod telemetry;

pub use error::ProviderError;

use std::pin::Pin;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::{ContentBlock, Conversation};

// ─── Token usage ──────────────────────────────────────────────────────────────

/// Token counts extracted from a provider API response.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u32,
    pub output: u32,
    /// Input tokens served from the provider's prompt cache (cache hit).
    /// All cache/reasoning fields ride the worker-RPC `LlmResponse` wire path,
    /// so they are `#[serde(default)]` to stay backward-compatible with older
    /// serialized rows that predate cache-token accounting.
    #[serde(default)]
    pub cache_read: u32,
    /// Input tokens written to the provider's prompt cache (cache creation).
    #[serde(default)]
    pub cache_write: u32,
    /// Reasoning/chain-of-thought tokens billed as output (when the provider
    /// reports them separately, e.g. OpenAI `reasoning_tokens`).
    #[serde(default)]
    pub reasoning_output: u32,
    /// Total prompt-context tokens the model actually saw this turn, normalized
    /// across provider formats so consumers (the context gauge, compaction
    /// trigger, UI usage_pct) read one number with consistent semantics:
    ///
    /// - Anthropic format (Anthropic / MiniMax coding plan / GLM): the wire
    ///   `input_tokens` EXCLUDES cached reads/writes, so the true context is
    ///   `input + cache_read + cache_write`. Without this, a cache hit reports
    ///   ~2k while the real context is 100k+ and proactive compaction never
    ///   fires.
    /// - OpenAI format (chat + responses): `prompt_tokens` / `input_tokens`
    ///   already INCLUDE cached tokens, so context is just `input` (adding
    ///   cache_read would double-count).
    /// - Google format: `usageMetadata.promptTokenCount` already includes
    ///   cached content, so context is just `input`.
    ///
    /// `#[serde(default)]` for wire/back-compat: rows serialized before this
    /// field existed deserialize to 0, and [`TokenUsage::context_total`] falls
    /// back to `input + cache_read + cache_write` in that case.
    #[serde(default)]
    pub context_total: u32,
}

impl TokenUsage {
    /// True prompt-context token count for this turn (see [`Self::context_total`]).
    ///
    /// Returns the explicit normalized field when an adapter set it; otherwise
    /// falls back to `input + cache_read + cache_write` for back-compat with
    /// rows serialized before the field existed (Anthropic-format math, the
    /// only case where the legacy `input`-only gauge undercounted).
    pub fn context_total(&self) -> u32 {
        if self.context_total > 0 {
            self.context_total
        } else {
            self.input
                .saturating_add(self.cache_read)
                .saturating_add(self.cache_write)
        }
    }
}

// ─── Stream events ────────────────────────────────────────────────────────────

/// Events yielded by the streaming response from an LLM provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

// ─── Non-streaming LLM response ───────────────────────────────────────────────

/// Terminal aggregate of a single provider call.
///
/// Phase 6a-redux — the wire-shape returned by
/// [`crate::services::SupervisorServices::invoke_llm`]. The host collects a
/// provider's [`StreamEvent`] stream into this shape so the worker (Phase 7)
/// can call into the host's vault-resident provider without itself ever
/// holding the API key.
///
/// `content` is the merged set of assistant content blocks (text + tool_use
/// deltas) accumulated during the stream — i.e. what the consumer would have
/// appended to the conversation had it consumed the stream itself.
/// `thinking` is the model's chain-of-thought, stored separately because
/// providers stream it through `StreamEvent::Thinking` rather than as
/// `ContentBlock::Thinking` deltas. `usage` is the last `StreamEvent::Usage`
/// observed (or `Default::default()` if none was emitted).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmResponse {
    /// Assistant content blocks — text + complete `ToolUse` blocks accumulated
    /// from `StreamEvent::Delta` events.
    pub content: Vec<ContentBlock>,
    /// Chain-of-thought stream concatenated into a single string. Empty when
    /// the model did not emit any thinking events.
    pub thinking: String,
    /// Token usage report; `Default::default()` if the provider did not emit
    /// a `StreamEvent::Usage` frame.
    pub usage: TokenUsage,
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

// ─── Reasoning effort ─────────────────────────────────────────────────────────

/// Normalized reasoning-effort tier, translated per wire format at request time.
///
/// This is the provider-neutral knob; each format builder maps it to its own
/// representation (OpenAI Responses `reasoning.effort`, Anthropic
/// `thinking.budget_tokens`, Google `thinkingConfig.thinkingBudget`).
///
/// IMPORTANT: this field is `Option<ReasoningEffort>` on [`ProviderConfig`] and
/// every existing call site leaves it `None`. When `None`, each format MUST
/// emit byte-identical requests to its pre-B5 behavior (see the per-format
/// `build_request` impls). The tiers below only take effect when a caller
/// explicitly opts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    /// OpenAI Responses `reasoning.effort` token for this tier.
    ///
    /// `Minimal` maps to `"low"` rather than `"minimal"`: newer OpenAI models
    /// (gpt-5.5+) dropped `minimal` from their supported effort set (`none`,
    /// `low`, `medium`, `high`, `xhigh`) and 400 on it, while older gpt-5 models
    /// never had `none`. `low` is the only weak tier every gpt-5.x model
    /// accepts, so collapsing `Minimal`→`low` keeps our cheapest helper calls
    /// (compaction, extraction, chat-title) wire-valid on the whole family. The
    /// Anthropic/Google budget tiers ([`thinking_budget`]) are unaffected.
    pub fn openai_effort(self) -> &'static str {
        match self {
            ReasoningEffort::Minimal | ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
        }
    }

    /// Suggested thinking-token budget for this tier (Anthropic / Google).
    /// The caller is responsible for clamping below the model's output limit.
    pub fn thinking_budget(self) -> u32 {
        match self {
            ReasoningEffort::Minimal => 1024,
            ReasoningEffort::Low => 4096,
            ReasoningEffort::Medium => 12_000,
            ReasoningEffort::High => 24_000,
        }
    }
}

/// Derive the default reasoning-effort tier for a catalog model.
///
/// The policy is intentionally capability-driven: callers pass the model's
/// catalog `reasoning` capability plus the resolved wire [`FormatFamily`], not a
/// provider ID. Formats where [`ProviderConfig::reasoning_effort`] set to `None`
/// suppresses reasoning should opt reasoning-capable models into a shared
/// default tier here. Formats where `None` already has reasoning semantics keep
/// returning `None` so existing request bodies remain unchanged.
pub fn default_reasoning_effort_for_model(
    reasoning: bool,
    format_family: FormatFamily,
    _model_id: &str,
) -> Option<ReasoningEffort> {
    if !reasoning {
        return None;
    }

    match format_family {
        // Anthropic-compatible formats suppress extended thinking when this is
        // `None`, so enable the shared default tier for reasoning-capable
        // catalog models (including Anthropic-format third-party providers).
        FormatFamily::Anthropic => Some(ReasoningEffort::Medium),
        // OpenAI Responses already renders `None` as `reasoning.effort =
        // "medium"` for its own reasoning-capable model families, with
        // model-family gating in the request builder. Preserving `None` keeps
        // Codex/gpt-5.x request bytes unchanged and avoids enabling Responses
        // reasoning for unrelated compatible model IDs solely because catalog
        // metadata says `reasoning = true`.
        FormatFamily::OpenAIResponses => None,
        // OpenAI Chat has no shared reasoning-effort request knob here, and
        // Google remains opt-in until its live defaults are characterized.
        FormatFamily::OpenAI | FormatFamily::Google => None,
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
    /// Normalized reasoning-effort tier. `None` preserves the pre-B5 default
    /// behavior for every format (OpenAI Responses `effort:"medium"`, no
    /// Anthropic `thinking` block, no Google `thinkingConfig`). `Some(tier)`
    /// opts the request into per-format reasoning control.
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Metadata attached to each provider call for OTel tracing.
#[derive(Clone, Default)]
pub struct TelemetryMeta {
    /// Task ID for correlation.
    pub task_id: Option<String>,
    /// Agent type (e.g. "worker", "reviewer").
    pub agent_type: Option<String>,
    /// Session ID for grouping.
    pub session_id: Option<String>,
    /// Operation kind for distinguishing background or system provider calls.
    pub operation: Option<String>,
    /// Attributed user ID for caller-scoped provider calls.
    pub user_id: Option<String>,
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

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
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

    /// Clone of this provider's [`ProviderConfig`], if it has one.
    ///
    /// Real network providers return `Some(self.config.clone())`; test mocks
    /// and other config-less providers leave the default `None`. This is the
    /// hook [`LlmProvider::with_reasoning_effort`] uses to rebuild a variant of
    /// the provider with a different reasoning tier without each call site
    /// needing to know the concrete provider type.
    fn config_snapshot(&self) -> Option<ProviderConfig> {
        None
    }

    /// Return a fresh provider identical to this one but with its
    /// [`ProviderConfig::reasoning_effort`] overridden to `effort`.
    ///
    /// Used by the cheap background call sites (compaction summary, knowledge
    /// extraction, chat-title generation) which hold an already-constructed
    /// provider but want to issue a single request at a weaker reasoning tier
    /// without disturbing the provider the main agent loop streams through.
    ///
    /// The default implementation rebuilds from [`Self::config_snapshot`] with
    /// `reasoning_effort` set. Providers without a config snapshot (test mocks)
    /// fall back to returning an unchanged equivalent via the same snapshot
    /// hook returning `None`, in which case the override is a no-op handled by
    /// the caller. Concrete network providers therefore only need to implement
    /// `config_snapshot`.
    fn with_reasoning_effort(&self, effort: ReasoningEffort) -> Option<Box<dyn LlmProvider>> {
        let mut config = self.config_snapshot()?;
        config.reasoning_effort = Some(effort);
        Some(create_provider(config))
    }
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

#[cfg(test)]
mod reasoning_effort_override_tests {
    use super::*;
    use std::pin::Pin;

    #[test]
    fn default_reasoning_effort_enables_anthropic_reasoning_model() {
        assert_eq!(
            default_reasoning_effort_for_model(true, FormatFamily::Anthropic, "claude-sonnet-4"),
            Some(ReasoningEffort::Medium)
        );
    }

    #[test]
    fn default_reasoning_effort_keeps_non_reasoning_model_disabled() {
        for family in [
            FormatFamily::OpenAI,
            FormatFamily::OpenAIResponses,
            FormatFamily::Anthropic,
            FormatFamily::Google,
        ] {
            assert_eq!(
                default_reasoning_effort_for_model(false, family, "plain-model"),
                None,
                "{family:?} non-reasoning catalog models must not opt into reasoning"
            );
        }
    }

    #[test]
    fn default_reasoning_effort_preserves_openai_responses_none() {
        assert_eq!(
            default_reasoning_effort_for_model(
                true,
                FormatFamily::OpenAIResponses,
                "gpt-5.1-codex"
            ),
            None,
            "OpenAI Responses maps None to its existing medium default in the request builder"
        );
    }

    fn config_for(format_family: FormatFamily) -> ProviderConfig {
        ProviderConfig {
            base_url: "https://example.test".to_string(),
            auth: AuthMethod::NoAuth,
            format_family,
            model_id: "test-model".to_string(),
            context_window: 128_000,
            telemetry: None,
            session_affinity_key: None,
            provider_headers: Default::default(),
            capabilities: ProviderCapabilities::default(),
            // Start from a STRONG tier so a regression that fails to override
            // (or that lowers the wrong field) is visible.
            reasoning_effort: Some(ReasoningEffort::High),
        }
    }

    /// B5a: every concrete network provider must round-trip its config through
    /// `config_snapshot`, and `with_reasoning_effort` must rebuild it with the
    /// requested tier — leaving every other field untouched. Minimal is the
    /// weakest tier the cheap background call sites request.
    #[test]
    fn with_reasoning_effort_minimal_rebuilds_each_format_family() {
        for family in [
            FormatFamily::OpenAI,
            FormatFamily::OpenAIResponses,
            FormatFamily::Anthropic,
            FormatFamily::Google,
        ] {
            let provider = create_provider(config_for(family));

            // The provider exposes its config so the override can rebuild it.
            let snapshot = provider
                .config_snapshot()
                .unwrap_or_else(|| panic!("{family:?} provider must expose config_snapshot"));
            assert_eq!(snapshot.reasoning_effort, Some(ReasoningEffort::High));

            let weak = provider
                .with_reasoning_effort(ReasoningEffort::Minimal)
                .unwrap_or_else(|| {
                    panic!("{family:?} provider must support with_reasoning_effort")
                });
            let weak_snapshot = weak
                .config_snapshot()
                .unwrap_or_else(|| panic!("{family:?} downgraded provider must expose config"));

            // The weakest tier is applied …
            assert_eq!(
                weak_snapshot.reasoning_effort,
                Some(ReasoningEffort::Minimal),
                "{family:?}: cheap call must run at the weakest reasoning tier"
            );
            // … and nothing else changed (model/base_url/window preserved).
            assert_eq!(weak_snapshot.model_id, snapshot.model_id);
            assert_eq!(weak_snapshot.base_url, snapshot.base_url);
            assert_eq!(weak_snapshot.context_window, snapshot.context_window);

            // The override must NOT mutate the original provider in place — the
            // main agent loop keeps streaming through it at its own tier.
            assert_eq!(
                provider.config_snapshot().unwrap().reasoning_effort,
                Some(ReasoningEffort::High),
                "{family:?}: original provider effort must be left unchanged"
            );
        }
    }

    /// Minimal is the weakest available tier (guards against the enum gaining a
    /// weaker variant that the cheap call sites should switch to).
    #[test]
    fn minimal_is_the_weakest_reasoning_tier() {
        assert!(
            ReasoningEffort::Minimal.thinking_budget() <= ReasoningEffort::Low.thinking_budget()
        );
        assert!(
            ReasoningEffort::Low.thinking_budget() <= ReasoningEffort::Medium.thinking_budget()
        );
        assert!(
            ReasoningEffort::Medium.thinking_budget() <= ReasoningEffort::High.thinking_budget()
        );
        // `Minimal` maps to the `"low"` OpenAI wire token (gpt-5.5+ rejects
        // `"minimal"`; `low` is the weakest tier the whole gpt-5.x family
        // accepts). See `openai_effort`.
        assert_eq!(ReasoningEffort::Minimal.openai_effort(), "low");
    }

    /// Config-less providers (test mocks) return `None`, so call sites keep the
    /// original provider rather than panicking.
    #[test]
    fn config_less_provider_returns_none() {
        struct MockProvider;
        impl LlmProvider for MockProvider {
            fn name(&self) -> &str {
                "mock"
            }
            fn stream<'a>(
                &'a self,
                _conversation: &'a Conversation,
                _tools: &'a [Value],
                _tool_choice: Option<ToolChoice>,
            ) -> Pin<
                Box<
                    dyn futures::Future<
                            Output = anyhow::Result<
                                Pin<
                                    Box<
                                        dyn futures::Stream<Item = anyhow::Result<StreamEvent>>
                                            + Send,
                                    >,
                                >,
                            >,
                        > + Send
                        + 'a,
                >,
            > {
                Box::pin(async { unreachable!("mock stream not used in this test") })
            }
        }

        let provider = MockProvider;
        assert!(provider.config_snapshot().is_none());
        assert!(
            provider
                .with_reasoning_effort(ReasoningEffort::Minimal)
                .is_none(),
            "config-less provider yields no override so the caller keeps the original"
        );
    }
}
