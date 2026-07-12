//! [`ImageControllerConfig`] — env-driven runtime knobs for the controller.

use std::num::ParseIntError;

/// Default buildkitd DNS (matches `buildkitd-service.yaml` shipped in PR 4).
const DEFAULT_BUILDKITD_HOST: &str = "tcp://djinn-buildkitd.djinn.svc.cluster.local:1234";
/// Default Zot registry DNS (matches `zot-service.yaml` shipped in PR 4).
const DEFAULT_REGISTRY_HOST: &str = "djinn-zot.djinn.svc.cluster.local:5000";
/// Default builder-Pod image. Post-P5 this is just a bash+buildctl
/// shell — moby/buildkit ships `buildctl` alongside buildkitd. Operators
/// can repoint this at a custom image via `DJINN_IMAGE_BUILDER_IMAGE`.
const DEFAULT_BUILDER_IMAGE: &str = "moby/buildkit:latest";
/// Default agent-worker helper image. The generated Dockerfile COPYs the
/// `djinn-agent-worker` binary out of this image. The ref must pin
/// something stable (sha-tagged in prod, `:dev` for Tilt).
const DEFAULT_AGENT_WORKER_IMAGE: &str = "djinn/agent-runtime:dev";
/// Default namespace for build Jobs + registry-auth Secret lookup.
const DEFAULT_NAMESPACE: &str = "djinn";
/// Default registry-auth Secret name referenced by the build-Job Pod spec.
const DEFAULT_REGISTRY_AUTH_SECRET: &str = "djinn-zot-auth";
/// Default PVC claim name holding the per-project bare mirrors.
const DEFAULT_MIRROR_PVC: &str = "djinn-mirrors";
/// Default ServiceAccount the build Pod runs under. The build Pod has to
/// authenticate to a managed registry (ECR/GCR/ACR) via the cluster's
/// IRSA-style annotations on this SA — see
/// `deploy/helm/djinn/templates/serviceaccount-controller.yaml`. If left
/// unset, K8s falls back to `default` (no IRSA → 401 Unauthorized on push).
const DEFAULT_BUILD_SERVICE_ACCOUNT: &str = "djinn-controller";
/// Default concurrency cap — the maximum number of build Jobs the
/// controller keeps in flight against the single shared buildkitd at
/// once. Matches the Helm values default
/// (`imagePipeline.controller.maxConcurrentBuilds`). 4 is a deliberate
/// balance: low enough that a herd of rebuilds (e.g. a worker-ref bump
/// invalidating every project's image hash) can't starve buildkitd's CPU
/// into a liveness-probe-kill crash loop, high enough to keep throughput
/// reasonable. Enforced cluster-wide by counting live build Jobs each
/// reconcile pass — see [`crate::ImageController::enqueue`].
const DEFAULT_MAX_CONCURRENT: usize = 4;

/// Environment-variable names consumed by [`ImageControllerConfig::from_env`].
pub mod env {
    pub const BUILDKITD_HOST: &str = "DJINN_IMAGE_BUILDKITD_HOST";
    pub const REGISTRY_HOST: &str = "DJINN_IMAGE_REGISTRY_HOST";
    pub const BUILDER_IMAGE: &str = "DJINN_IMAGE_BUILDER_IMAGE";
    pub const AGENT_WORKER_IMAGE: &str = "DJINN_IMAGE_AGENT_WORKER_IMAGE";
    /// The djinn release version (e.g. `0.6.57`). Folded into the catalog
    /// image hash so a version bump forces every project image to rebuild —
    /// guaranteeing new agent prompts/tools propagate even if
    /// `DJINN_IMAGE_AGENT_WORKER_IMAGE` is an unversioned tag (`:latest`).
    pub const DJINN_VERSION: &str = "DJINN_VERSION";
    pub const MAX_CONCURRENT: &str = "DJINN_IMAGE_MAX_CONCURRENT";
    pub const NAMESPACE: &str = "DJINN_IMAGE_NAMESPACE";
    pub const REGISTRY_AUTH_SECRET: &str = "DJINN_IMAGE_REGISTRY_AUTH_SECRET";
    pub const MIRROR_PVC: &str = "DJINN_IMAGE_MIRROR_PVC";
    pub const BUILD_SERVICE_ACCOUNT: &str = "DJINN_IMAGE_BUILD_SERVICE_ACCOUNT";
    pub const ZOT_RETENTION_ENABLED: &str = "DJINN_ZOT_RETENTION_ENABLED";
    pub const ZOT_RETENTION_DRY_RUN: &str = "DJINN_ZOT_RETENTION_DRY_RUN";
    pub const ZOT_RETENTION_NEWEST_TAGS: &str = "DJINN_ZOT_RETENTION_NEWEST_TAGS";
    pub const ZOT_RETENTION_ENDPOINT: &str = "DJINN_ZOT_RETENTION_ENDPOINT";
    pub const ZOT_RETENTION_USERNAME: &str = "DJINN_ZOT_RETENTION_USERNAME";
    pub const ZOT_RETENTION_PASSWORD: &str = "DJINN_ZOT_RETENTION_PASSWORD";
}

