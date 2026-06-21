//! Static registry of built-in LLM providers that Djinn knows how to use.
//!
//! Compile-time list of built-in LLM providers. Each entry carries the
//! metadata the catalog and
//! provider-tools layers need: required env vars, OAuth key names, and
//! documentation links.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::provider::{AuthMethod, FormatFamily, ProviderCapabilities};

/// Whether a provider's credential is a personal **subscription** (a consumer
/// coding/token plan or vendor consumer sub) or a fungible **API key**.
///
/// This distinction is load-bearing: subscription credentials are tied to a
/// single person's plan and sharing one across users violates the provider's
/// ToS (and risks an account ban). Subscriptions are therefore enforced to be
/// per-user-only and can never be stored as an org-shared fallback. API keys
/// retain the org-shared capability.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialClass {
    /// A fungible, billable API key (Anthropic, OpenAI API key, Bedrock, …).
    /// May be stored org-shared.
    ApiKey,
    /// A personal coding/token plan or consumer vendor subscription. Must be
    /// stored private to the owning user; never org-shared.
    Subscription,
}

impl CredentialClass {
    /// True for personal-subscription credentials (must be per-user-only).
    pub fn is_subscription(self) -> bool {
        matches!(self, CredentialClass::Subscription)
    }
}

/// How a builtin provider's API-key auth header is shaped.
///
/// Const-friendly descriptor; the concrete [`AuthMethod`] (which carries the
/// runtime key/token) is built from this via [`AuthShape::to_auth_method`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AuthShape {
    /// Standard `Authorization: Bearer <token>` header.
    Bearer,
    /// Custom API-key header with the given name (e.g. Anthropic `x-api-key`).
    Header(&'static str),
}

impl AuthShape {
    /// Build the concrete [`AuthMethod`] for this shape with the given key.
    pub fn to_auth_method(self, api_key: String) -> AuthMethod {
        match self {
            AuthShape::Bearer => AuthMethod::BearerToken(api_key),
            AuthShape::Header(header) => AuthMethod::ApiKeyHeader {
                header: header.to_string(),
                key: api_key,
            },
        }
    }
}

/// How a builtin provider's wire [`FormatFamily`] is resolved.
///
/// Most providers use a fixed family; `openai` is special-cased to switch to
/// the Responses API for models that support it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FormatRule {
    /// Always use this format family, regardless of the model.
    Fixed(FormatFamily),
    /// Use [`FormatFamily::OpenAIResponses`] when the model is a Responses-API
    /// model (per [`is_openai_responses_model`]), otherwise
    /// [`FormatFamily::OpenAI`].
    OpenAiResponsesByModel,
}

impl FormatRule {
    /// Resolve the concrete [`FormatFamily`] for the given model id.
    pub fn resolve(self, model_id: &str) -> FormatFamily {
        match self {
            FormatRule::Fixed(family) => family,
            FormatRule::OpenAiResponsesByModel => {
                if is_openai_responses_model(model_id) {
                    FormatFamily::OpenAIResponses
                } else {
                    FormatFamily::OpenAI
                }
            }
        }
    }
}

/// Metadata for a built-in provider that Djinn can drive natively.
pub struct BuiltinProvider {
    /// Provider slug (e.g. `"anthropic"`, `"chatgpt_codex"`).
    pub id: &'static str,
    /// Human-readable name for UI display.
    pub display_name: &'static str,
    /// Environment variables required for API-key auth (may be empty for
    /// OAuth-only providers).
    pub required_env_vars: &'static [&'static str],
    /// Config key names whose presence indicates an active OAuth token.
    /// Empty for providers that don't support OAuth.
    pub oauth_keys: &'static [&'static str],
    /// Documentation link shown in the catalog.
    pub docs_url: &'static str,
    /// If set, this provider's OAuth capabilities are folded into the
    /// given parent in the catalog response and this entry is hidden.
    /// Internally the provider still exists for dispatch/model sourcing.
    pub merge_into: Option<&'static str>,
    /// If true, this provider is used for authentication only (not model
    /// dispatch) and should be excluded from the model provider catalog.
    pub auth_only: bool,
    /// Wire-format-family resolution rule for this provider.
    pub format_rule: FormatRule,
    /// API-key auth header shape for this provider.
    pub auth_shape: AuthShape,
    /// Whether the provider supports SSE streaming.
    pub streaming: bool,
    /// Default `max_tokens` to send in requests (Anthropic requires one).
    pub max_tokens_default: Option<u32>,
    /// Whether this provider's credential is a personal subscription or a
    /// fungible API key. Set explicitly per entry — drives per-user-only
    /// enforcement in the credential-set path.
    pub credential_class: CredentialClass,
}

impl BuiltinProvider {
    /// Resolve this provider's wire [`FormatFamily`] for the given model id.
    pub fn format_family(&self, model_id: &str) -> FormatFamily {
        self.format_rule.resolve(model_id)
    }

    /// Build this provider's [`AuthMethod`] from the given API key.
    pub fn auth_method(&self, api_key: String) -> AuthMethod {
        self.auth_shape.to_auth_method(api_key)
    }

    /// Whether this provider's credential is a personal subscription.
    pub fn is_subscription(&self) -> bool {
        self.credential_class.is_subscription()
    }

    /// Build this provider's [`ProviderCapabilities`].
    pub fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: self.streaming,
            max_tokens_default: self.max_tokens_default,
        }
    }
}

