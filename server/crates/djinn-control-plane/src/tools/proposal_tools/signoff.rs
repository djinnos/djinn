// Signoff tools and readiness/composed-gate helpers for proposals.
//
// This submodule owns:
// - `proposal_signoff` and `proposal_signoff_clear` MCP tool methods;
// - deterministic DoR readiness helpers (`parse_ac_items`, `format_readiness_error`);
// - composed-gate logic (`evaluate_composed_gate`, `ComposedGateResult`,
//   `build_gate_status`) that checks DoR + tribunal conditions before
//   sign-off, graduation, and review promotion;
// - human-authority helpers (`current_explicit_verdict_override`,
//   `revision_metadata_is_human_accept`, `current_human_accept_authority`,
//   `current_human_gate_authority`).
//
// Debate-trail support (unresolved blocking entries, judge verdicts,
// needs-evidence spikes) is embedded in the composed-gate helpers rather than
// a standalone `debate.rs` module because the debate checks are not cohesive
// without the surrounding gate evaluation context — they are evaluated as
// part of a single pass over DoR + tribunal conditions.
//
// Refinement-loop ownership: `build_gate_status` calls
// `crate::tools::refinement_tools::build_refinement_status` to populate the
// adversary dry count — a thin call-through, not standalone refinement glue.
// The human-authority helpers check for `refinement_stop` lifecycle events
// (`current_human_accept_authority`), which is a signoff-readiness concern.
// Neither warrants a separate `refinement.rs` module.

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::Deserialize;

use crate::server::DjinnMcpServer;
use crate::tools::acting_user::acting_caps;
use crate::tools::epic_ops::AcceptanceCriterionItem;
use crate::tools::proposal_ops::{ProposalModel, ProposalSingleResponse};
use crate::tools::proposal_readiness::evaluate_proposal_readiness;
use djinn_db::ProposalRepository;

use super::{err_single, proposal_not_found_error};

// ── Param structs ───────────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalSignoffParams {
    /// Proposal UUID or short_id.
    pub id: String,
    /// `scoped` (product) or `technical` (engineering).
    pub kind: String,
}

// ── Readiness gate helpers (shared with `create.rs` and `mod.rs`) ───────────

/// Parse a stored acceptance-criteria JSON string into `AcceptanceCriterionItem`
/// values for the readiness evaluator.  Returns an empty vec when the JSON is
/// missing, empty, or unparseable.
pub(crate) fn parse_ac_items(ac_json: &str) -> Vec<AcceptanceCriterionItem> {
    serde_json::from_str::<Vec<AcceptanceCriterionItem>>(ac_json).unwrap_or_default()
}

/// Convert a `ProposalReadinessResult` into a user-facing error string,
/// prepending a short preamble so callers can return it directly as a tool
/// error.
pub(crate) fn format_readiness_error(
    result: &crate::tools::proposal_readiness::ProposalReadinessResult,
) -> Option<String> {
    result
        .to_error_string()
        .map(|details| format!("proposal not ready for review: {details}"))
}

// ── Composed gate: DoR + tribunal (task cuzf) ─────────────────────────────

/// Result of the composed tribunal gate check.
pub(crate) struct ComposedGateResult {
    failures: Vec<String>,
}

impl ComposedGateResult {
    pub(crate) fn to_error_string(&self) -> Option<String> {
        if self.failures.is_empty() {
            return None;
        }
        Some(format!(
            "proposal not ready for review: {}",
            self.failures.join("; ")
        ))
    }
}

async fn current_explicit_verdict_override(
    repo: &ProposalRepository,
    proposal: &djinn_core::models::proposal::Proposal,
) -> bool {
    match repo.latest_verdict_override(&proposal.id).await {
        Ok(Some((override_on_seq, _))) => override_on_seq == proposal.latest_revision_seq,
        _ => false,
    }
}

