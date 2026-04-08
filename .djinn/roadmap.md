---
title: ADR-043 Roadmap — Active Decomposition Status
type: roadmap
tags: ["adr-043","repo-map","scip","worktree"]
---

# ADR-043: Monorepo-aware SCIP indexing, project-add trigger, and worktree reuse

## Status: In Progress

Epic remains open. Core repo-map behavior exists, but the decomposition goal is still in progress: the implementation is still split across large cross-cutting modules and requires further seam extraction before this roadmap can claim completion.

## Current State

- Monorepo workspace discovery/indexer planning still lives primarily in `server/src/repo_map.rs`.
- Canonical graph build/cache/persist logic remains concentrated in `server/src/mcp_bridge.rs`.
- Canonical warm/staleness refresh policy remains concentrated in `server/src/server/state/mod.rs`.
- The epic goal is to extract these responsibilities into dedicated seams so repo-map and code-intelligence behavior are easier to reason about, test, and evolve.

## Completed Work So Far

### Wave 1 — Core Infrastructure
- Monorepo workspace discovery and per-workspace SCIP command planning (`server/src/repo_map.rs`)
- Project-created repo-map refresh scheduling via watcher/event-bus coordination
- Base-cache reuse across worktrees via canonical cache lookup
- Phase-1 worktree-reuse policy with diff-threshold planning

### Wave 2 — Follow-on hardening already landed
- MCP contract proof for `project_add` scheduling through event-bus coordination
- Startup refresh scheduling based on cache presence
- Persisted graph artifacts for repo-map cache entries
- Small-diff graph patching with fallback to full reindexing
- Initial repo-map indexing/workspace planning extraction via completed task `2g6z`

## Remaining Work

1. Extract canonical graph build/cache/persist responsibilities from `mcp_bridge.rs` into a dedicated service used by both repo-map refresh and `code_graph` flows.
2. Extract canonical warm/staleness refresh policy from `server/src/server/state/mod.rs` into a planner/orchestrator seam with thin state wiring.
3. Apply [[decisions/adr-028-module-visibility-enforcement-and-deep-module-architecture]] facade cleanup so only intentional repo-map/code-intelligence APIs stay public.

## Active Tasks

- `ekjj` — extract canonical graph build/cache service from `mcp_bridge.rs`
- `9qpj` — extract canonical graph refresh planner from server state wiring
- `7wsc` — apply ADR-028 facade cleanup to repo-map and code-intelligence seams

## Relations
- [[decisions/adr-043-repository-map-scip-powered-structural-context-for-agent-sessions]]
- [[brief]]