/// Returns true for OpenAI models that support the Responses API.
pub fn is_openai_responses_model(model_id: &str) -> bool {
    let lower = model_id.to_lowercase();
    lower.starts_with("gpt-5")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
}

/// Default wire-format-family rule for unknown / custom providers.
///
/// Reproduces the historical `_ =>` arm in `completion::provider_format_family`
/// (`FormatFamily::OpenAI`).
pub const DEFAULT_FORMAT_RULE: FormatRule = FormatRule::Fixed(FormatFamily::OpenAI);

/// Default API-key auth shape for unknown / custom providers.
///
/// Reproduces the historical `_ =>` arm in `completion::provider_auth`
/// (`AuthMethod::BearerToken`).
pub const DEFAULT_AUTH_SHAPE: AuthShape = AuthShape::Bearer;

/// Default provider capabilities for unknown / custom providers.
///
/// Reproduces the historical `_ =>` arm in `completion::provider_capabilities`
/// (`ProviderCapabilities::default()`): streaming on, no default max_tokens.
pub fn default_capabilities() -> ProviderCapabilities {
    ProviderCapabilities::default()
}

/// All providers Djinn can use out of the box.
///
/// Providers already present in models.dev (e.g. `anthropic`, `openai`) are
/// listed here so their IDs pass the `is_provider_usable()` gate.  Providers
/// **not** in models.dev (e.g. `chatgpt_codex`) get synthetic catalog entries
/// via [`super::catalog::CatalogService::inject_builtin_providers`].
pub static BUILTIN_PROVIDERS: &[BuiltinProvider] = &[
    BuiltinProvider {
        id: "anthropic",
        display_name: "Anthropic",
        required_env_vars: &["ANTHROPIC_API_KEY"],
        oauth_keys: &[],
        docs_url: "https://docs.anthropic.com",
        merge_into: None,
        auth_only: false,
        format_rule: FormatRule::Fixed(FormatFamily::Anthropic),
        auth_shape: AuthShape::Header("x-api-key"),
        streaming: true,
        max_tokens_default: Some(64_000),
        credential_class: CredentialClass::ApiKey,
    },
    BuiltinProvider {
        id: "openai",
        display_name: "OpenAI",
        required_env_vars: &["OPENAI_API_KEY"],
        oauth_keys: &[],
        docs_url: "https://platform.openai.com/docs",
        merge_into: None,
        auth_only: false,
        format_rule: FormatRule::OpenAiResponsesByModel,
        auth_shape: AuthShape::Bearer,
        streaming: true,
        max_tokens_default: None,
        // The bare OpenAI API key is a fungible billable key. The personal
        // ChatGPT/Codex OAuth path is the separate `chatgpt_codex` entry below
        // (a subscription), which merges into `openai` in the catalog.
        credential_class: CredentialClass::ApiKey,
    },
    BuiltinProvider {
        id: "google",
        display_name: "Google AI",
        required_env_vars: &["GOOGLE_API_KEY"],
        oauth_keys: &[],
        docs_url: "https://ai.google.dev/docs",
        merge_into: None,
        auth_only: false,
        format_rule: FormatRule::Fixed(FormatFamily::Google),
        auth_shape: AuthShape::Header("x-goog-api-key"),
        streaming: true,
        max_tokens_default: None,
        credential_class: CredentialClass::ApiKey,
    },
    // OpenAI-compatible. Already present in models.dev (`fireworks-ai`) with the
    // right `api` base_url + model list; listed here only so the env-var
    // bootstrap upserts FIREWORKS_API_KEY into the vault. `id` must match the
    // catalog provider id so dispatch-time credential lookup resolves.
    BuiltinProvider {
        id: "fireworks-ai",
        display_name: "Fireworks AI",
        required_env_vars: &["FIREWORKS_API_KEY"],
        oauth_keys: &[],
        docs_url: "https://fireworks.ai/docs/",
        merge_into: None,
        auth_only: false,
        format_rule: FormatRule::Fixed(FormatFamily::OpenAI),
        auth_shape: AuthShape::Bearer,
        streaming: true,
        max_tokens_default: None,
        credential_class: CredentialClass::ApiKey,
    },
    // Anthropic-compatible. Already present in models.dev (`minimax-coding-plan`)
    // with the right `api` base_url (https://api.minimax.io/anthropic/v1) + model
    // list (MiniMax-M3, …); listed here only so the env-var bootstrap upserts
    // MINIMAX_API_KEY into the vault. The coding-plan subscription key only works
    // on this endpoint and authenticates via Authorization: Bearer.
    BuiltinProvider {
        id: "minimax-coding-plan",
        display_name: "MiniMax Coding Plan",
        required_env_vars: &["MINIMAX_API_KEY"],
        oauth_keys: &[],
        docs_url: "https://platform.minimax.io/docs",
        merge_into: None,
        auth_only: false,
        format_rule: FormatRule::Fixed(FormatFamily::Anthropic),
        auth_shape: AuthShape::Bearer,
        streaming: true,
        max_tokens_default: Some(64_000),
        credential_class: CredentialClass::Subscription,
    },
    // OpenAI-compatible. Models.dev-native (`xiaomi-token-plan-sgp`, "Xiaomi MiMo
    // Token Plan (SGP)") with api `https://token-plan-sgp.xiaomimimo.com/v1` (npm
    // @ai-sdk/openai-compatible) + the MiMo model list (mimo-v2.5-pro, …); models
    // come from the live catalog refresh, so none are hand-added to snapshot.json.
    // Listed here so the env-var bootstrap upserts XIAOMI_API_KEY into the vault.
    // The Token Plan key authenticates via `Authorization: Bearer <key>`. CN/AMS
    // region variants `xiaomi-token-plan-cn`/`xiaomi-token-plan-ams` also exist in
    // models.dev.
    BuiltinProvider {
        id: "xiaomi-token-plan-sgp",
        display_name: "Xiaomi MiMo Token Plan (SGP)",
        required_env_vars: &["XIAOMI_API_KEY"],
        oauth_keys: &[],
        docs_url: "https://platform.xiaomimimo.com",
        merge_into: None,
        auth_only: false,
        format_rule: FormatRule::Fixed(FormatFamily::OpenAI),
        auth_shape: AuthShape::Bearer,
        streaming: true,
        max_tokens_default: None,
        credential_class: CredentialClass::Subscription,
    },
    // Anthropic-compatible. Models.dev-native (`kimi-for-coding`, "Kimi for
    // Coding") with api `https://api.kimi.com/coding/v1` (npm @ai-sdk/anthropic) +
    // the K2.7 Code model list (k2p7, …); models come from the live catalog
    // refresh, so none are hand-added to snapshot.json. Listed here so the env-var
    // bootstrap upserts KIMI_API_KEY into the vault. The subscription key
    // authenticates via `Authorization: Bearer <key>` (NOT the Anthropic-native
    // `x-api-key` path), and the endpoint is the Kimi Code coding endpoint, NOT
    // the api.moonshot.ai pay-as-you-go API.
    BuiltinProvider {
        id: "kimi-for-coding",
        display_name: "Kimi for Coding",
        required_env_vars: &["KIMI_API_KEY"],
        oauth_keys: &[],
        docs_url: "https://www.kimi.com/code/docs",
        merge_into: None,
        auth_only: false,
        format_rule: FormatRule::Fixed(FormatFamily::Anthropic),
        auth_shape: AuthShape::Bearer,
        streaming: true,
        max_tokens_default: Some(64_000),
        credential_class: CredentialClass::Subscription,
    },
    // OpenAI-compatible. Present in models.dev (`opencode`, "OpenCode Zen") with
    // base_url https://opencode.ai/zen/v1 + model list (including the rotating
    // `*-free` models); listed here so the env-var bootstrap upserts
    // OPENCODE_API_KEY into the vault. Zen requires opencode-cli identity headers
    // to escape the anonymous rate-limit tier — injected per-request in
    // `provider_resolution::provider_headers_for`.
    BuiltinProvider {
        id: "opencode",
        display_name: "OpenCode Zen",
        required_env_vars: &["OPENCODE_API_KEY"],
        oauth_keys: &[],
        docs_url: "https://opencode.ai/docs/zen/",
        merge_into: None,
        auth_only: false,
        format_rule: FormatRule::Fixed(FormatFamily::OpenAI),
        auth_shape: AuthShape::Bearer,
        streaming: true,
        max_tokens_default: None,
        credential_class: CredentialClass::Subscription,
    },
    // OpenAI-compatible. Present in models.dev (`zai-coding-plan`, "Z.AI Coding
    // Plan") with base_url https://api.z.ai/api/coding/paas/v4 + GLM model list;
    // listed here so the env-var bootstrap upserts ZHIPU_API_KEY into the vault.
    // The coding-plan subscription key authenticates via Authorization: Bearer.
    BuiltinProvider {
        id: "zai-coding-plan",
        display_name: "Z.AI Coding Plan",
        required_env_vars: &["ZHIPU_API_KEY"],
        oauth_keys: &[],
        docs_url: "https://docs.z.ai/devpack/overview",
        merge_into: None,
        auth_only: false,
        format_rule: FormatRule::Fixed(FormatFamily::OpenAI),
        auth_shape: AuthShape::Bearer,
        streaming: true,
        max_tokens_default: None,
        credential_class: CredentialClass::Subscription,
    },
    // OAuth-only provider whose capabilities are folded into "openai" in the
    // catalog.  Internally still a distinct provider for dispatch & models.
    BuiltinProvider {
        id: "chatgpt_codex",
        display_name: "ChatGPT Codex",
        required_env_vars: &[],
        oauth_keys: &["CHATGPT_CODEX_TOKEN"],
        docs_url: "https://platform.openai.com/docs",
        merge_into: Some("openai"),
        auth_only: false,
        format_rule: FormatRule::Fixed(FormatFamily::OpenAIResponses),
        auth_shape: AuthShape::Bearer,
        streaming: true,
        max_tokens_default: None,
        // Personal ChatGPT/Codex OAuth subscription — never org-shareable.
        credential_class: CredentialClass::Subscription,
    },
    BuiltinProvider {
        id: "githubcopilot",
        display_name: "GitHub Copilot",
        required_env_vars: &[],
        oauth_keys: &["GITHUB_COPILOT_TOKEN"],
        docs_url: "https://docs.github.com/en/copilot",
        merge_into: None,
        auth_only: false,
        format_rule: FormatRule::Fixed(FormatFamily::OpenAI),
        auth_shape: AuthShape::Bearer,
        streaming: true,
        max_tokens_default: None,
        // Personal GitHub Copilot OAuth subscription — never org-shareable.
        credential_class: CredentialClass::Subscription,
    },
    BuiltinProvider {
        id: "github_app",
        display_name: "GitHub App",
        required_env_vars: &[],
        oauth_keys: &["GITHUB_APP_TOKEN"],
        docs_url: "https://docs.github.com/en/apps",
        merge_into: None,
        auth_only: true,
        format_rule: FormatRule::Fixed(FormatFamily::OpenAI),
        auth_shape: AuthShape::Bearer,
        streaming: true,
        max_tokens_default: None,
        // GitHub App is auth-only infra (not a personal model subscription).
        credential_class: CredentialClass::ApiKey,
    },
    BuiltinProvider {
        id: "gcp_vertex_ai",
        display_name: "Google Cloud Vertex AI",
        required_env_vars: &["GCP_VERTEX_PROJECT_ID"],
        oauth_keys: &[],
        docs_url: "https://cloud.google.com/vertex-ai/docs",
        merge_into: None,
        auth_only: false,
        format_rule: FormatRule::Fixed(FormatFamily::OpenAI),
        auth_shape: AuthShape::Bearer,
        streaming: true,
        max_tokens_default: None,
        credential_class: CredentialClass::ApiKey,
    },
    BuiltinProvider {
        id: "aws_bedrock",
        display_name: "AWS Bedrock",
        required_env_vars: &["AWS_ACCESS_KEY_ID"],
        oauth_keys: &[],
        docs_url: "https://docs.aws.amazon.com/bedrock/",
        merge_into: None,
        auth_only: false,
        format_rule: FormatRule::Fixed(FormatFamily::OpenAI),
        auth_shape: AuthShape::Bearer,
        streaming: true,
        max_tokens_default: None,
        credential_class: CredentialClass::ApiKey,
    },
    BuiltinProvider {
        id: "azure_openai",
        display_name: "Azure OpenAI",
        required_env_vars: &["AZURE_OPENAI_API_KEY"],
        oauth_keys: &[],
        docs_url: "https://learn.microsoft.com/en-us/azure/ai-services/openai/",
        merge_into: None,
        auth_only: false,
        format_rule: FormatRule::Fixed(FormatFamily::OpenAI),
        auth_shape: AuthShape::Bearer,
        streaming: true,
        max_tokens_default: None,
        credential_class: CredentialClass::ApiKey,
    },
];

