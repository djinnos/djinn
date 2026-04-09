---
title: Split oversized test files by scenario module — Roadmap
type: requirement
tags: ["epic-0l5s","testing","decomposition","roadmap"]
---

# Split oversized test files by scenario module — Roadmap

## Goal
Mechanically split the oversized Rust test files in this epic into sibling scenario modules without changing behavior, production code, or effective test names. The split should preserve each file's existing module path shape so `cargo test <name>` invocations and CI history remain stable.

## Current targets
- `server/crates/djinn-db/src/repositories/note/tests.rs` — currently 3606 lines in this checkout; natural scenario clusters are separated by helper setup at the top and comment-grouped sections such as wikilink graph, session-scoped consolidation, and scope/heal-on-edit regressions.
- `server/src/mcp_contract_tests.rs` — 2810 lines; already partitioned into top-level modules (`board_tools`, `execution_tools`, `credential_tools`, `memory_tools`, `project_tools`, `settings_tools`, `system_tools`, `task_tools`, `session_tools`, `provider_tools`), which are the natural split units.
- `server/crates/djinn-agent/src/extension/tests.rs` — 2805 lines; only `fuzzy_replace_tests` is an explicit nested module today, but the file is clustered by tool boundary and scenario families (`call_lsp_*`, `lsp_tool_boundary_*`, memory dispatch, schema snapshots, `code_graph_*`, epic extension handlers). The split should introduce scenario modules while keeping externally visible test paths stable under `extension::tests::<scenario>::...`.
- `server/src/db/task_tests.rs` — 1981 lines; stretch target. It has clear comment-delimited scenario groups: existing CRUD/blockers tests, state machine tests, sync terminal-state tests, closed-task export/continuation tests, and rstest filter/count matrices.

## Wave 1
1. Split `server/src/mcp_contract_tests.rs` into `server/src/mcp_contract_tests/` with one file per existing top-level tool module plus a thin `mod.rs`/root shim.
2. Split `server/crates/djinn-agent/src/extension/tests.rs` into `server/crates/djinn-agent/src/extension/tests/` with scenario files grouped by current test families (fuzzy replace/LSP validation, tool dispatch/memory routing, schema snapshots and code graph, etc.).
3. Split `server/crates/djinn-db/src/repositories/note/tests.rs` into `server/crates/djinn-db/src/repositories/note/tests/` with files grouped by current scenario sections (CRUD/storage, search/ranking, wikilink graph, consolidation/housekeeping, session-scoped consolidation, scope/heal-on-edit regressions).
4. Stretch: split `server/src/db/task_tests.rs` into `server/src/db/task_tests/` with one scenario file per existing comment-delimited family.

## Task shaping guidance
- Keep the work mechanical: file moves, `mod` declarations, shared helper imports, and zero production-code behavior changes.
- Preserve existing module paths where a file already has nested modules; for `mcp_contract_tests.rs` this means each current top-level module becomes its own sibling file without renaming the module.
- For files that are mostly flat tests today, introduce scenario modules only as needed to keep filenames under the epic target while avoiding churn beyond the test tree.
- Prefer a root `tests.rs`/`mod.rs` shim that re-exports or declares sibling modules, so call sites and enclosing parent modules remain unchanged.
- Run targeted cargo tests for the touched crate/module plus any necessary full-suite smoke command to prove names and wiring remain intact.

## Done criteria for the epic
- No single targeted test source file remains over roughly 800 lines.
- No production code is changed.
- The affected test suites pass after the split.
- Test names/module paths remain stable enough that existing focused `cargo test` usage keeps working.

## Open note
`mcp_contract_tests.rs` is the cleanest first task because the current top-level modules already map 1:1 to files; `task_tests.rs` is stretch and lower priority if session budget is tight.
