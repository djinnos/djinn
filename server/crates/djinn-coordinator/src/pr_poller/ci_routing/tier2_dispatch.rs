//! Dispatching Lead under a Tier-2 lease (proposal `nafu`, wave 5).
//!
//! # The gap this closes
//!
//! Wave 3b's executor opened the Tier-2 lease and returned. Nothing dispatched
//! Lead, and nothing wrote the `ci_route` directive block that wave 4's
//! supervisor validator reads — so `read_arbiter_directive` had **no producer
//! anywhere in the workspace**, the validator only ever saw `NoRoute`, and a
//! route that reached Tier 2 left the task sitting in `pr_draft`/`pr_review`
//! with the legacy remediation path suppressed and no remedy in its place.
//!
//! This module is the producer. It does three things, in an order that matters:
//!
//! 1. write the route's keys into the arbitration row's `directive` column, so
//!    the supervisor can name the exact row its guard must resolve;
//! 2. bind the row to the lease with `attach_lead_session`, so the quiescence
//!    report can count "leases with a dispatched adjudication whose result has
//!    not been applied";
//! 3. `Escalate` the task into `needs_lead_intervention`, which is the only
//!    production transition into the Lead lane.
//!
//! The board transition is **last**. If the directive write fails, no Lead is
//! dispatched and the lease stays open for the next poll to retry; if the
//! transition fails, the arbitration row is already correct and the next poll
//! finds the lease held and retries the transition alone. The reverse order
//! would dispatch a Lead session with no route block, which the supervisor
//! would then adjudicate under the *legacy* arbiter contract — the exact
//! mixed-state the `ci_route` block exists to prevent.
//!
//! # What is not here
//!
//! No provider call, no dependency edge, no second retry mechanism, and no new
//! Lead result. The one board transition is the existing `Escalate` the
//! second-strike park rung already uses.

#[cfg(test)]
use djinn_db::{CiLane, CiOriginState, CiTier2Reason};
use djinn_db::{
    CiRouteAttemptRepository,
    repositories::task_arbitration::{CreateArbitrationParams, TaskArbitrationRepository},
};

use super::executor::CiTier2Handoff;
use crate::actor::CoordinatorActor;

/// What one Tier-2 dispatch attempt did. Reported, never silently dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CiTier2Dispatch {
    /// The arbitration row carries the route and the task is in the Lead lane.
    Dispatched,
    /// A Lead session is already adjudicating this hold cycle. Dispatching a
    /// second one would be the "at most one Lead adjudication for current
    /// evidence" rule broken by the code that enforces it.
    AlreadyInFlight,
    /// The arbitration row could not be written. Nothing was transitioned; the
    /// lease stays open and the next poll retries.
    ArbitrationUnavailable,
    /// The row was written but the board transition was refused. The next poll
    /// finds the lease held and retries the transition alone.
    TransitionRefused,
}

impl CoordinatorActor {
    /// Turn an opened Tier-2 lease into a Lead session bound to that lease.
    pub(crate) async fn dispatch_ci_tier2_lead(
        &self,
        task: &djinn_core::models::Task,
        handoff: &CiTier2Handoff,
        pr_url: Option<&str>,
    ) -> CiTier2Dispatch {
        let arbitrations = TaskArbitrationRepository::new(self.db.clone());
        let (hold_cycle, existing) = match arbitrations.resolve_current_hold_cycle(&task.id).await {
            Ok(pair) => pair,
            Err(error) => {
                tracing::warn!(
                    task_id = %task.short_id,
                    %error,
                    "ci route: could not resolve the hold cycle for a Tier-2 dispatch"
                );
                return CiTier2Dispatch::ArbitrationUnavailable;
            }
        };

        // An unconsumed row means a Lead session for this hold cycle is already
        // in flight or awaiting its result. The head-scoped Tier-2 lease
        // already forbids a second adjudication for this PR head; this is the
        // same rule enforced on the board side, where the arbitration row —
        // not the lease — is what a Lead session actually reads.
        if existing.is_some() {
            return CiTier2Dispatch::AlreadyInFlight;
        }

        let directive = route_directive(handoff);
        let created = arbitrations
            .try_create(CreateArbitrationParams {
                task_id: &task.id,
                hold_cycle,
                deadline_at: None,
                mirror_head_sha: None,
                github_head_sha: Some(&handoff.identity.pr_head_sha),
                pr_url,
                failing_ci_job_ids: &serde_json::Value::Array(Vec::new()),
                dossier: Some(&ci_dossier(handoff)),
                directive: Some(&directive),
                // A CI route's verification command is Lead's *output*, never
                // its input: seeding one here would be the platform inventing
                // the command the repair validator exists to refuse.
                verification_command: None,
                excluded_models: &serde_json::Value::Array(Vec::new()),
            })
            .await;
        match created {
            Ok(djinn_db::repositories::task_arbitration::TryCreateResult::Created(_)) => {}
            Ok(_) => return CiTier2Dispatch::AlreadyInFlight,
            Err(error) => {
                tracing::warn!(
                    task_id = %task.short_id,
                    %error,
                    "ci route: could not create the arbitration row for a Tier-2 dispatch"
                );
                return CiTier2Dispatch::ArbitrationUnavailable;
            }
        }

        // Bind the route to the session that will adjudicate it. The lease id
        // fences this: a stale caller cannot attach to a lease that has already
        // been resolved. It is what makes `unapplied_lead_results` in the
        // rollback quiescence report mean "a Lead was actually dispatched"
        // rather than "a lease exists".
        let routes = CiRouteAttemptRepository::new(self.db.clone());
        if let Err(error) = routes
            .attach_lead_session(
                &handoff.subject,
                &handoff.provider_action_key,
                &handoff.tier2_lease_id,
                &format!("arbitration:{}:{hold_cycle}", task.id),
            )
            .await
        {
            tracing::warn!(
                task_id = %task.short_id,
                %error,
                "ci route: could not bind the Lead dispatch to its Tier-2 lease"
            );
        }

        // Last, and only now. `Escalate` is the single production transition
        // into `needs_lead_intervention`, and it is legal from both origin
        // states this route can carry.
        if let Err(error) = self
            .task_repo()
            .transition(
                &task.id,
                djinn_core::models::TransitionAction::Escalate,
                "system",
                "coordinator",
                Some(&format!(
                    "CI evidence routing: {} on the {} lane needs adjudication",
                    handoff.reason.as_str(),
                    handoff.identity.lane.as_str()
                )),
                None,
            )
            .await
        {
            tracing::warn!(
                task_id = %task.short_id,
                status = %task.status,
                %error,
                "ci route: Tier-2 escalation refused; the arbitration row is written and \
                 the next poll retries the transition"
            );
            return CiTier2Dispatch::TransitionRefused;
        }

        tracing::info!(
            task_id = %task.short_id,
            lane = handoff.identity.lane.as_str(),
            reason = handoff.reason.as_str(),
            key = %handoff.provider_action_key,
            hold_cycle,
            "ci route: dispatched Lead under a Tier-2 lease"
        );
        CiTier2Dispatch::Dispatched
    }
}

