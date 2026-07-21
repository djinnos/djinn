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
// Submodule layout:
// - `create.rs` / `create_tests.rs`: CRUD/import/export/show/list/target tools
// - `feedback.rs`: feedback add/resolve tools
// - `signoff.rs` / `signoff_tests.rs` / `tribunal_tests.rs`: signoff/clear tools,
//   readiness/composed-gate helpers (including debate-trail gate logic), and
//   tribunal regression tests
// - `lifecycle.rs` / `graduation_readiness_tests.rs`: graduate, stop-build,
//   reconcile, build-teardown helpers and graduation readiness tests
// - `mdx.rs`: MDX/block-patch parsing helpers (including native-skill
//   provenance fields on block-patch params)
// - mod.rs / `end_to_end_planner_tests.rs`: shared response/error constructors,
//   permission gates, router composition, and cross-cutting end-to-end planner
//   regressions
//
// Refinement-loop / debate-trail ownership decision (subtask 5z0q):
//
// No standalone `refinement.rs` or `debate.rs` submodules are created because
// the production glue is not cohesive outside its current owners:
//
// 1. **Debate-trail gate checks** are embedded in `signoff.rs`'s
//    `evaluate_composed_gate` and `build_gate_status`. They are evaluated as
//    part of a single pass over DoR + tribunal conditions — extracting them
//    would fragment a single-pass gate evaluation into multiple modules with
//    no standalone meaning. The composed gate is the single call-through point
//    used by signoff, lifecycle (graduate), and create (update).
//
// 2. **Refinement status projection** consists of thin call-throughs to
//    `crate::tools::refinement_tools::build_refinement_status` in two places:
//    `signoff.rs` (adversary dry count in `build_gate_status`) and `create.rs`
//    (refinement status in `proposal_show`). These are consumers of an
//    external module, not standalone glue.
//
// 3. **Block catalog** (`proposal_block_catalog`) has zero production
//    references in `proposal_tools/` — it is only exercised in integration
//    tests (`end_to_end_planner_tests.rs`).
//
// 4. **Native-skill provenance** (`native_skill_name`/`native_skill_version`)
//    lives as fields on `ProposalBlockPatchParams` in `mdx.rs` — they are
//    block-patch infrastructure, not native-skill glue.

mod create;
pub(crate) mod feedback;
mod lifecycle;
mod mdx;
mod params;
pub(crate) mod signoff;

// Re-export CRUD tool parameter types from `params.rs` so the public module path
// `crate::tools::proposal_tools::{...}` stays stable for existing dispatch and
// MCP-extension consumers.
pub use params::{
    ProposalCreateParams, ProposalDeleteParams, ProposalExportParams, ProposalImportParams,
    ProposalListParams, ProposalShowParams, ProposalTargetParams, ProposalUpdateParams,
};

// Re-export the list response type (still defined in `create.rs`).
pub use create::ProposalListResponse;

// Re-export feedback parameter types so `crate::tools::proposal_tools::...`
// stays stable for dispatch.rs and MCP-extension consumers.
pub use feedback::{ProposalFeedbackAddParams, ProposalFeedbackResolveParams};

// Re-export lifecycle parameter/response types so `crate::tools::proposal_tools::...`
// stays stable for dispatch.rs and MCP-extension consumers.
pub use lifecycle::{
    ProposalGraduateParams, ProposalReconcileObsoleteEpicParams, ProposalStopBuildParams,
    ProposalStopBuildResponse,
};

// Re-export signoff parameter types so `crate::tools::proposal_tools::...`
// stays stable for dispatch.rs and MCP-extension consumers.
pub use signoff::ProposalSignoffParams;

// Re-export shared readiness/gate helpers from `signoff.rs` so `create.rs`
// and `lifecycle.rs` can continue importing from `super::*`.
pub(super) use signoff::{
    build_gate_status, evaluate_composed_gate, format_readiness_error, parse_ac_items,
};

// Re-export MDX/block-patch types so the public module path
// `crate::tools::proposal_tools::{...}` stays stable for existing dispatch
// and MCP-extension consumers.
pub use mdx::{
    BlockPatchOutcome, BlockPatchSelector, ByteRangeSelector, ProposalBlockPatchParams,
    apply_block_patch,
};

