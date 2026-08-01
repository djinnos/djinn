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
    /// The object exists but has reached a terminal condition: nothing will run
    /// under it again. See [`job_reached_terminal_condition`].
    ///
    /// This is the distinction between "the object is here" and "its holder is
    /// alive", and conflating them is what stranded build leases in production:
    /// a task-run Job stays listed for its whole `ttlSecondsAfterFinished`
    /// (3600s) after it completes, so a reader that treats mere presence as
    /// liveness sees a live owner for an hour after the owner died.
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
/// Condition Kubernetes sets, exactly once and permanently, on a Job whose
/// completions have all succeeded.
pub const JOB_CONDITION_COMPLETE: &str = "Complete";
/// Condition Kubernetes sets, exactly once and permanently, on a Job that
/// exhausted its backoff limit or active deadline.
pub const JOB_CONDITION_FAILED: &str = "Failed";

/// Return the terminal Job condition whose status is affirmatively `True`.
///
/// Kubernetes may retain `Complete=False` / `Failed=False` conditions while a
/// Job is still live.  Keeping the predicate here gives inventory and the
/// task-run retention reader one lifecycle definition.
pub fn terminal_job_condition(
    status: Option<&k8s_openapi::api::batch::v1::JobStatus>,
) -> Option<&'static str> {
    let conditions = status?.conditions.as_ref()?;
    if conditions.iter().any(|condition| {
        condition.type_ == JOB_CONDITION_FAILED && condition.status.eq_ignore_ascii_case("True")
    }) {
        Some(JOB_CONDITION_FAILED)
    } else if conditions.iter().any(|condition| {
        condition.type_ == JOB_CONDITION_COMPLETE && condition.status.eq_ignore_ascii_case("True")
    }) {
        Some(JOB_CONDITION_COMPLETE)
    } else {
        None
    }
}

/// Whether a Job has reached a TERMINAL condition — nothing will run under it
/// again, ever.
///
/// # Why the conditions and not the `succeeded`/`failed` counters
///
/// The counters are a per-Pod tally, and a tally is not a lifecycle. A nonzero
/// `succeeded` on a Job with `completions: N` means one Pod finished while the
/// rest are still running; a nonzero `failed` on a Job with a nonzero
/// `backoffLimit` means a Pod died and a replacement is about to be created.
/// Either read would call a Job terminal while its work is live — and the only
/// consumer of this flag retires the build lease that work is holding, so a
/// false positive releases capacity out from under a running compile.
///
/// The conditions carry no such ambiguity: the Job controller sets `Complete`
/// or `Failed` only when it has stopped managing Pods for this Job.
///
/// `SuccessCriteriaMet` and `FailureTarget` are deliberately NOT terminal.
/// They are the intermediate conditions the controller sets first, while it is
/// still tearing down Pods; production's own trace for a finished task-run Job
/// reads `Suspended → SuccessCriteriaMet → Complete`, so treating
/// `SuccessCriteriaMet` as terminal would fire during teardown rather than
/// after it.
#[must_use]
pub fn job_reached_terminal_condition(
    status: Option<&k8s_openapi::api::batch::v1::JobStatus>,
) -> bool {
    terminal_job_condition(status).is_some()
}

