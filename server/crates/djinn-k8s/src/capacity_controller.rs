//! Identity-fenced wire decisions for the derived-capacity controller.
//!
//! Observation and actuation are kept separate so every ambiguous read has a
//! closed, testable outcome and cannot accidentally become a PATCH.

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Node, Pod, PodSpec};
use kube::{
    Api, Client,
    api::{ApiResource, DynamicObject, GroupVersionKind, ListParams, Patch, PatchParams},
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::capacity::{
    CapacityOutcome, CpuMillicores, DerivedCapacity, FailSafeCapacity, MemoryBytes, PodCount,
    ResourceVector, ResourceVectorDerivationInputs, ResourceVectorInput, ResourceVectorOutcome,
    derive_resource_vector, podset_cost_from_pod_spec,
};
use crate::capacity_damping::{BindingQuota, CapacityVector};
use crate::capacity_damping::{CapacityDamper, SampleKind};

pub const QUOTA_OWNER_LABEL: &str = "djinn.io/quota-owner";
pub const DERIVED_CAPACITY_OWNER: &str = "derived-capacity";
pub const BINDING_RESOURCE_ANNOTATION: &str = "djinn.io/binding-resource";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeObservation {
    pub name: String,
    pub selector_matches: bool,
    pub ready: bool,
    pub unschedulable: bool,
    pub terminating: bool,
    pub allocatable_cpu: Option<CpuMillicores>,
    pub allocatable_memory: Option<MemoryBytes>,
    pub allocatable_pods: Option<PodCount>,
}

/// Checked aggregate of all schedulable, selected nodes. Names accompany the
/// sum so protected workload accounting cannot charge another pool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EligibleNodeCapacity {
    pub allocatable: ResourceVector,
    pub names: BTreeSet<String>,
}

fn pod_resource_vector(pod: &Pod) -> Option<ResourceVector> {
    let spec = pod.spec.as_ref()?;
    let request = |container: &k8s_openapi::api::core::v1::Container| {
        let requests = container.resources.as_ref()?.requests.as_ref()?;
        Some((
            cpu_quantity(&requests.get("cpu")?.0)?,
            memory_quantity(&requests.get("memory")?.0)?,
        ))
    };
    let sum = |values: Vec<(CpuMillicores, MemoryBytes)>| {
        values
            .into_iter()
            .try_fold((0_i64, 0_i64), |(cpu, memory), (next_cpu, next_memory)| {
                Some((
                    cpu.checked_add(next_cpu.get())?,
                    memory.checked_add(next_memory.get())?,
                ))
            })
    };
    let regular = sum(spec
        .containers
        .iter()
        .map(request)
        .collect::<Option<Vec<_>>>()?)?;
    let init = spec
        .init_containers
        .iter()
        .flatten()
        .map(request)
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .fold((0_i64, 0_i64), |maximum, value| {
            (maximum.0.max(value.0.get()), maximum.1.max(value.1.get()))
        });
    Some(ResourceVector {
        cpu: CpuMillicores::new(regular.0.max(init.0)).ok()?,
        memory: MemoryBytes::new(regular.1.max(init.1)).ok()?,
        pods: PodCount::new(1).ok()?,
    })
}

/// Fold only labeled protected Pods actually assigned to an eligible node.
pub fn protected_requests_on_nodes(
    pods: &[Pod],
    eligible_node_names: &BTreeSet<String>,
) -> Result<ResourceVector, ConservativeReason> {
    pods.iter()
        .filter(|pod| {
            pod.metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("djinn.io/capacity-reserved"))
                .is_some_and(|value| value == "true")
                && pod
                    .spec
                    .as_ref()
                    .and_then(|spec| spec.node_name.as_ref())
                    .is_some_and(|node| eligible_node_names.contains(node))
        })
        .try_fold(ResourceVector::ZERO, |sum, pod| {
            sum.checked_add(pod_resource_vector(pod).ok_or(ConservativeReason::ObservationFailed)?)
                .map_err(|_| ConservativeReason::ObservationFailed)
        })
}

fn memory_quantity(value: &str) -> Option<MemoryBytes> {
    let (number, multiplier) = [
        ("Ki", 1_i64 << 10),
        ("Mi", 1_i64 << 20),
        ("Gi", 1_i64 << 30),
        ("Ti", 1_i64 << 40),
        ("Pi", 1_i64 << 50),
        ("Ei", 1_i64 << 60),
        ("K", 1_000),
        ("M", 1_000_000),
        ("G", 1_000_000_000),
        ("T", 1_000_000_000_000),
        ("P", 1_000_000_000_000_000),
        ("E", 1_000_000_000_000_000_000),
    ]
    .into_iter()
    .find_map(|(suffix, multiplier)| value.strip_suffix(suffix).map(|n| (n, multiplier)))
    .unwrap_or((value, 1));
    MemoryBytes::new(number.parse::<i64>().ok()?.checked_mul(multiplier)?).ok()
}

fn pod_quantity(value: &str) -> Option<PodCount> {
    PodCount::new(value.parse().ok()?).ok()
}

