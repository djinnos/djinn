//! Identity-fenced wire decisions for the derived-capacity controller.
//!
//! Observation and actuation are kept separate so every ambiguous read has a
//! closed, testable outcome and cannot accidentally become a PATCH.

use k8s_openapi::api::core::v1::{Node, Pod};
use kube::{
    Api, Client,
    api::{ApiResource, DynamicObject, GroupVersionKind, ListParams, Patch, PatchParams},
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::capacity::{CapacityOutcome, CpuMillicores, DerivedCapacity, FailSafeCapacity};
use crate::capacity::{DerivationInputs, derive, scheduler_effective_request};
use crate::capacity_damping::{BindingQuota, CapacityVector};
use crate::capacity_damping::{CapacityDamper, SampleKind};

pub const QUOTA_OWNER_LABEL: &str = "djinn.io/quota-owner";
pub const DERIVED_CAPACITY_OWNER: &str = "derived-capacity";
pub const BINDING_RESOURCE_ANNOTATION: &str = "djinn.io/binding-resource";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeObservation {
    pub name: String,
    pub selector_matches: bool,
    pub terminating: bool,
    pub allocatable_cpu: Option<CpuMillicores>,
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
        Some(Self {
            queue_name: std::env::var("DJINN_CAPACITY_QUEUE_NAME").ok()?,
            node_selector_key: std::env::var("DJINN_CAPACITY_NODE_SELECTOR_KEY").ok()?,
            node_selector_value: std::env::var("DJINN_CAPACITY_NODE_SELECTOR_VALUE").ok()?,
            idle_cost: parse_cpu("DJINN_CAPACITY_IDLE_CPU")?,
            compile_cost: parse_cpu("DJINN_CAPACITY_COMPILE_CPU")?,
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

fn cpu_quantity(value: &str) -> Option<CpuMillicores> {
    if let Some(m) = value.strip_suffix('m') {
        return CpuMillicores::new(m.parse().ok()?).ok();
    }
    CpuMillicores::new(value.parse::<i64>().ok()?.checked_mul(1_000)?).ok()
}

fn pod_request(pod: &Pod) -> Option<CpuMillicores> {
    let spec = pod.spec.as_ref()?;
    let requests = |container: &k8s_openapi::api::core::v1::Container| {
        container
            .resources
            .as_ref()?
            .requests
            .as_ref()?
            .get("cpu")
            .and_then(|q| cpu_quantity(&q.0))
    };
    scheduler_effective_request(
        spec.containers
            .iter()
            .map(requests)
            .collect::<Option<Vec<_>>>()?,
        spec.init_containers
            .iter()
            .flatten()
            .map(requests)
            .collect::<Option<Vec<_>>>()?,
    )
    .ok()
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
                .map(|node| NodeObservation {
                    name: node.metadata.name.unwrap_or_default(),
                    selector_matches: true,
                    terminating: node.metadata.deletion_timestamp.is_some(),
                    allocatable_cpu: node
                        .status
                        .and_then(|s| s.allocatable)
                        .and_then(|a| a.get("cpu").and_then(|q| cpu_quantity(&q.0))),
                })
                .collect();
            let node = select_node(&node_observations).ok()?;
            let protected = pods
                .list(&ListParams::default().labels("djinn.io/capacity-reserved=true"))
                .await
                .ok()?;
            if protected.items.len() < config.expected_protected_pods {
                return None;
            }
            let protected_cpu = protected
                .items
                .iter()
                .try_fold(0_i64, |sum, pod| sum.checked_add(pod_request(pod)?.get()))?;
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
            let capacity = derive(
                DerivationInputs {
                    allocatable: node.allocatable_cpu?,
                    protected: CpuMillicores::new(protected_cpu).ok()?,
                    idle_cost: config.idle_cost,
                    compile_cost: config.compile_cost,
                },
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
    fn quota_controller_wire_serializes_resource_typed_values() {
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
    }

    #[test]
    fn quota_controller_node_selection_requires_one_stable_identity() {
        let node = |name: &str, matches| NodeObservation {
            name: name.into(),
            selector_matches: matches,
            terminating: false,
            allocatable_cpu: Some(CpuMillicores::new(12_000).unwrap()),
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
        let second = node("b", true);
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
    fn quota_controller_failsafe_returns_no_mutation_on_conservative_input() {
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
}
