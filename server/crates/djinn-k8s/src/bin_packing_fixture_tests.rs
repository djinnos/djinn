//! Exact-arithmetic bin-packing fixture for the role-classed enforcement pod
//! shape rendered by qut0 (`launcher.rs` / `sidecar.rs` / `warm_job.rs`).
//!
//! This is a **pure-arithmetic** guard, NOT a scheduler simulation: it parses
//! the CPU/memory `Quantity` strings that qut0 actually renders into the worker
//! container, the mandatory cgroup-launcher sidecar, and a default backing
//! service sidecar, converts them to exact integer millicores / bytes, and does
//! integer addition against a named 12-CPU / 48Gi node fixture with a protected
//! core (3.4 CPU / 8Gi) and one warm Job (4 CPU / 2Gi, sourced from the rendered
//! warm manifest).
//!
//! Why the values are *rendered* rather than typed as literals: every request
//! this fixture reasons about comes back out of the real render path
//! (`worker_resources`, `launcher_sidecar_container`, `sidecar_container`,
//! `build_warm_job`, and the sidecar default envelope via [`super::parse_resources`]).
//! A change to any role request, the launcher/sidecar request, or the default
//! backing-service envelope therefore flows straight into these sums, so the
//! pinned per-component assertions and the zero-slack build-capable identity
//! fail loudly (AC3). This module lives as a child of `sidecar` purely so it can
//! read the private default-resource envelope from [`super::parse_resources`]
//! without adding any production surface — it is `#[cfg(test)]` only.
//!
//! Reconciled sums (all CPU in millicores):
//!   node                = 12000        (12 CPU)
//!   protected core      =  3400        (3.4 CPU)   — fixture constant
//!   warm Job            =  4000        (4 CPU)     — rendered
//!   remaining           =  4600        (node - protected - warm)
//!   light pod           =   350        (worker 300 + launcher 50)
//!   light pod + sidecar =   450        (+ backing sidecar 100)
//!   build-capable pod   =  1150        (worker 1000 + launcher 50 + sidecar 100)
//!
//!   10 light, no sidecar : 3400 + 4000 + 10*350  = 10900 <= 12000  (fits)
//!   10 light, + sidecar  : 3400 + 4000 + 10*450  = 11900 <= 12000  (fits, 100m slack)
//!   4 build-capable      : 3400 + 4000 +  4*1150 = 12000 == 12000  (exactly full)
//!   5 build-capable      : 3400 + 4000 +  5*1150 = 13150 >  12000  (infeasible)

use k8s_openapi::api::core::v1::ResourceRequirements;

use super::{BackingServiceSpec, parse_resources, sidecar_container};
use crate::config::KubernetesConfig;
use crate::launcher::{RoleResourceClass, launcher_sidecar_container, worker_resources};
use crate::warm_job::build_warm_job;

// ---- Named node / protected-core fixture constants ------------------------
//
// The node shape and the protected core are *environment* inputs (not rendered
// by qut0), so they are encoded here as explicit named values. Any future
// change to them is a deliberate edit that reruns every sum below.

/// Total allocatable CPU of the fixture node, in millicores (12 CPU).
const NODE_CPU_MILLICORES: i64 = 12_000;
/// Total allocatable memory of the fixture node, in bytes (48 GiB).
const NODE_MEMORY_BYTES: i64 = 48 * GIB;
/// Protected-core (djinn-server + always-on infra) CPU reservation, millicores.
const PROTECTED_CORE_CPU_MILLICORES: i64 = 3_400;
/// Protected-core memory reservation, in bytes (8 GiB).
const PROTECTED_CORE_MEMORY_BYTES: i64 = 8 * GIB;

const KIB: i64 = 1 << 10;
const MIB: i64 = 1 << 20;
const GIB: i64 = 1 << 30;

const IMAGE: &str = "registry.example/proj:fixture";

// ---- Exact Quantity parsers ----------------------------------------------

