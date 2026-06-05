//! Pure manifest builders for an on-demand backing-service Pod + ClusterIP
//! Service (Phase C). A worker requests a service via MCP; the provisioner
//! creates one of these per (task, service-type), the worker connects over
//! `<service_name>.<namespace>.svc:<port>`, and it's torn down with the task
//! (ownerRef to the task-run Job + a label-based reaper backstop).

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    Container, ContainerPort, EmptyDirVolumeSource, EnvVar, Pod, PodSpec, ResourceRequirements,
    Service, ServicePort, ServiceSpec, TCPSocketAction, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use crate::config::KubernetesConfig;
use crate::warm_job::sanitize_id;

pub const COMPONENT_BACKING_SERVICE: &str = "backing-service";
pub const LABEL_COMPONENT: &str = "djinn.app/component";
pub const LABEL_TASK_RUN_ID: &str = "djinn.app/task-run-id";
pub const LABEL_SERVICE_TYPE: &str = "djinn.app/service-type";
pub const LABEL_INSTANCE_ID: &str = "djinn.app/service-instance-id";

/// What the builder needs from a `service_presets` row (mapped by the
/// provisioner so djinn-k8s doesn't depend on djinn-db).
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
}

/// Stable (pod, service) names for an instance — the service name is the DNS
/// host the worker dials (`<service>.<namespace>.svc`).
pub fn backing_service_names(instance_id: &str) -> (String, String) {
    let base = format!("djinn-svc-{}", &sanitize_id(instance_id));
    (base.clone(), base)
}

fn labels(instance_id: &str, task_run_id: &str, service_type: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (LABEL_COMPONENT.to_string(), COMPONENT_BACKING_SERVICE.to_string()),
        (LABEL_TASK_RUN_ID.to_string(), sanitize_id(task_run_id)),
        (LABEL_SERVICE_TYPE.to_string(), sanitize_id(service_type)),
        (LABEL_INSTANCE_ID.to_string(), sanitize_id(instance_id)),
    ])
}

