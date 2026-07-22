//! Regression guards for Kubernetes identifier budgets on the warm Job.
//!
//! The graph warmer once stalled for a full day because every `Job` create was
//! rejected 422: an 88-byte colon-separated `work_id` was stamped straight
//! into `djinn.app/admission-work-id`, and the 67-char deterministic Job name
//! overran the `job-name` label the Job controller derives from
//! `metadata.name`. Nothing caught it — the existing admission tests assert
//! against an in-memory inventory, which performs no apiserver validation, so
//! the invalid manifest type-checked and shipped.
//!
//! These tests validate the manifest *as dispatched*, so any future label
//! added to the warm Job is checked against the real grammar before it can
//! reach a cluster.

use super::*;
use crate::config::KubernetesConfig;
use crate::label_value::{LABEL_VALUE_MAX_BYTES, is_valid_label_value};
use crate::warm_job::{
    ANNOTATION_FENCING_TOKEN, ANNOTATION_GRAPH_REVISION, ANNOTATION_WARM_REQUEST_ID,
    GATE_AUTHORIZATION_KEY, VOLUME_WARM_GATE, build_leased_warm_job, build_warm_job,
};

/// A realistic production project id — a full 36-char UUIDv7, which is what
/// overran every budget in the original failure.
const PROJECT_ID: &str = "019ea3bd-a305-73e3-806c-4edcc96ebfe2";
/// A realistic 40-char git revision.
const REVISION: &str = "d6360bb71ebb0824da8c85b4633e582c879c983b";

fn warm_request(project_id: &str, revision: &str) -> WarmAdmissionRequest {
    let work_id = warm_work_id(project_id, revision);
    WarmAdmissionRequest {
        domain: "graph-warm".to_string(),
        object_name: deterministic_warm_job_name(project_id, &work_id),
        work_id,
        generation: 1,
    }
}

#[test]
fn leased_manifest_is_deterministic_and_closed_until_its_uid_is_authorized() {
    let cfg = KubernetesConfig::for_testing();
    let identity = LeasedWarmJobIdentity::new(PROJECT_ID, "warm-request-019f", REVISION, 73);
    let retry = LeasedWarmJobIdentity::new(PROJECT_ID, "warm-request-019f", REVISION, 73);
    assert_eq!(identity.object_name, retry.object_name);

    let job = build_leased_warm_job(&cfg, PROJECT_ID, "example/warm:latest", None, &identity);
    assert_eq!(
        job.metadata.name.as_deref(),
        Some(identity.object_name.as_str())
    );
    let annotations = job.metadata.annotations.as_ref().expect("Job annotations");
    let template = job
        .spec
        .as_ref()
        .expect("Job spec")
        .template
        .metadata
        .as_ref()
        .expect("template metadata");
    assert_eq!(template.annotations.as_ref(), Some(annotations));
    assert_eq!(
        annotations
            .get(ANNOTATION_WARM_REQUEST_ID)
            .map(String::as_str),
        Some("warm-request-019f")
    );
    assert_eq!(
        annotations
            .get(ANNOTATION_GRAPH_REVISION)
            .map(String::as_str),
        Some(REVISION)
    );
    assert_eq!(
        annotations
            .get(ANNOTATION_FENCING_TOKEN)
            .map(String::as_str),
        Some("73")
    );

    // Durable request/revision/fencing identities are tracing and inventory
    // annotations, never labels that could become unbounded metric dimensions.
    for (where_, labels) in [
        ("Job metadata", job.metadata.labels.as_ref()),
        ("Pod-template metadata", template.labels.as_ref()),
    ] {
        let labels = labels.expect("bounded metadata labels");
        for (key, value) in [
            (ANNOTATION_WARM_REQUEST_ID, identity.warm_request_id.as_str()),
            (ANNOTATION_GRAPH_REVISION, identity.graph_revision.as_str()),
            (ANNOTATION_FENCING_TOKEN, "73"),
        ] {
            assert!(
                !labels.contains_key(key),
                "{where_} must not expose high-cardinality identity key {key} as a label"
            );
            assert!(
                !labels.values().any(|label_value| label_value == value),
                "{where_} must not expose high-cardinality identity value {value:?} as a label"
            );
        }
    }

    let pod = job.spec.as_ref().unwrap().template.spec.as_ref().unwrap();
    let gate = pod
        .init_containers
        .as_ref()
        .expect("closed gate init container");
    assert_eq!(gate.len(), 1);
    let script = gate[0].command.as_ref().expect("gate command")[2].as_str();
    assert!(
        script.contains("while :; do"),
        "gate must be closed by default: {script}"
    );
    assert!(
        script.contains("DJINN_BOUND_POD_UID"),
        "gate must bind UID: {script}"
    );
    assert!(
        script.contains("DJINN_WARM_FENCING_TOKEN"),
        "gate must fence authorization: {script}"
    );
    assert!(script.contains(GATE_AUTHORIZATION_KEY));
    let bound_uid = gate[0]
        .env
        .as_ref()
        .expect("gate UID environment")
        .iter()
        .find(|env| env.name == "DJINN_BOUND_POD_UID")
        .expect("bound UID environment variable");
    assert_eq!(
        bound_uid
            .value_from
            .as_ref()
            .and_then(|source| source.field_ref.as_ref())
            .map(|field_ref| field_ref.field_path.as_str()),
        Some("metadata.uid"),
        "gate authorization must be scoped to the immutable Pod UID, not its reusable name"
    );
    assert!(pod.containers[0].command.as_ref().unwrap()[2].contains("warm-graph"));

    let gate_volume = pod
        .volumes
        .as_ref()
        .unwrap()
        .iter()
        .find(|volume| volume.name == VOLUME_WARM_GATE)
        .expect("authorization volume");
    assert_eq!(
        gate_volume.config_map.as_ref().unwrap().optional,
        Some(true)
    );
    let warmer = &pod.containers[0];
    assert_eq!(
        warmer
            .resources
            .as_ref()
            .unwrap()
            .requests
            .as_ref()
            .unwrap()["cpu"]
            .0,
        "4"
    );
    assert_eq!(
        warmer.resources.as_ref().unwrap().limits.as_ref().unwrap()["cpu"].0,
        "4"
    );
    assert!(
        warmer
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .any(|mount| mount.name == crate::warm_job::VOLUME_WORKSPACE)
    );
    assert!(
        warmer
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .any(|mount| mount.name == crate::job::VOLUME_CACHE)
    );
}

