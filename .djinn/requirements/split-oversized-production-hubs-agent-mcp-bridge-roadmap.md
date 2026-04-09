---
title: Split oversized production hubs (agent + mcp_bridge) — Roadmap
type: requirement
tags: ["roadmap","epic-zj2c","adr-028","facade-split"]
---

# Split oversized production hubs (agent + mcp_bridge) — Roadmap

## Goal
Make the remaining oversized production hubs navigable with facade-style splits that preserve existing call-site APIs. Follow [[decisions/adr-028-module-visibility-enforcement-and-deep-module-architecture]]: keep the original module path as the facade, move implementation detail into child modules, and avoid widening visibility or introducing cyclic re-exports.

## Constraints
- One file per task/sitting.
- No public API churn at call sites.
- Keep public types/functions reachable from the original module path via re-exports as needed.
- End each task with `cargo check --tests` clean.
- Watch PageRank/coupling stability: these are healthy hubs, so split for navigability rather than architectural churn.
- `server/src/mcp_bridge.rs` must wait until the canonical-graph shim-removal epic (`2jaz`) has landed, because that dead-code cleanup changes the target shape.
- `server/crates/djinn-agent/src/compaction.rs` is a high-PageRank hub; preserve its external surface and prefer internal helper extraction over signature churn.

## Wave 1
Create one worker task for each immediately-actionable hub in `djinn-agent`:
1. `server/crates/djinn-agent/src/extension/handlers.rs` — split by handler family behind the same facade module.
2. `server/crates/djinn-agent/src/actors/slot/reply_loop.rs` — extract streaming/tool-result decode helpers behind the same facade module.
3. `server/crates/djinn-agent/src/compaction.rs` — separate summarizer helpers from selection-policy logic with extra public-surface review.
4. `server/crates/djinn-agent/src/actors/slot/lifecycle.rs` — extract cleanup/teardown flows behind the same facade module.
5. `server/crates/djinn-agent/src/actors/coordinator/mod.rs` — continue pushing logic into existing submodules while preserving `CoordinatorHandle` and public re-exports.

These can run independently because they target distinct files. Reviewers should still check for facade-path stability and avoid accidental visibility widening.

## Deferred to Wave 2
6. `server/src/mcp_bridge.rs` — after epic `2jaz` lands, extract graph-neighbor helper families (including `group_neighbors_by_file` and siblings) into a child module while keeping the current facade path stable.

## Per-task acceptance shape
Each worker task should:
- reduce the target file to roughly the epic comfort zone (target under ~1200 lines where feasible for that file);
- keep public entry points available from the original module path;
- avoid broad call-site edits outside the target module except for internal module wiring;
- pass `cargo check --tests` in `server/`.

## Notes for execution
- Prefer `mod helpers;` / `mod streaming;` / `mod teardown;` style internal modules over creating new top-level APIs.
- Move tests with the extracted logic when that reduces churn; otherwise keep facade-level integration tests proving the public path is unchanged.
- For `coordinator/mod.rs`, continue the existing pattern already present in `actor`, `dispatch`, `handle`, `health`, `messages`, `prompt_eval`, `reentrance`, `rules`, `types`, and `wave`.
- For `compaction.rs`, explicitly verify that downstream imports still come from `crate::compaction` after the split.



## Wave 2 completion (2026-04-09)
- `server/src/mcp_bridge.rs` graph-neighbor helpers were split into `server/src/mcp_bridge/graph_neighbors.rs` behind the existing `mcp_bridge` facade, completing the deferred post-`2jaz` work.
- With `ofqc` closed, all planned hubs for this epic are now complete: `extension/handlers.rs`, `slot/reply_loop.rs`, `compaction.rs`, `slot/lifecycle.rs`, `coordinator/mod.rs`, and `mcp_bridge.rs`.
- Outcome: the epic goal is met. No further waves are planned for this roadmap.
