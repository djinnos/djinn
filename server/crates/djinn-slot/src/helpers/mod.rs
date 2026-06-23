//! Slot helper functions.

mod code_context;
mod feedback;
pub mod provider_resolution;
mod reviewer_diff;

pub use provider_resolution::{
    OAuthAuthMethodWire, OAuthCapabilitiesWire, OAuthConfigWire, OAuthFormatFamilyWire,
    ProviderCredential, auth_method_for_provider, capabilities_for_provider, default_base_url,
    format_family_for_provider, load_provider_credential, parse_model_id,
};
