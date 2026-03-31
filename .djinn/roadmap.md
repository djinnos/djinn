---
title: ADR-043 Roadmap — Final Status
type: roadmap
tags: ["adr-043","repo-map","scip","worktree"]
---

# ADR-043: Monorepo-aware SCIP indexing, project-add trigger, and worktree reuse

## Status: Complete

All three original gaps identified in [[ADR-043 Repository Map SCIP-Powered Structural Context for Agent Sessions]] have been addressed.

## Delivered

### Wave 1 — Core Infrastructure
- Monorepo workspace discovery and per-workspace SCIP command planning (`server/src/repo_map.rs`)
- Project-created repo-map refresh scheduling via watcher/event-bus coordination
- Base-cache reuse across worktrees via canonical cache lookup
- Phase-1 worktree-reuse policy with diff-threshold planning

### Wave 2 — Hardening and Optimization
- **MCP contract proof** (`b5893416`): Contract test calls the real MCP `project_add` tool path and asserts refresh is scheduled through event-bus coordination. Uses `cfg(test)` observation channel in repo_map watcher.
- **Startup refresh** (`84530e73`): `startup_needs_refresh` guard checks registered projects for HEAD cache on boot. Schedules refresh on cache miss, skips on cache hit.
- **Persisted graph artifacts** (`036af280`): `RepoGraphArtifact` type captures per-file/per-symbol graph relationships. New `graph_artifact` column in `repo_map_cache` table. Backward compatible (NULL for old entries).
- **Small-diff graph patching** (`a31e2c04`): `patch_changed_files()` strips stale file/symbol nodes, re-parses only changed files, reruns ranking/rendering. Falls back to full reindex when artifact missing, diff exceeds threshold (20 files), or errors occur.

## Test Coverage
- Startup cache-miss scheduling + cache-hit skip
- MCP contract proof: project_add → event-bus → watcher refresh
- Graph artifact round-trip (serialize/deserialize/reconstruct)
- DB backward compat (NULL artifact on old entries)
- Small-diff patching + fallback paths (missing artifact, large diff, malformed JSON)

## No Remaining Gaps
All ADR-043 goals are met. Epic is ready for closure.

## Relations
- [[ADR-043 Repository Map SCIP-Powered Structural Context for Agent Sessions]]