/// The `ci_route` block the supervisor's `read_arbiter_directive` parses.
///
/// Every field is required on the reading side, and a block missing any of
/// them is `Malformed` — which applies nothing at all. That is deliberate: the
/// alternative is a Lead result applied against a route the guard cannot name,
/// which is the unguarded apply this whole wave exists to remove.
fn route_directive(handoff: &CiTier2Handoff) -> serde_json::Value {
    serde_json::json!({
        "kind": "ci_evidence_routing",
        "goal": "Adjudicate the captured CI evidence for this PR head. Reopen with a \
                 grounded repair and a repository-valid verification command, or reopen \
                 with an explicit diagnosis naming what remains unknown.",
        "terminal_disposition_required": false,
        "ci_route": {
            "lane": handoff.identity.lane.as_str(),
            "origin_state": handoff.origin_state.as_str(),
            "tier2_reason": handoff.reason.as_str(),
            "subject_kind": handoff.subject.kind.as_str(),
            "subject_id": handoff.subject.id,
            "provider_action_key": handoff.provider_action_key,
            "tier2_lease_id": handoff.tier2_lease_id,
            "pr_number": handoff.identity.pr_number,
            "pr_head_sha": handoff.identity.pr_head_sha,
            "run_id": handoff.identity.run_id,
            "run_head_sha": handoff.identity.run_head_sha,
            "dequeue_id": handoff.identity.dequeue_id,
            "evidence_references": handoff.evidence_references,
            // The commands the failing checks actually executed, read from the
            // Actions job logs by `reproduction_commands`. This is the
            // proposal's second permitted source — "directly exposed as a
            // command by CI evidence" — not project configuration and not a
            // command derived from a job name.
            //
            // Legitimately empty when no blocking check was reproducible, in
            // which case every repair on this route degrades to a diagnosis and
            // shows up in reporting as
            // `verification_command_not_repository_valid` rather than silently.
            "repository_commands": handoff.repository_commands,
        }
    })
}

/// The evidence dossier the Lead prompt renders.
fn ci_dossier(handoff: &CiTier2Handoff) -> serde_json::Value {
    serde_json::json!({
        "kind": "ci_evidence_routing_dossier",
        "summary": format!(
            "CI evidence on the {} lane reached Tier 2 ({}). Adjudicate from the captured \
             evidence only: no rerun, no requeue, and no approval of non-passing CI.",
            handoff.identity.lane.as_str(),
            handoff.reason.as_str(),
        ),
        "lane": handoff.identity.lane.as_str(),
        "origin_state": handoff.origin_state.as_str(),
        "tier2_reason": handoff.reason.as_str(),
        "pr_number": handoff.identity.pr_number,
        "pr_head_sha": handoff.identity.pr_head_sha,
        "run_id": handoff.identity.run_id,
        "run_head_sha": handoff.identity.run_head_sha,
        "dequeue_id": handoff.identity.dequeue_id,
        "evidence_references": handoff.evidence_references,
    })
}

/// Whether a lane/origin pair agrees with itself.
///
/// Used by the dispatch fixtures. A `pr_head` route whose origin says
/// `pr_review` would reopen from the wrong board state, and the two are
/// derived independently — the lane from the evidence, the origin from the
/// target — so they can genuinely disagree.
#[cfg(test)]
pub(crate) fn lane_agrees_with_origin(lane: CiLane, origin: CiOriginState) -> bool {
    matches!(
        (lane, origin),
        (CiLane::PrHead, CiOriginState::PrDraft) | (CiLane::MergeGroup, CiOriginState::PrReview)
    )
}

/// The Tier-2 reasons that may reach a Lead dispatch.
///
/// All five: the proposal's Tier-2 entry list is closed and every member of it
/// is adjudicable. Present as a function rather than as a comment so a sixth
/// reason added later fails to compile here.
#[cfg(test)]
pub(crate) fn dispatchable_reason(reason: CiTier2Reason) -> bool {
    match reason {
        CiTier2Reason::CausalFailure
        | CiTier2Reason::EvidenceUnknown
        | CiTier2Reason::ProviderActionFailed
        | CiTier2Reason::OutcomeUnknown
        | CiTier2Reason::RetryExhausted => true,
    }
}

#[cfg(test)]
mod tests;
