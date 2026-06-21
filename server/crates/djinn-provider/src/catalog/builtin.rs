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