/// Assert every label on `job` (both object and Pod-template) is legal, and
/// that `metadata.name` survives being defaulted into the `job-name` label.
fn assert_job_identifiers_are_kubernetes_legal(job: &Job) {
    let name = job.metadata.name.as_deref().expect("job name");
    assert!(
        is_valid_label_value(name),
        "metadata.name {name:?} ({} bytes) is not a legal label value; the Job \
         controller defaults it into spec.template.labels[job-name], so an \
         oversized name 422s the create",
        name.len()
    );

    let mut label_sets = [("metadata", job.metadata.labels.as_ref())].to_vec();
    if let Some(spec) = job.spec.as_ref() {
        label_sets.push((
            "spec.template.metadata",
            spec.template
                .metadata
                .as_ref()
                .and_then(|m| m.labels.as_ref()),
        ));
    }
    for (where_, labels) in label_sets {
        let Some(labels) = labels else { continue };
        for (key, value) in labels {
            assert!(
                is_valid_label_value(value),
                "{where_}.labels[{key}] = {value:?} ({} bytes) is not a legal \
                 Kubernetes label value (max {LABEL_VALUE_MAX_BYTES} bytes, \
                 alphanumeric/-/_/. only, alphanumeric edges)",
                value.len()
            );
        }
    }
}

#[test]
fn dispatched_warm_job_identifiers_are_kubernetes_legal() {
    let mut cfg = KubernetesConfig::for_testing();
    cfg.database_url = Some("postgres://djinn@djinn-postgres:5432/djinn".into());
    let mut job = build_warm_job(&cfg, PROJECT_ID, "reg.example:5000/p:abc123", None);
    stamp_admission_identity(&mut job, &warm_request(PROJECT_ID, REVISION));

    assert_job_identifiers_are_kubernetes_legal(&job);
}