/// Build the backing-service Pod. `owner` (the task-run Job) is set by the
/// provisioner so the Pod is garbage-collected when the task ends.
pub fn build_backing_service_pod(
    config: &KubernetesConfig,
    spec: &BackingServiceSpec,
    instance_id: &str,
    task_run_id: &str,
    owner: Option<OwnerReference>,
) -> Pod {
    let (pod_name, _) = backing_service_names(instance_id);
    let lbls = labels(instance_id, task_run_id, &spec.service_type);

    let env: Vec<EnvVar> = spec
        .env
        .iter()
        .map(|(k, v)| EnvVar {
            name: k.clone(),
            value: Some(v.clone()),
            ..EnvVar::default()
        })
        .collect();

    // /dev/shm as a memory-backed emptyDir: the K8s default (64Mi) is too small
    // for some service defaults (e.g. Postgres shared memory). Harmless for the
    // others.
    let shm_volume = Volume {
        name: "dshm".to_string(),
        empty_dir: Some(EmptyDirVolumeSource {
            medium: Some("Memory".to_string()),
            size_limit: Some(Quantity("256Mi".to_string())),
        }),
        ..Volume::default()
    };

    let container = Container {
        name: spec.service_type.clone(),
        image: Some(spec.image.clone()),
        image_pull_policy: Some(config.image_pull_policy.clone()),
        env: Some(env),
        ports: Some(vec![ContainerPort {
            container_port: spec.port,
            ..ContainerPort::default()
        }]),
        volume_mounts: Some(vec![VolumeMount {
            name: "dshm".to_string(),
            mount_path: "/dev/shm".to_string(),
            ..VolumeMount::default()
        }]),
        // TCP readiness on the service port — cheap and works for all three
        // (avoids RabbitMQ's HTTP management probe CPU cost).
        readiness_probe: Some(k8s_openapi::api::core::v1::Probe {
            tcp_socket: Some(TCPSocketAction {
                port: IntOrString::Int(spec.port),
                ..TCPSocketAction::default()
            }),
            initial_delay_seconds: Some(3),
            period_seconds: Some(3),
            failure_threshold: Some(20),
            ..k8s_openapi::api::core::v1::Probe::default()
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
    };

    let node_selector = (!config.node_selector.is_empty()).then(|| config.node_selector.clone());
    let tolerations =
        (!config.tolerations.is_empty()).then(|| config.tolerations.clone());

    Pod {
        metadata: ObjectMeta {
            name: Some(pod_name),
            namespace: Some(config.namespace.clone()),
            labels: Some(lbls),
            owner_references: owner.map(|o| vec![o]),
            ..ObjectMeta::default()
        },
        spec: Some(PodSpec {
            restart_policy: Some("Never".to_string()),
            containers: vec![container],
            volumes: Some(vec![shm_volume]),
            node_selector,
            tolerations,
            ..PodSpec::default()
        }),
        ..Pod::default()
    }
}

/// Build the ClusterIP Service fronting the backing-service Pod.
pub fn build_backing_service_service(
    config: &KubernetesConfig,
    spec: &BackingServiceSpec,
    instance_id: &str,
    task_run_id: &str,
    owner: Option<OwnerReference>,
) -> Service {
    let (_, service_name) = backing_service_names(instance_id);
    let lbls = labels(instance_id, task_run_id, &spec.service_type);
    Service {
        metadata: ObjectMeta {
            name: Some(service_name),
            namespace: Some(config.namespace.clone()),
            labels: Some(lbls.clone()),
            owner_references: owner.map(|o| vec![o]),
            ..ObjectMeta::default()
        },
        spec: Some(ServiceSpec {
            // Select the Pod by its instance-id label.
            selector: Some(BTreeMap::from([(
                LABEL_INSTANCE_ID.to_string(),
                sanitize_id(instance_id),
            )])),
            ports: Some(vec![ServicePort {
                port: spec.port,
                target_port: Some(IntOrString::Int(spec.port)),
                ..ServicePort::default()
            }]),
            cluster_ip: None,
            ..ServiceSpec::default()
        }),
        ..Service::default()
    }
}

/// Render a preset's connection template into a concrete URL the worker uses.
/// Replaces `{host}` with the in-cluster DNS name and `{port}` with the port.
pub fn render_conn_string(
    conn_template: &str,
    config: &KubernetesConfig,
    instance_id: &str,
    port: i32,
) -> String {
    let (_, service_name) = backing_service_names(instance_id);
    let host = format!("{service_name}.{}.svc.cluster.local", config.namespace);
    conn_template
        .replace("{host}", &host)
        .replace("{port}", &port.to_string())
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
        }
    }

    #[test]
    fn pod_and_service_share_instance_selector() {
        let cfg = KubernetesConfig::for_testing();
        let pod = build_backing_service_pod(&cfg, &spec(), "inst-1", "tr-1", None);
        let svc = build_backing_service_service(&cfg, &spec(), "inst-1", "tr-1", None);
        let pod_lbl = pod.metadata.labels.unwrap();
        let sel = svc.spec.unwrap().selector.unwrap();
        assert_eq!(pod_lbl.get(LABEL_INSTANCE_ID), sel.get(LABEL_INSTANCE_ID));
        let c = &pod.spec.unwrap().containers[0];
        assert_eq!(c.image.as_deref(), Some("postgres:18-alpine"));
        assert_eq!(c.ports.as_ref().unwrap()[0].container_port, 5432);
    }

    #[test]
    fn conn_string_renders_dns_host() {
        let cfg = KubernetesConfig::for_testing();
        let url = render_conn_string(
            "postgres://postgres:postgres@{host}:{port}/app_test",
            &cfg,
            "inst-1",
            5432,
        );
        assert!(url.contains(".svc.cluster.local:5432"), "got {url}");
        assert!(url.starts_with("postgres://postgres:postgres@djinn-svc-inst-1."), "got {url}");
    }
}
