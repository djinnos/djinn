use djinn_core::auth_context::current_user_id;
use djinn_core::events::DjinnEventEnvelope;
use djinn_core::models::settings::{DispatchPause, DispatchPauseScope, DispatchPauseState};
use djinn_db::repositories::dispatch_pause::{
    DispatchPauseMutation, DispatchPauseRepository, DispatchPauseTarget,
};
use djinn_db::{ProjectRepository, UserRepository};
use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::server::DjinnMcpServer;
use crate::tools::acting_user::require_admin;

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DispatchPauseParams {
    /// Pause scope: `global`, `project`, or `user`.
    pub scope: DispatchPauseScope,
    /// Required for `project` and `user`; must be omitted for `global`.
    #[serde(default)]
    pub target_id: Option<String>,
    /// Non-empty administrative reason recorded with the pause.
    pub reason: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DispatchResumeParams {
    /// Resume scope: `global`, `project`, or `user`.
    pub scope: DispatchPauseScope,
    /// Required for `project` and `user`; must be omitted for `global`.
    #[serde(default)]
    pub target_id: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct DispatchPauseStatusParams {
    /// Optional scope filter. Omit to return all pause scopes.
    #[serde(default)]
    pub scope: Option<DispatchPauseScope>,
    /// Required when filtering `project` or `user`; invalid for `global`.
    #[serde(default)]
    pub target_id: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct DispatchPauseMutationResponse {
    pub ok: bool,
    pub changed: bool,
    pub scope: DispatchPauseScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<DispatchPause>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<DispatchPause>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct DispatchPauseStatusResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<DispatchPauseScope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<DispatchPause>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<DispatchPauseState>,
    pub error: Option<String>,
}

#[tool_router(router = dispatch_pause_tool_router, vis = "pub")]
impl DjinnMcpServer {
    #[tool(description = "Admin-only: pause dispatch globally, for one project, or for one user.")]
    pub async fn dispatch_pause(
        &self,
        Parameters(p): Parameters<DispatchPauseParams>,
    ) -> Json<DispatchPauseMutationResponse> {
        if let Err(error) = require_admin(self.state.db()).await {
            return mutation_error(p.scope, p.target_id, error);
        }

        let reason = p.reason.trim().to_owned();
        if reason.is_empty() {
            return mutation_error(p.scope, p.target_id, "reason must not be empty".to_string());
        }

        let target = match self.resolve_target(p.scope, p.target_id.as_deref()).await {
            Ok(target) => target,
            Err(error) => return mutation_error(p.scope, p.target_id, error),
        };
        let actor = current_user_id().unwrap_or_else(|| "system".to_string());
        let pause = DispatchPause {
            paused_by: actor,
            paused_at: now_rfc3339(),
            reason,
            expires_at: None,
        };

        let repo = DispatchPauseRepository::new(self.state.db().clone(), self.state.event_bus());
        match repo.pause(target, pause).await {
            Ok(mutation) => {
                emit_if_changed(&self.state.event_bus(), &mutation, None, None);
                mutation_ok(mutation)
            }
            Err(error) => mutation_error(p.scope, p.target_id, error.to_string()),
        }
    }

    #[tool(description = "Admin-only: resume dispatch globally, for one project, or for one user.")]
    pub async fn dispatch_resume(
        &self,
        Parameters(p): Parameters<DispatchResumeParams>,
    ) -> Json<DispatchPauseMutationResponse> {
        if let Err(error) = require_admin(self.state.db()).await {
            return mutation_error(p.scope, p.target_id, error);
        }

        let target = match self.resolve_target(p.scope, p.target_id.as_deref()).await {
            Ok(target) => target,
            Err(error) => return mutation_error(p.scope, p.target_id, error),
        };
        let resumed_by = current_user_id().unwrap_or_else(|| "system".to_string());
        let resumed_at = now_rfc3339();

        let repo = DispatchPauseRepository::new(self.state.db().clone(), self.state.event_bus());
        match repo.resume(target).await {
            Ok(mutation) => {
                emit_if_changed(
                    &self.state.event_bus(),
                    &mutation,
                    Some(resumed_by.as_str()),
                    Some(resumed_at.as_str()),
                );
                mutation_ok(mutation)
            }
            Err(error) => mutation_error(p.scope, p.target_id, error.to_string()),
        }
    }

    #[tool(
        description = "Read-only: return dispatch pause status globally, for one project, for one user, or all scopes.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn dispatch_pause_status(
        &self,
        Parameters(p): Parameters<DispatchPauseStatusParams>,
    ) -> Json<DispatchPauseStatusResponse> {
        let repo = DispatchPauseRepository::new(self.state.db().clone(), self.state.event_bus());
        match p.scope {
            None => {
                if p.target_id
                    .as_deref()
                    .is_some_and(|id| !id.trim().is_empty())
                {
                    return status_error(
                        None,
                        p.target_id,
                        "scope is required when target_id is provided".to_string(),
                    );
                }
                match repo.get_status().await {
                    Ok(state) => Json(DispatchPauseStatusResponse {
                        ok: true,
                        scope: None,
                        target_id: None,
                        current: None,
                        state: Some(state),
                        error: None,
                    }),
                    Err(error) => status_error(None, p.target_id, error.to_string()),
                }
            }
            Some(scope) => {
                let target = match self.resolve_target(scope, p.target_id.as_deref()).await {
                    Ok(target) => target,
                    Err(error) => return status_error(Some(scope), p.target_id, error),
                };
                let target_id = target.target_id().map(str::to_owned);
                match repo.get_scope(target).await {
                    Ok(current) => Json(DispatchPauseStatusResponse {
                        ok: true,
                        scope: Some(scope),
                        target_id,
                        current,
                        state: None,
                        error: None,
                    }),
                    Err(error) => status_error(Some(scope), p.target_id, error.to_string()),
                }
            }
        }
    }

    async fn resolve_target(
        &self,
        scope: DispatchPauseScope,
        target_id: Option<&str>,
    ) -> Result<DispatchPauseTarget, String> {
        match scope {
            DispatchPauseScope::Global => {
                if target_id.is_some_and(|id| !id.trim().is_empty()) {
                    return Err("target_id must be omitted for global scope".to_string());
                }
                Ok(DispatchPauseTarget::global())
            }
            DispatchPauseScope::Project => {
                let project_id = required_target_id(target_id, "project")?;
                let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
                match repo.get(&project_id).await {
                    Ok(Some(_)) => Ok(DispatchPauseTarget::project(project_id)),
                    Ok(None) => Err(format!("project `{project_id}` not found")),
                    Err(error) => Err(format!("project lookup failed: {error}")),
                }
            }
            DispatchPauseScope::User => {
                let user_id = required_target_id(target_id, "user")?;
                let repo = UserRepository::new(self.state.db().clone());
                match repo.get_by_id(&user_id).await {
                    Ok(Some(_)) => Ok(DispatchPauseTarget::user(user_id)),
                    Ok(None) => Err(format!("user `{user_id}` not found")),
                    Err(error) => Err(format!("user lookup failed: {error}")),
                }
            }
        }
    }
}

fn required_target_id(target_id: Option<&str>, scope: &str) -> Result<String, String> {
    let Some(target_id) = target_id else {
        return Err(format!("target_id is required for {scope} scope"));
    };
    let trimmed = target_id.trim();
    if trimmed.is_empty() {
        Err(format!("target_id is required for {scope} scope"))
    } else {
        Ok(trimmed.to_owned())
    }
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("UTC timestamp should format as RFC3339")
}

fn emit_if_changed(
    event_bus: &djinn_core::events::EventBus,
    mutation: &DispatchPauseMutation,
    resumed_by: Option<&str>,
    resumed_at: Option<&str>,
) {
    if mutation.changed {
        event_bus.send(DjinnEventEnvelope::dispatch_pause_changed(
            mutation.scope,
            mutation.target_id.as_deref(),
            mutation.current.as_ref(),
            mutation.previous.as_ref(),
            resumed_by,
            resumed_at,
        ));
    }
}

fn mutation_ok(mutation: DispatchPauseMutation) -> Json<DispatchPauseMutationResponse> {
    Json(DispatchPauseMutationResponse {
        ok: true,
        changed: mutation.changed,
        scope: mutation.scope,
        target_id: mutation.target_id,
        previous: mutation.previous,
        current: mutation.current,
        error: None,
    })
}

fn mutation_error(
    scope: DispatchPauseScope,
    target_id: Option<String>,
    error: String,
) -> Json<DispatchPauseMutationResponse> {
    Json(DispatchPauseMutationResponse {
        ok: false,
        changed: false,
        scope,
        target_id,
        previous: None,
        current: None,
        error: Some(error),
    })
}

fn status_error(
    scope: Option<DispatchPauseScope>,
    target_id: Option<String>,
    error: String,
) -> Json<DispatchPauseStatusResponse> {
    Json(DispatchPauseStatusResponse {
        ok: false,
        scope,
        target_id,
        current: None,
        state: None,
        error: Some(error),
    })
}