/// Set of all built-in provider IDs (for fast lookup).
pub fn builtin_provider_ids() -> HashSet<String> {
    BUILTIN_PROVIDERS.iter().map(|p| p.id.to_string()).collect()
}

/// Find a builtin provider by ID, with canonical-alias fallback.
pub fn find_builtin_provider(provider_id: &str) -> Option<&'static BuiltinProvider> {
    // Exact match first.
    if let Some(p) = BUILTIN_PROVIDERS.iter().find(|p| p.id == provider_id) {
        return Some(p);
    }
    // Canonical alias (strip separators, lowercase).
    let canonical = canonical_id(provider_id);
    BUILTIN_PROVIDERS
        .iter()
        .find(|p| canonical_id(p.id) == canonical)
}

/// Resolve a provider ID to its canonical builtin name, if it matches one.
pub fn resolve_builtin_name(provider_id: &str) -> Option<&'static str> {
    find_builtin_provider(provider_id).map(|p| p.id)
}

/// OAuth key names for a provider (empty if not OAuth-capable).
pub fn oauth_keys_for_provider(provider_id: &str) -> Vec<String> {
    find_builtin_provider(provider_id)
        .map(|p| p.oauth_keys.iter().map(|k| k.to_string()).collect())
        .unwrap_or_default()
}

