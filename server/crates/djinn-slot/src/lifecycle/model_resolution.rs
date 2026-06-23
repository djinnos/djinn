//! Model + credential resolution for the task lifecycle.
use crate::helpers::{ProviderCredential, load_provider_credential, parse_model_id};
use crate::host::SlotContext;

/// Resolved model + credential for a dispatch.
pub(crate) struct ResolvedModelCredential {
    pub provider_id: String,
    pub model_name: String,
    pub credential: ProviderCredential,
}

/// Resolve the model and credential for a role's preferred model.
pub(crate) async fn resolve_role_model_preference(
    model_id: &str,
    ctx: &SlotContext,
) -> Result<ResolvedModelCredential, String> {
    let (provider_id, model_name) = parse_model_id(model_id)?;
    let credential = load_provider_credential(&provider_id, ctx).await?;
    Ok(ResolvedModelCredential {
        provider_id,
        model_name,
        credential,
    })
}