fn parse_bool(value: &str, default: bool, variable: &str) -> bool {
    match value.parse::<bool>() {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%value, %error, "{variable} invalid; keeping default");
            default
        }
    }
}

/// Runtime configuration for [`crate::ImageController`].
///
/// Loaded once on `djinn-server` boot via [`ImageControllerConfig::from_env`]
/// and cloned into the controller. Fields mirror what
/// `image-controller-deployment.yaml` exposes as env, so Helm operators can
/// tune the controller without touching code — even though the controller
/// itself now runs inside the djinn-server Pod.
#[derive(Debug, Clone)]
pub struct ImageControllerConfig {
    /// `tcp://` endpoint of the in-cluster BuildKit daemon.
    pub buildkitd_host: String,
    /// `host:port` of the Zot registry (no scheme — buildx formats the URL).
    pub registry_host: String,
    /// Image the build Pod runs. Post-P5 it only needs `buildctl` + a
    /// POSIX shell; `moby/buildkit:latest` is the default.
    pub builder_image: String,
    /// Full image ref for the agent-worker helper image (the image the
    /// generated Dockerfile `COPY --from=...`s the worker binary from).
    /// Tilt publishes `djinn/agent-runtime:dev` to the local registry;
    /// prod ships a sha-tagged image to the shared registry.
    pub agent_worker_image: String,
    /// The djinn release version, folded into the catalog image hash so a
    /// version bump always forces a rebuild with the current agent worker
    /// (prompts + tool schemas). Empty/`dev` outside a tagged release.
    pub build_version: String,
    /// Namespace where build Jobs are created and the registry-auth Secret
    /// is mounted from.
    pub namespace: String,
    /// Name of the registry-auth Secret mounted into every build Job.
    pub registry_auth_secret: String,
    /// Name of the PVC (ReadWriteMany) holding per-project bare mirrors.
    pub mirror_pvc: String,
    /// Maximum number of concurrent build Jobs the controller will admit.
    /// Enforced cluster-wide: each reconcile pass counts the live
    /// (pending + running) build Jobs the controller owns and skips
    /// enqueueing once that count reaches this cap, so a mass-rebuild
    /// herd can't starve the single shared buildkitd. Deferred projects
    /// are not lost — they're re-evaluated on a later reconcile tick as
    /// slots free. A per-project in-flight guard additionally coalesces
    /// duplicate enqueues for the same project.
    pub max_concurrent: usize,
    /// ServiceAccount the build Pod runs under. Has to carry the cluster's
    /// IRSA / Workload-Identity annotation so the docker credential helper
    /// in the build Pod can authenticate to the managed registry. Defaults
    /// to `djinn-controller` so behavior matches the rest of the chart's
    /// controller-dispatched Jobs.
    pub build_service_account: String,
    pub zot_retention: ZotRetentionConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZotRetentionConfig {
    pub enabled: bool,
    pub dry_run: bool,
    pub newest_tags: usize,
    pub endpoint: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl ImageControllerConfig {
    /// Defaults suitable for unit tests (no env access).
    pub fn for_testing() -> Self {
        Self {
            buildkitd_host: DEFAULT_BUILDKITD_HOST.into(),
            registry_host: DEFAULT_REGISTRY_HOST.into(),
            builder_image: DEFAULT_BUILDER_IMAGE.into(),
            agent_worker_image: DEFAULT_AGENT_WORKER_IMAGE.into(),
            build_version: "dev".into(),
            namespace: DEFAULT_NAMESPACE.into(),
            registry_auth_secret: DEFAULT_REGISTRY_AUTH_SECRET.into(),
            mirror_pvc: DEFAULT_MIRROR_PVC.into(),
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            build_service_account: DEFAULT_BUILD_SERVICE_ACCOUNT.into(),
            zot_retention: ZotRetentionConfig {
                enabled: false,
                dry_run: true,
                newest_tags: 5,
                endpoint: format!("http://{DEFAULT_REGISTRY_HOST}"),
                username: None,
                password: None,
            },
        }
    }

    /// Load a [`ImageControllerConfig`] from env, falling back to
    /// [`Self::for_testing`] values for anything unset.
    ///
    /// A malformed `DJINN_IMAGE_MAX_CONCURRENT` is logged at `warn` and
    /// falls back to the default — the controller still boots.
    pub fn from_env() -> Self {
        let mut cfg = Self::for_testing();
        if let Ok(v) = std::env::var(env::BUILDKITD_HOST) {
            cfg.buildkitd_host = v;
        }
        if let Ok(v) = std::env::var(env::REGISTRY_HOST) {
            cfg.registry_host = v;
        }
        if let Ok(v) = std::env::var(env::BUILDER_IMAGE) {
            cfg.builder_image = v;
        }
        if let Ok(v) = std::env::var(env::AGENT_WORKER_IMAGE) {
            cfg.agent_worker_image = v;
        }
        if let Ok(v) = std::env::var(env::DJINN_VERSION)
            && !v.trim().is_empty()
        {
            cfg.build_version = v;
        }
        if let Ok(v) = std::env::var(env::NAMESPACE) {
            cfg.namespace = v;
        }
        if let Ok(v) = std::env::var(env::REGISTRY_AUTH_SECRET) {
            cfg.registry_auth_secret = v;
        }
        if let Ok(v) = std::env::var(env::MIRROR_PVC) {
            cfg.mirror_pvc = v;
        }
        if let Ok(v) = std::env::var(env::BUILD_SERVICE_ACCOUNT) {
            cfg.build_service_account = v;
        }
        if let Ok(v) = std::env::var(env::MAX_CONCURRENT) {
            match v.parse::<usize>().and_then(validate_positive) {
                Ok(n) => cfg.max_concurrent = n,
                Err(e) => tracing::warn!(
                    value = %v,
                    error = %e,
                    "DJINN_IMAGE_MAX_CONCURRENT invalid; keeping default"
                ),
            }
        }
        if let Ok(v) = std::env::var(env::ZOT_RETENTION_ENABLED) {
            cfg.zot_retention.enabled = parse_bool(&v, false, env::ZOT_RETENTION_ENABLED);
        }
        if let Ok(v) = std::env::var(env::ZOT_RETENTION_DRY_RUN) {
            cfg.zot_retention.dry_run = parse_bool(&v, true, env::ZOT_RETENTION_DRY_RUN);
        }
        if let Ok(v) = std::env::var(env::ZOT_RETENTION_NEWEST_TAGS) {
            match v.parse::<usize>().and_then(validate_positive) {
                Ok(n) => cfg.zot_retention.newest_tags = n,
                Err(e) => {
                    tracing::warn!(value = %v, error = %e, "Zot retention newest-tag count invalid; keeping default")
                }
            }
        }
        if let Ok(v) = std::env::var(env::ZOT_RETENTION_ENDPOINT)
            && !v.trim().is_empty()
        {
            cfg.zot_retention.endpoint = v;
        }
        cfg.zot_retention.username = std::env::var(env::ZOT_RETENTION_USERNAME)
            .ok()
            .filter(|v| !v.is_empty());
        cfg.zot_retention.password = std::env::var(env::ZOT_RETENTION_PASSWORD)
            .ok()
            .filter(|v| !v.is_empty());
        cfg
    }
}

fn validate_positive(n: usize) -> Result<usize, ParseIntError> {
    if n == 0 {
        // Reuse a ParseIntError shape by parsing a sentinel.  Simpler than
        // adding a dedicated error type; the message we log is the raw
        // env value anyway.
        "0".parse::<std::num::NonZeroUsize>()
            .map(|n| n.get())
            .map_err(|_| "0".parse::<i32>().unwrap_err())
    } else {
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_testing_defaults_are_populated() {
        let cfg = ImageControllerConfig::for_testing();
        assert!(cfg.buildkitd_host.starts_with("tcp://"));
        assert!(cfg.registry_host.contains(':'));
        assert_eq!(cfg.max_concurrent, DEFAULT_MAX_CONCURRENT);
    }

    #[test]
    fn from_env_honors_documented_vars() {
        // SAFETY: single-threaded unit test.
        unsafe {
            std::env::set_var(env::BUILDKITD_HOST, "tcp://bk.example:1234");
            std::env::set_var(env::REGISTRY_HOST, "reg.example:5000");
            std::env::set_var(env::MAX_CONCURRENT, "7");
        }
        let cfg = ImageControllerConfig::from_env();
        assert_eq!(cfg.buildkitd_host, "tcp://bk.example:1234");
        assert_eq!(cfg.registry_host, "reg.example:5000");
        assert_eq!(cfg.max_concurrent, 7);
        unsafe {
            std::env::remove_var(env::BUILDKITD_HOST);
            std::env::remove_var(env::REGISTRY_HOST);
            std::env::remove_var(env::MAX_CONCURRENT);
        }
    }

    #[test]
    fn from_env_invalid_max_concurrent_falls_back() {
        let saved = std::env::var(env::MAX_CONCURRENT).ok();
        unsafe {
            std::env::set_var(env::MAX_CONCURRENT, "not-a-number");
        }
        let cfg = ImageControllerConfig::from_env();
        assert_eq!(cfg.max_concurrent, DEFAULT_MAX_CONCURRENT);
        unsafe {
            match saved {
                Some(prev) => std::env::set_var(env::MAX_CONCURRENT, prev),
                None => std::env::remove_var(env::MAX_CONCURRENT),
            }
        }
    }
}
