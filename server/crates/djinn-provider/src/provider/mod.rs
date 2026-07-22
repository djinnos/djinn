pub mod capture;
pub mod client;
pub mod error;
pub mod first_event;
pub mod format;
pub mod telemetry;
pub mod transport;

pub use error::ProviderError;
pub use transport::{
    ExhaustedTransportCategory, ExhaustedTransportDiagnostic, TransportClassificationInput,
    classify_exhausted_transport, oversized_transport_request,
};

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
    /// Unattributed reasoning/thinking token from providers that stream their
    /// chain-of-thought as a bare string without a content-block identity
    /// (e.g. OpenAI Chat `reasoning_content`, OpenAI Responses
    /// `reasoning_summary_text_delta`). This is the load-bearing aggregate for
    /// those formats and must NOT be suppressed by the presence of provider
    /// state.
    Thinking(String),
    /// A thinking **delta** carrying the content-block identity (index) it
    /// belongs to, so consumers can reconcile it against the signed block that
    /// completes at `content_block_stop`.  Anthropic emits these for every
    /// `thinking_delta` SSE frame, keyed by the wire content index.
    ///
    /// Consumers that accumulate thinking for display/telemetry must append the
    /// text once. Persistence paths reconcile these fragments by exact `id`
    /// against [`Self::ThinkingBlockComplete`] and retain only unmatched
    /// residuals.
    ThinkingDelta { id: u64, text: String },
    /// A thinking **block completion** carrying the content-block identity
    /// (index) it belongs to, emitted exactly once when a signed/attributed
    /// thinking block finishes (Anthropic `content_block_stop` on a `thinking`
    /// block). The complete text and optional signature are embedded; consumers
    /// must NOT re-append this text to any string aggregate that already
    /// consumed the matching [`Self::ThinkingDelta`] events.
    ///
    /// Persistence paths retain the block once; canonical assembly reconciles
    /// the `id` against unresolved `ThinkingDelta` fragments and suppresses
    /// only exact matches.
    ThinkingBlockComplete {
        id: u64,
        thinking: String,
        signature: Option<String>,
    },
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

/// Pre-resolved model-dependent metadata for the target of a provider config
/// restamp. Callers build this from catalog / builtin-provider lookups before
/// calling [`restamp_provider_config_for_model`].
#[derive(Clone, Debug)]
pub struct RestampTarget {
    /// Wire model ID to send on requests.
    pub model_id: String,
    /// Wire format family for the target provider/model.
    pub format_family: FormatFamily,
    /// Whether the target model supports reasoning (from catalog metadata).
    pub reasoning: bool,
    /// Context window size in tokens for the target model.
    pub context_window: u32,
    /// Provider-level capabilities for the target provider.
    pub capabilities: ProviderCapabilities,
    /// Tool-schema compatibility quirk for the target provider/model.
    pub tool_schema_compat: Option<ToolSchemaCompat>,
}

/// Stamp a target provider/model onto an existing [`ProviderConfig`] and
/// re-resolve model-dependent defaults.
///
/// This is the central helper for failover / model-restamp paths: instead of
/// mutating only `model_id` and carrying stale defaults from the previous
/// model, this re-resolves every model-dependent field from `target`.
///
/// Model-dependent fields that are **re-resolved** from `target`:
/// - `model_id`
/// - `format_family`
/// - `context_window`
/// - `capabilities` (including `max_tokens_default`)
/// - `reasoning_effort` (via [`default_reasoning_effort_for_model`])
/// - `tool_schema_compat`
///
/// Transport / session fields that are **preserved** from the original config:
/// - `base_url`
/// - `auth`
/// - `telemetry`
/// - `session_affinity_key`
/// - `provider_headers`
pub fn restamp_provider_config_for_model(
    mut config: ProviderConfig,
    target: &RestampTarget,
) -> ProviderConfig {
    config.model_id = target.model_id.clone();
    config.format_family = target.format_family;
    config.context_window = target.context_window;
    config.capabilities = target.capabilities.clone();
    config.reasoning_effort = default_reasoning_effort_for_model(
        target.reasoning,
        target.format_family,
        &target.model_id,
    );
    config.tool_schema_compat = target.tool_schema_compat;
    config
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
    /// Tool-schema compatibility quirk applied to tool definitions before
    /// sending them to this provider.  `None` is the identity state: native
    /// providers (OpenAI, Anthropic, etc.) receive tool schemas verbatim.
    /// `Some(compat)` activates the corresponding schema-projection rules.
    pub tool_schema_compat: Option<ToolSchemaCompat>,
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

/// Tool-schema compatibility quirks that alter JSON Schema projection before
/// sending tool definitions to a provider.
///
/// Each variant selects a set of schema-rewriting rules applied during
/// request building.  `None` on [`ProviderConfig::tool_schema_compat`] is the
/// identity state: native providers (OpenAI, Anthropic, etc.) receive tool
/// schemas verbatim.
///
/// Downstream projection logic consumes these variants to strip unsupported
/// keywords, collapse tuple schemas, coerce enums, etc.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToolSchemaCompat {
    /// Moonshot / Kimi API compatibility.  Strips `$ref` sibling keywords,
    /// collapses `prefixItems` / `tuple` schemas, and removes
    /// `unevaluatedItems`.
    Moonshot,
    /// Google Gemini `generateContent` compatibility.  Applies a keyword
    /// whitelist, coerces `enum` to single-element `type: "string"`,
    /// filters `required`, handles `nullable`, and removes unsupported keys.
    Gemini,
    /// OpenAI-family object-shape enforcement.  Deeply enforces `object`
    /// `properties`, flattens top-level `anyOf`, and strips `null` variants.
    OpenAi,
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
            tool_schema_compat: None,
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

    /// Every native/no-quirk fixture starts with `tool_schema_compat: None`
    /// so identity behavior is preserved until a resolver sets it.
    #[test]
    fn native_configs_default_to_no_tool_schema_compat() {
        for family in [
            FormatFamily::OpenAI,
            FormatFamily::OpenAIResponses,
            FormatFamily::Anthropic,
            FormatFamily::Google,
        ] {
            let cfg = config_for(family);
            assert_eq!(
                cfg.tool_schema_compat, None,
                "{family:?}: native config must default to tool_schema_compat = None"
            );
        }
    }
}

