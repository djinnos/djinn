//! Provider resolution helpers for the slot crate.
//!
//! The pure provider-identification functions live here. Credential loading
//! (which involves OAuth refresh and is host-specific) delegates to
//! [`SlotHostCallbacks::resolve_provider_credential`].
//! Host-only credential serialization (for worker Pod Secrets / runtime wire payloads) intentionally
//! stays in `djinn-agent` behind that boundary.

use crate::host::SlotContext;

pub fn format_family_for_provider(
    provider_id: &str,
    model_id: &str,
) -> djinn_provider::provider::FormatFamily {
    use djinn_provider::provider::FormatFamily;
    let lower = provider_id.to_lowercase();
    if lower.contains("anthropic") || lower.contains("minimax") || lower.contains("kimi") {
        FormatFamily::Anthropic
    } else if lower.contains("google") || lower.contains("gemini") || lower.contains("vertex") {
        FormatFamily::Google
    } else if lower.contains("codex")
        || model_id.contains("codex")
        || (is_openai_responses_model(model_id) && is_native_openai_provider(&lower))
    {
        FormatFamily::OpenAIResponses
    } else {
        FormatFamily::OpenAI
    }
}

fn is_openai_responses_model(model_id: &str) -> bool {
    let lower = model_id.to_lowercase();
    lower.starts_with("gpt-5")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
}

fn is_native_openai_provider(provider_id_lower: &str) -> bool {
    provider_id_lower == "openai"
        || provider_id_lower.starts_with("openai")
        || provider_id_lower.contains("chatgpt")
}

pub fn capabilities_for_provider(
    provider_id: &str,
) -> djinn_provider::provider::ProviderCapabilities {
    use djinn_provider::provider::ProviderCapabilities;
    let lower = provider_id.to_lowercase();
    if lower.contains("synthetic") || lower.contains("local") {
        ProviderCapabilities {
            streaming: false,
            max_tokens_default: None,
        }
    } else if lower.contains("anthropic") || lower.contains("kimi") {
        ProviderCapabilities {
            streaming: true,
            max_tokens_default: Some(64_000),
        }
    } else {
        ProviderCapabilities::default()
    }
}

pub fn auth_method_for_provider(
    provider_id: &str,
    api_key: &str,
) -> djinn_provider::provider::AuthMethod {
    use djinn_provider::provider::AuthMethod;
    if provider_id.to_lowercase().contains("anthropic") {
        AuthMethod::ApiKeyHeader {
            header: "x-api-key".to_string(),
            key: api_key.to_string(),
        }
    } else {
        AuthMethod::BearerToken(api_key.to_string())
    }
}

pub fn default_base_url(provider_id: &str) -> String {
    let lower = provider_id.to_lowercase();
    if lower.contains("anthropic") {
        "https://api.anthropic.com".to_string()
    } else if lower.contains("google") || lower.contains("gemini") {
        "https://generativelanguage.googleapis.com".to_string()
    } else {
        "https://api.openai.com".to_string()
    }
}

/// Resolved provider credentials — either an API key from the vault or an
/// OAuth-derived `ProviderConfig` that already carries the right base URL,
/// auth method, and model defaults.
pub enum ProviderCredential {
    /// Traditional API-key credential (key_name, decrypted key).
    ApiKey(String, String),
    /// OAuth-derived full provider config (base_url, auth, model already set).
    OAuthConfig(Box<djinn_provider::provider::ProviderConfig>),
}

impl ProviderCredential {
    /// Stamp the resolved per-role model onto an OAuth-derived config.
    pub fn with_model_id(mut self, model_id: &str) -> Self {
        if let ProviderCredential::OAuthConfig(cfg) = &mut self {
            cfg.model_id = model_id.to_string();
        }
        self
    }
}

pub async fn load_provider_credential(
    provider_id: &str,
    ctx: &SlotContext,
) -> anyhow::Result<ProviderCredential> {
    ctx.callbacks
        .resolve_provider_credential(provider_id, ctx)
        .await
        .map_err(|e| anyhow::anyhow!("credential resolution failed: {e}"))
}

pub fn parse_model_id(model_id: &str) -> anyhow::Result<(String, String)> {
    let Some((provider_id, model_name)) = model_id.split_once('/') else {
        return Err(anyhow::anyhow!(
            "invalid model id '{model_id}', expected provider/model"
        ));
    };
    Ok((provider_id.to_owned(), model_name.to_owned()))
}

pub(crate) fn build_telemetry_meta(
    agent_type_str: &str,
    task_id: &str,
) -> djinn_provider::provider::TelemetryMeta {
    build_telemetry_meta_with_attribution(agent_type_str, task_id, None, None)
}

pub(crate) fn build_telemetry_meta_with_attribution(
    agent_type_str: &str,
    task_id: &str,
    operation: Option<&str>,
    user_id: Option<&str>,
) -> djinn_provider::provider::TelemetryMeta {
    djinn_provider::provider::TelemetryMeta {
        task_id: Some(task_id.to_owned()),
        agent_type: Some(agent_type_str.to_owned()),
        session_id: Some(task_id.to_owned()),
        operation: operation.map(str::to_owned),
        user_id: user_id.map(str::to_owned),
    }
}

