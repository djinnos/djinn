//! Manifest-level CPU-accounting regression coverage for role-classed task runs.
//!
//! This is path-included by `job.rs` so it can exercise the real private renderer
//! surface without adding test helpers to production code.

use super::*;

#[derive(Debug)]
struct CpuContribution {
    collection: &'static str,
    container_name: String,
    rendered_quantity: String,
    parsed_millicores: u64,
}

#[derive(Debug)]
struct CpuInventory {
    contributions: Vec<CpuContribution>,
    total_millicores: u64,
}

fn contribution_diagnostics(inventory: &CpuInventory) -> Vec<String> {
    inventory
        .contributions
        .iter()
        .map(|contribution| {
            format!(
                "{}/{}: {} ({}m)",
                contribution.collection,
                contribution.container_name,
                contribution.rendered_quantity,
                contribution.parsed_millicores,
            )
        })
        .collect()
}

fn rendered_pod(job: &Job) -> &PodSpec {
    job.spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .expect("rendered Job must contain spec.template.spec")
}

/// Parse the CPU quantity forms emitted by this renderer without rounding a
/// fractional core value. Kubernetes accepts a bare core quantity or an `m`
/// millicore quantity; values more precise than one millicore are rejected.
fn parse_cpu_millicores(quantity: &str) -> Result<u64, String> {
    let quantity = quantity.trim();
    if quantity.is_empty() {
        return Err("empty quantity".to_owned());
    }

    if let Some(millicores) = quantity.strip_suffix('m') {
        return millicores
            .parse::<u64>()
            .map_err(|error| format!("invalid millicore quantity {quantity:?}: {error}"));
    }

    let (whole, fractional) = match quantity.split_once('.') {
        Some((whole, fractional)) => (whole, Some(fractional)),
        None => (quantity, None),
    };
    let whole_cores = whole
        .parse::<u64>()
        .map_err(|error| format!("invalid core quantity {quantity:?}: {error}"))?;
    let whole_millicores = whole_cores
        .checked_mul(1_000)
        .ok_or_else(|| format!("core quantity overflows millicores: {quantity:?}"))?;

    let Some(fractional) = fractional else {
        return Ok(whole_millicores);
    };
    if fractional.is_empty()
        || !fractional.bytes().all(|digit| digit.is_ascii_digit())
        || fractional.len() > 3
    {
        return Err(format!(
            "CPU quantity must be representable in whole millicores: {quantity:?}"
        ));
    }
    let fractional_millicores = fractional
        .parse::<u64>()
        .map_err(|error| format!("invalid fractional CPU quantity {quantity:?}: {error}"))?
        * 10_u64.pow((3 - fractional.len()) as u32);
    whole_millicores
        .checked_add(fractional_millicores)
        .ok_or_else(|| format!("CPU quantity overflows millicores: {quantity:?}"))
}

fn collect_cpu_requests(job: &Job) -> CpuInventory {
    let pod = rendered_pod(job);
    assert!(
        pod.containers
            .iter()
            .any(|container| container.name == "worker"),
        "rendered regular containers must include worker; got {:?}",
        pod.containers
            .iter()
            .map(|container| container.name.as_str())
            .collect::<Vec<_>>()
    );

    let mut contributions = Vec::new();
    for (collection, containers) in [
        ("containers", pod.containers.as_slice()),
        (
            "init_containers",
            pod.init_containers.as_deref().unwrap_or_default(),
        ),
    ] {
        for container in containers {
            let resources = container.resources.as_ref().unwrap_or_else(|| {
                panic!(
                    "{collection}/{} lacks resources while accounting PodSet CPU",
                    container.name
                )
            });
            let quantity = resources
                .requests
                .as_ref()
                .and_then(|requests| requests.get("cpu"))
                .unwrap_or_else(|| {
                    panic!(
                        "{collection}/{} lacks resources.requests.cpu while accounting PodSet CPU",
                        container.name
                    )
                })
                .0
                .clone();
            let parsed_millicores = parse_cpu_millicores(&quantity).unwrap_or_else(|error| {
                panic!(
                    "{collection}/{} has unparseable CPU request {quantity:?}: {error}",
                    container.name
                )
            });
            contributions.push(CpuContribution {
                collection,
                container_name: container.name.clone(),
                rendered_quantity: quantity,
                parsed_millicores,
            });
        }
    }

    let total_millicores = contributions
        .iter()
        .map(|contribution| contribution.parsed_millicores)
        .sum();
    CpuInventory {
        contributions,
        total_millicores,
    }
}

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

#[test]
fn task_run_pod_cpu_request_by_class() {
    // Every renderer input other than the role is shared: this proves the
    // complete returned PodSet changes only by its role-classed worker request.
    let config = KubernetesConfig::for_testing();
    let service = postgres_service();
    let task_run_id = Uuid::nil();
    let render = |role| {
        build_task_run_job(
            &config,
            &task_run_id,
            "role-cpu-project",
            "role-cpu-secret",
            "registry.example/djinn:role-cpu",
            std::slice::from_ref(&service),
            None,
            false,
            Some(role),
        )
    };

    let planner = collect_cpu_requests(&render(RoleKind::Planner));
    let worker = collect_cpu_requests(&render(RoleKind::Worker));

    // The explicit service must be observed in the manifest-derived inventory,
    // not merely supplied to the renderer. The launcher needs no special case:
    // if rendered it is already included by the init-container traversal.
    for (role, inventory) in [("Planner", &planner), ("Worker", &worker)] {
        assert!(
            inventory.contributions.iter().any(|contribution| {
                contribution.collection == "init_containers"
                    && contribution.container_name == "svc-postgres"
            }),
            "{role} inventory omitted rendered backing service; contributions: {:?}",
            contribution_diagnostics(inventory)
        );
    }

    assert!(
        planner.total_millicores < worker.total_millicores,
        "Planner whole-PodSet CPU request must be below Worker: planner={}m {:?}; worker={}m {:?}",
        planner.total_millicores,
        contribution_diagnostics(&planner),
        worker.total_millicores,
        contribution_diagnostics(&worker),
    );
}
