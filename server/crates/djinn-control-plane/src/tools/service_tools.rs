//! Backing-service catalog MCP tool.
//!
//! Backing services (Postgres/Redis/RabbitMQ) are injected declaratively: a
//! project's selected catalog image declares which presets every task-run
//! Pod provides (see `image_set_services`), and djinn-k8s injects
//! each as a native sidecar with the connection string pre-set in an env var
//! (e.g. `TEST_POSTGRES_URL`). There is no on-demand "request a service" step
//! any more — the worker just reads the env var. This module only exposes the
//! read-only preset catalog so operators/UI can see what's injectable.

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;
use djinn_db::ServicePresetRepository;

#[derive(Deserialize, JsonSchema)]
pub struct ServicePresetListParams {}

#[derive(Serialize, JsonSchema)]
pub struct ServicePresetDto {
    pub id: String,
    pub name: String,
    pub service_type: String,
    pub image: String,
    pub conn_env_var: String,
}

#[derive(Serialize, JsonSchema)]
pub struct ServicePresetListResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub presets: Vec<ServicePresetDto>,
}

#[tool_router(router = service_tool_router, vis = "pub")]
impl DjinnMcpServer {
    #[tool(
        description = "List the available backing-service presets (Postgres/Redis/RabbitMQ) that an image can declare via image_set_services. Declared services are injected as native sidecars into every task-run Pod, reachable on 127.0.0.1 with the connection string in the preset's env var."
    )]
    pub async fn service_preset_list(
        &self,
        Parameters(_): Parameters<ServicePresetListParams>,
    ) -> Json<ServicePresetListResponse> {
        match ServicePresetRepository::new(self.state.db().clone())
            .list()
            .await
        {
            Ok(rows) => Json(ServicePresetListResponse {
                status: "ok".into(),
                error: None,
                presets: rows
                    .into_iter()
                    .map(|p| ServicePresetDto {
                        id: p.id,
                        name: p.name,
                        service_type: p.service_type,
                        image: p.image,
                        conn_env_var: p.conn_env_var,
                    })
                    .collect(),
            }),
            Err(e) => Json(ServicePresetListResponse {
                status: "error".into(),
                error: Some(format!("db error: {e}")),
                presets: Vec::new(),
            }),
        }
    }
}
