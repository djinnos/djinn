//! Contract test for per-project `build_resources` overrides (ri23 Part 3).
//!
//! Drives the `build_resources_v1.yaml` fixture through the public resolver
//! and render surface of `djinn-k8s`, proving:
//!
//! * unset per-kind fields inherit the deployment defaults;
//! * valid distinct task and warm CPU + memory quantities render **unchanged**
//!   into their corresponding Job/Pod specs;
//! * malformed, zero/negative, request-above-limit, below-minimum, and
//!   above-maximum values reject at resolution — so no Job is ever built;
//! * task and warm resolve independently;
//! * production retains the 4-vCPU deployment default.

use djinn_k8s::build_resources::{ResolveError, ResourceBounds};
use djinn_k8s::launcher::RoleResourceClass;
use djinn_k8s::{
    KubernetesConfig, apply_resolved_resources, resolve_task_run_resources, resolve_warm_resources,
};
use djinn_stack::resources::BuildResourceOverrides;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::ResourceRequirements;
use serde::Deserialize;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Case {
    name: String,
    kind: Kind,
    expect: Expect,
    #[serde(default)]
    overrides: Option<BuildResourceOverrides>,
    #[serde(default)]
    bounds: Option<ResourceBounds>,
    #[serde(default)]
    expected: Option<Expected>,
    #[serde(default)]
    inherits_defaults: bool,
    #[serde(default)]
    reject_kind: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Task,
    Warm,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Expect {
    Ok,
    Reject,
}

#[derive(Debug, Deserialize)]
struct Expected {
    cpu_request: String,
    cpu_limit: String,
    memory_request: String,
    memory_limit: String,
}

fn resolve(config: &KubernetesConfig, case: &Case) -> Result<ResourceRequirements, ResolveError> {
    let bounds = case.bounds.clone().unwrap_or_default();
    match case.kind {
        Kind::Task => resolve_task_run_resources(
            config,
            RoleResourceClass::BuildCapable,
            case.overrides.as_ref(),
            &bounds,
        ),
        Kind::Warm => resolve_warm_resources(config, case.overrides.as_ref(), &bounds),
    }
}

fn get(rr: &ResourceRequirements, section: &str, key: &str) -> String {
    let map = match section {
        "requests" => rr.requests.as_ref(),
        _ => rr.limits.as_ref(),
    };
    map.unwrap().get(key).unwrap().0.clone()
}

/// Render the resolved requirements into a real Job of the case's kind, then
/// return the primary container's resources — proving the quantities land on
/// the actual k8s spec, not just the resolver output.
fn render_into_job(
    config: &KubernetesConfig,
    kind: Kind,
    rr: ResourceRequirements,
) -> ResourceRequirements {
    let mut job: Job = match kind {
        Kind::Task => djinn_k8s::job::build_task_run_job(
            config,
            &Uuid::nil(),
            "proj",
            "secret",
            "img:tag",
            &[],
            None,
            false,
            None,
        ),
        Kind::Warm => djinn_k8s::build_warm_job(config, "proj", "img:tag", None),
    };
    apply_resolved_resources(&mut job, rr);
    job.spec
        .unwrap()
        .template
        .spec
        .unwrap()
        .containers
        .into_iter()
        .find(|c| c.name == "worker" || c.name == "warmer")
        .unwrap()
        .resources
        .unwrap()
}

fn reject_variant(err: &ResolveError) -> &'static str {
    match err {
        ResolveError::InvalidQuantity { .. } => "invalid_quantity",
        ResolveError::InvalidBound { .. } => "invalid_bound",
        ResolveError::RequestExceedsLimit { .. } => "request_exceeds_limit",
        ResolveError::BelowMinimum { .. } => "below_minimum",
        ResolveError::AboveMaximum { .. } => "above_maximum",
    }
}

#[test]
fn build_resources_v1_contract() {
    let raw = include_str!("fixtures/build_resources_v1.yaml");
    let fixture: Fixture = serde_yaml::from_str(raw).expect("fixture parses");
    let config = KubernetesConfig::for_testing();

    // Production retains the 4-vCPU deployment default.
    assert_eq!(config.cpu_limit, "4", "task-run cpu limit default");
    assert_eq!(config.warm_cpu_limit, "4", "warm cpu limit default");

    for case in &fixture.cases {
        let outcome = resolve(&config, case);
        match case.expect {
            Expect::Reject => {
                let err = outcome
                    .as_ref()
                    .err()
                    .unwrap_or_else(|| panic!("case {}: expected reject, got Ok", case.name));
                if let Some(expected_kind) = &case.reject_kind {
                    assert_eq!(
                        reject_variant(err),
                        expected_kind,
                        "case {}: wrong reject variant",
                        case.name
                    );
                }
                // Reject means no Job: we never reach render_into_job.
            }
            Expect::Ok => {
                let rr =
                    outcome.unwrap_or_else(|e| panic!("case {}: expected Ok, got {e}", case.name));

                if case.inherits_defaults {
                    let (cr, cl, mr, ml) = match case.kind {
                        Kind::Task => (
                            &config.cpu_request,
                            &config.cpu_limit,
                            &config.memory_request,
                            &config.memory_limit,
                        ),
                        Kind::Warm => (
                            &config.warm_cpu_request,
                            &config.warm_cpu_limit,
                            &config.warm_memory_request,
                            &config.warm_memory_limit,
                        ),
                    };
                    assert_eq!(&get(&rr, "requests", "cpu"), cr, "{}", case.name);
                    assert_eq!(&get(&rr, "limits", "cpu"), cl, "{}", case.name);
                    assert_eq!(&get(&rr, "requests", "memory"), mr, "{}", case.name);
                    assert_eq!(&get(&rr, "limits", "memory"), ml, "{}", case.name);
                }

                // Render into the real Job and assert the quantities land there
                // unchanged.
                let rendered = render_into_job(&config, case.kind, rr);
                if let Some(exp) = &case.expected {
                    assert_eq!(
                        get(&rendered, "requests", "cpu"),
                        exp.cpu_request,
                        "{}: cpu request",
                        case.name
                    );
                    assert_eq!(
                        get(&rendered, "limits", "cpu"),
                        exp.cpu_limit,
                        "{}: cpu limit",
                        case.name
                    );
                    assert_eq!(
                        get(&rendered, "requests", "memory"),
                        exp.memory_request,
                        "{}: memory request",
                        case.name
                    );
                    assert_eq!(
                        get(&rendered, "limits", "memory"),
                        exp.memory_limit,
                        "{}: memory limit",
                        case.name
                    );
                }
            }
        }
    }
}

/// Task and warm resolve independently: overriding one kind never perturbs the
/// other, which stays on its own deployment default.
#[test]
fn task_and_warm_resolve_independently() {
    let config = KubernetesConfig::for_testing();
    let task_only = BuildResourceOverrides {
        cpu_request: Some("2".into()),
        cpu_limit: Some("2".into()),
        memory_request: Some("7Gi".into()),
        memory_limit: Some("7Gi".into()),
    };
    let task = resolve_task_run_resources(
        &config,
        RoleResourceClass::BuildCapable,
        Some(&task_only),
        &ResourceBounds::default(),
    )
    .expect("task resolves");
    let warm =
        resolve_warm_resources(&config, None, &ResourceBounds::default()).expect("warm resolves");

    assert_eq!(get(&task, "requests", "memory"), "7Gi");
    // Warm ignored the task override entirely.
    assert_eq!(get(&warm, "requests", "memory"), config.warm_memory_request);
    assert_eq!(get(&warm, "limits", "cpu"), config.warm_cpu_limit);
}
