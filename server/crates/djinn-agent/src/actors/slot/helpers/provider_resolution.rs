use super::*;

// Pure provider identification helpers live canonically in `djinn-slot`.
pub use djinn_slot::helpers::provider_resolution::{
    auth_method_for_provider, capabilities_for_provider, default_base_url,
    format_family_for_provider, parse_model_id,
};

/// Resolved provider credentials — API key or OAuth-derived `ProviderConfig`.
pub enum ProviderCredential {
    /// Traditional API-key credential (key_name, decrypted key).
    ApiKey(String, String),
    /// OAuth-derived full provider config (base_url, auth, model already set).
    OAuthConfig(Box<djinn_provider::provider::ProviderConfig>),
}

impl ProviderCredential {
    /// Stamp the resolved per-role model onto an OAuth-derived config.
    /// No-op for API-key credentials; the worker stamps those from the
    /// spec's per-role model.
    pub fn with_model_id(mut self, model_id: &str) -> Self {
        if let ProviderCredential::OAuthConfig(cfg) = &mut self {
            cfg.model_id = model_id.to_string();
        }
        self
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
        }
    }
}

/// Serializes codex OAuth token refresh process-wide. Codex/OpenAI rotate the
/// refresh token on every use (single-use), so concurrent dispatches racing an
/// expired token would each POST the SAME refresh_token — the first rotates it
/// and the rest get `invalid_grant`.
static CODEX_REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
pub(crate) fn build_provider_from_resolved(
    resolved: crate::actors::slot::lifecycle::model_resolution::ResolvedModelCredential,
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
                },
            ))
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

// ─── Tests ───────────────────────────────────────────────────────────────────

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
        }
    }

    /// `Some(ReasoningEffort::Medium)` survives the `OAuthConfigWire`
    /// host→Secret→worker round-trip exactly.
    #[test]
    fn oauth_wire_round_trip_preserves_some_medium() {
        let original = sample_oauth_config(Some(ReasoningEffort::Medium));
        let wire = OAuthConfigWire::from_provider_config(&original);
        let json = serde_json::to_string(&wire).expect("serialize");
        let decoded: OAuthConfigWire = serde_json::from_str(&json).expect("deserialize");
        let reconstructed = decoded.to_provider_config();

        assert_eq!(
            reconstructed.reasoning_effort,
            Some(ReasoningEffort::Medium)
        );
        assert_eq!(reconstructed.base_url, original.base_url);
        assert_eq!(reconstructed.model_id, original.model_id);
        assert_eq!(reconstructed.context_window, original.context_window);
        assert_eq!(
            reconstructed.session_affinity_key,
            original.session_affinity_key
        );
        assert_eq!(reconstructed.provider_headers, original.provider_headers);
        assert!(reconstructed.telemetry.is_none());
    }

    /// A host-resolved `None` (e.g. Codex/OpenAI Responses) round-trips cleanly.
    #[test]
    fn oauth_wire_round_trip_preserves_none() {
        let original = sample_oauth_config(None);
        let wire = OAuthConfigWire::from_provider_config(&original);
        let json = serde_json::to_string(&wire).expect("serialize");
        let decoded: OAuthConfigWire = serde_json::from_str(&json).expect("deserialize");
        let reconstructed = decoded.to_provider_config();

        assert_eq!(reconstructed.reasoning_effort, None);
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
}
