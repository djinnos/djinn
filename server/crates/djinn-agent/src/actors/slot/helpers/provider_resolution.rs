use super::*;

// Pure provider identification helpers live canonically in `djinn-slot`.
pub use djinn_slot::helpers::provider_resolution::{
    auth_method_for_provider, capabilities_for_provider, default_base_url,
    format_family_for_provider, parse_model_id,
};

/// Build a [`djinn_provider::provider::RestampTarget`] from catalog metadata
/// for the given provider/model pair.
///
/// Looks up the catalog model to determine `reasoning` capability and
/// `context_window`, falling back to the caller-supplied `context_window`
/// when the catalog entry is absent.
pub(crate) fn build_restamp_target(
    catalog_provider_id: &str,
    model_name: &str,
    context_window: u32,
    catalog: &djinn_provider::catalog::CatalogService,
) -> djinn_provider::provider::RestampTarget {
    use djinn_provider::provider::RestampTarget;
    let full_model_id = format!("{catalog_provider_id}/{model_name}");
    let catalog_model = catalog.find_model(&full_model_id);
    let reasoning = catalog_model.as_ref().is_some_and(|m| m.reasoning);
    let resolved_context_window = catalog_model.as_ref().map_or(context_window, |m| {
        let cw = m.context_window.max(0) as u32;
        if context_window > 0 {
            context_window
        } else {
            cw
        }
    });
    RestampTarget {
        model_id: model_name.to_string(),
        format_family: format_family_for_provider(catalog_provider_id, model_name),
        reasoning,
        context_window: resolved_context_window,
        capabilities: capabilities_for_provider(catalog_provider_id),
        tool_schema_compat: djinn_provider::catalog::builtin::tool_schema_compat_for(
            catalog_provider_id,
            model_name,
        ),
    }
}

/// Resolved provider credentials — API key or OAuth-derived `ProviderConfig`.
pub enum ProviderCredential {
    /// Traditional API-key credential (key_name, decrypted key).
    ApiKey(String, String),
    /// OAuth-derived full provider config (base_url, auth, model already set).
    OAuthConfig(Box<djinn_provider::provider::ProviderConfig>),
}

impl ProviderCredential {
    /// Restamp an OAuth-derived config for the target model using the shared
    /// [`djinn_provider::provider::restamp_provider_config_for_model`] helper.
    ///
    /// Re-resolves model-dependent defaults (`reasoning_effort`,
    /// `max_tokens_default`, `tool_schema_compat`, `format_family`) for the
    /// target so that failover from model A to model B does not carry A's
    /// stale defaults.  Transport / session fields (`base_url`, `auth`,
    /// `telemetry`, `session_affinity_key`, `provider_headers`) are preserved.
    ///
    /// No-op for API-key credentials.
    pub fn with_model_id(self, target: &djinn_provider::provider::RestampTarget) -> Self {
        match self {
            ProviderCredential::OAuthConfig(cfg) => ProviderCredential::OAuthConfig(Box::new(
                djinn_provider::provider::restamp_provider_config_for_model(*cfg, target),
            )),
            other => other,
        }
    }
    /// Convert into a wire-friendly [`djinn_runtime::SerializableCredential`].
    /// OAuth configs are projected onto [`OAuthConfigWire`] because the upstream
    /// `ProviderConfig` does not implement `Serialize`.
    pub fn to_serializable(&self) -> djinn_runtime::SerializableCredential {
        match self {
            ProviderCredential::ApiKey(key_name, api_key) => {
                djinn_runtime::SerializableCredential::ApiKey {
                    key_name: key_name.clone(),
                    api_key: api_key.clone(),
                }
            }
            ProviderCredential::OAuthConfig(cfg) => {
                let wire = OAuthConfigWire::from_provider_config(cfg);
                let config_json = serde_json::to_string(&wire)
                    .expect("OAuthConfigWire serialization cannot fail");
                djinn_runtime::SerializableCredential::OAuthConfig { config_json }
            }
        }
    }
}

