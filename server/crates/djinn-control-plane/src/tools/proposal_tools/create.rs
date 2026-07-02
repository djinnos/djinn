// Create/read/import/export/list CRUD tools for the global Proposals layer.

use std::borrow::Cow;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::server::DjinnMcpServer;
use crate::tools::acting_user::acting_caps;
use crate::tools::epic_ops::AcceptanceCriterionItem;
use crate::tools::list_response::{
    self, ListMeta, NamedListResponse, named_list_response_schema, serialize_named_list_response,
};
use crate::tools::proposal_blocks::{
    parse_mdx_blocks, validate_mdx_blocks, validate_question_form_placement,
};
use crate::tools::proposal_ops::{
    ProposalDebateTrailModel, ProposalListSummary, ProposalModel, ProposalShowResponse,
    ProposalSignoffModel, ProposalSingleResponse, ProposalTargetModel,
};
use crate::tools::proposal_readiness::evaluate_proposal_readiness;
use crate::tools::proposal_tools::{
    err_show, err_single, graduated_epic_models, ToSnakeCase,
};
use crate::tools::validation::{
    validate_ac_count, validate_design, validate_limit, validate_mdx_body, validate_offset,
    validate_proposal_create_status, validate_sort, validate_title,
};
use djinn_db::{ProjectRepository, ProposalListQuery, ProposalListSummaryRow, ProposalRepository};

use crate::tools::proposal_tools::mdx::{parse_proposal_mdx, split_proposal_mdx_frontmatter};

// ── List response (NamedListResponse boilerplate, mirrors EpicListResponse) ──

#[derive(Clone)]
pub struct ProposalListResponse {
    pub proposals: Option<Vec<ProposalModel>>,
    pub meta: ListMeta,
}

impl NamedListResponse for ProposalListResponse {
    type Item = ProposalModel;
    const FIELD_NAME: &'static str = "proposals";
    const TITLE: &'static str = "ProposalListResponse";

    fn from_parts(items: Option<Vec<Self::Item>>, meta: ListMeta) -> Self {
        Self {
            proposals: items,
            meta,
        }
    }
    fn items(&self) -> Option<&Vec<Self::Item>> {
        self.proposals.as_ref()
    }
    fn meta(&self) -> &ListMeta {
        &self.meta
    }
}

impl Serialize for ProposalListResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serialize_named_list_response(self, serializer)
    }
}

impl schemars::JsonSchema for ProposalListResponse {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed(Self::TITLE)
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        named_list_response_schema::<ProposalModel>(generator, Self::TITLE, Self::FIELD_NAME)
    }
}

pub(crate) fn proposal_not_found_error(id: &str) -> String {
    format!("proposal not found: {id}")
}

/// List a proposal's targets and resolve each project id to an `owner/repo`
/// slug + name for display chips.
pub(crate) async fn target_models(
    proposal_repo: &ProposalRepository,
    project_repo: &ProjectRepository,
    proposal_id: &str,
) -> Result<Vec<ProposalTargetModel>, String> {
    let targets = proposal_repo
        .targets(proposal_id)
        .await
        .map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(targets.len());
    for t in &targets {
        let mut m = ProposalTargetModel::from(t);
        if let Ok(Some(p)) = project_repo.get(&t.project_id).await {
            m.project_path = Some(format!("{}/{}", p.github_owner, p.github_repo));
            m.project_name = Some(p.name);
        }
        out.push(m);
    }
    Ok(out)
}

