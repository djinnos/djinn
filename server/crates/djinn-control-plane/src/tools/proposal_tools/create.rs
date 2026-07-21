// Create/read/import/export/list/update/block-patch/delete CRUD tools for the
// global Proposals layer.
//
// This submodule owns the create/import/export/show/list/update/block-patch/
// delete mutation surface plus target add/remove and the cohesive list/show/
// target response shaping used by those tools.
//
// CRUD/target ownership checklist for task xpj0:
// - moved here: `proposal_add_target`, `proposal_remove_target`,
//   `target_models`, `finish_targets`, and `graduated_epic_models`;
// - already owned here: create/import/export/show/list tools; update/delete/
//   block-patch moved here from the py7d sibling slice;
// - tests for the CRUD concern live in `create_tests.rs` so this production
//   module stays under the Server Size Guard threshold;
// - intentionally shared in `mod.rs`: composed gate/readiness helpers and
//   `err_single`/`err_show` response constructors used by later feedback,
//   signoff, lifecycle, and refinement slices.
//
// Debate-trail and refinement-status data fetches in `proposal_show`:
// `proposal_show` fetches the debate trail (`repo.debate_trail`) and refinement
// status (`refinement_tools::build_refinement_status`) as part of the show
// response assembly. These are thin CRUD data fetches — not debate/refinement
// glue — so they remain here alongside the other show response fields (targets,
// feedback, revisions, signoffs, epics). The `refinement_active` field mapping
// in list summary construction is likewise a response-shaping concern.
//

use std::borrow::Cow;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::Serialize;
use serde_json::Value as JsonValue;

use crate::server::DjinnMcpServer;
use crate::tools::acting_user::acting_caps;
use crate::tools::list_response::{
    self, ListMeta, NamedListResponse, named_list_response_schema, serialize_named_list_response,
};
use crate::tools::proposal_ops::{
    ProposalDebateTrailModel, ProposalDeleteResponse, ProposalEpicModel, ProposalListRow,
    ProposalListSummary, ProposalModel, ProposalShowResponse, ProposalSignoffModel,
    ProposalSingleResponse, ProposalTargetModel, ProposalTargetsResponse, apply_revision_body_mode,
    validate_revision_bodies_value, validate_show_fields,
};
use crate::tools::proposal_readiness::evaluate_proposal_readiness;
use crate::tools::validation::{
    resolve_body_format_and_validate, validate_ac_count, validate_design, validate_limit,
    validate_offset, validate_proposal_create_status, validate_proposal_status, validate_sort,
    validate_title,
};
use djinn_db::{
    EpicRepository, ProjectRepository, ProposalListQuery, ProposalListSummaryRow,
    ProposalRepository,
};
use djinn_spec_lint::{parse_mdx_blocks, validate_question_form_placement};

use super::mdx::{
    ProposalBlockPatchParams, apply_block_patch, parse_proposal_mdx, split_proposal_mdx_frontmatter,
};

// Re-import shared helpers kept in `mod.rs` as `pub(super)`.
use super::{
    build_gate_status, err_show, err_single, evaluate_composed_gate, format_readiness_error,
    parse_ac_items, proposal_not_found_error,
};

// Parameter structs live in `params.rs` to keep this file under the size guard.
use super::params::{
    ProposalCreateParams, ProposalDeleteParams, ProposalExportParams, ProposalImportParams,
    ProposalListParams, ProposalShowParams, ProposalTargetParams, ProposalUpdateParams,
};

// ── Target/show response helpers ─────────────────────────────────────────────