#[cfg(test)]
mod restamp_tests {
    use super::*;
    use std::collections::HashMap;

    /// Build a source config that looks like a non-reasoning OpenAI model
    /// with no max_tokens default — the "before failover" state.
    fn openai_source_config() -> ProviderConfig {
        ProviderConfig {
            base_url: "https://custom-proxy.example.test".to_string(),
            auth: AuthMethod::BearerToken("test-bearer-token".to_string()),
            format_family: FormatFamily::OpenAI,
            model_id: "gpt-4o".to_string(),
            context_window: 128_000,
            telemetry: Some(TelemetryMeta {
                task_id: Some("task-42".to_string()),
                agent_type: Some("worker".to_string()),
                session_id: Some("sess-99".to_string()),
                operation: Some("complete".to_string()),
                user_id: Some("user-7".to_string()),
            }),
            session_affinity_key: Some("affinity-key-77".to_string()),
            provider_headers: {
                let mut h = HashMap::new();
                h.insert("chatgpt-account-id".to_string(), "acct-123".to_string());
                h
            },
            capabilities: ProviderCapabilities {
                streaming: true,
                max_tokens_default: None,
            },
            reasoning_effort: None,
            tool_schema_compat: None,
        }
    }

    /// Target metadata for an Anthropic reasoning-capable model (e.g.
    /// Claude Sonnet 4) — the "failover target" state.
    fn anthropic_reasoning_target() -> RestampTarget {
        RestampTarget {
            model_id: "claude-sonnet-4".to_string(),
            format_family: FormatFamily::Anthropic,
            reasoning: true,
            context_window: 200_000,
            capabilities: ProviderCapabilities {
                streaming: true,
                max_tokens_default: Some(64_000),
            },
            tool_schema_compat: None,
        }
    }

    // ── AC 2: restamp resolves target model_id, reasoning_effort, and
    //          max_tokens_default ──────────────────────────────────────────

    #[test]
    fn restamp_to_anthropic_reasoning_model_resolves_all_model_defaults() {
        let result = restamp_provider_config_for_model(
            openai_source_config(),
            &anthropic_reasoning_target(),
        );

        assert_eq!(
            result.model_id, "claude-sonnet-4",
            "model_id must be stamped to the target"
        );
        assert_eq!(
            result.reasoning_effort,
            Some(ReasoningEffort::Medium),
            "Anthropic-format reasoning-capable model must resolve to Medium"
        );
        assert_eq!(
            result.capabilities.max_tokens_default,
            Some(64_000),
            "max_tokens_default must be re-resolved from the target capabilities"
        );
        assert_eq!(
            result.format_family,
            FormatFamily::Anthropic,
            "format_family must be stamped to the target"
        );
        assert_eq!(
            result.context_window, 200_000,
            "context_window must be stamped to the target"
        );
    }

    #[test]
    fn restamp_to_non_reasoning_model_clears_reasoning_effort() {
        let mut target = anthropic_reasoning_target();
        target.reasoning = false;

        let result = restamp_provider_config_for_model(openai_source_config(), &target);

        assert_eq!(
            result.reasoning_effort, None,
            "non-reasoning target must resolve to None reasoning_effort"
        );
        assert_eq!(result.model_id, "claude-sonnet-4");
    }

