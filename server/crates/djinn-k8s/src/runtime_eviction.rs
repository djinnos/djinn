//! Telling a RECOVERABLE Kueue eviction apart from a genuinely abandoned Job
//! (task `03z3`, epic `fbiy`).
//!
//! # The defect this exists to close
//!
//! `fbiy-A1` (#2833) gave [`crate::runtime::KubernetesRuntime::watch_infra_death`]
//! an arm that fires when the fenced worker Pod is absent while its Job is still
//! nonterminal: it terminalises the task-run and foreground-deletes the Job so
//! the Job stops holding Kueue quota. For an abandoned Pod that is right.
//!
//! A routine Kueue eviction produces *the same observable state* — Kueue
//! re-suspends the Job and the Job controller deletes its Pod — and an eviction
//! is RECOVERABLE: when capacity returns, the very same Workload is re-admitted
//! and a new Pod runs. Deleting the Job there converts a run that was going to
//! finish into one that never can.
//!
//! `fbiy-B2` (#2842) then measured, on a live armed cluster, that the renderer's
//! `backoffLimit: 0` makes a force-deleted Pod an immediate Job *failure* (~5s),
//! which resolves through the pre-existing `job_failed_reason` arm. So on a real
//! cluster A1's arm is reached almost exclusively BY EVICTION — the one case
//! where its action is wrong.
//!
//! The fix narrows the arm rather than removing it: an abandoned Job still has
//! to be reaped, because a stale Job holding quota forever is why A1 exists.
//!
//! # The distinguisher, and why it is these two fields
//!
//! Measured on the disposable armed cluster (kind, Kubernetes 1.31, Kueue
//! 0.19.0, 2026-07-31; full transcript in the PR body):
//!
//! | phase | `job.spec.suspend` | Workload `Evicted` condition |
//! |---|---|---|
//! | admitted, running | `false` | *absent* |
//! | evicted (`stopPolicy: HoldAndDrain`) | `true` at t+0s, Pod gone at t+34s | `True`, reason `ClusterQueueStopped` |
//! | re-admitted (~1s after release) | `false` | **still present**, `False`, reason `QuotaReserved`, message `Previously: The ClusterQueue is stopped` |
//!
//! Two fields, because one alone is not enough:
//!
//! * [`job_is_suspended`] — `spec.suspend` on the Job object the watch has
//!   already fetched. Kueue writes it BEFORE the Pod goes away (t+0s versus
//!   t+34s above), so there is no window in which the Pod is missing and the
//!   suspension is not yet visible. It costs no extra API call and needs no
//!   Kueue CRD, so it also answers for a plain `kubectl patch suspend=true`.
//! * [`workload_eviction_record`] — the Workload's `Evicted` condition,
//!   **whatever its status**. This is the one that survives re-admission: Kueue
//!   flips the condition to `False` and keeps it, so a watch that samples after
//!   the queue was released can still see that this Workload's quota was taken
//!   back. `spec.suspend` alone cannot see that — the re-admitted Job reads
//!   `suspend: false` with the fenced Pod still gone, which is bit-for-bit the
//!   state A1 reaps on, and the release measured ~1s against a 15s poll.
//!
//! Neither is a timing heuristic: both are field reads, and no branch here
//! consults a clock, a duration or a sleep. That is `03z3`'s AC3.
//!
//! # Failing toward "hold", except where Kueue does not exist
//!
//! [`classify_absent_pod`] answers `Abandoned` only on a definite negative: the
//! Job is not suspended AND Kueue's own record says this Workload was never
//! evicted, or the Workload API is not served at all (no Kueue in this cluster,
//! so no eviction can have happened and A1's behaviour is the whole truth).
//! Any other failure — RBAC, a 5xx, a timeout — is [`PodAbsenceVerdict::Inconclusive`]
//! and holds, because the next poll is 15 seconds away while a wrong reap is
//! permanent. An evicted Workload holds NO quota in the meantime (measured: the
//! ClusterQueue's `pods` usage falls to 0 on eviction), so holding does not
//! strand the capacity the reap exists to release.

use k8s_openapi::api::batch::v1::Job;
use kube::Api;
use kube::api::ListParams;

use crate::workload_inventory::{CONDITION_EVICTED, KueueWorkload};

