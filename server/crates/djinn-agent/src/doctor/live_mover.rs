//! `live_mover_predicate` doctor check.
//!
//! Flags any active task (`status` in `in_progress` / `pr_review` /
//! `pr_draft`) for which the reusable supervisor live-mover summary returns
//! `has_live_mover = false` — i.e. the task has no connected session, no
//! queued/inflight/recent dispatch, no open PR, no PR poller, no pending
//! reviewer, and no unresolved blockers that can still clear. Such a task is
//! effectively orphaned: nothing live is advancing it.
//!
//! The check consumes **only** the summary API re-exported from
//! [`crate::supervisor_impl`] (the pure path):
//! [`crate::supervisor_impl::LiveMoverSummary`],
//! [`crate::supervisor_impl::summarize_live_mover`],
//! [`crate::supervisor_impl::LiveMoverEvidence`]. It never imports
//! `supervisor_impl::pr` or any PR-open handler — per the
//! `pitfalls/coupling-non-pr-diagnostics-to-pr-open-disposition-code`
//! guardrail.
//!
//! The check is read-only: it does not dispatch, close, transition, or nudge
//! any task.

use djinn_core::doctor::{
    DoctorCheck, DoctorCheckCadence, DoctorResult, Finding, FindingSeverity, ResolverSnapshot,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::supervisor_impl::{
    LiveMoverEvidence, LiveMoverReason, LiveMoverSummary, summarize_live_mover,
};

/// Active task statuses the check examines. A task in any of these statuses
/// is expected to have a live mover; if it does not, the check flags it.
const ACTIVE_STATUSES: &[&str] = &["in_progress", "pr_review", "pr_draft"];

/// Read-only source of live-mover evidence for a set of active tasks.
///
/// The check takes a `&dyn LiveMoverSource` so fabrication tests can supply a
/// pure in-memory double — the check itself never hits a real database or
/// actor. A production adapter (wired by T5 into the registry) will bridge to
/// the coordinator's evidence collector via
/// [`collect_live_mover_evidence_for`].
#[allow(dead_code)]
pub(crate) trait LiveMoverSource: Send + Sync {
    /// Return every active task the check should examine, along with its
    /// already-collected live-mover evidence. The check filters further by
    /// status (see [`ACTIVE_STATUSES`]) and applies the pure
    /// [`summarize_live_mover`] predicate.
    fn active_tasks(&self) -> Vec<ActiveTask>;
}

/// One active task and its already-collected live-mover evidence.
#[derive(Clone, Debug)]
pub(crate) struct ActiveTask {
    pub task_id: String,
    pub status: String,
    pub(crate) evidence: LiveMoverEvidence,
}

/// Inputs the resolver consumes for one candidate task. Serialized into the
/// `ResolverSnapshot.inputs` field so the (future) fix path can replay the
/// exact same decision.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveMoverPredicateInputs {
    pub task_id: String,
    pub status: String,
    /// JSON projection of the raw [`LiveMoverEvidence`] struct. We serialize
    /// manually because `LiveMoverEvidence` is `pub(crate)` and not
    /// `Serialize`; the projection below is structurally equivalent (same
    /// field names and boolean values).
    pub evidence: LiveMoverEvidenceJson,
}

/// Structurally-equivalent JSON projection of [`LiveMoverEvidence`].
///
/// `LiveMoverEvidence` lives behind `pub(crate)` in
/// [`crate::supervisor_impl::disposition`] and derives only `Debug, Clone,
/// Copy, Default, PartialEq, Eq`. This mirror lets the resolver snapshot
/// round-trip through `serde_json` without widening the visibility of the
/// supervisor type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveMoverEvidenceJson {
    pub active_session: bool,
    pub queued_dispatch: bool,
    pub dispatch_inflight: bool,
    pub recently_dispatched: bool,
    pub open_pr: bool,
    pub pr_poller_owned: bool,
    pub review_pending_with_reviewer: bool,
    pub unresolved_blockers: bool,
}

