//! Model-ID parsing + provider credential lookup for the task lifecycle.

use crate::actors::slot::helpers::{ProviderCredential, load_provider_credential, parse_model_id};
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
pub(crate) struct ModelResolutionError {
    pub reason: String,
}

/// Parse `model_id`, resolve against the provider catalog, and load the credential.
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
/// Returns `None` when no matching preference is found or on lookup errors.
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
    // Scope to the acting user (the task creator — supervisor_runner runs this
    // under `SESSION_USER_ID.scope(created_by_user_id)`, same as
    // `load_provider_credential`), so preference-matching sees exactly the
    // providers this task can authenticate, never another user's private creds.
    let credentials = match cred_repo
        .list_for_user(djinn_core::auth_context::current_user_id().as_deref())
        .await
    {
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

pub(crate) fn map_resume_selection_reason_to_rotation_cause(
    reason: Option<djinn_runtime::ResumeSelectionReason>,
) -> Option<RotationTerminationCause> {
    use djinn_runtime::ResumeSelectionReason as R;
    match reason {
        Some(R::LatestSafeCheckpoint) => Some(RotationTerminationCause::NoProgress),
        Some(R::AutoSubmitAccepted) => Some(RotationTerminationCause::Deadline),
        Some(R::AlternateCheckpointRef) => Some(RotationTerminationCause::Flaky),
        Some(R::NewerTaskBranch) => Some(RotationTerminationCause::RepeatedVerifyLoop),
        _ => None,
    }
}

/// Attempt model rotation for a resume dispatch.
/// Returns the rotated model ID, or `current_model_id` unchanged when rotation
/// is not applicable.
pub(crate) async fn attempt_resume_model_rotation(
    task_id: &str,
    current_model_id: &str,
    metadata: Option<&djinn_runtime::ResumeLifecycleMetadata>,
    app_state: &AgentContext,
) -> String {
    let Some(metadata) = metadata else {
        return current_model_id.to_string();
    };

    let prev_model = match &metadata.previous_model {
        Some(m) if !m.trim().is_empty() => m.as_str(),
        _ => return current_model_id.to_string(),
    };

    let cause = match map_resume_selection_reason_to_rotation_cause(metadata.selection_reason) {
        Some(cause) => cause,
        _ => return current_model_id.to_string(),
    };

    let outcome = resolve_model_with_rotation(task_id, Some(prev_model), cause, app_state).await;

    // Use selected_model() to extract the model id from the outcome before
    // the match consumes it.
    let selected = outcome.selected_model().map(str::to_string);

    match outcome {
        ModelRotationOutcome::Rotated { cause, .. } => {
            let model = selected.unwrap_or_else(|| current_model_id.to_string());
            tracing::info!(
                task_id = %task_id,
                previous_model = %prev_model,
                selected_model = %model,
                cause = ?cause,
                "model_rotation: rotated for resume dispatch"
            );
            model
        }
        ModelRotationOutcome::Fallback { reason, cause, .. } => {
            tracing::info!(
                task_id = %task_id,
                previous_model = %prev_model,
                reason = ?reason,
                cause = ?cause,
                "model_rotation: no alternate available for resume, retaining current model"
            );
            current_model_id.to_string()
        }
        ModelRotationOutcome::NotApplicable => current_model_id.to_string(),
    }
}

// ── Model rotation (y8pv / 48ru) ──────────────────────────────────────────

/// Termination causes that should trigger model rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RotationTerminationCause {
    /// No durable progress for the configured threshold.
    NoProgress,
    /// Session hit a deadline/turn/time bound.
    Deadline,
    /// Repeated flaky verification command failures.
    Flaky,
    /// Repeated verify-loop (command passed then failed or vice versa).
    RepeatedVerifyLoop,
}

impl RotationTerminationCause {
    /// Whether this cause warrants preferring a different model.
    pub fn should_rotate(self) -> bool {
        matches!(
            self,
            Self::NoProgress | Self::Deadline | Self::Flaky | Self::RepeatedVerifyLoop
        )
    }
}

/// Outcome of a model-rotation attempt.
#[derive(Debug, Clone)]
pub(crate) enum ModelRotationOutcome {
    /// A different model was selected.
    Rotated {
        previous_model: String,
        selected_model: String,
        cause: RotationTerminationCause,
    },
    /// No alternate available; existing model retained.
    Fallback {
        previous_model: String,
        reason: ModelRotationFallbackReason,
        cause: RotationTerminationCause,
    },
    /// Rotation not applicable.
    NotApplicable,
}