// Imports consumed by child-module test code via `use super::*`; keep
// them even if the non-test lib build does not reference them directly.
#[allow(unused_imports)]
use rmcp::handler::server::wrapper::Parameters;

use crate::server::DjinnMcpServer;
use crate::tools::acting_user::acting_caps;
use crate::tools::proposal_ops::{
    ProposalLintRejectionViolation, ProposalLintViolationSpan, ProposalShowResponse,
    ProposalSingleResponse,
};
#[allow(unused_imports)]
use djinn_db::{EpicRepository, ProjectRepository, ProposalRepository, TaskRepository};

pub(super) fn proposal_not_found_error(id: &str) -> String {
    format!("proposal not found: {id}")
}

// ── Permission gates ─────────────────────────────────────────────────────────

impl DjinnMcpServer {
    /// Gate a direct spec edit: allowed for the author, a PM, an engineer, or
    /// an admin. `Ok(())` when unauthenticated (trusted/system path).
    pub(crate) async fn gate_proposal_edit(
        &self,
        author_user_id: Option<&str>,
    ) -> Result<(), String> {
        if let Some(caps) = acting_caps(self.state.db()).await? {
            let is_author = author_user_id == Some(caps.user_id.as_str());
            if !caps.can_edit(is_author) {
                return Err(
                    "editing this proposal requires its author, a PM, an engineer, or an admin"
                        .to_string(),
                );
            }
        }
        Ok(())
    }
}

// ── Small response constructors (shared with `create.rs` via `pub(super)`) ───

pub(super) fn err_show(error: impl Into<String>) -> ProposalShowResponse {
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

pub(super) fn err_single(error: impl Into<String>) -> ProposalSingleResponse {
    ProposalSingleResponse {
        proposal: None,
        mdx: None,
        error: Some(error.into()),
        code: None,
        violations: None,
        latest_lint: None,
    }
}

/// Keep the legacy `error` string while retaining repository lint diagnostics.
pub(super) fn proposal_mutation_error(error: djinn_db::Error) -> ProposalSingleResponse {
    match error {
        djinn_db::Error::SpecLintRejected(rejection) => {
            let mut violations: Vec<_> = rejection
                .violations
                .into_iter()
                .map(|violation| ProposalLintRejectionViolation {
                    code: violation.code,
                    message: violation.message,
                    severity: "error".to_string(),
                    span: ProposalLintViolationSpan {
                        start_byte: violation.span_start,
                        end_byte: violation.span_end,
                    },
                })
                .collect();
            violations.sort_by(|a, b| {
                a.span
                    .start_byte
                    .cmp(&b.span.start_byte)
                    .then(a.span.end_byte.cmp(&b.span.end_byte))
                    .then(a.code.cmp(&b.code))
            });
            ProposalSingleResponse {
                proposal: None,
                mdx: None,
                error: Some(rejection.code.clone()),
                code: Some(rejection.code),
                violations: Some(violations),
                latest_lint: None,
            }
        }
        other => err_single(other.to_string()),
    }
}

// ── Router composition ────────────────────────────────────────────────────────

#[allow(clippy::items_after_test_module)]
impl DjinnMcpServer {
    /// Composite router for all proposal tools (CRUD/targets + feedback +
    /// signoff + lifecycle tools).
    /// Combines the create/import/export/show/list/target router from `create.rs`,
    /// the feedback router from `feedback.rs`, the signoff router from `signoff.rs`,
    /// and the graduate/stop-build/reconcile router from `lifecycle.rs`.
    pub fn proposal_tool_router() -> rmcp::handler::server::router::tool::ToolRouter<Self> {
        Self::proposal_create_tool_router()
            + Self::proposal_feedback_tool_router()
            + Self::proposal_signoff_tool_router()
            + Self::proposal_lifecycle_tool_router()
    }
}

// End-to-end planner refinement loop tests — extracted to
// `end_to_end_planner_tests.rs` to meet the 1500-line file-size guard.
// These are the only remaining cross-cutting regressions in `proposal_tools/`;
// they span create, update, signoff, and lifecycle tools.
#[cfg(test)]
include!("end_to_end_planner_tests.rs");
