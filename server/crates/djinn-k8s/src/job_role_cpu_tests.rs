//! Manifest-level CPU-accounting regression coverage for role-classed task runs.
//!
//! This is path-included by `job.rs` so it can exercise the real private renderer
//! surface without adding test helpers to production code.

use super::*;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CpuContribution {
    collection: &'static str,
    container_name: String,
    rendered_quantity: String,
    parsed_millicores: u64,
}

fn contribution_diagnostic(contribution: &CpuContribution) -> String {
    format!(
        "{}/{}: {} ({}m)",
        contribution.collection,
        contribution.container_name,
        contribution.rendered_quantity,
        contribution.parsed_millicores,
    )
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

fn cpu_contribution(collection: &'static str, container: &Container) -> CpuContribution {
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
    CpuContribution {
        collection,
        container_name: container.name.clone(),
        rendered_quantity: quantity,
        parsed_millicores,
    }
}

fn inventory_from_contributions(mut contributions: Vec<CpuContribution>) -> CpuInventory {
    contributions.sort();
    let total_millicores = contributions
        .iter()
        .map(|contribution| contribution.parsed_millicores)
        .sum();
    CpuInventory {
        contributions,
        total_millicores,
    }
}

/// The inventory consumed by the total assertion. Keep this traversal separate
/// from `complete_manifest_cpu_inventory`: the latter is the independent
/// contract that catches an accidental omission here.
fn accounting_cpu_inventory(job: &Job) -> CpuInventory {
    let pod = rendered_pod(job);
    let mut contributions = Vec::new();
    for (collection, containers) in [
        ("containers", pod.containers.as_slice()),
        (
            "init_containers",
            pod.init_containers.as_deref().unwrap_or_default(),
        ),
    ] {
        contributions.extend(
            containers
                .iter()
                .map(|container| cpu_contribution(collection, container)),
        );
    }
    inventory_from_contributions(contributions)
}

/// Independently enumerate both manifest collections. This intentionally does
/// not reuse the accounting traversal, so an accounting change that only walks
/// `containers` or filters an init sidecar is reported as an inventory diff.
fn complete_manifest_cpu_inventory(job: &Job) -> CpuInventory {
    let pod = rendered_pod(job);
    let regular = pod
        .containers
        .iter()
        .map(|container| cpu_contribution("containers", container));
    let init = pod
        .init_containers
        .iter()
        .flatten()
        .map(|container| cpu_contribution("init_containers", container));
    inventory_from_contributions(regular.chain(init).collect())
}

fn assert_complete_cpu_accounting(role: &str, job: &Job) -> CpuInventory {
    let accounting = accounting_cpu_inventory(job);
    let manifest = complete_manifest_cpu_inventory(job);
    let missing: Vec<_> = manifest
        .contributions
        .iter()
        .filter(|contribution| !accounting.contributions.contains(contribution))
        .map(contribution_diagnostic)
        .collect();
    let extra: Vec<_> = accounting
        .contributions
        .iter()
        .filter(|contribution| !manifest.contributions.contains(contribution))
        .map(contribution_diagnostic)
        .collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "{role} PodSet CPU accounting inventory differs from complete rendered manifest; missing: {missing:?}; extra: {extra:?}; manifest: {:?}; accounting: {:?}",
        contribution_diagnostics(&manifest),
        contribution_diagnostics(&accounting),
    );
    accounting
}

fn assert_named_contribution(
    role: &str,
    inventory: &CpuInventory,
    collection: &'static str,
    name: &str,
) {
    assert!(
        inventory.contributions.iter().any(|contribution| {
            contribution.collection == collection && contribution.container_name == name
        }),
        "{role} CPU accounting must include rendered {collection}/{name}; contributions: {:?}",
        contribution_diagnostics(inventory)
    );
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

    let planner_job = render(RoleKind::Planner);
    let worker_job = render(RoleKind::Worker);
    let planner = assert_complete_cpu_accounting("Planner", &planner_job);
    let worker = assert_complete_cpu_accounting("Worker", &worker_job);

    // These are rendered names, not CPU constants: they require that each role's
    // accounting sees the worker and the explicit backing service. The launcher
    // needs no special case: if it is rendered, the manifest comparison requires
    // it; if it is absent, no synthetic contribution is expected.
    for (role, inventory) in [("Planner", &planner), ("Worker", &worker)] {
        assert_named_contribution(role, inventory, "containers", "worker");
        assert_named_contribution(role, inventory, "init_containers", "svc-postgres");
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