/// Machine-readable reason model rotation fell back to the existing model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelRotationFallbackReason {
    /// No other connected provider had any models.
    NoAlternateAvailable,
    /// The only alternate models lacked valid credentials.
    CredentialsMissing,
    /// No previous model was recorded to rotate away from.
    NoPreviousModel,
}

impl ModelRotationOutcome {
    /// The selected model id, regardless of whether rotation succeeded
    /// or fell back. Returns `None` for `NotApplicable`.
    pub(crate) fn selected_model(&self) -> Option<&str> {
        match self {
            Self::Rotated { selected_model, .. } => Some(selected_model),
            Self::Fallback { previous_model, .. } => Some(previous_model),
            Self::NotApplicable => None,
        }
    }
}

/// Select a different model after a rotation-worthy termination.
/// Returns a [`ModelRotationOutcome`] and emits a lifecycle-step event.
pub(crate) async fn resolve_model_with_rotation(
    task_id: &str,
    previous_model: Option<&str>,
    cause: RotationTerminationCause,
    app_state: &AgentContext,
) -> ModelRotationOutcome {
    let Some(previous_model) = previous_model else {
        let outcome = ModelRotationOutcome::Fallback {
            previous_model: String::new(),
            reason: ModelRotationFallbackReason::NoPreviousModel,
            cause,
        };
        emit_rotation_event(task_id, &outcome, app_state).await;
        return outcome;
    };

    if !cause.should_rotate() {
        let outcome = ModelRotationOutcome::NotApplicable;
        emit_rotation_event(task_id, &outcome, app_state).await;
        return outcome;
    }

    // Scan connected providers for an alternate model. This mirrors the
    // credential-scoped catalog scan in `resolve_role_model_preference`
    // but does NOT require a role preference — it picks any available
    // model that is NOT the previous one.
    let cred_repo = djinn_provider::repos::CredentialRepository::new(
        app_state.db.clone(),
        app_state.event_bus.clone(),
    );
    let credentials = match cred_repo
        .list_for_user(djinn_core::auth_context::current_user_id().as_deref())
        .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                error = %e,
                "model_rotation: failed to list credentials; falling back"
            );
            let outcome = ModelRotationOutcome::Fallback {
                previous_model: previous_model.to_string(),
                reason: ModelRotationFallbackReason::CredentialsMissing,
                cause,
            };
            emit_rotation_event(task_id, &outcome, app_state).await;
            return outcome;
        }
    };

    let provider_ids = app_state.catalog.connected_provider_ids(&credentials);
    if provider_ids.is_empty() {
        let outcome = ModelRotationOutcome::Fallback {
            previous_model: previous_model.to_string(),
            reason: ModelRotationFallbackReason::CredentialsMissing,
            cause,
        };
        emit_rotation_event(task_id, &outcome, app_state).await;
        return outcome;
    }

    // Find the first model that is NOT the previous model.
    let mut found_alternate: Option<String> = None;
    let mut any_model_seen = false;
    for provider_id in &provider_ids {
        for model in app_state.catalog.list_models(provider_id) {
            let full_id = format!("{provider_id}/{}", model.id);
            if full_id == previous_model
                || model.id == previous_model
                || model.name == previous_model
            {
                continue;
            }
            // Verify this provider has a loadable credential before selecting it.
            if load_provider_credential(provider_id, app_state)
                .await
                .is_ok()
            {
                found_alternate = Some(full_id);
                break;
            }
            any_model_seen = true;
        }
        if found_alternate.is_some() {
            break;
        }
    }

    match found_alternate {
        Some(selected_model) => {
            let outcome = ModelRotationOutcome::Rotated {
                previous_model: previous_model.to_string(),
                selected_model: selected_model.clone(),
                cause,
            };
            tracing::info!(
                task_id = %task_id,
                previous_model = %previous_model,
                selected_model = %selected_model,
                cause = ?cause,
                "model_rotation: rotated to alternate model"
            );
            emit_rotation_event(task_id, &outcome, app_state).await;
            outcome
        }
        None => {
            let reason = if any_model_seen {
                ModelRotationFallbackReason::CredentialsMissing
            } else {
                ModelRotationFallbackReason::NoAlternateAvailable
            };
            let outcome = ModelRotationOutcome::Fallback {
                previous_model: previous_model.to_string(),
                reason,
                cause,
            };
            tracing::info!(
                task_id = %task_id,
                previous_model = %previous_model,
                reason = ?reason,
                cause = ?cause,
                "model_rotation: no alternate available, falling back"
            );
            emit_rotation_event(task_id, &outcome, app_state).await;
            outcome
        }
    }
}

