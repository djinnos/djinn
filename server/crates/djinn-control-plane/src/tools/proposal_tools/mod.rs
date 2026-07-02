// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
// MCP tools for the global Proposals layer (Phase 0).
//
// A proposal is a project-INDEPENDENT, collaboratively-authored artifact
// (spec body + acceptance criteria) that targets zero, one, or many projects
// via an editable M:N `proposal_targets` set. Discussion and suggestions share
// one `proposal_feedback` primitive (status == null → discussion; open/
// accepted/rejected → a trackable suggestion; author_kind == "ai" for future
// adversarial-review findings). Sign-offs gate approval, revisions/diffs track
// edits, and `proposal_graduate` kicks an approved proposal off into one epic
// per primary target (the existing single-repo-write execution engine).
//
// CRUD/create/read tools live in `create.rs`; update/delete/target/lifecycle
// tools still live here and will move in follow-up tasks.

mod create;
mod mdx;

// Re-export CRUD tool parameter/response types so the public module path
// `crate::tools::proposal_tools::{...}` stays stable for existing dispatch and
// MCP-extension consumers.
pub use create::{
    ProposalCreateParams, ProposalExportParams, ProposalImportParams, ProposalListParams,
    ProposalListResponse, ProposalShowParams,
};

// Re-export MDX/block-patch types so the public module path
// `crate::tools::proposal_tools::{...}` stays stable for existing dispatch
// and MCP-extension consumers.
pub use mdx::{
    BlockPatchOutcome, BlockPatchSelector, ByteRangeSelector, ProposalBlockPatchParams,
    apply_block_patch,
};

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::Deserialize;

use crate::server::DjinnMcpServer;
use crate::tools::epic_ops::AcceptanceCriterionItem;
use crate::tools::proposal_ops::{
    ProposalDebateTrailModel, ProposalDeleteResponse, ProposalEpicModel, ProposalFeedbackResponse,
    ProposalModel, ProposalReconcileObsoleteEpicResponse, ProposalShowResponse, ProposalSignoffModel,
    ProposalSingleResponse, ProposalTargetModel, ProposalTargetsResponse,
};
use crate::tools::proposal_readiness::evaluate_proposal_readiness;
use crate::tools::validation::{
    validate_ac_count, validate_body, validate_design, validate_limit, validate_mdx_body,
    validate_offset, validate_proposal_create_status, validate_proposal_status, validate_sort,
    validate_title,
};
use djinn_db::{EpicRepository, ProjectRepository, ProposalRepository, TaskRepository};

use create::{
    build_gate_status, build_list_summary, evaluate_composed_gate, parse_ac_items,
    proposal_not_found_error, target_models,
};
use mdx::{parse_proposal_mdx, split_proposal_mdx_frontmatter};

// Shared response constructors.

fn err_show(error: impl Into<String>) -> ProposalShowResponse {
    ProposalShowResponse {
        proposal: None,
        targets: None,
        feedback: None,
        revisions: None,
        signoffs: None,
        epics: None,
        memory_refs: vec![],
        debate_trail: None,
        refinement: None,
        gate_status: None,
        error: Some(error.into()),
    }
}

fn err_single(error: impl Into<String>) -> ProposalSingleResponse {
    ProposalSingleResponse {
        proposal: None,
        mdx: None,
        error: Some(error.into()),
    }
}

fn err_targets(error: impl Into<String>) -> ProposalTargetsResponse {
    ProposalTargetsResponse {
        targets: None,
        error: Some(error.into()),
    }
}

fn err_feedback(error: impl Into<String>) -> ProposalFeedbackResponse {
    ProposalFeedbackResponse {
        feedback: None,
        error: Some(error.into()),
    }
}