/// List a proposal's targets and resolve each project id to an `owner/repo`
/// slug + name for display chips.
pub(super) async fn target_models(
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
/// Resolve a proposal's graduated epics to `{epic_short_id, project_path,
/// status}` display models.
pub(super) async fn graduated_epic_models(
    repo: &ProposalRepository,
    epic_repo: &EpicRepository,
    project_repo: &ProjectRepository,
    proposal_id: &str,
    latest_revision_seq: i32,
    pending_reconcile: bool,
) -> Result<Vec<ProposalEpicModel>, String> {
    let links = repo
        .graduated_epics(proposal_id)
        .await
        .map_err(|e| e.to_string())?;
    let reconciliations = repo
        .latest_epic_reconciliations(proposal_id)
        .await
        .map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(links.len());
    for (epic_id, project_id) in links {
        let Some(epic) = epic_repo.get(&epic_id).await.ok().flatten() else {
            continue;
        };
        let reconciled_at_revision_seq = reconciliations.get(&epic_id).copied();
        let needs_reconcile = pending_reconcile
            && reconciled_at_revision_seq
                .map(|seq| seq < latest_revision_seq)
                .unwrap_or(false);
        let project_path = match project_repo.get(&project_id).await {
            Ok(Some(p)) => format!("{}/{}", p.github_owner, p.github_repo),
            _ => project_id.clone(),
        };
        out.push(ProposalEpicModel {
            epic_id,
            epic_short_id: epic.short_id,
            epic_title: epic.title,
            epic_emoji: epic.emoji,
            project_path,
            status: epic.status,
            reconciled_at_revision_seq,
            needs_reconcile,
        });
    }
    Ok(out)
}
pub(super) fn err_targets(error: impl Into<String>) -> ProposalTargetsResponse {
    ProposalTargetsResponse {
        targets: None,
        error: Some(error.into()),
    }
}
async fn finish_targets(
    repo: &ProposalRepository,
    project_repo: &ProjectRepository,
    proposal_id: &str,
) -> Json<ProposalTargetsResponse> {
    match target_models(repo, project_repo, proposal_id).await {
        Ok(targets) => Json(ProposalTargetsResponse {
            targets: Some(targets),
            error: None,
        }),
        Err(e) => Json(err_targets(e)),
    }
}

// ── List response (NamedListResponse boilerplate, mirrors EpicListResponse) ──

#[derive(Clone)]
pub struct ProposalListResponse {
    pub proposals: Option<Vec<ProposalListRow>>,
    pub meta: ListMeta,
}

impl NamedListResponse for ProposalListResponse {
    type Item = ProposalListRow;
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
        named_list_response_schema::<ProposalListRow>(generator, Self::TITLE, Self::FIELD_NAME)
    }
}

// ── List-summary helpers (used only by `proposal_list`) ──────────────────────

/// Whether a proposal status is non-terminal (still moving through scoping /
/// review). Only these get a batched list summary — terminal proposals never
/// show tribunal/gate chips. Mirrors the statuses the composed gate cares about
/// (`draft` and `in_review`).
fn proposal_status_is_non_terminal(status: &str) -> bool {
    matches!(status, "draft" | "in_review")
}

/// Heuristic for a needs-work judge verdict — kept identical to the one in
/// `build_gate_status` so the list and the detail gate agree.
fn judge_verdict_is_needs_work(verdict_body: &str) -> bool {
    let lower = verdict_body.to_lowercase();
    lower.contains("needs-work") || lower.contains("needs_work") || lower.contains("needs work")
}