fn revision_metadata_is_human_accept(event_metadata: Option<&str>) -> bool {
    let Some(event_metadata) = event_metadata else {
        return false;
    };
    let Ok(meta) = serde_json::from_str::<serde_json::Value>(event_metadata) else {
        return false;
    };

    meta.get("reason_tag")
        .or_else(|| meta.get("stop_reason"))
        .and_then(|v| v.as_str())
        == Some("human_accepted")
}

async fn current_human_accept_authority(
    repo: &ProposalRepository,
    proposal: &djinn_core::models::proposal::Proposal,
) -> bool {
    match repo.revisions(&proposal.id).await {
        Ok(revisions) => revisions
            .iter()
            .rev()
            .find(|revision| {
                revision.event_kind == "refinement_stop"
                    && revision.seq == proposal.latest_revision_seq
            })
            .is_some_and(|revision| {
                revision_metadata_is_human_accept(revision.event_metadata.as_deref())
            }),
        Err(_) => false,
    }
}

async fn current_human_gate_authority(
    repo: &ProposalRepository,
    proposal: &djinn_core::models::proposal::Proposal,
) -> bool {
    current_explicit_verdict_override(repo, proposal).await
        || current_human_accept_authority(repo, proposal).await
}

/// Run the composed gate: deterministic DoR check + tribunal conditions.
///
/// Returns `ComposedGateResult` with all failures collected. Callers
/// (proposal_create, proposal_update, proposal_signoff, proposal_graduate)
/// convert failures into tool error responses.
pub(crate) async fn evaluate_composed_gate(
    repo: &ProposalRepository,
    proposal: &djinn_core::models::proposal::Proposal,
    body: &str,
    ac_json: &str,
    target_count: usize,
) -> ComposedGateResult {
    let mut failures: Vec<String> = Vec::new();

    // 1. Deterministic DoR check (existing evaluator, reused not duplicated).
    let ac_items = parse_ac_items(ac_json);
    let readiness = evaluate_proposal_readiness(body, &ac_items, target_count);
    let readiness_error = format_readiness_error(&readiness);

    // Consult current human authority before deciding whether deterministic
    // readiness failures block. A current explicit override or human acceptance
    // is scoped to the latest revision only; stale lifecycle rows do not apply.
    let human_authority_is_current = current_human_gate_authority(repo, proposal).await;
    if let Some(err) = readiness_error
        && !human_authority_is_current
    {
        failures.push(err);
        // Preserve the historical no-authority DoR blocking behavior: DoR-only
        // failures stop the gate before tribunal diagnostics are appended.
        return ComposedGateResult { failures };
    }

    // 2. Tribunal conditions.
    let proposal_id = &proposal.id;

    // 2a. Check for a current explicit human override first — it gates whether
    // judge-verdict blocking entries and needs-work verdicts are enforced.
    let override_is_current = current_explicit_verdict_override(repo, proposal).await;

    // 2b. Unresolved blocking debate-trail entries.
    // Judge verdict rows are excluded at the query level — verdicts gate solely
    // through the latest-verdict channel below (2c), so they must not also count
    // as unresolved blocking rows (a stale reject verdict superseded by a later
    // approve verdict has nothing that resolves it, and would block forever).
    match repo
        .list_unresolved_blocking_debate_entries(proposal_id)
        .await
    {
        Ok(entries) => {
            if !entries.is_empty() {
                let ids: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
                failures.push(format!(
                    "unresolved blocking debate entries: {}",
                    ids.join(", ")
                ));
            }
        }
        Err(e) => {
            failures.push(format!("failed to check debate trail: {e}"));
        }
    }

    // 2c. Latest judge verdict.
    match repo.latest_judge_verdict(proposal_id).await {
        Ok(Some(verdict)) => {
            let verdict_lower = verdict.body.to_lowercase();
            // A "needs-work" verdict blocks unless overridden.
            if (verdict_lower.contains("needs-work")
                || verdict_lower.contains("needs_work")
                || verdict_lower.contains("needs work"))
                && !override_is_current
            {
                failures.push(format!(
                    "judge returned needs-work (verdict {}); no current human override",
                    verdict.id
                ));
            }
        }
        Err(e) => {
            failures.push(format!("failed to check judge verdict: {e}"));
        }
        _ => {}
    }

    // 2d. Needs-evidence spike parking.
    match repo.has_open_needs_evidence_spike(proposal_id).await {
        Ok(true) => {
            let claim = proposal
                .needs_evidence_claim
                .as_deref()
                .unwrap_or("unspecified");
            let spike_id = proposal
                .linked_spike_task_id
                .as_deref()
                .unwrap_or("unknown");
            failures.push(format!(
                "proposal parked on needs-evidence spike {spike_id} (claim: {claim})"
            ));
        }
        Err(e) => {
            failures.push(format!("failed to check needs-evidence spike: {e}"));
        }
        _ => {}
    }

    ComposedGateResult { failures }
}

