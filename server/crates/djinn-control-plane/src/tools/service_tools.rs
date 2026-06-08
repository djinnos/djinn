//! On-demand backing-service MCP tools (Phase C). A worker requests a service
//! at test time; the control plane provisions an ephemeral Pod + ClusterIP
//! Service for the task and returns connection info the worker exports.
//!
//! Authorization = capability (the project's selected IMAGE must allow the
//! preset) ∧ policy (`service_policy.allow_services`). Idempotent: one instance
//! per service-type per task. Gated behind `DJINN_BACKING_SERVICES_ENABLED`
//! (default off) — NetworkPolicies + the reaper must be in place before
//! enabling on a live cluster.

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::bridge::ProvisionServiceRequest;
use crate::server::DjinnMcpServer;
use djinn_db::{
    ImageRepository, ServiceInstanceRepository, ServiceInstanceStatus, ServicePolicyRepository,
    ServicePresetRepository, TaskRepository, TaskRunRepository,
};

fn backing_services_enabled() -> bool {
    std::env::var("DJINN_BACKING_SERVICES_ENABLED")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false)
}

fn parse_resources(json: &str) -> (String, String, String, String) {
    let v: serde_json::Value = serde_json::from_str(json).unwrap_or(serde_json::Value::Null);
    let g = |k: &str, d: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or(d).to_string();
    (
        g("cpu_request", "100m"),
        g("memory_request", "256Mi"),
        g("cpu_limit", "500m"),
        g("memory_limit", "512Mi"),
    )
}

fn parse_env(json: &str) -> Vec<(String, String)> {
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(serde_json::Value::Object(map)) => map
            .into_iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
            .collect(),
        _ => Vec::new(),
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct ServiceRequestParams {
    /// Task UUID or short_id this service is for (its current run owns the pod).
    pub task: String,
    /// Preset to request: a preset id (e.g. `preset-postgres-18`), service type
    /// (`postgres`/`redis`/`rabbitmq`), or preset name.
    pub preset: String,
}

#[derive(Serialize, JsonSchema)]
pub struct ServiceRequestResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Connection string to use (also set this in the named env var for tests).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conn_string: Option<String>,
    /// Env var convention for this service (e.g. TEST_POSTGRES_URL / REDIS_URL / AMQP_URL).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_var: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_type: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ServiceListParams {
    pub task: String,
}

#[derive(Serialize, JsonSchema)]
pub struct ServiceInstanceDto {
    pub service_type: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conn_string: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_var: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct ServiceListResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub services: Vec<ServiceInstanceDto>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ServiceReleaseParams {
    pub task: String,
    /// Release only this service type; omit to release all for the task.
    #[serde(default)]
    pub service_type: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct ServiceReleaseResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub released: i64,
}

fn err_req(msg: impl Into<String>) -> Json<ServiceRequestResponse> {
    Json(ServiceRequestResponse {
        status: "error".into(),
        error: Some(msg.into()),
        conn_string: None,
        env_var: None,
        service_type: None,
    })
}

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
        description = "List the available backing-service presets (Postgres/Redis/RabbitMQ) that an image can allow."
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