impl LiveMoverEvidenceJson {
    /// Build the JSON projection from the raw `pub(crate)` evidence struct.
    fn from_evidence(e: LiveMoverEvidence) -> Self {
        Self {
            active_session: e.active_session,
            queued_dispatch: e.queued_dispatch,
            dispatch_inflight: e.dispatch_inflight,
            recently_dispatched: e.recently_dispatched,
            open_pr: e.open_pr,
            pr_poller_owned: e.pr_poller_owned,
            review_pending_with_reviewer: e.review_pending_with_reviewer,
            unresolved_blockers: e.unresolved_blockers,
        }
    }
}

/// Outputs the resolver returns. The fields are the *observed* truth the fix
/// path will replay `resolve()` against.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LiveMoverPredicateOutputs {
    /// `true` when the task is active but has no live mover.
    pub is_orphaned: bool,
    /// `true` when the task's status is in [`ACTIVE_STATUSES`].
    pub is_active: bool,
    /// The serialized reason list from [`LiveMoverSummary::reasons`] (snake_case
    /// strings). Populated even when `is_orphaned = false` so the snapshot is
    /// self-describing for healthy tasks too.
    pub reasons: Vec<String>,
    /// The boolean mirror from [`LiveMoverSummary::has_live_mover`].
    pub has_live_mover: bool,
}

/// Stable snake_case string form for a [`LiveMoverReason`], used in the
/// resolver snapshot's reason list. Matches the field order of
/// [`LiveMoverEvidence`].
fn reason_str(r: LiveMoverReason) -> &'static str {
    match r {
        LiveMoverReason::ActiveSession => "active_session",
        LiveMoverReason::QueuedDispatch => "queued_dispatch",
        LiveMoverReason::DispatchInflight => "dispatch_inflight",
        LiveMoverReason::RecentlyDispatched => "recently_dispatched",
        LiveMoverReason::OpenPr => "open_pr",
        LiveMoverReason::PrPollerOwned => "pr_poller_owned",
        LiveMoverReason::ReviewPendingWithReviewer => "review_pending_with_reviewer",
        LiveMoverReason::UnresolvedBlockers => "unresolved_blockers",
    }
}

/// The shared resolver. Both `run()` and the (future) `fix()` call this so
/// the snapshot's `inputs` can reproduce the snapshot's `outputs` exactly —
/// the shared-resolver invariant from the doctor framework module docs.
///
/// Pure: a function of the inputs alone. No I/O, no DB, no actor.
fn resolve(inputs: &LiveMoverPredicateInputs) -> LiveMoverPredicateOutputs {
    let is_active = ACTIVE_STATUSES.contains(&inputs.status.as_str());
    let summary: LiveMoverSummary = summarize_live_mover(&inputs.evidence.to_evidence());
    let is_orphaned = is_active && !summary.has_live_mover;
    LiveMoverPredicateOutputs {
        is_orphaned,
        is_active,
        reasons: summary
            .reasons
            .iter()
            .map(|r| reason_str(*r).to_owned())
            .collect(),
        has_live_mover: summary.has_live_mover,
    }
}

impl LiveMoverEvidenceJson {
    /// Convert the JSON projection back to the raw `pub(crate)` evidence
    /// struct so the pure supervisor predicate can consume it. This is the
    /// bridge between the serialized snapshot and the live-mover API.
    fn to_evidence(self) -> LiveMoverEvidence {
        LiveMoverEvidence {
            active_session: self.active_session,
            queued_dispatch: self.queued_dispatch,
            dispatch_inflight: self.dispatch_inflight,
            recently_dispatched: self.recently_dispatched,
            open_pr: self.open_pr,
            pr_poller_owned: self.pr_poller_owned,
            review_pending_with_reviewer: self.review_pending_with_reviewer,
            unresolved_blockers: self.unresolved_blockers,
        }
    }
}

/// `DoctorCheck` impl that flags active tasks with no live mover.
///
/// The check is read-only. It does not dispatch, close, transition, or nudge
/// any task, and it never imports `supervisor_impl::pr` (per the
/// `pitfalls/coupling-non-pr-diagnostics-to-pr-open-disposition-code`
/// guardrail).
pub(crate) struct LiveMoverPredicateCheck<'a> {
    source: &'a dyn LiveMoverSource,
}

