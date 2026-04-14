---
title: ADR-057 Roadmap — FUSE-Mounted Memory
type: design
tags: ["adr-057","roadmap","memory","filesystem","fuse"]
---


# ADR-057 Roadmap — FUSE-Mounted Memory

## Status
Epic `tcet` remains open, but wave 1 is now landed in the codebase. The transport-neutral repository-backed filesystem core exists, Linux-only FUSE mount plumbing is wired behind the `memory-mount` feature/config gate, startup validates mount settings through `server/src/memory_mount.rs`, and integration coverage now exercises end-to-end read/list/create/update/rename/delete flows in `server/src/memory_fs/integration_tests.rs`.

What remains is the ADR-057 follow-on work that wave 1 explicitly left out: branch-aware mount selection, debounced write-through durability semantics at the mount/runtime layer, richer mount health/lifecycle reporting, and contraction of agent-facing CRUD memory tools once the filesystem path is trustworthy enough to become the primary interface.

## Current codebase anchors
- Memory note persistence and indexing live under `server/crates/djinn-db/src/repositories/note/`.
- Memory MCP surface lives under `server/crates/djinn-mcp/src/tools/memory_tools/`.
- Embedding runtime currently uses Candle in `server/src/semantic_memory.rs`.
- Server wiring and runtime state live under `server/src/server/` and `server/src/mcp_bridge.rs`.
- Existing note capabilities already cover frontmatter parsing, wikilink graph maintenance, access tracking (`touch_accessed`), and note associations in the SQLite-backed repository.

## Decomposition strategy
ADR-057 should continue in two follow-on waves after the shipped wave-1 foundation.

### Wave 2 — Branch-aware mount/runtime parity
Goal: make the mounted filesystem reflect the right knowledge branch and behave safely as a long-lived runtime surface.

1. Add a branch-selection seam beneath the filesystem core so reads and mutations can target a selected branch-aware knowledge view instead of the current single-project/default branch behavior.
2. Thread task/session context into mount startup and runtime so the mount can resolve the active memory branch for the running session, with clear fallback semantics when no task branch is active.
3. Add mount-runtime durability behavior that coalesces rapid write bursts instead of treating every kernel write/truncate callback as an independent repository mutation.
4. Expand coverage beyond the repository-core tests to include branch-aware mount behavior, mount health, and startup/runtime reporting.

### Wave 3 — Agent-facing contraction and branch UX
Goal: make the filesystem the default CRUD interface while preserving analytical MCP value.

- Reduce worker/planner/reviewer memory CRUD tool exposure so agents prefer filesystem reads/writes for note CRUD.
- Preserve analytical MCP tools such as `memory_build_context`, `memory_health`, graph/association surfaces, and confirmation flows.
- Decide and implement the user-visible branch UX (session-scoped mount view, explicit branch directories, or a hybrid) based on the branch-aware runtime added in wave 2.
- Document migration boundaries so existing MCP consumers retain compatibility while agent prompts/tool schemas shift toward filesystem-first usage.

### Evidence from landed wave 1
- `server/src/memory_mount.rs` documents branch-aware and multi-project mounting as intentionally out of scope for the first slice.
- `server/src/memory_fs/integration_tests.rs` explicitly calls out debounced write batching, branch-aware mount switching, and transport-specific adapter behavior as remaining gaps.
- `server/crates/djinn-agent/src/extension/tool_defs.rs` and `server/crates/djinn-mcp/src/dispatch.rs` still expose the full CRUD-oriented memory tool surface, confirming MCP contraction has not happened yet.

## Wave 1 task map
### Completed wave 1
1. `o6d9` — transport-neutral filesystem core scaffolding.
2. `mnqm` — repository-backed read/list/stat semantics with access tracking parity.
3. `8x6h` — write-through note mutations for create/update/rename/delete flows.
4. `c5dm` — Linux FUSE mount plumbing and startup/config validation behind feature gates.
5. `lxsq` — integration coverage for repository-backed filesystem semantics.

### Next wave task map
1. Add branch-aware branch/session selection beneath the memory filesystem core.
2. Integrate branch-aware session context into the Linux mount runtime and startup lifecycle.
3. Add debounced write-through batching plus mount health/runtime observability.
4. Contract agent-facing CRUD memory tool exposure once the filesystem runtime is branch-aware and operationally safe.

## Notes for workers
- Reuse existing note repository behavior instead of creating a second note model.
- Preserve current `memory_read` semantics by ensuring filesystem reads also trigger access tracking.
- Treat the mount transport as an adapter over a transport-neutral in-memory service so NFS/macOS fallback can be added later without redoing note semantics.
- Keep MCP analytical tools (`memory_build_context`, `memory_health`, `memory_graph`, `memory_associations`, `memory_confirm`) intact during this wave.

## Relations
- [[decisions/adr-057-proposal-fuse-mounted-memory-filesystem-as-the-primary-agent-interface]]
- [[decisions/adr-055-proposal-dolt-migration-and-per-task-knowledge-branching]]
- [[decisions/adr-054-proposal-memory-artifact-hygiene-and-proactive-knowledge-curation]]
- [[decisions/adr-053-semantic-memory-search-candle-embeddings-with-sqlite-vec]]

## Link cleanup note
- Normalized the stale ADR-054 roadmap/dependency alias to the canonical ADR permalink `[[decisions/adr-054-proposal-memory-artifact-hygiene-and-proactive-knowledge-curation]]`.
- Remaining alias-debt cleanup outside confidently canonical current targets is intentionally left for broader memory-health follow-up work.