fn job_record(j: Job) -> Option<WorkloadRecord> {
    let (images, commands) = containers(j.spec.as_ref().and_then(|s| s.template.spec.as_ref()));
    let terminal = job_reached_terminal_condition(j.status.as_ref());
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

// =============================================================================
// Kueue Workload observation (cutover slice S5, task kh7g)
// =============================================================================
//
// Everything above answers "does this object still exist?" by polling. What
// follows answers a different question — "has Kueue admitted this build, and is
// it still admitted?" — and answers it by WATCHING, because the two failure
// modes are not the same shape.
//
// A Workload's admission state is edge-triggered and reversible: Kueue admits,
// then preempts for quota, then re-admits, and every one of those edges matters
// to the run sitting behind it. A poll on any cadence silently collapses an
// admit/evict/admit sequence into whatever the last sample happened to see. The
// watch preserves the edges; the reconciler on the other end is what makes them
// idempotent.
//
// # Why a hand-rolled resource type instead of the Kueue crate
//
// Nothing here depends on Kueue's Go types, its CRD schema version beyond the
// group/version below, or on Kueue being installed at all. The struct models
// only `metadata` plus the `status.conditions` this repository reads; every
// other field the API server sends is ignored by serde. That is deliberate:
// `kueue.armed` defaults false and no namespace carries
// `djinn.io/kueue-managed`, so the production cluster today has NO Kueue CRDs
// and NO Workload objects. A type that deserialised the full Kueue schema would
// have to track their API revisions for a payload this process never reads.
//
// # Behaviour on a cluster with no Kueue installed
//
// [`KubeWorkloadWatch::run_session`] returns `Err` — the API server answers
// `404 NotFound` for an unregistered resource — and the caller backs off and
// retries. It never panics, never blocks another subsystem, and never mutates
// anything, because no observation is ever produced. That is the CURRENT
// production shape, which is why the reconciler's spawn site refuses to start
// this watch at all unless Kueue is armed.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
/// Re-exported because `djinn-server` reads Workload identity but does not
/// depend on `k8s-openapi`: the owner link is half of how a Workload is resolved
/// to a task-run, so a consumer that cannot name this type cannot exercise that
/// half at all.
pub use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::runtime::watcher;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// API group Kueue serves its `Workload` resource under.
pub const KUEUE_GROUP: &str = "kueue.x-k8s.io";
/// API version pinned by `deploy/helm/djinn/templates/kueue-topology.yaml`.
pub const KUEUE_WORKLOAD_VERSION: &str = "v1beta1";
/// Kueue's per-Job accounting object.
pub const KUEUE_WORKLOAD_KIND: &str = "Workload";
/// Plural, as it appears in the resource path.
pub const KUEUE_WORKLOAD_PLURAL: &str = "workloads";

/// Condition type Kueue sets once a Workload holds admitted quota.
pub const CONDITION_ADMITTED: &str = "Admitted";
/// Condition type Kueue sets when it takes admitted quota BACK — preemption,
/// a stopped ClusterQueue, a failed admission check, deactivation.
pub const CONDITION_EVICTED: &str = "Evicted";
/// Condition type Kueue sets when the underlying Job reached a terminal state.
pub const CONDITION_FINISHED: &str = "Finished";

/// The minimal projection of a Kueue `Workload` this process reads.
///
/// Unknown fields (`spec`, `status.admission`, `status.requeueState`, …) are
/// dropped by serde rather than modelled: see the module section above.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct KueueWorkload {
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub status: KueueWorkloadStatus,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct KueueWorkloadStatus {
    #[serde(default)]
    pub conditions: Vec<KueueWorkloadCondition>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct KueueWorkloadCondition {
    #[serde(rename = "type")]
    pub condition_type: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl kube::Resource for KueueWorkload {
    type DynamicType = ();
    type Scope = kube::core::NamespaceResourceScope;

    fn kind(_: &()) -> Cow<'_, str> {
        Cow::Borrowed(KUEUE_WORKLOAD_KIND)
    }
    fn group(_: &()) -> Cow<'_, str> {
        Cow::Borrowed(KUEUE_GROUP)
    }
    fn version(_: &()) -> Cow<'_, str> {
        Cow::Borrowed(KUEUE_WORKLOAD_VERSION)
    }
    fn plural(_: &()) -> Cow<'_, str> {
        Cow::Borrowed(KUEUE_WORKLOAD_PLURAL)
    }
    fn meta(&self) -> &ObjectMeta {
        &self.metadata
    }
    fn meta_mut(&mut self) -> &mut ObjectMeta {
        &mut self.metadata
    }
}

/// Where one Workload sits in Kueue's admission lifecycle, reduced to the three
/// answers the coordinator can act on.
///
/// `Pending` and `Admitted` are REVERSIBLE and the reconciler must be able to
/// move between them in both directions: a preemption is a live Workload
/// returning to the queue, not a terminal event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkloadAdmission {
    /// Kueue has not (or no longer) granted this Workload quota. `reason`
    /// carries Kueue's own word for it when there is one — `Preempted`,
    /// `ClusterQueueStopped`, `Deactivated` — and `None` when the Workload has
    /// simply not been looked at yet.
    Pending { reason: Option<String> },
    /// Kueue admitted the Workload; the Job is unsuspended and its Pod may run.
    Admitted,
    /// The Job behind this Workload reached a terminal state. Terminal, and
    /// deliberately NOT folded into `Pending`: a finished build must never be
    /// mistaken for one waiting on quota.
    Finished { reason: Option<String> },
}

