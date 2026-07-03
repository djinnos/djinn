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
// - `create.rs` / `create_tests.rs`: CRUD/import/export/list/target tools
// - `feedback.rs`: feedback add/resolve tools
// - `signoff.rs` / `signoff_tests.rs`: signoff/clear tools and readiness/
//   composed-gate helpers (including debate-trail gate logic)
// - `lifecycle.rs`: graduate, stop-build, reconcile, build-teardown helpers
// - `mdx.rs`: MDX/block-patch parsing helpers
// - mod.rs: shared response/error constructors, permission gates, and
//   router composition

mod create;
pub(crate) mod feedback;
mod lifecycle;
mod mdx;
pub(crate) mod signoff;

// Re-export CRUD tool parameter/response types so the public module path
// `crate::tools::proposal_tools::{...}` stays stable for existing dispatch and
// MCP-extension consumers.
pub use create::{
    ProposalCreateParams, ProposalDeleteParams, ProposalExportParams, ProposalImportParams,
    ProposalListParams, ProposalListResponse, ProposalShowParams, ProposalTargetParams,
    ProposalUpdateParams,
};

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
use crate::tools::proposal_ops::{ProposalShowResponse, ProposalSingleResponse};
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
    }
}

// End-to-end planner refinement loop tests — extracted to
// `end_to_end_planner_tests.rs` to meet the 1500-line file-size guard.
#[cfg(test)]
include!("end_to_end_planner_tests.rs");

// ── Router composition ────────────────────────────────────────────────────────

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
