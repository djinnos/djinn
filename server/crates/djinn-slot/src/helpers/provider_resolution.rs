//! Provider credential resolution for the slot crate.
//!
//! The pure provider-identification functions live here. Credential loading
//! (which involves OAuth refresh and is host-specific) delegates to
//! [`SlotHostCallbacks::resolve_provider_credential`].

use crate::host::SlotContext;

// ─── Pure provider identification ────────────────────────────────────────────

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

// ─── Credential types ────────────────────────────────────────────────────────

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

// ─── Wire mirror types ──────────────────────────────────────────────────────

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
            session_affinity_key: cfg.session_affinity_key.clone(),
            provider_headers: cfg.provider_headers.clone(),
            capabilities: OAuthCapabilitiesWire {
                streaming: cfg.capabilities.streaming,
                max_tokens_default: cfg.capabilities.max_tokens_default,
            },
            reasoning_effort: cfg.reasoning_effort,
        }
    }

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

// ─── Credential loading (delegated to host) ──────────────────────────────────

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

// ─── Telemetry ──────────────────────────────────────────────────────────────

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

// ─── Provider construction ──────────────────────────────────────────────────

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

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_provider::provider::{
        AuthMethod, FormatFamily, ProviderCapabilities, ReasoningEffort,
    };

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
    }

    #[test]
    fn oauth_wire_round_trip_preserves_none() {
        let original = sample_oauth_config(None);
        let wire = OAuthConfigWire::from_provider_config(&original);
        let json = serde_json::to_string(&wire).expect("serialize");
        let decoded: OAuthConfigWire = serde_json::from_str(&json).expect("deserialize");
        let reconstructed = decoded.to_provider_config();
        assert_eq!(reconstructed.reasoning_effort, None);
    }

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
    }
}