/// All OAuth keys for a provider, including keys inherited from merged children.
/// E.g. `"openai"` gets its own (empty) plus `chatgpt_codex`'s `CHATGPT_CODEX_TOKEN`.
pub fn all_oauth_keys_for_provider(provider_id: &str) -> Vec<String> {
    let mut keys = oauth_keys_for_provider(provider_id);
    keys.extend(merged_oauth_keys_for(provider_id));
    keys
}

/// Check whether a provider is a built-in (not custom/user-added).
pub fn is_builtin_provider(provider_id: &str) -> bool {
    find_builtin_provider(provider_id).is_some()
}

/// Check whether a provider is auth-only (used for authentication, not model dispatch).
/// Auth-only providers should be excluded from model provider catalogs.
pub fn is_auth_only_provider(provider_id: &str) -> bool {
    find_builtin_provider(provider_id).is_some_and(|p| p.auth_only)
}

/// Classify a provider id as a personal [`CredentialClass::Subscription`] or a
/// fungible [`CredentialClass::ApiKey`].
///
/// Resolution order:
/// 1. If the id maps to a built-in provider, use its explicit
///    [`BuiltinProvider::credential_class`] (the authoritative source — we set
///    it per-entry rather than guess).
/// 2. Otherwise (models.dev-native providers Djinn doesn't hand-register, e.g.
///    the regional `xiaomi-token-plan-*`, `alibaba-*-plan`, `tencent-*-plan`
///    variants), fall back to an id-pattern rule so personal coding/token
///    plans surfaced live from the catalog are still treated as subscriptions.
///
/// Anything not matched is an API key — the safe default for the long tail of
/// generic OpenAI-compatible providers.
pub fn classify_provider(provider_id: &str) -> CredentialClass {
    if let Some(p) = find_builtin_provider(provider_id) {
        return p.credential_class;
    }
    if is_subscription_id(provider_id) {
        CredentialClass::Subscription
    } else {
        CredentialClass::ApiKey
    }
}