    #[test]
    fn restamp_from_reasoning_to_non_reasoning_target_downgrades_effort() {
        // Source starts with an explicit strong tier.
        let mut source = openai_source_config();
        source.reasoning_effort = Some(ReasoningEffort::High);

        // Target is a non-reasoning OpenAI model.
        let target = RestampTarget {
            model_id: "gpt-4.1-mini".to_string(),
            format_family: FormatFamily::OpenAI,
            reasoning: false,
            context_window: 1_000_000,
            capabilities: ProviderCapabilities::default(),
            tool_schema_compat: None,
        };

        let result = restamp_provider_config_for_model(source, &target);

        assert_eq!(
            result.reasoning_effort, None,
            "non-reasoning target must not inherit the source's reasoning_effort"
        );
        assert_eq!(result.model_id, "gpt-4.1-mini");
    }

    // ── AC 3: transport / session fields are preserved ───────────────────

    #[test]
    fn restamp_preserves_auth_base_url_and_session_fields() {
        let result = restamp_provider_config_for_model(
            openai_source_config(),
            &anthropic_reasoning_target(),
        );

        // base_url
        assert_eq!(
            result.base_url, "https://custom-proxy.example.test",
            "base_url must be preserved"
        );

        // auth (AuthMethod does not impl PartialEq — pattern-match)
        match &result.auth {
            AuthMethod::BearerToken(token) => {
                assert_eq!(token, "test-bearer-token", "auth must be preserved");
            }
            _ => panic!("expected BearerToken auth"),
        }

        // session_affinity_key
        assert_eq!(
            result.session_affinity_key,
            Some("affinity-key-77".to_string()),
            "session_affinity_key must be preserved"
        );

        // provider_headers
        let mut expected_headers = HashMap::new();
        expected_headers.insert("chatgpt-account-id".to_string(), "acct-123".to_string());
        assert_eq!(
            result.provider_headers, expected_headers,
            "provider_headers must be preserved"
        );
    }

    #[test]
    fn restamp_preserves_telemetry_metadata() {
        let result = restamp_provider_config_for_model(
            openai_source_config(),
            &anthropic_reasoning_target(),
        );

        let tel = result
            .telemetry
            .as_ref()
            .expect("telemetry must be preserved");
        assert_eq!(tel.task_id.as_deref(), Some("task-42"));
        assert_eq!(tel.agent_type.as_deref(), Some("worker"));
        assert_eq!(tel.session_id.as_deref(), Some("sess-99"));
        assert_eq!(tel.operation.as_deref(), Some("complete"));
        assert_eq!(tel.user_id.as_deref(), Some("user-7"));
    }

    // ── tool_schema_compat re-resolution ─────────────────────────────────

    #[test]
    fn restamp_stamps_target_tool_schema_compat() {
        let mut target = anthropic_reasoning_target();
        target.tool_schema_compat = Some(ToolSchemaCompat::Moonshot);

        let result = restamp_provider_config_for_model(openai_source_config(), &target);

        assert_eq!(
            result.tool_schema_compat,
            Some(ToolSchemaCompat::Moonshot),
            "tool_schema_compat must be stamped from the target"
        );
    }

    #[test]
    fn restamp_from_quirk_to_identity_clears_tool_schema_compat() {
        let mut source = openai_source_config();
        source.tool_schema_compat = Some(ToolSchemaCompat::Moonshot);

        let target = anthropic_reasoning_target(); // tool_schema_compat: None

        let result = restamp_provider_config_for_model(source, &target);

        assert_eq!(
            result.tool_schema_compat, None,
            "restamping to identity target must clear a prior quirk"
        );
    }

    // ── Round-trip / idempotency ─────────────────────────────────────────

    #[test]
    fn restamp_is_idempotent_when_target_matches_current_state() {
        // First restamp: OpenAI source → Anthropic target.
        let first = restamp_provider_config_for_model(
            openai_source_config(),
            &anthropic_reasoning_target(),
        );

        // Second restamp with the same target should produce the same
        // model-dependent fields.
        let second =
            restamp_provider_config_for_model(first.clone(), &anthropic_reasoning_target());

        assert_eq!(second.model_id, first.model_id);
        assert_eq!(second.format_family, first.format_family);
        assert_eq!(second.context_window, first.context_window);
        assert_eq!(second.reasoning_effort, first.reasoning_effort);
        assert_eq!(
            second.capabilities.max_tokens_default,
            first.capabilities.max_tokens_default
        );
        assert_eq!(second.tool_schema_compat, first.tool_schema_compat);
        // Transport fields also survive the double restamp.
        assert_eq!(second.base_url, first.base_url);
        assert_eq!(second.session_affinity_key, first.session_affinity_key);
    }
}
