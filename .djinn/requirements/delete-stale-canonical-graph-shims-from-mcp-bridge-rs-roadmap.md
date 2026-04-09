---
title: Delete stale canonical-graph shims from mcp_bridge.rs — Roadmap
type: requirement
tags: ["roadmap","canonical-graph","mcp-bridge","dead-code-removal"]
---

# Delete stale canonical-graph shims from mcp_bridge.rs — Roadmap

## Goal
Make `server/src/canonical_graph.rs` the single source of truth for canonical-graph cache/build/warm helpers by removing the dead duplicate/shim block from `server/src/mcp_bridge.rs`.

## Wave 1
1. Repoint the remaining shim-mediated call path in `server/src/server/state/mod.rs` to `crate::canonical_graph::canonical_graph_count_commits_since` directly.
2. Verify no production callers still depend on the duplicated helper block in `server/src/mcp_bridge.rs`; preserve only the cache-introspection helpers that `server/state/mod.rs` and chat still use.
3. Delete the stale shim/duplicate block from `server/src/mcp_bridge.rs`, including the duplicate `GRAPH_NOT_WARMED_ERR`, forwarders, copied helper implementations, and mcp_bridge-local tests that only exercise that duplicated seam.
4. Run `cargo check --tests` to confirm the canonical module still owns the behavior without regressions.

## Task slicing
- **Task 1:** repoint remaining callers away from the shim seam and remove now-unused public wrapper exposure.
- **Task 2:** delete the dead canonical-graph block/tests from `mcp_bridge.rs` and make the suite pass against `canonical_graph.rs` only.

## Notes
- The in-file runtime call sites at `mcp_bridge.rs:397` and `:579` already call `crate::canonical_graph::build_graph_with_caches_for_project` directly; the remaining live external caller is `server/src/server/state/mod.rs:559`.
- `server/src/canonical_graph.rs` already contains the canonical implementations plus equivalent cache-hit / stale-blob / cache-only-reader tests, so the mcp_bridge-local duplicates should be removed rather than migrated.
- Keep `mcp_bridge` cache helpers that are still referenced externally (`canonical_graph_cache_has_entry_for`, `canonical_graph_cache_pinned_commit_for`, and `ensure_canonical_graph` for chat warm paths) unless a follow-up epic moves those seams too.

## Done criteria
- No remaining production use of `crate::mcp_bridge::canonical_graph_count_commits_since`.
- The stale `#[allow(dead_code)]` canonical-graph region is removed from `server/src/mcp_bridge.rs`.
- `cargo check --tests` passes with `canonical_graph.rs` as the only canonical-graph implementation seam.

## Relations
- [[decisions/adr-051-planner-as-patrol-and-architect-as-consultant]]


## Wave 2
- `wpjx` completed: `server/src/server/state/mod.rs` now calls `crate::canonical_graph::canonical_graph_count_commits_since` directly and the old `mcp_bridge` wrapper is gone.
- `4w76` completed: the stale canonical-graph shim block and duplicate tests were removed from `server/src/mcp_bridge.rs`, leaving `server/src/canonical_graph.rs` as the single implementation seam.

## Status
Epic complete. Wave 1 roadmap items are all closed and the codebase now reflects the intended single-source-of-truth canonical-graph seam.
