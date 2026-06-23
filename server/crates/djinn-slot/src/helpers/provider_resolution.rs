//! Provider credential resolution.
use crate::host::SlotContext;

/// Provider credential resolved from the database.
#[derive(Debug, Clone)]
pub struct ProviderCredential {
    pub provider_id: String,
    pub credential_data: String,
    pub owner: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OAuthConfigWire {
    pub client_id: String,
    pub auth_url: String,
    pub token_url: String,
}

#[derive(Debug, Clone)]
pub struct OAuthCapabilitiesWire {
    pub supports_streaming: bool,
}

#[derive(Debug, Clone)]
pub struct OAuthAuthMethodWire {
    pub method: String,
}

#[derive(Debug, Clone)]
pub struct OAuthFormatFamilyWire {
    pub family: String,
}

/// Parse a `provider/model` model ID string.
pub fn parse_model_id(model_id: &str) -> Result<(String, String), String> {
    let parts: Vec<&str> = model_id.splitn(2, '/').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(format!("invalid model_id format: {model_id}"));
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
}

/// Load a provider credential from the database.
pub async fn load_provider_credential(
    provider_id: &str,
    ctx: &SlotContext,
) -> Result<ProviderCredential, String> {
    // Credential resolution is delegated to the host through callbacks.
    // This stub returns an error; the host provides the real implementation.
    let _ = (provider_id, ctx);
    Err("credential resolution delegated to host callbacks".to_string())
}

pub fn build_provider_from_resolved(
    _provider_id: &str,
    _model_name: &str,
    _credential: &ProviderCredential,
    _ctx: &SlotContext,
) -> Result<Box<dyn djinn_provider::provider::LlmProvider>, String> {
    Err("provider construction delegated to host".to_string())
}

pub fn build_telemetry_meta(
    _provider_id: &str,
    _model_name: &str,
) -> djinn_provider::provider::TelemetryMeta {
    djinn_provider::provider::TelemetryMeta::default()
}

pub fn build_telemetry_meta_with_attribution(
    provider_id: &str,
    model_name: &str,
    _attribution: Option<&str>,
) -> djinn_provider::provider::TelemetryMeta {
    build_telemetry_meta(provider_id, model_name)
}

pub fn default_base_url(_provider_id: &str) -> Option<&'static str> {
    None
}

pub fn resolved_needs_base_url(_provider_id: &str) -> bool {
    false
}

pub fn auth_method_for_provider(_provider_id: &str) -> OAuthAuthMethodWire {
    OAuthAuthMethodWire {
        method: "api_key".to_string(),
    }
}

pub fn capabilities_for_provider(_provider_id: &str) -> OAuthCapabilitiesWire {
    OAuthCapabilitiesWire {
        supports_streaming: true,
    }
}

pub fn format_family_for_provider(_provider_id: &str) -> OAuthFormatFamilyWire {
    OAuthFormatFamilyWire {
        family: "openai".to_string(),
    }
}