/// Emit a `model_rotation` task-lifecycle-step event.
async fn emit_rotation_event(
    task_id: &str,
    outcome: &ModelRotationOutcome,
    app_state: &AgentContext,
) {
    let payload = match outcome {
        ModelRotationOutcome::Rotated {
            previous_model,
            selected_model,
            cause,
        } => serde_json::json!({
            "action": "rotated",
            "previous_model": previous_model,
            "selected_model": selected_model,
            "termination_cause": format!("{cause:?}"),
        }),
        ModelRotationOutcome::Fallback {
            previous_model,
            reason,
            cause,
        } => serde_json::json!({
            "action": "fallback",
            "previous_model": previous_model,
            "fallback_reason": format!("{reason:?}"),
            "termination_cause": format!("{cause:?}"),
        }),
        ModelRotationOutcome::NotApplicable => serde_json::json!({
            "action": "not_applicable",
        }),
    };

    app_state
        .event_bus
        .send(djinn_core::events::DjinnEventEnvelope::task_lifecycle_step(
            task_id,
            "model_rotation",
            &payload,
        ));
}

#[cfg(test)]
mod rotation_tests {
    use super::*;

    #[test]
    fn maps_resume_reason_to_rotation_cause() {
        use djinn_runtime::ResumeSelectionReason as R;
        assert_eq!(
            map_resume_selection_reason_to_rotation_cause(Some(R::LatestSafeCheckpoint)),
            Some(RotationTerminationCause::NoProgress)
        );
        assert_eq!(
            map_resume_selection_reason_to_rotation_cause(Some(R::AutoSubmitAccepted)),
            Some(RotationTerminationCause::Deadline)
        );
        assert_eq!(
            map_resume_selection_reason_to_rotation_cause(Some(R::AlternateCheckpointRef)),
            Some(RotationTerminationCause::Flaky)
        );
        assert_eq!(
            map_resume_selection_reason_to_rotation_cause(Some(R::NewerTaskBranch)),
            Some(RotationTerminationCause::RepeatedVerifyLoop)
        );
        assert_eq!(map_resume_selection_reason_to_rotation_cause(None), None);
        assert_eq!(
            map_resume_selection_reason_to_rotation_cause(Some(R::CleanTaskBranchFallback)),
            None
        );
    }

    #[test]
    fn rotation_cause_should_rotate() {
        assert!(RotationTerminationCause::NoProgress.should_rotate());
        assert!(RotationTerminationCause::Deadline.should_rotate());
        assert!(RotationTerminationCause::Flaky.should_rotate());
        assert!(RotationTerminationCause::RepeatedVerifyLoop.should_rotate());
    }

    #[test]
    fn outcome_selected_model_returns_correct_value() {
        let rotated = ModelRotationOutcome::Rotated {
            previous_model: "a/old".to_string(),
            selected_model: "b/new".to_string(),
            cause: RotationTerminationCause::NoProgress,
        };
        assert_eq!(rotated.selected_model(), Some("b/new"));

        let fallback = ModelRotationOutcome::Fallback {
            previous_model: "a/old".to_string(),
            reason: ModelRotationFallbackReason::NoAlternateAvailable,
            cause: RotationTerminationCause::NoProgress,
        };
        assert_eq!(fallback.selected_model(), Some("a/old"));

        assert_eq!(ModelRotationOutcome::NotApplicable.selected_model(), None);
    }

    #[test]
    fn fallback_reason_distinct() {
        assert_ne!(
            ModelRotationFallbackReason::NoAlternateAvailable,
            ModelRotationFallbackReason::CredentialsMissing
        );
        assert_ne!(
            ModelRotationFallbackReason::CredentialsMissing,
            ModelRotationFallbackReason::NoPreviousModel
        );
    }
}
