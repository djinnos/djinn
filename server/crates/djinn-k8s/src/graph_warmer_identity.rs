//! Admission identity for graph-warm Jobs: the work id, the deterministic
//! Job name, and the labels that carry both onto the manifest.
//!
//! Everything here is budgeted against Kubernetes label validation rather
//! than the (far more generous) object-name budget, because all three values
//! end up in `metadata.labels` or `spec.template.labels` — the Job controller
//! defaults `metadata.name` into `job-name`, so a name that is legal as a name
//! but oversized as a label rejects the entire create with a 422.

use k8s_openapi::api::batch::v1::Job;

use crate::graph_warmer::WarmAdmissionRequest;
use crate::label_value::LABEL_VALUE_MAX_BYTES;

/// Immutable inputs supplied by the durable warm-lease protocol before a Job
/// is created. Its request id is the stable idempotency key, not a
/// process-local attempt id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeasedWarmJobIdentity {
    pub warm_request_id: String,
    pub graph_revision: String,
    pub object_name: String,
    pub fencing_token: u64,
}

impl LeasedWarmJobIdentity {
    /// Derive the deterministic object name from the persisted request id.
    pub fn new(
        project_id: &str,
        warm_request_id: impl Into<String>,
        graph_revision: impl Into<String>,
        fencing_token: u64,
    ) -> Self {
        let warm_request_id = warm_request_id.into();
        Self {
            object_name: deterministic_warm_job_name(project_id, &warm_request_id),
            warm_request_id,
            graph_revision: graph_revision.into(),
            fencing_token,
        }
    }
}

/// Longest project segment that keeps [`deterministic_warm_job_name`] inside
/// the label-value budget: `djinn-warm-` (11) + project + `-g1-` (4) +
/// 16 hex digits = 31 + project, so the project segment gets 32 bytes.
const WARM_NAME_PROJECT_BUDGET: usize = LABEL_VALUE_MAX_BYTES - 31;

/// Reduce `raw` to lowercase alphanumerics and `-`, capped at `budget` bytes,
/// with non-alphanumeric edges trimmed so the result can sit at the end of a
/// label value.
fn warm_id_segment(raw: &str, budget: usize) -> String {
    let mapped: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(budget)
        .collect();
    mapped
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_string()
}

/// Stable admission identity for one (project, revision) warm.
///
/// This value is stamped into `djinn.app/admission-work-id` and read back out
/// by inventory reconciliation to rebuild the journal key, so it must be a
/// legal label value *natively* — sanitising it at the stamp site would make
/// the label-derived key differ from the durable one. Budget: `gw.` (3) +
/// project (≤32) + `.` (1) + revision (≤12) = 48 bytes worst case.
pub fn warm_work_id(project_id: &str, revision: &str) -> String {
    let project = warm_id_segment(project_id, WARM_NAME_PROJECT_BUDGET);
    let revision = warm_id_segment(revision, 12);
    let revision = if revision.is_empty() {
        "unknown".to_string()
    } else {
        revision
    };
    format!("gw.{project}.{revision}")
}

/// Deterministic Job name for one warm generation.
///
/// Kept within [`LABEL_VALUE_MAX_BYTES`] rather than the 253-byte object-name
/// budget, for the `job-name` projection described in the module docs.
/// Determinism is what makes the create idempotent across dispatch retries.
pub(crate) fn deterministic_warm_job_name(project_id: &str, work_id: &str) -> String {
    let project = warm_id_segment(project_id, WARM_NAME_PROJECT_BUDGET);
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in work_id.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("djinn-warm-{project}-g1-{hash:016x}")
}

/// Apply the admission identity (deterministic name + the three
/// `djinn.app/admission-*` labels) to a freshly built warm Job.
///
/// Extracted from the dispatch path so the manifest that actually reaches the
/// apiserver can be asserted against Kubernetes label validation in a unit
/// test — the validation gap that let an invalid manifest ship.
pub(crate) fn stamp_admission_identity(job: &mut Job, request: &WarmAdmissionRequest) {
    job.metadata.name = Some(request.object_name.clone());
    let labels = job.metadata.labels.get_or_insert_default();
    labels.insert(
        crate::workload_inventory::LABEL_ADMISSION_DOMAIN.into(),
        "warm_build".into(),
    );
    labels.insert(
        crate::workload_inventory::LABEL_ADMISSION_WORK_ID.into(),
        request.work_id.clone(),
    );
    labels.insert(
        crate::workload_inventory::LABEL_ADMISSION_GENERATION.into(),
        request.generation.to_string(),
    );
}

#[cfg(test)]
#[path = "graph_warmer_label_tests.rs"]
mod label_tests;
