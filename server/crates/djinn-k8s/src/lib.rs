//! Kubernetes-backed `SessionRuntime` — PR 1 scaffold.
//!
//! The crate delivers `KubernetesRuntime`, a `SessionRuntime` impl that
//! dispatches per-task-run work as K8s `Job`s. PR 1 lands the module layout,
//! typed configuration, and empty trait-impl shell — real cluster wiring
//! arrives in PR 3.

pub mod config;
pub mod job;
pub mod runtime;
pub mod secret;
pub mod token_review;

pub use config::KubernetesConfig;
pub use runtime::KubernetesRuntime;
