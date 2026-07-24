//! Resolution + render of per-project [`build_resources`] overrides.
//!
//! [`djinn_stack::resources::BuildResources`] carries optional per-project CPU
//! and memory overrides. This module combines them with the deployment-wide
//! defaults from [`KubernetesConfig`] — task-run and warm Pods resolved
//! **independently** — enforces the hard invariants, and produces the
//! [`ResourceRequirements`] rendered into the Job/Pod spec.
//!
//! Resolution fails closed: after defaults and overrides combine, each kind's
//! request must not exceed its limit, and every quantity must sit within any
//! administrator-configured per-kind minimum/maximum. A malformed, zero,
//! negative, request-above-limit, below-minimum, or above-maximum value
//! returns an error so the caller never creates a Job — values are never
//! clamped. When a project sets no override and the deployment configures no
//! bounds, the resolved requirements are byte-identical to
//! [`crate::launcher::worker_resources`] / the warm default, so the default
//! render path is unchanged.
//!
//! [`build_resources`]: djinn_stack::resources::BuildResources

use std::collections::BTreeMap;
use std::fmt;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::ResourceRequirements;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity as K8sQuantity;
use serde::{Deserialize, Serialize};

use djinn_stack::resources::{BuildResourceOverrides, Quantity};

use crate::config::KubernetesConfig;
use crate::launcher::RoleResourceClass;

/// Administrator-configured per-kind hard bounds for a Pod kind's CPU and
/// memory. Every bound is optional; `None` leaves that axis unbounded. Bounds
/// are enforced at resolution time: a resolved request below a minimum or a
/// resolved limit above a maximum fails closed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceBounds {
    /// Minimum acceptable CPU request.
    #[serde(default)]
    pub cpu_min: Option<String>,
    /// Maximum acceptable CPU limit.
    #[serde(default)]
    pub cpu_max: Option<String>,
    /// Minimum acceptable memory request.
    #[serde(default)]
    pub memory_min: Option<String>,
    /// Maximum acceptable memory limit.
    #[serde(default)]
    pub memory_max: Option<String>,
}

/// Why a `build_resources` resolution failed. Every variant means "reject the
/// config and do not create a Job".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// A resolved quantity was malformed, zero, or negative.
    InvalidQuantity { field: String, value: String },
    /// A configured administrator bound was itself not a valid quantity.
    InvalidBound { field: String, value: String },
    /// The resolved request exceeds the resolved limit for a resource axis.
    RequestExceedsLimit {
        resource: String,
        request: String,
        limit: String,
    },
    /// The resolved request is below the administrator minimum.
    BelowMinimum {
        resource: String,
        value: String,
        min: String,
    },
    /// The resolved limit is above the administrator maximum.
    AboveMaximum {
        resource: String,
        value: String,
        max: String,
    },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidQuantity { field, value } => {
                write!(f, "{field}: {value:?} is not a valid resource quantity")
            }
            Self::InvalidBound { field, value } => {
                write!(
                    f,
                    "{field}: configured bound {value:?} is not a valid quantity"
                )
            }
            Self::RequestExceedsLimit {
                resource,
                request,
                limit,
            } => write!(f, "{resource}: request {request:?} exceeds limit {limit:?}"),
            Self::BelowMinimum {
                resource,
                value,
                min,
            } => write!(f, "{resource}: request {value:?} is below minimum {min:?}"),
            Self::AboveMaximum {
                resource,
                value,
                max,
            } => write!(f, "{resource}: limit {value:?} is above maximum {max:?}"),
        }
    }
}

impl std::error::Error for ResolveError {}

/// Resolve the task-run Pod's resources: role-classed CPU request default
/// (light vs build-capable), shared CPU-limit / memory defaults, the optional
/// per-project `task` overrides, and the deployment's task bounds.
pub fn resolve_task_run_resources(
    config: &KubernetesConfig,
    class: RoleResourceClass,
    overrides: Option<&BuildResourceOverrides>,
    bounds: &ResourceBounds,
) -> Result<ResourceRequirements, ResolveError> {
    resolve(
        pick(cpu_request_of(overrides), class.cpu_request(config)),
        pick(cpu_limit_of(overrides), &config.cpu_limit),
        pick(memory_request_of(overrides), &config.memory_request),
        pick(memory_limit_of(overrides), &config.memory_limit),
        bounds,
    )
}