/// Build a structured [`ProposalGateStatusModel`] for `proposal_show`.
///
/// Collects DoR failures, tribunal conditions, and human-readable explanations
/// so the UI can render readiness without recomputing it client-side.
pub(crate) async fn build_gate_status(
    repo: &ProposalRepository,
    proposal: &djinn_core::models::proposal::Proposal,
    body: &str,
    ac_json: &str,
    target_count: usize,
) -> crate::tools::proposal_ops::ProposalGateStatusModel {
    use crate::tools::proposal_ops::{GateFailureModel, ProposalGateStatusModel};

    // 1. Deterministic DoR
    let ac_items = parse_ac_items(ac_json);
    let readiness = evaluate_proposal_readiness(body, &ac_items, target_count);
    let dor_failures: Vec<GateFailureModel> = readiness
        .failures
        .iter()
        .map(|f| {
            let message = match &f.detail {
                crate::tools::proposal_readiness::ReadinessFailureDetail::MissingSection {
                    check_name,
                } => format!("Missing required coverage: {check_name}"),
                crate::tools::proposal_readiness::ReadinessFailureDetail::Generic { message } => {
                    message.clone()
                }
            };
            GateFailureModel {
                check: format!("{:?}", f.check).to_snake_case(),
                message,
            }
        })
        .collect();
    let dor_ready = readiness.ready;

    // 2. Tribunal conditions
    let proposal_id = &proposal.id;
    let mut blocked_explanations: Vec<String> = Vec::new();

    // 2a. Human authority checks. Current human gate authority (either an
    // explicit verdict override or a human-accepted refinement stop on the
    // latest revision) suppresses deterministic DoR false-positive blocks for
    // proposal_show, while only an explicit current verdict override suppresses
    // judge verdict blocking semantics below.
    let human_authority_is_current = current_human_gate_authority(repo, proposal).await;
    let override_is_current = current_explicit_verdict_override(repo, proposal).await;

    // Add DoR failures to explanations only when they are actually blocking.
    // Keep `dor_failures` populated either way so clients can show diagnostics.
    if !dor_ready && !human_authority_is_current {
        for f in &dor_failures {
            blocked_explanations.push(f.message.clone());
        }
    }

    // 2b. Unresolved blocking debate entries. Judge verdict rows are excluded at
    // the query level — verdicts gate solely through the latest-verdict channel
    // below (2c), never as unresolved blocking rows.
    let (unresolved_blocking_ids, unresolved_count) = match repo
        .list_unresolved_blocking_debate_entries(proposal_id)
        .await
    {
        Ok(entries) => {
            if !entries.is_empty() {
                let ids: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
                blocked_explanations.push(format!(
                    "Unresolved blocking debate entries: {}",
                    ids.join(", ")
                ));
                let count = ids.len() as i32;
                (ids, count)
            } else {
                (vec![], 0)
            }
        }
        Err(e) => {
            blocked_explanations.push(format!("Failed to check debate trail: {e}"));
            (vec![], 0)
        }
    };

    // 2c. Latest judge verdict
    let (judge_verdict_body, judge_verdict_id, judge_needs_work) =
        match repo.latest_judge_verdict(proposal_id).await {
            Ok(Some(verdict)) => {
                let verdict_lower = verdict.body.to_lowercase();
                let needs_work = (verdict_lower.contains("needs-work")
                    || verdict_lower.contains("needs_work")
                    || verdict_lower.contains("needs work"))
                    && !override_is_current;
                if needs_work {
                    blocked_explanations.push(format!(
                        "Judge returned needs-work (verdict {}); no current human override",
                        verdict.id
                    ));
                }
                (
                    Some(verdict.body.clone()),
                    Some(verdict.id.clone()),
                    needs_work,
                )
            }
            Err(e) => {
                blocked_explanations.push(format!("Failed to check judge verdict: {e}"));
                (None, None, false)
            }
            _ => (None, None, false),
        };

    // 2d. Needs-evidence spike parking
    let needs_evidence = match repo.has_open_needs_evidence_spike(proposal_id).await {
        Ok(true) => {
            let claim = proposal
                .needs_evidence_claim
                .as_deref()
                .unwrap_or("unspecified");
            let spike_id = proposal
                .linked_spike_task_id
                .as_deref()
                .unwrap_or("unknown");
            blocked_explanations.push(format!(
                "Proposal parked on needs-evidence spike {spike_id} (claim: {claim})"
            ));
            Some(crate::tools::proposal_ops::NeedsEvidenceStatus {
                claim: claim.to_string(),
                spike_task_id: spike_id.to_string(),
                spike_short_id: spike_id.to_string(),
                spike_status: "open".to_string(),
                question: None,
                target_subsystem: None,
                spec_unknown_anchor: None,
                round: None,
                against_revision_seq: None,
                created_by_task_id: None,
                insufficient_in_session_research: None,
                expected_findings: None,
                evidence_phase: None,
                failure_reason: None,
            })
        }
        Err(e) => {
            blocked_explanations.push(format!("Failed to check needs-evidence spike: {e}"));
            None
        }
        _ => None,
    };

    // Adversary dry count from refinement status (non-critical)
    let adversary_dry_count =
        match crate::tools::refinement_tools::build_refinement_status(repo, proposal_id).await {
            Ok(status) => status.dry_rounds,
            _ => 0,
        };

    let ready = blocked_explanations.is_empty();

    ProposalGateStatusModel {
        ready,
        dor_ready,
        dor_failures,
        judge_verdict_body,
        judge_verdict_id,
        judge_needs_work,
        adversary_dry_count,
        unresolved_blocking_count: unresolved_count,
        unresolved_blocking_ids,
        needs_evidence,
        human_override_active: human_authority_is_current,
        blocked_explanations,
    }
}