/// Compose the list-row tribunal/readiness summary from a proposal row plus its
/// batched raw facts. Deterministic and query-free (all data is already loaded):
/// runs the in-memory DoR evaluator and approximates the composed gate.
fn build_list_summary(
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

// ── Tool router: create / import / export / show / list / update / block-patch / delete / target ──

#[tool_router(router = proposal_create_tool_router, vis = "pub(super)")]
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
        // Resolve the persisted body_format (auto-upgrading a markdown body that
        // carries block tags to mdx) and run the full MDX block-validation stack.
        let body_format = match resolve_body_format_and_validate(body, p.body_format.as_deref()) {
            Ok(f) => f,
            Err(e) => return Json(err_single(e)),
        };
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
                body_format: Some(body_format),
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
        // Resolve the persisted format — a markdown-frontmatter body that carries
        // block tags is upgraded to mdx — and validate the full block stack.
        let body_format =
            match resolve_body_format_and_validate(imported.body, Some(&imported.body_format)) {
                Ok(f) => f,
                Err(e) => return Json(err_single(e)),
            };

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
                        body_format: Some(body_format),
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
                    body_format: Some(body_format),
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

    /// Show a proposal with configurable field selection and revision body modes.
    #[tool(
        description = "Show a proposal (by UUID or short_id) with optional field selection. Pass `fields` to include only specific sections: proposal, targets, feedback, signoffs, revisions, debate, epics, gate_status (default: all). Pass `revision_bodies` to control revision verbosity: excerpt (default), full, omit. Omitted fields are absent from the response and their data is not loaded."
    )]
    pub async fn proposal_show(
        &self,
        Parameters(p): Parameters<ProposalShowParams>,
    ) -> Json<ProposalShowResponse> {
        // ── Validate params ──────────────────────────────────────────────
        if let Some(ref fields) = p.fields
            && let Err(e) = validate_show_fields(fields)
        {
            return Json(err_show(e));
        }
        if let Some(ref rb) = p.revision_bodies
            && let Err(e) = validate_revision_bodies_value(rb)
        {
            return Json(err_show(e));
        }
        // Helper: is this field selected? None = all selected (default).
        let field_selected = |name: &str| -> bool {
            match &p.fields {
                None => true,
                Some(f) => f.iter().any(|f| f == name),
            }
        };

        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_show(proposal_not_found_error(&p.id)));
        };

        // ── proposal ────────────────────────────────────────────────────
        let proposal_model = if field_selected("proposal") {
            Some(ProposalModel::from(&proposal))
        } else {
            None
        };

        // ── targets ─────────────────────────────────────────────────────
        // Build full target models only when `targets` is selected.
        // When only `gate_status` is selected, we need just the count.
        let targets: Option<Vec<ProposalTargetModel>> = if field_selected("targets") {
            match target_models(&repo, &project_repo, &proposal.id).await {
                Ok(t) => Some(t),
                Err(e) => return Json(err_show(e)),
            }
        } else {
            None
        };

        // ── feedback ────────────────────────────────────────────────────
        let feedback: Option<Vec<crate::tools::proposal_ops::ProposalFeedbackModel>> =
            if field_selected("feedback") {
                match repo.feedback(&proposal.id).await {
                    Ok(f) => Some(f.iter().map(Into::into).collect()),
                    Err(e) => return Json(err_show(e.to_string())),
                }
            } else {
                None
            };

        // ── revisions ───────────────────────────────────────────────────
        let mut revisions: Option<Vec<crate::tools::proposal_ops::ProposalRevisionModel>> =
            if field_selected("revisions") {
                match repo.revisions(&proposal.id).await {
                    Ok(r) => Some(r.iter().map(Into::into).collect()),
                    Err(e) => return Json(err_show(e.to_string())),
                }
            } else {
                None
            };

        // Apply revision body mode only when revisions are selected.
        let revision_bodies_mode = if field_selected("revisions") {
            p.revision_bodies.as_deref().unwrap_or("excerpt")
        } else {
            // When revisions are not selected, ignore revision_bodies.
            "omit"
        };
        if let Some(ref mut revs) = revisions {
            apply_revision_body_mode(revs, revision_bodies_mode);
        }

        // ── signoffs ────────────────────────────────────────────────────
        let signoffs: Option<Vec<ProposalSignoffModel>> = if field_selected("signoffs") {
            match repo.signoffs(&proposal.id).await {
                Ok(s) => Some(
                    s.iter()
                        .map(|so| {
                            ProposalSignoffModel::from_signoff(so, proposal.latest_revision_seq)
                        })
                        .collect(),
                ),
                Err(e) => return Json(err_show(e.to_string())),
            }
        } else {
            None
        };

        // ── epics (includes memory_refs) ────────────────────────────────
        let (epics, memory_refs): (
            Option<Vec<ProposalEpicModel>>,
            Vec<crate::tools::proposal_ops::ProposalMemoryRefModel>,
        ) = if field_selected("epics") {
            let epic_repo =
                djinn_db::EpicRepository::new(self.state.db().clone(), self.state.event_bus());
            let epics_result = match graduated_epic_models(
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
            (Some(epics_result), memory_refs)
        } else {
            (None, Vec::new())
        };

        // ── debate (includes refinement status) ─────────────────────────
        let (debate_trail, refinement): (
            Option<Vec<ProposalDebateTrailModel>>,
            Option<crate::tools::proposal_ops::ProposalRefinementStatusModel>,
        ) = if field_selected("debate") {
            let trail = match repo.debate_trail(&proposal.id).await {
                Ok(d) => Some(d.iter().map(ProposalDebateTrailModel::from).collect()),
                Err(e) => return Json(err_show(e.to_string())),
            };
            // Derive refinement status from lifecycle events + debate trail.
            // Non-critical — swallow errors silently so proposal_show still
            // works even if refinement status can't be computed.
            let refinement =
                crate::tools::refinement_tools::build_refinement_status(&repo, &proposal.id)
                    .await
                    .ok();
            (trail, refinement)
        } else {
            (None, None)
        };

        // ── gate_status ─────────────────────────────────────────────────
        let gate_status = if field_selected("gate_status") {
            let ac_json = &proposal.acceptance_criteria;
            // Use target count from already-loaded targets, or fetch just the
            // count when targets are not selected.
            let target_count = match &targets {
                Some(t) => t.len(),
                None => repo
                    .targets(&proposal.id)
                    .await
                    .map(|t| t.len())
                    .unwrap_or(0),
            };
            Some(build_gate_status(&repo, &proposal, &proposal.body, ac_json, target_count).await)
        } else {
            None
        };

        Json(ProposalShowResponse {
            proposal: proposal_model,
            targets,
            feedback,
            revisions,
            signoffs,
            epics,
            memory_refs,
            debate_trail,
            refinement,
            gate_status,
            error: None,
        })
    }

    /// List proposals (global) with optional filters and pagination.
    #[tool(
        description = "List proposal summaries globally with optional filters and offset pagination. Default rows contain identity, time, workflow summary, and acceptance counts only. Pass include_excerpts=true for body_excerpt/body_truncated, include_acceptance_criteria=true for criteria, or include_bodies=true for body plus excerpt metadata. For complete proposal detail, use proposal_show (the sole deep dive). Returns {proposals[], total_count, limit, offset, has_more}."
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

        let include_bodies = p.include_bodies.unwrap_or(false);
        let include_excerpts = p.include_excerpts.unwrap_or(false);
        let include_acceptance_criteria = p.include_acceptance_criteria.unwrap_or(false);

        let rows: Vec<ProposalListRow> = result
            .proposals
            .iter()
            .map(|(p, count)| {
                let model = ProposalListRow::from_proposal(
                    p,
                    *count,
                    include_bodies,
                    include_excerpts,
                    include_acceptance_criteria,
                );
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

    /// Add (or re-role) a target project on a proposal.
    #[tool(
        description = "Add a target project to a proposal (or change its role if already present). `project` is a UUID or owner/repo slug; `role` is `primary` (default) or `reference`. This is the re-target capability — editable at any time. Returns the proposal's updated target list."
    )]
    pub async fn proposal_add_target(
        &self,
        Parameters(p): Parameters<ProposalTargetParams>,
    ) -> Json<ProposalTargetsResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_targets(proposal_not_found_error(&p.id)));
        };
        if let Err(e) = self
            .gate_proposal_edit(proposal.author_user_id.as_deref())
            .await
        {
            return Json(err_targets(e));
        }
        let role = p.role.as_deref().unwrap_or("primary");
        if !matches!(role, "primary" | "reference") {
            return Json(err_targets(format!(
                "invalid role: {role:?} (expected primary or reference)"
            )));
        }
        let project_id = match project_repo.resolve(&p.project).await {
            Ok(Some(id)) => id,
            _ => return Json(err_targets(format!("project not found: {}", p.project))),
        };
        if let Err(e) = repo.add_target(&proposal.id, &project_id, role).await {
            return Json(err_targets(e.to_string()));
        }
        finish_targets(&repo, &project_repo, &proposal.id).await
    }

    /// Remove a target project from a proposal.
    #[tool(
        description = "Remove a target project from a proposal. `project` is a UUID or owner/repo slug. No-op if it wasn't a target. Returns the proposal's updated target list."
    )]
    pub async fn proposal_remove_target(
        &self,
        Parameters(p): Parameters<ProposalTargetParams>,
    ) -> Json<ProposalTargetsResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_targets(proposal_not_found_error(&p.id)));
        };
        if let Err(e) = self
            .gate_proposal_edit(proposal.author_user_id.as_deref())
            .await
        {
            return Json(err_targets(e));
        }
        // Fall back to the raw value so a stale target can still be removed.
        let project_id = project_repo
            .resolve(&p.project)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| p.project.clone());
        if let Err(e) = repo.remove_target(&proposal.id, &project_id).await {
            return Json(err_targets(e.to_string()));
        }
        finish_targets(&repo, &project_repo, &proposal.id).await
    }

    /// Update a proposal's editable fields.
    #[tool(
        description = "Update a proposal (by UUID or short_id): title, body, acceptance_criteria, status (draft|shared|ready|archived|superseded), and superseded_by. Only provided fields change."
    )]
    pub async fn proposal_update(
        &self,
        Parameters(p): Parameters<ProposalUpdateParams>,
    ) -> Json<ProposalSingleResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(existing) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_single(proposal_not_found_error(&p.id)));
        };
        if let Err(e) = self
            .gate_proposal_edit(existing.author_user_id.as_deref())
            .await
        {
            return Json(err_single(e));
        }

        let title = if let Some(ref t) = p.title {
            match validate_title(t) {
                Ok(v) => v,
                Err(e) => return Json(err_single(e)),
            }
        } else {
            existing.title.clone()
        };

        let body = p.body.as_deref().unwrap_or(&existing.body);
        if let Err(e) = validate_design(body) {
            return Json(err_single(e));
        }
        // Effective declared format: explicitly passed, else the proposal's
        // current format (matches the repository's own fallback on update).
        let declared_format = p
            .body_format
            .as_deref()
            .unwrap_or(existing.body_format.as_str());
        // Resolve the persisted format (auto-upgrading a markdown body that
        // carries block tags) and run the full MDX block-validation stack.
        let body_format = match resolve_body_format_and_validate(body, Some(declared_format)) {
            Ok(f) => f,
            Err(e) => return Json(err_single(e)),
        };
        if p.body.is_some()
            && body_format == "mdx"
            && let Err(e) = validate_question_form_placement(body)
        {
            return Json(err_single(e));
        }

        let ac_json = if let Some(ac) = &p.acceptance_criteria {
            if let Err(e) = validate_ac_count(ac.len()) {
                return Json(err_single(e));
            }
            serde_json::to_string(ac).unwrap_or_else(|_| "[]".to_string())
        } else {
            existing.acceptance_criteria.clone()
        };

        let status = p.status.as_deref().unwrap_or(&existing.status);
        if let Err(e) = validate_proposal_status(status) {
            return Json(err_single(e));
        }

        // Composed gate (task cuzf): block entering `in_review` when
        // DoR or tribunal conditions are not met. Existing body/MDX/AC-count
        // validation already passed above.
        if status == "in_review" {
            let target_count = repo
                .targets(&existing.id)
                .await
                .map(|t| t.len())
                .unwrap_or(0);
            let gate = evaluate_composed_gate(&repo, &existing, body, &ac_json, target_count).await;
            if let Some(err) = gate.to_error_string() {
                return Json(err_single(err));
            }
        }

        // Resolve superseded_by to a canonical proposal id when provided.
        let superseded_by = if let Some(ref s) = p.superseded_by {
            match repo.resolve(s).await.ok().flatten() {
                Some(target) => Some(target.id),
                None => return Json(err_single(format!("superseded_by proposal not found: {s}"))),
            }
        } else {
            existing.superseded_by.clone()
        };

        match repo
            .update(
                &existing.id,
                djinn_db::ProposalUpdateInput {
                    title: &title,
                    body,
                    acceptance_criteria: &ac_json,
                    status,
                    superseded_by: superseded_by.as_deref(),
                    body_format: Some(body_format),
                    // Plain `proposal_update` writes carry no authoring
                    // attribution metadata — block-patch / native-skill tagging
                    // is reserved for the targeted patch primitive.
                    event_metadata: None,
                },
            )
            .await
        {
            Ok(updated) => Json(ProposalSingleResponse {
                proposal: Some(ProposalModel::from(&updated)),
                mdx: None,
                error: None,
            }),
            Err(e) => Json(err_single(e.to_string())),
        }
    }

    /// Apply a single targeted MDX block patch to a proposal body.
    ///
    /// Locates a specific range in the proposal body using the provided
    /// selector (heading, exact text, or byte range), then replaces or wraps
    /// that range with the given MDX content. Unrelated body content is
    /// preserved. Each successful patch increments `latest_revision_seq` exactly
    /// once and records targeted-block-patch metadata.
    #[tool(
        description = "Apply a single targeted MDX block patch to a proposal body. Locates a range via selector (heading_text, exact_text, or byte_range), then replaces or wraps it with the given block_mdx. Unrelated content is preserved. Each successful patch records one proposal revision with targeted-block-patch metadata."
    )]
    pub async fn proposal_block_patch(
        &self,
        Parameters(p): Parameters<ProposalBlockPatchParams>,
    ) -> Json<ProposalSingleResponse> {
        // 1. Resolve proposal.
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(existing) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_single(proposal_not_found_error(&p.id)));
        };

        // 2. Edit gate.
        if let Err(e) = self
            .gate_proposal_edit(existing.author_user_id.as_deref())
            .await
        {
            return Json(err_single(e));
        }

        // 3. Stale revision guard.
        if let Some(expected_seq) = p.expected_latest_revision_seq
            && existing.latest_revision_seq != expected_seq
        {
            return Json(err_single(format!(
                "stale revision: expected latest_revision_seq={}, but proposal has {}",
                expected_seq, existing.latest_revision_seq
            )));
        }

        // 4. Apply the patch (selector resolution, body build, MDX validation,
        //    and metadata) via the shared pure transformation.
        let outcome = match apply_block_patch(&existing.body, &existing.body_format, &p) {
            Ok(o) => o,
            Err(e) => return Json(err_single(e)),
        };

        // 5. Persist through the revisioning path.
        let ac_json = existing.acceptance_criteria.clone();
        match repo
            .update(
                &existing.id,
                djinn_db::ProposalUpdateInput {
                    title: &existing.title,
                    body: &outcome.new_body,
                    acceptance_criteria: &ac_json,
                    status: &existing.status,
                    superseded_by: existing.superseded_by.as_deref(),
                    body_format: Some(outcome.new_body_format),
                    event_metadata: Some(&outcome.event_metadata),
                },
            )
            .await
        {
            Ok(updated) => Json(ProposalSingleResponse {
                proposal: Some(ProposalModel::from(&updated)),
                mdx: None,
                error: None,
            }),
            Err(e) => Json(err_single(e.to_string())),
        }
    }

    /// Delete a proposal (cascades to its targets and feedback).
    #[tool(
        description = "Delete a proposal (by UUID or short_id). Cascades to its targets and feedback. Returns {ok}."
    )]
    pub async fn proposal_delete(
        &self,
        Parameters(p): Parameters<ProposalDeleteParams>,
    ) -> Json<ProposalDeleteResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(existing) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(ProposalDeleteResponse {
                ok: None,
                error: Some(proposal_not_found_error(&p.id)),
            });
        };
        if let Err(e) = self
            .gate_proposal_edit(existing.author_user_id.as_deref())
            .await
        {
            return Json(ProposalDeleteResponse {
                ok: None,
                error: Some(e),
            });
        }
        match repo.delete(&existing.id).await {
            Ok(()) => Json(ProposalDeleteResponse {
                ok: Some(true),
                error: None,
            }),
            Err(e) => Json(ProposalDeleteResponse {
                ok: None,
                error: Some(e.to_string()),
            }),
        }
    }
}

#[cfg(test)]
mod create_tests {
    include!("create_tests.rs");
}