// ── Param structs ────────────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalCreateParams {
    pub title: String,
    /// Spec body (markdown or MDX depending on `body_format`).
    pub body: Option<String>,
    /// Acceptance criteria: plain strings or `{criterion, met}` objects.
    pub acceptance_criteria: Option<Vec<AcceptanceCriterionItem>>,
    /// Target projects (UUIDs or owner/repo slugs) this proposal touches.
    /// Editable later via proposal_add_target / proposal_remove_target.
    pub target_projects: Option<Vec<String>>,
    /// Initial status: `triage`, `draft` (default), or `in_review`. Proposer-
    /// role authors are always placed in `triage` regardless of this value.
    pub status: Option<String>,
    /// Body encoding: `markdown` (default) or `mdx` (block-aware).
    pub body_format: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalImportParams {
    /// Full portable proposal.mdx content, including optional YAML frontmatter.
    pub mdx: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalExportParams {
    /// Proposal UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalShowParams {
    /// Proposal UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalListParams {
    pub status: Option<String>,
    /// Filter by author user id.
    pub author: Option<String>,
    /// Filter to proposals targeting this project (UUID or owner/repo slug).
    pub target_project: Option<String>,
    /// Full-text search on title and body.
    pub text: Option<String>,
    /// Sort order: "created_desc" (default), "created", "updated", "updated_desc".
    pub sort: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

// ── Readiness gate helpers ──────────────────────────────────────────────────

/// Parse a stored acceptance-criteria JSON string into `AcceptanceCriterionItem`
/// values for the readiness evaluator.  Returns an empty vec when the JSON is
/// missing, empty, or unparseable.
pub(crate) fn parse_ac_items(ac_json: &str) -> Vec<AcceptanceCriterionItem> {
    serde_json::from_str::<Vec<AcceptanceCriterionItem>>(ac_json).unwrap_or_default()
}

/// Whether a proposal status is non-terminal (still moving through scoping /
/// review). Only these get a batched list summary — terminal proposals never
/// show tribunal/gate chips. Mirrors the statuses the composed gate cares about
/// (`draft` and `in_review`).
pub(crate) fn proposal_status_is_non_terminal(status: &str) -> bool {
    matches!(status, "draft" | "in_review")
}

/// Heuristic for a needs-work judge verdict — kept identical to the one in
/// `build_gate_status` so the list and the detail gate agree.
pub(crate) fn judge_verdict_is_needs_work(verdict_body: &str) -> bool {
    let lower = verdict_body.to_lowercase();
    lower.contains("needs-work") || lower.contains("needs_work") || lower.contains("needs work")
}

/// Compose the list-row tribunal/readiness summary from a proposal row plus its
/// batched raw facts. Deterministic and query-free (all data is already loaded):
/// runs the in-memory DoR evaluator and approximates the composed gate.
pub(crate) fn build_list_summary(
    proposal: &djinn_core::models::proposal::Proposal,
    raw: &ProposalListSummaryRow,
) -> ProposalListSummary {
    let ac_items = parse_ac_items(&proposal.acceptance_criteria);
    let dor_ready =
        evaluate_proposal_readiness(&proposal.body, &ac_items, raw.target_count as usize).ready;

    let needs_evidence = proposal.linked_spike_task_id.is_some();
    let judge_needs_work = raw
        .latest_judge_verdict_body
        .as_deref()
        .map(judge_verdict_is_needs_work)
        .unwrap_or(false);

    // Composed-gate approximation (no override lifecycle handling — see
    // `ProposalListSummary::gate_ready` docs; the authoritative check with
    // human-override suppression lives in `build_gate_status`).
    let gate_ready =
        dor_ready && !judge_needs_work && raw.unresolved_blocking_count == 0 && !needs_evidence;

    ProposalListSummary {
        refinement_active: raw.refinement_active,
        awaiting_review: raw.awaiting_review,
        current_round: raw.current_round,
        needs_evidence,
        dor_ready,
        gate_ready,
        unresolved_blocking_count: raw.unresolved_blocking_count,
    }
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
struct ComposedGateResult {
    failures: Vec<String>,
}

impl ComposedGateResult {
    fn to_error_string(&self) -> Option<String> {
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

// ── Tool router continuation ─────────────────────────────────────────────────

impl DjinnMcpServer {
    /// Create a global proposal.
    #[tool(
        description = "Create a global, project-independent proposal (collaborative spec). Optionally pass `target_projects` (UUIDs or owner/repo slugs) the proposal touches and `acceptance_criteria`. The author is the authenticated caller. Status defaults to `draft`."
    )]
    pub async fn proposal_create(
        &self,
        Parameters(p): Parameters<ProposalCreateParams>,
    ) -> Json<ProposalSingleResponse> {
        let title = match validate_title(&p.title) {
            Ok(t) => t,
            Err(e) => return Json(err_single(e)),
        };
        let body = p.body.as_deref().unwrap_or("");
        if let Err(e) = validate_design(body) {
            return Json(err_single(e));
        }
        if let Err(e) = validate_mdx_body(body, p.body_format.as_deref()) {
            return Json(err_single(e));
        }
        let body_format = p.body_format.as_deref().unwrap_or("markdown");
        if body_format == "mdx"
            && let Err(e) = validate_question_form_placement(body)
        {
            return Json(err_single(e));
        }
        let ac = p.acceptance_criteria.unwrap_or_default();
        if let Err(e) = validate_ac_count(ac.len()) {
            return Json(err_single(e));
        }
        let requested_status = match validate_proposal_create_status(p.status.as_deref()) {
            Ok(s) => s,
            Err(e) => return Json(err_single(e)),
        };
        // Proposer-role authors are forced into `triage` (an inbox a PM/engineer
        // promotes to `draft`); higher roles default to their requested status
        // (or `draft`).
        let status = match acting_caps(self.state.db()).await {
            Ok(Some(caps)) if !caps.is_admin && caps.role == "proposer" => Some("triage"),
            Ok(_) => requested_status,
            Err(e) => return Json(err_single(e)),
        };
        let ac_json = serde_json::to_string(&ac).unwrap_or_else(|_| "[]".to_string());

        // Pre-resolve target projects: used both for the readiness gate
        // (target count) and for seeding after proposal creation.
        let mut resolved_target_ids: Vec<String> = Vec::new();
        if let Some(targets) = &p.target_projects {
            let project_repo =
                ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
            for t in targets {
                if let Ok(Some(pid)) = project_repo.resolve(t).await {
                    resolved_target_ids.push(pid);
                }
            }
        }

        // Deterministic DoR gate: block entering `in_review` when the spec is
        // not ready. Existing body/MDX/AC-count validation already passed.
        let effective_status = status.unwrap_or("draft");
        if effective_status == "in_review" {
            let ac_items = parse_ac_items(&ac_json);
            let readiness = evaluate_proposal_readiness(body, &ac_items, resolved_target_ids.len());
            if let Some(err) = format_readiness_error(&readiness) {
                return Json(err_single(err));
            }
        }

        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let proposal = match repo
            .create(djinn_db::ProposalCreateInput {
                title: &title,
                body,
                acceptance_criteria: Some(&ac_json),
                status,
                body_format: p.body_format.as_deref(),
            })
            .await
        {
            Ok(p) => p,
            Err(e) => return Json(err_single(e.to_string())),
        };

        // Seed target projects (best-effort: unresolvable refs are skipped).
        for pid in &resolved_target_ids {
            let _ = repo.add_target(&proposal.id, pid, "primary").await;
        }

        // Composed gate (task cuzf): after seeding targets, verify the full
        // composed gate (DoR + tribunal) for `in_review` proposals. If
        // tribunal conditions block the transition, downgrade to `draft`
        // instead of blocking creation.
        let proposal = if effective_status == "in_review" {
            let gate =
                evaluate_composed_gate(&repo, &proposal, body, &ac_json, resolved_target_ids.len())
                    .await;
            if let Some(_err) = gate.to_error_string() {
                // Tribunal blocked — downgrade to draft.
                match repo
                    .update(
                        &proposal.id,
                        djinn_db::ProposalUpdateInput {
                            title: &proposal.title,
                            body: &proposal.body,
                            acceptance_criteria: &proposal.acceptance_criteria,
                            status: "draft",
                            superseded_by: None,
                            body_format: Some(&proposal.body_format),
                            event_metadata: None,
                        },
                    )
                    .await
                {
                    Ok(downgraded) => downgraded,
                    Err(_) => proposal,
                }
            } else {
                proposal
            }
        } else {
            proposal
        };

        Json(ProposalSingleResponse {
            proposal: Some(ProposalModel::from(&proposal)),
            mdx: None,
            error: None,
        })
    }

    /// Import a portable proposal.mdx document, creating or updating a proposal.
    #[tool(
        description = "Import a portable proposal.mdx document. Parses YAML frontmatter (optional id, title, body_format, acceptance_criteria), validates MDX custom block tags against the proposal block registry, then creates a new proposal or updates the existing proposal named by id."
    )]
    pub async fn proposal_import(
        &self,
        Parameters(p): Parameters<ProposalImportParams>,
    ) -> Json<ProposalSingleResponse> {
        let imported = match parse_proposal_mdx(&p.mdx) {
            Ok(imported) => imported,
            Err(e) => return Json(err_single(e)),
        };

        if let Err(e) = validate_design(imported.body) {
            return Json(err_single(e));
        }
        if imported.body_format == "mdx"
            && let Err(e) = validate_mdx_blocks(imported.body)
        {
            return Json(err_single(e.to_string()));
        }
        if imported.body_format == "mdx"
            && let Err(e) = parse_mdx_blocks(imported.body)
        {
            return Json(err_single(e.to_string()));
        }

        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let proposal = if let Some(id) = imported.id.as_deref() {
            let Some(existing) = repo.resolve(id).await.ok().flatten() else {
                return Json(err_single(proposal_not_found_error(id)));
            };
            if let Err(e) = self
                .gate_proposal_edit(existing.author_user_id.as_deref())
                .await
            {
                return Json(err_single(e));
            }
            // Composed gate (task cuzf): block import that would leave an
            // `in_review` proposal failing DoR or tribunal conditions.
            if existing.status == "in_review" {
                let target_count = repo
                    .targets(&existing.id)
                    .await
                    .map(|t| t.len())
                    .unwrap_or(0);
                let gate = evaluate_composed_gate(
                    &repo,
                    &existing,
                    imported.body,
                    &imported.acceptance_criteria_json,
                    target_count,
                )
                .await;
                if let Some(err) = gate.to_error_string() {
                    return Json(err_single(err));
                }
            }
            match repo
                .update(
                    &existing.id,
                    djinn_db::ProposalUpdateInput {
                        title: &imported.title,
                        body: imported.body,
                        acceptance_criteria: &imported.acceptance_criteria_json,
                        status: &existing.status,
                        superseded_by: existing.superseded_by.as_deref(),
                        body_format: Some(&imported.body_format),
                        // Imported proposals restore historical state — they
                        // are not authoring operations and carry no
                        // block-patch / native-skill attribution.
                        event_metadata: None,
                    },
                )
                .await
            {
                Ok(proposal) => proposal,
                Err(e) => return Json(err_single(e.to_string())),
            }
        } else {
            match repo
                .create(djinn_db::ProposalCreateInput {
                    title: &imported.title,
                    body: imported.body,
                    acceptance_criteria: Some(&imported.acceptance_criteria_json),
                    status: None,
                    body_format: Some(&imported.body_format),
                })
                .await
            {
                Ok(proposal) => proposal,
                Err(e) => return Json(err_single(e.to_string())),
            }
        };

        Json(ProposalSingleResponse {
            proposal: Some(ProposalModel::from(&proposal)),
            mdx: None,
            error: None,
        })
    }

    /// Export a proposal as a portable proposal.mdx string.
    #[tool(
        description = "Export a proposal (by UUID or short_id) as a portable proposal.mdx string. Returns the proposal and an `mdx` field containing YAML frontmatter (title, body_format, acceptance_criteria) followed by the body exactly as stored. For mdx proposals the output is validated for round-trip fidelity through the block registry parser."
    )]
    pub async fn proposal_export(
        &self,
        Parameters(p): Parameters<ProposalExportParams>,
    ) -> Json<ProposalSingleResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_single(proposal_not_found_error(&p.id)));
        };

        // Build the YAML frontmatter matching the parse_proposal_mdx format.
        let ac_items: Vec<JsonValue> =
            serde_json::from_str::<serde_json::Value>(&proposal.acceptance_criteria)
                .ok()
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default();

        let ac_yaml_lines: Vec<String> = ac_items
            .iter()
            .map(|item| {
                // Each AC is either a plain string or a {criterion, met} object.
                if let Some(s) = item.as_str() {
                    format!("  - {s}")
                } else {
                    // Structured: { "criterion": "...", "met": bool }
                    let criterion = item.get("criterion").and_then(|v| v.as_str()).unwrap_or("");
                    let met = item.get("met").and_then(|v| v.as_bool()).unwrap_or(false);
                    format!("  - criterion: {criterion}\n    met: {met}")
                }
            })
            .collect();

        let ac_section = if ac_yaml_lines.is_empty() {
            String::from("acceptance_criteria: []\n")
        } else {
            format!("acceptance_criteria:\n{}\n", ac_yaml_lines.join("\n"))
        };

        let mdx_output = format!(
            "---\ntitle: {}\nbody_format: {}\n{}---\n{}",
            proposal.title, proposal.body_format, ac_section, proposal.body,
        );

        // For mdx proposals: round-trip validate by parsing the output through
        // parse_mdx_blocks and confirming structural equality.
        if proposal.body_format == "mdx" {
            match parse_mdx_blocks(&proposal.body) {
                Err(e) => {
                    return Json(err_single(format!("round-trip validation failed: {e}")));
                }
                Ok(original_blocks) => {
                    // Extract the body from the exported mdx (after second ---)
                    let exported_body = split_proposal_mdx_frontmatter(&mdx_output)
                        .ok()
                        .map(|(_, body)| body)
                        .unwrap_or("");
                    match parse_mdx_blocks(exported_body) {
                        Err(e) => {
                            return Json(err_single(format!(
                                "round-trip parse failed on exported body: {e}"
                            )));
                        }
                        Ok(exported_blocks) => {
                            if original_blocks != exported_blocks {
                                return Json(err_single(
                                    "round-trip validation failed: exported MDX blocks \
                                     differ from original"
                                        .to_string(),
                                ));
                            }
                        }
                    }
                }
            }
        }

        Json(ProposalSingleResponse {
            proposal: Some(ProposalModel::from(&proposal)),
            mdx: Some(mdx_output),
            error: None,
        })
    }

    /// Show a proposal with targets, feedback, debate trail, revisions, and sign-offs.
    #[tool(
        description = "Show a proposal (by UUID or short_id) including target projects, the feedback/discussion thread, the debate-trail (objections/rebuttals/verdicts kept separate from feedback), its revision history, and review sign-offs (each flagged `stale` when given against an older revision)."
    )]
    pub async fn proposal_show(
        &self,
        Parameters(p): Parameters<ProposalShowParams>,
    ) -> Json<ProposalShowResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_show(proposal_not_found_error(&p.id)));
        };
        let targets = match target_models(&repo, &project_repo, &proposal.id).await {
            Ok(t) => t,
            Err(e) => return Json(err_show(e)),
        };
        let feedback = match repo.feedback(&proposal.id).await {
            Ok(f) => f.iter().map(Into::into).collect(),
            Err(e) => return Json(err_show(e.to_string())),
        };
        let revisions = match repo.revisions(&proposal.id).await {
            Ok(r) => r.iter().map(Into::into).collect(),
            Err(e) => return Json(err_show(e.to_string())),
        };
        let signoffs = match repo.signoffs(&proposal.id).await {
            Ok(s) => s
                .iter()
                .map(|so| ProposalSignoffModel::from_signoff(so, proposal.latest_revision_seq))
                .collect(),
            Err(e) => return Json(err_show(e.to_string())),
        };
        let epic_repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        let epics = match graduated_epic_models(
            &repo,
            &epic_repo,
            &project_repo,
            &proposal.id,
            proposal.latest_revision_seq,
            proposal.pending_reconcile,
        )
        .await
        {
            Ok(e) => e,
            Err(e) => return Json(err_show(e)),
        };
        let memory_refs = match repo.memory_refs_for_proposal(&proposal.id).await {
            Ok(refs) => refs.into_iter().map(Into::into).collect(),
            Err(e) => return Json(err_show(e.to_string())),
        };
        let debate_trail = match repo.debate_trail(&proposal.id).await {
            Ok(d) => Some(d.iter().map(ProposalDebateTrailModel::from).collect()),
            Err(e) => return Json(err_show(e.to_string())),
        };
        // Derive refinement status from lifecycle events + debate trail.
        // Non-critical — swallow errors silently so proposal_show still works
        // even if refinement status can't be computed.
        let refinement =
            crate::tools::refinement_tools::build_refinement_status(&repo, &proposal.id)
                .await
                .ok();
        // Build composed gate status (DoR + tribunal conditions).
        // Non-critical — swallow errors so proposal_show still works.
        let ac_json = &proposal.acceptance_criteria;
        let target_count = targets.len();
        let gate_status =
            Some(build_gate_status(&repo, &proposal, &proposal.body, ac_json, target_count).await);
        Json(ProposalShowResponse {
            proposal: Some(ProposalModel::from(&proposal)),
            targets: Some(targets),
            feedback: Some(feedback),
            revisions: Some(revisions),
            signoffs: Some(signoffs),
            epics: Some(epics),
            memory_refs,
            debate_trail,
            refinement,
            gate_status,
            error: None,
        })
    }

    /// List proposals (global) with optional filters and pagination.
    #[tool(
        description = "List proposals globally (not scoped to a project) with optional filters: status, author, target_project (UUID or owner/repo slug), text. Offset-based pagination. Returns {proposals[], total_count, limit, offset, has_more}."
    )]
    pub async fn proposal_list(
        &self,
        Parameters(p): Parameters<ProposalListParams>,
    ) -> Json<ProposalListResponse> {
        let sort = p.sort.as_deref().unwrap_or("created_desc");
        if let Err(e) = validate_sort(
            sort,
            &["created", "created_desc", "updated", "updated_desc"],
        ) {
            return Json(list_response::error::<ProposalListResponse>(e));
        }
        let limit = validate_limit(p.limit.unwrap_or(25));
        let offset = validate_offset(p.offset.unwrap_or(0));

        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());

        // Resolve the target_project filter to a UUID if a slug was passed.
        let target_project_id = if let Some(ref tref) = p.target_project {
            let project_repo =
                ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
            match project_repo.resolve(tref).await {
                Ok(Some(id)) => Some(id),
                _ => {
                    return Json(list_response::error::<ProposalListResponse>(format!(
                        "target_project not found: {tref}"
                    )));
                }
            }
        } else {
            None
        };

        let query = ProposalListQuery {
            status: p.status,
            text: p.text,
            author_user_id: p.author,
            target_project_id,
            sort: sort.to_owned(),
            limit,
            offset,
        };
        let result = match repo.list_filtered(query).await {
            Ok(result) => result,
            Err(e) => return Json(list_response::error::<ProposalListResponse>(e.to_string())),
        };

        // Batch the tribunal/readiness summary across only the non-terminal
        // proposals on this page (draft/in_review). Terminal proposals
        // (done/rejected/archived/superseded, and any non-active status) keep
        // `list_summary = None` so the list stays cheap and the UI shows no chips
        // for them. A summary-query failure is non-fatal: the list still renders
        // (rows just lack chips).
        let summary_ids: Vec<String> = result
            .proposals
            .iter()
            .filter(|(p, _)| proposal_status_is_non_terminal(&p.status))
            .map(|(p, _)| p.id.clone())
            .collect();
        let summaries = if summary_ids.is_empty() {
            std::collections::HashMap::new()
        } else {
            repo.list_summaries(&summary_ids).await.unwrap_or_default()
        };

        let rows: Vec<ProposalModel> = result
            .proposals
            .iter()
            .map(|(p, count)| {
                let model = ProposalModel::from_with_count(p, *count);
                match summaries.get(&p.id) {
                    Some(raw) => model.with_list_summary(build_list_summary(p, raw)),
                    None => model,
                }
            })
            .collect();

        Json(list_response::success::<ProposalListResponse>(
            rows,
            result.total_count,
            limit,
            offset,
        ))
    }
}

