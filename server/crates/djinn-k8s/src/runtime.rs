//! `KubernetesRuntime` — dispatches per-task-run work as K8s `Job`s.
//!
//! PR 1 ships only the module shell: a `SessionRuntime` impl whose methods
//! all return `RuntimeError` "not yet implemented" variants, plus an
//! object-safety assertion that the impl satisfies `dyn SessionRuntime`.
//! PR 3 fills in `prepare`, `attach_stdio`, `cancel`, and `teardown` against
//! a real kube-rs client.

use async_trait::async_trait;
use djinn_runtime::{
    BiStream, RunHandle, RuntimeError, SessionRuntime, TaskRunReport, TaskRunSpec,
};

use crate::config::KubernetesConfig;

/// Kubernetes-backed `SessionRuntime`.
///
/// Owns the cluster-side configuration plus a `kube::Client` acquired from
/// the ambient kubeconfig / in-cluster ServiceAccount.
pub struct KubernetesRuntime {
    #[allow(dead_code)]
    client: kube::Client,
    #[allow(dead_code)]
    config: KubernetesConfig,
}

impl KubernetesRuntime {
    /// Construct a new runtime by discovering a `kube::Client` from the
    /// ambient environment (in-cluster ServiceAccount when running in a Pod,
    /// `$KUBECONFIG` otherwise).
    ///
    /// Returns the underlying `kube::Error` on discovery failure rather than
    /// panicking — callers on a dev box without a cluster can surface the
    /// error and fall back to another runtime.
    pub async fn new(config: KubernetesConfig) -> Result<Self, kube::Error> {
        let client = kube::Client::try_default().await?;
        Ok(Self { client, config })
    }

    /// Construct a runtime from an already-built client — handy for tests and
    /// for call sites that share a client across multiple consumers.
    pub fn from_client(client: kube::Client, config: KubernetesConfig) -> Self {
        Self { client, config }
    }
}

#[async_trait]
impl SessionRuntime for KubernetesRuntime {
    async fn prepare(&self, _spec: &TaskRunSpec) -> Result<RunHandle, RuntimeError> {
        Err(RuntimeError::Prepare(
            "KubernetesRuntime::prepare not yet implemented — PR 3".to_string(),
        ))
    }

    async fn attach_stdio(&self, _handle: &RunHandle) -> Result<BiStream, RuntimeError> {
        Err(RuntimeError::Attach(
            "KubernetesRuntime::attach_stdio not yet implemented — PR 3".to_string(),
        ))
    }

    async fn cancel(&self, _handle: &RunHandle) -> Result<(), RuntimeError> {
        Err(RuntimeError::Cancel(
            "KubernetesRuntime::cancel not yet implemented — PR 3".to_string(),
        ))
    }

    async fn teardown(&self, _handle: RunHandle) -> Result<TaskRunReport, RuntimeError> {
        Err(RuntimeError::Teardown(
            "KubernetesRuntime::teardown not yet implemented — PR 3".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Object-safety: `dyn SessionRuntime` must accept a reference to
    /// `KubernetesRuntime`. This is a compile-only check.
    #[allow(dead_code)]
    fn _obj_safe(_: &dyn SessionRuntime) {}

    #[test]
    fn kubernetes_runtime_is_object_safe() {
        // Compile-only: `dyn SessionRuntime` is constructible from
        // `&KubernetesRuntime`. A full constructor call requires a live
        // `kube::Client`, so we gate that work into PR 3's integration tests.
        fn assert_object_safe<T: ?Sized>() {}
        assert_object_safe::<dyn SessionRuntime>();
    }
}
