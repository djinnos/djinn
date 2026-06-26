//! Model + credential resolution for the task lifecycle.
use crate::helpers::{ProviderCredential, load_provider_credential, parse_model_id};
use crate::host::SlotContext;

/// Resolved model + credential for a dispatch.
pub(crate) struct ResolvedModelCredential {
    pub catalog_provider_id: String,
    pub model_name: String,
    pub provider_credential: Option<ProviderCredential>,
}

/// Failure from [`resolve_model_and_credential`].
pub(crate) struct ModelResolutionError {
    pub reason: String,
}

impl std::fmt::Display for ModelResolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reason)
    }
}

/// Parse the requested `model_id`, resolve it against the provider catalog,
/// then load the associated credential.
pub(crate) async fn resolve_model_and_credential(
    model_id: &str,
    task_id: &str,
    ctx: &SlotContext,
) -> Result<ResolvedModelCredential, ModelResolutionError> {
    let (cpid, mname) = match parse_model_id(model_id) {
        Ok((provider_id, name)) => {
            let resolved = ctx
                .catalog
                .list_models(&provider_id)
                .iter()
                .find(|m| {
                    let bare = m.id.rsplit('/').next().unwrap_or(&m.id);
                    m.id == name || m.name == name || bare == name
                })
                .map(|m| m.id.clone())
                .unwrap_or(name);
            (provider_id, resolved)
        }
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: invalid model ID");
            return Err(ModelResolutionError {
                reason: e.to_string(),
            });
        }
    };
    ctx.event_bus
        .send(djinn_core::events::DjinnEventEnvelope::task_lifecycle_step(
            task_id,
            "credential_loading",
            &serde_json::json!({"provider_id": cpid}),
        ));
    let cred = match load_provider_credential(&cpid, ctx).await {
        Ok(cred) => cred,
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: missing credential");
            return Err(ModelResolutionError {
                reason: e.to_string(),
            });
        }
    };
    Ok(ResolvedModelCredential {
        catalog_provider_id: cpid,
        model_name: mname,
        provider_credential: Some(cred),
    })
}

/// Resolve the model and credential for a role's preferred model.
pub(crate) async fn resolve_role_model_preference(
    model_id: &str,
    ctx: &SlotContext,
) -> Result<ResolvedModelCredential, String> {
    let (provider_id, model_name) = parse_model_id(model_id).map_err(|e| e.to_string())?;
    let credential = load_provider_credential(&provider_id, ctx)
        .await
        .map_err(|e| e.to_string())?;
    Ok(ResolvedModelCredential {
        catalog_provider_id: provider_id,
        model_name,
        provider_credential: Some(credential),
    })
}
