use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;
use crate::tools::acting_user;
use djinn_provider::repos::CredentialRepository;

// ── credential_set ────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct CredentialSetInput {
    /// Provider ID this key belongs to (e.g. 'anthropic', 'openai').
    pub provider_id: String,
    /// Env-var style key name (e.g. 'ANTHROPIC_API_KEY').
    pub key_name: String,
    /// The raw API key value to store encrypted.
    pub api_key: String,
    /// Admin path: when `true`, store this as the ORG-SHARED fallback
    /// credential (`owner_user_id = NULL`) that any user without a personal
    /// credential for the same key_name will resolve. When `false`/absent, the
    /// credential is stamped private to the acting user (the connect-flow
    /// default). Defaults to `false`.
    #[serde(default)]
    pub org_shared: bool,
    /// Admin-only: act on behalf of this user id (e.g. another user to
    /// configure). Non-admins must omit it.
    #[serde(default)]
    pub target_user_id: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct CredentialSetResponse {
    pub ok: bool,
    pub success: bool,
    pub id: String,
    pub key_name: String,
    pub error: Option<String>,
}

// ── credential_list ───────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema, Default)]
pub struct CredentialListInput {
    /// Admin-only: act on behalf of this user id (e.g. another user to
    /// configure). Non-admins must omit it.
    #[serde(default)]
    pub target_user_id: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct CredentialListResponse {
    pub credentials: Vec<CredentialSummary>,
}

#[derive(Serialize, JsonSchema)]
pub struct CredentialSummary {
    pub id: String,
    pub provider_id: String,
    pub key_name: String,
    pub created_at: String,
    pub updated_at: String,
}

// ── credential_delete ─────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct CredentialDeleteInput {
    /// Env-var style key name of the credential to delete (e.g. 'ANTHROPIC_API_KEY').
    pub key_name: String,
}

#[derive(Serialize, JsonSchema)]
pub struct CredentialDeleteResponse {
    pub ok: bool,
    pub success: bool,
    pub deleted: bool,
    pub key_name: String,
    pub error: Option<String>,
}

// ── Tool router ───────────────────────────────────────────────────────────────

#[tool_router(router = credential_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Store or update an encrypted API key in the credential vault.
    /// Use this to save provider API keys for agent dispatch.
    #[tool(
        description = "Store or update an encrypted API key in the credential vault. Use this to save provider API keys for agent dispatch."
    )]
    pub async fn credential_set(
        &self,
        Parameters(input): Parameters<CredentialSetInput>,
    ) -> Json<CredentialSetResponse> {
        if input.key_name.is_empty() {
            return Json(CredentialSetResponse {
                ok: false,
                success: false,
                id: String::new(),
                key_name: input.key_name,
                error: Some("key_name must not be empty".into()),
            });
        }
        if input.api_key.is_empty() {
            return Json(CredentialSetResponse {
                ok: false,
                success: false,
                id: String::new(),
                key_name: input.key_name,
                error: Some("api_key must not be empty".into()),
            });
        }

        let repo = CredentialRepository::new(self.state.db().clone(), self.state.event_bus());

        // `org_shared` is the explicit admin path: write an org-shared
        // (`owner_user_id = NULL`) credential regardless of the acting user.
        // Otherwise `set()` stamps the credential private to the acting user
        // (read from `SESSION_USER_ID`); with no user context this also yields
        // an org-shared credential (local/single-user dev).
        let result = if input.org_shared {
            repo.set_with_owner(&input.provider_id, &input.key_name, &input.api_key, None)
                .await
        } else {
            // Non-org_shared: stamp the credential to the effective user. With
            // no `target_user_id` this is the acting user; an admin may target
            // another user (e.g. a target user to configure).
            let effective = match acting_user::resolve_effective_user(
                self.state.db(),
                input.target_user_id.as_deref(),
            )
            .await
            {
                Ok(u) => u,
                Err(e) => {
                    return Json(CredentialSetResponse {
                        ok: false,
                        success: false,
                        id: String::new(),
                        key_name: input.key_name,
                        error: Some(e),
                    });
                }
            };
            repo.set_with_owner(
                &input.provider_id,
                &input.key_name,
                &input.api_key,
                effective.as_deref(),
            )
            .await
        };

        match result {
            Ok(cred) => Json(CredentialSetResponse {
                ok: true,
                success: true,
                id: cred.id,
                key_name: cred.key_name,
                error: None,
            }),
            Err(e) => {
                tracing::warn!(key_name = %input.key_name, error = %e, "credential_set failed");
                Json(CredentialSetResponse {
                    ok: false,
                    success: false,
                    id: String::new(),
                    key_name: input.key_name,
                    error: Some(e.to_string()),
                })
            }
        }
    }

    /// List all stored credentials. Never returns raw key values.
    #[tool(
        description = "List all stored credentials. Never returns raw key values — only metadata (id, provider_id, key_name, timestamps)."
    )]
    pub async fn credential_list(
        &self,
        Parameters(input): Parameters<CredentialListInput>,
    ) -> Json<CredentialListResponse> {
        let repo = CredentialRepository::new(self.state.db().clone(), self.state.event_bus());

        // Per-user: the caller's own credentials plus org-shared fallbacks, so
        // the UI's "configured" state never reflects another user's keys. An
        // admin may pass `target_user_id` to inspect another user's set.
        let effective = match acting_user::resolve_effective_user(
            self.state.db(),
            input.target_user_id.as_deref(),
        )
        .await
        {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(error = %e, "credential_list: act-as denied");
                return Json(CredentialListResponse {
                    credentials: vec![],
                });
            }
        };

        let credentials = match repo.list_for_user(effective.as_deref()).await {
            Ok(creds) => creds
                .iter()
                .map(|c| CredentialSummary {
                    id: c.id.clone(),
                    provider_id: c.provider_id.clone(),
                    key_name: c.key_name.clone(),
                    created_at: c.created_at.clone(),
                    updated_at: c.updated_at.clone(),
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "credential_list failed");
                vec![]
            }
        };

        Json(CredentialListResponse { credentials })
    }

    /// Delete a stored credential by key_name.
    #[tool(
        description = "Delete a stored credential by key_name (e.g. 'ANTHROPIC_API_KEY'). Returns deleted=true if the credential existed."
    )]
    pub async fn credential_delete(
        &self,
        Parameters(input): Parameters<CredentialDeleteInput>,
    ) -> Json<CredentialDeleteResponse> {
        let repo = CredentialRepository::new(self.state.db().clone(), self.state.event_bus());

        match repo.delete(&input.key_name).await {
            Ok(deleted) => Json(CredentialDeleteResponse {
                ok: true,
                success: true,
                deleted,
                key_name: input.key_name,
                error: None,
            }),
            Err(e) => {
                tracing::warn!(key_name = %input.key_name, error = %e, "credential_delete failed");
                Json(CredentialDeleteResponse {
                    ok: false,
                    success: false,
                    deleted: false,
                    key_name: input.key_name,
                    error: Some(e.to_string()),
                })
            }
        }
    }
}
