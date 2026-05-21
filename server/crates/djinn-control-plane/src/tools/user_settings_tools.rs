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
    pub error: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct UserSettingsSetParams {
    /// Enable or disable auto-approve. Omit to keep the current value.
    pub auto_approve_prs: Option<bool>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct UserSettingsSetResponse {
    pub ok: bool,
    pub applied: bool,
    pub auto_approve_prs: Option<bool>,
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
                error: Some(missing_session()),
            });
        };
        let repo = UserSettingsRepository::new(self.state.db().clone());
        match repo.get_or_default(&user_id).await {
            Ok(s) => Json(UserSettingsGetResponse {
                ok: true,
                user_id: Some(user_id),
                auto_approve_prs: s.auto_approve_prs,
                error: None,
            }),
            Err(e) => Json(UserSettingsGetResponse {
                ok: false,
                user_id: Some(user_id),
                auto_approve_prs: false,
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
                error: Some(missing_session()),
            });
        };
        let repo = UserSettingsRepository::new(self.state.db().clone());
        // Only one field today; a future toggle would patch the current row
        // here instead of going straight to a single-column upsert.
        let Some(target) = p.auto_approve_prs else {
            // No-op patch returns the current value so the UI can confirm state.
            match repo.get_or_default(&user_id).await {
                Ok(s) => {
                    return Json(UserSettingsSetResponse {
                        ok: true,
                        applied: false,
                        auto_approve_prs: Some(s.auto_approve_prs),
                        error: None,
                    });
                }
                Err(e) => {
                    return Json(UserSettingsSetResponse {
                        ok: false,
                        applied: false,
                        auto_approve_prs: None,
                        error: Some(e.to_string()),
                    });
                }
            }
        };
        match repo.upsert_auto_approve_prs(&user_id, target).await {
            Ok(row) => Json(UserSettingsSetResponse {
                ok: true,
                applied: true,
                auto_approve_prs: Some(row.auto_approve_prs),
                error: None,
            }),
            Err(e) => Json(UserSettingsSetResponse {
                ok: false,
                applied: false,
                auto_approve_prs: None,
                error: Some(e.to_string()),
            }),
        }
    }
}
