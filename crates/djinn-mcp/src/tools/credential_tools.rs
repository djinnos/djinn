use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use djinn_provider::repos::CredentialRepository;
use crate::server::DjinnMcpServer;

// ── credential_set ────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct CredentialSetInput {
    /// Provider ID this key belongs to (e.g. 'anthropic', 'openai').
    pub provider_id: String,
    /// Env-var style key name (e.g. 'ANTHROPIC_API_KEY').
    pub key_name: String,
    /// The raw API key value to store encrypted.
    pub api_key: String,
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

        match repo
            .set(&input.provider_id, &input.key_name, &input.api_key)
            .await
        {
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
    pub async fn credential_list(&self) -> Json<CredentialListResponse> {
        let repo = CredentialRepository::new(self.state.db().clone(), self.state.event_bus());

        let credentials = match repo.list().await {
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
