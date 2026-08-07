//! Arming contract for the Kueue cutover (epic 4c9q, slice S1).
//!
//! These are the real current Job renderers, not copied label maps.
//!
//! Two halves:
//!
//! * `kueue_armed = false` (the shipped default) must reproduce the
//!   pre-cutover Job byte-for-byte: no `suspend`, no `kueue.x-k8s.io/queue-name`
//!   and no `djinn.io/kueue-build-object`, anywhere in the serialized object.
//! * `kueue_armed = true` must stamp all three onto BOTH `/metadata/labels` and
//!   `/spec/template/metadata/labels` — Kueue runs a Job webhook that reads the
//!   first and a Pod webhook that reads the second, and a post-render stamp that
//!   touches only the Job metadata (the `stamp_admission_identity` pattern in
//!   `graph_warmer_identity.rs:108`) silently satisfies half of that.

use djinn_k8s::config::{KubernetesConfig, LABEL_KUEUE_BUILD_OBJECT, LABEL_KUEUE_QUEUE_NAME};
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

// ---------------------------------------------------------------------------
// Whole-document sweep
// ---------------------------------------------------------------------------

/// Every JSON path in `value` whose key or string value mentions Kueue, plus
/// `suspend` wherever it appears.
///
/// Deliberately broader than [`reserved_label_locations`]: the disarmed
/// renderers must be byte-identical to the pre-cutover shape, so ANY new
/// Kueue-flavoured key, annotation, selector or `suspend` field is a
/// violation — not just the two label maps Kueue's webhooks read.
fn kueue_traces(value: &Value, path: &str, found: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let child_path = format!("{path}/{key}");
                if key == "suspend" || key.contains("kueue") {
                    found.push(format!("{child_path} = {child}"));
                }
                kueue_traces(child, &child_path, found);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                kueue_traces(child, &format!("{path}/{index}"), found);
            }
        }
        Value::String(text) if text.contains("kueue") => {
            found.push(format!("{path} = {text}"));
        }
        _ => {}
    }
}

fn kueue_surface(job: &Job) -> Vec<String> {
    let rendered = serde_json::to_value(job).expect("Job serializes");
    let mut found = Vec::new();
    kueue_traces(&rendered, "", &mut found);
    found.sort();
    found
}

fn disarmed_config() -> KubernetesConfig {
    let config = KubernetesConfig::for_testing();
    assert!(
        !config.kueue_armed,
        "kueue_armed must default to false; the disarmed half of this contract is vacuous otherwise"
    );
    config
}

fn armed_config() -> KubernetesConfig {
    KubernetesConfig {
        kueue_armed: true,
        kueue_local_queue_prefix: "release-djinn".into(),
        ..KubernetesConfig::for_testing()
    }
}

fn task_run_job(config: &KubernetesConfig) -> Job {
    build_task_run_job(
        config,
        &Uuid::nil(),
        "project-id",
        "task-run-secret",
        "registry.example/project:current",
        &[],
        None,
        false,
        None,
    )
}

fn warm_job(config: &KubernetesConfig) -> Job {
    build_warm_job(
        config,
        "project-id",
        "deadbeef",
        "registry.example/project:current",
        None,
        &[],
    )
}

fn scip_job(config: &KubernetesConfig) -> Job {
    build_scip_index_job(
        config,
        "project-id",
        "registry.example/project:current",
        "deadbeef1234567890",
        None,
        &[],
    )
}

// ---------------------------------------------------------------------------
// Disarmed (the shipped default)
// ---------------------------------------------------------------------------

#[test]
fn current_job_builders_do_not_opt_into_kueue_build_object_admission() {
    let config = disarmed_config();

    assert_not_a_kueue_build_object("task-run Job", &task_run_job(&config));
    assert_not_a_kueue_build_object("warm Job", &warm_job(&config));
    assert_not_a_kueue_build_object("standalone SCIP Job", &scip_job(&config));
}

#[test]
fn disarmed_renderers_carry_no_suspend_and_no_kueue_surface_at_all() {
    let config = disarmed_config();

    for (name, job) in [
        ("task-run Job", task_run_job(&config)),
        ("warm Job", warm_job(&config)),
        ("standalone SCIP Job", scip_job(&config)),
    ] {
        assert_eq!(
            kueue_surface(&job),
            Vec::<String>::new(),
            "{name} must be byte-identical to the pre-cutover shape when disarmed",
        );
    }
}

