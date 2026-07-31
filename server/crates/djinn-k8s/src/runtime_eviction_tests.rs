//! The distinguisher, exercised over the shapes MEASURED on the live armed
//! cluster (kind, Kubernetes 1.31 / Kueue 0.19.0, 2026-07-31).
//!
//! Every Workload fixture below is a transcription of a real `kubectl get
//! workloads -o json` from that run, not an invented shape — the whole reason
//! `03z3` exists is that a fixture modelled behaviour the cluster does not have.
//! The one that matters most is
//! [`the_evicted_record_survives_re_admission_where_spec_suspend_does_not`]:
//! after re-admission Kueue leaves `Evicted` behind with `status: "False"`, and
//! a status-sensitive read of it would see nothing exactly when the run is most
//! destructible.

use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};

use super::*;
use crate::workload_inventory::{KueueWorkloadCondition, KueueWorkloadStatus};

fn condition(kind: &str, status: &str, reason: &str, message: &str) -> KueueWorkloadCondition {
    KueueWorkloadCondition {
        condition_type: kind.into(),
        status: status.into(),
        reason: Some(reason.into()),
        message: Some(message.into()),
    }
}

fn workload_for(job_name: &str, conditions: Vec<KueueWorkloadCondition>) -> KueueWorkload {
    KueueWorkload {
        metadata: ObjectMeta {
            name: Some(format!("job-{job_name}-1aa22")),
            owner_references: Some(vec![OwnerReference {
                api_version: "batch/v1".into(),
                kind: "Job".into(),
                name: job_name.into(),
                uid: "job-uid".into(),
                controller: Some(true),
                ..OwnerReference::default()
            }]),
            ..ObjectMeta::default()
        },
        status: KueueWorkloadStatus { conditions },
    }
}

/// Measured: an admitted, running Workload — no `Evicted` condition at all.
fn admitted_workload(job_name: &str) -> KueueWorkload {
    workload_for(
        job_name,
        vec![
            condition(
                "QuotaReserved",
                "True",
                "QuotaReserved",
                "Quota reserved in ClusterQueue djinn-kueue",
            ),
            condition("Admitted", "True", "Admitted", "The workload is admitted"),
            condition(
                "PodsReady",
                "True",
                "Started",
                "All pods reached readiness and the workload is running",
            ),
        ],
    )
}

/// Measured under `stopPolicy: HoldAndDrain`.
fn evicted_workload(job_name: &str) -> KueueWorkload {
    workload_for(
        job_name,
        vec![
            condition(
                "QuotaReserved",
                "False",
                "Inadmissible",
                "ClusterQueue djinn-kueue is inactive",
            ),
            condition(
                "Evicted",
                "True",
                "ClusterQueueStopped",
                "The ClusterQueue is stopped",
            ),
            condition(
                "Admitted",
                "False",
                "NoReservation",
                "The workload has no reservation",
            ),
        ],
    )
}

/// Measured ~1 second after the queue was released. `Evicted` is STILL THERE.
fn re_admitted_workload(job_name: &str) -> KueueWorkload {
    workload_for(
        job_name,
        vec![
            condition(
                "QuotaReserved",
                "True",
                "QuotaReserved",
                "Quota reserved in ClusterQueue djinn-kueue",
            ),
            condition(
                "Evicted",
                "False",
                "QuotaReserved",
                "Previously: The ClusterQueue is stopped",
            ),
            condition("Admitted", "True", "Admitted", "The workload is admitted"),
            condition(
                "Requeued",
                "True",
                "ClusterQueueRestarted",
                "The ClusterQueue was restarted after being stopped",
            ),
        ],
    )
}

fn job_with_suspend(suspend: Option<bool>) -> Job {
    Job {
        spec: Some(JobSpec {
            suspend,
            ..JobSpec::default()
        }),
        ..Job::default()
    }
}

#[test]
fn spec_suspend_is_read_as_the_api_server_defaults_it() {
    assert!(job_is_suspended(&job_with_suspend(Some(true))));
    assert!(!job_is_suspended(&job_with_suspend(Some(false))));
    assert!(
        !job_is_suspended(&job_with_suspend(None)),
        "an absent `suspend` is `false`: a disarmed cluster renders the key not at all, and \
         reading that as suspended would disable the containment everywhere Kueue is not used",
    );
    assert!(!job_is_suspended(&Job::default()));
}

/// THE ONE THAT CLOSES THE DEFECT.
///
/// A status-sensitive read (`Evicted == True`, which is what
/// `classify_workload_admission` correctly does for "where is this Workload
/// NOW") answers `None` for the re-admitted Workload — and the re-admitted
/// Workload's Job reads `suspend: false` with the fenced Pod still gone, which
/// is exactly the state the containment reaps. Measured live: the release of
/// the queue to a new Pod took ~1s against a 15s poll, so that IS the sample the
/// watch takes.
#[test]
fn the_evicted_record_survives_re_admission_where_spec_suspend_does_not() {
    let re_admitted = re_admitted_workload("job-x");
    assert!(!job_is_suspended(&job_with_suspend(Some(false))));

    let record = workload_eviction_record(&re_admitted)
        .expect("Kueue leaves the Evicted condition behind after re-admission");
    assert!(
        record.contains("status False") && record.contains("Previously: The ClusterQueue is"),
        "the record must carry Kueue's own words, so an operator can see WHICH eviction held \
         the reap off; got {record}",
    );

    let status_sensitive = re_admitted
        .status
        .conditions
        .iter()
        .find(|c| c.condition_type == CONDITION_EVICTED && c.status == "True");
    assert!(
        status_sensitive.is_none(),
        "fixture invariant: the re-admitted Workload's Evicted condition is False, which is why \
         this distinguisher must be status-INSENSITIVE",
    );
}

#[test]
fn an_evicted_workload_records_kueue_s_own_reason() {
    let record = workload_eviction_record(&evicted_workload("job-x"))
        .expect("an evicted Workload carries the condition");
    assert!(
        record.contains("status True") && record.contains("reason ClusterQueueStopped"),
        "got {record}",
    );
}

#[test]
fn a_workload_that_was_never_evicted_yields_no_record() {
    assert_eq!(workload_eviction_record(&admitted_workload("job-x")), None);
    assert_eq!(
        workload_eviction_record(&KueueWorkload::default()),
        None,
        "a Workload with no conditions at all is not evidence of an eviction",
    );
}

/// Resolution is by ownerReference, so ANOTHER run's eviction cannot hold this
/// run's reap off.
///
/// Without this, one preempted build in a busy namespace would disarm the
/// containment for every abandoned Job beside it.
#[test]
fn the_workload_is_resolved_through_its_owner_reference() {
    let workloads = vec![evicted_workload("other-job"), admitted_workload("mine")];
    assert_eq!(
        workload_of(&workloads, "mine").and_then(|w| w.metadata.name.clone()),
        Some("job-mine-1aa22".to_string()),
    );
    assert!(
        workload_of(&workloads, "mine")
            .and_then(workload_eviction_record)
            .is_none(),
        "the neighbouring Workload's eviction must not be read as this Job's",
    );
    assert!(
        workload_of(&workloads, "absent").is_none(),
        "a Job with no Workload has no eviction record, and is reaped as fbiy-A1 built it",
    );
}

/// An ownerReference to a *Pod* named like the Job is not the Job's Workload.
#[test]
fn owner_kind_is_part_of_the_match() {
    let mut workload = evicted_workload("mine");
    workload.metadata.owner_references.as_mut().unwrap()[0].kind = "Pod".into();
    assert!(workload_of(std::slice::from_ref(&workload), "mine").is_none());
}