impl WorkloadAdmission {
    /// Rendered form, used as the durable projection's state column.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending { .. } => "pending",
            Self::Admitted => "admitted",
            Self::Finished { .. } => "finished",
        }
    }

    /// Kueue's own explanation, when it gave one.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Pending { reason } | Self::Finished { reason } => reason.as_deref(),
            Self::Admitted => None,
        }
    }

    /// True while the build behind this Workload is waiting on quota.
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending { .. })
    }
}

fn condition<'a>(w: &'a KueueWorkload, kind: &str) -> Option<&'a KueueWorkloadCondition> {
    w.status
        .conditions
        .iter()
        .find(|c| c.condition_type == kind && c.status.eq_ignore_ascii_case("True"))
}

/// Reduce a Workload's conditions to one admission answer.
///
/// # Precedence, and why `Evicted` outranks `Admitted`
///
/// Kueue does not clear `Admitted=True` at the instant it evicts: an evicted
/// Workload is observable carrying BOTH conditions true while the Job is being
/// re-suspended. Reading `Admitted` first would therefore report a preempted
/// build as still running, which is precisely the direction that must never be
/// wrong — an over-reported admission strands a run nothing will ever finish,
/// while an over-reported pending merely re-observes on the next event.
pub fn classify_workload_admission(w: &KueueWorkload) -> WorkloadAdmission {
    if let Some(finished) = condition(w, CONDITION_FINISHED) {
        return WorkloadAdmission::Finished {
            reason: finished.reason.clone(),
        };
    }
    if let Some(evicted) = condition(w, CONDITION_EVICTED) {
        return WorkloadAdmission::Pending {
            reason: evicted.reason.clone(),
        };
    }
    if condition(w, CONDITION_ADMITTED).is_some() {
        return WorkloadAdmission::Admitted;
    }
    WorkloadAdmission::Pending {
        reason: w
            .status
            .conditions
            .iter()
            .find(|c| c.condition_type == CONDITION_ADMITTED)
            .and_then(|c| c.reason.clone()),
    }
}

/// Resolve the task-run this Workload accounts for, or `None` when it accounts
/// for something else (a warm Job, a SCIP Job, another tenant's workload).
///
/// Two sources, in order, because neither alone is sufficient:
///
/// 1. `djinn.app/task-run-id` on the Workload itself. Kueue propagates the
///    parent Job's labels onto the Workload it derives, so this is present for
///    a task-run Job rendered by `job_labels()`.
/// 2. the `djinn-taskrun-<uuid>` name of the owning Job. Kueue owner-links every
///    Workload to the object it accounts for, and that link survives label
///    propagation changing between Kueue releases.
///
/// A Workload that answers neither is NOT ours and the caller must leave every
/// task-run untouched.
pub fn workload_task_run_id(w: &KueueWorkload) -> Option<String> {
    if let Some(raw) = w
        .metadata
        .labels
        .as_ref()
        .and_then(|l| l.get(crate::job::LABEL_TASK_RUN_ID))
        && uuid::Uuid::parse_str(raw).is_ok()
    {
        return Some(raw.clone());
    }
    w.metadata
        .owner_references
        .as_ref()?
        .iter()
        .filter(|o| o.kind == "Job")
        .find_map(|o| crate::job::task_run_id_from_job_name(&o.name))
}

/// One observation handed to the reconciler.
#[derive(Clone, Debug, PartialEq)]
pub enum WorkloadObservation {
    /// A fresh list/watch session has begun; everything that follows until the
    /// next `SessionRestarted` is a replay of current cluster state.
    ///
    /// The reconciler needs this edge to distinguish "Kueue changed something"
    /// from "we reconnected and are being told the same thing again".
    SessionRestarted,
    /// The Workload exists with this state.
    Applied(KueueWorkload),
    /// The Workload is gone. Kueue garbage-collects a Workload with its owning
    /// Job, so this is a lifecycle end, not an eviction.
    Deleted(KueueWorkload),
}

/// The watch seam. Production watches a real API server; tests drive a scripted
/// stream through the same reconciler.
#[async_trait]
pub trait WorkloadWatch: Send + Sync + 'static {
    /// Run ONE watch session, pumping observations into `tx` until the session
    /// ends.
    ///
    /// `Ok(())` means the stream ended cleanly, `Err` that it broke. The caller
    /// re-opens either way — the distinction exists so a broken session can be
    /// logged and backed off differently from an orderly close, not so one of
    /// them can be treated as terminal.
    async fn run_session(
        &self,
        tx: tokio::sync::mpsc::Sender<WorkloadObservation>,
    ) -> Result<(), String>;
}