/// Build an [`LlmProvider`] from a resolved model + credential.
pub(crate) fn build_provider_from_resolved(
    resolved: crate::lifecycle::model_resolution::ResolvedModelCredential,
    context_window: u32,
    telemetry: Option<djinn_provider::provider::TelemetryMeta>,
    session_affinity_key: Option<String>,
    base_url: String,
) -> Option<Box<dyn djinn_provider::provider::LlmProvider>> {
    match resolved.provider_credential {
        Some(ProviderCredential::OAuthConfig(mut cfg)) => {
            cfg.model_id = resolved.model_name.clone();
            cfg.context_window = context_window;
            cfg.telemetry = telemetry;
            cfg.session_affinity_key = session_affinity_key;
            Some(djinn_provider::provider::create_provider(*cfg))
        }
        Some(ProviderCredential::ApiKey(_key_name, api_key)) => {
            let provider_headers = provider_headers_for(
                &resolved.catalog_provider_id,
                session_affinity_key.as_deref(),
            );
            Some(djinn_provider::provider::create_provider(
                djinn_provider::provider::ProviderConfig {
                    base_url,
                    auth: auth_method_for_provider(&resolved.catalog_provider_id, &api_key),
                    format_family: format_family_for_provider(
                        &resolved.catalog_provider_id,
                        &resolved.model_name,
                    ),
                    model_id: resolved.model_name.clone(),
                    context_window,
                    telemetry,
                    session_affinity_key,
                    provider_headers,
                    capabilities: capabilities_for_provider(&resolved.catalog_provider_id),
                    reasoning_effort: None,
                    tool_schema_compat: djinn_provider::catalog::builtin::tool_schema_compat_for(
                        &resolved.catalog_provider_id,
                        &resolved.model_name,
                    ),
                },
            ))
        }
        None => None,
    }
}

fn provider_headers_for(
    provider_id: &str,
    session_key: Option<&str>,
) -> std::collections::HashMap<String, String> {
    let mut headers = std::collections::HashMap::new();
    if provider_id == "opencode" {
        let sid = session_key.unwrap_or("djinn").to_string();
        headers.insert("x-opencode-client".to_string(), "cli".to_string());
        headers.insert("User-Agent".to_string(), "opencode/1.15.12".to_string());
        headers.insert("x-opencode-session".to_string(), sid.clone());
        headers.insert("x-opencode-project".to_string(), sid.clone());
        headers.insert("x-opencode-request".to_string(), sid);
    }
    headers
}

pub(crate) fn resolved_needs_base_url(
    resolved: &crate::lifecycle::model_resolution::ResolvedModelCredential,
) -> bool {
    matches!(
        resolved.provider_credential,
        Some(ProviderCredential::ApiKey(..))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to build a `ResolvedModelCredential` with an API-key credential
    /// for the given provider/model.
    fn mock_resolved(
        provider_id: &str,
        model_name: &str,
    ) -> crate::lifecycle::model_resolution::ResolvedModelCredential {
        crate::lifecycle::model_resolution::ResolvedModelCredential {
            catalog_provider_id: provider_id.to_string(),
            model_name: model_name.to_string(),
            provider_credential: Some(ProviderCredential::ApiKey(
                "TEST_KEY".to_string(),
                "sk-test".to_string(),
            )),
        }
    }

    #[test]
    fn kimi_quirked_model_receives_moonshot_compat() {
        let resolved = mock_resolved("kimi-for-coding", "k2p7");
        let provider = build_provider_from_resolved(
            resolved,
            128_000,
            None,
            None,
            "https://api.example.com".to_string(),
        )
        .expect("provider should be created");

        let config = provider
            .config_snapshot()
            .expect("concrete provider exposes config");
        assert_eq!(
            config.tool_schema_compat,
            Some(djinn_provider::provider::ToolSchemaCompat::Moonshot),
        );
    }

    #[test]
    fn minimax_quirked_model_receives_moonshot_compat() {
        let resolved = mock_resolved("minimax-coding-plan", "MiniMax-M3");
        let provider = build_provider_from_resolved(
            resolved,
            64_000,
            None,
            None,
            "https://api.example.com".to_string(),
        )
        .expect("provider should be created");

        let config = provider
            .config_snapshot()
            .expect("concrete provider exposes config");
        assert_eq!(
            config.tool_schema_compat,
            Some(djinn_provider::provider::ToolSchemaCompat::Moonshot),
        );
    }

    #[test]
    fn google_quirked_model_receives_gemini_compat() {
        let resolved = mock_resolved("google", "gemini-2.5-pro");
        let provider = build_provider_from_resolved(
            resolved,
            1_000_000,
            None,
            None,
            "https://generativelanguage.googleapis.com".to_string(),
        )
        .expect("provider should be created");

        let config = provider
            .config_snapshot()
            .expect("concrete provider exposes config");
        assert_eq!(
            config.tool_schema_compat,
            Some(djinn_provider::provider::ToolSchemaCompat::Gemini),
        );
    }

    #[test]
    fn openai_native_model_receives_none_compat() {
        let resolved = mock_resolved("openai", "gpt-4.1-mini");
        let provider = build_provider_from_resolved(
            resolved,
            128_000,
            None,
            None,
            "https://api.openai.com".to_string(),
        )
        .expect("provider should be created");

        let config = provider
            .config_snapshot()
            .expect("concrete provider exposes config");
        assert_eq!(config.tool_schema_compat, None);
    }

    #[test]
    fn anthropic_native_model_receives_none_compat() {
        let resolved = mock_resolved("anthropic", "claude-3-5-haiku");
        let provider = build_provider_from_resolved(
            resolved,
            200_000,
            None,
            None,
            "https://api.anthropic.com".to_string(),
        )
        .expect("provider should be created");

        let config = provider
            .config_snapshot()
            .expect("concrete provider exposes config");
        assert_eq!(config.tool_schema_compat, None);
    }
}
