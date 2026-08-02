use std::collections::BTreeSet;

use djinn_k8s::{
    capacity::{CpuMillicores, MemoryBytes, PodCount, ResourceVector, SCIP_CAPACITY_FIXTURE},
    capacity_controller::{aggregate_eligible_nodes, observe_node, protected_requests_on_nodes},
};
use k8s_openapi::api::core::v1::{Node, Pod};
use serde_json::json;

fn node(name: &str, cpu: &str, memory: &str, pods: &str) -> Node {
    serde_json::from_value(json!({
        "apiVersion": "v1", "kind": "Node",
        "metadata": {"name": name, "labels": {"pool": "build"}},
        "spec": {},
        "status": {
            "conditions": [{"type": "Ready", "status": "True"}],
            "allocatable": {"cpu": cpu, "memory": memory, "pods": pods}
        }
    }))
    .unwrap()
}

#[test]
fn capacity_node_observation() {
    let mut not_ready = node("not-ready", "99", "99Gi", "99");
    not_ready
        .status
        .as_mut()
        .unwrap()
        .conditions
        .as_mut()
        .unwrap()[0]
        .status = "False".into();
    let mut cordoned = node("cordoned", "99", "99Gi", "99");
    cordoned.spec.as_mut().unwrap().unschedulable = Some(true);
    let mut terminating = node("terminating", "99", "99Gi", "99");
    terminating.metadata.deletion_timestamp =
        Some(serde_json::from_value(json!("2026-01-01T00:00:00Z")).unwrap());
    let mut mismatched = node("other-pool", "99", "99Gi", "99");
    mismatched
        .metadata
        .labels
        .as_mut()
        .unwrap()
        .insert("pool".into(), "other".into());

    let observed: Vec<_> = [
        node("small", "4", "8Gi", "20"),
        node("large", "7", "16Gi", "35"),
        node("memory", "2", "4Gi", "5"),
        not_ready,
        cordoned,
        terminating,
        mismatched,
    ]
    .iter()
    .map(|node| observe_node(node, "pool", "build"))
    .collect();
    let aggregate = aggregate_eligible_nodes(&observed).unwrap();
    assert_eq!(
        aggregate.names,
        BTreeSet::from(["large".into(), "memory".into(), "small".into()])
    );
    assert_eq!(
        aggregate.allocatable,
        ResourceVector {
            cpu: CpuMillicores::new(13_000).unwrap(),
            memory: MemoryBytes::new(28 << 30).unwrap(),
            pods: PodCount::new(60).unwrap(),
        },
        "sum is neither first-node nor max-node selection and retains pods"
    );

    let fixture = observe_node(&node("fixture", "12", "48Gi", "110"), "pool", "build");
    assert_eq!(
        aggregate_eligible_nodes(&[fixture])
            .unwrap()
            .allocatable
            .cpu,
        SCIP_CAPACITY_FIXTURE.allocatable
    );
    let mut missing = observed[0].clone();
    missing.allocatable_memory = None;
    assert!(
        aggregate_eligible_nodes(&[missing]).is_err(),
        "missing dimensions fail closed"
    );
    let malformed = observe_node(
        &node("malformed", "4", "not-a-quantity", "20"),
        "pool",
        "build",
    );
    assert!(
        aggregate_eligible_nodes(&[malformed]).is_err(),
        "malformed allocatable dimensions from a Node fail closed"
    );
    let mut overflow = observed[0].clone();
    overflow.allocatable_cpu = Some(CpuMillicores::new(i64::MAX).unwrap());
    assert!(
        aggregate_eligible_nodes(&[overflow, observed[1].clone()]).is_err(),
        "overflow fails closed"
    );
}

#[test]
fn capacity_protected_scoping() {
    let pod = |name: &str, node_name: &str| -> Pod {
        serde_json::from_value(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": name, "labels": {"djinn.io/capacity-reserved": "true"}},
            "spec": {"nodeName": node_name, "containers": [{
                "name": "main", "resources": {"requests": {"cpu": "1100m", "memory": "512Mi"}}
            }]}
        }))
        .unwrap()
    };
    let eligible = BTreeSet::from(["eligible-node".to_owned()]);
    let excluded = pod("protected-on-excluded", "excluded-node");
    assert_eq!(
        protected_requests_on_nodes(&[excluded], &eligible).unwrap(),
        ResourceVector::ZERO,
        "protected-on-excluded must not charge eligible-node capacity"
    );
    let included = pod("protected-on-eligible", "eligible-node");
    assert_eq!(
        protected_requests_on_nodes(&[included], &eligible).unwrap(),
        ResourceVector {
            cpu: CpuMillicores::new(1_100).unwrap(),
            memory: MemoryBytes::new(512 << 20).unwrap(),
            pods: PodCount::new(1).unwrap(),
        },
        "protected-on-eligible must charge eligible-node capacity"
    );
}