#[cfg(test)]
mod list_summary_tests {
    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
    use djinn_core::events::EventBus;
    use djinn_db::{
        Database, ProjectRepository, ProposalCreateInput, ProposalDebateTrailCreateInput,
        ProposalRepository,
    };

    /// A well-formed body that passes all deterministic readiness checks.
    fn ready_body() -> &'static str {
        r#"
# Problem
Users cannot do X.

# Scope
In scope: Y. Out of scope: Z.

# Objectives
- Deliver A

## File map
```file-map
    src/main.rs
```

# Dependencies
Blocked by service C.

# Open Questions
What happens if D fails?
"#
    }

    async fn test_server() -> (DjinnMcpServer, Database) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        (DjinnMcpServer::new(test_mcp_state(db.clone())), db)
    }

    /// Pull the `list_summary` object for a given proposal id out of a
    /// `proposal_list` response.
    fn summary_for<'a>(
        list: &'a serde_json::Value,
        proposal_id: &str,
    ) -> Option<&'a serde_json::Value> {
        list.get("proposals")?
            .as_array()?
            .iter()
            .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(proposal_id))
            .and_then(|p| p.get("list_summary"))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_list_surfaces_tribunal_and_gate_summary() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = project_repo
            .create("svc-list-sum", "test", "svc-list-sum-repo")
            .await
            .unwrap();

        // Messy: empty body (fails DoR), no target, active refinement, one
        // blocking objection, and a judge needs-work verdict.
        let messy = repo
            .create(ProposalCreateInput {
                title: "Messy",
                body: "just some text",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        repo.record_refinement_lifecycle(&messy.id, "refinement_start", None)
            .await
            .unwrap();
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &messy.id,
            kind: "objection",
            body: "unbounded scope",
            blocking: true,
            agent_role: "adversary",
            author_kind: "agent",
            author_model: Some("m"),
            source_task_id: None,
            against_revision_seq: 1,
            round: 2,
            body_metadata: None,
        })
        .await
        .unwrap();
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &messy.id,
            kind: "verdict",
            body: "verdict: needs-work",
            blocking: true,
            agent_role: "judge",
            author_kind: "agent",
            author_model: Some("m"),
            source_task_id: None,
            against_revision_seq: 1,
            round: 2,
            body_metadata: None,
        })
        .await
        .unwrap();

        // Clean: DoR-passing body, a target, refinement converged awaiting
        // review, an approving verdict, no blocking objections.
        let clean = repo
            .create(ProposalCreateInput {
                title: "Clean",
                body: ready_body(),
                acceptance_criteria: Some(r#"[{"criterion":"API returns 200","met":false}]"#),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        repo.add_target(&clean.id, &project.id, "primary")
            .await
            .unwrap();
        repo.record_refinement_lifecycle(&clean.id, "refinement_start", None)
            .await
            .unwrap();
        repo.record_refinement_lifecycle(&clean.id, "refinement_awaiting_review", None)
            .await
            .unwrap();

        let list = server
            .dispatch_tool("proposal_list", serde_json::json!({ "limit": 50 }))
            .await
            .unwrap();
        assert!(
            list.get("error").is_none(),
            "proposal_list failed: {:?}",
            list.get("error")
        );

        let m = summary_for(&list, &messy.id).expect("messy has a list_summary");
        assert_eq!(m["refinement_active"], serde_json::json!(true));
        assert_eq!(m["awaiting_review"], serde_json::json!(false));
        assert_eq!(m["current_round"], serde_json::json!(2));
        assert_eq!(m["needs_evidence"], serde_json::json!(false));
        assert_eq!(m["dor_ready"], serde_json::json!(false));
        assert_eq!(m["gate_ready"], serde_json::json!(false));
        assert_eq!(
            m["unresolved_blocking_count"],
            serde_json::json!(1),
            "the judge verdict row must be excluded from the objection count"
        );

        let c = summary_for(&list, &clean.id).expect("clean has a list_summary");
        assert_eq!(c["refinement_active"], serde_json::json!(true));
        assert_eq!(c["awaiting_review"], serde_json::json!(true));
        assert_eq!(c["dor_ready"], serde_json::json!(true));
        assert_eq!(c["gate_ready"], serde_json::json!(true));
        assert_eq!(c["unresolved_blocking_count"], serde_json::json!(0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_list_omits_summary_for_terminal_proposals() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let done = repo
            .create(ProposalCreateInput {
                title: "Shipped",
                body: ready_body(),
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        repo.set_status(&done.id, "done").await.unwrap();

        let list = server
            .dispatch_tool("proposal_list", serde_json::json!({ "limit": 50 }))
            .await
            .unwrap();
        let entry = list
            .get("proposals")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(done.id.as_str()))
            })
            .expect("proposal present in list");
        assert!(
            entry.get("list_summary").is_none(),
            "terminal proposals must not carry a list_summary (chips hidden)"
        );
    }
}