/// Parse a rendered CPU `Quantity` string to exact integer millicores.
///
/// Only the two forms qut0 actually renders are accepted: `"<n>m"` (millicores)
/// and `"<n>"` (whole cores). Anything else (a fractional-core string, a bare
/// float) is a render-shape change we want to notice, so it panics rather than
/// silently rounding.
fn cpu_millicores(q: &str) -> i64 {
    match q.strip_suffix('m') {
        Some(m) => m
            .parse()
            .unwrap_or_else(|_| panic!("non-integer millicore quantity: {q:?}")),
        None => {
            q.parse::<i64>()
                .unwrap_or_else(|_| panic!("non-integer whole-core quantity: {q:?}"))
                * 1000
        }
    }
}

/// Parse a rendered memory `Quantity` string to exact integer bytes.
fn memory_bytes(q: &str) -> i64 {
    for (suffix, mult) in [("Gi", GIB), ("Mi", MIB), ("Ki", KIB)] {
        if let Some(n) = q.strip_suffix(suffix) {
            return n
                .parse::<i64>()
                .unwrap_or_else(|_| panic!("non-integer memory quantity: {q:?}"))
                * mult;
        }
    }
    q.parse::<i64>()
        .unwrap_or_else(|_| panic!("unrecognized memory quantity: {q:?}"))
}

fn request_cpu(r: &ResourceRequirements) -> i64 {
    cpu_millicores(
        &r.requests
            .as_ref()
            .expect("requests set")
            .get("cpu")
            .expect("cpu request set")
            .0,
    )
}

fn request_memory(r: &ResourceRequirements) -> i64 {
    memory_bytes(
        &r.requests
            .as_ref()
            .expect("requests set")
            .get("memory")
            .expect("memory request set")
            .0,
    )
}

// ---- Rendered per-component requests (the single source of truth) ----------

/// A default backing-service sidecar spec: the CPU/memory *request* is the
/// default envelope the dispatch path falls back to (rendered via the private
/// [`super::parse_resources`] with an empty JSON object), so a change to that
/// default flows into the sums below.
fn default_backing_sidecar_spec() -> BackingServiceSpec {
    let (cpu_request, memory_request, cpu_limit, memory_limit) = parse_resources("{}");
    BackingServiceSpec {
        service_type: "postgres".into(),
        image: "postgres:18-alpine".into(),
        port: 5432,
        env: Vec::new(),
        cpu_request,
        memory_request,
        cpu_limit,
        memory_limit,
        conn_template: "postgres://postgres:postgres@{host}:{port}/app_test".into(),
        conn_env_var: "DATABASE_URL".into(),
    }
}

/// All rendered requests the fixture reasons about, in exact integer units.
struct Rendered {
    light_worker_cpu: i64,
    light_worker_mem: i64,
    build_worker_cpu: i64,
    build_worker_mem: i64,
    launcher_cpu: i64,
    launcher_mem: i64,
    sidecar_cpu: i64,
    sidecar_mem: i64,
    warm_cpu: i64,
    warm_mem: i64,
}

impl Rendered {
    fn from_render() -> Self {
        let cfg = KubernetesConfig::for_testing();

        let light = worker_resources(&cfg, RoleResourceClass::Light);
        let build = worker_resources(&cfg, RoleResourceClass::BuildCapable);

        let launcher = launcher_sidecar_container(&cfg, IMAGE);
        let launcher_res = launcher.resources.as_ref().expect("launcher resources");

        let sidecar = sidecar_container(&cfg, &default_backing_sidecar_spec());
        let sidecar_res = sidecar.resources.as_ref().expect("sidecar resources");

        // One rendered warm Job; parse the warm container's own requests so a
        // change to the warm resource render (not just the config field) shows.
        let warm = build_warm_job(&cfg, "fixture", IMAGE, None);
        let warm_container = &warm
            .spec
            .as_ref()
            .expect("warm job spec")
            .template
            .spec
            .as_ref()
            .expect("warm pod spec")
            .containers[0];
        let warm_res = warm_container.resources.as_ref().expect("warm resources");

        Self {
            light_worker_cpu: request_cpu(&light),
            light_worker_mem: request_memory(&light),
            build_worker_cpu: request_cpu(&build),
            build_worker_mem: request_memory(&build),
            launcher_cpu: request_cpu(launcher_res),
            launcher_mem: request_memory(launcher_res),
            sidecar_cpu: request_cpu(sidecar_res),
            sidecar_mem: request_memory(sidecar_res),
            warm_cpu: request_cpu(warm_res),
            warm_mem: request_memory(warm_res),
        }
    }

