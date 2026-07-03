# proposal_tools split — final implementation PR checklist

Epic: 7yqy — Split proposal_tools.rs into concern-focused submodules

This document is the final ownership map for the `proposal_tools/` directory
after the full decomposition (subtasks b2vq, d6hy, ca70, py7d, xpj0, 5z0q, a2vb,
and this final cleanup nx7v). It maps every major function, tool method, type,
and test group to its owning module.

---

## 1. Module topology

```
tools/proposal_tools/
├── mod.rs                         # module entry, re-exports, shared helpers, router
├── mdx.rs                         # MDX/block-patch parsing + inline tests
├── create.rs                      # CRUD/import/export/show/list/update/delete/target tools
├── feedback.rs                    # feedback add/resolve tools
├── signoff.rs                     # signoff/clear + readiness/composed-gate helpers
├── lifecycle.rs                   # graduate/stop-build/reconcile/teardown tools
├── create_tests.rs                # CRUD concern tests (included from create.rs)
├── signoff_tests.rs               # signoff concern tests (included from signoff.rs)
├── tribunal_tests.rs              # debate/tribunal regression tests (included from signoff.rs)
├── graduation_readiness_tests.rs  # lifecycle graduation tests (included from lifecycle.rs)
└── end_to_end_planner_tests.rs    # cross-cutting planner regressions (included from mod.rs)
```

The residual monolith `tools/proposal_tools.rs` is **absent**. The stable
top-level path `crate::tools::proposal_tools` is preserved via
`tools/mod.rs: pub mod proposal_tools;` and the `pub use` re-exports in
`proposal_tools/mod.rs`.

---

## 2. Production ownership map

### mod.rs — shared helpers, re-exports, router

| Item | Kind | Visibility |
|---|---|---|
| `proposal_not_found_error(id)` | free function | `pub(super)` |
| `err_show(error)` | free function | `pub(super)` |
| `err_single(error)` | free function | `pub(super)` |
| `gate_proposal_edit(author_user_id)` | method on `DjinnMcpServer` | `pub(crate)` |
| `proposal_tool_router()` | method on `DjinnMcpServer` | `pub` |
| Re-exports: `ProposalCreateParams`, `ProposalDeleteParams`, `ProposalExportParams`, `ProposalImportParams`, `ProposalListParams`, `ProposalListResponse`, `ProposalShowParams`, `ProposalTargetParams`, `ProposalUpdateParams`, `ProposalFeedbackAddParams`, `ProposalFeedbackResolveParams`, `ProposalGraduateParams`, `ProposalReconcileObsoleteEpicParams`, `ProposalStopBuildParams`, `ProposalStopBuildResponse`, `ProposalSignoffParams`, `BlockPatchOutcome`, `BlockPatchSelector`, `ByteRangeSelector`, `ProposalBlockPatchParams`, `apply_block_patch` | `pub use` | stable public surface |
| Super re-exports: `build_gate_status`, `evaluate_composed_gate`, `format_readiness_error`, `parse_ac_items` | `pub(super) use` | consumed by `create.rs` and `lifecycle.rs` via `super::*` |

### mdx.rs — MDX/block-patch parsing helpers

| Item | Kind | Visibility |
|---|---|---|
| `ImportedProposalMdx` | struct | `pub(super)` |
| `ProposalMdxFrontmatter` | struct | `pub(super)` |
| `split_proposal_mdx_frontmatter(mdx)` | free function | `pub(super)` |
| `parse_proposal_mdx(raw_mdx)` | free function | `pub(super)` |
| `BlockPatchSelector` | struct (enum) | `pub` |
| `ByteRangeSelector` | struct | `pub` |
| `ResolvedRange` | struct | `pub(super)` |
| `resolve_selector(body, selector)` | free function | `pub(super)` |
| `resolve_heading_selector(...)` | free function | `pub(super)` |
| `resolve_exact_text_selector(...)` | free function | `pub(super)` |
| `resolve_byte_range_selector(...)` | free function | `pub(super)` |
| `ProposalBlockPatchParams` | struct | `pub` |
| `BlockPatchOutcome` | struct | `pub` |
| `apply_block_patch(...)` | free function | `pub` |

### create.rs — CRUD/import/export/show/list/update/delete/target tools

