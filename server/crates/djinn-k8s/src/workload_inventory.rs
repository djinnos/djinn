//! Broad, data-only Kubernetes inventory used by coordinator admission recovery.
//!
//! ## Why every call here is on a client-side clock
//!
//! [`WorkloadInventory::list`] is the first thing a build-admission
//! reconciliation pass does, and until 2026-07-29 it had no client-side
//! deadline at all. `kube`'s default client will happily keep a request
//! outstanding indefinitely when the API server accepts the connection and then
//! stops answering, so ONE such call parked the whole reconciliation loop
//! forever while every liveness signal in the process still read healthy — the
//! board stopped dispatching for five hours and nothing anywhere said why.
//!
//! A hung probe is not more informative than a failed one: both mean "the API
//! server did not answer". Every method below therefore runs under a bounded
//! budget and maps expiry onto the answer the method already has for "could not
//! establish this" — `Err` for the LIST, `Uncertain` for the per-object probes.
//! No timeout can turn absence-of-answer into proof of absence.
use async_trait::async_trait;
use k8s_openapi::api::{batch::v1::Job, core::v1::Pod};
use kube::{Api, api::ListParams};
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

pub const LABEL_ADMISSION_DOMAIN: &str = "djinn.app/admission-domain";
pub const LABEL_ADMISSION_WORK_ID: &str = "djinn.app/admission-work-id";
pub const LABEL_ADMISSION_GENERATION: &str = "djinn.app/admission-generation";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkloadObjectKind {
    Job,
    Pod,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkloadRecord {
    pub kind: WorkloadObjectKind,
    pub name: String,
    pub uid: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub terminal: bool,
    pub images: Vec<String>,
    pub commands: Vec<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UidGetResult {
    Present,
    NotFound,
    Uncertain,
}

/// Authoritative answer to "does an object with this name exist right now?".
///
/// Distinct from [`UidGetResult`], which can only be asked about a row that
/// already recorded a UID. An admission row that never reached Live has no UID
/// at all, so absence of its object can only be established by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObjectPresence {
    /// The API server returned an object under this name. `uid` is its
    /// immutable identity when the metadata carried one.
    Present { uid: Option<String> },
    /// The API server answered authoritatively that no such object exists.
    Absent,
    /// The probe could not answer. Never treated as proof of anything.
    Uncertain,
}

/// Environment variable overriding the per-call Kubernetes budget, in seconds.
pub const CALL_TIMEOUT_ENV: &str = "DJINN_K8S_WORKLOAD_INVENTORY_TIMEOUT_SECS";
/// Default per-call budget. Comfortably above a healthy namespaced LIST and far
/// below the reconciliation cadence, so a slow API server costs one pass rather
/// than every subsequent one.
const DEFAULT_CALL_TIMEOUT_SECS: u64 = 30;
/// Floor, so a misconfigured tiny value cannot make every pass expire before a
/// healthy API server can answer.
const MIN_CALL_TIMEOUT_SECS: u64 = 5;
/// Ceiling, so a misconfigured huge value cannot silently restore the unbounded
/// wait this budget exists to remove.
const MAX_CALL_TIMEOUT_SECS: u64 = 300;

/// Parse and bound the configured per-call budget. Anything unparseable or out
/// of range falls back to the default rather than disabling the deadline.
fn parse_call_timeout(raw: Option<&str>) -> Duration {
    let secs = raw
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|secs| (MIN_CALL_TIMEOUT_SECS..=MAX_CALL_TIMEOUT_SECS).contains(secs))
        .unwrap_or(DEFAULT_CALL_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// The process-wide per-call budget, read from the environment exactly once.
fn call_timeout() -> Duration {
    static BUDGET: OnceLock<Duration> = OnceLock::new();
    *BUDGET.get_or_init(|| parse_call_timeout(std::env::var(CALL_TIMEOUT_ENV).ok().as_deref()))
}

/// Run one Kubernetes call under `budget`, yielding `None` if it expired.
///
/// Separate from [`call_timeout`] so tests can drive the deadline directly
/// instead of mutating process-global environment state.
pub(crate) async fn with_call_timeout<F>(
    budget: Duration,
    op: &'static str,
    fut: F,
) -> Option<F::Output>
where
    F: Future,
{
    match tokio::time::timeout(budget, fut).await {
        Ok(value) => Some(value),
        Err(_) => {
            tracing::error!(
                op,
                timeout_secs = budget.as_secs(),
                "workload inventory Kubernetes call exceeded its client-side budget; \
                 treating it as unanswered rather than waiting forever"
            );
            None
        }
    }
}

#[async_trait]
pub trait WorkloadInventory: Send + Sync {
    async fn list(&self) -> Result<Vec<WorkloadRecord>, String>;
    async fn get_uid(&self, kind: WorkloadObjectKind, name: &str, uid: &str) -> UidGetResult;

    /// Probe one named object independently of any recorded UID.
    async fn presence(&self, kind: WorkloadObjectKind, name: &str) -> ObjectPresence;
}

pub struct KubeWorkloadInventory {
    client: crate::KubeClient,
    namespace: String,
}
impl KubeWorkloadInventory {
    pub fn new(client: crate::KubeClient, namespace: impl Into<String>) -> Self {
        Self {
            client,
            namespace: namespace.into(),
        }
    }
}

fn containers(spec: Option<&k8s_openapi::api::core::v1::PodSpec>) -> (Vec<String>, Vec<String>) {
    let mut images = Vec::new();
    let mut commands = Vec::new();
    if let Some(spec) = spec {
        for c in &spec.containers {
            if let Some(i) = &c.image {
                images.push(i.clone());
            }
            commands.extend(c.command.clone().unwrap_or_default());
            commands.extend(c.args.clone().unwrap_or_default());
        }
    }
    (images, commands)
}
fn job_record(j: Job) -> Option<WorkloadRecord> {
    let (images, commands) = containers(j.spec.as_ref().and_then(|s| s.template.spec.as_ref()));
    let terminal = j
        .status
        .as_ref()
        .is_some_and(|s| s.succeeded.unwrap_or(0) > 0 || s.failed.unwrap_or(0) > 0);
    Some(WorkloadRecord {
        kind: WorkloadObjectKind::Job,
        name: j.metadata.name?,
        uid: j.metadata.uid,
        labels: j.metadata.labels.unwrap_or_default(),
        terminal,
        images,
        commands,
    })
}
#[async_trait]
impl WorkloadInventory for KubeWorkloadInventory {
    async fn list(&self) -> Result<Vec<WorkloadRecord>, String> {
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        // Child Pods inherit the Job's admission labels. Treating both as
        // independent workloads duplicates task identities and double-counts
        // warm jobs, while the Job is the UID-fenced lifecycle object.
        let jobs = with_call_timeout(call_timeout(), "workload_inventory.list", async {
            jobs.list(&ListParams::default()).await
        })
        .await
        .ok_or_else(|| {
            format!(
                "workload inventory LIST exceeded its {}s client-side budget",
                call_timeout().as_secs()
            )
        })?
        .map_err(|e| e.to_string())?;
        Ok(jobs.items.into_iter().filter_map(job_record).collect())
    }
    async fn get_uid(&self, kind: WorkloadObjectKind, name: &str, uid: &str) -> UidGetResult {
        let probe = async {
            match kind {
                WorkloadObjectKind::Job => {
                    Api::<Job>::namespaced(self.client.clone(), &self.namespace)
                        .get_opt(name)
                        .await
                        .map(|v| v.and_then(|o| o.metadata.uid))
                }
                WorkloadObjectKind::Pod => {
                    Api::<Pod>::namespaced(self.client.clone(), &self.namespace)
                        .get_opt(name)
                        .await
                        .map(|v| v.and_then(|o| o.metadata.uid))
                }
            }
        };
        // An expired probe is indistinguishable from an unanswered one, and
        // `Uncertain` is exactly the answer that proves nothing.
        let Some(result) =
            with_call_timeout(call_timeout(), "workload_inventory.get_uid", probe).await
        else {
            return UidGetResult::Uncertain;
        };
        match result {
            Ok(Some(found)) if found == uid => UidGetResult::Present,
            Ok(None) => UidGetResult::NotFound,
            _ => UidGetResult::Uncertain,
        }
    }

    async fn presence(&self, kind: WorkloadObjectKind, name: &str) -> ObjectPresence {
        let probe = async {
            match kind {
                WorkloadObjectKind::Job => {
                    Api::<Job>::namespaced(self.client.clone(), &self.namespace)
                        .get_opt(name)
                        .await
                        .map(|v| v.map(|o| o.metadata.uid))
                }
                WorkloadObjectKind::Pod => {
                    Api::<Pod>::namespaced(self.client.clone(), &self.namespace)
                        .get_opt(name)
                        .await
                        .map(|v| v.map(|o| o.metadata.uid))
                }
            }
        };
        let Some(result) =
            with_call_timeout(call_timeout(), "workload_inventory.presence", probe).await
        else {
            return ObjectPresence::Uncertain;
        };
        match result {
            Ok(Some(uid)) => ObjectPresence::Present { uid },
            Ok(None) => ObjectPresence::Absent,
            Err(_) => ObjectPresence::Uncertain,
        }
    }
}

pub fn has_canonical_warm_signature(r: &WorkloadRecord) -> bool {
    r.name.starts_with("djinn-warm-")
        && !r.images.is_empty()
        && r.commands
            .iter()
            .any(|c| c.contains(crate::warm_job::WARM_COMMAND_BIN) && c.contains("warm-graph"))
}

#[cfg(test)]
mod call_timeout_tests {
    use super::*;

    /// The property that matters is not "a constant exists" but "a call that
    /// never answers is cut off". Drive a future that is pending forever and
    /// assert the helper returns rather than parking the caller.
    #[tokio::test(start_paused = true)]
    async fn a_call_that_never_answers_is_cut_off() {
        let outcome = with_call_timeout(
            Duration::from_secs(30),
            "test.hang",
            std::future::pending::<u8>(),
        )
        .await;
        assert!(
            outcome.is_none(),
            "an unanswered Kubernetes call must expire, not park the reconciliation loop forever"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_call_that_answers_within_budget_is_passed_through() {
        let outcome =
            with_call_timeout(Duration::from_secs(30), "test.ok", std::future::ready(7u8)).await;
        assert_eq!(outcome, Some(7), "the budget must not change a live answer");
    }

    #[test]
    fn unset_budget_uses_the_default() {
        assert_eq!(
            parse_call_timeout(None),
            Duration::from_secs(DEFAULT_CALL_TIMEOUT_SECS)
        );
    }

    #[test]
    fn a_valid_budget_is_adopted() {
        assert_eq!(parse_call_timeout(Some("60")), Duration::from_secs(60));
        assert_eq!(parse_call_timeout(Some(" 15 ")), Duration::from_secs(15));
    }

    #[test]
    fn out_of_range_and_unparseable_budgets_fall_back_to_the_default() {
        let default = Duration::from_secs(DEFAULT_CALL_TIMEOUT_SECS);
        for raw in ["0", "1", "301", "99999", "forever", "", "-5"] {
            assert_eq!(
                parse_call_timeout(Some(raw)),
                default,
                "{raw:?} must fall back to the default rather than remove the deadline"
            );
        }
    }
}