/// Non-vacuity for [`kueue_surface`]: a neutered sweep that always returned an
/// empty vector would pass the test above forever.
#[test]
fn kueue_surface_sweep_finds_the_armed_shape() {
    let armed = kueue_surface(&task_run_job(&armed_config()));

    assert!(
        armed.iter().any(|entry| entry == "/spec/suspend = true"),
        "sweep must observe the armed suspend field, got {armed:?}",
    );
    assert!(
        armed
            .iter()
            .filter(|entry| entry.contains(LABEL_KUEUE_QUEUE_NAME))
            .count()
            >= 2,
        "sweep must observe the queue-name label at both label locations, got {armed:?}",
    );
}

// ---------------------------------------------------------------------------
// Armed
// ---------------------------------------------------------------------------

/// Assert the Kueue opt-in at BOTH label locations explicitly, by JSON pointer.
///
/// Asserting only `job.metadata.labels` would pass against a post-render stamp
/// that misses Kueue's Pod webhook, which is the precise defect this checks for.
fn assert_kueue_labels_at_both_locations(name: &str, job: &Job, expected_queue: &str) {
    let rendered = serde_json::to_value(job).expect("Job serializes");
    for pointer in ["/metadata/labels", "/spec/template/metadata/labels"] {
        let labels = rendered
            .pointer(pointer)
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("{name} has no labels at {pointer}"));
        assert_eq!(
            labels.get(LABEL_KUEUE_BUILD_OBJECT).and_then(Value::as_str),
            Some("true"),
            "{name} must carry the build-object label at {pointer}",
        );
        assert_eq!(
            labels.get(LABEL_KUEUE_QUEUE_NAME).and_then(Value::as_str),
            Some(expected_queue),
            "{name} must target its own LocalQueue at {pointer}",
        );
    }
    assert_eq!(
        rendered.pointer("/spec/suspend"),
        Some(&Value::Bool(true)),
        "{name} must be created suspended so Kueue owns the admission decision",
    );
}

#[test]
fn armed_renderers_opt_into_kueue_at_both_label_locations() {
    let config = armed_config();

    assert_kueue_labels_at_both_locations(
        "task-run Job",
        &task_run_job(&config),
        "release-djinn-task-run",
    );
    assert_kueue_labels_at_both_locations("warm Job", &warm_job(&config), "release-djinn-warm");
    assert_kueue_labels_at_both_locations(
        "standalone SCIP Job",
        &scip_job(&config),
        "release-djinn-scip",
    );
}

/// The three kinds must land in three DIFFERENT LocalQueues. A helper that
/// returned one constant queue name would satisfy every per-kind assertion
/// above if each were read in isolation.
#[test]
fn armed_renderers_use_three_distinct_local_queues() {
    let config = armed_config();

    let queue_of = |job: &Job| {
        job.metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get(LABEL_KUEUE_QUEUE_NAME))
            .cloned()
            .expect("armed Job carries a queue-name label")
    };

    let queues = [
        queue_of(&task_run_job(&config)),
        queue_of(&warm_job(&config)),
        queue_of(&scip_job(&config)),
    ];
    let unique: std::collections::BTreeSet<_> = queues.iter().collect();
    assert_eq!(
        unique.len(),
        3,
        "task-run, warm and SCIP must use distinct LocalQueues, got {queues:?}",
    );
}

/// The queue name must follow the chart's `<djinn.fullname>-<kind>` LocalQueue
/// naming, or an armed Job names a LocalQueue that does not exist and is never
/// admitted — i.e. hangs forever.
#[test]
fn local_queue_prefix_is_configurable_and_not_hard_coded() {
    let config = KubernetesConfig {
        kueue_armed: true,
        kueue_local_queue_prefix: "some-other-release".into(),
        ..KubernetesConfig::for_testing()
    };

    assert_eq!(
        task_run_job(&config)
            .metadata
            .labels
            .expect("labels")
            .get(LABEL_KUEUE_QUEUE_NAME)
            .map(String::as_str),
        Some("some-other-release-task-run"),
    );
}

// ---------------------------------------------------------------------------
// Scanner self-test — retained verbatim from the inert-release contract.
// ---------------------------------------------------------------------------

#[test]
fn label_scanner_rejects_explicit_job_and_pod_template_fixtures() {
    let config = KubernetesConfig::for_testing();
    let rendered = build_warm_job(
        &config,
        "project-id",
        "deadbeef",
        "registry.example/project:current",
        None,
        &[],
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