/// Convert a Kubernetes Node into the additive observation seam. A missing
/// Ready condition is deliberately not treated as Ready.
pub fn observe_node(node: &Node, selector_key: &str, selector_value: &str) -> NodeObservation {
    let allocatable = node
        .status
        .as_ref()
        .and_then(|status| status.allocatable.as_ref());
    NodeObservation {
        name: node.metadata.name.clone().unwrap_or_default(),
        selector_matches: node
            .metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get(selector_key))
            .is_some_and(|value| value == selector_value),
        ready: node
            .status
            .as_ref()
            .and_then(|status| status.conditions.as_ref())
            .is_some_and(|conditions| {
                conditions
                    .iter()
                    .any(|c| c.type_ == "Ready" && c.status == "True")
            }),
        unschedulable: node.spec.as_ref().and_then(|spec| spec.unschedulable) == Some(true),
        terminating: node.metadata.deletion_timestamp.is_some(),
        allocatable_cpu: allocatable
            .and_then(|resources| resources.get("cpu"))
            .and_then(|quantity| cpu_quantity(&quantity.0)),
        allocatable_memory: allocatable
            .and_then(|resources| resources.get("memory"))
            .and_then(|quantity| memory_quantity(&quantity.0)),
        allocatable_pods: allocatable
            .and_then(|resources| resources.get("pods"))
            .and_then(|quantity| pod_quantity(&quantity.0)),
    }
}

/// Derive legacy controller actuation values from complete vector observations
/// and the actual rendered build-capable task Pod. Any omitted or malformed
/// dimension becomes the established conservative/no-mutation outcome.
pub fn derive_capacity_from_rendered_build_job(
    allocatable: ResourceVector,
    protected: ResourceVector,
    headroom: ResourceVector,
    build_pod: &PodSpec,
    compile_cost: CpuMillicores,
    fail_safe: FailSafeCapacity,
) -> CapacityOutcome {
    let podset_cost = match podset_cost_from_pod_spec(build_pod) {
        Ok(cost) => cost,
        Err(reason) => {
            return CapacityOutcome::Conservative {
                capacity: fail_safe,
                reason,
            };
        }
    };
    let vector = match derive_resource_vector(ResourceVectorDerivationInputs {
        protected_population_complete: true,
        allocatable: ResourceVectorInput::complete(allocatable),
        protected: ResourceVectorInput::complete(protected),
        headroom: ResourceVectorInput::complete(headroom),
        podset_cost: ResourceVectorInput::complete(podset_cost),
    }) {
        ResourceVectorOutcome::Derived(vector) => vector,
        ResourceVectorOutcome::Conservative { reason, .. } => {
            return CapacityOutcome::Conservative {
                capacity: fail_safe,
                reason,
            };
        }
    };
    let Some(binding_cpu) = vector
        .admitted_podsets
        .get()
        .checked_mul(podset_cost.cpu.get())
    else {
        return CapacityOutcome::Conservative {
            capacity: fail_safe,
            reason: crate::capacity::CapacityError::Overflow,
        };
    };
    if compile_cost.get() == 0 {
        return CapacityOutcome::Conservative {
            capacity: fail_safe,
            reason: crate::capacity::CapacityError::ZeroCost,
        };
    }
    CapacityOutcome::Derived(DerivedCapacity {
        pods: vector.admitted_podsets.get(),
        binding_cpu: CpuMillicores::new(binding_cpu).expect("checked non-negative product"),
        compile_slots: vector.raw.cpu.get() / compile_cost.get(),
    })
}

fn rendered_pod_spec(job: &Job) -> Option<&PodSpec> {
    job.spec.as_ref()?.template.spec.as_ref()
}

