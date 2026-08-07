// MCP tools for proposal refinement kickoff and status.
//
// The refinement workflow coordinates Advocate, Adversary, and Judge roles
// through bounded debate rounds. These tools expose the minimal control-plane
// surfaces: starting refinement, and reading the current refinement status
// derived from debate-trail entries.
//
// Refinement state is tracked via lightweight `proposal_revisions` lifecycle
// entries (`event_kind = "refinement_start"` / `"refinement_stop"`) with
// structured `event_metadata`. Current round and dry-round counts are derived
// from the debate trail at query time.

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::Deserialize;

use crate::server::DjinnMcpServer;
use crate::tools::proposal_ops::{
    DemandRoundResponse, NeedsEvidenceDemandResponse, NeedsEvidenceDemandResult,
    ProposalRefinementStartResponse, ProposalRefinementStatusModel,
    ProposalRefinementStatusResponse, ProposalRefinementStopResponse, VerdictOverrideResponse,
};
use crate::tools::proposal_tools::feedback::human_feedback_disposition_contract_available;
use crate::tools::refinement_helpers::validate_demand_evidence;
pub use crate::tools::refinement_helpers::{
    ProposalRefinementDemandEvidenceParams, build_refinement_status,
};
use djinn_core::models::NeedsEvidenceClaim;
use djinn_db::{
    AdmitRefinementRunRequest, ProposalRepository, RefinementAdmissionError,
    RefinementAdmissionOutcome, RefinementAdmissionSource, TaskRepository,
};
#[cfg(test)]
use djinn_db::{NeedsEvidenceClaimLink, ProposalDebateTrailCreateInput};

fn err_refinement_start(error: impl Into<String>) -> ProposalRefinementStartResponse {
    ProposalRefinementStartResponse {
        proposal_id: None,
        refinement: None,
        error: Some(error.into()),
    }
}

fn err_refinement_status(error: impl Into<String>) -> ProposalRefinementStatusResponse {
    ProposalRefinementStatusResponse {
        proposal_id: None,
        refinement: None,
        error: Some(error.into()),
    }
}

/// A rejected admission, rendered for a human but still machine-triageable.
///
/// `message` is what reaches an error toast, so it says what happened and what
/// to do next. `code` keeps the stable machine-readable classification as a
/// SEPARATE field — before this existed, the raw Rust variant name
/// (`"AlreadyActive"`) was the entire user-facing string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionRejection {
    pub code: &'static str,
    pub message: String,
}

/// Durable admission details. Boundary capture is owned by an actual admission,
/// not by an idempotent request that merely finds the same round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RefinementAdmission {
    /// Exact durable run selected by admission. A feedback boundary uses this
    /// after capture to mark its pending cohort admitted.
    pub run_id: String,
    pub pending_dispatch: bool,
    pub admitted: bool,
}

impl std::fmt::Display for AdmissionRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Render every [`RefinementAdmissionError`] variant as an actionable sentence.
pub fn admission_rejection(error: &RefinementAdmissionError) -> AdmissionRejection {
    match error {
        RefinementAdmissionError::AlreadyActive { .. } => AdmissionRejection {
            code: "already_active",
            message: "A tribunal round is already running for this proposal. \
                      Wait for it to finish (or stop it) before starting another."
                .to_owned(),
        },
        RefinementAdmissionError::GenerationConflict { .. } => AdmissionRejection {
            code: "generation_conflict",
            message: "This proposal's refinement run was replaced while the request was \
                      in flight. Reload the proposal and try again."
                .to_owned(),
        },
        RefinementAdmissionError::AdmissionConflict => AdmissionRejection {
            code: "admission_conflict",
            message: "Another refinement request for this proposal committed at the same \
                      moment. Reload the proposal and try again."
                .to_owned(),
        },
        RefinementAdmissionError::Database(_) => AdmissionRejection {
            code: "persistence",
            message: "Could not reach the database to record the refinement request. \
                      Try again; if it keeps failing the server needs attention."
                .to_owned(),
        },
        RefinementAdmissionError::ProposalNotFound { proposal_id } => AdmissionRejection {
            code: "proposal_not_found",
            message: format!(
                "Proposal {proposal_id} no longer exists, so refinement cannot be admitted."
            ),
        },
        RefinementAdmissionError::InvalidRequest(detail) => AdmissionRejection {
            code: "invalid_request",
            message: format!("The refinement request was rejected as invalid: {detail}."),
        },
    }
}

