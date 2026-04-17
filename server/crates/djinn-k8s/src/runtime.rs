//! `KubernetesRuntime` — dispatches per-task-run work as K8s `Job`s.
//!
//! PR 1 ships only the module shell: a `SessionRuntime` impl whose methods
//! all `unimplemented!()`, plus an object-safety assertion that the impl
//! satisfies `dyn SessionRuntime`. PR 3 fills in `prepare`, `attach_stdio`,
//! `cancel`, and `teardown` against a real kube-rs client.

use async_trait::async_trait;
use djinn_runtime::{
    BiStream, RunHandle, RuntimeError, SessionRuntime, TaskRunReport, TaskRunSpec,
};

use crate::config::KubernetesConfig;

/// Kubernetes-backed `SessionRuntime`.
///
/// Owns the cluster-side configuration plus (eventually, in PR 3) a `kube::Client`.
pub struct KubernetesRuntime {
    #[allow(dead_code)]
    config: KubernetesConfig,
}

impl KubernetesRuntime {
    /// Construct a new runtime from a validated configuration.
    pub fn new(config: KubernetesConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl SessionRuntime for KubernetesRuntime {
    async fn prepare(&self, _spec: &TaskRunSpec) -> Result<RunHandle, RuntimeError> {
        unimplemented!("KubernetesRuntime::prepare lands in PR 3")
    }

    async fn attach_stdio(&self, _handle: &RunHandle) -> Result<BiStream, RuntimeError> {
        unimplemented!("KubernetesRuntime::attach_stdio lands in PR 3")
    }

    async fn cancel(&self, _handle: &RunHandle) -> Result<(), RuntimeError> {
        unimplemented!("KubernetesRuntime::cancel lands in PR 3")
    }

    async fn teardown(&self, _handle: RunHandle) -> Result<TaskRunReport, RuntimeError> {
        unimplemented!("KubernetesRuntime::teardown lands in PR 3")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kubernetes_runtime_is_object_safe() {
        fn assert_object_safe<T: ?Sized>() {}
        assert_object_safe::<dyn SessionRuntime>();
        // Use the type so the compiler keeps its bounds in scope.
        let _ = std::marker::PhantomData::<KubernetesRuntime>;
    }
}