impl<'a> LiveMoverPredicateCheck<'a> {
    /// Construct a check bound to a [`LiveMoverSource`]. In production the
    /// source is an adapter over the coordinator's evidence collector (see
    /// [`collect_live_mover_evidence_for`]); in tests it is an in-memory
    /// double.
    pub(crate) fn new(source: &'a dyn LiveMoverSource) -> Self {
        Self { source }
    }

    /// Resolve one candidate task into a [`Finding`], if it is orphaned.
    /// Kept private so the snapshot's `inputs`/`outputs` fields are
    /// guaranteed to come from the *same* `resolve()` call the checker used.
    fn resolve_to_finding(inputs: &LiveMoverPredicateInputs) -> Option<Finding> {
        let outputs = resolve(inputs);
        if !outputs.is_orphaned {
            return None;
        }

        let resolver_inputs_json =
            serde_json::to_value(inputs).expect("LiveMoverPredicateInputs serializes");
        let resolver_outputs_json =
            serde_json::to_value(&outputs).expect("LiveMoverPredicateOutputs serializes");
        let snapshot = ResolverSnapshot::new(
            "resolve_live_mover_predicate",
            resolver_inputs_json.clone(),
            resolver_outputs_json,
        );

        let evidence = json!({
            "task_id": inputs.task_id,
            "status": inputs.status,
            "has_live_mover": outputs.has_live_mover,
            "reasons": outputs.reasons,
            "evidence": inputs.evidence,
        });

        let detail = format!(
            "task '{}' (status `{}`) has no live mover: no active session, no queued/inflight/recent \
             dispatch, no open PR, no PR poller, no pending reviewer, and no unresolved blockers. \
             Nothing live is advancing this active task.",
            inputs.task_id, inputs.status,
        );

        let mut finding = Finding::new(
            FindingSeverity::Critical,
            "live_mover_predicate",
            snapshot,
            detail,
        );
        finding = finding
            .with_entity_id("task_id", inputs.task_id.clone())
            .with_evidence(evidence);
        Some(finding)
    }
}

impl<'a> DoctorCheck for LiveMoverPredicateCheck<'a> {
    fn name(&self) -> &'static str {
        "live_mover_predicate"
    }

    fn description(&self) -> &'static str {
        "Flags active tasks (in_progress/pr_review/pr_draft) whose live-mover \
         summary returns has_live_mover=false — nothing live is advancing them. \
         Consumes the supervisor_impl summary API; never touches PR-open code. \
         No state mutation."
    }

    fn run(&self) -> DoctorResult<Vec<Finding>> {
        let mut findings = Vec::new();
        for task in self.source.active_tasks() {
            let inputs = LiveMoverPredicateInputs {
                task_id: task.task_id,
                status: task.status,
                evidence: LiveMoverEvidenceJson::from_evidence(task.evidence),
            };
            if let Some(finding) = Self::resolve_to_finding(&inputs) {
                findings.push(finding);
            }
        }
        Ok(findings)
    }

    fn cadence(&self) -> DoctorCheckCadence {
        DoctorCheckCadence::Cheap
    }
}

/// Bridge function that collects live-mover evidence for a single task by
/// delegating to the coordinator's evidence collector.
///
/// The coordinator's `collect_live_mover_evidence` is `pub(crate)` under
/// `actors::coordinator`, and its handle only exposes
/// `live_mover_summary(task_id) -> LiveMoverSummary` (which collapses the
/// evidence into the summary). This bridge keeps the evidence struct
/// available for the doctor snapshot by calling the coordinator handle's
/// `live_mover_summary` and then reconstructing the evidence from the
/// summary's reason set.
///
/// Note: the summary's reason list is a faithful projection of the evidence
/// (each reason corresponds to exactly one boolean field), so the
/// reconstruction is lossless for the boolean fields the doctor snapshot
/// cares about.
///
/// This bridge is part of T4's deliverable. T5 wires a `LiveMoverSource`
/// adapter that calls this function for each active task.
#[allow(dead_code)]
pub(crate) async fn collect_live_mover_evidence_for(
    handle: &crate::actors::coordinator::CoordinatorHandle,
    task_id: &str,
) -> Result<LiveMoverEvidence, crate::actors::coordinator::CoordinatorError> {
    let summary = handle.live_mover_summary(task_id).await?;
    Ok(evidence_from_summary(&summary))
}