/// The production watch: `kube`'s reflector-grade `watcher` over namespaced
/// Kueue Workloads, filtered to djinn's own build objects.
pub struct KubeWorkloadWatch {
    client: crate::KubeClient,
    namespace: String,
}

impl KubeWorkloadWatch {
    pub fn new(client: crate::KubeClient, namespace: impl Into<String>) -> Self {
        Self {
            client,
            namespace: namespace.into(),
        }
    }
}

#[async_trait]
impl WorkloadWatch for KubeWorkloadWatch {
    async fn run_session(
        &self,
        tx: tokio::sync::mpsc::Sender<WorkloadObservation>,
    ) -> Result<(), String> {
        use futures::StreamExt;

        let api: kube::Api<KueueWorkload> =
            kube::Api::namespaced(self.client.clone(), &self.namespace);
        // Kueue derives one Workload per captured Job and copies the Job's
        // labels onto it, so the same marker that selects djinn's build objects
        // selects their Workloads. Without it this watch would also stream every
        // other tenant's Workloads in the namespace.
        let config = watcher::Config::default()
            .labels(&format!("{}=true", crate::config::LABEL_KUEUE_BUILD_OBJECT));
        let mut stream = watcher::watcher(api, config).boxed();

        while let Some(event) = stream.next().await {
            let observation = match event {
                Ok(watcher::Event::Init) => WorkloadObservation::SessionRestarted,
                Ok(watcher::Event::Apply(w) | watcher::Event::InitApply(w)) => {
                    WorkloadObservation::Applied(w)
                }
                Ok(watcher::Event::Delete(w)) => WorkloadObservation::Deleted(w),
                Ok(watcher::Event::InitDone) => continue,
                Err(e) => return Err(e.to_string()),
            };
            if tx.send(observation).await.is_err() {
                // The reconciler is gone (leadership released). Ending the
                // session here is what lets this task be dropped instead of
                // holding a live watch against the API server forever.
                return Ok(());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod job_terminal_condition_tests {
    use super::*;
    use k8s_openapi::api::batch::v1::{JobCondition, JobStatus};

    fn condition(kind: &str, status: &str) -> JobCondition {
        JobCondition {
            type_: kind.to_owned(),
            status: status.to_owned(),
            ..Default::default()
        }
    }

    fn status(conditions: Vec<JobCondition>) -> JobStatus {
        JobStatus {
            conditions: Some(conditions),
            ..Default::default()
        }
    }

    #[test]
    fn a_job_with_no_status_is_not_terminal() {
        assert!(!job_reached_terminal_condition(None));
    }

    #[test]
    fn a_running_job_is_not_terminal() {
        let running = JobStatus {
            active: Some(1),
            ..Default::default()
        };
        assert!(!job_reached_terminal_condition(Some(&running)));
    }

    #[test]
    fn a_complete_job_is_terminal() {
        assert!(job_reached_terminal_condition(Some(&status(vec![
            condition("Complete", "True")
        ]))));
    }

    #[test]
    fn a_failed_job_is_terminal() {
        assert!(job_reached_terminal_condition(Some(&status(vec![
            condition("Failed", "True")
        ]))));
    }

    /// A condition that is present but FALSE is not that condition. A reader
    /// that checked only for the type would call every suspended Job complete.
    #[test]
    fn a_complete_condition_that_is_false_is_not_terminal() {
        assert!(!job_reached_terminal_condition(Some(&status(vec![
            condition("Complete", "False")
        ]))));
    }

    /// The exact production trace: `Suspended → SuccessCriteriaMet → Complete`.
    ///
    /// `SuccessCriteriaMet` is the INTERMEDIATE condition the Job controller
    /// sets while it is still tearing Pods down. Treating it as terminal would
    /// retire the build lease of a Job whose Pod is still being killed, which
    /// is releasing capacity out from under running work.
    #[test]
    fn success_criteria_met_alone_is_not_terminal() {
        assert!(!job_reached_terminal_condition(Some(&status(vec![
            condition("Suspended", "False"),
            condition("SuccessCriteriaMet", "True"),
        ]))));
        assert!(
            job_reached_terminal_condition(Some(&status(vec![
                condition("Suspended", "False"),
                condition("SuccessCriteriaMet", "True"),
                condition("Complete", "True"),
            ]))),
            "the same Job one condition later IS terminal"
        );
    }

    /// `FailureTarget` is `SuccessCriteriaMet`'s mirror on the failure path and
    /// is equally intermediate.
    #[test]
    fn failure_target_alone_is_not_terminal() {
        assert!(!job_reached_terminal_condition(Some(&status(vec![
            condition("FailureTarget", "True")
        ]))));
    }

    /// The mutation guard for the derivation itself.
    ///
    /// This is precisely the shape the previous counter-based reading got
    /// wrong: a Job with `completions: 3` reports `succeeded: 1` while two Pods
    /// are still running, and one with a nonzero `backoffLimit` reports
    /// `failed: 1` while a replacement Pod is being created. Reverting
    /// `job_reached_terminal_condition` to `succeeded > 0 || failed > 0` makes
    /// both of these read as terminal and fails here.
    #[test]
    fn the_pod_counters_alone_never_make_a_job_terminal() {
        let partial_success = JobStatus {
            succeeded: Some(1),
            active: Some(2),
            ..Default::default()
        };
        assert!(
            !job_reached_terminal_condition(Some(&partial_success)),
            "one succeeded Pod out of several is not a finished Job"
        );

        let retrying = JobStatus {
            failed: Some(1),
            active: Some(1),
            ..Default::default()
        };
        assert!(
            !job_reached_terminal_condition(Some(&retrying)),
            "a failed Pod inside the backoff limit is not a finished Job"
        );
    }

    /// The whole point of the flag, stated as the production fact: a Job that
    /// has completed is still LISTED for its `ttlSecondsAfterFinished`, and the
    /// record must say so.
    #[test]
    fn a_completed_job_record_is_listed_and_flagged_terminal() {
        let job = Job {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                name: Some("djinn-taskrun-abc".to_owned()),
                uid: Some("uid-1".to_owned()),
                ..Default::default()
            },
            spec: None,
            status: Some(status(vec![condition("Complete", "True")])),
        };
        let record = job_record(job).expect("a named Job yields a record");
        assert_eq!(record.name, "djinn-taskrun-abc");
        assert!(
            record.terminal,
            "a Complete Job is still listed; the record is what carries the fact \
             that its holder is gone"
        );
    }
}

#[cfg(test)]
mod kueue_workload_tests {
    use super::*;

    fn condition(kind: &str, status: &str, reason: Option<&str>) -> KueueWorkloadCondition {
        KueueWorkloadCondition {
            condition_type: kind.to_owned(),
            status: status.to_owned(),
            reason: reason.map(ToOwned::to_owned),
            message: None,
        }
    }

    fn workload(conditions: Vec<KueueWorkloadCondition>) -> KueueWorkload {
        KueueWorkload {
            metadata: ObjectMeta::default(),
            status: KueueWorkloadStatus { conditions },
        }
    }

    #[test]
    fn a_workload_with_no_conditions_is_pending() {
        assert_eq!(
            classify_workload_admission(&workload(vec![])),
            WorkloadAdmission::Pending { reason: None }
        );
    }

    #[test]
    fn an_admitted_condition_reads_as_admitted() {
        assert_eq!(
            classify_workload_admission(&workload(vec![condition(
                CONDITION_ADMITTED,
                "True",
                None
            )])),
            WorkloadAdmission::Admitted
        );
    }

    /// `Admitted=False` is not admission. A classifier that looked only at the
    /// condition's PRESENCE would report every queued Workload as admitted.
    #[test]
    fn an_admitted_condition_that_is_false_is_still_pending() {
        assert_eq!(
            classify_workload_admission(&workload(vec![condition(
                CONDITION_ADMITTED,
                "False",
                Some("NoReservation")
            )])),
            WorkloadAdmission::Pending {
                reason: Some("NoReservation".to_owned())
            }
        );
    }

    /// The precedence that actually matters: Kueue leaves `Admitted=True` in
    /// place while it evicts, so an evicted Workload is observable carrying
    /// both. Reading `Admitted` first would report a preempted build as running.
    #[test]
    fn eviction_outranks_a_stale_admitted_condition() {
        assert_eq!(
            classify_workload_admission(&workload(vec![
                condition(CONDITION_ADMITTED, "True", None),
                condition(CONDITION_EVICTED, "True", Some("Preempted")),
            ])),
            WorkloadAdmission::Pending {
                reason: Some("Preempted".to_owned())
            }
        );
    }

    /// A finished build must never be mistaken for one waiting on quota — that
    /// would leave the projection reporting queue pressure that does not exist.
    #[test]
    fn a_finished_workload_is_terminal_not_pending() {
        let admission = classify_workload_admission(&workload(vec![
            condition(CONDITION_ADMITTED, "True", None),
            condition(CONDITION_EVICTED, "True", Some("Preempted")),
            condition(CONDITION_FINISHED, "True", Some("JobFinished")),
        ]));
        assert_eq!(
            admission,
            WorkloadAdmission::Finished {
                reason: Some("JobFinished".to_owned())
            }
        );
        assert!(!admission.is_pending());
    }

    #[test]
    fn the_task_run_label_identifies_the_run() {
        let id = uuid::Uuid::now_v7().to_string();
        let mut w = workload(vec![]);
        w.metadata.labels = Some(BTreeMap::from([(
            crate::job::LABEL_TASK_RUN_ID.to_owned(),
            id.clone(),
        )]));
        assert_eq!(workload_task_run_id(&w), Some(id));
    }

    #[test]
    fn the_owning_job_name_identifies_the_run_when_the_label_is_absent() {
        let id = uuid::Uuid::now_v7().to_string();
        let mut w = workload(vec![]);
        w.metadata.owner_references = Some(vec![
            k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
                kind: "Job".to_owned(),
                name: format!("djinn-taskrun-{id}"),
                ..Default::default()
            },
        ]);
        assert_eq!(workload_task_run_id(&w), Some(id));
    }

    /// A warm Job's Workload carries the same build-object label and reaches
    /// the same watch. It is not a task-run and must resolve to nothing.
    #[test]
    fn a_workload_that_is_not_a_task_run_resolves_to_nothing() {
        let mut w = workload(vec![]);
        w.metadata.labels = Some(BTreeMap::from([(
            crate::config::LABEL_KUEUE_BUILD_OBJECT.to_owned(),
            "true".to_owned(),
        )]));
        w.metadata.owner_references = Some(vec![
            k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
                kind: "Job".to_owned(),
                name: "djinn-warm-someproject-7".to_owned(),
                ..Default::default()
            },
        ]);
        assert_eq!(workload_task_run_id(&w), None);
    }

    /// A malformed label must not be believed just because it is present: the
    /// fallback to the owning Job name is what makes the resolution honest.
    #[test]
    fn a_malformed_label_falls_through_to_the_owner_reference() {
        let id = uuid::Uuid::now_v7().to_string();
        let mut w = workload(vec![]);
        w.metadata.labels = Some(BTreeMap::from([(
            crate::job::LABEL_TASK_RUN_ID.to_owned(),
            "not-a-uuid".to_owned(),
        )]));
        w.metadata.owner_references = Some(vec![
            k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
                kind: "Job".to_owned(),
                name: format!("djinn-taskrun-{id}"),
                ..Default::default()
            },
        ]);
        assert_eq!(workload_task_run_id(&w), Some(id));
    }

