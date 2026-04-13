---
title: ADR-057 Roadmap — FUSE-Mounted Memory
type: design
tags: ["adr-057","roadmap","memory","filesystem","fuse"]
---

# ADR-057 Roadmap — FUSE-Mounted Memory

## Status
Epic `tcet` remains open. The ADR is only a proposal today: the codebase still exposes CRUD-oriented memory MCP tools in `server/crates/djinn-mcp/src/tools/memory_tools/`, and there is no FUSE/NFS mount implementation, branch-aware memory view, or write-through filesystem daemon.

## Current codebase anchors
- Memory note persistence and indexing live under `server/crates/djinn-db/src/repositories/note/`.
- Memory MCP surface lives under `server/crates/djinn-mcp/src/tools/memory_tools/`.
- Embedding runtime currently uses Candle in `server/src/semantic_memory.rs`.
- Server wiring and runtime state live under `server/src/server/` and `server/src/mcp_bridge.rs`.
- Existing note capabilities already cover frontmatter parsing, wikilink graph maintenance, access tracking (`touch_accessed`), and note associations in the SQLite-backed repository.

## Decomposition strategy
Implement ADR-057 in waves, starting with a filesystem-facing seam that reuses the existing note repository instead of trying to replace MCP in one step.

### Wave 1 — Filesystem service foundation
Goal: introduce a reusable virtual-memory-filesystem service that can project notes as files and route file operations into the current repository.

1. Define the mount abstraction and transport-neutral filesystem service.
2. Implement read/list path semantics against `NoteRepository`, including access tracking parity.
3. Implement write/update/delete/rename translation with frontmatter parsing and repository-backed index updates.
4. Add Linux mount transport and startup/config plumbing behind a disabled-by-default feature flag.

### Wave 2 — Behavior parity and hardening
Goal: make the filesystem surface trustworthy enough to stand beside MCP.

- Broken-link and wikilink visibility behavior
- Debounced write batching and durability semantics
- Integration tests for read/write/list/rename/delete flows
- Operational health and mount lifecycle reporting

### Wave 3 — Branch-aware mounting and MCP contraction
Goal: connect the filesystem view to task/session context and begin shrinking CRUD MCP usage.

- Session-aware branch switching or explicit branch directories
- macOS fallback transport decision/implementation
- Agent prompt and server contract updates to prefer filesystem operations for CRUD
- Deprecation plan for CRUD-oriented memory MCP tools while retaining smart analytical tools

## Wave 1 task map
1. **Design the virtual filesystem core API** around the existing note repository and note path conventions.
2. **Implement read/list/stat path projection** from repository notes to virtual filesystem entries, preserving access tracking.
3. **Implement write-through mutation translation** for create/edit/delete/rename, reusing note CRUD/frontmatter/indexing logic.
4. **Integrate Linux FUSE mount plumbing** behind configuration/feature gates with startup validation and documentation.

## Notes for workers
- Reuse existing note repository behavior instead of creating a second note model.
- Preserve current `memory_read` semantics by ensuring filesystem reads also trigger access tracking.
- Treat the mount transport as an adapter over a transport-neutral in-memory service so NFS/macOS fallback can be added later without redoing note semantics.
- Keep MCP analytical tools (`memory_build_context`, `memory_health`, `memory_graph`, `memory_associations`, `memory_confirm`) intact during this wave.

## Relations
- [[decisions/adr-057-proposal-fuse-mounted-memory-filesystem-as-the-primary-agent-interface]]
- [[decisions/adr-055-proposal-dolt-migration-and-per-task-knowledge-branching]]
- [[decisions/adr-054-proposal-memory-extraction-quality-gates-and-note-taxonomy]]
- [[decisions/adr-053-semantic-memory-search-candle-embeddings-with-sqlite-vec]]