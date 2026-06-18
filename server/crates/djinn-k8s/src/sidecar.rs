//! Native-sidecar injection for backing services (Postgres / Redis / RabbitMQ).
//!
//! A project's selected catalog image declares which service presets every
//! task-run / verification Pod should provide (the `image_service_presets`
//! junction, migration 66). Each declared service is injected as a *native
//! sidecar*: an init container with `restartPolicy: Always`. A native sidecar
//!   * shares the Pod network namespace, so the worker reaches it on
//!     `127.0.0.1:<port>` — no ClusterIP Service, DNS name, or NetworkPolicy;
//!   * has its startup gated before the worker container starts (via a startup
//!     probe), so the DB is already accepting connections when the worker runs;
//!   * is terminated automatically by the kubelet when the worker container
//!     exits, so the Job still reaches Completed. A plain extra container would
//!     run forever and the Job would never finish — that's the whole reason a
//!     *native* sidecar (initContainer + `restartPolicy: Always`) is required.
//!
//! The connection string is exported to the worker container as the preset's
//! env var (e.g. `TEST_POSTGRES_URL`) so the agent just reads it and uses it —
//! no MCP round-trip, no explicit "start a database" step.
//!
//! [`sidecar_container`] / [`sidecar_conn_env`] are pure manifest builders;
//! [`resolve_image_services`] is the impure resolver that reads the catalog for
//! a project's image (used by the task-run + verification dispatch paths).

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    Container, ContainerPort, EmptyDirVolumeSource, EnvVar, Probe, ResourceRequirements,
    SecurityContext, TCPSocketAction, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use djinn_db::{Database, ImageRepository, ServicePresetRepository};

use crate::config::KubernetesConfig;

/// Pod-level Memory `emptyDir` mounted into each sidecar at `/dev/shm`. The K8s
/// default `/dev/shm` (64Mi) is too small for some service defaults (Postgres
/// dynamic shared memory); harmless for Redis/RabbitMQ. The Pod adds the volume
/// only when at least one service is injected.
pub const SIDECAR_DSHM_VOLUME: &str = "svc-dshm";

/// Everything the sidecar builder needs from a `service_presets` row. The
/// caller maps it so this module never reaches into DB row shape.
#[derive(Clone, Debug)]
pub struct BackingServiceSpec {
    pub service_type: String,
    pub image: String,
    pub port: i32,
    pub env: Vec<(String, String)>,
    pub cpu_request: String,
    pub memory_request: String,
    pub cpu_limit: String,
    pub memory_limit: String,
    pub conn_template: String,
    pub conn_env_var: String,
}

/// The pod-level `/dev/shm` Memory `emptyDir` referenced by the sidecar mounts.
/// Add it to the Pod's volumes only when at least one service is injected.
pub fn sidecar_dshm_volume() -> Volume {
    Volume {
        name: SIDECAR_DSHM_VOLUME.to_string(),
        empty_dir: Some(EmptyDirVolumeSource {
            medium: Some("Memory".to_string()),
            size_limit: Some(Quantity("256Mi".to_string())),
        }),
        ..Volume::default()
    }
}

/// Render a preset's connection template for a pod-local sidecar: `{host}` →
/// `127.0.0.1`, `{port}` → the service port.
pub fn render_local_conn(conn_template: &str, port: i32) -> String {
    conn_template
        .replace("{host}", "127.0.0.1")
        .replace("{port}", &port.to_string())
}

/// The env var the worker container exports for this service's connection.
pub fn sidecar_conn_env(spec: &BackingServiceSpec) -> EnvVar {
    EnvVar {
        name: spec.conn_env_var.clone(),
        value: Some(render_local_conn(&spec.conn_template, spec.port)),
        ..EnvVar::default()
    }
}

/// Build the native-sidecar `Container` for one backing service. The caller
/// appends it to the Pod's `initContainers` (NOT `containers`); the
/// `restartPolicy: Always` is what makes the kubelet treat it as a sidecar.
pub fn sidecar_container(config: &KubernetesConfig, spec: &BackingServiceSpec) -> Container {
    let env: Vec<EnvVar> = spec
        .env
        .iter()
        .map(|(k, v)| EnvVar {
            name: k.clone(),
            value: Some(v.clone()),
            ..EnvVar::default()
        })
        .collect();

    let probe = || Probe {
        tcp_socket: Some(TCPSocketAction {
            port: IntOrString::Int(spec.port),
            ..TCPSocketAction::default()
        }),
        initial_delay_seconds: Some(1),
        period_seconds: Some(2),
        // ~60s budget: Postgres initdb on alpine is a few seconds; generous so
        // a slow first pull/init doesn't wedge the worker start.
        failure_threshold: Some(30),
        ..Probe::default()
    };

    Container {
        name: format!("svc-{}", spec.service_type),
        image: Some(spec.image.clone()),
        image_pull_policy: Some(config.image_pull_policy.clone()),
        // Native sidecar: an init container that keeps running for the life of
        // the worker container and is auto-terminated when the worker exits.
        restart_policy: Some("Always".to_string()),
        env: Some(env),
        ports: Some(vec![ContainerPort {
            container_port: spec.port,
            ..ContainerPort::default()
        }]),
        volume_mounts: Some(vec![VolumeMount {
            name: SIDECAR_DSHM_VOLUME.to_string(),
            mount_path: "/dev/shm".to_string(),
            ..VolumeMount::default()
        }]),
        // The worker container does not start until this startup probe passes —
        // so the service is accepting connections before any test runs.
        startup_probe: Some(probe()),
        readiness_probe: Some(probe()),
        // Override the Pod-level securityContext (which forces uid 10001 so the
        // worker can write the shared /mirror PVC). Stock service images expect
        // to start as root and drop to their own service user in their
        // entrypoint (e.g. Postgres chowns PGDATA then `su`s to `postgres`);
        // forcing uid 10001 breaks initdb. The sidecar touches no shared PVC, so
        // running it as the image default is safe — the Pod boundary is the
        // perimeter.
        security_context: Some(SecurityContext {
            run_as_user: Some(0),
            run_as_group: Some(0),
            run_as_non_root: Some(false),
            ..SecurityContext::default()
        }),
        resources: Some(ResourceRequirements {
            requests: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity(spec.cpu_request.clone())),
                ("memory".to_string(), Quantity(spec.memory_request.clone())),
            ])),
            limits: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity(spec.cpu_limit.clone())),
                ("memory".to_string(), Quantity(spec.memory_limit.clone())),
            ])),
            ..ResourceRequirements::default()
        }),
        ..Container::default()
    }
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