#[test]
fn admission_labels_carry_the_identity_reconciliation_reads_back() {
    let mut cfg = KubernetesConfig::for_testing();
    cfg.database_url = Some("postgres://djinn@djinn-postgres:5432/djinn".into());
    let mut job = build_warm_job(&cfg, PROJECT_ID, "reg.example:5000/p:abc123", None);
    let request = warm_request(PROJECT_ID, REVISION);
    stamp_admission_identity(&mut job, &request);

    let labels = job.metadata.labels.as_ref().expect("labels");
    assert_eq!(
        labels
            .get(crate::workload_inventory::LABEL_ADMISSION_DOMAIN)
            .map(String::as_str),
        Some("warm_build")
    );
    // The label must equal the durable work id verbatim: inventory
    // reconciliation rebuilds the journal key from this label, so any
    // sanitisation applied here (rather than at construction) would make the
    // recovered key un-matchable against the journal row.
    assert_eq!(
        labels
            .get(crate::workload_inventory::LABEL_ADMISSION_WORK_ID)
            .map(String::as_str),
        Some(request.work_id.as_str())
    );
    assert_eq!(
        labels
            .get(crate::workload_inventory::LABEL_ADMISSION_GENERATION)
            .map(String::as_str),
        Some("1")
    );
}

#[test]
fn work_id_is_natively_label_safe_and_revision_scoped() {
    let work_id = warm_work_id(PROJECT_ID, REVISION);
    assert!(
        is_valid_label_value(&work_id),
        "work_id {work_id:?} is not a legal label value"
    );
    // Distinct revisions are distinct admission identities — otherwise a warm
    // for a new HEAD would be deduped against the previous generation.
    let other = warm_work_id(PROJECT_ID, "96dc7aa21ad520b0d435ddbecafbe14b254589fd");
    assert_ne!(work_id, other);
    // Distinct projects stay distinct.
    assert_ne!(
        work_id,
        warm_work_id("019f7cbc-f0b2-7f73-ae27-d51460869dc3", REVISION)
    );
}

#[test]
fn job_name_is_deterministic_and_within_the_label_budget() {
    let work_id = warm_work_id(PROJECT_ID, REVISION);
    let name = deterministic_warm_job_name(PROJECT_ID, &work_id);

    assert!(
        name.len() <= LABEL_VALUE_MAX_BYTES,
        "job name {name:?} is {} bytes, over the {LABEL_VALUE_MAX_BYTES}-byte \
         job-name label budget",
        name.len()
    );
    assert!(is_valid_label_value(&name));
    assert!(name.starts_with("djinn-warm-"), "name: {name}");
    // Determinism is what makes the create idempotent across retries.
    assert_eq!(name, deterministic_warm_job_name(PROJECT_ID, &work_id));
    // Different work ids must not collide onto one Job name.
    assert_ne!(
        name,
        deterministic_warm_job_name(PROJECT_ID, &warm_work_id(PROJECT_ID, "cafebabe"))
    );
}

#[test]
fn identifiers_stay_legal_for_hostile_project_ids() {
    // Project ids are not always tidy UUIDs — a slug-shaped or over-long id
    // must still yield legal identifiers rather than a silent 422.
    for project_id in [
        "019ea3bd-a305-73e3-806c-4edcc96ebfe2",
        "a",
        "UPPER-Case-Project",
        "owner/repo:weird*chars",
        &"x".repeat(200),
        "---leading-and-trailing---",
    ] {
        let work_id = warm_work_id(project_id, REVISION);
        let name = deterministic_warm_job_name(project_id, &work_id);
        assert!(
            is_valid_label_value(&work_id),
            "work_id {work_id:?} illegal for project {project_id:?}"
        );
        assert!(
            is_valid_label_value(&name),
            "job name {name:?} ({} bytes) illegal for project {project_id:?}",
            name.len()
        );
    }
}

#[test]
fn empty_revision_falls_back_without_producing_a_trailing_separator() {
    // `discover_mirror_main_tip` can fail; the fallback must still be legal
    // (a bare `gw.<project>.` would end on `.` and be rejected).
    for revision in ["", "***"] {
        let work_id = warm_work_id(PROJECT_ID, revision);
        assert!(
            is_valid_label_value(&work_id),
            "work_id {work_id:?} illegal for revision {revision:?}"
        );
        assert!(work_id.ends_with("unknown"), "work_id: {work_id}");
    }
}
