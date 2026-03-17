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
    /// If set, this provider's OAuth capabilities are folded into the
    /// given parent in the catalog response and this entry is hidden.
    /// Internally the provider still exists for dispatch/model sourcing.
    pub merge_into: Option<&'static str>,
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
    },
    BuiltinProvider {
        id: "openai",
        display_name: "OpenAI",
        required_env_vars: &["OPENAI_API_KEY"],
        oauth_keys: &[],
        docs_url: "https://platform.openai.com/docs",
        merge_into: None,
    },
    BuiltinProvider {
        id: "google",
        display_name: "Google AI",
        required_env_vars: &["GOOGLE_API_KEY"],
        oauth_keys: &[],
        docs_url: "https://ai.google.dev/docs",
        merge_into: None,
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
    },
    BuiltinProvider {
        id: "githubcopilot",
        display_name: "GitHub Copilot",
        required_env_vars: &[],
        oauth_keys: &["GITHUB_COPILOT_TOKEN"],
        docs_url: "https://docs.github.com/en/copilot",
        merge_into: None,
    },
    BuiltinProvider {
        id: "gcp_vertex_ai",
        display_name: "Google Cloud Vertex AI",
        required_env_vars: &["GCP_VERTEX_PROJECT_ID"],
        oauth_keys: &[],
        docs_url: "https://cloud.google.com/vertex-ai/docs",
        merge_into: None,
    },
    BuiltinProvider {
        id: "aws_bedrock",
        display_name: "AWS Bedrock",
        required_env_vars: &["AWS_ACCESS_KEY_ID"],
        oauth_keys: &[],
        docs_url: "https://docs.aws.amazon.com/bedrock/",
        merge_into: None,
    },
    BuiltinProvider {
        id: "azure_openai",
        display_name: "Azure OpenAI",
        required_env_vars: &["AZURE_OPENAI_API_KEY"],
        oauth_keys: &[],
        docs_url: "https://learn.microsoft.com/en-us/azure/ai-services/openai/",
        merge_into: None,
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
        _ => false,
    })
}
