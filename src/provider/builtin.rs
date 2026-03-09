//! Static registry of built-in LLM providers that Djinn knows how to use.
//!
//! Replaces the runtime `goose::providers::providers()` call with a
//! compile-time list.  Each entry carries the metadata the catalog and
//! provider-tools layers need: required env vars, OAuth key names, and
//! documentation links.

use std::collections::HashSet;

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
    },
    BuiltinProvider {
        id: "openai",
        display_name: "OpenAI",
        required_env_vars: &["OPENAI_API_KEY"],
        oauth_keys: &[],
        docs_url: "https://platform.openai.com/docs",
    },
    BuiltinProvider {
        id: "google",
        display_name: "Google AI",
        required_env_vars: &["GOOGLE_API_KEY"],
        oauth_keys: &[],
        docs_url: "https://ai.google.dev/docs",
    },
    BuiltinProvider {
        id: "chatgpt_codex",
        display_name: "ChatGPT Codex",
        required_env_vars: &[],
        oauth_keys: &["CHATGPT_CODEX_TOKEN"],
        docs_url: "https://platform.openai.com/docs",
    },
    BuiltinProvider {
        id: "githubcopilot",
        display_name: "GitHub Copilot",
        required_env_vars: &[],
        oauth_keys: &["GITHUB_COPILOT_TOKEN"],
        docs_url: "https://docs.github.com/en/copilot",
    },
    BuiltinProvider {
        id: "gcp_vertex_ai",
        display_name: "Google Cloud Vertex AI",
        required_env_vars: &["GCP_VERTEX_PROJECT_ID"],
        oauth_keys: &[],
        docs_url: "https://cloud.google.com/vertex-ai/docs",
    },
    BuiltinProvider {
        id: "aws_bedrock",
        display_name: "AWS Bedrock",
        required_env_vars: &["AWS_ACCESS_KEY_ID"],
        oauth_keys: &[],
        docs_url: "https://docs.aws.amazon.com/bedrock/",
    },
    BuiltinProvider {
        id: "azure_openai",
        display_name: "Azure OpenAI",
        required_env_vars: &["AZURE_OPENAI_API_KEY"],
        oauth_keys: &[],
        docs_url: "https://learn.microsoft.com/en-us/azure/ai-services/openai/",
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

/// Check whether a provider is a built-in (not custom/user-added).
pub fn is_builtin_provider(provider_id: &str) -> bool {
    find_builtin_provider(provider_id).is_some()
}

/// Strip non-alphanumeric chars and lowercase for fuzzy ID matching.
fn canonical_id(id: &str) -> String {
    id.chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

/// Check whether any of the given OAuth keys have a stored token on disk.
///
/// Looks in `~/.djinn/oauth/` for token files written by
/// [`crate::agent::oauth::codex`] and [`crate::agent::oauth::copilot`].
pub fn is_oauth_key_present(oauth_keys: &[String]) -> bool {
    use crate::agent::oauth::{codex::CodexTokens, copilot::CopilotTokens};

    oauth_keys.iter().any(|key| match key.as_str() {
        "CHATGPT_CODEX_TOKEN" => CodexTokens::load_cached()
            .map(|t| !t.is_expired())
            .unwrap_or(false),
        "GITHUB_COPILOT_TOKEN" => CopilotTokens::load_cached().is_some(),
        _ => false,
    })
}
