//! Scheduler-effective PodSet cost is derived from the rendered Kubernetes API
//! object, never from a parallel copy of task-run configuration values.

use std::collections::BTreeMap;

use djinn_k8s::capacity::{
    CapacityError, CpuMillicores, MemoryBytes, PodCount, ResourceVector, podset_cost_from_pod_spec,
};
use djinn_k8s::config::KubernetesConfig;
use djinn_k8s::job::build_task_run_job;
use djinn_k8s::sidecar::BackingServiceSpec;
use djinn_runtime::RoleKind;
use k8s_openapi::api::core::v1::{Container, PodSpec, ResourceRequirements};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use uuid::Uuid;

fn postgres_service() -> BackingServiceSpec {
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

fn requested(name: &str, cpu: &str, memory: &str) -> Container {
    Container {
        name: name.into(),
        resources: Some(ResourceRequirements {
            requests: Some(BTreeMap::from([
                ("cpu".into(), Quantity(cpu.into())),
                ("memory".into(), Quantity(memory.into())),
            ])),
            ..ResourceRequirements::default()
        }),
        ..Container::default()
    }
}

fn vector(cpu: i64, memory: i64) -> ResourceVector {
    ResourceVector {
        cpu: CpuMillicores::new(cpu).unwrap(),
        memory: MemoryBytes::new(memory).unwrap(),
        pods: PodCount::new(1).unwrap(),
    }
}

#[test]
fn podset_cost_from_rendered_job() {
    let config = KubernetesConfig::for_testing();
    let service = postgres_service();
    let job = build_task_run_job(
        &config,
        &Uuid::nil(),
        "capacity-fixture-project",
        "capacity-fixture-secret",
        "registry.example/djinn:capacity-fixture",
        std::slice::from_ref(&service),
        None,
        false,
        Some(RoleKind::Worker),
    );
    let pod = job
        .spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .expect("real task-run builder returns a PodSpec");

    // This assertion consumes only the rendered PodSpec. It deliberately does
    // not reconstruct a total from KubernetesConfig or BackingServiceSpec.
    assert_eq!(
        podset_cost_from_pod_spec(pod),
        Ok(vector(1_150, 2_368 * 1024 * 1024)),
    );
}

#[test]
fn podset_cost_applies_sidecar_sum_and_ordinary_init_max_per_dimension() {
    let mut sidecar = requested("native-sidecar", "150m", "128Mi");
    sidecar.restart_policy = Some("Always".into());
    let pod = PodSpec {
        containers: vec![requested("worker", "200m", "256Mi")],
        init_containers: Some(vec![
            sidecar,
            // CPU exceeds steady state (350m), while memory does not (384Mi).
            requested("prepare-cpu", "500m", "64Mi"),
            // Memory exceeds steady state, while CPU does not.
            requested("prepare-memory", "100m", "1Gi"),
        ]),
        ..PodSpec::default()
    };

    assert_eq!(
        podset_cost_from_pod_spec(&pod),
        Ok(vector(500, 1024 * 1024 * 1024)),
    );
}

#[test]
fn podset_cost_rejects_missing_malformed_and_overflowing_requests() {
    let missing_cpu = PodSpec {
        containers: vec![Container {
            name: "missing-cpu".into(),
            resources: Some(ResourceRequirements {
                requests: Some(BTreeMap::from([("memory".into(), Quantity("1Mi".into()))])),
                ..ResourceRequirements::default()
            }),
            ..Container::default()
        }],
        ..PodSpec::default()
    };
    assert_eq!(
        podset_cost_from_pod_spec(&missing_cpu),
        Err(CapacityError::MissingContainerCpuRequest),
    );

    let malformed_memory = PodSpec {
        containers: vec![requested("bad-memory", "1", "1G")],
        ..PodSpec::default()
    };
    assert_eq!(
        podset_cost_from_pod_spec(&malformed_memory),
        Err(CapacityError::MalformedContainerMemoryRequest),
    );

    let overflow = PodSpec {
        containers: vec![
            requested("max", "9223372036854775807m", "1Mi"),
            requested("one-more", "1m", "1Mi"),
        ],
        ..PodSpec::default()
    };
    assert_eq!(
        podset_cost_from_pod_spec(&overflow),
        Err(CapacityError::Overflow)
    );
}