/// True when `provider_id` is a personal subscription. Convenience wrapper over
/// [`classify_provider`].
pub fn is_subscription_provider(provider_id: &str) -> bool {
    classify_provider(provider_id).is_subscription()
}

/// Resolve the **governable subscription identity** a `(provider_id, model_id)`
/// reference belongs to, if any.
///
/// This bridges the one case where a subscription's models are surfaced under a
/// *different* provider namespace than the subscription's own id: the ChatGPT/
/// Codex OAuth subscription (`chatgpt_codex`) merges into `openai`, so its models
/// are namespaced `openai/...-codex` everywhere a member sees them. A plain
/// `openai` BYO **API key** is NOT a subscription and must stay ungoverned, so we
/// can't just treat all of `openai` as the codex sub — we key off the `codex`
/// model marker (the same marker [`MODEL_SOURCE_MAP`] uses to source the codex
/// model list from openai).
///
/// Returns (lowercased, matching the stored blocklist form):
/// - `Some("chatgpt_codex")` for an `openai` model whose id marks it a Codex
///   model (so org policy can block the Codex sub without touching plain openai).
/// - `Some(provider_id)` when the provider itself is a subscription.
/// - `None` for non-subscription providers (plain API keys), which are never
///   governed by the org allow/block list.
pub fn governable_subscription_for_model(provider_id: &str, model_id: &str) -> Option<String> {
    // Codex models surface under the openai namespace; recognise them by the
    // `codex` marker and attribute them to the chatgpt_codex subscription.
    if canonical_id(provider_id) == "openai" && model_id.to_ascii_lowercase().contains("codex") {
        return Some("chatgpt_codex".to_string());
    }
    if is_subscription_provider(provider_id) {
        return Some(provider_id.to_ascii_lowercase());
    }
    None
}

/// Id-pattern rule for personal coding/token plans and consumer vendor subs
/// that are **not** hand-registered as built-ins (models.dev-native only).
///
/// This is the fallback arm of [`classify_provider`]; hand-registered builtins
/// carry an explicit class and never reach here.
fn is_subscription_id(provider_id: &str) -> bool {
    let id = provider_id.to_ascii_lowercase();

    // Coding/token-plan suffixes used across vendors' consumer plans.
    if id.contains("-coding-plan") || id.contains("-token-plan") || id.contains("-for-coding") {
        return true;
    }

    // Known consumer-subscription vendor prefixes / exact ids. Regional and
    // plan-tier variants (e.g. `xiaomi-token-plan-cn`, `zhipuai-coding-plan`)
    // are covered either here or by the suffix rule above.
    const SUBSCRIPTION_PREFIXES: &[&str] = &[
        "xiaomi",
        "moonshotai",
        "alibaba",
        "tencent",
        "stepfun",
        "kuae-cloud",
        "umans-ai",
    ];
    if SUBSCRIPTION_PREFIXES
        .iter()
        .any(|prefix| id.starts_with(prefix))
    {
        return true;
    }

    const SUBSCRIPTION_IDS: &[&str] = &[
        "zai",
        "zhipuai",
        "kimi-for-coding",
        "minimax-coding-plan",
        "opencode",
        "opencode-go",
    ];
    SUBSCRIPTION_IDS.iter().any(|exact| id == *exact)
}

/// Data-residency jurisdiction of a subscription provider's hosting.
///
/// models.dev has **no** region field, so this classification is djinn-owned:
/// it is derived from the provider id/name where the catalog encodes a region
/// (`-cn`, `-ams` = EU, `-sgp` = Singapore, "(China)") and hardcoded to `Cn`
/// for Chinese vendors regardless of which endpoint a particular plan hits.
/// It exists to frame the org AI-policy allow/block surface — notably the
/// "block all China-hosted subscriptions" action — by data residency.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Jurisdiction {
    /// United States hosted.
    Us,
    /// European Union hosted (e.g. Amsterdam `-ams`).
    Eu,
    /// Mainland-China hosted, or a Chinese vendor regardless of endpoint.
    Cn,
    /// Anywhere else / unknown (e.g. Singapore `-sgp`, generic gateways).
    Other,
}

impl Jurisdiction {
    /// True for mainland-China residency.
    pub fn is_china(self) -> bool {
        matches!(self, Jurisdiction::Cn)
    }
}

/// Chinese vendor id prefixes/exact ids that are classified `Cn` regardless of
/// which regional endpoint a particular plan authenticates against. The vendor
/// owns the account/data even when a model is mirrored to a non-CN region, so
/// data-residency policy treats the whole vendor as China-hosted.
const CHINA_VENDOR_MARKERS: &[&str] = &[
    "zai",
    "zhipuai",
    "zhipu",
    "minimax",
    "kimi",
    "moonshotai",
    "moonshot",
    "xiaomi",
    "alibaba",
    "qwen",
    "tencent",
    "hunyuan",
    "stepfun",
    "iflowcn",
    "siliconflow",
    "modelscope",
    "qiniu",
    "qihang",
    "bailing",
    "deepseek",
];

