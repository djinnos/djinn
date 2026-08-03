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
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

use crate::capacity::{
    CapacityOutcome, CpuMillicores, DerivedCapacity, FailSafeCapacity, MemoryBytes, PodCount,
    ResourceVector, ResourceVectorDerivationInputs, ResourceVectorInput, ResourceVectorOutcome,
    derive_resource_vector, podset_cost_from_pod_spec,
};
use crate::capacity_damping::{BindingQuota, CapacityVector};

pub const QUOTA_OWNER_LABEL: &str = "djinn.io/quota-owner";
pub const DERIVED_CAPACITY_OWNER: &str = "derived-capacity";
pub const BINDING_RESOURCE_ANNOTATION: &str = "djinn.io/binding-resource";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeObservation {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub selector_matches: bool,
    pub ready: bool,
    pub unschedulable: bool,
    pub terminating: bool,
    pub allocatable_cpu: Option<CpuMillicores>,
    pub allocatable_memory: Option<MemoryBytes>,
    pub allocatable_pods: Option<PodCount>,
}

/// Complete-vector actuation is intentionally distinct from the legacy damped
/// single-binding decision so it cannot be built from a partial target.
#[derive(Clone, Debug, PartialEq)]
pub enum FlavorActuationDecision {
    Patch { patch: Value },
    Noop,
    NoMutation { reason: ConservativeReason },
}

/// The configured ownership contract for one controller-owned ResourceFlavor.
/// The selector is an exact label conjunction, so later source adapters supply
/// effective labels without selecting a Node or NodePool API here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedFlavor {
    pub flavor_name: String,
    pub selector: Option<BTreeMap<String, String>>,
    pub static_fallback: ResourceVector,
}

/// Source-neutral observation of one complete capacity-bearing object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapacityObjectObservation {
    pub effective_labels: BTreeMap<String, String>,
    pub vector: ResourceVector,
}

/// Successful ownership derivation preserves the assigned aggregate so callers
/// can prove component-wise conservation across the resulting flavors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlavorOwnershipTargets {
    pub targets: Vec<FlavorQuotaTarget>,
    pub assigned_aggregate: ResourceVector,
}

/// Ownership failures retain static targets for the existing complete-vector
/// patch builder, rather than copying a shared dynamic aggregate to flavors.
#[derive(Clone, Debug, PartialEq)]
pub enum FlavorOwnershipDecision {
    Derived(FlavorOwnershipTargets),
    Fenced {
        reason: ConservativeReason,
        decision: FlavorActuationDecision,
    },
}

fn matches_flavor_selector(
    labels: &BTreeMap<String, String>,
    selector: &BTreeMap<String, String>,
) -> bool {
    selector
        .iter()
        .all(|(key, value)| labels.get(key) == Some(value))
}

fn static_flavor_targets(owned_flavors: &[OwnedFlavor]) -> Vec<FlavorQuotaTarget> {
    owned_flavors
        .iter()
        .map(|flavor| FlavorQuotaTarget {
            flavor_name: flavor.flavor_name.clone(),
            vector: flavor.static_fallback,
        })
        .collect()
}

/// Split the scheduler-effective aggregate between exclusively owned flavors.
/// The final flavor receives the remainder, preserving every derived component.
fn derived_flavor_targets(
    ownership: FlavorOwnershipTargets,
    derived: ResourceVector,
) -> Option<Vec<FlavorQuotaTarget>> {
    let aggregate = ownership.assigned_aggregate;
    let split = |total: i64, denominator: i64, weights: Vec<i64>| -> Option<Vec<i64>> {
        if denominator <= 0 {
            return None;
        }
        let last = weights.len().checked_sub(1)?;
        let mut remainder = total;
        weights
            .into_iter()
            .enumerate()
            .map(|(index, weight)| {
                let value = if index == last {
                    remainder
                } else {
                    i64::try_from(
                        (i128::from(total) * i128::from(weight)) / i128::from(denominator),
                    )
                    .ok()?
                };
                remainder = remainder.checked_sub(value)?;
                Some(value)
            })
            .collect()
    };
    let cpus = split(
        derived.cpu.get(),
        aggregate.cpu.get(),
        ownership
            .targets
            .iter()
            .map(|t| t.vector.cpu.get())
            .collect(),
    )?;
    let memories = split(
        derived.memory.get(),
        aggregate.memory.get(),
        ownership
            .targets
            .iter()
            .map(|t| t.vector.memory.get())
            .collect(),
    )?;
    let pods = split(
        derived.pods.get(),
        aggregate.pods.get(),
        ownership
            .targets
            .iter()
            .map(|t| t.vector.pods.get())
            .collect(),
    )?;
    ownership
        .targets
        .into_iter()
        .zip(cpus.into_iter().zip(memories).zip(pods))
        .map(|(target, ((cpu, memory), pods))| {
            Some(FlavorQuotaTarget {
                flavor_name: target.flavor_name,
                vector: ResourceVector {
                    cpu: CpuMillicores::new(cpu).ok()?,
                    memory: MemoryBytes::new(memory).ok()?,
                    pods: PodCount::new(pods).ok()?,
                },
            })
        })
        .collect()
}

fn validate_owned_flavors(owned_flavors: &[OwnedFlavor]) -> Result<(), ConservativeReason> {
    if owned_flavors.is_empty()
        || owned_flavors.iter().any(|flavor| {
            flavor.flavor_name.is_empty() || flavor.selector.as_ref().is_none_or(BTreeMap::is_empty)
        })
    {
        return Err(ConservativeReason::MissingFlavorOwnership);
    }
    if owned_flavors
        .iter()
        .map(|flavor| &flavor.flavor_name)
        .collect::<BTreeSet<_>>()
        .len()
        != owned_flavors.len()
        || owned_flavors
            .iter()
            .map(|flavor| flavor.selector.as_ref().expect("selectors validated above"))
            .collect::<BTreeSet<_>>()
            .len()
            != owned_flavors.len()
    {
        return Err(ConservativeReason::DuplicateFlavorOwnership);
    }
    Ok(())
}

/// Assign every observed vector to exactly one owned flavor. Unmatched objects
/// are outside controller ownership. Multiple matches never use API list order
/// as a tie breaker and instead fence the complete observation.
pub fn derive_flavor_ownership(
    owned_flavors: &[OwnedFlavor],
    objects: &[CapacityObjectObservation],
) -> Result<FlavorOwnershipTargets, ConservativeReason> {
    validate_owned_flavors(owned_flavors)?;
    let mut vectors = vec![ResourceVector::ZERO; owned_flavors.len()];
    let mut assigned_aggregate = ResourceVector::ZERO;
    for object in objects {
        let matches: Vec<_> = owned_flavors
            .iter()
            .enumerate()
            .filter(|(_, flavor)| {
                matches_flavor_selector(
                    &object.effective_labels,
                    flavor.selector.as_ref().expect("selectors validated above"),
                )
            })
            .map(|(index, _)| index)
            .collect();
        match matches.as_slice() {
            [] => {}
            [index] => {
                vectors[*index] = vectors[*index]
                    .checked_add(object.vector)
                    .map_err(|_| ConservativeReason::FlavorOwnershipOverflow)?;
                assigned_aggregate = assigned_aggregate
                    .checked_add(object.vector)
                    .map_err(|_| ConservativeReason::FlavorOwnershipOverflow)?;
            }
            _ => return Err(ConservativeReason::FlavorOwnershipAmbiguous),
        }
    }
    Ok(FlavorOwnershipTargets {
        targets: owned_flavors
            .iter()
            .zip(vectors)
            .map(|(flavor, vector)| FlavorQuotaTarget {
                flavor_name: flavor.flavor_name.clone(),
                vector,
            })
            .collect(),
        assigned_aggregate,
    })
}

/// Fence every ownership failure through the established named, fenced vector
/// patch builder using each owned flavor's own declared static fallback.
pub fn flavor_ownership_patch_decision(
    queue: &QueueObservation,
    configured_name: &str,
    owned_flavors: &[OwnedFlavor],
    objects: &[CapacityObjectObservation],
) -> FlavorOwnershipDecision {
    match derive_flavor_ownership(owned_flavors, objects) {
        Ok(targets) => FlavorOwnershipDecision::Derived(targets),
        Err(reason) => FlavorOwnershipDecision::Fenced {
            reason,
            decision: flavor_vector_patch_decision(
                queue,
                configured_name,
                &static_flavor_targets(owned_flavors),
            ),
        },
    }
}