/// Resolve the backing services declared on a project's selected catalog image.
///
/// Returns an empty list when the project has no catalog image, the image
/// declares no services, or any read fails — service injection is best-effort
/// and must never block task dispatch.
pub async fn resolve_image_services(db: &Database, project_id: &str) -> Vec<BackingServiceSpec> {
    let image_repo = ImageRepository::new(db.clone());
    let image = match image_repo.resolve_for_project(project_id).await {
        Ok(Some(image)) => image,
        Ok(None) => return Vec::new(),
        Err(error) => {
            tracing::warn!(
                project_id = %project_id,
                %error,
                "sidecar: resolve_for_project failed; injecting no backing services"
            );
            return Vec::new();
        }
    };

    let preset_ids = match image_repo.list_service_presets(&image.id).await {
        Ok(ids) => ids,
        Err(error) => {
            tracing::warn!(
                project_id = %project_id,
                image_id = %image.id,
                %error,
                "sidecar: list_service_presets failed; injecting no backing services"
            );
            return Vec::new();
        }
    };
    if preset_ids.is_empty() {
        return Vec::new();
    }

    let preset_repo = ServicePresetRepository::new(db.clone());
    let mut specs = Vec::with_capacity(preset_ids.len());
    for preset_id in preset_ids {
        match preset_repo.get(&preset_id).await {
            Ok(Some(p)) => {
                let (cpu_request, memory_request, cpu_limit, memory_limit) =
                    parse_resources(&p.resources);
                specs.push(BackingServiceSpec {
                    service_type: p.service_type,
                    image: p.image,
                    port: p.port,
                    env: parse_env(&p.env),
                    cpu_request,
                    memory_request,
                    cpu_limit,
                    memory_limit,
                    conn_template: p.conn_template,
                    conn_env_var: p.conn_env_var,
                });
            }
            Ok(None) => tracing::warn!(
                project_id = %project_id,
                %preset_id,
                "sidecar: image references an unknown service preset; skipping"
            ),
            Err(error) => tracing::warn!(
                project_id = %project_id,
                %preset_id,
                %error,
                "sidecar: service preset read failed; skipping"
            ),
        }
    }
    specs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> BackingServiceSpec {
        BackingServiceSpec {
            service_type: "postgres".into(),
            image: "postgres:18-alpine".into(),
            port: 5432,
            env: vec![("POSTGRES_PASSWORD".into(), "postgres".into())],
            cpu_request: "100m".into(),
            memory_request: "256Mi".into(),
            cpu_limit: "500m".into(),
            memory_limit: "512Mi".into(),
            conn_template: "postgres://postgres:postgres@{host}:{port}/app_test".into(),
            conn_env_var: "TEST_POSTGRES_URL".into(),
        }
    }

    #[test]
    fn conn_env_renders_loopback_host() {
        let env = sidecar_conn_env(&spec());
        assert_eq!(env.name, "TEST_POSTGRES_URL");
        assert_eq!(
            env.value.as_deref(),
            Some("postgres://postgres:postgres@127.0.0.1:5432/app_test")
        );
    }

    #[test]
    fn sidecar_is_a_native_sidecar() {
        let cfg = KubernetesConfig::for_testing();
        let c = sidecar_container(&cfg, &spec());
        // restartPolicy: Always on an initContainer == native sidecar.
        assert_eq!(c.restart_policy.as_deref(), Some("Always"));
        assert_eq!(c.name, "svc-postgres");
        assert_eq!(c.image.as_deref(), Some("postgres:18-alpine"));
        assert_eq!(c.ports.as_ref().unwrap()[0].container_port, 5432);
        // Startup probe gates the worker start on DB readiness.
        assert!(c.startup_probe.is_some());
        // Must run as the image default (root) so the service entrypoint can
        // initialise its data dir; the Pod default uid 10001 would break it.
        let sc = c.security_context.as_ref().expect("container securityContext");
        assert_eq!(sc.run_as_user, Some(0));
        assert_eq!(sc.run_as_non_root, Some(false));
    }
}
