use super::*;

pub fn format_family_for_provider(
    provider_id: &str,
    model_id: &str,
) -> djinn_provider::provider::FormatFamily {
    use djinn_provider::provider::FormatFamily;
    let lower = provider_id.to_lowercase();
    // MiniMax (incl. the coding-plan providers) only exposes an
    // Anthropic-compatible endpoint (`https://api.minimax.io/anthropic/v1`).
    // Xiaomi MiMo (`xiaomi-mimo`) likewise routes through its
    // Anthropic-compatible Token Plan endpoint; `mimo` catches the provider id.
    // Kimi Coding Plan (`kimi-coding-plan`) speaks the Anthropic wire format via
    // the Kimi Code subscription endpoint; `kimi` catches the provider id.
    if lower.contains("anthropic")
        || lower.contains("minimax")
        || lower.contains("mimo")
        || lower.contains("kimi")
    {
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

/// Returns true for OpenAI models that support the Responses API (GPT-5.x,
/// o-series reasoning models).  These get better quality and reasoning
/// summaries when routed through `/responses` instead of `/chat/completions`.
fn is_openai_responses_model(model_id: &str) -> bool {
    let lower = model_id.to_lowercase();
    lower.starts_with("gpt-5")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
}

/// Returns true for provider IDs that point to OpenAI's own API (not
/// third-party OpenAI-compatible endpoints like Fireworks, Together, etc.).
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
    } else if lower.contains("anthropic") || lower.contains("mimo") || lower.contains("kimi") {
        // Xiaomi MiMo (`xiaomi-mimo`) and Kimi Coding Plan (`kimi-coding-plan`)
        // speak the Anthropic wire format, which requires a default
        // `max_tokens`; mirror the Anthropic caps.
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
    /// Convert into a wire-friendly [`djinn_runtime::SerializableCredential`]
    /// suitable for shipping into a worker Pod via the per-task-run K8s
    /// Secret (Phase 7a).
    ///
    /// API-key credentials pass through unchanged. OAuth-derived
    /// [`djinn_provider::provider::ProviderConfig`] payloads are projected
    /// onto an [`OAuthConfigWire`] serde mirror and JSON-encoded — the
    /// upstream `ProviderConfig` (and its `AuthMethod` /
    /// `ProviderCapabilities` constituents) deliberately do not implement
    /// `Serialize`, so the wire mirror is the load-bearing seam keeping
    /// `djinn-provider` free of a serde-everywhere derive sprawl.
    /// Stamp the resolved per-role model onto an OAuth-derived config.
    ///
    /// OAuth provider configs (codex, copilot) are built with a hardcoded
    /// provider-default model (e.g. `CODEX_DEFAULT_MODEL = "gpt-5.1-codex"`).
    /// On the live (server) stage path that gets overridden by
    /// `resolved.model_name` (stage.rs), but the worker runs the dispatched
    /// credential snapshot directly as `provider_override`, so that override
    /// never runs there. Without this, every codex/copilot worker run ignores
    /// the user's configured model and sends the provider default — which a
    /// ChatGPT-account Codex token rejects ("model not supported …"). Apply it
    /// here so the snapshot carries the user's model. No-op for API-key creds
    /// (the worker stamps those from the spec's per-role model).
    pub fn with_model_id(mut self, model_id: &str) -> Self {
        if let ProviderCredential::OAuthConfig(cfg) = &mut self {
            cfg.model_id = model_id.to_string();
        }
        self
    }

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
/// exclusively to encode the OAuth-derived variant of [`ProviderCredential`]
/// into a wire blob (Phase 7a).
///
/// The worker (Phase 7b) will deserialise this and reconstruct a live
/// `ProviderConfig` — the two-way translation lives outside this file because
/// the worker links a different subset of crates than the coordinator.
///
/// `reasoning_effort` mirrors the host-resolved `ProviderConfig.reasoning_effort`
/// so the capability-driven policy (epic 160n / 5dej) survives the Secret JSON
/// round-trip on OAuth paths. The field is `#[serde(default)]` so legacy blobs
/// shipped before this field existed still decode to `None` and reconstruct a
/// `ProviderConfig` with reasoning suppressed — exactly the pre-policy shape.
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
    /// Optional reasoning-effort tier resolved by the host-side capability
    /// policy (`default_reasoning_effort_for_model`). Reuses the upstream
    /// `ReasoningEffort` enum (already serde-friendly, `rename_all =
    /// "lowercase"`) so wire tokens match the rest of the provider pipeline.
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
            // `telemetry` is intentionally dropped — it is per-call metadata
            // (task_id / agent_type / session_id) the worker rebuilds locally.
            session_affinity_key: cfg.session_affinity_key.clone(),
            provider_headers: cfg.provider_headers.clone(),
            capabilities: OAuthCapabilitiesWire {
                streaming: cfg.capabilities.streaming,
                max_tokens_default: cfg.capabilities.max_tokens_default,
            },
            // Round-trip the host-resolved reasoning-effort tier (epic 160n
            // wire preservation). For Anthropic-format OAuth paths this is
            // the `Some(Medium)` the capability policy selected; for Codex /
            // OpenAI Responses it remains `None` per the shared policy.
            reasoning_effort: cfg.reasoning_effort,
        }
    }

    /// Inverse of [`OAuthConfigWire::from_provider_config`]: reconstitute a
    /// live [`djinn_provider::provider::ProviderConfig`] from the wire mirror.
    /// Used by `djinn-agent-worker` (Phase 7b) to rebuild an OAuth-derived
    /// provider config that was shipped over the Secret mount as opaque JSON.
    ///
    /// `telemetry` and `session_affinity_key` are left at the wire-encoded
    /// values; the worker overrides them per-stage before constructing the
    /// concrete provider client.
    ///
    /// `reasoning_effort` is reconstructed from the wire-encoded value (epic
    /// 160n wire preservation). For legacy JSON blobs that predate the field,
    /// `#[serde(default)]` decodes it as `None` and the reconstructed config
    /// preserves the pre-policy `None` behavior — a safe no-op for older
    /// workers / hosts.
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
/// refresh token on every use (single-use), so concurrent task-run dispatches
/// hitting an expired token would each POST the SAME refresh_token — the first
/// rotates it and the rest get `invalid_grant`; OpenAI then invalidates the
/// whole token family on reuse, poisoning the credential until a manual
/// reconnect. Holding this lock across [reload → check → refresh → save] makes
/// losers reuse the winner's freshly-saved token instead of racing a second
/// refresh. (Process-local: correct for the single-replica VPS; on a
/// multi-replica deploy a Postgres advisory lock would close the cross-replica
/// window — tracked as a follow-up.)
static CODEX_REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub async fn load_provider_credential(
    provider_id: &str,
    app_state: &AgentContext,
) -> anyhow::Result<ProviderCredential> {
    // 1. Try OAuth tokens first for OAuth-capable providers.
    // Also resolve merged children: e.g. "openai" → "chatgpt_codex".
    let effective_oauth_id = match provider_id {
        "chatgpt_codex" | "githubcopilot" => provider_id,
        other => djinn_provider::catalog::builtin::resolve_oauth_provider(other).unwrap_or(other),
    };
    let credential_repo =
        CredentialRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    match effective_oauth_id {
        "chatgpt_codex" => {
            if let Some(tokens) =
                crate::oauth::codex::CodexTokens::load_from_db(&credential_repo).await
            {
                if !tokens.is_expired() {
                    return Ok(ProviderCredential::OAuthConfig(Box::new(
                        crate::oauth::codex_provider_config(&tokens),
                    )));
                }
                // Expired → refresh under the single-flight lock (see
                // CODEX_REFRESH_LOCK) so concurrent dispatches don't race the
                // single-use refresh token. Double-check after acquiring: a
                // peer may have already refreshed while we waited.
                let _guard = CODEX_REFRESH_LOCK.lock().await;
                let current = crate::oauth::codex::CodexTokens::load_from_db(&credential_repo)
                    .await
                    .unwrap_or(tokens);
                if !current.is_expired() {
                    return Ok(ProviderCredential::OAuthConfig(Box::new(
                        crate::oauth::codex_provider_config(&current),
                    )));
                }
                match crate::oauth::codex::refresh_cached_token(&current, &credential_repo).await {
                    Ok(refreshed) => {
                        return Ok(ProviderCredential::OAuthConfig(Box::new(
                            crate::oauth::codex_provider_config(&refreshed),
                        )));
                    }
                    Err(e) => {
                        tracing::warn!(
                            provider = provider_id,
                            error = %e,
                            "OAuth token refresh failed; falling back to credential vault"
                        );
                    }
                }
            }
        }
        "githubcopilot" => {
            if let Some(tokens) =
                crate::oauth::copilot::CopilotTokens::load_from_db(&credential_repo).await
            {
                if !tokens.is_expired() {
                    return Ok(ProviderCredential::OAuthConfig(Box::new(
                        crate::oauth::copilot_provider_config(&tokens),
                    )));
                }
                // Copilot refresh requires the github_token → try exchange.
                match crate::oauth::copilot::refresh_copilot_token(&tokens, &credential_repo).await
                {
                    Ok(refreshed) => {
                        return Ok(ProviderCredential::OAuthConfig(Box::new(
                            crate::oauth::copilot_provider_config(&refreshed),
                        )));
                    }
                    Err(e) => {
                        tracing::warn!(
                            provider = provider_id,
                            error = %e,
                            "Copilot token refresh failed; falling back to credential vault"
                        );
                    }
                }
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

pub fn parse_model_id(model_id: &str) -> anyhow::Result<(String, String)> {
    let Some((provider_id, model_name)) = model_id.split_once('/') else {
        return Err(anyhow::anyhow!(
            "invalid model id '{model_id}', expected provider/model"
        ));
    };
    Ok((provider_id.to_owned(), model_name.to_owned()))
}

/// Build telemetry metadata for OTel span instrumentation.
pub(crate) fn build_telemetry_meta(
    agent_type_str: &str,
    task_id: &str,
) -> djinn_provider::provider::TelemetryMeta {
    build_telemetry_meta_with_attribution(agent_type_str, task_id, None, None)
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

/// Build an [`djinn_provider::provider::LlmProvider`] from a resolved model +
/// credential. Single home for the OAuth-vs-API-key construction shared by the
/// per-stage worker session ([`crate::supervisor_impl::stage`]), host-side
/// `invoke_llm` ([`crate::direct_services`]), and post-session memory
/// extraction ([`crate::actors::slot::llm_extraction`]). Callers differ only in
/// the inputs threaded here — `context_window`, `telemetry`,
/// `session_affinity_key`, and the (async-resolved) provider `base_url` — so
/// those stay caller-supplied. `base_url` is unused for OAuth configs (they
/// carry their own); callers can skip the lookup and pass `String::new()`.
/// Returns `None` when no credential was resolved.
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
            // Computed before the move of `session_affinity_key` into the config.
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

/// Provider-specific outbound HTTP headers applied to every request for a
/// resolved API-key provider.
///
/// OpenCode Zen (`opencode`) identifies its first-party CLI via these headers.
/// The Zen gateway uses them for metrics and session-sticky provider routing —
/// `x-opencode-session` pins a session to one upstream backend — and strips them
/// before forwarding to the real provider; for free models the rate limit is
/// per-IP, not per-header (see opencode `routes/zen/util/handler.ts`). Matching
/// the real CLI's shape keeps us off the anonymous path and gives stable sticky
/// routing. Values mirror `session/llm/request.ts`: client `cli`, User-Agent
/// `opencode/<version>`, and the session id in `x-opencode-session`. Every other
/// provider gets no extra headers, so these only ever reach the Zen endpoint.
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
/// OAuth configs carry their own, so callers can skip the async lookup.
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

    /// 7mhn: `Some(ReasoningEffort::Medium)` survives the
    /// `OAuthConfigWire` host→Secret→worker round-trip exactly.
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
        // `telemetry` is intentionally dropped on the wire (per-call metadata
        // the worker rebuilds locally).
        assert!(reconstructed.telemetry.is_none());
    }

    /// 7mhn: a host-resolved `None` (e.g. Codex/OpenAI Responses) round-trips
    /// cleanly so the policy's "preserve None" outcome is observable.
    #[test]
    fn oauth_wire_round_trip_preserves_none() {
        let original = sample_oauth_config(None);
        let wire = OAuthConfigWire::from_provider_config(&original);
        let json = serde_json::to_string(&wire).expect("serialize");
        let decoded: OAuthConfigWire = serde_json::from_str(&json).expect("deserialize");
        let reconstructed = decoded.to_provider_config();

        assert_eq!(reconstructed.reasoning_effort, None);
    }

    /// 7mhn: a legacy OAuth JSON blob shipped before the
    /// `reasoning_effort` field existed must still deserialize and reconstruct
    /// with `reasoning_effort: None` (the pre-policy safe default). The
    /// `#[serde(default)]` annotation on the wire field is what makes this
    /// possible.
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

    /// 7mhn: the JSON wire token for `Some(Medium)` is the literal `"medium"`
    /// (the upstream enum derives `Serialize`/`Deserialize` with
    /// `rename_all = "lowercase"`), so the policy output is visible in the
    /// Secret blob and easily greppable in `kubectl describe`/`oc get secret`
    /// dumps.
    #[test]
    fn oauth_wire_serializes_some_medium_as_lowercase_token() {
        let original = sample_oauth_config(Some(ReasoningEffort::Medium));
        let wire = OAuthConfigWire::from_provider_config(&original);
        let json: Value = serde_json::to_value(&wire).expect("to_value");
        assert_eq!(
            json["reasoning_effort"],
            Value::String("medium".to_string())
        );

        // The other tier tokens must round-trip with their lowercase spelling.
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

    /// 7mhn acceptance criteria: Codex/gpt-5.x OpenAI Responses request
    /// rendering is unchanged or byte-equivalent under the shared policy.
    ///
    /// The capability-driven policy from 5dej explicitly returns `None` for
    /// `FormatFamily::OpenAIResponses` (including `gpt-5.1-codex`), so a
    /// host-resolved `ProviderConfig` always lands with `reasoning_effort:
    /// None` for Codex. The OpenAI Responses request builder already renders
    /// that `None` as `reasoning.effort = "medium"` (its pre-B5 default), and
    /// the byte-equivalence of the request body across the wire is guarded by
    /// the upstream `test_default_policy_preserves_openai_responses_request_bytes`
    /// test in `djinn-provider`.
    ///
    /// This test is the host-side wire contract: the wire round-trip must
    /// preserve the host's `None` so the worker's reconstructed
    /// `ProviderConfig` is identical to the host's, and the request body
    /// rendered from the reconstructed config matches the host's. We also
    /// assert the shared policy keeps `gpt-5.1-codex` at `None` (so a future
    /// change to the policy cannot silently turn on Responses reasoning for
    /// Codex without breaking this guard).
    #[test]
    fn oauth_wire_codex_openai_responses_request_rendering_is_byte_equivalent() {
        use djinn_provider::provider::default_reasoning_effort_for_model;

        // (a) The shared capability-driven policy must keep Codex at `None`:
        // this is the load-bearing reason the request body is byte-equivalent
        // across the wire. The wire cannot make Codex byte-different from the
        // host's `None` baseline if the wire is the only seam.
        let policy_for_codex = default_reasoning_effort_for_model(
            true,
            FormatFamily::OpenAIResponses,
            "gpt-5.1-codex",
        );
        assert_eq!(
            policy_for_codex, None,
            "shared policy must keep Codex/gpt-5.1-codex reasoning_effort at None; \
             otherwise Codex request bodies would change under the policy"
        );

        // (b) A host-resolved `ProviderConfig` for Codex has `None`
        // (per the policy above). The OAuth wire round-trip must preserve
        // that `None` exactly so the worker's reconstructed config is
        // byte-equivalent to the host's.
        let mut host_config = sample_oauth_config(None);
        host_config.base_url = "https://api.openai.com".to_string();
        host_config.format_family = FormatFamily::OpenAIResponses;
        host_config.model_id = "gpt-5.1-codex".to_string();
        host_config.context_window = 400_000;

        // Sanity: the host config starts at `None` (per the policy).
        assert_eq!(host_config.reasoning_effort, None);

        let wire = OAuthConfigWire::from_provider_config(&host_config);
        let json = serde_json::to_string(&wire).expect("serialize");
        let decoded: OAuthConfigWire = serde_json::from_str(&json).expect("deserialize");
        let reconstructed = decoded.to_provider_config();

        // (c) Byte-equivalence at the wire seam: the reconstructed
        // `ProviderConfig` matches the host's for every wire-surviving field.
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

        // `telemetry` is intentionally dropped on the wire (per-call metadata
        // the worker rebuilds locally); the worker overrides it before
        // constructing the concrete provider client.
        assert!(reconstructed.telemetry.is_none());
    }
}