/// The sole control-plane admission path. The repository serializes liveness,
/// stale reaping, generation replacement, and the first durable dispatch intent;
/// coordinator delivery is only a post-commit hint.
pub(crate) async fn admit_refinement_run(
    server: &DjinnMcpServer,
    repo: &ProposalRepository,
    proposal_id: &str,
    source: RefinementAdmissionSource,
    explicit_owner_user_id: Option<&str>,
) -> Result<RefinementAdmission, AdmissionRejection> {
    // Every admission source — explicit start, demanded round, and
    // committed-revision resume alike — must leave the run attributed.
    // `proposals.refinement_owner_user_id` is the ONLY attribution
    // `drive_durable_refinement_intents` accepts: with it absent the coordinator
    // re-claims the run's dispatch intent on every tick and then `continue`s, so
    // the run never creates a role task, never appends a debate entry, and
    // reports `stale` in the gap between two claims — while `run_state` still
    // reads `active`. Demand and revision admission previously passed no owner
    // at all, which minted generation after generation of exactly that run.
    //
    // Resolution order is explicit override → already-persisted owner →
    // proposal author. Falling back to the persisted owner (rather than
    // unconditionally to the author) also stops a re-`start` from silently
    // reassigning a run that someone else already owns.
    let attributed_user = match explicit_owner_user_id {
        Some(explicit) => Some(explicit.to_owned()),
        None => repo
            .get(proposal_id)
            .await
            .map_err(|error| admission_rejection(&RefinementAdmissionError::Database(error)))?
            .and_then(|proposal| {
                proposal
                    .refinement_owner_user_id
                    .or(proposal.author_user_id)
            }),
    }
    .filter(|owner| !owner.trim().is_empty());

    let identity = match &source {
        RefinementAdmissionSource::ExplicitStart { actor } => format!("explicit:{actor}"),
        RefinementAdmissionSource::Demand { demand_id } => format!("demand:{demand_id}"),
        RefinementAdmissionSource::Revision { revision_id } => format!("revision:{revision_id}"),
    };
    // FNV-1a gives a bounded deterministic key even for unbounded demand text.
    let hash = identity.bytes().fold(0xcbf29ce484222325u64, |h, byte| {
        (h ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    let outcome = repo
        .reap_and_admit(AdmitRefinementRunRequest {
            proposal_id: proposal_id.to_owned(),
            idempotency_key: format!("refinement-admission/v1/{hash:016x}"),
            source,
            heartbeat_grace_millis: 60_000,
        })
        .await
        .map_err(|error| {
            let rejection = admission_rejection(&error);
            tracing::info!(
                proposal_id,
                code = rejection.code,
                %error,
                "refinement admission rejected"
            );
            rejection
        })?;
    let (run_id, admitted) = match outcome {
        RefinementAdmissionOutcome::Admitted { run_id, .. } => (run_id, true),
        RefinementAdmissionOutcome::Existing { run_id, .. } => (run_id, false),
    };
    // Only ever write a resolved owner. Writing `None` here would clear an
    // attribution that the durable dispatcher depends on, turning a working run
    // into a silently undispatchable one.
    if let Some(attributed_user) = attributed_user.as_deref() {
        repo.start_refinement_with_owner(proposal_id, Some(attributed_user))
            .await
            .map_err(|error| admission_rejection(&RefinementAdmissionError::Database(error)))?;
    }
    Ok(RefinementAdmission {
        run_id: run_id.clone(),
        pending_dispatch: match server.state.coordinator().await {
            Some(coordinator) => coordinator.wake_refinement_run(run_id).await.is_err(),
            None => true,
        },
        admitted,
    })
}

// ── Param structs ────────────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalRefinementStartParams {
    /// Proposal UUID or short_id.
    pub proposal_id: String,
    /// User the refinement run is attributed to: owner of the spawned
    /// refinement (tribunal) tasks and the scope for per-user role-model
    /// resolution. Omit to attribute the run to the proposal author.
    #[serde(default)]
    pub owner_user_id: Option<String>,
    /// Stable caller-generated identity for this start request. Reuse it when
    /// retrying the same request; omit it for a new independent start.
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalRefinementStatusParams {
    /// Proposal UUID or short_id.
    pub proposal_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalRefinementStopParams {
    /// Proposal UUID or short_id.
    pub proposal_id: String,
    /// Auditable operator explanation.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalRefinementDemandRoundParams {
    /// Proposal UUID or short_id.
    pub proposal_id: String,
    /// Why another round is being demanded. Recorded in proposal history.
    pub reason: Option<String>,
    /// Stable caller-generated identity for this demand. Reuse it when
    /// retrying the same request; omit it for a new independent demand.
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalRefinementResolveParams {
    /// Proposal UUID or short_id.
    pub proposal_id: String,
    /// The human's decision: `accept` (keep the refined spec) or `reject`
    /// (revert the live spec to the pre-refinement snapshot).
    pub decision: String,
    /// Optional reviewer note — why accepted/rejected. Recorded for the audit
    /// trail.
    #[serde(default)]
    pub feedback: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalVerdictOverrideParams {
    /// Proposal UUID or short_id.
    pub proposal_id: String,
    /// Why the verdict is being overridden. Required — recorded in proposal
    /// history for audit.
    pub reason: String,
    /// Optional debate-trail entry id of the verdict being overridden.
    #[serde(default)]
    pub overridden_verdict_entry_id: Option<String>,
}

fn err_demand_evidence(error: impl Into<String>) -> NeedsEvidenceDemandResponse {
    NeedsEvidenceDemandResponse {
        proposal_id: None,
        accepted: false,
        result: None,
        error: Some(error.into()),
        conflict_code: None,
    }
}
// ── Tool router ──────────────────────────────────────────────────────────────

#[tool_router(router = refinement_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Start proposal refinement. Validates the proposal is in a state that
    /// supports refinement (draft or in_review), records a `refinement_start`
    /// lifecycle entry, delegates to the coordinator to initialize the runtime
    /// refinement loop, and returns the initial refinement status.
    ///
    /// The coordinator is authoritative for duplicate-start rejection. If the
    /// coordinator rejects the start (e.g. duplicate active run), a
    /// `refinement_stop` lifecycle entry is recorded with the error reason.
    ///
    /// The autonomous tribunal (Adversary → Advocate → Judge) revises the spec
    /// in place and parks for a single human accept/reject when it converges.
    /// Same-model fallback is allowed when diverse models are unavailable —
    /// this is not presented as an error.
    #[tool(
        description = "Start proposal refinement for the given proposal. Validates the proposal exists and is in draft or in_review state. Records a refinement_start lifecycle event and delegates to the coordinator to initialize the runtime refinement loop. The autonomous tribunal revises the spec in place and parks for a single human accept/reject when the judge converges. Returns the initial refinement status. Same-model fallback is used when diverse models are unavailable."
    )]
    pub async fn proposal_refinement_start(
        &self,
        Parameters(p): Parameters<ProposalRefinementStartParams>,
    ) -> Json<ProposalRefinementStartResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());

        let Some(proposal) = repo.resolve(&p.proposal_id).await.ok().flatten() else {
            return Json(err_refinement_start(format!(
                "proposal not found: {}",
                p.proposal_id
            )));
        };
        // Only allow refinement for proposals in draft or in_review.
        if !matches!(proposal.status.as_str(), "draft" | "in_review") {
            return Json(err_refinement_start(format!(
                "proposal status '{}' does not support refinement (must be draft or in_review)",
                proposal.status
            )));
        }

        // Refinement dispatches tribunal tasks into the proposal's target
        // project (see create_refinement_task_with_context, which reads
        // targets[0].project_id). With no target the coordinator silently
        // fails task creation and terminates with an opaque agent_failure and
        // zero entries — so reject fast here with an actionable message.
        match repo.targets(&proposal.id).await {
            Ok(targets) if !targets.is_empty() => {}
            Ok(_) => {
                return Json(err_refinement_start(
                    "proposal has no target project; add one with proposal_add_target \
                     before starting refinement"
                        .to_string(),
                ));
            }
            Err(e) => {
                return Json(err_refinement_start(format!(
                    "failed to check proposal targets: {e}"
                )));
            }
        }

        // Gate only a boundary that can actually materialize a new feedback
        // obligation. Ordinary refinement remains available when there is no
        // uncaptured blocking source, and already-materialized generations stay
        // drainable when the active schema registry is incompatible.
        let controls = self.state.feedback_refinement_controls();
        let captures_new_feedback = if controls.capture {
            match repo
                .has_capturable_feedback_refinement_boundary(&proposal.id)
                .await
            {
                Ok(capturable) => capturable,
                Err(error) => {
                    return Json(err_refinement_start(format!(
                        "failed to inspect feedback capture boundary: {error}"
                    )));
                }
            }
        } else {
            false
        };
        if captures_new_feedback && !human_feedback_disposition_contract_available() {
            return Json(err_refinement_start(
                "human-feedback refinement capture requires compatible active Advocate and Judge role schemas"
                    .to_owned(),
            ));
        }

        let refinement_owner_user_id = p
            .owner_user_id
            .clone()
            .or_else(|| proposal.author_user_id.clone());
        // Preserve caller retries while giving each independently submitted start
        // a distinct durable key, even when attributed to the same actor.
        let request_id = p
            .request_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        let admission = match admit_refinement_run(
            self,
            &repo,
            &proposal.id,
            RefinementAdmissionSource::ExplicitStart {
                actor: format!(
                    "{}:request:{request_id}",
                    refinement_owner_user_id
                        .clone()
                        .unwrap_or_else(|| "proposal-author".to_owned())
                ),
            },
            p.owner_user_id.as_deref(),
        )
        .await
        {
            Ok(admission) => admission,
            Err(rejection) => return Json(err_refinement_start(rejection.message)),
        };

        if controls.capture
            && admission.admitted
            && let Err(error) = repo
                .capture_feedback_refinement_boundary(&proposal.id)
                .await
        {
            return Json(err_refinement_start(format!(
                "refinement was admitted but feedback boundary capture failed: {error}"
            )));
        }

        let refinement = ProposalRefinementStatusModel {
            active: true,
            run_id: None,
            generation: None,
            run_state: None,
            liveness: None,
            liveness_evidence: None,
            last_heartbeat_at: None,
            current_round: Some(1),
            dry_rounds: 0,
            total_entries: 0,
            stop_reason: None,
            awaiting_review: false,
            judge_summary: None,
            snapshot_revision_seq: None,
            needs_evidence: None,
            evidence_lifecycle_state: crate::tools::proposal_ops::EvidenceLifecycleState::Active,
        };

        Json(ProposalRefinementStartResponse {
            proposal_id: Some(proposal.id),
            refinement: Some(refinement),
            error: admission
                .pending_dispatch
                .then(|| "accepted; dispatch pending".to_owned()),
        })
    }

    /// Read the current refinement status for a proposal. Returns whether
    /// refinement is active, the current round, dry-round count, total
    /// debate-trail entries, update-authority mode, and stop reason (if any).
    ///
    /// Status is a read-only exact-run repository snapshot. Legacy debate data
    /// may fill compatible display fields but cannot affect run liveness.
    #[tool(
        description = "Read a read-only exact-run proposal refinement snapshot. Returns compatible active, round, count, and stop fields plus optional run ID, generation, state, and bounded liveness evidence. Legacy debate data may populate display fields but cannot affect exact-run liveness."
    )]
    pub async fn proposal_refinement_status(
        &self,
        Parameters(p): Parameters<ProposalRefinementStatusParams>,
    ) -> Json<ProposalRefinementStatusResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());

        let Some(proposal) = repo.resolve(&p.proposal_id).await.ok().flatten() else {
            return Json(err_refinement_status(format!(
                "proposal not found: {}",
                p.proposal_id
            )));
        };

        match build_refinement_status(&repo, &proposal.id).await {
            Ok(refinement) => Json(ProposalRefinementStatusResponse {
                proposal_id: Some(proposal.id),
                refinement: Some(refinement),
                error: None,
            }),
            Err(e) => Json(err_refinement_status(e)),
        }
    }

    /// Demand another tribunal round for a proposal whose refinement has
    /// stopped (e.g. after a judge verdict, round cap, or spawn cap) or is
    /// parked awaiting human review.
    /// Reuses the existing coordinator refinement loop — clears the stop or
    /// parked review state and re-enqueues an Advocate→Adversary→Judge cycle.
    /// Records the demand action in proposal history.
    #[tool(
        description = "Demand another tribunal round for a proposal whose refinement has stopped (e.g. after a judge verdict or round cap) or is parked awaiting human review. Reuses the existing coordinator refinement loop. Records the action in proposal history. Returns an error if refinement is still actively running."
    )]
    pub async fn proposal_refinement_demand_round(
        &self,
        Parameters(p): Parameters<ProposalRefinementDemandRoundParams>,
    ) -> Json<DemandRoundResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());

        let Some(proposal) = repo.resolve(&p.proposal_id).await.ok().flatten() else {
            return Json(DemandRoundResponse {
                proposal_id: None,
                accepted: false,
                refinement: None,
                error: Some(format!("proposal not found: {}", p.proposal_id)),
            });
        };

        let current_refinement = match build_refinement_status(&repo, &proposal.id).await {
            Ok(status) => status,
            Err(e) => {
                return Json(DemandRoundResponse {
                    proposal_id: Some(proposal.id),
                    accepted: false,
                    refinement: None,
                    error: Some(e),
                });
            }
        };

        // Only allow demanding a round for proposals in draft or in_review,
        // except for the explicit human-review path: a converged tribunal parks
        // in `awaiting_review` while the latest start remains lifecycle-active,
        // and a human may demand another round from that parked state.
        if !matches!(proposal.status.as_str(), "draft" | "in_review")
            && !current_refinement.awaiting_review
        {
            return Json(DemandRoundResponse {
                proposal_id: Some(proposal.id),
                accepted: false,
                refinement: None,
                error: Some(format!(
                    "proposal status '{}' does not support demanding a refinement round (must be draft or in_review)",
                    proposal.status
                )),
            });
        }

        // The repository is the sole live/stale authority. Do not derive an
        // admission rejection from legacy lifecycle rows here.

        // Same missing-target blind spot as refinement_start: a demanded round
        // dispatches a fresh tribunal task, which needs a target project. Reject
        // fast here rather than terminating with an opaque agent_failure.
        match repo.targets(&proposal.id).await {
            Ok(targets) if !targets.is_empty() => {}
            Ok(_) => {
                return Json(DemandRoundResponse {
                    proposal_id: Some(proposal.id),
                    accepted: false,
                    refinement: None,
                    error: Some(
                        "proposal has no target project; add one with proposal_add_target \
                         before demanding a refinement round"
                            .to_string(),
                    ),
                });
            }
            Err(e) => {
                return Json(DemandRoundResponse {
                    proposal_id: Some(proposal.id),
                    accepted: false,
                    refinement: None,
                    error: Some(format!("failed to check proposal targets: {e}")),
                });
            }
        }

        // As with explicit start, fail closed only when this demand can create a
        // new human-feedback generation. A plain demanded round does not depend
        // on the source-specific disposition schema.
        let controls = self.state.feedback_refinement_controls();
        let captures_new_feedback = if controls.capture {
            match repo
                .has_capturable_feedback_refinement_boundary(&proposal.id)
                .await
            {
                Ok(capturable) => capturable,
                Err(error) => {
                    return Json(DemandRoundResponse {
                        proposal_id: Some(proposal.id),
                        accepted: false,
                        refinement: Some(current_refinement),
                        error: Some(format!(
                            "failed to inspect feedback capture boundary: {error}"
                        )),
                    });
                }
            }
        } else {
            false
        };
        if captures_new_feedback && !human_feedback_disposition_contract_available() {
            return Json(DemandRoundResponse {
                proposal_id: Some(proposal.id),
                accepted: false,
                refinement: Some(current_refinement),
                error: Some(
                    "human-feedback refinement capture requires compatible active Advocate and Judge role schemas"
                        .to_owned(),
                ),
            });
        }

        let request_id = p
            .request_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        let demand_id = format!(
            "reason:{}:request:{request_id}",
            p.reason.as_deref().unwrap_or("unspecified-demand")
        );
        let admission = match admit_refinement_run(
            self,
            &repo,
            &proposal.id,
            RefinementAdmissionSource::Demand { demand_id },
            None,
        )
        .await
        {
            Ok(admission) => admission,
            Err(rejection) => {
                return Json(DemandRoundResponse {
                    proposal_id: Some(proposal.id),
                    accepted: false,
                    refinement: Some(current_refinement),
                    error: Some(rejection.message),
                });
            }
        };

        if controls.capture
            && admission.admitted
            && let Err(error) = repo
                .capture_feedback_refinement_boundary(&proposal.id)
                .await
        {
            return Json(DemandRoundResponse {
                proposal_id: Some(proposal.id),
                accepted: false,
                refinement: Some(current_refinement),
                error: Some(format!(
                    "refinement was admitted but feedback boundary capture failed: {error}"
                )),
            });
        }

        // Carry the human's note into the round that was just admitted.
        // `ProposalRepository::latest_current_revision_reviewer_feedback` — the
        // only path by which a tribunal role sees reviewer feedback — reads it
        // from a current-revision row tagged `human_demand_round`. Nothing ever
        // wrote that row, so every demanded round ran without the feedback that
        // motivated it. The row is deliberately NOT a `refinement_start`: that
        // kind doubles as the current-run boundary for debate-trail scoping.
        if let Some(reason) = p.reason.as_deref().map(str::trim).filter(|s| !s.is_empty())
            && let Err(error) = repo
                .record_refinement_lifecycle(
                    &proposal.id,
                    "refinement_demand_round",
                    Some(&serde_json::json!({
                        "source": "human_demand_round",
                        "reviewer_feedback": reason,
                        "reason": reason,
                    })),
                )
                .await
        {
            tracing::warn!(
                proposal_id = %proposal.id,
                %error,
                "failed to record demanded-round reviewer feedback; the round runs without it"
            );
        }

        let refinement = ProposalRefinementStatusModel {
            active: true,
            run_id: None,
            generation: None,
            run_state: None,
            liveness: None,
            liveness_evidence: None,
            last_heartbeat_at: None,
            current_round: Some(1),
            dry_rounds: 0,
            total_entries: 0,
            stop_reason: None,
            awaiting_review: false,
            judge_summary: None,
            snapshot_revision_seq: None,
            needs_evidence: None,
            evidence_lifecycle_state: crate::tools::proposal_ops::EvidenceLifecycleState::Active,
        };

        Json(DemandRoundResponse {
            proposal_id: Some(proposal.id),
            accepted: true,
            refinement: Some(refinement),
            error: admission
                .pending_dispatch
                .then(|| "accepted; dispatch pending".to_owned()),
        })
    }

    #[tool(
        description = "Resolve the human's single accept/reject review of a converged proposal refinement (the autonomous Adversary→Advocate→Judge tribunal parks for one human review when it converges). `decision` is `accept` (keep the refined spec) or `reject` (revert the live spec to the pre-refinement snapshot). Optional `feedback` is recorded. Errors if no refinement is parked awaiting review."
    )]
    pub async fn proposal_refinement_resolve(
        &self,
        Parameters(p): Parameters<ProposalRefinementResolveParams>,
    ) -> Json<crate::tools::proposal_ops::ResolveReviewResponse> {
        use crate::tools::proposal_ops::ResolveReviewResponse;
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());

        let Some(proposal) = repo.resolve(&p.proposal_id).await.ok().flatten() else {
            return Json(ResolveReviewResponse {
                proposal_id: None,
                resolved: false,
                error: Some(format!("proposal not found: {}", p.proposal_id)),
            });
        };

        let accept = match p.decision.as_str() {
            "accept" => true,
            "reject" => false,
            other => {
                return Json(ResolveReviewResponse {
                    proposal_id: Some(proposal.id),
                    resolved: false,
                    error: Some(format!(
                        "invalid decision: {other:?} (expected `accept` or `reject`)"
                    )),
                });
            }
        };

        match self.state.coordinator().await {
            Some(handle) => {
                if let Err(e) = handle
                    .resolve_refinement_review(proposal.id.clone(), accept, p.feedback.clone())
                    .await
                {
                    return Json(ResolveReviewResponse {
                        proposal_id: Some(proposal.id),
                        resolved: false,
                        error: Some(format!("coordinator could not resolve review: {e}")),
                    });
                }
            }
            None => {
                return Json(ResolveReviewResponse {
                    proposal_id: Some(proposal.id),
                    resolved: false,
                    error: Some("coordinator not available".to_string()),
                });
            }
        }

        Json(ResolveReviewResponse {
            proposal_id: Some(proposal.id),
            resolved: true,
            error: None,
        })
    }

    #[tool(
        description = "Stop a stalled proposal refinement handoff. Issues the canonical operator_stop tag under the exact run generation fence. Refuses healthy runs whose correlated role task is not terminal."
    )]
    pub async fn proposal_refinement_stop(
        &self,
        Parameters(p): Parameters<ProposalRefinementStopParams>,
    ) -> Json<ProposalRefinementStopResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.proposal_id).await.ok().flatten() else {
            return Json(ProposalRefinementStopResponse {
                proposal_id: None,
                stopped: false,
                stop_tag: None,
                code: Some("proposal_not_found".into()),
                error: Some(format!("proposal not found: {}", p.proposal_id)),
            });
        };
        if let Err(error) = self
            .gate_proposal_edit(proposal.author_user_id.as_deref())
            .await
        {
            return Json(ProposalRefinementStopResponse {
                proposal_id: Some(proposal.id),
                stopped: false,
                stop_tag: None,
                code: Some("forbidden".into()),
                error: Some(error),
            });
        }
        let exact = match repo
            .load_current_refinement_run_snapshot(&proposal.id, 60_000)
            .await
        {
            Ok(Some(exact)) => exact,
            Ok(None) => {
                return Json(ProposalRefinementStopResponse {
                    proposal_id: Some(proposal.id),
                    stopped: false,
                    stop_tag: None,
                    code: Some("no_refinement_run".into()),
                    error: Some("proposal has no refinement run".into()),
                });
            }
            Err(error) => {
                return Json(ProposalRefinementStopResponse {
                    proposal_id: Some(proposal.id),
                    stopped: false,
                    stop_tag: None,
                    code: Some("persistence".into()),
                    error: Some(error.to_string()),
                });
            }
        };
        let actor =
            djinn_core::auth_context::current_user_id().unwrap_or_else(|| "operator".into());
        match repo.operator_stop_stalled_refinement(&exact.snapshot.run.run_id, exact.generation, &actor, p.reason).await {
            Ok(true) => Json(ProposalRefinementStopResponse { proposal_id: Some(proposal.id), stopped: true, stop_tag: Some("operator_stop".into()), code: None, error: None }),
            Ok(false) => Json(ProposalRefinementStopResponse { proposal_id: Some(proposal.id), stopped: false, stop_tag: None, code: Some("already_stopped".into()), error: Some("refinement was already stopped".into()) }),
            Err(djinn_db::RefinementIntentMutationError::NotStalledHandoff { .. }) => Json(ProposalRefinementStopResponse { proposal_id: Some(proposal.id), stopped: false, stop_tag: None, code: Some("not_stalled_handoff".into()), error: Some("refinement role task is not terminal or its handoff already has a successor".into()) }),
            Err(error) => Json(ProposalRefinementStopResponse { proposal_id: Some(proposal.id), stopped: false, stop_tag: None, code: Some("persistence".into()), error: Some(error.to_string()) }),
        }
    }

    /// Override a latest `needs-work` verdict with auditable sign-off/approval
    /// metadata. The override is scoped to the current revision — later edits
    /// that advance the proposal revision make the override stale, preventing
    /// silent inheritance by future revisions.
    #[tool(
        description = "Override a judge needs-work verdict with auditable approval metadata. The override is scoped to the current proposal revision — later spec edits that advance the revision make it stale. Records who overrode, when, and why in proposal history."
    )]
    pub async fn proposal_verdict_override(
        &self,
        Parameters(p): Parameters<ProposalVerdictOverrideParams>,
    ) -> Json<VerdictOverrideResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());

        let Some(proposal) = repo.resolve(&p.proposal_id).await.ok().flatten() else {
            return Json(VerdictOverrideResponse {
                proposal_id: None,
                overridden: false,
                override_on_revision_seq: None,
                error: Some(format!("proposal not found: {}", p.proposal_id)),
            });
        };

        if p.reason.trim().is_empty() {
            return Json(VerdictOverrideResponse {
                proposal_id: Some(proposal.id),
                overridden: false,
                override_on_revision_seq: None,
                error: Some("override reason must not be empty".to_string()),
            });
        }

        // Record the override as a lifecycle event scoped to current revision.
        let override_seq = proposal.latest_revision_seq;
        let user_id = djinn_core::auth_context::current_user_id();
        let override_metadata = serde_json::json!({
            "source": "human_verdict_override",
            "override_by": user_id,
            "override_reason": p.reason,
            "override_on_revision_seq": override_seq,
            "overridden_verdict_entry_id": p.overridden_verdict_entry_id,
        });

        if let Err(e) = repo
            .record_refinement_lifecycle(&proposal.id, "verdict_override", Some(&override_metadata))
            .await
        {
            return Json(VerdictOverrideResponse {
                proposal_id: Some(proposal.id),
                overridden: false,
                override_on_revision_seq: None,
                error: Some(format!("failed to record verdict override: {e}")),
            });
        }

        Json(VerdictOverrideResponse {
            proposal_id: Some(proposal.id),
            overridden: true,
            override_on_revision_seq: Some(override_seq),
            error: None,
        })
    }

    /// Demand a read-only evidence spike for an insufficiently-evidenced
    /// feasibility claim. The Judge calls this when in-session research is
    /// not enough to resolve a load-bearing claim in the spec.
    ///
    /// Validates the caller is the active Judge, the proposal is not terminal,
    /// the refinement round/revision match, the question is falsifiable, the
    /// spec anchor exists, and the per-run needs-evidence cap is not exhausted.
    /// On acceptance: creates a single read-only evidence spike task (Architect
    /// routing), writes a `needs_evidence` debate-trail entry with structured
    /// `NeedsEvidenceClaimLink` metadata, links the spike to the proposal via
    /// `set_structured_needs_evidence_spike`, parks the proposal, and writes a
    /// `refinement_awaiting_evidence_started` lifecycle event.
    ///
    /// Race protection: concurrent valid demands cannot produce two open
    /// spikes because `set_structured_needs_evidence_spike` atomically sets
    /// `linked_spike_task_id` only when the column is NULL. The loser's
    /// spike task is closed, and a conflict response is returned.
    #[tool(
        description = "Demand a read-only evidence spike for an insufficiently-evidenced feasibility claim. The Judge calls this when in-session research cannot resolve a load-bearing claim. Validates the proposal and refinement state, checks the per-run needs-evidence cap, records the structured claim, creates a linked read-only spike task for the Architect, writes a needs_evidence debate entry, and parks refinement until spike findings arrive."
    )]
    pub async fn proposal_refinement_demand_evidence(
        &self,
        Parameters(p): Parameters<ProposalRefinementDemandEvidenceParams>,
    ) -> Json<NeedsEvidenceDemandResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());

        let Some(proposal) = repo.resolve(&p.proposal_id).await.ok().flatten() else {
            return Json(err_demand_evidence(format!(
                "proposal not found: {}",
                p.proposal_id
            )));
        };

        // Build refinement status for validation.
        let refinement = match build_refinement_status(&repo, &proposal.id).await {
            Ok(status) => status,
            Err(e) => {
                return Json(NeedsEvidenceDemandResponse {
                    proposal_id: Some(proposal.id),
                    accepted: false,
                    result: None,
                    error: Some(e),
                    conflict_code: None,
                });
            }
        };

        // Run the full validation gate before any mutation. This ensures
        // no-mutation-on-reject: no lifecycle events, no debate entries, no
        // proposal fields are touched when the demand is invalid.
        // Returns the Judge task id on success.
        let judge_task_id =
            match validate_demand_evidence(&repo, &task_repo, &proposal, &refinement, &p).await {
                Ok(id) => {
                    djinn_telemetry::evidence_metrics::record(
                        djinn_telemetry::evidence_metrics::EvidenceStage::Demand,
                        djinn_telemetry::evidence_metrics::EvidenceOutcome::Accepted,
                    );
                    id
                }
                Err(e) => {
                    djinn_telemetry::evidence_metrics::reject(
                        djinn_telemetry::evidence_metrics::EvidenceRejection::Validation,
                    );
                    return Json(NeedsEvidenceDemandResponse {
                        proposal_id: Some(proposal.id),
                        accepted: false,
                        result: None,
                        error: Some(e),
                        conflict_code: None,
                    });
                }
            };

        // Build the structured claim for persistence. The claim JSON is stored
        // on the proposal alongside the spike task id.
        let claim = NeedsEvidenceClaim {
            question: p.question.clone(),
            target_subsystem: p.target_subsystem.clone(),
            spec_unknown_anchor: p.spec_unknown_anchor.clone(),
            insufficient_in_session_research: p.insufficient_in_session_research.clone(),
            expected_findings: p.expected_findings.clone(),
            round: p.round,
            against_revision_seq: p.against_revision_seq,
            created_by_task_id: judge_task_id.clone(),
        };

        // ── Step 1: Resolve project_id for the spike task ────────────────
        let project_id = match repo.targets(&proposal.id).await {
            Ok(targets) if !targets.is_empty() => targets[0].project_id.clone(),
            _ => {
                return Json(NeedsEvidenceDemandResponse {
                    proposal_id: Some(proposal.id),
                    accepted: false,
                    result: None,
                    error: Some(
                        "proposal has no target project; cannot create evidence spike task"
                            .to_string(),
                    ),
                    conflict_code: None,
                });
            }
        };

        // ── Step 2: Create the evidence spike task ───────────────────────
        let spike_title = format!("Evidence spike: {}", p.question.trim());
        let spike_description = format!(
            "## Evidence Spike\n\n\
             **Proposal:** {proposal_id} (short_id: {short_id})\n\
             **Question:** {question}\n\
             **Target subsystem:** {target_subsystem}\n\
             **Spec unknown anchor:** {spec_unknown_anchor}\n\
             **Insufficiency rationale:** {insufficiency}\n\
             **Expected findings:** {expected_findings}\n\n\
             ### Read-Only Constraints\n\n\
             This is a **read-only** evidence investigation. The spike must:\n\
             - Only read and analyze existing code, docs, and specs.\n\
             - NOT modify, create, or delete any production files.\n\
             - Produce structured findings as evidence for the Judge.\n\
             - Return findings via the evidence_findings debate-trail entry.",
            proposal_id = proposal.id,
            short_id = proposal.short_id,
            question = p.question.trim(),
            target_subsystem = p.target_subsystem.trim(),
            spec_unknown_anchor = p.spec_unknown_anchor.trim(),
            insufficiency = p.insufficient_in_session_research.trim(),
            expected_findings = p.expected_findings.trim(),
        );

        let labels = vec![
            "refinement-evidence".to_string(),
            "read-only".to_string(),
            format!("proposal:{}", proposal.short_id),
        ];

        let labels = serde_json::json!(labels);
        let allocated = match repo
            .demand_evidence_atomically(djinn_db::AtomicEvidenceDemandInput {
                proposal_id: &proposal.id,
                project_id: &project_id,
                claim: &claim,
                title: &spike_title,
                description: &spike_description,
                labels: &labels,
                load_bearing_category: &p.load_bearing_category,
            })
            .await
        {
            Ok(value) => value,
            Err(e) if e.to_string().contains("active_evidence_conflict") => {
                return Json(NeedsEvidenceDemandResponse {
                    proposal_id: Some(proposal.id),
                    accepted: false,
                    result: None,
                    error: Some("active_evidence_conflict".into()),
                    conflict_code: Some("active_evidence_conflict".into()),
                });
            }
            Err(e) => {
                return Json(err_demand_evidence(format!(
                    "failed to allocate evidence demand atomically: {e}"
                )));
            }
        };

        Json(NeedsEvidenceDemandResponse {
            proposal_id: Some(proposal.id),
            accepted: true,
            result: Some(NeedsEvidenceDemandResult {
                claim: claim.question.clone(),
                spike_task_id: Some(allocated.spike_task_id),
                against_revision_seq: p.against_revision_seq,
                round: p.round,
                finding_id: Some(allocated.finding_id),
                attempt_id: Some(allocated.attempt_id),
                lifecycle: Some("spike_active".into()),
                replayed: Some(allocated.replayed),
            }),
            error: None,
            conflict_code: None,
        })
    }
}

#[cfg(test)]
mod tests;