// ── Param structs left in mod.rs for the follow-up slices. ────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalUpdateParams {
    /// Proposal UUID or short_id.
    pub id: String,
    pub title: Option<String>,
    pub body: Option<String>,
    /// Acceptance criteria: plain strings or `{criterion, met}` objects.
    pub acceptance_criteria: Option<Vec<AcceptanceCriterionItem>>,
    /// draft | in_review | approved | building | done | rejected | archived | superseded.
    pub status: Option<String>,
    /// UUID or short_id of the proposal that supersedes this one.
    pub superseded_by: Option<String>,
    /// Body encoding: `markdown` (default) or `mdx` (block-aware).
    pub body_format: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalDeleteParams {
    /// Proposal UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalTargetParams {
    /// Proposal UUID or short_id.
    pub id: String,
    /// Target project: UUID or owner/repo slug (must be registered).
    pub project: String,
    /// `primary` (a write-target, default) or `reference` (read-only context).
    pub role: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalFeedbackAddParams {
    /// Proposal UUID or short_id.
    pub proposal_id: String,
    pub body: String,
    /// Parent feedback id for a threaded reply.
    pub parent_id: Option<String>,
    /// `user` (default) or `ai`.
    pub author_kind: Option<String>,
    /// Model id when author_kind is `ai`.
    pub author_model: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalFeedbackResolveParams {
    /// Feedback entry UUID.
    pub id: String,
    /// The proposal revision that addressed this feedback (omit for a plain
    /// dismissal with no spec change).
    pub resolved_revision_seq: Option<i32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalSignoffParams {
    /// Proposal UUID or short_id.
    pub id: String,
    /// `scoped` (product) or `technical` (engineering).
    pub kind: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalSignoffClearParams {
    /// Proposal UUID or short_id.
    pub id: String,
    /// `scoped` (product) or `technical` (engineering).
    pub kind: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalGraduateParams {
    /// Proposal UUID or short_id (must be `approved`).
    pub id: String,
    /// Build owner — must be a participant (author or a sign-off giver).
    /// Defaults to the kicking-off user.
    pub owner_user_id: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalStopBuildParams {
    /// Proposal UUID or short_id (must be `building`).
    pub id: String,
    /// `abort` (tear the build down and revert to `approved`), `freeze` (hold
    /// the build's tasks out of dispatch, leaving epics/tasks/branches in
    /// place), or `unfreeze` (resume a frozen build).
    pub mode: String,
    /// Why the build is being stopped. Recorded as the force-close reason on
    /// each torn-down task. Required for `abort`.
    pub reason: Option<String>,
    /// When true on `abort`, compute and return the blast radius (epics, open
    /// tasks, running sessions) WITHOUT mutating anything.
    pub preview: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalReconcileObsoleteEpicParams {
    /// Proposal UUID or short_id. Alias for `proposal_id`.
    pub id: Option<String>,
    /// Proposal UUID or short_id. Alias for `id`.
    pub proposal_id: Option<String>,
    /// Epic UUID or short_id to retire from this proposal's graduated epics.
    pub epic_id: String,
    /// Why obsolete work is being force-closed. Defaults to a reconcile teardown reason.
    pub reason: Option<String>,
    /// When true, compute blast radius without closing tasks, closing/unlinking the epic, or killing sessions.
    pub preview: Option<bool>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct ProposalStopBuildResponse {
    pub ok: bool,
    pub mode: String,
    pub proposal_id: Option<String>,
    /// Resulting proposal status (`approved` after a non-preview abort,
    /// `building` after freeze/unfreeze/preview).
    pub status: Option<String>,
    /// `true` when this was a dry-run that did not mutate anything.
    pub preview: bool,
    /// Epics torn down (abort) or that would be (preview).
    pub epics_closed: i64,
    /// Worker tasks force-closed (abort) or open right now (preview).
    pub tasks_closed: i64,
    /// Running worker sessions killed (abort) or live now (preview).
    pub sessions_killed: i64,
    pub error: Option<String>,
}

/// Resolve a proposal's graduated epics to `{epic_short_id, project_path,
/// status}` display models.
async fn graduated_epic_models(
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

/// Helper: convert CamelCase/PascalCase enum variant to snake_case.
trait ToSnakeCase {
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

// ── Tool router ──────────────────────────────────────────────────────────────

#[tool_router(router = proposal_tool_router, vis = "pub")]
impl DjinnMcpServer {}