| Item | Kind | Visibility |
|---|---|---|
| `target_models(...)` | free function | `pub(super)` |
| `graduated_epic_models(...)` | free function | `pub(super)` |
| `err_targets(error)` | free function | `pub(super)` |
| `finish_targets(...)` | free function | `pub(super)` |
| `ProposalListResponse` | struct | `pub` |
| `proposal_status_is_non_terminal(status)` | free function | `pub(super)` |
| `judge_verdict_is_needs_work(...)` | free function | `pub(super)` |
| `build_list_summary(...)` | free function | `pub(super)` |
| `ProposalCreateParams` | struct | `pub` |
| `ProposalImportParams` | struct | `pub` |
| `ProposalExportParams` | struct | `pub` |
| `ProposalShowParams` | struct | `pub` |
| `ProposalListParams` | struct | `pub` |
| `ProposalTargetParams` | struct | `pub` |
| `ProposalUpdateParams` | struct | `pub` |
| `ProposalDeleteParams` | struct | `pub` |
| `proposal_create(...)` | `#[tool]` method | `pub` |
| `proposal_import(...)` | `#[tool]` method | `pub` |
| `proposal_export(...)` | `#[tool]` method | `pub` |
| `proposal_show(...)` | `#[tool]` method | `pub` |
| `proposal_list(...)` | `#[tool]` method | `pub` |
| `proposal_add_target(...)` | `#[tool]` method | `pub` |
| `proposal_remove_target(...)` | `#[tool]` method | `pub` |
| `proposal_update(...)` | `#[tool]` method | `pub` |
| `proposal_block_patch(...)` | `#[tool]` method | `pub` |
| `proposal_delete(...)` | `#[tool]` method | `pub` |
| `proposal_create_tool_router()` | method | `pub` |

### feedback.rs — feedback add/resolve tools

| Item | Kind | Visibility |
|---|---|---|
| `ProposalFeedbackAddParams` | struct | `pub` |
| `ProposalFeedbackResolveParams` | struct | `pub` |
| `err_feedback(error)` | free function | `pub(super)` |
| `proposal_feedback_add(...)` | `#[tool]` method | `pub` |
| `proposal_feedback_resolve(...)` | `#[tool]` method | `pub` |
| `proposal_feedback_tool_router()` | method | `pub` |

### signoff.rs — signoff/clear + readiness/composed-gate helpers + debate gate

| Item | Kind | Visibility |
|---|---|---|
| `ProposalSignoffParams` | struct | `pub` |
| `ComposedGateResult` | struct | `pub(super)` |
| `parse_ac_items(...)` | free function | `pub(super)` |
| `format_readiness_error(...)` | free function | `pub(super)` |
| `current_explicit_verdict_override(...)` | free function | `pub(super)` |
| `current_human_accept_authority(...)` | free function | `pub(super)` |
| `current_human_gate_authority(...)` | free function | `pub(super)` |
| `evaluate_composed_gate(...)` | free function | `pub(super)` |
| `revision_metadata_is_human_accept(...)` | free function | `pub(super)` |
| `build_gate_status(...)` | free function | `pub(super)` |
| `proposal_signoff(...)` | `#[tool]` method | `pub` |
| `proposal_signoff_clear(...)` | `#[tool]` method | `pub` |
| `proposal_signoff_tool_router()` | method | `pub` |

### lifecycle.rs — graduate/stop-build/reconcile/teardown tools

| Item | Kind | Visibility |
|---|---|---|
| `ProposalGraduateParams` | struct | `pub` |
| `ProposalStopBuildParams` | struct | `pub` |
| `ProposalReconcileObsoleteEpicParams` | struct | `pub` |
| `ProposalStopBuildResponse` | struct | `pub` |
| `proposal_graduate(...)` | `#[tool]` method | `pub` |
| `proposal_stop_build(...)` | `#[tool]` method | `pub` |
| `proposal_reconcile_obsolete_epic(...)` | `#[tool]` method | `pub` |
| `abort_proposal_build(...)` | method | `pub(super)` |
| `teardown_obsolete_proposal_epic(...)` | method | `pub(super)` |
| `proposal_lifecycle_tool_router()` | method | `pub` |

### Refinement/debate ownership decision

