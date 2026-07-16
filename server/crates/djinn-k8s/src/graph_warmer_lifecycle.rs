//! Deterministic terminal-outcome seam for Kubernetes warm Jobs.
//!
//! Kept separate from dispatch scheduling so lifecycle tests can model terminal
//! observations without an apiserver.

use std::time::Duration;

use async_trait::async_trait;
use djinn_core::clock::{Clock, SystemClock};
use k8s_openapi::api::batch::v1::Job;
use kube::api::Api;
use tracing::{debug, warn};

/// Interval used by the Job-watcher loop to poll `.status.succeeded` /
/// `.status.failed`.
const WATCH_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Backstop cap on watcher polling if the apiserver keeps returning errors.
const WATCH_DEADLINE: Duration = Duration::from_secs(3600);

/// Terminal outcome reported by a [`WarmJobWatcher`]. Drives the in-process
/// convergence hook: on [`WarmTerminalOutcome::Succeeded`] the warm Job pod has
/// already rewritten `repo_graph_cache`, so the server invalidates its RAM slot
/// (see [`WarmCompletionSink`]); on [`WarmTerminalOutcome::Failed`] the row is
/// unchanged and no convergence is needed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarmTerminalOutcome {
    /// The warm Job reported `.status.succeeded > 0`, meaning a fresh blob was
    /// persisted.
    Succeeded,
    /// The warm Job reported `.status.failed > 0`, disappeared before success
    /// was observed, or the watcher gave up at its deadline. No fresh blob was
    /// persisted.
    Failed,
}

/// Optional Job-terminal watcher. Production uses
/// [`KubeClientJobWatcher`]; tests pass [`NoopJobWatcher`] to keep the
/// unit tests free of any apiserver dependency.
#[async_trait]
pub trait WarmJobWatcher: Send + Sync {
    /// Poll the Job `job_name` in `namespace` until it reaches a terminal
    /// state (succeeded OR failed) or the watcher's internal deadline
    /// elapses. Implementations MUST NOT block forever. The returned
    /// [`WarmTerminalOutcome`] tells the caller whether a fresh graph blob was
    /// persisted (→ trigger in-process cache convergence).
    async fn wait_terminal(&self, namespace: &str, job_name: &str) -> WarmTerminalOutcome;

    /// Return the UID observed for a Job after its create request. This keeps
    /// `WarmJobDispatcher`'s raw public result unchanged while allowing the
    /// admission lifecycle to fence its facts with Kubernetes object identity.
    async fn job_uid(&self, _namespace: &str, _job_name: &str) -> Option<String> {
        None
    }
}

/// Production watcher backed by `kube::Api::<Job>::get`. Polls on
/// [`WATCH_POLL_INTERVAL`].
pub struct KubeClientJobWatcher {
    client: kube::Client,
}

impl KubeClientJobWatcher {
    pub fn new(client: kube::Client) -> Self {
        Self { client }
    }
}

/// One result from a watcher poll, reduced to only the terminal facts needed
/// by the lifecycle decision. Keeping this decision independent of kube types
/// gives tests deterministic control over both observations and deadline
/// progression without requiring an apiserver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WarmJobObservation {
    Running,
    Succeeded,
    Failed,
    /// The Job was not found. This can be TTL cleanup, but can equally be an
    /// operator deletion or eviction, so it is never evidence of a completed
    /// warm unless success was observed before disappearance.
    Disappeared,
    /// A non-404 API error. Retry while the bounded watcher deadline remains.
    ApiError,
}

/// Return a terminal outcome when an observation proves one, or when the
/// bounded watcher deadline has elapsed. In particular, disappearance is
/// fail-closed: only an observed `.status.succeeded > 0` can prove persistence.
pub(super) fn terminal_outcome_after_poll(
    observation: WarmJobObservation,
    deadline_expired: bool,
) -> Option<WarmTerminalOutcome> {
    match observation {
        WarmJobObservation::Succeeded => Some(WarmTerminalOutcome::Succeeded),
        WarmJobObservation::Failed | WarmJobObservation::Disappeared => {
            Some(WarmTerminalOutcome::Failed)
        }
        WarmJobObservation::Running | WarmJobObservation::ApiError if deadline_expired => {
            Some(WarmTerminalOutcome::Failed)
        }
        WarmJobObservation::Running | WarmJobObservation::ApiError => None,
    }
}

#[async_trait]
impl WarmJobWatcher for KubeClientJobWatcher {
    async fn wait_terminal(&self, namespace: &str, job_name: &str) -> WarmTerminalOutcome {
        let api: Api<Job> = Api::namespaced(self.client.clone(), namespace);
        let deadline = SystemClock::new().now_instant() + WATCH_DEADLINE;
        loop {
            let observation = match api.get(job_name).await {
                Ok(job) => {
                    if let Some(status) = job.status.as_ref() {
                        if status.succeeded.unwrap_or(0) > 0 {
                            WarmJobObservation::Succeeded
                        } else if status.failed.unwrap_or(0) > 0 {
                            WarmJobObservation::Failed
                        } else {
                            WarmJobObservation::Running
                        }
                    } else {
                        WarmJobObservation::Running
                    }
                }
                Err(kube::Error::Api(resp)) if resp.code == 404 => WarmJobObservation::Disappeared,
                Err(e) => {
                    warn!(
                        job = %job_name,
                        error = %e,
                        "K8sGraphWarmer watcher: api get failed (continuing)"
                    );
                    WarmJobObservation::ApiError
                }
            };

            let deadline_expired = SystemClock::new().now_instant() >= deadline;
            if let Some(outcome) = terminal_outcome_after_poll(observation, deadline_expired) {
                match outcome {
                    WarmTerminalOutcome::Succeeded => {
                        debug!(job = %job_name, "K8sGraphWarmer watcher: succeeded");
                    }
                    WarmTerminalOutcome::Failed if observation == WarmJobObservation::Failed => {
                        warn!(job = %job_name, "K8sGraphWarmer watcher: failed");
                    }
                    WarmTerminalOutcome::Failed
                        if observation == WarmJobObservation::Disappeared =>
                    {
                        warn!(job = %job_name, "K8sGraphWarmer watcher: job disappeared without observed success");
                    }
                    WarmTerminalOutcome::Failed => {
                        warn!(job = %job_name, "K8sGraphWarmer watcher: deadline exceeded");
                    }
                }
                return outcome;
            }
            tokio::time::sleep(WATCH_POLL_INTERVAL).await;
        }
    }

    async fn job_uid(&self, namespace: &str, job_name: &str) -> Option<String> {
        let api: Api<Job> = Api::namespaced(self.client.clone(), namespace);
        match api.get(job_name).await {
            Ok(job) => job.metadata.uid,
            Err(error) => {
                warn!(job = %job_name, error = %error, "K8sGraphWarmer: could not observe Job UID");
                None
            }
        }
    }
}

/// No-op watcher used by unit tests. Reports success so the in-flight slot is
/// released and the completion hook (if any) fires, matching the common
/// "warm completed" path the tests exercise.
pub struct NoopJobWatcher;

#[async_trait]
impl WarmJobWatcher for NoopJobWatcher {
    async fn wait_terminal(&self, _namespace: &str, _job_name: &str) -> WarmTerminalOutcome {
        WarmTerminalOutcome::Succeeded
    }
}