    fn light_pod_cpu(&self) -> i64 {
        self.light_worker_cpu + self.launcher_cpu
    }
    fn light_pod_mem(&self) -> i64 {
        self.light_worker_mem + self.launcher_mem
    }
    fn build_pod_cpu(&self) -> i64 {
        self.build_worker_cpu + self.launcher_cpu + self.sidecar_cpu
    }
    fn build_pod_mem(&self) -> i64 {
        self.build_worker_mem + self.launcher_mem + self.sidecar_mem
    }
    fn reserved_cpu(&self) -> i64 {
        PROTECTED_CORE_CPU_MILLICORES + self.warm_cpu
    }
    fn reserved_mem(&self) -> i64 {
        PROTECTED_CORE_MEMORY_BYTES + self.warm_mem
    }
}

// ---- AC3: every request the sums depend on is pinned to its rendered value --
//
// This is the "fail loudly on any future change" guard. Each rendered request
// is asserted equal to the exact value the reconciled arithmetic assumes; a
// change to a role request, the launcher/sidecar request, the default
// backing-service envelope, or the warm/protected-core constants breaks a named
// assertion here with the offending value in the message.

#[test]
fn rendered_requests_match_the_bin_packing_fixture_values() {
    let r = Rendered::from_render();

    // Role-classed worker CPU requests.
    assert_eq!(r.light_worker_cpu, 300, "light worker CPU request changed");
    assert_eq!(
        r.build_worker_cpu, 1000,
        "build-capable worker CPU request changed"
    );
    // Shared worker memory request (same across roles).
    assert_eq!(r.light_worker_mem, 2 * GIB, "worker memory request changed");
    assert_eq!(
        r.build_worker_mem, r.light_worker_mem,
        "worker memory request must be role-independent"
    );

    // Launcher sidecar (role-independent, fixed envelope).
    assert_eq!(r.launcher_cpu, 50, "launcher sidecar CPU request changed");
    assert_eq!(
        r.launcher_mem,
        64 * MIB,
        "launcher sidecar memory request changed"
    );

    // Default backing-service sidecar envelope.
    assert_eq!(
        r.sidecar_cpu, 100,
        "default backing sidecar CPU request changed"
    );
    assert_eq!(
        r.sidecar_mem,
        256 * MIB,
        "default backing sidecar memory request changed"
    );

    // Warm Job.
    assert_eq!(r.warm_cpu, 4000, "warm Job CPU request changed");
    assert_eq!(r.warm_mem, 2 * GIB, "warm Job memory request changed");

    // Node / protected-core fixture constants.
    assert_eq!(NODE_CPU_MILLICORES, 12_000);
    assert_eq!(NODE_MEMORY_BYTES, 48 * GIB);
    assert_eq!(PROTECTED_CORE_CPU_MILLICORES, 3_400);
    assert_eq!(PROTECTED_CORE_MEMORY_BYTES, 8 * GIB);
}

// ---- AC1: ten light pods + protected core + warm Job, without and with a
//           default backing sidecar each, by exact arithmetic ----------------

#[test]
fn ten_light_pods_fit_alongside_protected_core_and_warm_without_sidecars() {
    let r = Rendered::from_render();

    // CPU: 3400 + 4000 + 10*(300+50) = 10900 <= 12000.
    let total_cpu = r.reserved_cpu() + 10 * r.light_pod_cpu();
    assert_eq!(r.light_pod_cpu(), 350);
    assert_eq!(total_cpu, 10_900);
    assert!(
        total_cpu <= NODE_CPU_MILLICORES,
        "ten light pods (no sidecar) must fit on CPU: {total_cpu} > {NODE_CPU_MILLICORES}"
    );
    assert_eq!(
        NODE_CPU_MILLICORES - total_cpu,
        1_100,
        "CPU slack for ten sidecar-less light pods"
    );

    // Memory: 10Gi reserved + 10*(2Gi+64Mi) well under 48Gi.
    let total_mem = r.reserved_mem() + 10 * r.light_pod_mem();
    assert!(
        total_mem <= NODE_MEMORY_BYTES,
        "ten light pods (no sidecar) must fit on memory: {total_mem} > {NODE_MEMORY_BYTES}"
    );
}

