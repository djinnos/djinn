//! Rendered CPU quantities, in millicores.
//!
//! One place turns a Kubernetes CPU quantity into the millicores the launcher
//! and the build-slot weight policy both reason about, so the quota a workload
//! is charged and the CPU it is actually given can never drift. Split out of
//! `launcher.rs`, which is at the 51200-byte `Server Guards` ceiling; pure code
//! motion, re-exported through `launcher` so no import path changes.

use djinn_cgroup_launcher::LeasedQuota;
use k8s_openapi::api::core::v1::Container;

use crate::KubernetesConfig;
use crate::launcher::LAUNCHER_CONTAINER_NAME;

/// Environment variable carrying the lease ceiling (bare millicores) into the
/// launcher sidecar.
///
/// Named once because three places have to agree on it: the sidecar render
/// (`launcher::launcher_sidecar_container`), the post-render retune below, and
/// the `resize-v2` CPU ceiling (`launcher::apply_launcher_authority_protocol`),
/// which reads this value back out rather than recomputing it — see
/// [`rendered_lease_millicores`].
pub const LEASED_MILLICORES_ENV: &str = "DJINN_LAUNCHER_LEASED_MILLICORES";

/// Re-point the rendered launcher's lease ceiling at `cpu_limit`.
///
/// The sidecar is rendered from the deployment default, but a per-project
/// `build_resources.task.cpu_limit` override is applied to the built Job
/// afterwards. This keeps the lease equal to the CPU limit the pod actually
/// carries; see [`crate::build_resources::apply_resolved_resources`], which is
/// the only caller and applies both in one place so they cannot drift.
///
/// A limit that does not map onto a usable lease quota leaves the rendered
/// value in place rather than writing something unbounded: the render-time gate
/// in [`validate_enforcement_render`] has already refused to arm such a config,
/// so this is the belt to that braces.
pub fn retune_launcher_lease(pod: &mut k8s_openapi::api::core::v1::PodSpec, cpu_limit: &str) {
    let Some(millicores) = parse_cpu_millicores(cpu_limit) else {
        return;
    };
    let value = millicores.to_string();
    for container in pod.init_containers.iter_mut().flatten() {
        if container.name != LAUNCHER_CONTAINER_NAME {
            continue;
        }
        for env in container.env.iter_mut().flatten() {
            if env.name == LEASED_MILLICORES_ENV {
                env.value = Some(value.clone());
            }
        }
    }
}

/// The lease ceiling actually rendered onto a launcher sidecar, in millicores.
///
/// Deliberately reads the container back rather than recomputing
/// [`launcher_leased_millicores`] from the config: [`retune_launcher_lease`]
/// may already have re-pointed it at a per-project
/// `build_resources.task.cpu_limit` override, and a `resize-v2` CPU ceiling
/// derived from the deployment default would then clamp such a pod *below* the
/// lease its own launcher grants — the 7deu ancestor clamp, re-entered through
/// the override path.
///
/// `Err` carries the raw value for the error message; `Ok` is a value that
/// parses as a positive millicore count. The env is written as bare millicores
/// (no `m` suffix) by both writers, so this parse is deliberately strict.
pub(crate) fn rendered_lease_millicores(container: &Container) -> Result<u32, String> {
    let raw = container
        .env
        .iter()
        .flatten()
        .find(|entry| entry.name == LEASED_MILLICORES_ENV)
        .and_then(|entry| entry.value.as_deref());
    let Some(raw) = raw else {
        return Err("unset".to_string());
    };
    raw.trim()
        .parse::<u32>()
        .ok()
        .filter(|millicores| *millicores > 0)
        .ok_or_else(|| format!("{raw:?}"))
}

/// The CPU quota (millicores) a granted lease lifts an invocation leaf to.
///
/// Derived from the pod's declared CPU limit, so the ceiling a build actually
/// runs under is the ceiling the manifest advertises. Parsing mirrors
/// `crate::job::cpu_limit_to_jobs`: a bare number is cores, an `m` suffix is
/// millicores, and anything unparseable falls back to the launcher crate's own
/// default rather than to something unbounded.
pub fn launcher_leased_millicores(config: &KubernetesConfig) -> u32 {
    parse_cpu_millicores(&config.cpu_limit).unwrap_or(LeasedQuota::DEFAULT_MILLICORES)
}

/// The graph-warm Job's rendered CPU request, in millicores.
///
/// Exposed so build-slot weighting is derived from the SAME value that renders
/// the warm Job, rather than from a number typed into the admission layer. On
/// the default render this is 4000m -- identical to
/// [`launcher_leased_millicores`], which is why a warm Job and a leased task
/// invocation weigh exactly one build slot each.
#[must_use]
pub fn warm_job_millicores(config: &KubernetesConfig) -> u32 {
    parse_cpu_millicores(&config.warm_cpu_request)
        .or_else(|| parse_cpu_millicores(&config.warm_cpu_limit))
        .unwrap_or(LeasedQuota::DEFAULT_MILLICORES)
}

pub(crate) fn parse_cpu_millicores(quantity: &str) -> Option<u32> {
    let quantity = quantity.trim();
    match quantity.strip_suffix('m') {
        Some(millis) => millis.parse::<u32>().ok(),
        None => quantity
            .parse::<f64>()
            .ok()
            .filter(|cores| cores.is_finite() && *cores >= 0.0)
            .map(|cores| (cores * 1000.0) as u32),
    }
    .filter(|millicores| {
        (LeasedQuota::MIN_MILLICORES..=LeasedQuota::MAX_MILLICORES).contains(millicores)
    })
}
