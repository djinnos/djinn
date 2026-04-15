pub mod codex;
pub mod copilot;
/// Legacy GitHub OAuth App device-code flow.
///
/// Retained because the CoordinatorActor still uses the resulting long-lived
/// user token to push commits as the authenticated user. New code should
/// prefer [`crate::github_app`] (installation-token based). The migration of
/// CoordinatorActor is tracked in `TODO.md`.
pub mod github_oauth_app_legacy;
/// Backwards-compatible alias so existing
/// `use djinn_provider::oauth::github_app::*` imports keep resolving during
/// the transition. Prefer importing from [`crate::github_app`] in new code.
pub use github_oauth_app_legacy as github_app;

use crate::provider::{AuthMethod, FormatFamily, ProviderCapabilities, ProviderConfig};

/// Which OAuth flow to run, determined by provider ID.
pub enum OAuthFlowKind {
    Codex,
    Copilot,
    GitHubApp,
}

impl OAuthFlowKind {
    /// Map a provider ID string to the appropriate OAuth flow, if one exists.
    pub fn from_provider_id(id: &str) -> Option<Self> {
        match id {
            "chatgpt-codex" | "chatgpt_codex" | "codex" | "openai-codex" => Some(Self::Codex),
            "github-copilot" | "github_copilot" | "copilot" => Some(Self::Copilot),
            "github-app" | "github_app" | "githubapp" => Some(Self::GitHubApp),
            _ => None,
        }
    }
}

/// Build a `ProviderConfig` for the Codex provider using the stored tokens.
pub fn codex_provider_config(tokens: &codex::CodexTokens) -> ProviderConfig {
    let mut provider_headers = std::collections::HashMap::new();
    if let Some(account_id) = &tokens.account_id {
        provider_headers.insert("chatgpt-account-id".to_string(), account_id.clone());
    }
    ProviderConfig {
        base_url: codex::CODEX_API_BASE.to_string(),
        auth: AuthMethod::BearerToken(tokens.access_token.clone()),
        format_family: FormatFamily::OpenAIResponses,
        model_id: codex::CODEX_DEFAULT_MODEL.to_string(),
        context_window: 128_000,
        telemetry: None,
        session_affinity_key: None,
        provider_headers,
        capabilities: ProviderCapabilities::default(),
    }
}

/// Build a `ProviderConfig` for the Copilot provider using the stored tokens.
pub fn copilot_provider_config(tokens: &copilot::CopilotTokens) -> ProviderConfig {
    ProviderConfig {
        base_url: tokens.api_endpoint.clone(),
        auth: AuthMethod::BearerToken(tokens.copilot_token.clone()),
        format_family: FormatFamily::OpenAI,
        model_id: copilot::COPILOT_DEFAULT_MODEL.to_string(),
        context_window: 128_000,
        telemetry: None,
        session_affinity_key: None,
        provider_headers: Default::default(),
        capabilities: ProviderCapabilities::default(),
    }
}