No standalone `refinement.rs` or `debate.rs` submodules exist. The production
glue is not cohesive outside its current owners:

1. **Debate-trail gate checks** are embedded in `signoff.rs`'s
   `evaluate_composed_gate` and `build_gate_status` — single-pass gate
   evaluation used by signoff, lifecycle (graduate), and create (update).
2. **Refinement status projection** is thin call-throughs to
   `crate::tools::refinement_tools::build_refinement_status` in `signoff.rs`
   and `create.rs` — consumers of an external module, not standalone glue.
3. **Block catalog** (`proposal_block_catalog`) has zero production references
   in `proposal_tools/` — only exercised in integration tests.
4. **Native-skill provenance** (`native_skill_name`/`native_skill_version`)
   lives as fields on `ProposalBlockPatchParams` in `mdx.rs` — block-patch
   infrastructure, not native-skill glue.

---

## 3. Test ownership map

| Test file | Included from | Concern | Key tests |
|---|---|---|---|
| `create_tests.rs` | `create.rs` | CRUD / targets / list summary / schema | `proposal_list_surfaces_tribunal_and_gate_summary`, `proposal_list_omits_summary_for_terminal_proposals`, `proposal_create_params_schema_is_lean_and_excludes_block_vocabulary`, `proposal_update_params_schema_is_lean_and_excludes_block_vocabulary` |
| `mdx.rs` inline `import_tests` | `mdx.rs` | MDX import parsing | `proposal_import_creates_valid_mdx_and_preserves_fields`, `proposal_import_rejects_unknown_block_with_tag_name`, `proposal_import_updates_when_id_is_present`, `proposal_import_without_frontmatter_defaults_to_plain_markdown` |
| `mdx.rs` inline `export_tests` | `mdx.rs` | MDX export rendering | `proposal_export_markdown_preserves_frontmatter_and_body`, `proposal_export_mdx_round_trips_through_block_parser`, `proposal_export_canonical_fixture_body_is_byte_identical`, `proposal_export_nonexistent_id_returns_error` |
| `mdx.rs` inline `block_patch_tests` | `mdx.rs` | Block-patch selector/application | `block_patch_replace_by_exact_text_preserves_unrelated_content`, `block_patch_increments_revision_seq_once`, `block_patch_rejects_stale_expected_revision`, `block_patch_rejects_missing_selector`, `block_patch_rejects_ambiguous_exact_text`, `block_patch_heading_selector_replaces_section`, `block_patch_wrap_preserves_selected_content_and_inserts_before`, `block_patch_byte_range_selector`, `block_patch_byte_range_rejects_stale_text`, `block_patch_records_event_metadata` |
| `mdx.rs` inline `block_patch_regression_tests` | `mdx.rs` | Block-patch end-to-end regressions | `regression_two_patches_increment_latest_revision_seq_exactly_twice`, `regression_unrelated_body_content_preserved_across_patches`, `regression_revisions_expose_targeted_block_patch_metadata_with_skill_attribution`, `regression_block_patches_then_export_round_trips_cleanly`, `regression_bare_angle_bracket_guidance_is_backticked` |
| `feedback.rs` | — | Feedback | No dedicated tests; behavior covered by end-to-end planner tests |
| `signoff_tests.rs` | `signoff.rs` | Signoff / readiness | `draft_incomplete_proposal_fails_signoff_and_remains_draft`, `draft_complete_proposal_accepts_signoff_and_advances`, `needs_work_verdict_blocks_signoff_with_deterministic_message`, `superseded_reject_verdicts_do_not_block_gate`, `latest_reject_verdict_still_blocks_via_needs_work_channel` |
| `tribunal_tests.rs` | `signoff.rs` | Debate / tribunal / spike | `draft_to_in_review_blocked_by_dor_failures`, `draft_to_in_review_blocked_by_needs_work_verdict`, `needs_evidence_spike_parking_resume_and_graduation`, `graduation_succeeds_with_human_verdict_override`, `spike_finding_visible_in_proposal_show_debate_trail`, `export_roundtrip_after_refinement_revision` |
| `graduation_readiness_tests.rs` | `lifecycle.rs` | Graduation readiness | `approved_missing_sections_fails_graduation_with_check_names`, `complete_approved_proposal_graduates_and_creates_breakdown_task`, `non_approved_proposal_fails_with_status_guardrail`, `no_primary_target_fails_with_target_guardrail`, `readiness_error_format_is_consistent_across_lifecycle_gates` |
| `lifecycle.rs` inline `stop_build_tests` | `lifecycle.rs` | Stop-build / teardown | `building_proposal`, `freeze_and_unfreeze_toggle_the_flag`, `preview_reports_blast_radius_without_mutating`, `scoped_teardown_preview_reports_blast_radius_without_mutating`, `scoped_teardown_closes_only_target_epic_and_preserves_build`, `scoped_teardown_blocks_merged_work_before_preview_or_mutation`, `abort_tears_down_and_reverts_to_approved` |
| `end_to_end_planner_tests.rs` | `mod.rs` | Cross-cutting planner/refinement | `planner_authoring_session_resolves_visual_spec_from_native_registry`, `non_authoring_sessions_do_not_receive_visual_spec`, `get_block_catalog_pull_surface_returns_lean_vocabulary`, `refinement_loop_increments_revision_seq_once_per_targeted_patch`, `refinement_loop_revisions_carry_visual_spec_attribution`, `refinement_loop_enriched_proposal_exports_as_valid_mdx`, `refinement_loop_workflow_surfaces_remain_lazy`, `refinement_loop_end_to_end_ties_all_y4td_surfaces_together` |

