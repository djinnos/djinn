//! Per-user preferences exposed over MCP.
//!
//! Identity comes from the task-local [`current_user_id`] scope set by the
//! HTTP MCP handler at request authentication — there is no `user_id`
//! parameter on these tools, by design. An unauthenticated caller gets a
//! clear error rather than reading or mutating someone else's settings.

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;
use djinn_core::auth_context::current_user_id;
use djinn_db::UserSettingsRepository;

#[derive(Deserialize, schemars::JsonSchema)]
pub struct UserSettingsGetParams {}

#[derive(Serialize, schemars::JsonSchema)]
pub struct UserSettingsGetResponse {
    pub ok: bool,
    /// `users.id` of the signed-in caller (echoed so the UI can sanity-check identity).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Auto-approve PRs that are otherwise ready to merge. When true, the
    /// poller POSTs an APPROVE review using this user's GitHub token at the
    /// moment the PR has CI green + no conflicts + no existing approvals.
    /// Defaults to false. Each task's `created_by_user_id` decides whose
    /// toggle applies; background-agent tasks (`created_by_user_id IS NULL`)
    /// are never auto-approved.
    pub auto_approve_prs: bool,
    /// This user's ordered model selection (highest priority first), full
    /// `provider/model` ids. Empty when the user has no explicit selection
    /// (callers then fall back to the global deployment model list).
    pub models: Vec<String>,
    pub error: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct UserSettingsSetParams {
    /// Enable or disable auto-approve. Omit to keep the current value.
    pub auto_approve_prs: Option<bool>,
    /// Ordered model selection for THIS user (highest priority first), as full
    /// `provider/model` ids. Each must be a model on a provider this user has
    /// connected. Pass `[]` to clear the selection (→ global fallback). Omit to
    /// keep the current value.
    pub models: Option<Vec<String>>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct UserSettingsSetResponse {
    pub ok: bool,
    pub applied: bool,
    pub auto_approve_prs: Option<bool>,
    /// The resulting model selection after the patch.
    pub models: Option<Vec<String>>,
    pub error: Option<String>,
}

fn missing_session() -> String {
    "sign in with GitHub to manage user settings".to_string()
}

#[tool_router(router = user_settings_tool_router, vis = "pub")]
impl DjinnMcpServer {
    #[tool(description = "Get the signed-in user's settings (auto_approve_prs, …). \
        Returns defaults if the user has never set anything. \
        Errors when no authenticated session is present.")]
    pub async fn user_settings_get(
        &self,
        Parameters(_): Parameters<UserSettingsGetParams>,
    ) -> Json<UserSettingsGetResponse> {
        let Some(user_id) = current_user_id() else {
            return Json(UserSettingsGetResponse {
                ok: false,
                user_id: None,
                auto_approve_prs: false,
                models: Vec::new(),
                error: Some(missing_session()),
            });
        };
        let repo = UserSettingsRepository::new(self.state.db().clone());
        match repo.get_or_default(&user_id).await {
            Ok(s) => Json(UserSettingsGetResponse {
                ok: true,
                user_id: Some(user_id),
                auto_approve_prs: s.auto_approve_prs,
                models: s.models.unwrap_or_default(),
                error: None,
            }),
            Err(e) => Json(UserSettingsGetResponse {
                ok: false,
                user_id: Some(user_id),
                auto_approve_prs: false,
                models: Vec::new(),
                error: Some(e.to_string()),
            }),
        }
    }

    #[tool(description = "Patch the signed-in user's settings. Only provided fields \
        are updated; omitted fields keep their current values. \
        Errors when no authenticated session is present.")]
    pub async fn user_settings_set(
        &self,
        Parameters(p): Parameters<UserSettingsSetParams>,
    ) -> Json<UserSettingsSetResponse> {
        let Some(user_id) = current_user_id() else {
            return Json(UserSettingsSetResponse {
                ok: false,
                applied: false,
                auto_approve_prs: None,
                models: None,
                error: Some(missing_session()),
            });
        };
        let repo = UserSettingsRepository::new(self.state.db().clone());

        let err = |msg: String| {
            Json(UserSettingsSetResponse {
                ok: false,
                applied: false,
                auto_approve_prs: None,
                models: None,
                error: Some(msg),
            })
        };

        let mut applied = false;

        // Per-user model selection: validate every id against THIS user's
        // connected providers before persisting, so a user can't select a
        // model on a provider they haven't connected.
        if let Some(models) = p.models.as_ref() {
            if let Err(e) = self
                .state
                .validate_models_for_user(models, Some(&user_id))
                .await
            {
                return err(e);
            }
            if let Err(e) = repo.upsert_models(&user_id, models).await {
                return err(e.to_string());
            }
            applied = true;
        }

        if let Some(target) = p.auto_approve_prs {
            if let Err(e) = repo.upsert_auto_approve_prs(&user_id, target).await {
                return err(e.to_string());
            }
            applied = true;
        }

        // A changed model selection alters the union of models the fleet must
        // be able to dispatch, so recompute slot-pool capacity and trigger a
        // dispatch pass. No-op for auto-approve-only patches.
        if p.models.is_some() {
            self.state.apply_user_model_change().await;
        }

        match repo.get_or_default(&user_id).await {
            Ok(s) => Json(UserSettingsSetResponse {
                ok: true,
                applied,
                auto_approve_prs: Some(s.auto_approve_prs),
                models: Some(s.models.unwrap_or_default()),
                error: None,
            }),
            Err(e) => err(e.to_string()),
        }
    }
}