/// Data-residency [`Jurisdiction`] for a (subscription) provider id.
///
/// djinn-owned classification (models.dev carries no region field). Resolution
/// order, most specific first:
/// 1. Explicit Chinese vendor markers ⇒ [`Jurisdiction::Cn`] (overrides any
///    regional-endpoint suffix — the vendor still owns the data).
/// 2. Regional id suffixes the catalog encodes: `-cn`/`(china)` ⇒ `Cn`,
///    `-ams` ⇒ `Eu`, `-sgp` ⇒ `Other` (Singapore).
/// 3. Otherwise [`Jurisdiction::Us`] for US-headquartered vendors, falling
///    through to [`Jurisdiction::Other`] for the unknown long tail.
///
/// This is only meaningful for subscription providers; org policy applies it to
/// the subscription set. The classification is intentionally coarse and safe:
/// uncertain providers land in `Other`, never silently in `Cn`.
pub fn provider_jurisdiction(provider_id: &str) -> Jurisdiction {
    let id = provider_id.to_ascii_lowercase();

    // 1. Chinese vendors — vendor identity wins over endpoint region.
    if CHINA_VENDOR_MARKERS
        .iter()
        .any(|m| id.starts_with(m) || id.contains(&format!("-{m}")) || id.contains(m))
    {
        return Jurisdiction::Cn;
    }

    // 2. Regional endpoint suffixes the catalog already encodes.
    if id.ends_with("-cn") || id.contains("china") {
        return Jurisdiction::Cn;
    }
    if id.ends_with("-ams") || id.contains("-ams-") {
        return Jurisdiction::Eu;
    }
    if id.ends_with("-sgp") || id.contains("-sgp-") {
        // Singapore — not EU, not CN.
        return Jurisdiction::Other;
    }

    // 3. Known US-headquartered subscription vendors.
    const US_VENDORS: &[&str] = &[
        "openai",
        "chatgpt",
        "anthropic",
        "claude",
        "github",
        "copilot",
    ];
    if US_VENDORS
        .iter()
        .any(|v| id.starts_with(v) || id.contains(v))
    {
        return Jurisdiction::Us;
    }

    Jurisdiction::Other
}

// ── Curated flagship / "recommended" model map ───────────────────────────────
//
// For each SUPPORTED provider, the latest state-of-the-art flagship model id(s).
// This is the curated subset the UI marks "recommended" so members aren't shown
// every historical variant (no GPT-5.2/5.3/5.4 when 5.5 exists, no o1/o3
// clutter). A provider absent from this map has NO recommended model — the UI
// falls back to showing all of its models.
//
// ──────────────────────────────────────────────────────────────────────────────
// MAINTENANCE: when a provider ships a new flagship, update the one slice below
// (it is a single-line edit per provider). Model ids are the **bare** ids as
// they appear in the provider's own catalog list (NOT `provider/` prefixed) —
// matching is done on (provider_id, bare model id). For merged children whose
// models are re-namespaced to a parent (e.g. `chatgpt_codex` → `openai`), list
// the ids under the CHILD's own id; the lookup also resolves the parent-
// namespaced form so the recommendation survives re-tagging.
// ──────────────────────────────────────────────────────────────────────────────
//
// (provider_id, &[flagship bare model ids])
const RECOMMENDED_MODELS: &[(&str, &[&str])] = &[
    // Single-vendor subs / first-party APIs → the one current flagship (+ its
    // codex/coding variant where the vendor ships a distinct one).
    //
    // OpenAI (BYO API key): current GPT-5.5 family flagship + the latest Codex.
    ("openai", &["gpt-5.5", "gpt-5.3-codex"]),
    // ChatGPT/Codex subscription: the same Codex flagship, under the child id.
    ("chatgpt_codex", &["gpt-5.3-codex"]),
    // Anthropic: current Claude Opus flagship.
    ("anthropic", &["claude-opus-4-8"]),
    // MiniMax coding plan: current MiniMax flagship.
    ("minimax-coding-plan", &["MiniMax-M3"]),
    // Kimi for Coding: current Kimi K2.7 Code flagship.
    ("kimi-for-coding", &["k2p7"]),
    // Z.AI / Zhipu coding plan: current GLM flagship.
    ("zai-coding-plan", &["glm-5.2"]),
    // Xiaomi MiMo token plan (SGP region variant we hand-register): MiMo flagship.
    ("xiaomi-token-plan-sgp", &["mimo-v2.5-pro"]),
    // Multi-model gateways expose many third-party models → a SMALL curated set
    // of the current top models, not a single flagship.
    //
    // OpenCode Zen: top Anthropic/OpenAI/Google flagships it proxies.
    (
        "opencode",
        &["claude-opus-4-8", "gpt-5.5", "gemini-3.1-pro"],
    ),
    // Fireworks AI: top open-weight flagships it serves (bare ids are slash
    // paths in the live catalog).
    (
        "fireworks-ai",
        &[
            "accounts/fireworks/models/glm-5p2",
            "accounts/fireworks/models/minimax-m3",
            "accounts/fireworks/models/kimi-k2p7-code",
        ],
    ),
];

/// Curated flagship / "recommended" model ids for `provider_id` — the latest
/// state-of-the-art model(s) the UI surfaces as recommended. Empty slice when
/// the provider has no curated recommendation (UI shows all its models).
///
/// See [`RECOMMENDED_MODELS`] for the (1-line-per-provider) curated map.
pub fn recommended_model_ids(provider_id: &str) -> &'static [&'static str] {
    RECOMMENDED_MODELS
        .iter()
        .find(|(id, _)| *id == provider_id)
        .map(|(_, ids)| *ids)
        .unwrap_or(&[])
}