/// Sum all eligible node vectors. A node that otherwise belongs to the pool
/// but lacks a dimension is invalid, rather than a zero-sized node.
pub fn aggregate_eligible_nodes(
    nodes: &[NodeObservation],
) -> Result<EligibleNodeCapacity, ConservativeReason> {
    let mut result = EligibleNodeCapacity {
        allocatable: ResourceVector::ZERO,
        names: BTreeSet::new(),
    };
    for node in nodes.iter().filter(|node| {
        node.selector_matches && node.ready && !node.unschedulable && !node.terminating
    }) {
        let vector = ResourceVectorInput {
            cpu: node.allocatable_cpu,
            memory: node.allocatable_memory,
            pods: node.allocatable_pods,
        };
        let complete = match (vector.cpu, vector.memory, vector.pods) {
            (Some(cpu), Some(memory), Some(pods)) if !node.name.is_empty() => {
                ResourceVector { cpu, memory, pods }
            }
            _ => return Err(ConservativeReason::ConservativeNodeIdentity),
        };
        result.allocatable = result
            .allocatable
            .checked_add(complete)
            .map_err(|_| ConservativeReason::ConservativeNodeIdentity)?;
        result.names.insert(node.name.clone());
    }
    (!result.names.is_empty())
        .then_some(result)
        .ok_or(ConservativeReason::ConservativeNodeIdentity)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueResource {
    pub name: String,
    pub nominal_quota: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueObservation {
    pub name: String,
    pub resource_version: String,
    pub owner: Option<String>,
    pub binding_resource: Option<String>,
    pub resources: Vec<QueueResource>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConservativeReason {
    ConservativeNodeIdentity,
    QueueNameMismatch,
    QueueOwnerMismatch,
    UnknownBindingResource,
    AmbiguousBindingResource,
    ObservationFailed,
    CompileBoundDisarmed,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ActuationDecision {
    Patch {
        vector: CapacityVector,
        patch: Value,
    },
    NoMutation {
        compile_slots: i64,
        reason: ConservativeReason,
    },
    Noop {
        vector: CapacityVector,
    },
}

#[derive(Clone, Debug)]
pub struct CapacityControllerConfig {
    pub queue_name: String,
    pub node_selector_key: String,
    pub node_selector_value: String,
    pub idle_cost: CpuMillicores,
    pub compile_cost: CpuMillicores,
    /// Explicit resources retained from the eligible aggregate before admitting
    /// build-capable task PodSets. Every dimension is required.
    pub headroom: ResourceVector,
    /// The real rendered build-capable task Job used for scheduler-effective
    /// PodSet accounting on every observation tick.
    pub build_job: Job,
    pub fail_safe: FailSafeCapacity,
    pub expected_protected_pods: usize,
}

impl CapacityControllerConfig {
    pub fn from_env() -> Option<Self> {
        if std::env::var("DJINN_CAPACITY_ENABLED").ok().as_deref() != Some("true") {
            return None;
        }
        let parse_cpu = |name: &str| {
            std::env::var(name)
                .ok()?
                .strip_suffix('m')?
                .parse()
                .ok()
                .and_then(|v| CpuMillicores::new(v).ok())
        };
        let headroom = ResourceVector {
            cpu: parse_cpu("DJINN_CAPACITY_HEADROOM_CPU")?,
            memory: memory_quantity(&std::env::var("DJINN_CAPACITY_HEADROOM_MEMORY").ok()?)?,
            pods: pod_quantity(&std::env::var("DJINN_CAPACITY_HEADROOM_PODS").ok()?)?,
        };
        let build_job = controller_build_job();
        Some(Self {
            queue_name: std::env::var("DJINN_CAPACITY_QUEUE_NAME").ok()?,
            node_selector_key: std::env::var("DJINN_CAPACITY_NODE_SELECTOR_KEY").ok()?,
            node_selector_value: std::env::var("DJINN_CAPACITY_NODE_SELECTOR_VALUE").ok()?,
            idle_cost: parse_cpu("DJINN_CAPACITY_IDLE_CPU")?,
            compile_cost: parse_cpu("DJINN_CAPACITY_COMPILE_CPU")?,
            headroom,
            build_job,
            fail_safe: FailSafeCapacity {
                pods: std::env::var("DJINN_CAPACITY_FAIL_SAFE_PODS")
                    .ok()?
                    .parse()
                    .ok()?,
                compile_slots: std::env::var("DJINN_CAPACITY_FAIL_SAFE_COMPILE_SLOTS")
                    .ok()?
                    .parse()
                    .ok()?,
            },
            expected_protected_pods: std::env::var("DJINN_CAPACITY_EXPECTED_PROTECTED_PODS")
                .ok()?
                .parse()
                .ok()?,
        })
    }
}

fn controller_build_job() -> Job {
    let render_config = crate::config::KubernetesConfig::from_env();
    crate::job::build_task_run_job(
        &render_config,
        &uuid::Uuid::nil(),
        "capacity-controller",
        "capacity-controller",
        &render_config.image,
        &[],
        None,
        false,
        Some(djinn_runtime::RoleKind::Worker),
    )
}

fn cpu_quantity(value: &str) -> Option<CpuMillicores> {
    if let Some(m) = value.strip_suffix('m') {
        return CpuMillicores::new(m.parse().ok()?).ok();
    }
    CpuMillicores::new(value.parse::<i64>().ok()?.checked_mul(1_000)?).ok()
}

fn queue_api(client: Client) -> Api<DynamicObject> {
    let resource = ApiResource::from_gvk(&GroupVersionKind::gvk(
        "kueue.x-k8s.io",
        "v1beta1",
        "ClusterQueue",
    ));
    Api::all_with(client, &resource)
}

/// Leader-owned, 30-second observation and actuation loop. Any read/parsing or
/// identity failure publishes fail-safe K and performs no queue write.
pub async fn run_capacity_controller(
    client: Client,
    config: CapacityControllerConfig,
    compile_bound_armed: Arc<dyn Fn() -> bool + Send + Sync>,
    snapshots: watch::Sender<CapacityVector>,
) {
    let nodes: Api<Node> = Api::all(client.clone());
    let pods: Api<Pod> = Api::all(client.clone());
    let queues = queue_api(client);
    let selector = format!(
        "{}={}",
        config.node_selector_key, config.node_selector_value
    );
    let mut damper: Option<CapacityDamper> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    loop {
        tick.tick().await;
        let observed = async {
            let node_list = nodes
                .list(&ListParams::default().labels(&selector))
                .await
                .ok()?;
            let node_observations: Vec<_> = node_list
                .items
                .into_iter()
                .map(|node| {
                    observe_node(
                        &node,
                        &config.node_selector_key,
                        &config.node_selector_value,
                    )
                })
                .collect();
            let nodes = aggregate_eligible_nodes(&node_observations).ok()?;
            let protected = pods
                .list(&ListParams::default().labels("djinn.io/capacity-reserved=true"))
                .await
                .ok()?;
            if protected.items.len() < config.expected_protected_pods {
                return None;
            }
            let protected = protected_requests_on_nodes(&protected.items, &nodes.names).ok()?;
            let queue = queues.get(&config.queue_name).await.ok()?;
            let data = queue.data;
            let resources = data
                .pointer("/spec/resourceGroups/0/flavors/0/resources")?
                .as_array()?;
            let observation = QueueObservation {
                name: config.queue_name.clone(),
                resource_version: queue.metadata.resource_version?,
                owner: queue
                    .metadata
                    .labels
                    .as_ref()
                    .and_then(|m| m.get(QUOTA_OWNER_LABEL))
                    .cloned(),
                binding_resource: queue
                    .metadata
                    .annotations
                    .as_ref()
                    .and_then(|m| m.get(BINDING_RESOURCE_ANNOTATION))
                    .cloned(),
                resources: resources
                    .iter()
                    .map(|r| {
                        Some(QueueResource {
                            name: r.get("name")?.as_str()?.into(),
                            nominal_quota: r.get("nominalQuota")?.as_str()?.into(),
                        })
                    })
                    .collect::<Option<Vec<_>>>()?,
            };
            let capacity = derive_capacity_from_rendered_build_job(
                nodes.allocatable,
                protected,
                config.headroom,
                rendered_pod_spec(&config.build_job)?,
                config.compile_cost,
                config.fail_safe,
            );
            let CapacityOutcome::Derived(raw) = capacity else {
                return None;
            };
            let binding = binding_for(&observation, &config.queue_name, raw).ok()?;
            Some((
                observation,
                capacity,
                CapacityVector {
                    binding,
                    compile_slots: raw.compile_slots,
                },
            ))
        }
        .await;

        let Some((queue, capacity, raw)) = observed else {
            let current = damper
                .as_mut()
                .map(|d| d.reset_after_error(config.fail_safe.compile_slots, Instant::now()))
                .unwrap_or(CapacityVector {
                    binding: BindingQuota::Pods(config.fail_safe.pods),
                    compile_slots: config.fail_safe.compile_slots,
                });
            let _ = snapshots.send(current);
            continue;
        };
        let now = Instant::now();
        let live = queue
            .resources
            .iter()
            .find(|r| Some(r.name.as_str()) == queue.binding_resource.as_deref())
            .and_then(|r| {
                r.nominal_quota
                    .strip_suffix('m')
                    .unwrap_or(&r.nominal_quota)
                    .parse()
                    .ok()
            })
            .unwrap_or(config.fail_safe.pods);
        let damper = damper.get_or_insert_with(|| {
            CapacityDamper::new(
                match raw.binding {
                    BindingQuota::Pods(_) => BindingQuota::Pods(live),
                    BindingQuota::CpuMillicores(_) => BindingQuota::CpuMillicores(live),
                },
                config.fail_safe.compile_slots,
                now,
            )
        });
        let damped = damper.observe(raw, SampleKind::Periodic, now);
        let _ = snapshots.send(damped);
        if let ActuationDecision::Patch { patch, .. } = patch_decision(
            &queue,
            &config.queue_name,
            capacity,
            damped,
            compile_bound_armed(),
            config.fail_safe,
        ) {
            let params = PatchParams::default();
            if let Err(error) = queues
                .patch(
                    &config.queue_name,
                    &params,
                    &Patch::Json::<()>(
                        serde_json::from_value(patch).expect("valid internal JSON patch"),
                    ),
                )
                .await
            {
                tracing::warn!(%error, "capacity controller: ClusterQueue PATCH failed");
            }
        }
    }
}

pub fn select_node(nodes: &[NodeObservation]) -> Result<&NodeObservation, ConservativeReason> {
    let matches: Vec<_> = nodes
        .iter()
        .filter(|n| n.selector_matches && !n.terminating && n.allocatable_cpu.is_some())
        .collect();
    if matches.len() == 1 {
        Ok(matches[0])
    } else {
        Err(ConservativeReason::ConservativeNodeIdentity)
    }
}

pub fn binding_for(
    queue: &QueueObservation,
    configured_name: &str,
    derived: DerivedCapacity,
) -> Result<BindingQuota, ConservativeReason> {
    if queue.name != configured_name {
        return Err(ConservativeReason::QueueNameMismatch);
    }
    if queue.owner.as_deref() != Some(DERIVED_CAPACITY_OWNER) {
        return Err(ConservativeReason::QueueOwnerMismatch);
    }
    let resource = match queue.binding_resource.as_deref() {
        Some("pods") => BindingQuota::Pods(derived.pods),
        Some("cpu") => BindingQuota::CpuMillicores(derived.binding_cpu.get()),
        _ => return Err(ConservativeReason::UnknownBindingResource),
    };
    let name = match resource {
        BindingQuota::Pods(_) => "pods",
        BindingQuota::CpuMillicores(_) => "cpu",
    };
    if queue.resources.iter().filter(|r| r.name == name).count() != 1 {
        return Err(ConservativeReason::AmbiguousBindingResource);
    }
    Ok(resource)
}

/// Produce the only supported ClusterQueue mutation: a JSON Patch guarded by
/// the observed resourceVersion and targeting exactly one annotated resource.
pub fn patch_decision(
    queue: &QueueObservation,
    configured_name: &str,
    capacity: CapacityOutcome,
    damped: CapacityVector,
    compile_bound_armed: bool,
    fail_safe: FailSafeCapacity,
) -> ActuationDecision {
    let CapacityOutcome::Derived(derived) = capacity else {
        return ActuationDecision::NoMutation {
            compile_slots: fail_safe.compile_slots,
            reason: ConservativeReason::ObservationFailed,
        };
    };
    let Ok(raw_binding) = binding_for(queue, configured_name, derived) else {
        return ActuationDecision::NoMutation {
            compile_slots: fail_safe.compile_slots,
            reason: binding_for(queue, configured_name, derived).unwrap_err(),
        };
    };
    let raw_value = match raw_binding {
        BindingQuota::Pods(v) | BindingQuota::CpuMillicores(v) => v,
    };
    let damped_value = match damped.binding {
        BindingQuota::Pods(v) | BindingQuota::CpuMillicores(v) => v,
    };
    if !raw_binding.same_resource(damped.binding) {
        return ActuationDecision::NoMutation {
            compile_slots: fail_safe.compile_slots,
            reason: ConservativeReason::AmbiguousBindingResource,
        };
    }
    // The compile-bound precondition gates widening relative to the live
    // binding quota.  Comparing CPU millicores with fail_safe.pods mixed units
    // after the oru9 surface migration and could authorize an unsafe raise.
    let live_value = queue
        .resources
        .iter()
        .find(|resource| {
            resource.name
                == match raw_binding {
                    BindingQuota::Pods(_) => "pods",
                    BindingQuota::CpuMillicores(_) => "cpu",
                }
        })
        .and_then(|resource| {
            resource
                .nominal_quota
                .strip_suffix('m')
                .unwrap_or(&resource.nominal_quota)
                .parse::<i64>()
                .ok()
        })
        .unwrap_or(raw_value);
    if !compile_bound_armed && damped_value > live_value {
        return ActuationDecision::NoMutation {
            compile_slots: fail_safe.compile_slots,
            reason: ConservativeReason::CompileBoundDisarmed,
        };
    }
    let resource_name = match damped.binding {
        BindingQuota::Pods(_) => "pods",
        BindingQuota::CpuMillicores(_) => "cpu",
    };
    let Some(index) = queue.resources.iter().position(|r| r.name == resource_name) else {
        return ActuationDecision::NoMutation {
            compile_slots: fail_safe.compile_slots,
            reason: ConservativeReason::AmbiguousBindingResource,
        };
    };
    let quantity = match damped.binding {
        BindingQuota::Pods(v) => v.to_string(),
        BindingQuota::CpuMillicores(v) => format!("{v}m"),
    };
    if queue.resources[index].nominal_quota == quantity {
        return ActuationDecision::Noop { vector: damped };
    }
    ActuationDecision::Patch {
        vector: damped,
        patch: json!([
            {"op":"test", "path":"/metadata/resourceVersion", "value":queue.resource_version},
            {"op":"replace", "path":format!("/spec/resourceGroups/0/flavors/0/resources/{index}/nominalQuota"), "value":quantity}
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn resources(cpu: i64, memory: i64, pods: i64) -> ResourceVector {
        ResourceVector {
            cpu: CpuMillicores::new(cpu).unwrap(),
            memory: MemoryBytes::new(memory).unwrap(),
            pods: PodCount::new(pods).unwrap(),
        }
    }
    fn rendered_job(role: djinn_runtime::RoleKind) -> Job {
        let config = crate::config::KubernetesConfig::for_testing();
        crate::job::build_task_run_job(
            &config,
            &uuid::Uuid::nil(),
            "capacity-controller",
            "capacity-controller",
            "registry.example/djinn:capacity",
            &[],
            None,
            false,
            Some(role),
        )
    }
    fn derived() -> DerivedCapacity {
        DerivedCapacity {
            pods: 10,
            binding_cpu: CpuMillicores::new(7_500).unwrap(),
            compile_slots: 2,
        }
    }
    fn queue(binding: &str) -> QueueObservation {
        QueueObservation {
            name: "djinn-kueue".into(),
            resource_version: "42".into(),
            owner: Some(DERIVED_CAPACITY_OWNER.into()),
            binding_resource: Some(binding.into()),
            resources: vec![
                QueueResource {
                    name: "pods".into(),
                    nominal_quota: "3".into(),
                },
                QueueResource {
                    name: "cpu".into(),
                    nominal_quota: "10k".into(),
                },
                QueueResource {
                    name: "memory".into(),
                    nominal_quota: "100Ti".into(),
                },
            ],
        }
    }
    fn outcome() -> CapacityOutcome {
        CapacityOutcome::Derived(derived())
    }
    fn safe() -> FailSafeCapacity {
        FailSafeCapacity {
            pods: 3,
            compile_slots: 2,
        }
    }

    #[test]
    fn memory_binding_boundary() {
        let worker = rendered_job(djinn_runtime::RoleKind::Worker);
        let light = rendered_job(djinn_runtime::RoleKind::Planner);
        let worker_pod = rendered_pod_spec(&worker).unwrap();
        let light_pod = rendered_pod_spec(&light).unwrap();
        let worker_cost = podset_cost_from_pod_spec(worker_pod).unwrap();
        let light_cost = podset_cost_from_pod_spec(light_pod).unwrap();
        let vps = resources(12_000, 48 * 1024 * 1024 * 1024, 110);
        let CapacityOutcome::Derived(cpu_bound) = derive_capacity_from_rendered_build_job(
            vps,
            resources(0, 0, 0),
            resources(0, 0, 0),
            worker_pod,
            CpuMillicores::new(2_800).unwrap(),
            safe(),
        ) else {
            panic!("complete VPS observation must derive")
        };
        assert_eq!(cpu_bound.pods, vps.cpu.get() / worker_cost.cpu.get());
        assert!(cpu_bound.pods < vps.memory.get() / worker_cost.memory.get());
        let two_gib_per_core = resources(12_000, 24 * 1024 * 1024 * 1024, 110);
        let CapacityOutcome::Derived(memory_bound) = derive_capacity_from_rendered_build_job(
            two_gib_per_core,
            resources(0, 0, 0),
            resources(0, 0, 0),
            light_pod,
            CpuMillicores::new(2_800).unwrap(),
            safe(),
        ) else {
            panic!("complete 2Gi/core observation must derive")
        };
        let expected = (two_gib_per_core.cpu.get() / light_cost.cpu.get())
            .min(two_gib_per_core.memory.get() / light_cost.memory.get())
            .min(two_gib_per_core.pods.get() / light_cost.pods.get());
        assert_eq!(memory_bound.pods, expected);
        assert_eq!(
            memory_bound.pods,
            two_gib_per_core.memory.get() / light_cost.memory.get()
        );
    }

    #[test]
    fn quota_controller_wire_decision_serializes_resource_typed_values() {
        let p = patch_decision(
            &queue("pods"),
            "djinn-kueue",
            outcome(),
            CapacityVector {
                binding: BindingQuota::Pods(10),
                compile_slots: 2,
            },
            true,
            safe(),
        );
        let ActuationDecision::Patch { patch, .. } = p else {
            panic!()
        };
        assert_eq!(patch[1]["value"], "10");
        let p = patch_decision(
            &queue("cpu"),
            "djinn-kueue",
            outcome(),
            CapacityVector {
                binding: BindingQuota::CpuMillicores(7_500),
                compile_slots: 2,
            },
            true,
            safe(),
        );
        let ActuationDecision::Patch { patch, .. } = p else {
            panic!()
        };
        assert_eq!(patch[1]["value"], "7500m");
    }

    #[tokio::test]
    async fn quota_controller_wire_records_exactly_one_fenced_patch_per_surface() {
        use crate::runtime_fixture::{RecordedApiserver, recording_client};

        for (surface, damped, expected) in [
            (
                "pods",
                CapacityVector {
                    binding: BindingQuota::Pods(10),
                    compile_slots: 2,
                },
                "\"value\":\"10\"",
            ),
            (
                "cpu",
                CapacityVector {
                    binding: BindingQuota::CpuMillicores(7_500),
                    compile_slots: 2,
                },
                "\"value\":\"7500m\"",
            ),
        ] {
            let decision = patch_decision(
                &queue(surface),
                "djinn-kueue",
                outcome(),
                damped,
                true,
                safe(),
            );
            let ActuationDecision::Patch { patch, .. } = decision else {
                panic!("derived fixture must patch")
            };
            let recorder = RecordedApiserver::new();
            let api = queue_api(recording_client(&recorder, "default"));
            let result = api
                .patch(
                    "djinn-kueue",
                    &PatchParams::default(),
                    &Patch::Json::<()>(serde_json::from_value(patch).unwrap()),
                )
                .await;
            assert!(result.is_err(), "fixture refuses after recording the wire");
            let mutations = recorder.mutations();
            assert_eq!(mutations.len(), 1);
            assert_eq!(mutations[0].method, "PATCH");
            assert!(mutations[0].path.ends_with("/clusterqueues/djinn-kueue"));
            assert!(mutations[0].body.contains("resourceVersion"));
            assert!(mutations[0].body.contains(expected));
            assert!(!mutations[0].body.contains("memory"));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn quota_controller_wire_drives_full_recorded_observation_and_actuation() {
        use crate::runtime_fixture::capacity_controller_cluster;
        for (surface, expected) in [("pods", "\"value\":\"10\""), ("cpu", "\"value\":\"7500m\"")] {
            let (client, recorder) = capacity_controller_cluster("default", surface);
            let config = CapacityControllerConfig {
                queue_name: "djinn-kueue".into(),
                node_selector_key: "kubernetes.io/hostname".into(),
                node_selector_value: "worker-1".into(),
                idle_cost: CpuMillicores::new(750).unwrap(),
                compile_cost: CpuMillicores::new(2_800).unwrap(),
                headroom: ResourceVector::ZERO,
                build_job: controller_build_job(),
                fail_safe: safe(),
                expected_protected_pods: 5,
            };
            let (tx, _rx) = watch::channel(CapacityVector {
                binding: BindingQuota::Pods(3),
                compile_slots: 2,
            });
            let task = tokio::spawn(run_capacity_controller(
                client,
                config,
                Arc::new(|| true),
                tx,
            ));
            for _ in 0..12 {
                tokio::time::advance(Duration::from_secs(30)).await;
                tokio::task::yield_now().await;
                if !recorder.mutations().is_empty() {
                    break;
                }
            }
            task.abort();
            let mutations = recorder.mutations();
            assert_eq!(mutations.len(), 1);
            assert!(mutations[0].body.contains(expected));
            assert!(mutations[0].body.contains("resourceVersion"));
            assert!(!mutations[0].body.contains("memory"));
        }
    }

    #[test]
    fn quota_controller_queue_selection_rejects_ambiguity_and_warm_owner() {
        let mut q = queue("pods");
        q.owner = Some("warm-borrow".into());
        assert_eq!(
            binding_for(&q, "djinn-kueue", derived()),
            Err(ConservativeReason::QueueOwnerMismatch)
        );
        let mut q = queue("pods");
        q.resources.push(QueueResource {
            name: "pods".into(),
            nominal_quota: "9".into(),
        });
        assert_eq!(
            binding_for(&q, "djinn-kueue", derived()),
            Err(ConservativeReason::AmbiguousBindingResource)
        );
        let mut q = queue("bogus");
        assert_eq!(
            binding_for(&q, "djinn-kueue", derived()),
            Err(ConservativeReason::UnknownBindingResource)
        );
        q.name = "other".into();
        assert_eq!(
            binding_for(&q, "djinn-kueue", derived()),
            Err(ConservativeReason::QueueNameMismatch)
        );
        let mut q = queue("pods");
        q.owner = None;
        assert_eq!(
            binding_for(&q, "djinn-kueue", derived()),
            Err(ConservativeReason::QueueOwnerMismatch)
        );
        let mut q = queue("pods");
        q.binding_resource = None;
        assert_eq!(
            binding_for(&q, "djinn-kueue", derived()),
            Err(ConservativeReason::UnknownBindingResource)
        );
        let mut q = queue("pods");
        q.resources.retain(|resource| resource.name != "pods");
        assert_eq!(
            binding_for(&q, "djinn-kueue", derived()),
            Err(ConservativeReason::AmbiguousBindingResource)
        );
        let mut partial_oru9 = queue("cpu");
        partial_oru9
            .resources
            .retain(|resource| resource.name != "cpu");
        assert_eq!(
            binding_for(&partial_oru9, "djinn-kueue", derived()),
            Err(ConservativeReason::AmbiguousBindingResource)
        );
        let mut warm_9cbn = queue("pods");
        warm_9cbn.name = "djinn-warm".into();
        warm_9cbn.owner = Some("warm-borrow".into());
        assert_eq!(
            binding_for(&warm_9cbn, "djinn-kueue", derived()),
            Err(ConservativeReason::QueueNameMismatch),
            "the additional warm queue cannot become the configured writer"
        );
    }

    #[test]
    fn quota_controller_node_selection_requires_one_stable_identity() {
        let node = |name: &str, matches| NodeObservation {
            name: name.into(),
            selector_matches: matches,
            ready: true,
            unschedulable: false,
            terminating: false,
            allocatable_cpu: Some(CpuMillicores::new(12_000).unwrap()),
            allocatable_memory: Some(MemoryBytes::new(48 << 30).unwrap()),
            allocatable_pods: Some(PodCount::new(110).unwrap()),
        };
        assert_eq!(select_node(&[node("a", true)]).unwrap().name, "a");
        assert!(select_node(&[]).is_err());
        assert!(select_node(&[node("a", true), node("b", true)]).is_err());
        assert!(select_node(&[node("a", false)]).is_err());
        let mut terminating = node("a", true);
        terminating.terminating = true;
        assert!(select_node(&[terminating]).is_err());
        let mut missing = node("a", true);
        missing.allocatable_cpu = None;
        assert!(select_node(&[missing]).is_err());

        let first = node("a", true);
        let mut second = node("b", true);
        second.allocatable_cpu = Some(CpuMillicores::new(48_000).unwrap());
        assert_eq!(
            select_node(&[first.clone(), second.clone()]),
            select_node(&[second, first]),
            "ambiguous selection is independent of API list order"
        );
    }

    #[test]
    fn quota_controller_precondition_never_widens_when_disarmed() {
        let decision = patch_decision(
            &queue("pods"),
            "djinn-kueue",
            outcome(),
            CapacityVector {
                binding: BindingQuota::Pods(10),
                compile_slots: 2,
            },
            false,
            safe(),
        );
        assert_eq!(
            decision,
            ActuationDecision::NoMutation {
                compile_slots: 2,
                reason: ConservativeReason::CompileBoundDisarmed
            }
        );
        let no_op = patch_decision(
            &queue("pods"),
            "djinn-kueue",
            outcome(),
            CapacityVector {
                binding: BindingQuota::Pods(3),
                compile_slots: 2,
            },
            false,
            safe(),
        );
        assert!(matches!(no_op, ActuationDecision::Noop { .. }));

        let mut high_live = queue("pods");
        high_live.resources[0].nominal_quota = "10".into();
        let shrink = patch_decision(
            &high_live,
            "djinn-kueue",
            outcome(),
            CapacityVector {
                binding: BindingQuota::Pods(3),
                compile_slots: 2,
            },
            false,
            safe(),
        );
        assert!(matches!(shrink, ActuationDecision::Patch { .. }));

        let wrong_type = patch_decision(
            &queue("cpu"),
            "djinn-kueue",
            outcome(),
            CapacityVector {
                binding: BindingQuota::Pods(10),
                compile_slots: 2,
            },
            true,
            safe(),
        );
        assert_eq!(
            wrong_type,
            ActuationDecision::NoMutation {
                compile_slots: 2,
                reason: ConservativeReason::AmbiguousBindingResource,
            }
        );
    }

    #[test]
    fn quota_controller_failsafe_decision_returns_no_mutation_on_conservative_input() {
        assert_eq!(cpu_quantity("bogus"), None);
        assert_eq!(cpu_quantity("-1m"), None);
        assert_eq!(cpu_quantity("9223372036854775807"), None);
        let conservative = CapacityOutcome::Conservative {
            capacity: safe(),
            reason: crate::capacity::CapacityError::IncompleteProtectedPopulation,
        };
        assert_eq!(
            patch_decision(
                &queue("pods"),
                "djinn-kueue",
                conservative,
                CapacityVector {
                    binding: BindingQuota::Pods(3),
                    compile_slots: 2
                },
                true,
                safe()
            ),
            ActuationDecision::NoMutation {
                compile_slots: 2,
                reason: ConservativeReason::ObservationFailed
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn quota_controller_failsafe_quota_damping_restart_read_failure_publishes_known_k_and_writes_nothing()
     {
        use crate::runtime_fixture::{RecordedApiserver, recording_client};

        let recorder = RecordedApiserver::new();
        let client = recording_client(&recorder, "default");
        let config = CapacityControllerConfig {
            queue_name: "djinn-kueue".into(),
            node_selector_key: "kubernetes.io/hostname".into(),
            node_selector_value: "worker-1".into(),
            idle_cost: CpuMillicores::new(750).unwrap(),
            compile_cost: CpuMillicores::new(2_800).unwrap(),
            headroom: ResourceVector::ZERO,
            build_job: controller_build_job(),
            fail_safe: safe(),
            expected_protected_pods: 5,
        };
        let (tx, mut rx) = watch::channel(CapacityVector {
            binding: BindingQuota::Pods(99),
            compile_slots: 99,
        });
        let controller = tokio::spawn(run_capacity_controller(
            client,
            config,
            Arc::new(|| true),
            tx,
        ));
        tokio::task::yield_now().await;
        rx.changed().await.unwrap();
        assert_eq!(
            *rx.borrow_and_update(),
            CapacityVector {
                binding: BindingQuota::Pods(3),
                compile_slots: 2,
            }
        );
        assert!(recorder.mutations().is_empty());
        controller.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn quota_controller_failsafe_protected_pod_failure_empty_and_malformed_selector() {
        use crate::runtime_fixture::{CapacityPods, capacity_controller_cluster_with_pods};
        for (pod_mode, selector_key) in [
            (CapacityPods::ReadFailure, "kubernetes.io/hostname"),
            (CapacityPods::Empty, "kubernetes.io/hostname"),
            (CapacityPods::Complete, "bad selector key"),
        ] {
            let (client, recorder) =
                capacity_controller_cluster_with_pods("default", "pods", pod_mode);
            let config = CapacityControllerConfig {
                queue_name: "djinn-kueue".into(),
                node_selector_key: selector_key.into(),
                node_selector_value: "worker-1".into(),
                idle_cost: CpuMillicores::new(750).unwrap(),
                compile_cost: CpuMillicores::new(2_800).unwrap(),
                headroom: ResourceVector::ZERO,
                build_job: controller_build_job(),
                fail_safe: safe(),
                expected_protected_pods: 5,
            };
            let (tx, mut rx) = watch::channel(CapacityVector {
                binding: BindingQuota::Pods(99),
                compile_slots: 99,
            });
            let task = tokio::spawn(run_capacity_controller(
                client,
                config,
                Arc::new(|| true),
                tx,
            ));
            tokio::task::yield_now().await;
            rx.changed().await.unwrap();
            assert_eq!(rx.borrow_and_update().compile_slots, 2);
            assert!(recorder.mutations().is_empty());
            task.abort();
        }
    }
}