---

## 4. Production module size audit

All production modules are well under the ~1,500 nloc soft target:

| Module | Total lines | Production nloc (non-blank, non-comment) | Status |
|---|---|---|---|
| `mod.rs` | 180 | 72 | ✅ under threshold |
| `feedback.rs` | 127 | 97 | ✅ under threshold |
| `signoff.rs` | 556 | 413 | ✅ under threshold |
| `mdx.rs` | 1802 | 343 | ✅ under threshold (production only) |
| `lifecycle.rs` | 1180 | 590 | ✅ under threshold |
| `create.rs` | 1122 | 906 | ✅ under threshold |

**mdx.rs total-line note (1802 lines):** The file exceeds 1,500 total lines,
but production nloc is only 343 — the remainder is 1,346 lines of inline test
code (`import_tests`, `export_tests`, `block_patch_tests`,
`block_patch_regression_tests`) that was moved with the owning concern per the
epic's test-relocation requirement. Splitting test code into a separate file
would not reduce production nloc and would fragment a cohesive parser/selector
test suite. No production module requires an exception; the total-line
oversize is purely test code and is cohesion-driven.

---

## 5. Public surface compatibility

The stable public module path `crate::tools::proposal_tools::{...}` (and the
re-exported `djinn_control_plane::tools::proposal_tools::{...}` for the
`djinn-mcp-extension` crate) is preserved by the `pub use` re-exports in
`mod.rs`. Verified consumers:

- `dispatch.rs`: imports `ProposalCreateParams`, `ProposalDeleteParams`,
  `ProposalExportParams`, `ProposalImportParams`, `ProposalListParams`,
  `ProposalListResponse`, `ProposalShowParams`, `ProposalSignoffParams`,
  `ProposalStopBuildParams`, `ProposalTargetParams`, `ProposalUpdateParams`,
  `ProposalBlockPatchParams`.
- `djinn-mcp-extension/src/handlers/task_epic.rs`: imports
  `ProposalBlockPatchParams`, `ProposalUpdateParams`, `apply_block_patch`.

Both crates compile cleanly (`cargo clippy -p djinn-control-plane` and
`cargo clippy -p djinn-mcp-extension` pass with no warnings).

---

## 6. Verification commands

```
test ! -f server/crates/djinn-control-plane/src/tools/proposal_tools.rs   # PASS — absent
grep -n "pub mod proposal_tools" server/crates/djinn-control-plane/src/tools/mod.rs  # line 24
wc -l server/crates/djinn-control-plane/src/tools/proposal_tools/*.rs | sort -n  # see size table above
cargo clippy -p djinn-control-plane  # PASS — no warnings
cargo clippy -p djinn-mcp-extension  # PASS — no warnings
```

No MCP tool names, schemas, signatures, responses, database schema, or
behavioral expectations were changed by this split.