/// Whether `(provider_id, model_id)` is a curated flagship the UI marks
/// "recommended". Accepts either a bare model id or a `provider/...`-qualified
/// id, and tolerates merged-child re-namespacing: a model re-tagged to a parent
/// provider (e.g. a `chatgpt_codex` model surfaced under `openai`) still matches
/// the recommendation registered under the parent. Matching is case-sensitive
/// on the model id (catalog ids are canonical).
pub fn is_recommended_model(provider_id: &str, model_id: &str) -> bool {
    // Strip a leading `provider_id/` qualifier if present, keeping the rest of
    // the (possibly multi-segment) model id intact.
    let bare = model_id
        .strip_prefix(&format!("{provider_id}/"))
        .unwrap_or(model_id);
    recommended_model_ids(provider_id)
        .iter()
        .any(|id| *id == bare || *id == model_id)
}

/// Strip non-alphanumeric chars and lowercase for fuzzy ID matching.
fn canonical_id(id: &str) -> String {
    id.chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

/// IDs of providers that should be hidden from the catalog (merged into a parent).
pub fn merged_provider_ids() -> HashSet<String> {
    BUILTIN_PROVIDERS
        .iter()
        .filter_map(|p| p.merge_into.map(|_| p.id.to_string()))
        .collect()
}

/// Collect OAuth keys from child providers that merge into `parent_id`.
pub fn merged_oauth_keys_for(parent_id: &str) -> Vec<String> {
    BUILTIN_PROVIDERS
        .iter()
        .filter(|p| p.merge_into == Some(parent_id))
        .flat_map(|p| p.oauth_keys.iter().map(|k| k.to_string()))
        .collect()
}

/// Resolve a catalog-facing provider ID to the internal provider that handles
/// its OAuth flow.  E.g. `"openai"` → `"chatgpt_codex"` (because codex is
/// merged into openai).
pub fn resolve_oauth_provider(provider_id: &str) -> Option<&'static str> {
    BUILTIN_PROVIDERS
        .iter()
        .find(|p| p.merge_into == Some(provider_id) && !p.oauth_keys.is_empty())
        .map(|p| p.id)
}