/// Resolve the warm Pod's resources from the warm deployment defaults, the
/// optional per-project `warm` overrides, and the deployment's warm bounds.
pub fn resolve_warm_resources(
    config: &KubernetesConfig,
    overrides: Option<&BuildResourceOverrides>,
    bounds: &ResourceBounds,
) -> Result<ResourceRequirements, ResolveError> {
    resolve(
        pick(cpu_request_of(overrides), &config.warm_cpu_request),
        pick(cpu_limit_of(overrides), &config.warm_cpu_limit),
        pick(memory_request_of(overrides), &config.warm_memory_request),
        pick(memory_limit_of(overrides), &config.warm_memory_limit),
        bounds,
    )
}

/// Overwrite the primary container's resources on an already-built Job with the
/// resolved requirements. Targets the `worker` (task-run) or `warmer` (warm)
/// container.
pub fn apply_resolved_resources(job: &mut Job, resources: ResourceRequirements) {
    if let Some(spec) = job.spec.as_mut()
        && let Some(pod) = spec.template.spec.as_mut()
        && let Some(container) = pod
            .containers
            .iter_mut()
            .find(|c| c.name == "worker" || c.name == "warmer")
    {
        container.resources = Some(resources);
    }
}

fn cpu_request_of(o: Option<&BuildResourceOverrides>) -> Option<&str> {
    o.and_then(|o| o.cpu_request.as_deref())
}
fn cpu_limit_of(o: Option<&BuildResourceOverrides>) -> Option<&str> {
    o.and_then(|o| o.cpu_limit.as_deref())
}
fn memory_request_of(o: Option<&BuildResourceOverrides>) -> Option<&str> {
    o.and_then(|o| o.memory_request.as_deref())
}
fn memory_limit_of(o: Option<&BuildResourceOverrides>) -> Option<&str> {
    o.and_then(|o| o.memory_limit.as_deref())
}

/// Override wins over the deployment default; the winner is trimmed to its
/// canonical (whitespace-free) Quantity form for the render.
fn pick<'a>(over: Option<&'a str>, default: &'a str) -> &'a str {
    over.unwrap_or(default).trim()
}

fn resolve(
    cpu_request: &str,
    cpu_limit: &str,
    memory_request: &str,
    memory_limit: &str,
    bounds: &ResourceBounds,
) -> Result<ResourceRequirements, ResolveError> {
    check_axis(
        "cpu",
        cpu_request,
        cpu_limit,
        bounds.cpu_min.as_deref(),
        bounds.cpu_max.as_deref(),
    )?;
    check_axis(
        "memory",
        memory_request,
        memory_limit,
        bounds.memory_min.as_deref(),
        bounds.memory_max.as_deref(),
    )?;
    Ok(ResourceRequirements {
        requests: Some(BTreeMap::from([
            ("cpu".to_string(), K8sQuantity(cpu_request.to_string())),
            (
                "memory".to_string(),
                K8sQuantity(memory_request.to_string()),
            ),
        ])),
        limits: Some(BTreeMap::from([
            ("cpu".to_string(), K8sQuantity(cpu_limit.to_string())),
            ("memory".to_string(), K8sQuantity(memory_limit.to_string())),
        ])),
        ..ResourceRequirements::default()
    })
}

fn check_axis(
    resource: &str,
    request: &str,
    limit: &str,
    min: Option<&str>,
    max: Option<&str>,
) -> Result<(), ResolveError> {
    let req = parse_positive(resource, "request", request)?;
    let lim = parse_positive(resource, "limit", limit)?;
    if req > lim {
        return Err(ResolveError::RequestExceedsLimit {
            resource: resource.to_string(),
            request: request.to_string(),
            limit: limit.to_string(),
        });
    }
    if let Some(min) = min {
        let min_q = parse_bound(resource, "min", min)?;
        if req < min_q {
            return Err(ResolveError::BelowMinimum {
                resource: resource.to_string(),
                value: request.to_string(),
                min: min.to_string(),
            });
        }
    }
    if let Some(max) = max {
        let max_q = parse_bound(resource, "max", max)?;
        if lim > max_q {
            return Err(ResolveError::AboveMaximum {
                resource: resource.to_string(),
                value: limit.to_string(),
                max: max.to_string(),
            });
        }
    }
    Ok(())
}

fn parse_positive(resource: &str, role: &str, value: &str) -> Result<Quantity, ResolveError> {
    match Quantity::parse(value) {
        Some(q) if q.is_positive() => Ok(q),
        _ => Err(ResolveError::InvalidQuantity {
            field: format!("{resource}_{role}"),
            value: value.to_string(),
        }),
    }
}