// ── Snake-case helper for gate-status model ─────────────────────────────────

/// Helper: convert CamelCase/PascalCase enum variant to snake_case.
pub(crate) trait ToSnakeCase {
    fn to_snake_case(&self) -> String;
}

impl ToSnakeCase for String {
    fn to_snake_case(&self) -> String {
        let mut out = String::with_capacity(self.len() + 4);
        for (i, ch) in self.chars().enumerate() {
            if ch.is_uppercase() {
                if i > 0 {
                    out.push('_');
                }
                out.extend(ch.to_lowercase());
            } else {
                out.push(ch);
            }
        }
        out
    }
}

// ── Tool router ─────────────────────────────────────────────────────────────

#[tool_router(router = proposal_signoff_tool_router, vis = "pub(super)")]
impl DjinnMcpServer {
    /// Give a review sign-off (scoped or technical) as the authenticated user.
    #[tool(
        description = "Record a review sign-off on a proposal as the authenticated user. `kind` is `scoped` (product/scope) or `technical` (engineering). The sign-off anchors to the current head revision; later spec edits mark it stale. When a proposal in `in_review` has both a fresh scoped and technical sign-off, it auto-advances to `approved`."
    )]
    pub async fn proposal_signoff(
        &self,
        Parameters(p): Parameters<ProposalSignoffParams>,
    ) -> Json<ProposalSingleResponse> {
        if !matches!(p.kind.as_str(), "scoped" | "technical") {
            return Json(err_single(format!(
                "invalid sign-off kind: {:?} (expected scoped or technical)",
                p.kind
            )));
        }
        let Some(user_id) = djinn_core::auth_context::current_user_id() else {
            return Json(err_single(
                "sign-off requires an authenticated user".to_string(),
            ));
        };
        // Role gate: scoped = PM/engineer/admin; technical = engineer/admin.
        match acting_caps(self.state.db()).await {
            Ok(Some(caps)) if !caps.can_signoff(&p.kind) => {
                return Json(err_single(format!(
                    "a {} sign-off requires the {} role",
                    p.kind,
                    if p.kind == "technical" {
                        "engineer (or admin)"
                    } else {
                        "PM, engineer (or admin)"
                    }
                )));
            }
            Err(e) => return Json(err_single(e)),
            _ => {}
        }
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_single(proposal_not_found_error(&p.id)));
        };

        // Composed gate (task cuzf): block sign-off on draft/in_review
        // proposals when DoR or tribunal conditions are not met.  This
        // prevents sign-off-driven auto-advance from bypassing readiness
        // or tribunal checks.
        if matches!(proposal.status.as_str(), "draft" | "in_review") {
            let target_count = repo
                .targets(&proposal.id)
                .await
                .map(|t| t.len())
                .unwrap_or(0);
            let gate = evaluate_composed_gate(
                &repo,
                &proposal,
                &proposal.body,
                &proposal.acceptance_criteria,
                target_count,
            )
            .await;
            if let Some(err) = gate.to_error_string() {
                return Json(err_single(err));
            }
        }

        match repo.add_signoff(&proposal.id, &p.kind, &user_id).await {
            Ok(updated) => Json(ProposalSingleResponse {
                proposal: Some(ProposalModel::from(&updated)),
                mdx: None,
                error: None,
                code: None,
                violations: None,
                latest_lint: None,
            }),
            Err(e) => Json(err_single(e.to_string())),
        }
    }

    /// Withdraw the authenticated user's sign-off of a given kind.
    #[tool(
        description = "Withdraw the authenticated user's sign-off (`scoped` or `technical`) from a proposal. May demote an `approved` proposal back to `in_review` if the approval gate is no longer met."
    )]
    pub async fn proposal_signoff_clear(
        &self,
        Parameters(p): Parameters<ProposalSignoffParams>,
    ) -> Json<ProposalSingleResponse> {
        let Some(user_id) = djinn_core::auth_context::current_user_id() else {
            return Json(err_single(
                "sign-off requires an authenticated user".to_string(),
            ));
        };
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_single(proposal_not_found_error(&p.id)));
        };
        match repo.clear_signoff(&proposal.id, &p.kind, &user_id).await {
            Ok(updated) => Json(ProposalSingleResponse {
                proposal: Some(ProposalModel::from(&updated)),
                mdx: None,
                error: None,
                code: None,
                violations: None,
                latest_lint: None,
            }),
            Err(e) => Json(err_single(e.to_string())),
        }
    }
}

#[cfg(test)]
mod signoff_tests {
    include!("signoff_tests.rs");
}

// Tribunal / P4 regression tests — extracted to `tribunal_tests.rs` to meet
// the 1500-line file-size guard. The debate-trail gate logic lives in this
// module (see `evaluate_composed_gate`), so tribunal tests are paired with
// the signoff concern.
#[cfg(test)]
include!("tribunal_tests.rs");