#[test]
fn ten_light_pods_fit_alongside_protected_core_and_warm_with_one_sidecar_each() {
    let r = Rendered::from_render();

    // CPU: 3400 + 4000 + 10*(300+50+100) = 11900 <= 12000 (only 100m slack).
    let pod_cpu = r.light_pod_cpu() + r.sidecar_cpu;
    let total_cpu = r.reserved_cpu() + 10 * pod_cpu;
    assert_eq!(pod_cpu, 450);
    assert_eq!(total_cpu, 11_900);
    assert!(
        total_cpu <= NODE_CPU_MILLICORES,
        "ten light pods (one sidecar each) must fit on CPU: {total_cpu} > {NODE_CPU_MILLICORES}"
    );
    assert_eq!(
        NODE_CPU_MILLICORES - total_cpu,
        100,
        "CPU slack for ten light pods each with one backing sidecar"
    );

    // Memory: 10Gi reserved + 10*(2Gi+64Mi+256Mi) still under 48Gi.
    let pod_mem = r.light_pod_mem() + r.sidecar_mem;
    let total_mem = r.reserved_mem() + 10 * pod_mem;
    assert!(
        total_mem <= NODE_MEMORY_BYTES,
        "ten light pods (one sidecar each) must fit on memory: {total_mem} > {NODE_MEMORY_BYTES}"
    );
}

// ---- AC2: four build-capable pods total exactly 12 CPU; a fifth is infeasible

#[test]
fn four_build_capable_pods_with_launcher_and_sidecar_total_exactly_twelve_cpu() {
    let r = Rendered::from_render();

    // build-capable pod = worker 1000 + launcher 50 + sidecar 100 = 1150m.
    assert_eq!(r.build_pod_cpu(), 1150);

    // Exactly full: 3400 + 4000 + 4*1150 = 12000 == node.
    let total_four = r.reserved_cpu() + 4 * r.build_pod_cpu();
    assert_eq!(
        total_four, NODE_CPU_MILLICORES,
        "four build-capable pods must total EXACTLY 12 CPU with protected core + warm"
    );

    // Zero slack: the room left after protected core + warm is exactly four
    // build-capable pods. This is the assertion that makes any future
    // zero-slack request change explicit.
    assert_eq!(
        NODE_CPU_MILLICORES - r.reserved_cpu(),
        4 * r.build_pod_cpu(),
        "remaining CPU after protected core + warm must equal exactly four build-capable pods"
    );

    // Memory is NOT the binding constraint here — confirm four build pods fit.
    let total_mem = r.reserved_mem() + 4 * r.build_pod_mem();
    assert!(
        total_mem <= NODE_MEMORY_BYTES,
        "four build-capable pods must fit on memory: {total_mem} > {NODE_MEMORY_BYTES}"
    );
}

#[test]
fn a_fifth_build_capable_pod_is_infeasible_on_cpu() {
    let r = Rendered::from_render();

    // 3400 + 4000 + 5*1150 = 13150 > 12000.
    let total_five = r.reserved_cpu() + 5 * r.build_pod_cpu();
    assert_eq!(total_five, 13_150);
    assert!(
        total_five > NODE_CPU_MILLICORES,
        "a fifth build-capable pod must overcommit CPU: {total_five} <= {NODE_CPU_MILLICORES}"
    );
    // The overshoot is exactly one build-capable pod beyond a full node.
    assert_eq!(
        total_five - NODE_CPU_MILLICORES,
        r.build_pod_cpu(),
        "the fifth pod overshoots the full node by exactly one build-capable pod"
    );
}