    #[tool(
        description = "Request an on-demand backing service (Postgres/Redis/RabbitMQ) for this task's test run. Returns a connection string + the env var to export. Idempotent per service-type. Requires the project's selected image to allow the preset."
    )]
    pub async fn service_request(
        &self,
        Parameters(input): Parameters<ServiceRequestParams>,
    ) -> Json<ServiceRequestResponse> {
        if !backing_services_enabled() {
            return err_req("backing services are not enabled on this deployment");
        }
        let db = self.state.db();
        let task_repo = TaskRepository::new(db.clone(), self.state.event_bus());
        let task = match task_repo.resolve(&input.task).await {
            Ok(Some(t)) => t,
            Ok(None) => return err_req(format!("task not found: {}", input.task)),
            Err(e) => return err_req(format!("db error: {e}")),
        };
        let project_id = task.project_id.clone();

        // Current run owns the service (ownerRef + idempotency key).
        let runs = match TaskRunRepository::new(db.clone())
            .list_for_task(&task.id)
            .await
        {
            Ok(r) => r,
            Err(e) => return err_req(format!("db error: {e}")),
        };
        let Some(task_run_id) = runs.first().map(|r| r.id.clone()) else {
            return err_req("no task run found for this task (is it dispatched?)");
        };

        // Policy kill-switch.
        match ServicePolicyRepository::new(db.clone())
            .allow_services(&project_id)
            .await
        {
            Ok(true) => {}
            Ok(false) => return err_req("backing services are disabled for this project"),
            Err(e) => return err_req(format!("db error: {e}")),
        }

        // Resolve the preset by id, then service_type, then name.
        let preset_repo = ServicePresetRepository::new(db.clone());
        let preset = match preset_repo.get(&input.preset).await {
            Ok(Some(p)) => Some(p),
            Ok(None) => match preset_repo.list().await {
                Ok(all) => all
                    .into_iter()
                    .find(|p| p.service_type == input.preset || p.name == input.preset),
                Err(e) => return err_req(format!("db error: {e}")),
            },
            Err(e) => return err_req(format!("db error: {e}")),
        };
        let Some(preset) = preset else {
            return err_req(format!("unknown preset: {}", input.preset));
        };

        // Capability: the project's selected image must allow this preset.
        let image_repo = ImageRepository::new(db.clone());
        let image = match image_repo.resolve_for_project(&project_id).await {
            Ok(Some(i)) => i,
            Ok(None) => {
                return err_req(
                    "project has no selected image, so it can't request services (assign an image that allows this preset)",
                );
            }
            Err(e) => return err_req(format!("db error: {e}")),
        };
        match image_repo.is_preset_allowed(&image.id, &preset.id).await {
            Ok(true) => {}
            Ok(false) => {
                return err_req(format!(
                    "image '{}' does not allow the '{}' service",
                    image.name, preset.name
                ));
            }
            Err(e) => return err_req(format!("db error: {e}")),
        }

        // Idempotent claim (one per task+type).
        let instance_repo = ServiceInstanceRepository::new(db.clone());
        let instance_id = uuid::Uuid::now_v7().to_string();
        let (instance, _created) = match instance_repo
            .claim(
                &instance_id,
                &task_run_id,
                &project_id,
                &preset.id,
                &preset.service_type,
            )
            .await
        {
            Ok(v) => v,
            Err(e) => return err_req(format!("claim service: {e}")),
        };
        // Already provisioned → return the stored connection.
        if instance.status == ServiceInstanceStatus::READY
            && let Some(conn) = instance.conn_string.clone()
        {
            return Json(ServiceRequestResponse {
                status: "ok".into(),
                error: None,
                conn_string: Some(conn),
                env_var: Some(preset.conn_env_var.clone()),
                service_type: Some(preset.service_type.clone()),
            });
        }

        // Provision the Pod + Service.
        let (cpu_request, memory_request, cpu_limit, memory_limit) =
            parse_resources(&preset.resources);
        let req = ProvisionServiceRequest {
            instance_id: instance.id.clone(),
            task_run_id: task_run_id.clone(),
            service_type: preset.service_type.clone(),
            image: preset.image.clone(),
            port: preset.port,
            env: parse_env(&preset.env),
            cpu_request,
            memory_request,
            cpu_limit,
            memory_limit,
            conn_template: preset.conn_template.clone(),
        };
        match self.state.provision_backing_service(req).await {
            Ok(conn) => {
                if let Err(e) = instance_repo
                    .mark_ready(
                        &instance.id,
                        &conn.pod_name,
                        &conn.service_name,
                        None,
                        &preset.conn_env_var,
                        &conn.conn_string,
                    )
                    .await
                {
                    return err_req(format!("persist ready: {e}"));
                }
                Json(ServiceRequestResponse {
                    status: "ok".into(),
                    error: None,
                    conn_string: Some(conn.conn_string),
                    env_var: Some(preset.conn_env_var),
                    service_type: Some(preset.service_type),
                })
            }
            Err(e) => {
                let _ = instance_repo.mark_failed(&instance.id, &e).await;
                err_req(format!("provision: {e}"))
            }
        }
    }

    #[tool(description = "List the backing services provisioned for this task's current run.")]
    pub async fn service_list(
        &self,
        Parameters(input): Parameters<ServiceListParams>,
    ) -> Json<ServiceListResponse> {
        let db = self.state.db();
        let task_repo = TaskRepository::new(db.clone(), self.state.event_bus());
        let task = match task_repo.resolve(&input.task).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                return Json(ServiceListResponse {
                    status: "error".into(),
                    error: Some(format!("task not found: {}", input.task)),
                    services: Vec::new(),
                });
            }
            Err(e) => {
                return Json(ServiceListResponse {
                    status: "error".into(),
                    error: Some(format!("db error: {e}")),
                    services: Vec::new(),
                });
            }
        };
        let runs = TaskRunRepository::new(db.clone())
            .list_for_task(&task.id)
            .await
            .unwrap_or_default();
        let Some(task_run_id) = runs.first().map(|r| r.id.clone()) else {
            return Json(ServiceListResponse {
                status: "ok".into(),
                error: None,
                services: Vec::new(),
            });
        };
        match ServiceInstanceRepository::new(db.clone())
            .list_for_task(&task_run_id)
            .await
        {
            Ok(rows) => Json(ServiceListResponse {
                status: "ok".into(),
                error: None,
                services: rows
                    .into_iter()
                    .map(|i| ServiceInstanceDto {
                        service_type: i.service_type,
                        status: i.status,
                        conn_string: i.conn_string,
                        env_var: i.conn_env_var,
                    })
                    .collect(),
            }),
            Err(e) => Json(ServiceListResponse {
                status: "error".into(),
                error: Some(format!("db error: {e}")),
                services: Vec::new(),
            }),
        }
    }

    #[tool(
        description = "Release backing services for this task's run (delete the pods/services). Pass service_type to release one, or omit to release all."
    )]
    pub async fn service_release(
        &self,
        Parameters(input): Parameters<ServiceReleaseParams>,
    ) -> Json<ServiceReleaseResponse> {
        let db = self.state.db();
        let task_repo = TaskRepository::new(db.clone(), self.state.event_bus());
        let task = match task_repo.resolve(&input.task).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                return Json(ServiceReleaseResponse {
                    status: "error".into(),
                    error: Some(format!("task not found: {}", input.task)),
                    released: 0,
                });
            }
            Err(e) => {
                return Json(ServiceReleaseResponse {
                    status: "error".into(),
                    error: Some(format!("db error: {e}")),
                    released: 0,
                });
            }
        };
        let runs = TaskRunRepository::new(db.clone())
            .list_for_task(&task.id)
            .await
            .unwrap_or_default();
        let Some(task_run_id) = runs.first().map(|r| r.id.clone()) else {
            return Json(ServiceReleaseResponse {
                status: "ok".into(),
                error: None,
                released: 0,
            });
        };
        let instance_repo = ServiceInstanceRepository::new(db.clone());
        let instances = instance_repo
            .list_for_task(&task_run_id)
            .await
            .unwrap_or_default();
        let mut released: i64 = 0;
        for inst in instances {
            if let Some(t) = &input.service_type
                && &inst.service_type != t
            {
                continue;
            }
            if inst.status == ServiceInstanceStatus::RELEASED {
                continue;
            }
            let _ = self.state.release_backing_service(&inst.id).await;
            let _ = instance_repo.mark_released(&inst.id).await;
            released += 1;
        }
        Json(ServiceReleaseResponse {
            status: "ok".into(),
            error: None,
            released,
        })
    }
}