fn quota_quantities(vector: ResourceVector) -> [(&'static str, String); 3] {
    [
        ("cpu", format!("{}m", vector.cpu.get())),
        ("memory", vector.memory.get().to_string()),
        ("pods", vector.pods.get().to_string()),
    ]
}

/// Make one deterministic resourceVersion-fenced patch for complete flavor
/// vectors. Every selected flavor must have exactly one named cpu, memory, and
/// pods entry before any replacement is emitted.
pub fn flavor_vector_patch_decision(
    queue: &QueueObservation,
    configured_name: &str,
    targets: &[FlavorQuotaTarget],
) -> FlavorActuationDecision {
    if queue.name != configured_name {
        return FlavorActuationDecision::NoMutation {
            reason: ConservativeReason::QueueNameMismatch,
        };
    }
    if queue.owner.as_deref() != Some(DERIVED_CAPACITY_OWNER) {
        return FlavorActuationDecision::NoMutation {
            reason: ConservativeReason::QueueOwnerMismatch,
        };
    }
    if targets.is_empty() {
        return FlavorActuationDecision::NoMutation {
            reason: ConservativeReason::MissingFlavorVector,
        };
    }
    // A repeated target would otherwise generate two independently complete
    // vectors for the same flavor. Refuse it before constructing any patch so a
    // caller cannot accidentally make the last target win.
    if targets
        .iter()
        .map(|target| &target.flavor_name)
        .collect::<BTreeSet<_>>()
        .len()
        != targets.len()
    {
        return FlavorActuationDecision::NoMutation {
            reason: ConservativeReason::AmbiguousFlavorVector,
        };
    }
    let mut operations = vec![json!({
        "op": "test",
        "path": "/metadata/resourceVersion",
        "value": queue.resource_version,
    })];
    let mut changed = false;
    for target in targets {
        let matching: Vec<_> = queue
            .flavors
            .iter()
            .filter(|flavor| flavor.name == target.flavor_name)
            .collect();
        let [flavor] = matching.as_slice() else {
            return FlavorActuationDecision::NoMutation {
                reason: if matching.is_empty() {
                    ConservativeReason::MissingFlavorVector
                } else {
                    ConservativeReason::AmbiguousFlavorVector
                },
            };
        };
        for (resource_name, quantity) in quota_quantities(target.vector) {
            let matching: Vec<_> = flavor
                .resources
                .iter()
                .enumerate()
                .filter(|(_, resource)| resource.name == resource_name)
                .collect();
            let [(resource_index, resource)] = matching.as_slice() else {
                return FlavorActuationDecision::NoMutation {
                    reason: if matching.is_empty() {
                        ConservativeReason::MissingFlavorVector
                    } else {
                        ConservativeReason::AmbiguousFlavorVector
                    },
                };
            };
            if resource.nominal_quota != quantity {
                changed = true;
                operations.push(json!({
                    "op": "replace",
                    "path": format!(
                        "/spec/resourceGroups/{}/flavors/{}/resources/{resource_index}/nominalQuota",
                        flavor.resource_group_index, flavor.flavor_index,
                    ),
                    "value": quantity,
                }));
            }
        }
    }
    if changed {
        FlavorActuationDecision::Patch {
            patch: Value::Array(operations),
        }
    } else {
        FlavorActuationDecision::Noop
    }
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
        labels: node.metadata.labels.clone().unwrap_or_default(),
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

/// A named flavor together with the concrete JSON indexes observed for it.
/// Names select resources; indexes only construct the RFC 6902 path afterward.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueFlavor {
    pub name: String,
    pub resource_group_index: usize,
    pub flavor_index: usize,
    pub resources: Vec<QueueResource>,
}

/// A complete cpu, memory, and pods quota target for one named flavor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlavorQuotaTarget {
    pub flavor_name: String,
    pub vector: ResourceVector,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueObservation {
    pub name: String,
    pub resource_version: String,
    pub owner: Option<String>,
    pub binding_resource: Option<String>,
    pub resources: Vec<QueueResource>,
    /// Full ClusterQueue shape. `resources` remains only for the legacy
    /// single-binding compatibility seam.
    pub flavors: Vec<QueueFlavor>,
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
    MissingFlavorVector,
    AmbiguousFlavorVector,
    MissingFlavorOwnership,
    DuplicateFlavorOwnership,
    FlavorOwnershipAmbiguous,
    FlavorOwnershipOverflow,
    NodePoolDedicationUnasserted,
    NodePoolApiNotFound,
    NodePoolApiForbidden,
    NodePoolLimitsMissing,
    NodePoolLimitsMalformed,
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
    /// The release marker is the sole selector for complete-vector writes.
    pub contract: CapacityContract,
    pub source: CapacitySource,
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
    pub static_fallback: ResourceVector,
    /// JSON selector rendered by Helm for explicit flavor ownership.
    pub flavor_selector: Option<BTreeMap<String, String>>,
    /// Explicit per-flavor selectors for a multi-cohort ClusterQueue. The
    /// chart's single-pool selector remains supported by `flavor_selector`.
    pub flavor_selectors: BTreeMap<String, BTreeMap<String, String>>,
    /// Helm-declared dedicated NodePool identity, never a discovery hint.
    pub nodepool_name: String,
    /// The rendered dedicated-pool assertion required by nodepool-limits.
    pub nodepool_dedicated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapacitySource {
    Static,
    NodeSum,
    NodePoolLimits,
    Invalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapacityContract {
    /// The one-release PR #2901 compatibility protocol.
    Legacy,
    /// The explicit complete-vector protocol.
    VectorV1,
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
        let contract = match std::env::var("DJINN_CAPACITY_CONTRACT") {
            Err(std::env::VarError::NotPresent) => CapacityContract::Legacy,
            Ok(value) if value == "vector-v1" => CapacityContract::VectorV1,
            // An unknown marker cannot silently activate either writer.
            Ok(_) | Err(_) => return None,
        };
        // Old charts supplied an idle CPU reserve, rather than the new complete
        // headroom vector. Keep that lane independent of vector-only inputs.
        let idle_cost = parse_cpu("DJINN_CAPACITY_IDLE_CPU")?;
        let headroom = if contract == CapacityContract::VectorV1 {
            ResourceVector {
                cpu: parse_cpu("DJINN_CAPACITY_HEADROOM_CPU")?,
                memory: memory_quantity(&std::env::var("DJINN_CAPACITY_HEADROOM_MEMORY").ok()?)?,
                pods: pod_quantity(&std::env::var("DJINN_CAPACITY_HEADROOM_PODS").ok()?)?,
            }
        } else {
            ResourceVector {
                cpu: idle_cost,
                ..ResourceVector::ZERO
            }
        };
        let source = if contract == CapacityContract::Legacy {
            // Old charts did not declare a source; retain their Node/Pod lane.
            CapacitySource::NodeSum
        } else {
            match std::env::var("DJINN_CAPACITY_SOURCE").ok().as_deref() {
                Some("static") => CapacitySource::Static,
                Some("node-sum") => CapacitySource::NodeSum,
                Some("nodepool-limits") => CapacitySource::NodePoolLimits,
                _ => CapacitySource::Invalid,
            }
        };
        let (flavor_selector, flavor_selectors) =
            match std::env::var("DJINN_CAPACITY_FLAVOR_SELECTOR") {
                Ok(value) => {
                    let value: Value = serde_json::from_str(&value).ok()?;
                    let selector = value.as_object()?;
                    if selector.is_empty() {
                        return None;
                    }
                    if selector.values().all(Value::is_string) {
                        (Some(serde_json::from_value(value).ok()?), BTreeMap::new())
                    } else {
                        let selectors: BTreeMap<String, BTreeMap<String, String>> =
                            serde_json::from_value(value).ok()?;
                        if selectors
                            .iter()
                            .any(|(name, selector)| name.is_empty() || selector.is_empty())
                        {
                            return None;
                        }
                        (None, selectors)
                    }
                }
                Err(std::env::VarError::NotPresent) => (None, BTreeMap::new()),
                Err(_) => return None,
            };
        let static_fallback = if contract == CapacityContract::VectorV1 {
            ResourceVector {
                cpu: cpu_quantity(&std::env::var("DJINN_CAPACITY_STATIC_CPU").ok()?)?,
                memory: memory_quantity(&std::env::var("DJINN_CAPACITY_STATIC_MEMORY").ok()?)?,
                pods: pod_quantity(&std::env::var("DJINN_CAPACITY_STATIC_PODS").ok()?)?,
            }
        } else {
            // Never derive a vector from legacy sentinel-shaped quotas.
            ResourceVector::ZERO
        };
        if contract == CapacityContract::VectorV1
            && (source == CapacitySource::Invalid
                || (flavor_selector.is_none() && flavor_selectors.is_empty()))
        {
            return None;
        }
        let build_job = controller_build_job();
        Some(Self {
            contract,
            source,
            queue_name: std::env::var("DJINN_CAPACITY_QUEUE_NAME").ok()?,
            node_selector_key: std::env::var("DJINN_CAPACITY_NODE_SELECTOR_KEY").ok()?,
            node_selector_value: std::env::var("DJINN_CAPACITY_NODE_SELECTOR_VALUE").ok()?,
            idle_cost,
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
            static_fallback,
            flavor_selector,
            flavor_selectors,
            nodepool_name: std::env::var("DJINN_CAPACITY_NODEPOOL_NAME").unwrap_or_default(),
            nodepool_dedicated: std::env::var("DJINN_CAPACITY_NODEPOOL_DEDICATED")
                .ok()
                .as_deref()
                == Some("true"),
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

fn nodepool_api(client: Client) -> Api<DynamicObject> {
    let resource = ApiResource::from_gvk(&GroupVersionKind::gvk("karpenter.sh", "v1", "NodePool"));
    Api::all_with(client, &resource)
}

fn nodepool_api_reason(status: u16) -> ConservativeReason {
    match status {
        404 => ConservativeReason::NodePoolApiNotFound,
        403 => ConservativeReason::NodePoolApiForbidden,
        _ => ConservativeReason::ObservationFailed,
    }
}

fn nodepool_observation(
    pool: DynamicObject,
) -> Result<CapacityObjectObservation, ConservativeReason> {
    let name = pool
        .metadata
        .name
        .filter(|name| !name.is_empty())
        .ok_or(ConservativeReason::NodePoolLimitsMissing)?;
    // CR metadata labels are deliberately ignored: only template labels reach Nodes.
    let mut labels = match pool.data.pointer("/spec/template/metadata/labels") {
        // A dedicated pool can be selected solely by its generated identity.
        None => BTreeMap::new(),
        Some(labels) => labels
            .as_object()
            .ok_or(ConservativeReason::NodePoolLimitsMalformed)?
            .iter()
            .map(|(key, value)| {
                value
                    .as_str()
                    .map(|value| (key.clone(), value.to_owned()))
                    .ok_or(ConservativeReason::NodePoolLimitsMalformed)
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?,
    };
    let limit = |resource: &str| {
        pool.data
            .pointer(&format!("/spec/limits/{resource}"))
            .ok_or(ConservativeReason::NodePoolLimitsMissing)?
            .as_str()
            .ok_or(ConservativeReason::NodePoolLimitsMalformed)
    };
    let vector = ResourceVector {
        cpu: cpu_quantity(limit("cpu")?).ok_or(ConservativeReason::NodePoolLimitsMalformed)?,
        memory: memory_quantity(limit("memory")?)
            .ok_or(ConservativeReason::NodePoolLimitsMalformed)?,
        pods: pod_quantity(limit("pods")?).ok_or(ConservativeReason::NodePoolLimitsMalformed)?,
    };
    labels.insert("karpenter.sh/nodepool".into(), name);
    Ok(CapacityObjectObservation {
        effective_labels: labels,
        vector,
    })
}

fn observe_queue(queue: DynamicObject, configured_name: &str) -> Option<QueueObservation> {
    let flavors = queue
        .data
        .pointer("/spec/resourceGroups")?
        .as_array()?
        .iter()
        .enumerate()
        .map(|(resource_group_index, group)| {
            group
                .get("flavors")?
                .as_array()?
                .iter()
                .enumerate()
                .map(|(flavor_index, flavor)| {
                    Some(QueueFlavor {
                        name: flavor.get("name")?.as_str()?.into(),
                        resource_group_index,
                        flavor_index,
                        resources: flavor
                            .get("resources")?
                            .as_array()?
                            .iter()
                            .map(|resource| {
                                Some(QueueResource {
                                    name: resource.get("name")?.as_str()?.into(),
                                    nominal_quota: resource.get("nominalQuota")?.as_str()?.into(),
                                })
                            })
                            .collect::<Option<Vec<_>>>()?,
                    })
                })
                .collect::<Option<Vec<_>>>()
        })
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    Some(QueueObservation {
        name: configured_name.into(),
        resource_version: queue.metadata.resource_version?,
        owner: queue
            .metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get(QUOTA_OWNER_LABEL))
            .cloned(),
        binding_resource: queue
            .metadata
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.get(BINDING_RESOURCE_ANNOTATION))
            .cloned(),
        resources: flavors.first()?.resources.clone(),
        flavors,
    })
}

/// Leader-owned, 30-second observation and actuation loop. Any read/parsing or
/// identity failure publishes fail-safe K and restores the declared static
/// vector through the resourceVersion-fenced patch seam.
pub async fn run_capacity_controller(
    client: Client,
    config: CapacityControllerConfig,
    compile_bound_armed: Arc<dyn Fn() -> bool + Send + Sync>,
    snapshots: watch::Sender<CapacityVector>,
) {
    let nodes: Api<Node> = Api::all(client.clone());
    let pods: Api<Pod> = Api::all(client.clone());
    let nodepools = nodepool_api(client.clone());
    let queues = queue_api(client);
    let selector = format!(
        "{}={}",
        config.node_selector_key, config.node_selector_value
    );
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    loop {
        tick.tick().await;
        // The queue identity is the only universally permitted read.  Source
        // routing below is explicit and never discovers Karpenter APIs.
        let queue = queues
            .get(&config.queue_name)
            .await
            .ok()
            .and_then(|queue| observe_queue(queue, &config.queue_name));
        // PR #2901's annotated binding is a separate wire protocol. The
        // absent release marker reaches this branch before any vector source
        // routing, so old sentinel quotas can never become a static vector.
        if config.contract == CapacityContract::Legacy {
            let observed = async {
                let queue = queue.clone()?;
                let node_list = nodes
                    .list(&ListParams::default().labels(&selector))
                    .await
                    .ok()?;
                let node_observations: Vec<_> = node_list
                    .items
                    .iter()
                    .map(|node| {
                        observe_node(node, &config.node_selector_key, &config.node_selector_value)
                    })
                    .collect();
                let node = select_node(&node_observations).ok()?;
                let allocatable = ResourceVector {
                    cpu: node.allocatable_cpu?,
                    memory: node.allocatable_memory?,
                    pods: node.allocatable_pods?,
                };
                let protected_pods = pods
                    .list(&ListParams::default().labels("djinn.io/capacity-reserved=true"))
                    .await
                    .ok()?;
                if protected_pods.items.len() < config.expected_protected_pods {
                    return None;
                }
                let protected = protected_requests_on_nodes(
                    &protected_pods.items,
                    &BTreeSet::from([node.name.clone()]),
                )
                .ok()?;
                let capacity = derive_capacity_from_rendered_build_job(
                    allocatable,
                    protected,
                    config.headroom,
                    rendered_pod_spec(&config.build_job)?,
                    config.compile_cost,
                    config.fail_safe,
                );
                let CapacityOutcome::Derived(raw) = capacity else {
                    return None;
                };
                let binding = binding_for(&queue, &config.queue_name, raw).ok()?;
                Some((
                    queue,
                    capacity,
                    CapacityVector {
                        binding,
                        compile_slots: raw.compile_slots,
                    },
                ))
            }
            .await;
            let Some((queue, capacity, snapshot)) = observed else {
                let _ = snapshots.send(CapacityVector {
                    binding: BindingQuota::Pods(config.fail_safe.pods),
                    compile_slots: config.fail_safe.compile_slots,
                });
                continue;
            };
            let _ = snapshots.send(snapshot);
            if let ActuationDecision::Patch { patch, .. } = patch_decision(
                &queue,
                &config.queue_name,
                capacity,
                snapshot,
                compile_bound_armed(),
                config.fail_safe,
            ) {
                let _ = queues
                    .patch(
                        &config.queue_name,
                        &PatchParams::default(),
                        &Patch::Json::<()>(
                            serde_json::from_value(patch).expect("valid internal JSON patch"),
                        ),
                    )
                    .await;
            }
            continue;
        }
        if config.source == CapacitySource::NodePoolLimits {
            let result: Result<Vec<FlavorQuotaTarget>, ConservativeReason> = async {
                if !config.nodepool_dedicated || config.nodepool_name.is_empty() {
                    return Err(ConservativeReason::NodePoolDedicationUnasserted);
                }
                let queue = queue.clone().ok_or(ConservativeReason::ObservationFailed)?;
                let pools = nodepools
                    .list(&ListParams::default())
                    .await
                    .map_err(|error| match error {
                        kube::Error::Api(response) => nodepool_api_reason(response.code),
                        _ => ConservativeReason::ObservationFailed,
                    })?;
                let objects = pools
                    .items
                    .into_iter()
                    .filter(|pool| pool.metadata.name.as_deref() == Some(&config.nodepool_name))
                    .map(nodepool_observation)
                    .collect::<Result<Vec<_>, _>>()?;
                if objects.is_empty() {
                    return Err(ConservativeReason::NodePoolLimitsMissing);
                }
                let owned = queue
                    .flavors
                    .iter()
                    .map(|flavor| OwnedFlavor {
                        flavor_name: flavor.name.clone(),
                        selector: config
                            .flavor_selectors
                            .get(&flavor.name)
                            .cloned()
                            .or_else(|| config.flavor_selector.clone()),
                        static_fallback: config.static_fallback,
                    })
                    .collect::<Vec<_>>();
                Ok(derive_flavor_ownership(&owned, &objects)?.targets)
            }
            .await;
            let targets = result.unwrap_or_else(|reason| {
                tracing::warn!(reason = ?reason, "capacity controller static fallback");
                queue
                    .as_ref()
                    .map(|queue| {
                        queue
                            .flavors
                            .iter()
                            .map(|flavor| FlavorQuotaTarget {
                                flavor_name: flavor.name.clone(),
                                vector: config.static_fallback,
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            });
            let _ = snapshots.send(CapacityVector {
                binding: BindingQuota::Pods(config.fail_safe.pods),
                compile_slots: config.fail_safe.compile_slots,
            });
            if let Some(queue) = queue
                && let FlavorActuationDecision::Patch { patch } =
                    flavor_vector_patch_decision(&queue, &config.queue_name, &targets)
            {
                let _ = queues
                    .patch(
                        &config.queue_name,
                        &PatchParams::default(),
                        &Patch::Json::<()>(
                            serde_json::from_value(patch).expect("valid internal JSON patch"),
                        ),
                    )
                    .await;
            }
            continue;
        }
        // This branch is before all Node/Pod APIs.
        if config.source != CapacitySource::NodeSum {
            if config.source == CapacitySource::Invalid {
                tracing::warn!(
                    reason = "CapacitySourceInvalid",
                    "capacity controller static fallback"
                );
            }
            let _ = snapshots.send(CapacityVector {
                binding: BindingQuota::Pods(config.fail_safe.pods),
                compile_slots: config.fail_safe.compile_slots,
            });
            if let Some(queue) = queue {
                let targets = queue
                    .flavors
                    .iter()
                    .map(|flavor| FlavorQuotaTarget {
                        flavor_name: flavor.name.clone(),
                        vector: config.static_fallback,
                    })
                    .collect::<Vec<_>>();
                if let FlavorActuationDecision::Patch { patch } =
                    flavor_vector_patch_decision(&queue, &config.queue_name, &targets)
                {
                    let _ = queues
                        .patch(
                            &config.queue_name,
                            &PatchParams::default(),
                            &Patch::Json::<()>(
                                serde_json::from_value(patch).expect("valid internal JSON patch"),
                            ),
                        )
                        .await;
                }
            }
            continue;
        }
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
            let observation = queue.clone()?;
            // Establish exclusive ownership before deriving either capacity or
            // the node-name scope used for protected Pods. Unmatched Nodes are
            // outside this controller's capacity domain.
            let owned: Vec<_> =
                observation
                    .flavors
                    .iter()
                    .map(|flavor| OwnedFlavor {
                        flavor_name: flavor.name.clone(),
                        selector: config.flavor_selectors.get(&flavor.name).cloned().or_else(
                            || {
                                (observation.flavors.len() == 1).then(|| {
                                    config.flavor_selector.clone().unwrap_or_else(|| {
                                        BTreeMap::from([(
                                            config.node_selector_key.clone(),
                                            config.node_selector_value.clone(),
                                        )])
                                    })
                                })
                            },
                        ),
                        static_fallback: config.static_fallback,
                    })
                    .collect();
            validate_owned_flavors(&owned).ok()?;
            let mut assigned_names = BTreeSet::new();
            let mut objects = Vec::new();
            for node in node_observations.iter().filter(|node| {
                node.selector_matches && node.ready && !node.unschedulable && !node.terminating
            }) {
                let matches = owned
                    .iter()
                    .filter(|flavor| {
                        matches_flavor_selector(
                            &node.labels,
                            flavor.selector.as_ref().expect("ownership validated"),
                        )
                    })
                    .count();
                if matches == 0 {
                    continue;
                }
                if matches > 1 {
                    return None;
                }
                let vector = ResourceVector {
                    cpu: node.allocatable_cpu?,
                    memory: node.allocatable_memory?,
                    pods: node.allocatable_pods?,
                };
                if node.name.is_empty() || !assigned_names.insert(node.name.clone()) {
                    return None;
                }
                objects.push(CapacityObjectObservation {
                    effective_labels: node.labels.clone(),
                    vector,
                });
            }
            let ownership = derive_flavor_ownership(&owned, &objects).ok()?;
            if assigned_names.is_empty() {
                return None;
            }
            let assigned_allocatable = ownership.assigned_aggregate;
            let protected = pods
                .list(&ListParams::default().labels("djinn.io/capacity-reserved=true"))
                .await
                .ok()?;
            if protected.items.len() < config.expected_protected_pods {
                return None;
            }
            let protected = protected_requests_on_nodes(&protected.items, &assigned_names).ok()?;
            let podset_cost =
                podset_cost_from_pod_spec(rendered_pod_spec(&config.build_job)?).ok()?;
            let ResourceVectorOutcome::Derived(vector) =
                derive_resource_vector(ResourceVectorDerivationInputs {
                    protected_population_complete: true,
                    allocatable: ResourceVectorInput::complete(assigned_allocatable),
                    protected: ResourceVectorInput::complete(protected),
                    headroom: ResourceVectorInput::complete(config.headroom),
                    podset_cost: ResourceVectorInput::complete(podset_cost),
                })
            else {
                return None;
            };
            let capacity = derive_capacity_from_rendered_build_job(
                assigned_allocatable,
                protected,
                config.headroom,
                rendered_pod_spec(&config.build_job)?,
                config.compile_cost,
                config.fail_safe,
            );
            let CapacityOutcome::Derived(raw) = capacity else {
                return None;
            };
            let target_vector = ResourceVector {
                cpu: vector.raw.cpu,
                memory: vector.raw.memory,
                pods: vector.admitted_podsets,
            };
            let targets = derived_flavor_targets(ownership, target_vector)?;
            Some((
                observation,
                targets,
                CapacityVector {
                    binding: BindingQuota::Pods(raw.pods),
                    compile_slots: raw.compile_slots,
                },
            ))
        }
        .await;

        let Some((queue, targets, snapshot)) = observed else {
            tracing::warn!(
                reason = "NodeSumObservationFailed",
                "capacity controller static fallback"
            );
            let _ = snapshots.send(CapacityVector {
                binding: BindingQuota::Pods(config.fail_safe.pods),
                compile_slots: config.fail_safe.compile_slots,
            });
            if let Some(queue) = queue {
                let targets = queue
                    .flavors
                    .iter()
                    .map(|flavor| FlavorQuotaTarget {
                        flavor_name: flavor.name.clone(),
                        vector: config.static_fallback,
                    })
                    .collect::<Vec<_>>();
                if let FlavorActuationDecision::Patch { patch } =
                    flavor_vector_patch_decision(&queue, &config.queue_name, &targets)
                {
                    let _ = queues
                        .patch(
                            &config.queue_name,
                            &PatchParams::default(),
                            &Patch::Json::<()>(
                                serde_json::from_value(patch).expect("valid internal JSON patch"),
                            ),
                        )
                        .await;
                }
            }
            continue;
        };
        let _ = snapshots.send(snapshot);
        let _ = compile_bound_armed();
        if let FlavorActuationDecision::Patch { patch } =
            flavor_vector_patch_decision(&queue, &config.queue_name, &targets)
        {
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
            flavors: vec![QueueFlavor {
                name: "default".into(),
                resource_group_index: 0,
                flavor_index: 0,
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
            }],
        }
    }
    fn outcome() -> CapacityOutcome {
        CapacityOutcome::Derived(derived())
    }

    fn flavor(name: &str, resource_group_index: usize, flavor_index: usize) -> QueueFlavor {
        QueueFlavor {
            name: name.into(),
            resource_group_index,
            flavor_index,
            // Deliberately not cpu-first: vector actuation must resolve these
            // indexes from the names below.
            resources: vec![
                QueueResource {
                    name: "memory".into(),
                    nominal_quota: "1".into(),
                },
                QueueResource {
                    name: "pods".into(),
                    nominal_quota: "1".into(),
                },
                QueueResource {
                    name: "cpu".into(),
                    nominal_quota: "1m".into(),
                },
            ],
        }
    }

    fn vector_queue(flavors: Vec<QueueFlavor>) -> QueueObservation {
        QueueObservation {
            name: "djinn-kueue".into(),
            resource_version: "vector-rv".into(),
            owner: Some(DERIVED_CAPACITY_OWNER.into()),
            binding_resource: None,
            resources: flavors[0].resources.clone(),
            flavors,
        }
    }

    fn apply_patch_to_live_queue(live: &mut Value, patch: &Value) {
        for operation in patch.as_array().expect("JSON patch is an array") {
            if operation["op"] == "replace" {
                let path = operation["path"].as_str().expect("replace has path");
                *live.pointer_mut(path).expect("replacement path exists") =
                    operation["value"].clone();
            }
        }
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
        let CapacityOutcome::Derived(vps_light_memory_bound) =
            derive_capacity_from_rendered_build_job(
                vps,
                resources(0, 0, 0),
                resources(0, 0, 0),
                light_pod,
                CpuMillicores::new(2_800).unwrap(),
                safe(),
            )
        else {
            panic!("complete VPS observation must derive for the light role")
        };
        let vps_light_expected = (vps.cpu.get() / light_cost.cpu.get())
            .min(vps.memory.get() / light_cost.memory.get())
            .min(vps.pods.get() / light_cost.pods.get());
        assert_eq!(vps_light_memory_bound.pods, vps_light_expected);
        assert_eq!(
            vps_light_memory_bound.pods,
            vps.memory.get() / light_cost.memory.get()
        );
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
    async fn quota_controller_wire() {
        use crate::runtime_fixture::{RecordedApiserver, recording_client};

        let queue = vector_queue(vec![flavor("default", 0, 0)]);
        let target = FlavorQuotaTarget {
            flavor_name: "default".into(),
            // Raw cpu/memory and the limiting PodSet count from the normative
            // vector derivation are serialized without a legacy binding choice.
            vector: resources(7_500, 8_192, 7),
        };
        let FlavorActuationDecision::Patch { patch } =
            flavor_vector_patch_decision(&queue, "djinn-kueue", &[target])
        else {
            panic!("complete vector target must patch")
        };
        let expected = json!([
            {"op":"test", "path":"/metadata/resourceVersion", "value":"vector-rv"},
            {"op":"replace", "path":"/spec/resourceGroups/0/flavors/0/resources/2/nominalQuota", "value":"7500m"},
            {"op":"replace", "path":"/spec/resourceGroups/0/flavors/0/resources/0/nominalQuota", "value":"8192"},
            {"op":"replace", "path":"/spec/resourceGroups/0/flavors/0/resources/1/nominalQuota", "value":"7"},
        ]);
        assert_eq!(
            patch, expected,
            "all three named vector dimensions are required"
        );

        let recorder = RecordedApiserver::new();
        let api = queue_api(recording_client(&recorder, "default"));
        let result = api
            .patch(
                "djinn-kueue",
                &PatchParams::default(),
                &Patch::Json::<()>(serde_json::from_value(patch.clone()).unwrap()),
            )
            .await;
        assert!(result.is_err(), "fixture refuses after recording the wire");
        let mutations = recorder.mutations();
        assert_eq!(mutations.len(), 1);
        assert_eq!(mutations[0].method, "PATCH");
        assert_eq!(
            serde_json::from_str::<Value>(&mutations[0].body).unwrap(),
            expected
        );

        let mut live = json!({"metadata":{"resourceVersion":"vector-rv"},"spec":{"resourceGroups":[{"flavors":[{"resources":[{"name":"memory","nominalQuota":"1"},{"name":"pods","nominalQuota":"1"},{"name":"cpu","nominalQuota":"1m"}]}]}]}});
        apply_patch_to_live_queue(&mut live, &patch);
        assert_eq!(
            live.pointer("/spec/resourceGroups/0/flavors/0/resources/2/nominalQuota"),
            Some(&json!("7500m"))
        );
        assert_eq!(
            live.pointer("/spec/resourceGroups/0/flavors/0/resources/0/nominalQuota"),
            Some(&json!("8192"))
        );
        assert_eq!(
            live.pointer("/spec/resourceGroups/0/flavors/0/resources/1/nominalQuota"),
            Some(&json!("7"))
        );
    }

    #[test]
    fn quota_controller_flavor_addressing() {
        let queue = vector_queue(vec![flavor("spot", 0, 0), flavor("on-demand", 0, 1)]);
        let targets = [
            FlavorQuotaTarget {
                flavor_name: "spot".into(),
                vector: resources(4_000, 8_192, 5),
            },
            FlavorQuotaTarget {
                flavor_name: "on-demand".into(),
                vector: resources(6_000, 16_384, 8),
            },
        ];
        let FlavorActuationDecision::Patch { patch } =
            flavor_vector_patch_decision(&queue, "djinn-kueue", &targets)
        else {
            panic!("two complete flavor vectors must patch")
        };
        // Both flavors have memory,pods,cpu ordering. These paths prove name,
        // rather than positional, addressing for each resource and flavor.
        assert_eq!(
            patch[1]["path"],
            "/spec/resourceGroups/0/flavors/0/resources/2/nominalQuota"
        );
        assert_eq!(
            patch[2]["path"],
            "/spec/resourceGroups/0/flavors/0/resources/0/nominalQuota"
        );
        assert_eq!(
            patch[3]["path"],
            "/spec/resourceGroups/0/flavors/0/resources/1/nominalQuota"
        );
        assert_eq!(
            patch[4]["path"],
            "/spec/resourceGroups/0/flavors/1/resources/2/nominalQuota"
        );
        assert_eq!(
            patch[5]["path"],
            "/spec/resourceGroups/0/flavors/1/resources/0/nominalQuota"
        );
        assert_eq!(
            patch[6]["path"],
            "/spec/resourceGroups/0/flavors/1/resources/1/nominalQuota"
        );

        let mut live = json!({"spec":{"resourceGroups":[{"flavors":[
            {"resources":[{"name":"memory","nominalQuota":"1"},{"name":"pods","nominalQuota":"1"},{"name":"cpu","nominalQuota":"1m"}]},
            {"resources":[{"name":"memory","nominalQuota":"1"},{"name":"pods","nominalQuota":"1"},{"name":"cpu","nominalQuota":"1m"}]}
        ]}]}});
        apply_patch_to_live_queue(&mut live, &patch);
        let flavors = live
            .pointer("/spec/resourceGroups/0/flavors")
            .unwrap()
            .as_array()
            .unwrap();
        let totals = flavors.iter().fold((0_i64, 0_i64, 0_i64), |sum, flavor| {
            let resources = flavor["resources"].as_array().unwrap();
            (
                sum.0
                    + resources[2]["nominalQuota"]
                        .as_str()
                        .unwrap()
                        .strip_suffix('m')
                        .unwrap()
                        .parse::<i64>()
                        .unwrap(),
                sum.1
                    + resources[0]["nominalQuota"]
                        .as_str()
                        .unwrap()
                        .parse::<i64>()
                        .unwrap(),
                sum.2
                    + resources[1]["nominalQuota"]
                        .as_str()
                        .unwrap()
                        .parse::<i64>()
                        .unwrap(),
            )
        });
        assert_eq!(totals, (10_000, 24_576, 13));
        let per_flavor: Vec<_> = flavors
            .iter()
            .map(|flavor| {
                let resources = flavor["resources"].as_array().unwrap();
                (
                    resources[2]["nominalQuota"].as_str().unwrap(),
                    resources[0]["nominalQuota"].as_str().unwrap(),
                    resources[1]["nominalQuota"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            per_flavor,
            vec![("4000m", "8192", "5"), ("6000m", "16384", "8")]
        );

        let mut missing = queue.clone();
        missing.flavors[0]
            .resources
            .retain(|resource| resource.name != "pods");
        assert!(matches!(
            flavor_vector_patch_decision(&missing, "djinn-kueue", &targets),
            FlavorActuationDecision::NoMutation {
                reason: ConservativeReason::MissingFlavorVector
            }
        ));
        let mut duplicate = queue.clone();
        duplicate.flavors[1].resources.push(QueueResource {
            name: "cpu".into(),
            nominal_quota: "1m".into(),
        });
        assert!(matches!(
            flavor_vector_patch_decision(&duplicate, "djinn-kueue", &targets),
            FlavorActuationDecision::NoMutation {
                reason: ConservativeReason::AmbiguousFlavorVector
            }
        ));
        let mut duplicate_flavor = queue.clone();
        duplicate_flavor.flavors.push(flavor("spot", 1, 0));
        assert!(matches!(
            flavor_vector_patch_decision(&duplicate_flavor, "djinn-kueue", &targets),
            FlavorActuationDecision::NoMutation {
                reason: ConservativeReason::AmbiguousFlavorVector
            }
        ));
        let repeated_targets = [targets[0].clone(), targets[0].clone()];
        assert!(matches!(
            flavor_vector_patch_decision(&queue, "djinn-kueue", &repeated_targets),
            FlavorActuationDecision::NoMutation {
                reason: ConservativeReason::AmbiguousFlavorVector
            }
        ));
    }

    #[test]
    fn capacity_flavor_ownership() {
        let owned = [
            OwnedFlavor {
                flavor_name: "flavor-a".into(),
                selector: Some(BTreeMap::from([("cohort".into(), "a".into())])),
                static_fallback: resources(111, 222, 3),
            },
            OwnedFlavor {
                flavor_name: "flavor-b".into(),
                selector: Some(BTreeMap::from([("cohort".into(), "b".into())])),
                static_fallback: resources(444, 555, 6),
            },
        ];
        let assigned = [
            CapacityObjectObservation {
                effective_labels: BTreeMap::from([("cohort".into(), "a".into())]),
                vector: resources(1_000, 2_000, 3),
            },
            CapacityObjectObservation {
                effective_labels: BTreeMap::from([("cohort".into(), "a".into())]),
                vector: resources(4_000, 5_000, 6),
            },
            CapacityObjectObservation {
                effective_labels: BTreeMap::from([("cohort".into(), "b".into())]),
                vector: resources(7_000, 8_000, 9),
            },
            CapacityObjectObservation {
                effective_labels: BTreeMap::from([("cohort".into(), "unowned".into())]),
                vector: resources(99_000, 99_000, 99),
            },
        ];
        let derived = derive_flavor_ownership(&owned, &assigned).unwrap();
        assert_eq!(derived.targets[0].vector, resources(5_000, 7_000, 9));
        assert_eq!(derived.targets[1].vector, resources(7_000, 8_000, 9));
        assert_eq!(derived.assigned_aggregate, resources(12_000, 15_000, 18));
        assert_eq!(
            derived
                .targets
                .iter()
                .try_fold(ResourceVector::ZERO, |sum, target| sum
                    .checked_add(target.vector)),
            Ok(derived.assigned_aggregate),
            "exclusive flavor outputs must conserve every assigned component"
        );

        let overlapping = [
            OwnedFlavor {
                flavor_name: "flavor-a".into(),
                selector: Some(BTreeMap::from([("tier".into(), "general".into())])),
                static_fallback: owned[0].static_fallback,
            },
            OwnedFlavor {
                flavor_name: "flavor-b".into(),
                selector: Some(BTreeMap::from([("region".into(), "east".into())])),
                static_fallback: owned[1].static_fallback,
            },
        ];
        let ambiguous_object = [CapacityObjectObservation {
            effective_labels: BTreeMap::from([
                ("tier".into(), "general".into()),
                ("region".into(), "east".into()),
            ]),
            vector: resources(12_000, 15_000, 18),
        }];
        assert_eq!(
            derive_flavor_ownership(&overlapping, &ambiguous_object),
            Err(ConservativeReason::FlavorOwnershipAmbiguous),
            "ownership cannot use first-match, last-match, or duplicate accounting"
        );

        let queue = vector_queue(vec![flavor("flavor-a", 0, 0), flavor("flavor-b", 0, 1)]);
        let FlavorOwnershipDecision::Fenced { reason, decision } =
            flavor_ownership_patch_decision(&queue, "djinn-kueue", &overlapping, &ambiguous_object)
        else {
            panic!("ambiguous ownership must fence every owned flavor")
        };
        assert_eq!(reason, ConservativeReason::FlavorOwnershipAmbiguous);
        let FlavorActuationDecision::Patch { patch } = decision else {
            panic!("static fallback must be a fenced JSON patch")
        };
        assert_eq!(
            patch[0],
            json!({"op":"test", "path":"/metadata/resourceVersion", "value":"vector-rv"})
        );
        assert_eq!(patch.as_array().unwrap().len(), 7);
        let mut live = json!({"metadata":{"resourceVersion":"vector-rv"},"spec":{"resourceGroups":[{"flavors":[
            {"resources":[{"name":"memory","nominalQuota":"1"},{"name":"pods","nominalQuota":"1"},{"name":"cpu","nominalQuota":"1m"}]},
            {"resources":[{"name":"memory","nominalQuota":"1"},{"name":"pods","nominalQuota":"1"},{"name":"cpu","nominalQuota":"1m"}]}
        ]}]}});
        apply_patch_to_live_queue(&mut live, &patch);
        assert_eq!(
            live,
            json!({"metadata":{"resourceVersion":"vector-rv"},"spec":{"resourceGroups":[{"flavors":[
                {"resources":[{"name":"memory","nominalQuota":"222"},{"name":"pods","nominalQuota":"3"},{"name":"cpu","nominalQuota":"111m"}]},
                {"resources":[{"name":"memory","nominalQuota":"555"},{"name":"pods","nominalQuota":"6"},{"name":"cpu","nominalQuota":"444m"}]}
            ]}]}}),
            "each flavor restores its own static vector, never the shared aggregate"
        );

        let duplicate = [owned[0].clone(), owned[0].clone()];
        assert_eq!(
            derive_flavor_ownership(&duplicate, &assigned),
            Err(ConservativeReason::DuplicateFlavorOwnership)
        );
        let missing = [OwnedFlavor {
            selector: None,
            ..owned[0].clone()
        }];
        assert_eq!(
            derive_flavor_ownership(&missing, &assigned),
            Err(ConservativeReason::MissingFlavorOwnership)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn quota_controller_wire_drives_full_recorded_observation_and_actuation() {
        // The focused two-flavor fixture above covers the exact vector wire
        // body; this integration fixture continues to cover the controller
        // observation loop.
        use crate::runtime_fixture::capacity_controller_cluster;
        let build_job = controller_build_job();
        for surface in ["pods", "cpu"] {
            let (client, recorder) = capacity_controller_cluster("default", surface);
            let config = CapacityControllerConfig {
                contract: CapacityContract::VectorV1,
                source: CapacitySource::NodeSum,
                queue_name: "djinn-kueue".into(),
                node_selector_key: "kubernetes.io/hostname".into(),
                node_selector_value: "worker-1".into(),
                idle_cost: CpuMillicores::new(750).unwrap(),
                compile_cost: CpuMillicores::new(2_800).unwrap(),
                headroom: ResourceVector::ZERO,
                build_job: build_job.clone(),
                fail_safe: safe(),
                expected_protected_pods: 5,
                static_fallback: resources(12_000, 48 * 1024 * 1024 * 1024, 3),
                flavor_selector: None,
                flavor_selectors: BTreeMap::new(),
                nodepool_name: String::new(),
                nodepool_dedicated: false,
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
            let patch: Value = serde_json::from_str(&mutations[0].body).unwrap();
            assert_eq!(
                patch[0],
                json!({"op":"test", "path":"/metadata/resourceVersion", "value":"42"})
            );
            assert_eq!(patch.as_array().unwrap().len(), 4);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn capacity_nodepool_source() {
        use crate::runtime_fixture::{NodePoolFixture, capacity_controller_nodepool_cluster};

        let pool: DynamicObject = serde_json::from_value(json!({
            "apiVersion":"karpenter.sh/v1", "kind":"NodePool",
            "metadata":{"name":"dedicated","labels":{"cohort":"wrong"}},
            "spec":{"template":{"metadata":{"labels":{"cohort":"right"}}},"limits":{"cpu":"12","memory":"16Gi","pods":"42"}}
        })).unwrap();
        let observed = nodepool_observation(pool).unwrap();
        assert_eq!(
            observed.effective_labels.get("cohort"),
            Some(&"right".into())
        );
        assert_eq!(
            observed.effective_labels.get("karpenter.sh/nodepool"),
            Some(&"dedicated".into())
        );
        assert_eq!(
            observed.vector,
            resources(12_000, 16 * 1024 * 1024 * 1024, 42)
        );
        let identity_only: DynamicObject = serde_json::from_value(json!({"apiVersion":"karpenter.sh/v1","kind":"NodePool","metadata":{"name":"identity"},"spec":{"template":{"metadata":{}},"limits":{"cpu":"1","memory":"1","pods":"1"}}})).unwrap();
        assert_eq!(
            nodepool_observation(identity_only)
                .unwrap()
                .effective_labels
                .get("karpenter.sh/nodepool"),
            Some(&"identity".into())
        );
        let owned = [
            OwnedFlavor {
                flavor_name: "a".into(),
                selector: Some(BTreeMap::from([("cohort".into(), "a".into())])),
                static_fallback: ResourceVector::ZERO,
            },
            OwnedFlavor {
                flavor_name: "b".into(),
                selector: Some(BTreeMap::from([("cohort".into(), "b".into())])),
                static_fallback: ResourceVector::ZERO,
            },
        ];
        let pools = [
            CapacityObjectObservation {
                effective_labels: BTreeMap::from([("cohort".into(), "a".into())]),
                vector: resources(12_000, 16 * 1024 * 1024 * 1024, 42),
            },
            CapacityObjectObservation {
                effective_labels: BTreeMap::from([("cohort".into(), "b".into())]),
                vector: resources(8_000, 8 * 1024 * 1024 * 1024, 20),
            },
            CapacityObjectObservation {
                effective_labels: BTreeMap::from([("cohort".into(), "unmatched".into())]),
                vector: resources(99_000, 99, 99),
            },
        ];
        let ownership = derive_flavor_ownership(&owned, &pools).unwrap();
        assert_eq!(
            ownership.targets,
            vec![
                FlavorQuotaTarget {
                    flavor_name: "a".into(),
                    vector: resources(12_000, 16 * 1024 * 1024 * 1024, 42)
                },
                FlavorQuotaTarget {
                    flavor_name: "b".into(),
                    vector: resources(8_000, 8 * 1024 * 1024 * 1024, 20)
                },
            ]
        );
        assert_eq!(
            ownership.assigned_aggregate,
            resources(20_000, 24 * 1024 * 1024 * 1024, 62)
        );
        assert_eq!(
            ownership
                .targets
                .iter()
                .fold(ResourceVector::ZERO, |sum, target| sum
                    .checked_add(target.vector)
                    .unwrap()),
            ownership.assigned_aggregate
        );
        assert_eq!(
            derive_flavor_ownership(
                &[
                    owned[0].clone(),
                    OwnedFlavor {
                        flavor_name: "overlap".into(),
                        selector: Some(BTreeMap::from([
                            ("cohort".into(), "a".into()),
                            ("zone".into(), "shared".into()),
                        ])),
                        static_fallback: ResourceVector::ZERO
                    }
                ],
                &[CapacityObjectObservation {
                    effective_labels: BTreeMap::from([
                        ("cohort".into(), "a".into()),
                        ("zone".into(), "shared".into()),
                    ]),
                    vector: resources(1_000, 1, 1),
                }]
            ),
            Err(ConservativeReason::FlavorOwnershipAmbiguous)
        );

        for (limits, reason) in [
            (json!({}), ConservativeReason::NodePoolLimitsMissing),
            (
                json!({"cpu":"bad","memory":"1","pods":"1"}),
                ConservativeReason::NodePoolLimitsMalformed,
            ),
            (
                json!({"cpu":"1","memory":"1"}),
                ConservativeReason::NodePoolLimitsMissing,
            ),
            (
                json!({"cpu":"-1","memory":"1","pods":"1"}),
                ConservativeReason::NodePoolLimitsMalformed,
            ),
            (
                json!({"cpu":"9223372036854775807","memory":"1","pods":"1"}),
                ConservativeReason::NodePoolLimitsMalformed,
            ),
        ] {
            let pool: DynamicObject = serde_json::from_value(json!({"apiVersion":"karpenter.sh/v1","kind":"NodePool","metadata":{"name":"dedicated"},"spec":{"template":{"metadata":{}},"limits":limits}})).unwrap();
            assert_eq!(nodepool_observation(pool), Err(reason));
        }
        assert_eq!(
            nodepool_api_reason(404),
            ConservativeReason::NodePoolApiNotFound
        );
        assert_eq!(
            nodepool_api_reason(403),
            ConservativeReason::NodePoolApiForbidden
        );

        for fixture in [
            NodePoolFixture::Valid,
            NodePoolFixture::Missing,
            NodePoolFixture::Malformed,
            NodePoolFixture::Incomplete,
            NodePoolFixture::Negative,
            NodePoolFixture::Overflow,
            NodePoolFixture::NotFound,
            NodePoolFixture::Forbidden,
        ] {
            let (client, recorder, live) = capacity_controller_nodepool_cluster("default", fixture);
            let config = CapacityControllerConfig {
                contract: CapacityContract::VectorV1,
                source: CapacitySource::NodePoolLimits,
                queue_name: "djinn-kueue".into(),
                node_selector_key: "unused".into(),
                node_selector_value: "unused".into(),
                idle_cost: CpuMillicores::new(1).unwrap(),
                compile_cost: CpuMillicores::new(1).unwrap(),
                headroom: ResourceVector::ZERO,
                build_job: controller_build_job(),
                fail_safe: safe(),
                expected_protected_pods: 0,
                static_fallback: resources(9_000, 8_192, 9),
                flavor_selector: Some(BTreeMap::from([("cohort".into(), "right".into())])),
                flavor_selectors: BTreeMap::new(),
                nodepool_name: "dedicated".into(),
                nodepool_dedicated: true,
            };
            let (tx, _) = watch::channel(CapacityVector {
                binding: BindingQuota::Pods(0),
                compile_slots: 0,
            });
            let task = tokio::spawn(run_capacity_controller(
                client,
                config,
                Arc::new(|| true),
                tx,
            ));
            for _ in 0..4 {
                tokio::time::advance(Duration::from_secs(30)).await;
                tokio::task::yield_now().await;
                if !recorder.mutations().is_empty() {
                    break;
                }
            }
            task.abort();
            let requests = recorder.all();
            let paths: Vec<_> = requests
                .iter()
                .map(|r| (r.method.as_str(), r.path.as_str()))
                .collect();
            assert_eq!(
                paths[0],
                (
                    "GET",
                    "/apis/kueue.x-k8s.io/v1beta1/clusterqueues/djinn-kueue"
                )
            );
            assert_eq!(paths[1], ("GET", "/apis/karpenter.sh/v1/nodepools"));
            assert!(
                !paths
                    .iter()
                    .any(|(_, path)| *path == "/api/v1/nodes" || *path == "/api/v1/pods")
            );
            let patch: Value = serde_json::from_str(&recorder.mutations()[0].body).unwrap();
            let values: Vec<_> = patch
                .as_array()
                .unwrap()
                .iter()
                .skip(1)
                .map(|op| op["value"].as_str().unwrap())
                .collect();
            if matches!(fixture, NodePoolFixture::Valid) {
                assert_eq!(values, ["12000m", "17179869184", "42"]);
            } else {
                assert_eq!(values, ["9000m", "8192", "9"]);
            }
            assert_eq!(
                patch,
                json!([
                    {"op":"test","path":"/metadata/resourceVersion","value":"nodepool-rv"},
                    {"op":"replace","path":"/spec/resourceGroups/0/flavors/0/resources/1/nominalQuota","value":if matches!(fixture, NodePoolFixture::Valid) {"12000m"} else {"9000m"}},
                    {"op":"replace","path":"/spec/resourceGroups/0/flavors/0/resources/2/nominalQuota","value":if matches!(fixture, NodePoolFixture::Valid) {"17179869184"} else {"8192"}},
                    {"op":"replace","path":"/spec/resourceGroups/0/flavors/0/resources/0/nominalQuota","value":if matches!(fixture, NodePoolFixture::Valid) {"42"} else {"9"}}
                ])
            );
            let expected = if matches!(fixture, NodePoolFixture::Valid) {
                ["42", "12000m", "17179869184"]
            } else {
                ["9", "9000m", "8192"]
            };
            let final_queue = live.lock().unwrap();
            let final_values: Vec<_> =
                final_queue["spec"]["resourceGroups"][0]["flavors"][0]["resources"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| r["nominalQuota"].as_str().unwrap())
                    .collect();
            assert_eq!(final_values, expected);
        }
        // An unasserted dedication (including an empty configured identity) must
        // fence before the Karpenter request while retaining the complete vector.
        for (dedicated, name) in [(false, "dedicated"), (true, "")] {
            let (client, recorder, _live) =
                capacity_controller_nodepool_cluster("default", NodePoolFixture::Valid);
            let config = CapacityControllerConfig {
                contract: CapacityContract::VectorV1,
                source: CapacitySource::NodePoolLimits,
                queue_name: "djinn-kueue".into(),
                node_selector_key: "unused".into(),
                node_selector_value: "unused".into(),
                idle_cost: CpuMillicores::new(1).unwrap(),
                compile_cost: CpuMillicores::new(1).unwrap(),
                headroom: ResourceVector::ZERO,
                build_job: controller_build_job(),
                fail_safe: safe(),
                expected_protected_pods: 0,
                static_fallback: resources(9_000, 8_192, 9),
                flavor_selector: Some(BTreeMap::from([("cohort".into(), "right".into())])),
                flavor_selectors: BTreeMap::new(),
                nodepool_name: name.into(),
                nodepool_dedicated: dedicated,
            };
            let (tx, _) = watch::channel(CapacityVector {
                binding: BindingQuota::Pods(0),
                compile_slots: 0,
            });
            let task = tokio::spawn(run_capacity_controller(
                client,
                config,
                Arc::new(|| true),
                tx,
            ));
            for _ in 0..4 {
                tokio::time::advance(Duration::from_secs(30)).await;
                tokio::task::yield_now().await;
                if !recorder.mutations().is_empty() {
                    break;
                }
            }
            task.abort();
            assert!(
                !recorder
                    .all()
                    .iter()
                    .any(|request| request.path.contains("karpenter"))
            );
            let patch: Value = serde_json::from_str(&recorder.mutations()[0].body).unwrap();
            assert_eq!(patch.as_array().unwrap().len(), 4);
            assert_eq!(
                patch[0],
                json!({"op":"test","path":"/metadata/resourceVersion","value":"nodepool-rv"})
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn capacity_source_selection() {
        use crate::runtime_fixture::{
            capacity_controller_cluster, capacity_controller_multi_flavor_cluster,
        };

        for source in [CapacitySource::Static, CapacitySource::NodeSum] {
            let (client, recorder) = if source == CapacitySource::NodeSum {
                capacity_controller_multi_flavor_cluster("default", "pods")
            } else {
                capacity_controller_cluster("default", "pods")
            };
            let flavor_selectors = if source == CapacitySource::NodeSum {
                BTreeMap::from([
                    (
                        "a".into(),
                        BTreeMap::from([("djinn.io/capacity-pool".into(), "a".into())]),
                    ),
                    (
                        "b".into(),
                        BTreeMap::from([("djinn.io/capacity-pool".into(), "b".into())]),
                    ),
                ])
            } else {
                BTreeMap::new()
            };
            let config = CapacityControllerConfig {
                contract: CapacityContract::VectorV1,
                source,
                queue_name: "djinn-kueue".into(),
                node_selector_key: if source == CapacitySource::NodeSum {
                    "djinn.io/eligible".into()
                } else {
                    "kubernetes.io/hostname".into()
                },
                node_selector_value: if source == CapacitySource::NodeSum {
                    "true".into()
                } else {
                    "worker-1".into()
                },
                idle_cost: CpuMillicores::new(750).unwrap(),
                compile_cost: CpuMillicores::new(2_800).unwrap(),
                headroom: ResourceVector::ZERO,
                build_job: controller_build_job(),
                fail_safe: safe(),
                expected_protected_pods: 5,
                static_fallback: resources(9_000, 8_192, 9),
                flavor_selector: None,
                flavor_selectors,
                nodepool_name: String::new(),
                nodepool_dedicated: false,
            };
            let (tx, _) = watch::channel(CapacityVector {
                binding: BindingQuota::Pods(3),
                compile_slots: 2,
            });
            let task = tokio::spawn(run_capacity_controller(
                client,
                config,
                Arc::new(|| true),
                tx,
            ));
            for _ in 0..8 {
                tokio::time::advance(Duration::from_secs(30)).await;
                tokio::task::yield_now().await;
                if !recorder.mutations().is_empty() {
                    break;
                }
            }
            task.abort();
            let requests = recorder.all();
            let paths: Vec<_> = requests
                .iter()
                .map(|request| (request.method.as_str(), request.path.as_str()))
                .collect();
            assert!(
                paths.iter().all(|(_, path)| !path.contains("karpenter")),
                "no CRD presence probe: {paths:?}"
            );
            if source == CapacitySource::Static {
                assert_eq!(
                    paths,
                    vec![
                        (
                            "GET",
                            "/apis/kueue.x-k8s.io/v1beta1/clusterqueues/djinn-kueue"
                        ),
                        (
                            "PATCH",
                            "/apis/kueue.x-k8s.io/v1beta1/clusterqueues/djinn-kueue"
                        ),
                    ]
                );
                let patch: Value = serde_json::from_str(&recorder.mutations()[0].body).unwrap();
                assert_eq!(patch[0]["path"], "/metadata/resourceVersion");
                assert_eq!(patch[1]["value"], "9000m");
                assert_eq!(patch[2]["value"], "8192");
                assert_eq!(patch[3]["value"], "9");
            } else {
                assert_eq!(
                    paths,
                    vec![
                        (
                            "GET",
                            "/apis/kueue.x-k8s.io/v1beta1/clusterqueues/djinn-kueue"
                        ),
                        ("GET", "/api/v1/nodes"),
                        ("GET", "/api/v1/pods"),
                        (
                            "PATCH",
                            "/apis/kueue.x-k8s.io/v1beta1/clusterqueues/djinn-kueue"
                        ),
                    ],
                    "node-sum permits only queue identity, Nodes, protected Pods, and the fenced patch"
                );
                assert_eq!(recorder.mutations().len(), 1);
                let patch: Value = serde_json::from_str(&recorder.mutations()[0].body).unwrap();
                assert_eq!(
                    patch,
                    json!([
                        {"op":"test","path":"/metadata/resourceVersion","value":"42"},
                        {"op":"replace","path":"/spec/resourceGroups/0/flavors/0/resources/1/nominalQuota","value":"9480m"},
                        {"op":"replace","path":"/spec/resourceGroups/0/flavors/0/resources/2/nominalQuota","value":"51536461824"},
                        {"op":"replace","path":"/spec/resourceGroups/0/flavors/0/resources/0/nominalQuota","value":"9"},
                        {"op":"replace","path":"/spec/resourceGroups/0/flavors/1/resources/1/nominalQuota","value":"6320m"},
                        {"op":"replace","path":"/spec/resourceGroups/0/flavors/1/resources/2/nominalQuota","value":"34357641216"},
                        {"op":"replace","path":"/spec/resourceGroups/0/flavors/1/resources/0/nominalQuota","value":"6"}
                    ])
                );
                // 20 cores, 80 GiB and 180 Pods are exclusively assigned. The
                // five assigned protected Pods, configured deductions, and
                // rendered PodSet cost produce this conserved global target;
                // the 100-core unmatched Node and its 90-core protected Pod do
                // not enter either side of the derivation.
                assert_eq!(9_480 + 6_320, 15_800);
                assert_eq!(51_536_461_824_i64 + 34_357_641_216, 85_894_103_040);
                assert_eq!(9 + 6, 15);
            }
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
            labels: BTreeMap::new(),
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
            contract: CapacityContract::VectorV1,
            source: CapacitySource::NodeSum,
            queue_name: "djinn-kueue".into(),
            node_selector_key: "kubernetes.io/hostname".into(),
            node_selector_value: "worker-1".into(),
            idle_cost: CpuMillicores::new(750).unwrap(),
            compile_cost: CpuMillicores::new(2_800).unwrap(),
            headroom: ResourceVector::ZERO,
            build_job: controller_build_job(),
            fail_safe: safe(),
            expected_protected_pods: 5,
            static_fallback: resources(12_000, 48 * 1024 * 1024 * 1024, 3),
            flavor_selector: None,
            flavor_selectors: BTreeMap::new(),
            nodepool_name: String::new(),
            nodepool_dedicated: false,
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
                contract: CapacityContract::VectorV1,
                source: CapacitySource::NodeSum,
                queue_name: "djinn-kueue".into(),
                node_selector_key: selector_key.into(),
                node_selector_value: "worker-1".into(),
                idle_cost: CpuMillicores::new(750).unwrap(),
                compile_cost: CpuMillicores::new(2_800).unwrap(),
                headroom: ResourceVector::ZERO,
                build_job: controller_build_job(),
                fail_safe: safe(),
                expected_protected_pods: 5,
                static_fallback: resources(12_000, 48 * 1024 * 1024 * 1024, 3),
                flavor_selector: None,
                flavor_selectors: BTreeMap::new(),
                nodepool_name: String::new(),
                nodepool_dedicated: false,
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
            let mutations = recorder.mutations();
            assert_eq!(
                mutations.len(),
                1,
                "incomplete node-sum restores static capacity"
            );
            let patch: Value = serde_json::from_str(&mutations[0].body).unwrap();
            assert_eq!(
                patch,
                json!([
                    {"op":"test","path":"/metadata/resourceVersion","value":"42"},
                    {"op":"replace","path":"/spec/resourceGroups/0/flavors/0/resources/1/nominalQuota","value":"12000m"},
                    {"op":"replace","path":"/spec/resourceGroups/0/flavors/0/resources/2/nominalQuota","value":"51539607552"}
                ])
            );
            // The fixture already declares the fail-safe Pod quota of three, so
            // the fenced fallback correctly omits a redundant Pods replacement.
            task.abort();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn capacity_mixed_version_contract() {
        use crate::runtime_fixture::{
            capacity_controller_cluster, capacity_controller_legacy_sentinel_cluster,
        };

        // This fixture owns its process environment because it verifies the
        // startup contract, rather than constructing a config by hand.
        let set = |name: &str, value: &str| unsafe { std::env::set_var(name, value) };
        let unset = |name: &str| unsafe { std::env::remove_var(name) };
        for (name, value) in [
            ("DJINN_CAPACITY_ENABLED", "true"),
            ("DJINN_CAPACITY_IDLE_CPU", "750m"),
            ("DJINN_CAPACITY_HEADROOM_CPU", "0m"),
            ("DJINN_CAPACITY_HEADROOM_MEMORY", "0"),
            ("DJINN_CAPACITY_HEADROOM_PODS", "0"),
            ("DJINN_CAPACITY_SOURCE", "static"),
            ("DJINN_CAPACITY_FLAVOR_SELECTOR", r#"{"pool":"default"}"#),
            ("DJINN_CAPACITY_STATIC_CPU", "12000m"),
            ("DJINN_CAPACITY_STATIC_MEMORY", "8192"),
            ("DJINN_CAPACITY_STATIC_PODS", "9"),
            ("DJINN_CAPACITY_QUEUE_NAME", "djinn-kueue"),
            ("DJINN_CAPACITY_NODE_SELECTOR_KEY", "kubernetes.io/hostname"),
            ("DJINN_CAPACITY_NODE_SELECTOR_VALUE", "worker-1"),
            ("DJINN_CAPACITY_COMPILE_CPU", "2800m"),
            ("DJINN_CAPACITY_FAIL_SAFE_PODS", "3"),
            ("DJINN_CAPACITY_FAIL_SAFE_COMPILE_SLOTS", "2"),
            ("DJINN_CAPACITY_EXPECTED_PROTECTED_PODS", "5"),
        ] {
            set(name, value);
        }

        // Absent marker is the old-chart lane, even with all new inputs and
        // sentinel-shaped topology present. It only changes the annotation
        // selected binding resource.
        unset("DJINN_CAPACITY_CONTRACT");
        let legacy = CapacityControllerConfig::from_env().expect("legacy chart config");
        assert_eq!(legacy.contract, CapacityContract::Legacy);
        assert_eq!(legacy.static_fallback, ResourceVector::ZERO);
        let (client, recorder) = capacity_controller_legacy_sentinel_cluster("default", "pods");
        let (tx, _) = watch::channel(CapacityVector {
            binding: BindingQuota::Pods(3),
            compile_slots: 2,
        });
        let task = tokio::spawn(run_capacity_controller(
            client,
            legacy,
            Arc::new(|| true),
            tx,
        ));
        for _ in 0..4 {
            tokio::time::advance(Duration::from_secs(30)).await;
            tokio::task::yield_now().await;
            if !recorder.mutations().is_empty() {
                break;
            }
        }
        task.abort();
        let legacy_patch: Value = serde_json::from_str(&recorder.mutations()[0].body).unwrap();
        assert_eq!(
            legacy_patch[0],
            json!({"op":"test","path":"/metadata/resourceVersion","value":"42"})
        );
        assert_eq!(legacy_patch.as_array().unwrap().len(), 2);
        assert_eq!(
            legacy_patch[1]["path"],
            "/spec/resourceGroups/0/flavors/0/resources/0/nominalQuota"
        );
        assert!(!recorder.mutations()[0].body.contains("10000"));
        assert!(!recorder.mutations()[0].body.contains("100Ti"));

        // A complete explicit vector declaration takes the landed named,
        // resourceVersion-fenced all-resource wire path.
        set("DJINN_CAPACITY_CONTRACT", "vector-v1");
        let vector = CapacityControllerConfig::from_env().expect("complete vector contract");
        assert_eq!(vector.contract, CapacityContract::VectorV1);
        let (client, recorder) = capacity_controller_cluster("default", "pods");
        let (tx, _) = watch::channel(CapacityVector {
            binding: BindingQuota::Pods(3),
            compile_slots: 2,
        });
        let task = tokio::spawn(run_capacity_controller(
            client,
            vector,
            Arc::new(|| true),
            tx,
        ));
        for _ in 0..4 {
            tokio::time::advance(Duration::from_secs(30)).await;
            tokio::task::yield_now().await;
            if !recorder.mutations().is_empty() {
                break;
            }
        }
        let patch: Value = serde_json::from_str(&recorder.mutations()[0].body).unwrap();
        task.abort();
        assert_eq!(
            patch,
            json!([
                {"op":"test","path":"/metadata/resourceVersion","value":"42"},
                {"op":"replace","path":"/spec/resourceGroups/0/flavors/0/resources/1/nominalQuota","value":"12000m"},
                {"op":"replace","path":"/spec/resourceGroups/0/flavors/0/resources/2/nominalQuota","value":"8192"},
                {"op":"replace","path":"/spec/resourceGroups/0/flavors/0/resources/0/nominalQuota","value":"9"}
            ])
        );

        // None of these activation defects produces a controller config, hence
        // no recorded fixture mutation and no partial-vector write.
        for (name, value) in [
            ("DJINN_CAPACITY_STATIC_CPU", None),
            ("DJINN_CAPACITY_STATIC_MEMORY", None),
            ("DJINN_CAPACITY_STATIC_PODS", None),
            ("DJINN_CAPACITY_FLAVOR_SELECTOR", None),
            ("DJINN_CAPACITY_FLAVOR_SELECTOR", Some("{}")),
            ("DJINN_CAPACITY_FLAVOR_SELECTOR", Some(r#"{"default":{}}"#)),
            ("DJINN_CAPACITY_STATIC_CPU", Some("NaN")),
            ("DJINN_CAPACITY_CONTRACT", Some("vector-v2")),
        ] {
            set("DJINN_CAPACITY_CONTRACT", "vector-v1");
            set("DJINN_CAPACITY_STATIC_CPU", "12000m");
            set("DJINN_CAPACITY_STATIC_MEMORY", "8192");
            set("DJINN_CAPACITY_STATIC_PODS", "9");
            set("DJINN_CAPACITY_FLAVOR_SELECTOR", r#"{"pool":"default"}"#);
            match value {
                Some(value) => set(name, value),
                None => unset(name),
            }
            let (_client, recorder) = capacity_controller_cluster("default", "pods");
            assert!(
                CapacityControllerConfig::from_env().is_none(),
                "{name}={value:?} must fail closed"
            );
            assert!(recorder.mutations().is_empty());
        }
    }
}