    /// The resource coordinates are the whole contract with the API server. A
    /// typo here is a permanent `404` that reads as "Kueue is not installed".
    #[test]
    fn the_resource_coordinates_address_kueue_workloads() {
        use kube::Resource;
        assert_eq!(KueueWorkload::group(&()), "kueue.x-k8s.io");
        assert_eq!(KueueWorkload::version(&()), "v1beta1");
        assert_eq!(KueueWorkload::kind(&()), "Workload");
        assert_eq!(KueueWorkload::plural(&()), "workloads");
        assert_eq!(KueueWorkload::api_version(&()), "kueue.x-k8s.io/v1beta1");
    }

    /// Kueue sends far more than this type models. Unknown fields must be
    /// dropped rather than failing the whole watch session.
    #[test]
    fn unmodelled_fields_do_not_break_deserialisation() {
        let raw = serde_json::json!({
            "apiVersion": "kueue.x-k8s.io/v1beta1",
            "kind": "Workload",
            "metadata": {"name": "job-djinn-taskrun-x-abcde", "namespace": "djinn"},
            "spec": {"queueName": "djinn-task-run", "podSets": [{"count": 1, "name": "main"}]},
            "status": {
                "admission": {"clusterQueue": "djinn-build"},
                "conditions": [
                    {"type": "Admitted", "status": "True", "reason": "Admitted",
                     "message": "The workload is admitted", "lastTransitionTime": "2026-07-30T00:00:00Z"}
                ]
            }
        });
        let parsed: KueueWorkload =
            serde_json::from_value(raw).expect("must tolerate extra fields");
        assert_eq!(
            classify_workload_admission(&parsed),
            WorkloadAdmission::Admitted
        );
    }
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