fn parse_bound(resource: &str, role: &str, value: &str) -> Result<Quantity, ResolveError> {
    Quantity::parse(value).ok_or_else(|| ResolveError::InvalidBound {
        field: format!("{resource}_{role}"),
        value: value.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> KubernetesConfig {
        KubernetesConfig::for_testing()
    }

    fn req(rr: &ResourceRequirements, key: &str) -> String {
        rr.requests.as_ref().unwrap().get(key).unwrap().0.clone()
    }
    fn lim(rr: &ResourceRequirements, key: &str) -> String {
        rr.limits.as_ref().unwrap().get(key).unwrap().0.clone()
    }

    #[test]
    fn unset_task_inherits_role_classed_defaults() {
        let c = cfg();
        let rr = resolve_task_run_resources(
            &c,
            RoleResourceClass::BuildCapable,
            None,
            &ResourceBounds::default(),
        )
        .expect("defaults resolve");
        assert_eq!(req(&rr, "cpu"), c.cpu_request);
        assert_eq!(lim(&rr, "cpu"), c.cpu_limit);
        assert_eq!(req(&rr, "memory"), c.memory_request);
        assert_eq!(lim(&rr, "memory"), c.memory_limit);

        let light = resolve_task_run_resources(
            &c,
            RoleResourceClass::Light,
            None,
            &ResourceBounds::default(),
        )
        .expect("light defaults resolve");
        assert_eq!(req(&light, "cpu"), c.light_cpu_request);
    }

    #[test]
    fn task_and_warm_resolve_independently() {
        let c = cfg();
        let br = BuildResourceOverrides {
            cpu_request: Some("2".into()),
            cpu_limit: Some("2".into()),
            memory_request: Some("3Gi".into()),
            memory_limit: Some("3Gi".into()),
        };
        // Only the task block is set; warm must fall back to warm defaults.
        let task = resolve_task_run_resources(
            &c,
            RoleResourceClass::BuildCapable,
            Some(&br),
            &ResourceBounds::default(),
        )
        .unwrap();
        let warm = resolve_warm_resources(&c, None, &ResourceBounds::default()).unwrap();
        assert_eq!(req(&task, "memory"), "3Gi");
        assert_eq!(req(&warm, "memory"), c.warm_memory_request);
        assert_ne!(req(&task, "memory"), req(&warm, "memory"));
    }

    #[test]
    fn request_above_limit_after_defaults_combine_rejects() {
        let c = cfg();
        // Only cpu_request overridden, to above the default cpu_limit ("4").
        let br = BuildResourceOverrides {
            cpu_request: Some("8".into()),
            ..Default::default()
        };
        let err = resolve_task_run_resources(
            &c,
            RoleResourceClass::BuildCapable,
            Some(&br),
            &ResourceBounds::default(),
        )
        .unwrap_err();
        assert!(matches!(err, ResolveError::RequestExceedsLimit { .. }));
    }

    #[test]
    fn below_min_and_above_max_reject() {
        let c = cfg();
        let bounds = ResourceBounds {
            cpu_min: Some("2".into()),
            cpu_max: Some("8".into()),
            memory_min: Some("1Gi".into()),
            memory_max: Some("16Gi".into()),
        };
        let below = BuildResourceOverrides {
            cpu_request: Some("1".into()),
            cpu_limit: Some("4".into()),
            ..Default::default()
        };
        assert!(matches!(
            resolve_task_run_resources(&c, RoleResourceClass::BuildCapable, Some(&below), &bounds),
            Err(ResolveError::BelowMinimum { .. })
        ));
        let above = BuildResourceOverrides {
            cpu_request: Some("4".into()),
            cpu_limit: Some("16".into()),
            ..Default::default()
        };
        assert!(matches!(
            resolve_task_run_resources(&c, RoleResourceClass::BuildCapable, Some(&above), &bounds),
            Err(ResolveError::AboveMaximum { .. })
        ));
    }

    #[test]
    fn apply_overwrites_primary_container() {
        let c = cfg();
        let id = uuid::Uuid::nil();
        let mut job = crate::job::build_task_run_job(
            &c,
            &id,
            "proj",
            "secret",
            "img:tag",
            &[],
            None,
            false,
            None,
        );
        let br = BuildResourceOverrides {
            cpu_request: Some("2".into()),
            cpu_limit: Some("3".into()),
            memory_request: Some("5Gi".into()),
            memory_limit: Some("6Gi".into()),
        };
        let rr = resolve_task_run_resources(
            &c,
            RoleResourceClass::BuildCapable,
            Some(&br),
            &ResourceBounds::default(),
        )
        .unwrap();
        apply_resolved_resources(&mut job, rr);
        let container = &job.spec.unwrap().template.spec.unwrap().containers[0];
        let res = container.resources.as_ref().unwrap();
        assert_eq!(res.requests.as_ref().unwrap().get("cpu").unwrap().0, "2");
        assert_eq!(res.limits.as_ref().unwrap().get("memory").unwrap().0, "6Gi");
    }
}
