//! Model-ID parsing + provider credential lookup for the task lifecycle.
//!
//! This is a pure code-motion extraction from `run_task_lifecycle` (task #14
//! preparatory work). The caller is responsible for reacting to
//! [`ModelResolutionError`] — e.g. task-status transition to interrupted and
//! releasing the slot — so that the extracted function has no knowledge of the
//! surrounding task-run context.

use crate::actors::slot::helpers::{
    ProviderCredential, load_provider_credential, parse_model_id,
};
use crate::context::AgentContext;
use djinn_db::AgentRepository;

/// Resolved catalog/provider identity + credential ready to drive an LLM
/// provider for the upcoming session.
pub(crate) struct ResolvedModelCredential {
    pub catalog_provider_id: String,
    pub model_name: String,
    pub provider_credential: Option<ProviderCredential>,
}

/// Failure from [`resolve_model_and_credential`].
///
/// Carries the human-readable reason string that the caller will thread into
/// the task-status transition (preserving the original error-to-transition
/// semantics of `run_task_lifecycle`).
pub(crate) struct ModelResolutionError {
    pub reason: String,
}

/// Parse the requested `model_id`, resolve it against the provider catalog
/// (allowing display-name / bare-suffix matches), then load the associated
/// credential from the vault.
///
/// Mirrors the byte-for-byte behavior of the former inline block in
/// `run_task_lifecycle`:
///   - emits the `credential_loading` task-lifecycle step event after a
///     successful model-ID parse, before the credential lookup,
///   - returns a [`ModelResolutionError`] with the same reason strings the old
///     block used for task-status transitions on failure (the displayed
///     `anyhow::Error` text),
///   - logs the same `tracing::warn!` lines on failure paths.
///
/// The caller is responsible for all task-status transitions and slot release
/// on error — this function does not touch either.
pub(crate) async fn resolve_model_and_credential(
    model_id: &str,
    task_id: &str,
    app_state: &AgentContext,
) -> Result<ResolvedModelCredential, ModelResolutionError> {
    let (cpid, mname) = match parse_model_id(model_id) {
        Ok((provider_id, name)) => {
            // Settings may store display names (e.g. "GPT-5.3 Codex") or
            // bare suffixes (e.g. "GLM-4.7" for internal "hf:zai-org/GLM-4.7").
            // Resolve to the actual model ID for the provider API.
            let resolved = app_state
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
    app_state
        .event_bus
        .send(djinn_core::events::DjinnEventEnvelope::task_lifecycle_step(
            task_id,
            "credential_loading",
            &serde_json::json!({"provider_id": cpid}),
        ));
    let cred = match load_provider_credential(&cpid, app_state).await {
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

/// Resolve a per-role project preference into a concrete `provider/model` id.
///
/// Reads the default `agents` row for `(project_id, base_role)` (the same
/// shape the coordinator's `resolve_role_model_preference` uses at dispatch
/// time), normalizes the stored `model_preference` string against connected
/// providers' catalogs, and returns the first `provider/model` match.
///
/// Returns `None` when:
///   - no default role is configured for this `(project, base_role)`,
///   - `model_preference` is unset / whitespace,
///   - the preference string cannot be matched to any connected model,
///   - any repository/catalog lookup errors (logged at `warn`).
///
/// Used by `supervisor_runner` to populate `TaskRunSpec::model_id_per_role`
/// per-stage; the caller falls back to the dispatch-resolved default when
/// this returns `None`.
pub(crate) async fn resolve_role_model_preference(
    project_id: &str,
    base_role: &str,
    app_state: &AgentContext,
) -> Option<String> {
    let role_repo = AgentRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let db_role = match role_repo
        .get_default_for_base_role(project_id, base_role)
        .await
    {
        Ok(Some(r)) => r,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(
                project_id,
                base_role,
                error = %e,
                "supervisor_runner: failed to load default role for model_preference"
            );
            return None;
        }
    };

    let preference = match db_role.model_preference.as_deref() {
        Some(p) if !p.trim().is_empty() => p.trim().to_string(),
        _ => return None,
    };

    // Match `preference` (a bare suffix like "claude-opus-4-6", a display
    // name, the full `provider/model` id, or the catalog id) against every
    // connected provider — identical resolution to dispatch's priority path.
    let cred_repo = djinn_provider::repos::CredentialRepository::new(
        app_state.db.clone(),
        app_state.event_bus.clone(),
    );
    let credentials = match cred_repo.list().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                project_id,
                base_role,
                error = %e,
                "supervisor_runner: failed to list credentials for model_preference"
            );
            return None;
        }
    };
    let credential_provider_ids = app_state.catalog.connected_provider_ids(&credentials);
    if credential_provider_ids.is_empty() {
        return None;
    }

    for provider_id in &credential_provider_ids {
        for model in app_state.catalog.list_models(provider_id) {
            let bare = model.id.rsplit('/').next().unwrap_or(&model.id);
            let full_id = format!("{provider_id}/{}", model.id);
            if model.id == preference
                || model.name == preference
                || bare == preference
                || full_id == preference
            {
                return Some(full_id);
            }
        }
    }

    None
}
