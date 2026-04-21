//! Kubernetes-backed `SessionRuntime` — PR 1 scaffold.
//!
//! The crate delivers `KubernetesRuntime`, a `SessionRuntime` impl that
//! dispatches per-task-run work as K8s `Job`s. PR 1 lands the module layout,
//! typed configuration, and empty trait-impl shell — real cluster wiring
//! arrives in PR 3.

pub mod config;
pub mod graph_warmer;
pub mod job;
pub mod runtime;
pub mod secret;
pub mod token_review;
pub mod warm_job;

pub use config::KubernetesConfig;
pub use graph_warmer::{
    K8sGraphWarmer, KubeClientDispatcher, KubeClientJobWatcher, NoopJobWatcher, WarmJobDispatcher,
    WarmJobWatcher,
};
pub use runtime::KubernetesRuntime;
pub use warm_job::build_warm_job;
