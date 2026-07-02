import re
from pathlib import Path

ROOT = Path("server/crates/djinn-control-plane/src/tools/proposal_tools")


def main():
    create = (ROOT / "create.rs").read_text()
    mod = (ROOT / "mod.rs").read_text()

    # ── create.rs ───────────────────────────────────────────────────────────

    create = resolve_header_conflict(create)
    create = resolve_import_conflict(create)
    create = resolve_params_conflict(create)
    create = resolve_tool_methods_conflict(create)

    # ── mod.rs ──────────────────────────────────────────────────────────────

    mod = resolve_reexports_conflict(mod)
    mod = resolve_mod_import_conflict(mod)
    mod = resolve_mod_params_conflict(mod)
    mod = resolve_mod_tool_methods_conflict(mod)

    assert "<<<<<<< HEAD" not in create
    assert "=======" not in create
    assert ">>>>>>> origin/main" not in create
    assert "<<<<<<< HEAD" not in mod
    assert "=======" not in mod
    assert ">>>>>>> origin/main" not in mod

    (ROOT / "create.rs").write_text(create)
    (ROOT / "mod.rs").write_text(mod)
    print("Conflicts resolved.")


def between(text, start, end):
    s = text.find(start)
    if s == -1:
        raise RuntimeError(f"could not locate {start!r}")
    e = text.find(end, s + len(start))
    if e == -1:
        raise RuntimeError(f"could not locate {end!r} after {start!r}")
    return text[s:e + len(end)]


def replace_block(text, start, end, replacement):
    block = between(text, start, end)
    return text.replace(block, replacement, 1)


def resolve_header_conflict(text):
    return replace_block(
        text,
        "// Create/read/import/export/list/update/block-patch/delete CRUD tools for the\n",
        ">>>>>>> origin/main\n",
        """// Create/read/import/export/list/update/block-patch/delete CRUD tools for the
// global Proposals layer.
//
// This submodule owns the create/import/export/show/list/update/block-patch/
// delete mutation surface plus target add/remove and the cohesive list/show/
// target response shaping used by those tools.
//
// CRUD/target ownership checklist for task xpj0:
// - moved here: `proposal_add_target`, `proposal_remove_target`,
//   `target_models`, `finish_targets`, and `graduated_epic_models`;
// - already owned here: create/import/export/show/list tools and list-summary
//   tests; update/delete/block-patch moved here from the py7d sibling slice;
// - intentionally shared in `mod.rs`: composed gate/readiness helpers and
//   `err_single`/`err_show`/`err_targets` response constructors used by later
//   feedback, signoff, lifecycle, and refinement slices.
//
""",
    )


def resolve_import_conflict(text):
    return replace_block(
        text,
        "use crate::tools::proposal_ops::{\n",
        ">>>>>>> origin/main\n",
        """use crate::tools::proposal_ops::{
    ProposalDebateTrailModel, ProposalDeleteResponse, ProposalEpicModel, ProposalListSummary,
    ProposalModel, ProposalShowResponse, ProposalSignoffModel, ProposalSingleResponse,
    ProposalTargetModel, ProposalTargetsResponse,
};
""",
    )


def resolve_params_conflict(text):
    return replace_block(
        text,
        "#[derive(Deserialize, schemars::JsonSchema)]\n",
        ">>>>>>> origin/main\n",
        """#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalTargetParams {
    /// Proposal UUID or short_id.
    pub id: String,
    /// Target project: UUID or owner/repo slug (must be registered).
    pub project: String,
    /// `primary` (a write-target, default) or `reference` (read-only context).
    pub role: Option<String>,
}

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

// ── Tool router: create / import / export / show / list / update / block-patch / delete / target ──
""",
    )


def resolve_tool_methods_conflict(text):
    # The last conflict block in create.rs is the tool methods one.
    markers = [m.start() for m in re.finditer(r"<<<<<<< HEAD", text)]
    marker = markers[-1]
    end_marker = text.find(">>>>>>> origin/main", marker)
    if end_marker == -1:
        raise RuntimeError("no closing marker for create.rs tool methods")
    end = end_marker + len(">>>>>>> origin/main\n")

    block = text[marker:end]
    sep = block.find("=======\n")
    if sep == -1:
        raise RuntimeError("no separator in create.rs tool methods conflict")
    head = block[len("<<<<<<< HEAD\n"):sep]
    tail = block[sep + len("=======\n"):block.find(">>>>>>> origin/main")]

    # HEAD side is missing the closing `}` for proposal_remove_target.
    head = head.rstrip() + "\n    }\n"

    return text[:marker] + head + "\n" + tail + text[end:]


def resolve_reexports_conflict(text):
    return replace_block(
        text,
        "pub use create::{\n",
        ">>>>>>> origin/main\n",
        """pub use create::{
    ProposalCreateParams, ProposalDeleteParams, ProposalExportParams, ProposalImportParams,
    ProposalListParams, ProposalListResponse, ProposalShowParams, ProposalTargetParams,
    ProposalUpdateParams,
};
""",
    )


def resolve_mod_import_conflict(text):
    return replace_block(
        text,
        "use crate::tools::proposal_ops::{\n",
        ">>>>>>> origin/main\n",
        """use crate::tools::proposal_ops::{
    ProposalFeedbackResponse, ProposalModel, ProposalReconcileObsoleteEpicResponse,
    ProposalShowResponse, ProposalSingleResponse,
};
""",
    )


def resolve_mod_params_conflict(text):
    # Remove the entire update/delete/target params block from mod.rs.
    return replace_block(
        text,
        "#[derive(Deserialize, schemars::JsonSchema)]\n",
        ">>>>>>> origin/main\n",
        "#[derive(Deserialize, schemars::JsonSchema)]\npub struct ProposalFeedbackAddParams {\n",
    )


def resolve_mod_tool_methods_conflict(text):
    markers = [m.start() for m in re.finditer(r"<<<<<<< HEAD", text)]
    if len(markers) != 4:
        raise RuntimeError(f"expected 4 HEAD markers in mod.rs, found {len(markers)}")
    marker = markers[3]
    end_marker = text.find(">>>>>>> origin/main", marker)
    if end_marker == -1:
        raise RuntimeError("no closing marker for mod.rs tool methods")
    end = end_marker + len(">>>>>>> origin/main\n")
    return text[:marker] + text[end:]


if __name__ == "__main__":
    main()
