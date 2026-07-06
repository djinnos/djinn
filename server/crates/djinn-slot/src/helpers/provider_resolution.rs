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
    /// Restamp the resolved per-role model and re-resolve model-dependent
    /// defaults onto an OAuth-derived config using the shared restamp helper.
    ///
    /// No-op for API-key credentials; the worker stamps those from the
    /// spec's per-role model.
    pub fn restamp_to(self, target: &djinn_provider::provider::RestampTarget) -> Self {
        if let ProviderCredential::OAuthConfig(cfg) = self {
            let cfg = djinn_provider::provider::restamp_provider_config_for_model(*cfg, target);
            ProviderCredential::OAuthConfig(Box::new(cfg))
        } else {
            self
        }
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

/// Build a [`RestampTarget`] from resolved model identity, the model's
/// reasoning capability flag, and the caller's context-window budget.
///
/// This centralises the catalog → restamp-target derivation so both
/// OAuth and API-key construction paths share identical defaulting policy.
fn build_restamp_target(
    resolved: &crate::lifecycle::model_resolution::ResolvedModelCredential,
    context_window: u32,
) -> djinn_provider::provider::RestampTarget {
    djinn_provider::provider::RestampTarget {
        model_id: resolved.model_name.clone(),
        format_family: format_family_for_provider(
            &resolved.catalog_provider_id,
            &resolved.model_name,
        ),
        reasoning: resolved.reasoning,
        context_window,
        capabilities: capabilities_for_provider(&resolved.catalog_provider_id),
        tool_schema_compat: djinn_provider::catalog::builtin::tool_schema_compat_for(
            &resolved.catalog_provider_id,
            &resolved.model_name,
        ),
    }
}

/// Build an [`LlmProvider`] from a resolved model + credential.
///
/// Both the OAuth and API-key paths route through the shared
/// [`restamp_provider_config_for_model`] helper so model-dependent fields
/// (`reasoning_effort`, `max_tokens_default`, `tool_schema_compat`, etc.)
/// are re-resolved from the target model rather than carried stale from a
/// previous failover source.
pub(crate) fn build_provider_from_resolved(
    resolved: crate::lifecycle::model_resolution::ResolvedModelCredential,
    context_window: u32,
    telemetry: Option<djinn_provider::provider::TelemetryMeta>,
    session_affinity_key: Option<String>,
    base_url: String,
) -> Option<Box<dyn djinn_provider::provider::LlmProvider>> {
    let target = build_restamp_target(&resolved, context_window);
    match resolved.provider_credential {
        Some(ProviderCredential::OAuthConfig(cfg)) => {
            // Restamp the OAuth config: re-resolve model_id, format_family,
            // reasoning_effort, capabilities, tool_schema_compat from the
            // target model. Transport/session fields (base_url, auth,
            // provider_headers) are preserved by the restamp helper.
            let mut cfg =
                djinn_provider::provider::restamp_provider_config_for_model(*cfg, &target);
            // Per-run fields that the restamp helper does not own:
            cfg.telemetry = telemetry;
            cfg.session_affinity_key = session_affinity_key;
            Some(djinn_provider::provider::create_provider(cfg))
        }
        Some(ProviderCredential::ApiKey(_key_name, api_key)) => {
            let provider_headers = provider_headers_for(
                &resolved.catalog_provider_id,
                session_affinity_key.as_deref(),
            );
            // Build a skeleton config with transport/session fields, then
            // restamp all model-dependent defaults through the shared helper.
            let skeleton = djinn_provider::provider::ProviderConfig {
                base_url,
                auth: auth_method_for_provider(&resolved.catalog_provider_id, &api_key),
                // Stale values — restamp overwrites them all:
                format_family: djinn_provider::provider::FormatFamily::OpenAI,
                model_id: String::new(),
                context_window: 0,
                capabilities: djinn_provider::provider::ProviderCapabilities::default(),
                reasoning_effort: None,
                tool_schema_compat: None,
                telemetry,
                session_affinity_key,
                provider_headers,
            };
            let cfg =
                djinn_provider::provider::restamp_provider_config_for_model(skeleton, &target);
            Some(djinn_provider::provider::create_provider(cfg))
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
    use djinn_provider::provider::{ReasoningEffort, RestampTarget};

    /// Helper to build a `ResolvedModelCredential` with an API-key credential
    /// for the given provider/model. Defaults `reasoning` to `false`.
    fn mock_resolved(
        provider_id: &str,
        model_name: &str,
    ) -> crate::lifecycle::model_resolution::ResolvedModelCredential {
        mock_resolved_with_reasoning(provider_id, model_name, false)
    }

    /// Helper to build a `ResolvedModelCredential` with an explicit
    /// `reasoning` flag.
    fn mock_resolved_with_reasoning(
        provider_id: &str,
        model_name: &str,
        reasoning: bool,
    ) -> crate::lifecycle::model_resolution::ResolvedModelCredential {
        crate::lifecycle::model_resolution::ResolvedModelCredential {
            catalog_provider_id: provider_id.to_string(),
            model_name: model_name.to_string(),
            provider_credential: Some(ProviderCredential::ApiKey(
                "TEST_KEY".to_string(),
                "sk-test".to_string(),
            )),
            reasoning,
        }
    }

    /// Helper to build a `ProviderCredential::OAuthConfig` with reasonable
    /// defaults that can be restamped.
    fn mock_oauth_credential(provider_id: &str, model_id: &str) -> ProviderCredential {
        ProviderCredential::OAuthConfig(Box::new(djinn_provider::provider::ProviderConfig {
            base_url: format!("https://api.{provider_id}.example.com"),
            auth: djinn_provider::provider::AuthMethod::BearerToken("oauth-token".to_string()),
            format_family: format_family_for_provider(provider_id, model_id),
            model_id: model_id.to_string(),
            context_window: 128_000,
            telemetry: None,
            session_affinity_key: None,
            provider_headers: std::collections::HashMap::new(),
            capabilities: capabilities_for_provider(provider_id),
            reasoning_effort: None,
            tool_schema_compat: djinn_provider::catalog::builtin::tool_schema_compat_for(
                provider_id,
                model_id,
            ),
        }))
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

    // ── AC3: Failover/restamp regression tests ─────────────────────────

    #[test]
    fn failover_to_anthropic_reasoning_model_resolves_reasoning_effort() {
        // Model A (OpenAI, non-reasoning) → failover to B (Anthropic, reasoning).
        // The API-key path must resolve reasoning_effort to Some(Medium) for B.
        let resolved = mock_resolved_with_reasoning("anthropic", "claude-sonnet-4", true);
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
        assert_eq!(
            config.reasoning_effort,
            Some(ReasoningEffort::Medium),
            "Anthropic reasoning model must resolve reasoning_effort to Some(Medium)"
        );
        assert_eq!(
            config.model_id, "claude-sonnet-4",
            "model_id must be stamped to the target"
        );
        assert_eq!(
            config.capabilities.max_tokens_default,
            Some(64_000),
            "max_tokens_default must be re-resolved from the target capabilities"
        );
    }

    #[test]
    fn failover_to_kimi_reasoning_model_resolves_reasoning_effort() {
        // Failover to a Kimi (Anthropic-format) reasoning model.
        let resolved = mock_resolved_with_reasoning("kimi-for-coding", "k2p7", true);
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
            config.reasoning_effort,
            Some(ReasoningEffort::Medium),
            "Kimi (Anthropic-format) reasoning model must resolve reasoning_effort to Some(Medium)"
        );
        assert_eq!(
            config.capabilities.max_tokens_default,
            Some(64_000),
            "Kimi max_tokens_default must be re-resolved"
        );
        assert_eq!(
            config.tool_schema_compat,
            Some(djinn_provider::provider::ToolSchemaCompat::Moonshot),
            "Kimi must keep its Moonshot tool-schema quirk"
        );
    }

    #[test]
    fn failover_to_minimax_reasoning_model_resolves_reasoning_effort() {
        // Failover to a MiniMax (Anthropic-format) reasoning model.
        let resolved = mock_resolved_with_reasoning("minimax-coding-plan", "MiniMax-M3", true);
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
            config.reasoning_effort,
            Some(ReasoningEffort::Medium),
            "MiniMax (Anthropic-format) reasoning model must resolve reasoning_effort"
        );
    }

    #[test]
    fn failover_to_non_reasoning_model_clears_reasoning_effort() {
        // Failover to a non-reasoning model — reasoning_effort must be None.
        let resolved = mock_resolved_with_reasoning("openai", "gpt-4.1-mini", false);
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
        assert_eq!(
            config.reasoning_effort, None,
            "Non-reasoning model must have reasoning_effort = None"
        );
    }

    #[test]
    fn failover_preserves_session_and_telemetry_fields() {
        // Restamp from model A → model B; per-run fields must survive.
        use djinn_provider::provider::TelemetryMeta;

        let telemetry = TelemetryMeta {
            task_id: Some("task-42".to_string()),
            agent_type: Some("worker".to_string()),
            session_id: Some("session-7".to_string()),
            operation: Some("completion".to_string()),
            user_id: Some("user-1".to_string()),
        };
        let session_key = Some("affinity-key-abc".to_string());

        let resolved = mock_resolved_with_reasoning("anthropic", "claude-sonnet-4", true);
        let provider = build_provider_from_resolved(
            resolved,
            200_000,
            Some(telemetry.clone()),
            session_key.clone(),
            "https://api.anthropic.com".to_string(),
        )
        .expect("provider should be created");

        let config = provider
            .config_snapshot()
            .expect("concrete provider exposes config");

        assert_eq!(
            config.telemetry.as_ref().unwrap().task_id,
            Some("task-42".to_string())
        );
        assert_eq!(
            config.telemetry.as_ref().unwrap().agent_type,
            Some("worker".to_string())
        );
        assert_eq!(
            config.telemetry.as_ref().unwrap().session_id,
            Some("session-7".to_string())
        );
        assert_eq!(
            config.telemetry.as_ref().unwrap().operation,
            Some("completion".to_string())
        );
        assert_eq!(
            config.telemetry.as_ref().unwrap().user_id,
            Some("user-1".to_string())
        );
        assert_eq!(config.session_affinity_key, session_key);
        // Model-dependent fields must be the target's, not stale:
        assert_eq!(config.model_id, "claude-sonnet-4");
        assert_eq!(config.reasoning_effort, Some(ReasoningEffort::Medium));
    }

    #[test]
    fn oauth_restamp_to_reasoning_model_resolves_defaults() {
        // OAuth credential for a non-reasoning model is restamped onto a
        // reasoning Anthropic target. The restamp must resolve
        // reasoning_effort and max_tokens_default.
        let cred = mock_oauth_credential("openai", "gpt-4.1-mini");
        let target = RestampTarget {
            model_id: "claude-sonnet-4".to_string(),
            format_family: djinn_provider::provider::FormatFamily::Anthropic,
            reasoning: true,
            context_window: 200_000,
            capabilities: capabilities_for_provider("anthropic"),
            tool_schema_compat: None,
        };

        let restamped = cred.restamp_to(&target);
        if let ProviderCredential::OAuthConfig(cfg) = restamped {
            assert_eq!(cfg.model_id, "claude-sonnet-4");
            assert_eq!(
                cfg.reasoning_effort,
                Some(ReasoningEffort::Medium),
                "OAuth restamp must resolve reasoning_effort for the target model"
            );
            assert_eq!(
                cfg.capabilities.max_tokens_default,
                Some(64_000),
                "OAuth restamp must resolve max_tokens_default from target capabilities"
            );
            // Transport fields preserved from the source OAuth config:
            assert!(
                cfg.base_url.contains("openai"),
                "base_url must be preserved from the source config"
            );
            assert!(
                matches!(&cfg.auth, djinn_provider::provider::AuthMethod::BearerToken(t) if t == "oauth-token"),
                "auth must be preserved from the source config"
            );
        } else {
            panic!("restamp_to on OAuthConfig must yield OAuthConfig");
        }
    }

    #[test]
    fn oauth_restamp_preserves_telemetry_and_session_affinity() {
        use djinn_provider::provider::TelemetryMeta;

        let mut cred = mock_oauth_credential("anthropic", "claude-3-5-haiku");
        // Simulate per-run fields already on the OAuth config:
        if let ProviderCredential::OAuthConfig(ref mut cfg) = cred {
            cfg.telemetry = Some(TelemetryMeta {
                task_id: Some("task-99".to_string()),
                agent_type: Some("reviewer".to_string()),
                session_id: Some("sess-99".to_string()),
                operation: None,
                user_id: None,
            });
            cfg.session_affinity_key = Some("affinity-99".to_string());
        }

        let target = RestampTarget {
            model_id: "claude-sonnet-4".to_string(),
            format_family: djinn_provider::provider::FormatFamily::Anthropic,
            reasoning: true,
            context_window: 200_000,
            capabilities: capabilities_for_provider("anthropic"),
            tool_schema_compat: None,
        };

        let restamped = cred.restamp_to(&target);
        if let ProviderCredential::OAuthConfig(cfg) = restamped {
            assert_eq!(cfg.model_id, "claude-sonnet-4");
            assert_eq!(cfg.reasoning_effort, Some(ReasoningEffort::Medium));
            // Per-run fields preserved:
            assert_eq!(
                cfg.telemetry.as_ref().unwrap().task_id,
                Some("task-99".to_string())
            );
            assert_eq!(cfg.session_affinity_key, Some("affinity-99".to_string()));
        } else {
            panic!("restamp_to on OAuthConfig must yield OAuthConfig");
        }
    }
}