/// Check whether any of the given OAuth keys have a stored token.
///
/// Checks the vault credential key names for the well-known OAuth credential
/// DB keys.  This is the crate-internal version; the server layer adds a
/// filesystem-cache fallback for backward compatibility.
pub fn is_oauth_key_present(oauth_keys: &[String], credential_key_names: &HashSet<String>) -> bool {
    const CODEX_OAUTH_DB_KEY: &str = "__OAUTH_CHATGPT_CODEX";
    const COPILOT_OAUTH_DB_KEY: &str = "__OAUTH_GITHUB_COPILOT";

    oauth_keys.iter().any(|key| match key.as_str() {
        "CHATGPT_CODEX_TOKEN" => credential_key_names.contains(CODEX_OAUTH_DB_KEY),
        "GITHUB_COPILOT_TOKEN" => credential_key_names.contains(COPILOT_OAUTH_DB_KEY),
        // `GITHUB_APP_TOKEN` was the key name for the retired device-code
        // OAuth App flow. The GitHub App flow is now driven by installations,
        // not stored tokens, so this key is intentionally not recognised.
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_subscriptions_classified_explicitly() {
        for id in [
            "minimax-coding-plan",
            "xiaomi-token-plan-sgp",
            "kimi-for-coding",
            "opencode",
            "zai-coding-plan",
            "chatgpt_codex",
            "githubcopilot",
        ] {
            assert!(
                is_subscription_provider(id),
                "{id} should classify as a subscription"
            );
        }
    }

    #[test]
    fn builtin_api_keys_classified_explicitly() {
        for id in [
            "anthropic",
            "openai",
            "google",
            "fireworks-ai",
            "aws_bedrock",
            "azure_openai",
            "gcp_vertex_ai",
            "github_app",
        ] {
            assert!(
                !is_subscription_provider(id),
                "{id} should classify as an api_key"
            );
        }
    }

    #[test]
    fn models_dev_native_subscriptions_via_id_fallback() {
        // Not hand-registered as builtins — must match the id-pattern rule.
        for id in [
            "xiaomi-token-plan-cn",
            "xiaomi-token-plan-ams",
            "zhipuai-coding-plan",
            "alibaba-qwen-coding-plan",
            "tencent-hunyuan-coding-plan",
            "stepfun-ai",
            "kuae-cloud-coding-plan",
            "umans-ai-coding-plan",
            "moonshotai-coding-plan",
            "zai",
            "zhipuai",
            "opencode-go",
        ] {
            assert!(
                is_subscription_provider(id),
                "{id} should classify as a subscription via the id fallback"
            );
        }
    }

    #[test]
    fn jurisdiction_classifies_china_vendors_regardless_of_endpoint() {
        for id in [
            "minimax-coding-plan",
            "kimi-for-coding",
            "zai-coding-plan",
            "zhipuai-coding-plan",
            "xiaomi-token-plan-ams", // EU endpoint but Chinese vendor ⇒ Cn
            "xiaomi-token-plan-sgp", // Singapore endpoint but Chinese vendor ⇒ Cn
            "alibaba-qwen-coding-plan",
            "tencent-hunyuan-coding-plan",
            "moonshotai-coding-plan",
            "siliconflow-cn",
        ] {
            assert_eq!(
                provider_jurisdiction(id),
                Jurisdiction::Cn,
                "{id} should classify as China-hosted"
            );
            assert!(provider_jurisdiction(id).is_china(), "{id} is_china");
        }
    }

    #[test]
    fn jurisdiction_classifies_us_and_eu_and_other() {
        assert_eq!(provider_jurisdiction("chatgpt_codex"), Jurisdiction::Us);
        assert_eq!(provider_jurisdiction("github-copilot"), Jurisdiction::Us);
        assert_eq!(provider_jurisdiction("anthropic"), Jurisdiction::Us);
        // A non-Chinese vendor on an Amsterdam endpoint reads as EU.
        assert_eq!(
            provider_jurisdiction("acme-token-plan-ams"),
            Jurisdiction::Eu
        );
        // Unknown long tail and Singapore endpoints land in Other, never Cn.
        assert_eq!(
            provider_jurisdiction("some-random-coding-plan"),
            Jurisdiction::Other
        );
        assert_eq!(
            provider_jurisdiction("acme-token-plan-sgp"),
            Jurisdiction::Other
        );
    }

    #[test]
    fn recommended_map_picks_current_flagships_not_clutter() {
        // OpenAI: the GPT-5.5 flagship + latest codex are recommended; older
        // GPT-5.x variants and o-series are not.
        assert!(is_recommended_model("openai", "gpt-5.5"));
        assert!(is_recommended_model("openai", "gpt-5.3-codex"));
        assert!(is_recommended_model("openai", "openai/gpt-5.5")); // qualified form
        assert!(!is_recommended_model("openai", "gpt-5.4"));
        assert!(!is_recommended_model("openai", "gpt-5.2"));
        assert!(!is_recommended_model("openai", "o3"));
        assert!(!is_recommended_model("openai", "gpt-4o"));

        // Single-vendor subs → one flagship each.
        assert!(is_recommended_model("minimax-coding-plan", "MiniMax-M3"));
        assert!(!is_recommended_model("minimax-coding-plan", "MiniMax-M2"));
        assert!(is_recommended_model("kimi-for-coding", "k2p7"));
        assert!(!is_recommended_model("kimi-for-coding", "k2p5"));
        assert!(is_recommended_model("zai-coding-plan", "glm-5.2"));
        assert!(!is_recommended_model("zai-coding-plan", "glm-4.7"));
        assert!(is_recommended_model(
            "xiaomi-token-plan-sgp",
            "mimo-v2.5-pro"
        ));
        assert!(is_recommended_model("anthropic", "claude-opus-4-8"));

        // Codex child id resolves its own flagship, and the parent-namespaced
        // form (after re-tagging) still matches the openai recommendation.
        assert!(is_recommended_model("chatgpt_codex", "gpt-5.3-codex"));

        // Multi-model gateway → a small curated set, not one and not all.
        let oc = recommended_model_ids("opencode");
        assert!(
            oc.len() > 1 && oc.len() <= 4,
            "opencode set should be small"
        );
        assert!(is_recommended_model("opencode", "gpt-5.5"));
        assert!(!is_recommended_model("opencode", "claude-3-5-haiku"));
        assert!(is_recommended_model(
            "fireworks-ai",
            "accounts/fireworks/models/glm-5p2"
        ));
        assert!(is_recommended_model(
            "fireworks-ai",
            "fireworks-ai/accounts/fireworks/models/minimax-m3"
        ));

        // Unmapped provider ⇒ nothing recommended (UI shows all).
        assert!(recommended_model_ids("aws_bedrock").is_empty());
        assert!(!is_recommended_model("aws_bedrock", "anything"));
    }

    #[test]
    fn codex_is_a_governable_us_subscription() {
        // chatgpt_codex itself is a subscription, hosted in the US.
        assert!(is_subscription_provider("chatgpt_codex"));
        assert_eq!(provider_jurisdiction("chatgpt_codex"), Jurisdiction::Us);

        // A codex model surfaced under the openai namespace is attributed to the
        // chatgpt_codex subscription (so org policy can block it)…
        assert_eq!(
            governable_subscription_for_model("openai", "openai/gpt-5.3-codex").as_deref(),
            Some("chatgpt_codex")
        );
        assert_eq!(
            governable_subscription_for_model("openai", "gpt-5.5-codex").as_deref(),
            Some("chatgpt_codex")
        );
        // …while a plain openai API-key model stays UNGOVERNED.
        assert_eq!(
            governable_subscription_for_model("openai", "openai/gpt-5.5"),
            None
        );
        // A genuine subscription provider maps to its own (lowercased) id.
        assert_eq!(
            governable_subscription_for_model(
                "minimax-coding-plan",
                "minimax-coding-plan/MiniMax-M3"
            )
            .as_deref(),
            Some("minimax-coding-plan")
        );
        // Non-subscription providers are never governable.
        assert_eq!(
            governable_subscription_for_model("anthropic", "anthropic/x"),
            None
        );
    }

    #[test]
    fn generic_long_tail_defaults_to_api_key() {
        for id in [
            "deepseek",
            "some-random-openai-compatible",
            "groq",
            "mistral",
        ] {
            assert!(
                !is_subscription_provider(id),
                "{id} should default to api_key"
            );
        }
    }
}