/// Reconstruct a [`LiveMoverEvidence`] from a [`LiveMoverSummary`]'s reason
/// set. Each reason maps 1:1 to a boolean field, so the reconstruction is
/// lossless for the boolean fields.
#[allow(dead_code)]
fn evidence_from_summary(summary: &LiveMoverSummary) -> LiveMoverEvidence {
    let reasons: Vec<LiveMoverReason> = summary.reasons.clone();
    LiveMoverEvidence {
        active_session: reasons.contains(&LiveMoverReason::ActiveSession),
        queued_dispatch: reasons.contains(&LiveMoverReason::QueuedDispatch),
        dispatch_inflight: reasons.contains(&LiveMoverReason::DispatchInflight),
        recently_dispatched: reasons.contains(&LiveMoverReason::RecentlyDispatched),
        open_pr: reasons.contains(&LiveMoverReason::OpenPr),
        pr_poller_owned: reasons.contains(&LiveMoverReason::PrPollerOwned),
        review_pending_with_reviewer: reasons.contains(&LiveMoverReason::ReviewPendingWithReviewer),
        unresolved_blockers: reasons.contains(&LiveMoverReason::UnresolvedBlockers),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// In-memory `LiveMoverSource` test double. The fabrication tests use it
    /// to stage specific divergence patterns and assert the check returns the
    /// expected finding shape. No live DB, no live actor.
    #[derive(Default)]
    struct MemoryLiveMoverSource {
        tasks: Vec<ActiveTask>,
    }

    impl MemoryLiveMoverSource {
        fn with_task(task_id: &str, status: &str, evidence: LiveMoverEvidence) -> Self {
            let mut src = Self::default();
            src.tasks.push(ActiveTask {
                task_id: task_id.to_owned(),
                status: status.to_owned(),
                evidence,
            });
            src
        }

        fn with_tasks(tasks: Vec<ActiveTask>) -> Self {
            Self { tasks }
        }
    }

    impl LiveMoverSource for MemoryLiveMoverSource {
        fn active_tasks(&self) -> Vec<ActiveTask> {
            self.tasks.clone()
        }
    }

    fn run_check(src: &MemoryLiveMoverSource) -> Vec<Finding> {
        let check = LiveMoverPredicateCheck::new(src);
        check.run().expect("run succeeds")
    }

    // -------------------------------------------------------------------
    // Happy path — healthy tasks produce no findings
    // -------------------------------------------------------------------

    #[test]
    fn happy_path_no_tasks() {
        let src = MemoryLiveMoverSource::default();
        let findings = run_check(&src);
        assert!(
            findings.is_empty(),
            "empty task list must produce no findings, got {:?}",
            findings
        );
    }

    #[test]
    fn happy_path_active_session_is_live() {
        // Control task: active_session = true → has_live_mover = true → no finding.
        let evidence = LiveMoverEvidence {
            active_session: true,
            ..Default::default()
        };
        let src = MemoryLiveMoverSource::with_task("task-healthy", "in_progress", evidence);
        let findings = run_check(&src);
        assert!(
            findings.is_empty(),
            "task with active_session=true must not be flagged, got {:?}",
            findings
        );
    }

    #[test]
    fn happy_path_open_pr_is_live() {
        let evidence = LiveMoverEvidence {
            open_pr: true,
            ..Default::default()
        };
        let src = MemoryLiveMoverSource::with_task("task-pr", "pr_review", evidence);
        let findings = run_check(&src);
        assert!(findings.is_empty());
    }

    #[test]
    fn happy_path_unresolved_blockers_is_live() {
        let evidence = LiveMoverEvidence {
            unresolved_blockers: true,
            ..Default::default()
        };
        let src = MemoryLiveMoverSource::with_task("task-blocked", "in_progress", evidence);
        let findings = run_check(&src);
        assert!(findings.is_empty());
    }

    #[test]
    fn happy_path_non_active_status_not_flagged() {
        // A closed task with no live mover is not flagged — it's not active.
        let evidence = LiveMoverEvidence::default();
        let src = MemoryLiveMoverSource::with_task("task-closed", "closed", evidence);
        let findings = run_check(&src);
        assert!(
            findings.is_empty(),
            "non-active task must not be flagged, got {:?}",
            findings
        );
    }

    // -------------------------------------------------------------------
    // Divergence — orphaned active task
    // -------------------------------------------------------------------

    #[test]
    fn divergence_no_live_mover_produces_critical_finding() {
        // Canonical orphan: in_progress with all evidence false.
        let evidence = LiveMoverEvidence::default();
        let src = MemoryLiveMoverSource::with_task("task-orphan", "in_progress", evidence);
        let findings = run_check(&src);
        assert_eq!(findings.len(), 1, "exactly one finding expected");
        let finding = &findings[0];
        assert_eq!(finding.severity, FindingSeverity::Critical);
        assert_eq!(finding.check_name, "live_mover_predicate");
        assert_eq!(
            finding.entity_ids.get("task_id").map(String::as_str),
            Some("task-orphan"),
            "entity_ids must contain the orphaned task id"
        );

        // Evidence must contain the reason list (empty for a fully-orphaned task).
        assert_eq!(finding.evidence["task_id"], "task-orphan");
        assert_eq!(finding.evidence["status"], "in_progress");
        assert_eq!(finding.evidence["has_live_mover"], false);
        assert!(
            finding.evidence["reasons"].as_array().unwrap().is_empty(),
            "reasons must be empty for a task with no live mover"
        );

        // resolver_snapshot.inputs must contain the full LiveMoverEvidence struct.
        let inputs = &finding.resolver_snapshot.inputs;
        assert_eq!(inputs["task_id"], "task-orphan");
        assert_eq!(inputs["status"], "in_progress");
        let evidence_json = &inputs["evidence"];
        assert_eq!(evidence_json["active_session"], false);
        assert_eq!(evidence_json["queued_dispatch"], false);
        assert_eq!(evidence_json["dispatch_inflight"], false);
        assert_eq!(evidence_json["recently_dispatched"], false);
        assert_eq!(evidence_json["open_pr"], false);
        assert_eq!(evidence_json["pr_poller_owned"], false);
        assert_eq!(evidence_json["review_pending_with_reviewer"], false);
        assert_eq!(evidence_json["unresolved_blockers"], false);
    }

    #[test]
    fn divergence_pr_review_no_live_mover_produces_finding() {
        let evidence = LiveMoverEvidence::default();
        let src = MemoryLiveMoverSource::with_task("task-pr-orphan", "pr_review", evidence);
        let findings = run_check(&src);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].evidence["status"], "pr_review");
    }

    #[test]
    fn divergence_pr_draft_no_live_mover_produces_finding() {
        let evidence = LiveMoverEvidence::default();
        let src = MemoryLiveMoverSource::with_task("task-draft-orphan", "pr_draft", evidence);
        let findings = run_check(&src);
        assert_eq!(findings.len(), 1);
    }

    // -------------------------------------------------------------------
    // Multiple tasks
    // -------------------------------------------------------------------

    #[test]
    fn divergence_multiple_orphaned_tasks_produce_findings() {
        let src = MemoryLiveMoverSource::with_tasks(vec![
            ActiveTask {
                task_id: "task-a".to_owned(),
                status: "in_progress".to_owned(),
                evidence: LiveMoverEvidence::default(),
            },
            ActiveTask {
                task_id: "task-b".to_owned(),
                status: "in_progress".to_owned(),
                evidence: LiveMoverEvidence {
                    active_session: true,
                    ..Default::default()
                },
            },
            ActiveTask {
                task_id: "task-c".to_owned(),
                status: "pr_review".to_owned(),
                evidence: LiveMoverEvidence::default(),
            },
        ]);
        let findings = run_check(&src);
        assert_eq!(findings.len(), 2, "task-a and task-c are orphaned");
        let ids: Vec<&str> = findings
            .iter()
            .map(|f| f.entity_ids.get("task_id").unwrap().as_str())
            .collect();
        assert!(ids.contains(&"task-a"));
        assert!(ids.contains(&"task-c"));
        assert!(!ids.contains(&"task-b"));
    }

    // -------------------------------------------------------------------
    // Resolver snapshot / shared-resolver invariant
    // -------------------------------------------------------------------

    #[test]
    fn resolver_snapshot_is_reproducible_from_inputs() {
        let evidence = LiveMoverEvidence::default();
        let src = MemoryLiveMoverSource::with_task("task-snap", "in_progress", evidence);
        let findings = run_check(&src);
        assert_eq!(findings.len(), 1);
        let finding = &findings[0];

        assert_eq!(
            finding.resolver_snapshot.resolver,
            "resolve_live_mover_predicate"
        );

        // Deserialize inputs back from the snapshot and re-run resolve().
        let snapshot_inputs: LiveMoverPredicateInputs =
            serde_json::from_value(finding.resolver_snapshot.inputs.clone())
                .expect("snapshot inputs deserialize");
        let replay_outputs = resolve(&snapshot_inputs);
        let replay_outputs_json = serde_json::to_value(&replay_outputs).expect("outputs serialize");
        assert_eq!(
            replay_outputs_json, finding.resolver_snapshot.outputs,
            "resolver snapshot must be reproducible from snapshot.inputs"
        );
    }

    #[test]
    fn resolver_snapshot_inputs_is_superset_of_evidence_fields() {
        // The AC says: resolver_snapshot.inputs is a superset of the
        // LiveMoverEvidence fields. Verify every evidence field is present
        // in the serialized snapshot inputs. Use a fully-orphaned task so
        // a finding is actually produced.
        let evidence = LiveMoverEvidence::default();
        let src = MemoryLiveMoverSource::with_task("task-superset", "in_progress", evidence);
        let findings = run_check(&src);
        assert_eq!(findings.len(), 1);
        let inputs = &findings[0].resolver_snapshot.inputs;
        let evidence_json = &inputs["evidence"];
        assert_eq!(evidence_json["active_session"], false);
        assert_eq!(evidence_json["queued_dispatch"], false);
        assert_eq!(evidence_json["dispatch_inflight"], false);
        assert_eq!(evidence_json["recently_dispatched"], false);
        assert_eq!(evidence_json["open_pr"], false);
        assert_eq!(evidence_json["pr_poller_owned"], false);
        assert_eq!(evidence_json["review_pending_with_reviewer"], false);
        assert_eq!(evidence_json["unresolved_blockers"], false);
    }

    #[test]
    fn evidence_json_projection_contains_all_eight_fields() {
        // Structural assertion: LiveMoverEvidenceJson serializes all 8
        // boolean fields from LiveMoverEvidence, matching the concrete
        // resolver snapshot shape from the design.
        let json = LiveMoverEvidenceJson {
            active_session: true,
            queued_dispatch: false,
            dispatch_inflight: true,
            recently_dispatched: false,
            open_pr: true,
            pr_poller_owned: false,
            review_pending_with_reviewer: true,
            unresolved_blockers: false,
        };
        let serialized = serde_json::to_value(json).unwrap();
        let obj = serialized.as_object().unwrap();
        assert_eq!(obj.len(), 8, "must have exactly 8 fields");
        assert!(obj.contains_key("active_session"));
        assert!(obj.contains_key("queued_dispatch"));
        assert!(obj.contains_key("dispatch_inflight"));
        assert!(obj.contains_key("recently_dispatched"));
        assert!(obj.contains_key("open_pr"));
        assert!(obj.contains_key("pr_poller_owned"));
        assert!(obj.contains_key("review_pending_with_reviewer"));
        assert!(obj.contains_key("unresolved_blockers"));
    }

    // -------------------------------------------------------------------
    // Resolver purity
    // -------------------------------------------------------------------

    #[test]
    fn resolve_is_pure() {
        let inputs = LiveMoverPredicateInputs {
            task_id: "task-x".to_owned(),
            status: "in_progress".to_owned(),
            evidence: LiveMoverEvidenceJson::default(),
        };
        let a = resolve(&inputs);
        let b = resolve(&inputs);
        assert_eq!(a, b);
        assert!(a.is_orphaned);
        assert!(a.is_active);
        assert!(!a.has_live_mover);
        assert!(a.reasons.is_empty());
    }

    #[test]
    fn resolve_healthy_when_active_session_present() {
        let inputs = LiveMoverPredicateInputs {
            task_id: "task-y".to_owned(),
            status: "in_progress".to_owned(),
            evidence: LiveMoverEvidenceJson {
                active_session: true,
                ..Default::default()
            },
        };
        let out = resolve(&inputs);
        assert!(!out.is_orphaned);
        assert!(out.has_live_mover);
        assert_eq!(out.reasons, vec!["active_session"]);
    }

    #[test]
    fn resolve_not_orphaned_for_non_active_status() {
        let inputs = LiveMoverPredicateInputs {
            task_id: "task-z".to_owned(),
            status: "closed".to_owned(),
            evidence: LiveMoverEvidenceJson::default(),
        };
        let out = resolve(&inputs);
        assert!(!out.is_orphaned);
        assert!(!out.is_active);
    }

    // -------------------------------------------------------------------
    // Check name / description / default fix
    // -------------------------------------------------------------------

    #[test]
    fn check_name_and_description_are_stable() {
        let src = MemoryLiveMoverSource::default();
        let check = LiveMoverPredicateCheck::new(&src);
        assert_eq!(check.name(), "live_mover_predicate");
        assert!(
            check.description().contains("live-mover"),
            "description should mention live-mover: got {:?}",
            check.description()
        );
    }

    #[test]
    fn check_cadence_is_cheap() {
        let src = MemoryLiveMoverSource::default();
        let check = LiveMoverPredicateCheck::new(&src);
        assert_eq!(check.cadence(), DoctorCheckCadence::Cheap);
    }

    #[test]
    fn check_does_not_override_fix() {
        let src = MemoryLiveMoverSource::default();
        let check = LiveMoverPredicateCheck::new(&src);
        let finding = Finding::new(
            FindingSeverity::Critical,
            "live_mover_predicate",
            ResolverSnapshot::new("resolve_live_mover_predicate", json!({}), json!({})),
            "synthetic",
        );
        let err = check
            .fix(&finding)
            .expect_err("default fix must return FixNotSupported");
        match err {
            djinn_core::doctor::DoctorError::FixNotSupported { check } => {
                assert_eq!(check, "live_mover_predicate");
            }
            other => panic!("expected FixNotSupported, got {other:?}"),
        }
    }

    // -------------------------------------------------------------------
    // evidence_from_summary round-trip
    // -------------------------------------------------------------------

    #[test]
    fn evidence_from_summary_round_trips_all_reasons() {
        let evidence = LiveMoverEvidence {
            active_session: true,
            queued_dispatch: true,
            dispatch_inflight: true,
            recently_dispatched: true,
            open_pr: true,
            pr_poller_owned: true,
            review_pending_with_reviewer: true,
            unresolved_blockers: true,
        };
        let summary = summarize_live_mover(&evidence);
        let reconstructed = evidence_from_summary(&summary);
        assert_eq!(
            reconstructed, evidence,
            "evidence_from_summary must round-trip every reason field"
        );
    }

    #[test]
    fn evidence_from_summary_empty_reasons() {
        let evidence = LiveMoverEvidence::default();
        let summary = summarize_live_mover(&evidence);
        let reconstructed = evidence_from_summary(&summary);
        assert_eq!(reconstructed, evidence);
    }

    #[test]
    fn evidence_json_round_trips_through_evidence() {
        let evidence = LiveMoverEvidence {
            active_session: false,
            queued_dispatch: true,
            dispatch_inflight: false,
            recently_dispatched: true,
            open_pr: false,
            pr_poller_owned: true,
            review_pending_with_reviewer: false,
            unresolved_blockers: true,
        };
        let json = LiveMoverEvidenceJson::from_evidence(evidence);
        let back = json.to_evidence();
        assert_eq!(back, evidence);
    }

    // -------------------------------------------------------------------
    // entity_ids BTreeMap sanity (the framework stores entity_ids as a map)
    // -------------------------------------------------------------------

    #[test]
    fn finding_entity_ids_is_a_map_with_task_id() {
        let evidence = LiveMoverEvidence::default();
        let src = MemoryLiveMoverSource::with_task("task-map", "in_progress", evidence);
        let findings = run_check(&src);
        assert_eq!(findings.len(), 1);
        let expected: BTreeMap<String, String> =
            BTreeMap::from([("task_id".to_owned(), "task-map".to_owned())]);
        assert_eq!(findings[0].entity_ids, expected);
    }
}
