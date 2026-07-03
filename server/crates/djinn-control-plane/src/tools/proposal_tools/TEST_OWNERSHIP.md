# Proposal tools test ownership and relocation checklist

## Current state

The production split into `proposal_tools/` is complete (see `mod.rs` header for the ownership decisions from subtask 5z0q).  This note records the final test placement after subtask a2vb (test relocation).

## Test ownership map

| Concern | Test file / module | What it covers | Why it lives there |
|---|---|---|---|
| CRUD / targets / list summary / schema | `create.rs` / `create_tests.rs` | `proposal_list` summary, `ProposalCreateParams`/`ProposalUpdateParams` schema-lean checks, `proposal_import` tool-level param validation | Directly tests create/update/list/router behavior of the CRUD concern. Import/export *tool method* param validation lives here (the tool methods are CRUD operations); the underlying MDX *parsing/rendering* helpers they call are tested in `mdx.rs`. |
| MDX / block-patch / import-export parser | `mdx.rs` inline `import_tests`, `export_tests`, `block_patch_tests`, `block_patch_regression_tests` | MDX frontmatter parsing, proposal spec rendering on export, selector resolution, block-patch application, and regression edge cases (selector not found, frontmatter-only edits, nested block boundaries) | These tests directly verify the MDX parser/selector/application helpers (`parse_proposal_mdx`, `split_proposal_mdx_frontmatter`, `resolve_selector`, `apply_block_patch`). They live in `mdx.rs` because the helpers are a cohesive parsing concern shared by import, export, and block-patch CRUD tools. |
| Feedback | `feedback.rs` | (none relocated yet; existing module is production-only) | Feedback tools are simple enough that their behavior is covered by the end-to-end planner tests; if future tests are added, they belong here. |
| Signoff / readiness / composed gate | `signoff.rs` / `signoff_tests.rs` | `proposal_signoff`, `proposal_signoff_clear`, gate-status formatting, human override, verdict override | These are signoff concern tests. |
| Debate / tribunal / spike | `signoff.rs` / `tribunal_tests.rs` | P4 tribunal regressions: composed-gate blocked transitions, needs-evidence spike parking/resume, human override, spike finding visibility, export round-trip | The production debate-trail gate logic lives in `signoff.rs` (`evaluate_composed_gate`), so the tribunal tests are paired with signoff. |
| Lifecycle / graduation | `lifecycle.rs` / `graduation_readiness_tests.rs` | `proposal_graduate` readiness guardrails, breakdown task creation, status guardrail ordering, readiness error format consistency | Directly tests graduation behavior owned by `lifecycle.rs`. |
| Lifecycle / stop-build / teardown | `lifecycle.rs` inline `stop_build_tests` | freeze/unfreeze, abort preview, scoped reconcile preview, scoped teardown, merged-work block, full abort cascade | Stop-build and reconcile are lifecycle tools; tests are colocated. |
| Cross-cutting planner/refinement | `mod.rs` includes `end_to_end_planner_tests.rs` | End-to-end planner loop regressions that span proposal create, update, signoff, graduation, and refinement status | These are genuinely cross-cutting: they exercise the router composition and the full proposal lifecycle. |

## What changed in a2vb

- Moved `graduation_readiness_tests.rs` from `mod.rs` inclusion into `lifecycle.rs` inclusion, because the tests verify `proposal_graduate` and its guardrails (lifecycle concern).
- Moved `tribunal_tests.rs` from `mod.rs` inclusion into `signoff.rs` inclusion, because the tribunal / debate gate is implemented in `signoff.rs`.
- Left `end_to_end_planner_tests.rs` included from `mod.rs` as the cross-cutting regression suite.
- Left `create_tests.rs` included from `create.rs` (already CRUD concern).
- Left inline `stop_build_tests` in `lifecycle.rs` (already lifecycle concern).
- Left inline `import_tests`, `export_tests`, `block_patch_tests`, and `block_patch_regression_tests` in `mdx.rs` (MDX/block-patch parsing helpers concern).
- No test assertions, fixtures, error strings, or response shapes were changed.

## Production module line-count note

`mdx.rs` remains ~1802 lines, which is above the 1500-line soft target.  It is retained as a single file because the MDX/block-patch helpers are a cohesive concern — `import_tests`, `export_tests`, `block_patch_tests`, and `block_patch_regression_tests` exercise `parse_proposal_mdx`, `split_proposal_mdx_frontmatter`, `resolve_selector`, and `apply_block_patch`, all of which share a single grammar.  Further splitting would fragment parser/selector/application helpers that must stay coherent.  A follow-up cleanup task should be considered if the project wants to hard-enforce the threshold.