/// Why the fenced Pod is missing, as far as observable cluster state can say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PodAbsenceVerdict {
    /// Nothing explains it: the Job is unsuspended, therefore owed a Pod, and
    /// Kueue has no record of ever taking its quota back. This is A1's subject —
    /// terminalise and reap.
    Abandoned,
    /// The Job's Pod is gone because its admission was withdrawn, and the run
    /// can still finish. Carries the evidence, for the log and the reason.
    Recoverable(String),
    /// The cluster could not be asked. Hold: a reap cannot be undone.
    Inconclusive(String),
}

/// `spec.suspend` on the Job, defaulting to false exactly as the API server does.
///
/// A suspended Job is nonterminal but NOT owed a Pod — its controller deletes
/// the Pods on purpose — so a missing Pod under it is expected rather than
/// evidence of abandonment.
pub(crate) fn job_is_suspended(job: &Job) -> bool {
    job.spec
        .as_ref()
        .and_then(|spec| spec.suspend)
        .unwrap_or(false)
}

/// The Workload Kueue keeps for `job_name`, resolved through the ownerReference
/// Kueue itself writes.
///
/// Deliberately not by name: the Workload's name is `job-<job>-<hash>` and the
/// hash is Kueue's business, while the owner link is part of its published
/// contract (`crate::workload_inventory` resolves task-runs the same way).
pub(crate) fn workload_of<'a>(
    workloads: &'a [KueueWorkload],
    job_name: &str,
) -> Option<&'a KueueWorkload> {
    workloads.iter().find(|workload| {
        workload
            .metadata
            .owner_references
            .as_ref()
            .is_some_and(|owners| {
                owners
                    .iter()
                    .any(|owner| owner.kind == "Job" && owner.name == job_name)
            })
    })
}

/// The Workload's record of having been evicted, if it carries one.
///
/// **Status-insensitive on purpose.** `classify_workload_admission` reads
/// `Evicted=True` because it answers "where is this Workload NOW"; this answers
/// "was this Workload's quota ever taken back", and the measured post-re-admission
/// value is `Evicted=False` with `message: "Previously: …"`. Requiring `True`
/// here would see nothing one second after the queue is released, which is the
/// exact sample that destroys a recoverable run.
pub(crate) fn workload_eviction_record(workload: &KueueWorkload) -> Option<String> {
    let evicted = workload
        .status
        .conditions
        .iter()
        .find(|condition| condition.condition_type == CONDITION_EVICTED)?;
    let name = workload.metadata.name.as_deref().unwrap_or("<unnamed>");
    Some(format!(
        "Kueue Workload {name} carries an Evicted condition (status {}{}{})",
        evicted.status,
        evicted
            .reason
            .as_deref()
            .map(|reason| format!(", reason {reason}"))
            .unwrap_or_default(),
        evicted
            .message
            .as_deref()
            .filter(|message| !message.is_empty())
            .map(|message| format!(", message {message}"))
            .unwrap_or_default(),
    ))
}

/// Why the fenced Pod of `job` is absent, read from the live cluster.
///
/// The Job is passed in rather than re-fetched: it is the same object the watch
/// just used to decide the Job is nonterminal, so the two halves of the decision
/// cannot disagree with each other.
pub(crate) async fn classify_absent_pod(
    client: &kube::Client,
    namespace: &str,
    job: &Job,
    job_name: &str,
) -> PodAbsenceVerdict {
    if job_is_suspended(job) {
        return PodAbsenceVerdict::Recoverable(format!(
            "Job {job_name} is suspended, so it is not owed a Pod (Kueue re-suspends an evicted \
             Job before its Pod is deleted)"
        ));
    }
    let workloads: Api<KueueWorkload> = Api::namespaced(client.clone(), namespace);
    match workloads.list(&ListParams::default()).await {
        Ok(list) => match workload_of(&list.items, job_name).and_then(workload_eviction_record) {
            Some(record) => PodAbsenceVerdict::Recoverable(record),
            None => PodAbsenceVerdict::Abandoned,
        },
        // The Workload API is not served here: this cluster has no Kueue, so no
        // eviction can have taken the Pod, and A1's containment is the whole
        // answer. Any OTHER error is an unanswered question, not a negative.
        Err(kube::Error::Api(response)) if response.code == 404 => PodAbsenceVerdict::Abandoned,
        Err(error) => PodAbsenceVerdict::Inconclusive(error.to_string()),
    }
}

#[cfg(test)]
#[path = "runtime_eviction_tests.rs"]
mod runtime_eviction_tests;
