---
title: ADR-057 Roadmap — FUSE-Mounted Memory
type: design
tags: ["adr-057","roadmap","memory","filesystem","fuse"]
---





# ADR-057 Roadmap — FUSE-Mounted Memory

## Status
Epic `tcet` remains open, but the remaining work is now a single narrow landing slice. The filesystem foundation and runtime slices are landed: the transport-neutral memory filesystem core, repository-backed read/list/stat behavior, write-through mutations, Linux FUSE mount plumbing, integration hardening, branch-aware view selection, session-aware runtime wiring, batching/flush-path coverage, filesystem-first CRUD contraction (`6lq5`), migration-boundary documentation (`hl1b`), and mount enablement/guardrails (`ski2`) are all closed.

The epic is not yet complete because the last runtime/status reporting slice did not land cleanly on main. Original task `u5qe` reached reviewer approval but accumulated repeated post-approval CI and merge-conflict churn, so it was reshaped into fresh replacement task `98vz` to re-land the same scoped outcome on a clean branch.

Remaining implementation work:
- `98vz` — land the mounted-memory `/health` view-selection payload, fallback reporting, and aligned runtime/router test coverage cleanly against current main

A narrow memory-hygiene companion task `lki7` is already closed and does not expand the implementation scope of the epic itself.

## Wave 3 task map
### Closed foundation and UX tasks
1. `6lq5` — contract agent-facing memory CRUD tool exposure for filesystem-first ADR-057 flows.
2. `hl1b` — document and verify the filesystem-first memory tool migration boundary.
3. `ski2` — add filesystem-first mount enablement and branch-aware usage guardrails.

### Final remaining landing slice
1. `98vz` — re-land mounted-memory view reporting and canonical-fallback visibility on `/health` after `u5qe` integration drift.

- Memory note persistence and indexing live under `server/crates/djinn-db/src/repositories/note/`.
- Memory MCP surface and dispatch live under `server/crates/djinn-mcp/src/tools/memory_tools/` and `server/crates/djinn-mcp/src/dispatch.rs`.
- Agent tool exposure and prompt surfaces live under `server/crates/djinn-agent/src/extension/` and `server/crates/djinn-agent/src/prompts/`.
- Mounted-memory runtime state and branch/view fallback resolution live under `server/src/memory_mount.rs` and `server/src/server/state/mod.rs`.
- Existing note capabilities already cover frontmatter parsing, wikilink graph maintenance, access tracking (`touch_accessed`), and note associations in the SQLite-backed repository.

## Decomposition strategy
ADR-057 should now continue as a final completion wave after the shipped wave-1 and landed wave-2 foundation.

### Wave 3 — Filesystem-first agent UX completion
Goal: complete the transition from CRUD-heavy MCP habits to an explicit filesystem-first agent experience without dropping the analytical MCP capabilities the ADR keeps.

1. Land the active contraction task `6lq5`, which reduces worker/planner/reviewer CRUD-oriented memory tool exposure and updates prompts/tool schemas toward filesystem-first note operations.
2. Expose the mounted-memory branch/view state and fallback semantics clearly in runtime/status surfaces so agents and operators can tell whether they are on the canonical view or a task-scoped worktree view.
3. Document and test the migration boundary between retained analytical memory tools (`memory_build_context`, `memory_health`, `memory_graph`, `memory_associations`, `memory_confirm`) and deprecated/reduced CRUD-oriented flows so future prompt/tool changes stay aligned with ADR-057.
4. Tighten filesystem-first guidance around mount enablement, usage expectations, and branch-aware guardrails so the mounted path is discoverable and explicit once agents lose broad CRUD MCP affordances.

## Evidence from landed work
- `server/src/server/state/mod.rs` now resolves memory mount view selection from active task/session worktree context and logs canonical fallbacks.
- `server/src/memory_mount.rs` implements Linux memory-mount plumbing, debounced queued writes, runtime status, and health-facing state.
- `server/crates/djinn-agent/src/extension/tool_defs.rs` still exposes CRUD-oriented memory tools to worker and planner roles today, confirming the filesystem-first contraction is not yet fully landed until `6lq5` completes.
- `server/crates/djinn-mcp/src/dispatch.rs` still retains the broad CRUD memory MCP surface, so migration/deprecation boundaries must be documented even if role-level exposure contracts first.

## Wave 3 task map
### Active
1. `6lq5` — contract agent-facing memory CRUD tool exposure for filesystem-first ADR-057 flows.

### Next completion slice
1. Add mounted-memory view reporting and fallback visibility to runtime/status surfaces.
2. Document filesystem-first migration boundaries for retained analytical memory MCP tools versus deprecated CRUD flows.
3. Add operator/agent-facing branch UX guardrails and enablement guidance for the mounted memory surface.

## Notes for workers
- Reuse existing note repository behavior instead of creating a second note model.
- Preserve current `memory_read` semantics by ensuring filesystem reads also trigger access tracking.
- Treat the mount transport as an adapter over a transport-neutral in-memory service so NFS/macOS fallback can be added later without redoing note semantics.
- Keep MCP analytical tools (`memory_build_context`, `memory_health`, `memory_graph`, `memory_associations`, `memory_confirm`) intact while filesystem-first CRUD guidance is tightened.
- Prefer incremental contraction and explicit documentation over silently removing runtime capabilities that still back existing compatibility surfaces.

## Relations
- [[decisions/adr-057-proposal-fuse-mounted-memory-filesystem-as-the-primary-agent-interface]]
- [[decisions/adr-055-proposal-dolt-migration-and-per-task-knowledge-branching]]
- [[decisions/adr-054-proposal-memory-artifact-hygiene-and-proactive-knowledge-curation]]
- [[decisions/adr-053-semantic-memory-search-candle-embeddings-with-sqlite-vec]]


## Completion update (2026-04-14 final close)
Replacement task `98vz` landed the last remaining runtime/status slice after the earlier `u5qe` reshape. With that re-landing closed, ADR-057 is complete: mounted-memory `/health` now reports canonical vs task-scoped view selection plus fallback context, and the previously planned filesystem-first contraction, migration-boundary documentation, guardrails, batching coverage, and branch-aware runtime work are all closed.

No further worker wave is planned for epic `tcet`. Any future changes should be treated as new follow-up work rather than continuation of this roadmap.
