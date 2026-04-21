//! [`ImageControllerConfig`] — env-driven runtime knobs for the controller.

use std::num::ParseIntError;

/// Default buildkitd DNS (matches `buildkitd-service.yaml` shipped in PR 4).
const DEFAULT_BUILDKITD_HOST: &str = "tcp://djinn-buildkitd.djinn.svc.cluster.local:1234";
/// Default Zot registry DNS (matches `zot-service.yaml` shipped in PR 4).
const DEFAULT_REGISTRY_HOST: &str = "djinn-zot.djinn.svc.cluster.local:5000";
/// Default djinn-image-builder image (Dockerfile shipped in PR 4).
const DEFAULT_BUILDER_IMAGE: &str = "ghcr.io/djinnos/djinn-image-builder:latest";
/// Default namespace for build Jobs + registry-auth Secret lookup.
const DEFAULT_NAMESPACE: &str = "djinn";
/// Default registry-auth Secret name referenced by the build-Job Pod spec.
const DEFAULT_REGISTRY_AUTH_SECRET: &str = "djinn-zot-auth";
/// Default PVC claim name holding the per-project bare mirrors.
const DEFAULT_MIRROR_PVC: &str = "djinn-mirrors";
/// Default concurrency cap — matches the Helm values default.
const DEFAULT_MAX_CONCURRENT: usize = 3;

/// Environment-variable names consumed by [`ImageControllerConfig::from_env`].
pub mod env {
    pub const BUILDKITD_HOST: &str = "DJINN_IMAGE_BUILDKITD_HOST";
    pub const REGISTRY_HOST: &str = "DJINN_IMAGE_REGISTRY_HOST";
    pub const BUILDER_IMAGE: &str = "DJINN_IMAGE_BUILDER_IMAGE";
    pub const MAX_CONCURRENT: &str = "DJINN_IMAGE_MAX_CONCURRENT";
    pub const NAMESPACE: &str = "DJINN_IMAGE_NAMESPACE";
    pub const REGISTRY_AUTH_SECRET: &str = "DJINN_IMAGE_REGISTRY_AUTH_SECRET";
    pub const MIRROR_PVC: &str = "DJINN_IMAGE_MIRROR_PVC";
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
    /// Image reference for the djinn-image-builder Pod (Node + docker
    /// buildx + `@devcontainers/cli`).
    pub builder_image: String,
    /// Namespace where build Jobs are created and the registry-auth Secret
    /// is mounted from.
    pub namespace: String,
    /// Name of the registry-auth Secret mounted into every build Job.
    pub registry_auth_secret: String,
    /// Name of the PVC (ReadWriteMany) holding per-project bare mirrors.
    pub mirror_pvc: String,
    /// Maximum number of concurrent build Jobs the controller will admit.
    /// Enforced via a tokio [`tokio::sync::Semaphore`] — in-flight guards
    /// prevent duplicate enqueues for the same project.
    pub max_concurrent: usize,
}

impl ImageControllerConfig {
    /// Defaults suitable for unit tests (no env access).
    pub fn for_testing() -> Self {
        Self {
            buildkitd_host: DEFAULT_BUILDKITD_HOST.into(),
            registry_host: DEFAULT_REGISTRY_HOST.into(),
            builder_image: DEFAULT_BUILDER_IMAGE.into(),
            namespace: DEFAULT_NAMESPACE.into(),
            registry_auth_secret: DEFAULT_REGISTRY_AUTH_SECRET.into(),
            mirror_pvc: DEFAULT_MIRROR_PVC.into(),
            max_concurrent: DEFAULT_MAX_CONCURRENT,
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
        if let Ok(v) = std::env::var(env::NAMESPACE) {
            cfg.namespace = v;
        }
        if let Ok(v) = std::env::var(env::REGISTRY_AUTH_SECRET) {
            cfg.registry_auth_secret = v;
        }
        if let Ok(v) = std::env::var(env::MIRROR_PVC) {
            cfg.mirror_pvc = v;
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
