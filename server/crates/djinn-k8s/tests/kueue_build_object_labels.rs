//! Regression contract for the inert Kueue prerequisite release.
//!
//! These are the real current Job renderers, not copied label maps.  The later
//! Kueue cutover owns opting build objects in, so this release must not render
//! `djinn.io/kueue-build-object=true` on either the Job or its Pod template.

use djinn_k8s::config::KubernetesConfig;
use djinn_k8s::job::build_task_run_job;
use djinn_k8s::{build_scip_index_job, build_warm_job};
use k8s_openapi::api::batch::v1::Job;
use serde_json::Value;
use uuid::Uuid;

const KUEUE_BUILD_OBJECT_LABEL: &str = "djinn.io/kueue-build-object";

/// Scan the serialized Kubernetes API shape so this contract covers exactly
/// the labels Kueue's Job and Pod admission webhooks inspect.
fn reserved_label_locations(job: &Job) -> Vec<&'static str> {
    let rendered = serde_json::to_value(job).expect("Job serializes");
    [
        ("/metadata/labels", "Job metadata"),
        ("/spec/template/metadata/labels", "Pod-template metadata"),
    ]
    .into_iter()
    .filter_map(|(pointer, description)| {
        let labels = rendered.pointer(pointer).and_then(Value::as_object)?;
        (labels.get(KUEUE_BUILD_OBJECT_LABEL).and_then(Value::as_str) == Some("true"))
            .then_some(description)
    })
    .collect()
}

fn assert_not_a_kueue_build_object(name: &str, job: &Job) {
    assert!(
        reserved_label_locations(job).is_empty(),
        "{name} prematurely opts into Kueue at: {:?}",
        reserved_label_locations(job),
    );
}

#[test]
fn current_job_builders_do_not_opt_into_kueue_build_object_admission() {
    let config = KubernetesConfig::for_testing();
    let task_run = build_task_run_job(
        &config,
        &Uuid::nil(),
        "project-id",
        "task-run-secret",
        "registry.example/project:current",
        &[],
        None,
        false,
        None,
    );
    let warm = build_warm_job(
        &config,
        "project-id",
        "registry.example/project:current",
        None,
    );
    let scip = build_scip_index_job(
        &config,
        "project-id",
        "registry.example/project:current",
        "deadbeef1234567890",
        None,
    );

    assert_not_a_kueue_build_object("task-run Job", &task_run);
    assert_not_a_kueue_build_object("warm Job", &warm);
    assert_not_a_kueue_build_object("standalone SCIP Job", &scip);
}

#[test]
fn label_scanner_rejects_explicit_job_and_pod_template_fixtures() {
    let config = KubernetesConfig::for_testing();
    let rendered = build_warm_job(
        &config,
        "project-id",
        "registry.example/project:current",
        None,
    );

    let mut job_labelled = rendered.clone();
    job_labelled
        .metadata
        .labels
        .get_or_insert_default()
        .insert(KUEUE_BUILD_OBJECT_LABEL.into(), "true".into());
    assert_eq!(
        reserved_label_locations(&job_labelled),
        vec!["Job metadata"],
        "the scanner must reject an explicitly labelled Job fixture",
    );

    let mut pod_labelled = rendered;
    pod_labelled
        .spec
        .as_mut()
        .expect("builder supplies JobSpec")
        .template
        .metadata
        .get_or_insert_default()
        .labels
        .get_or_insert_default()
        .insert(KUEUE_BUILD_OBJECT_LABEL.into(), "true".into());
    assert_eq!(
        reserved_label_locations(&pod_labelled),
        vec!["Pod-template metadata"],
        "the scanner must reject an explicitly labelled Pod-template fixture",
    );
}