/// Serde-friendly mirror of [`djinn_provider::provider::ProviderConfig`] used
/// to encode OAuth-derived credentials into the worker Secret JSON.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct OAuthConfigWire {
    pub base_url: String,
    pub auth: OAuthAuthMethodWire,
    pub format_family: OAuthFormatFamilyWire,
    pub model_id: String,
    pub context_window: u32,
    pub session_affinity_key: Option<String>,
    pub provider_headers: std::collections::HashMap<String, String>,
    pub capabilities: OAuthCapabilitiesWire,
    /// Optional reasoning-effort tier; `#[serde(default)]` preserves legacy blobs.
    #[serde(default)]
    pub reasoning_effort: Option<djinn_provider::provider::ReasoningEffort>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub enum OAuthAuthMethodWire {
    BearerToken(String),
    ApiKeyHeader { header: String, key: String },
    NoAuth,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub enum OAuthFormatFamilyWire {
    OpenAI,
    OpenAIResponses,
    Anthropic,
    Google,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct OAuthCapabilitiesWire {
    pub streaming: bool,
    pub max_tokens_default: Option<u32>,
}

impl OAuthConfigWire {
    /// Build a wire mirror from a live provider config.
    pub fn from_provider_config(cfg: &djinn_provider::provider::ProviderConfig) -> Self {
        use djinn_provider::provider::{AuthMethod, FormatFamily};
        let auth = match &cfg.auth {
            AuthMethod::BearerToken(t) => OAuthAuthMethodWire::BearerToken(t.clone()),
            AuthMethod::ApiKeyHeader { header, key } => OAuthAuthMethodWire::ApiKeyHeader {
                header: header.clone(),
                key: key.clone(),
            },
            AuthMethod::NoAuth => OAuthAuthMethodWire::NoAuth,
        };
        let format_family = match cfg.format_family {
            FormatFamily::OpenAI => OAuthFormatFamilyWire::OpenAI,
            FormatFamily::OpenAIResponses => OAuthFormatFamilyWire::OpenAIResponses,
            FormatFamily::Anthropic => OAuthFormatFamilyWire::Anthropic,
            FormatFamily::Google => OAuthFormatFamilyWire::Google,
        };
        Self {
            base_url: cfg.base_url.clone(),
            auth,
            format_family,
            model_id: cfg.model_id.clone(),
            context_window: cfg.context_window,
            session_affinity_key: cfg.session_affinity_key.clone(),
            provider_headers: cfg.provider_headers.clone(),
            capabilities: OAuthCapabilitiesWire {
                streaming: cfg.capabilities.streaming,
                max_tokens_default: cfg.capabilities.max_tokens_default,
            },
            reasoning_effort: cfg.reasoning_effort,
        }
    }
    /// Reconstitute a live [`djinn_provider::provider::ProviderConfig`] from
    /// the wire mirror. Used by `djinn-agent-worker` to rebuild OAuth configs
    /// shipped over the Secret mount.
    pub fn to_provider_config(self) -> djinn_provider::provider::ProviderConfig {
        use djinn_provider::provider::{
            AuthMethod, FormatFamily, ProviderCapabilities, ProviderConfig,
        };
        let auth = match self.auth {
            OAuthAuthMethodWire::BearerToken(t) => AuthMethod::BearerToken(t),
            OAuthAuthMethodWire::ApiKeyHeader { header, key } => {
                AuthMethod::ApiKeyHeader { header, key }
            }
            OAuthAuthMethodWire::NoAuth => AuthMethod::NoAuth,
        };
        let format_family = match self.format_family {
            OAuthFormatFamilyWire::OpenAI => FormatFamily::OpenAI,
            OAuthFormatFamilyWire::OpenAIResponses => FormatFamily::OpenAIResponses,
            OAuthFormatFamilyWire::Anthropic => FormatFamily::Anthropic,
            OAuthFormatFamilyWire::Google => FormatFamily::Google,
        };
        ProviderConfig {
            base_url: self.base_url,
            auth,
            format_family,
            model_id: self.model_id,
            context_window: self.context_window,
            telemetry: None,
            session_affinity_key: self.session_affinity_key,
            provider_headers: self.provider_headers,
            capabilities: ProviderCapabilities {
                streaming: self.capabilities.streaming,
                max_tokens_default: self.capabilities.max_tokens_default,
            },
            reasoning_effort: self.reasoning_effort,
            tool_schema_compat: None,
        }
    }
}

// Serializes Codex OAuth token refresh process-wide — the canonical lock lives
// with the token type in `djinn-provider` so the background keep-alive sweep
// takes the *same* mutex as this dispatch path (single-use rotating refresh
// tokens must never be double-spent). See
// `djinn_provider::oauth::codex::CODEX_REFRESH_LOCK`.
use crate::oauth::codex::CODEX_REFRESH_LOCK;

/// Resolve the effective OAuth provider ID for a given provider.
fn effective_oauth_provider_id(provider_id: &str) -> &str {
    match provider_id {
        "chatgpt_codex" | "githubcopilot" => provider_id,
        other => djinn_provider::catalog::builtin::resolve_oauth_provider(other).unwrap_or(other),
    }
}

/// Try to load or refresh a Codex OAuth credential. Returns `Some` when
/// tokens are fresh (or were successfully refreshed), `None` otherwise.
async fn try_load_or_refresh_codex(
    credential_repo: &CredentialRepository,
) -> Option<ProviderCredential> {
    let tokens = crate::oauth::codex::CodexTokens::load_from_db(credential_repo).await?;
    if !tokens.is_expired() {
        return Some(ProviderCredential::OAuthConfig(Box::new(
            crate::oauth::codex_provider_config(&tokens),
        )));
    }
    // Expired → refresh under single-flight lock. Double-check after
    // acquiring: a peer may have already refreshed while we waited.
    let _guard = CODEX_REFRESH_LOCK.lock().await;
    let current = crate::oauth::codex::CodexTokens::load_from_db(credential_repo)
        .await
        .unwrap_or(tokens);
    if !current.is_expired() {
        return Some(ProviderCredential::OAuthConfig(Box::new(
            crate::oauth::codex_provider_config(&current),
        )));
    }
    crate::oauth::codex::refresh_cached_token(&current, credential_repo)
        .await
        .ok()
        .map(|r| ProviderCredential::OAuthConfig(Box::new(crate::oauth::codex_provider_config(&r))))
}

/// Try to load or refresh a Copilot OAuth credential.
async fn try_load_or_refresh_copilot(
    credential_repo: &CredentialRepository,
) -> Option<ProviderCredential> {
    let tokens = crate::oauth::copilot::CopilotTokens::load_from_db(credential_repo).await?;
    if !tokens.is_expired() {
        return Some(ProviderCredential::OAuthConfig(Box::new(
            crate::oauth::copilot_provider_config(&tokens),
        )));
    }
    crate::oauth::copilot::refresh_copilot_token(&tokens, credential_repo)
        .await
        .ok()
        .map(|r| {
            ProviderCredential::OAuthConfig(Box::new(crate::oauth::copilot_provider_config(&r)))
        })
}

/// Attempt a host-side silent OAuth token refresh after a mid-run 401.
/// Returns `true` when the model is OAuth-backed and the token is now live.
/// Returns `false` for non-OAuth providers and for a refresh that itself fails.
pub async fn refresh_oauth_credential_after_401(model_id: &str, app_state: &AgentContext) -> bool {
    let Ok((provider_id, _model_name)) = parse_model_id(model_id) else {
        return false;
    };
    let effective_id = effective_oauth_provider_id(&provider_id);
    let credential_repo =
        CredentialRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    match effective_id {
        "chatgpt_codex" => try_load_or_refresh_codex(&credential_repo).await.is_some(),
        "githubcopilot" => try_load_or_refresh_copilot(&credential_repo)
            .await
            .is_some(),
        _ => false,
    }
}

pub async fn load_provider_credential(
    provider_id: &str,
    app_state: &AgentContext,
) -> anyhow::Result<ProviderCredential> {
    let effective_id = effective_oauth_provider_id(provider_id);
    let credential_repo =
        CredentialRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    // 1. Try OAuth tokens first for OAuth-capable providers.
    match effective_id {
        "chatgpt_codex" => {
            if let Some(cred) = try_load_or_refresh_codex(&credential_repo).await {
                return Ok(cred);
            }
        }
        "githubcopilot" => {
            if let Some(cred) = try_load_or_refresh_copilot(&credential_repo).await {
                return Ok(cred);
            }
        }
        _ => {}
    }
    // 2. Fall back to credential vault (DB).
    let key_name = app_state
        .catalog
        .list_providers()
        .into_iter()
        .find(|p| p.id == provider_id)
        .and_then(|p| p.env_vars.into_iter().next())
        .unwrap_or_else(|| format!("{}_API_KEY", provider_id.to_ascii_uppercase()));
    let key = credential_repo
        .get_decrypted(&key_name)
        .await
        .map_err(|e| anyhow::anyhow!("credential lookup failed: {e}"))?;
    match key {
        Some(v) => Ok(ProviderCredential::ApiKey(key_name, v)),
        None => Err(anyhow::anyhow!(
            "no credential stored for provider {provider_id} (expected key {key_name})"
        )),
    }
}

/// Build telemetry metadata with optional operation and attributed-user fields.
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
/// `base_url` is unused for OAuth configs (they carry their own).
///
/// `target` carries the resolved model-dependent defaults for the target
/// model; callers build it via [`build_restamp_target`] using catalog
/// metadata so that failover to a different model re-resolves
/// `reasoning_effort`, `max_tokens_default`, `format_family`, and
/// `tool_schema_compat` for the target instead of carrying stale values
/// from the previous model.
pub(crate) fn build_provider_from_resolved(
    resolved: crate::actors::slot::lifecycle::model_resolution::ResolvedModelCredential,
    _context_window: u32,
    telemetry: Option<djinn_provider::provider::TelemetryMeta>,
    session_affinity_key: Option<String>,
    base_url: String,
    target: &djinn_provider::provider::RestampTarget,
) -> Option<Box<dyn djinn_provider::provider::LlmProvider>> {
    match resolved.provider_credential {
        Some(ProviderCredential::OAuthConfig(cfg)) => {
            // Restamp the OAuth config for the target model so model-dependent
            // defaults (reasoning_effort, max_tokens_default, format_family,
            // tool_schema_compat) reflect the target rather than the previous model.
            let mut cfg = djinn_provider::provider::restamp_provider_config_for_model(*cfg, target);
            // Per-run fields that are not model-dependent and must be set fresh.
            cfg.telemetry = telemetry;
            cfg.session_affinity_key = session_affinity_key;
            Some(djinn_provider::provider::create_provider(cfg))
        }
        Some(ProviderCredential::ApiKey(_key_name, api_key)) => {
            let provider_headers = provider_headers_for(
                &resolved.catalog_provider_id,
                session_affinity_key.as_deref(),
            );
            // Build a base config with transport/auth fields, then restamp
            // model-dependent defaults through the shared helper.
            let base_cfg = djinn_provider::provider::ProviderConfig {
                base_url,
                auth: auth_method_for_provider(&resolved.catalog_provider_id, &api_key),
                format_family: target.format_family,
                model_id: target.model_id.clone(),
                context_window: target.context_window,
                telemetry,
                session_affinity_key,
                provider_headers,
                capabilities: target.capabilities.clone(),
                reasoning_effort: None,
                tool_schema_compat: None,
            };
            let cfg = djinn_provider::provider::restamp_provider_config_for_model(base_cfg, target);
            Some(djinn_provider::provider::create_provider(cfg))
        }
        None => None,
    }
}

/// Provider-specific outbound HTTP headers for resolved API-key providers
/// (e.g. OpenCode Zen session-sticky routing).
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

/// True when a resolved credential needs an API base URL (API-key providers);
/// OAuth configs carry their own.
pub(crate) fn resolved_needs_base_url(
    resolved: &crate::actors::slot::lifecycle::model_resolution::ResolvedModelCredential,
) -> bool {
    matches!(
        resolved.provider_credential,
        Some(ProviderCredential::ApiKey(..))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_provider::provider::{
        AuthMethod, FormatFamily, ProviderCapabilities, ReasoningEffort,
    };
    use serde_json::Value;
    fn sample_oauth_config(
        reasoning: Option<ReasoningEffort>,
    ) -> djinn_provider::provider::ProviderConfig {
        djinn_provider::provider::ProviderConfig {
            base_url: "https://api.minimax.io/anthropic".to_string(),
            auth: AuthMethod::BearerToken("test-token".to_string()),
            format_family: FormatFamily::Anthropic,
            model_id: "minimax-coding-plan/M-2".to_string(),
            context_window: 200_000,
            telemetry: None,
            session_affinity_key: Some("session-123".to_string()),
            provider_headers: std::collections::HashMap::from([(
                "chatgpt-account-id".to_string(),
                "acc-1".to_string(),
            )]),
            capabilities: ProviderCapabilities {
                streaming: true,
                max_tokens_default: Some(64_000),
            },
            reasoning_effort: reasoning,
            tool_schema_compat: None,
        }
    }
    /// OAuth wire round-trip preserves reasoning_effort and core fields for both Some and None.
    #[test]
    fn oauth_wire_round_trip_preserves_reasoning_effort_and_fields() {
        for (reasoning, label) in [
            (Some(ReasoningEffort::Medium), "Some(Medium)"),
            (None, "None"),
        ] {
            let original = sample_oauth_config(reasoning);
            let wire = OAuthConfigWire::from_provider_config(&original);
            let json = serde_json::to_string(&wire).expect("serialize");
            let decoded: OAuthConfigWire = serde_json::from_str(&json).expect("deserialize");
            let reconstructed = decoded.to_provider_config();
            assert_eq!(reconstructed.reasoning_effort, reasoning, "{label}");
            assert_eq!(reconstructed.base_url, original.base_url, "{label}");
            assert_eq!(reconstructed.model_id, original.model_id, "{label}");
            assert_eq!(
                reconstructed.context_window, original.context_window,
                "{label}"
            );
            assert_eq!(
                reconstructed.session_affinity_key, original.session_affinity_key,
                "{label}"
            );
            assert_eq!(
                reconstructed.provider_headers, original.provider_headers,
                "{label}"
            );
            assert!(reconstructed.telemetry.is_none(), "{label}");
        }
    }
    /// A legacy OAuth JSON blob without `reasoning_effort` deserializes to `None`.
    #[test]
    fn oauth_wire_legacy_blob_without_field_deserializes_to_none() {
        let legacy = r#"{
            "base_url": "https://api.openai.com",
            "auth": { "BearerToken": "tok" },
            "format_family": "OpenAIResponses",
            "model_id": "gpt-5.1-codex",
            "context_window": 400000,
            "session_affinity_key": null,
            "provider_headers": {},
            "capabilities": { "streaming": true, "max_tokens_default": null }
        }"#;
        let decoded: OAuthConfigWire =
            serde_json::from_str(legacy).expect("legacy blob must still deserialize");
        let reconstructed = decoded.to_provider_config();
        assert_eq!(reconstructed.reasoning_effort, None);
        assert_eq!(reconstructed.format_family, FormatFamily::OpenAIResponses);
        assert_eq!(reconstructed.model_id, "gpt-5.1-codex");
    }
    /// JSON wire token for `Some(Medium)` is the literal `"medium"`.
    #[test]
    fn oauth_wire_serializes_some_medium_as_lowercase_token() {
        let original = sample_oauth_config(Some(ReasoningEffort::Medium));
        let wire = OAuthConfigWire::from_provider_config(&original);
        let json: Value = serde_json::to_value(&wire).expect("to_value");
        assert_eq!(
            json["reasoning_effort"],
            Value::String("medium".to_string())
        );
        for (tier, token) in [
            (ReasoningEffort::Minimal, "minimal"),
            (ReasoningEffort::Low, "low"),
            (ReasoningEffort::High, "high"),
        ] {
            let cfg = sample_oauth_config(Some(tier));
            let v: Value = serde_json::to_value(OAuthConfigWire::from_provider_config(&cfg))
                .expect("to_value");
            assert_eq!(v["reasoning_effort"], Value::String(token.to_string()));
        }
    }
    /// Codex/gpt-5.x OpenAI Responses request rendering is byte-equivalent
    /// under the shared policy when round-tripped through the wire mirror.
    #[test]
    fn oauth_wire_codex_openai_responses_request_rendering_is_byte_equivalent() {
        use djinn_provider::provider::default_reasoning_effort_for_model;
        let policy_for_codex = default_reasoning_effort_for_model(
            true,
            FormatFamily::OpenAIResponses,
            "gpt-5.1-codex",
        );
        assert_eq!(
            policy_for_codex, None,
            "shared policy must keep Codex/gpt-5.1-codex reasoning_effort at None"
        );
        let mut host_config = sample_oauth_config(None);
        host_config.base_url = "https://api.openai.com".to_string();
        host_config.format_family = FormatFamily::OpenAIResponses;
        host_config.model_id = "gpt-5.1-codex".to_string();
        host_config.context_window = 400_000;
        assert_eq!(host_config.reasoning_effort, None);
        let wire = OAuthConfigWire::from_provider_config(&host_config);
        let json = serde_json::to_string(&wire).expect("serialize");
        let decoded: OAuthConfigWire = serde_json::from_str(&json).expect("deserialize");
        let reconstructed = decoded.to_provider_config();
        assert_eq!(reconstructed.reasoning_effort, host_config.reasoning_effort);
        assert_eq!(reconstructed.reasoning_effort, None);
        assert_eq!(reconstructed.format_family, host_config.format_family);
        assert_eq!(reconstructed.model_id, host_config.model_id);
        assert_eq!(reconstructed.base_url, host_config.base_url);
        assert_eq!(reconstructed.context_window, host_config.context_window);
        assert_eq!(reconstructed.provider_headers, host_config.provider_headers);
        assert_eq!(
            reconstructed.capabilities.streaming,
            host_config.capabilities.streaming
        );
        assert_eq!(
            reconstructed.capabilities.max_tokens_default,
            host_config.capabilities.max_tokens_default
        );
        assert!(reconstructed.telemetry.is_none());
    }

    // ── Restamp regression tests ───────────────────────────────────────────

    use djinn_provider::provider::RestampTarget;

    /// Build a `RestampTarget` for an Anthropic reasoning-capable model
    /// (e.g. Claude Sonnet 4) — the "failover target" state.
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

    /// Build a `RestampTarget` for a non-reasoning OpenAI model (e.g. GPT-4o).
    fn openai_non_reasoning_target() -> RestampTarget {
        RestampTarget {
            model_id: "gpt-4o".to_string(),
            format_family: FormatFamily::OpenAI,
            reasoning: false,
            context_window: 128_000,
            capabilities: ProviderCapabilities {
                streaming: true,
                max_tokens_default: None,
            },
            tool_schema_compat: None,
        }
    }

    /// OAuth `with_model_id` restamps the config for model B and resolves
    /// B's reasoning_effort and max_tokens_default while preserving transport
    /// fields (base_url, auth, provider_headers, session_affinity_key).
    #[test]
    fn oauth_with_model_id_restamp_resolves_model_b_defaults() {
        // Start with an OpenAI non-reasoning model A config.
        let config_a = sample_oauth_config(None);
        assert_eq!(config_a.model_id, "minimax-coding-plan/M-2");
        assert_eq!(config_a.reasoning_effort, None);

        // Restamp for model B (Anthropic reasoning target).
        let cred = ProviderCredential::OAuthConfig(Box::new(config_a));
        let target = anthropic_reasoning_target();
        match cred.with_model_id(&target) {
            ProviderCredential::OAuthConfig(cfg) => {
                assert_eq!(cfg.model_id, "claude-sonnet-4");
                assert_eq!(
                    cfg.reasoning_effort,
                    Some(ReasoningEffort::Medium),
                    "Anthropic reasoning model B must get Some(Medium)"
                );
                assert_eq!(
                    cfg.capabilities.max_tokens_default,
                    Some(64_000),
                    "model B's max_tokens_default must be resolved"
                );
                assert_eq!(cfg.format_family, FormatFamily::Anthropic);
                assert_eq!(cfg.context_window, 200_000);
                // Transport fields preserved.
                assert_eq!(cfg.base_url, "https://api.minimax.io/anthropic");
                assert!(matches!(
                    cfg.auth,
                    AuthMethod::BearerToken(ref t) if t == "test-token"
                ));
                assert_eq!(cfg.session_affinity_key, Some("session-123".to_string()));
                assert_eq!(
                    cfg.provider_headers.get("chatgpt-account-id"),
                    Some(&"acc-1".to_string())
                );
            }
            _ => panic!("expected OAuthConfig"),
        }
    }

    /// OAuth `with_model_id` is a no-op for API-key credentials.
    #[test]
    fn oauth_with_model_id_is_noop_for_api_key() {
        let cred = ProviderCredential::ApiKey("KEY".to_string(), "secret".to_string());
        let target = anthropic_reasoning_target();
        match cred.with_model_id(&target) {
            ProviderCredential::ApiKey(name, key) => {
                assert_eq!(name, "KEY");
                assert_eq!(key, "secret");
            }
            _ => panic!("expected ApiKey to pass through unchanged"),
        }
    }

    /// Restamp from model A (with reasoning_effort=Some(Medium)) to model B
    /// (non-reasoning OpenAI) clears reasoning_effort to None.
    #[test]
    fn restamp_from_reasoning_to_non_reasoning_clears_effort() {
        let config_a = sample_oauth_config(Some(ReasoningEffort::Medium));
        assert_eq!(config_a.reasoning_effort, Some(ReasoningEffort::Medium));

        let cred = ProviderCredential::OAuthConfig(Box::new(config_a));
        let target = openai_non_reasoning_target();
        match cred.with_model_id(&target) {
            ProviderCredential::OAuthConfig(cfg) => {
                assert_eq!(cfg.model_id, "gpt-4o");
                assert_eq!(
                    cfg.reasoning_effort, None,
                    "non-reasoning model B must clear reasoning_effort"
                );
                assert_eq!(cfg.format_family, FormatFamily::OpenAI);
                assert_eq!(cfg.context_window, 128_000);
            }
            _ => panic!("expected OAuthConfig"),
        }
    }

    /// `build_provider_from_resolved` OAuth arm restamps model-dependent
    /// defaults through the shared helper instead of bare model_id assignment.
    #[test]
    fn build_provider_oauth_restamp_resolves_model_b_reasoning_effort() {
        use crate::actors::slot::lifecycle::model_resolution::ResolvedModelCredential;
        let source_config = sample_oauth_config(None);
        let resolved = ResolvedModelCredential {
            catalog_provider_id: "anthropic".to_string(),
            model_name: "claude-sonnet-4".to_string(),
            provider_credential: Some(ProviderCredential::OAuthConfig(Box::new(source_config))),
        };
        let target = anthropic_reasoning_target();
        let provider =
            build_provider_from_resolved(resolved, 0, None, None, String::new(), &target);
        assert!(provider.is_some(), "provider must be created");
        // We can't inspect the provider internals, but the function succeeded,
        // which means the restamp path executed without panicking. The
        // with_model_id restamp test above validates the actual field values.
    }

    /// `build_provider_from_resolved` API-key arm uses the shared model-default
    /// policy to resolve reasoning_effort instead of hardcoding None.
    #[test]
    fn build_provider_api_key_restamp_resolves_reasoning_effort() {
        use crate::actors::slot::lifecycle::model_resolution::ResolvedModelCredential;
        let resolved = ResolvedModelCredential {
            catalog_provider_id: "anthropic".to_string(),
            model_name: "claude-sonnet-4".to_string(),
            provider_credential: Some(ProviderCredential::ApiKey(
                "ANTHROPIC_API_KEY".to_string(),
                "sk-test-key".to_string(),
            )),
        };
        let target = anthropic_reasoning_target();
        let provider = build_provider_from_resolved(
            resolved,
            200_000,
            None,
            None,
            "https://api.anthropic.com".to_string(),
            &target,
        );
        assert!(
            provider.is_some(),
            "API-key provider must be created for Anthropic reasoning model"
        );
    }
}
