---
title: ADR-057 Proposal: FUSE-Mounted Memory — Filesystem as the Primary Agent Interface
type: adr
tags: ["adr","fuse","filesystem","memory","agent-interface","dolt","mcp"]
---



# ADR-057 Proposal: FUSE-Mounted Memory — Filesystem as the Primary Agent Interface

## Status

Proposed

Date: 2026-04-13

Related: [[ADR-055 Proposal: Dolt Migration and Per-Task Knowledge Branching]], [[ADR-054 Proposal: Memory Extraction Quality Gates and Note Taxonomy]], [[ADR-056 Proposal: Planner-Driven Codebase Learning and Memory Hygiene]], [[ADR-023: Cognitive Memory Architecture — Multi-Signal Retrieval and Associative Learning]]

## Context

Djinn currently exposes ~20 MCP tools for memory operations: `memory_write`, `memory_edit`, `memory_read`, `memory_search`, `memory_move`, `memory_delete`, `memory_list`, `memory_catalog`, `memory_recent`, `memory_diff`, `memory_orphans`, `memory_broken_links`, `memory_reindex`, `memory_task_refs`, `memory_history`, `memory_confirm`, `memory_health`, `memory_graph`, `memory_associations`, `memory_build_context`.

Most of these are CRUD operations that duplicate what a filesystem already provides. Every LLM coding agent — Claude Code, Cursor, Windsurf, Copilot — already knows how to `Read`, `Write`, `Edit`, `Grep`, and `Glob` files. Teaching agents a custom MCP API for basic note operations is unnecessary friction. Agents invoke the wrong tool, pass wrong parameters, and waste tokens on tool discovery when they could just read and write files.

Meanwhile, [[ADR-055 Proposal: Dolt Migration and Per-Task Knowledge Branching]] introduces per-task knowledge branches in Dolt. The branch isolation model maps naturally to a filesystem: each branch is a directory, each note is a file, switching branches is changing directories.

### What already exists

Notes in `.djinn/` are already file-backed markdown with YAML frontmatter. The current system reads from disk, indexes into SQLite, and serves via MCP tools. This ADR proposes cutting out the MCP middleman for basic operations and letting the filesystem be the primary interface.

## Problem statement

1. **Large MCP surface area** — 20 tools for memory, most doing simple CRUD that a filesystem handles natively
2. **Agent friction** — agents must learn custom tools instead of using file primitives they already know
3. **Branch visibility** — ADR-055 introduces per-task branches, but without a filesystem mount agents have no natural way to see "their" branch vs main
4. **Tool lock-in** — only MCP-aware agents can use Djinn memory; any agent that can read files could participate

## Decision

Mount Djinn's Dolt-backed knowledge base as a **FUSE virtual filesystem**. Basic memory operations become file operations. MCP tools are reduced to a small set of smart operations that have no filesystem equivalent.

### 1. Mount layout

```
{project}/.djinn/memory/                  # FUSE mount point
├── patterns/
│   ├── use-rrf-fusion-for-multi-signal-ranking.md
│   └── ...
├── pitfalls/
├── cases/
├── decisions/
├── reference/
├── design/
└── research/
```

The mount shows the agent's **current branch view**. When an agent is working on `task_abc123`, its mount reflects that branch — it sees main's notes plus any notes it has written during the task. The branch is determined by the agent's session context, not a directory path.

Notes are standard markdown files with YAML frontmatter for metadata:

```yaml
---
title: Use RRF Fusion for Multi-Signal Ranking
confidence: 0.85
scope_paths: ["server/crates/djinn-db/src/repositories/note"]
tags: ["retrieval", "ranking", "rrf"]
access_count: 12
last_accessed: 2026-04-13T22:00:00Z
---

## Context
...
```

### 2. Write-through semantics

File operations trigger backend updates:

| File operation | Backend effect |
|---------------|----------------|
| **Read** a `.md` file | `touch_accessed()` — increments access_count, updates last_accessed |
| **Write** a new `.md` file | Dolt INSERT + DOLT_COMMIT + Qdrant embedding upsert |
| **Edit** an existing `.md` file | Dolt UPDATE + DOLT_COMMIT + Qdrant re-embed if content changed |
| **Delete** a `.md` file | Dolt DELETE + DOLT_COMMIT + Qdrant vector delete |
| **Move/rename** a file | Dolt UPDATE (permalink, folder, note_type derived from target path) + DOLT_COMMIT |
| **Glob** (`ls`, `find`) | Dolt SELECT on notes table, filtered by branch |
| **Grep** content | Dolt FULLTEXT search or direct content scan |

Frontmatter is parsed on write — agents can set `tags`, `scope_paths`, and other metadata by editing the YAML header. `confidence`, `access_count`, and `last_accessed` are read-only in frontmatter (system-managed).

Write-through is **batched and debounced** — rapid successive writes (common during agent editing) are coalesced into a single Dolt commit after a short quiet period (e.g. 500ms). This prevents commit spam while maintaining durability.

### 3. Branch-aware mounting

Two mounting strategies, depending on agent architecture:

**Option A — Session-scoped branch (preferred):**
The FUSE daemon receives the agent's session/task context. The mount point always shows the correct branch for the active session. When the coordinator dispatches a task, the FUSE layer switches to `task_{task_id}` branch. When the task completes or the agent returns to MCP interaction, it sees `main`.

**Option B — Explicit branch directories:**
```
{project}/.djinn/memory/
├── @main/                    # canonical branch (read-only for task agents)
│   ├── patterns/
│   └── ...
├── @task_abc123/             # task branch (read-write for this task's agent)
│   ├── patterns/
│   └── ...
└── @current -> @task_abc123  # symlink to active branch
```

Option A is cleaner (agents don't need to know about branches), but Option B is more transparent (agents can explicitly read from main while writing to their branch).

### 4. Wikilink resolution

Notes use `[[wikilinks]]` to reference each other. The FUSE layer resolves these for rendering:

- `[[Note Title]]` → resolves to the file path of the matching note
- Broken links are visible as files in a virtual `.broken-links/` directory
- Creating a file that matches a broken link title automatically repairs the link

### 5. MCP tools — what stays

The remaining MCP tools are the **smart operations** that cannot be expressed as file I/O:

| Tool | Why it stays |
|------|-------------|
| `memory_build_context` | Progressive disclosure with token budgets, multi-tier (L0/L1/L2) context assembly, RRF-ranked discovery — not a file read |
| `memory_semantic_search` | Qdrant nearest-neighbor by embedding — not expressible as grep |
| `memory_confirm` | Bayesian confidence signal — a structured action, not a file edit |
| `memory_health` | Aggregate statistics across the entire KB — not a file listing |
| `memory_graph` | Wikilink graph traversal and visualization — structural query |
| `memory_associations` | Hebbian co-access patterns — analytical query |

That's **6 tools** instead of 20. The 14 CRUD tools (`memory_write`, `memory_edit`, `memory_read`, `memory_search`, `memory_move`, `memory_delete`, `memory_list`, `memory_catalog`, `memory_recent`, `memory_diff`, `memory_orphans`, `memory_broken_links`, `memory_reindex`, `memory_task_refs`) are replaced by filesystem operations.

### 6. Embedding pipeline

On file write/edit, the FUSE daemon:
1. Parses frontmatter and content
2. Writes to Dolt (INSERT/UPDATE + COMMIT)
3. Computes embedding via Candle (ADR-053)
4. Upserts vector + metadata payload to Qdrant

This pipeline runs asynchronously after the file write returns — the agent doesn't block on embedding computation. A brief window of eventual consistency (10-50ms) is acceptable since semantic search is not the primary retrieval path for notes the agent just wrote.

### 7. Access tracking

Every `open()` + `read()` on a note file increments `access_count` and updates `last_accessed`. This solves the existing gap where `touch_accessed()` was not wired into MCP read paths (ADR-054 section 5) — the filesystem handles it naturally.

Co-access tracking: when an agent reads multiple notes in a session, the FUSE daemon buffers the read set and flushes co-access pairs to `note_associations` on session end (same as current `flush_co_access()` behavior).

### 8. Implementation: Rust FUSE

Use the `fuser` crate (Rust FUSE implementation) on Linux, with an NFS loopback fallback on macOS.

**Linux — native FUSE:**

```rust
use fuser::{Filesystem, MountOption};

struct DjinnMemoryFS {
    dolt_pool: MySqlPool,       // Dolt connection
    qdrant: QdrantClient,       // Vector search
    branch: String,             // Current branch
    write_buffer: DebouncedWriteBuffer,
}

impl Filesystem for DjinnMemoryFS {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) { ... }
    fn read(&mut self, _req: &Request, ino: u64, ..., reply: ReplyData) { ... }
    fn write(&mut self, _req: &Request, ino: u64, ..., reply: ReplyWrite) { ... }
    fn readdir(&mut self, _req: &Request, ino: u64, ..., reply: ReplyDirectory) { ... }
    // ...
}
```

FUSE is first-class on Linux — the kernel module ships with every distro, no extra dependencies. The `fuser` crate is mature and well-maintained. Mount options: `allow_other` (so agent subprocesses can access), `auto_unmount` (cleanup on crash).

**macOS — NFS loopback mount:**

Apple removed native FUSE support; third-party options (macFUSE, FUSE-T) require kernel extensions or SIP configuration. Instead, serve the same virtual filesystem over NFS on localhost:

```rust
// NFS server on 127.0.0.1:2049 (or unprivileged port)
// Same DjinnMemoryFS logic, different transport
let nfs_server = NfsServer::new(DjinnMemoryFS { ... });
nfs_server.bind("127.0.0.1:2049").await?;

// Auto-mount via:
// mount -t nfs -o port=2049,mountport=2049 127.0.0.1:/memory /path/to/.djinn/memory
```

NFS adds ~0.1-0.5ms latency over FUSE for local mounts — negligible for note-sized files. No kernel extension, no SIP changes, fully supported by macOS out of the box. The `nfsserve` or `vfs` crates provide the NFS server primitives.

**Windows** is not a target platform. Users run Djinn server on WSL, which is Linux and uses native FUSE.

**Platform detection at startup:**

```rust
#[cfg(target_os = "linux")]
fn mount_memory_fs(fs: DjinnMemoryFS, path: &Path) -> Result<()> {
    fuser::mount2(fs, path, &[MountOption::AutoUnmount, MountOption::AllowOther])?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn mount_memory_fs(fs: DjinnMemoryFS, path: &Path) -> Result<()> {
    let server = NfsLoopbackServer::new(fs);
    server.mount(path)?;
    Ok(())
}
```

The `DjinnMemoryFS` implementation is shared — only the transport layer differs between platforms.


## Alternatives considered

### A. Keep all operations as MCP tools
Rejected. 20 tools is too many. Agents already know files. The MCP surface should shrink, not grow.

### B. Filesystem overlay without FUSE (symlinks to Dolt working directory)
Insufficient. Dolt's filesystem representation is not designed for direct access — it's content-addressed chunks, not human-readable files. A translation layer is needed.

### C. WebDAV instead of FUSE
Possible but adds HTTP overhead. FUSE is kernel-level, lower latency, and agents already have native filesystem access. WebDAV would make sense for remote access but not for local agent use.

### D. Keep MCP for writes, filesystem for reads only
Half-measure. The power of this approach is that agents can write notes by creating files — the same workflow they use for code. Read-only FUSE would still require MCP tools for writes.

### E. NFS or 9P instead of FUSE
9P (Plan 9 protocol) is elegant but has poor Linux tooling. NFS adds network stack overhead for a local mount. FUSE is the standard choice for userspace filesystems on Linux and macOS.

## Consequences

### Positive
- MCP surface drops from 20 tools to 6
- Any file-aware agent can interact with Djinn memory (not just MCP-aware ones)
- Branch isolation is visible and intuitive (directory = branch)
- Access tracking happens automatically on file reads
- Agents use skills they already have (Read/Write/Grep/Glob)
- Frontmatter metadata is human-readable and editable
- Write-through keeps Dolt and Qdrant in sync transparently

### Negative
- FUSE adds a userspace daemon and kernel module dependency
- Write-through pipeline (FUSE → Dolt → Qdrant) adds complexity
- Debounced writes introduce a brief consistency window
- macOS FUSE support requires macFUSE (third-party kernel extension) or NFS fallback
- Frontmatter parsing on every write adds overhead
- Agents might write malformed frontmatter (need validation + graceful fallback)
- Testing is harder (need to mock FUSE layer or use integration tests with real mounts)

## Migration / rollout

### Phase 1 — Read-only FUSE/NFS mount
- Linux: FUSE mount of existing file-backed notes (read-only)
- macOS: NFS loopback mount of same virtual filesystem
- Validate that Read/Grep/Glob work correctly over the mount on both platforms
- Wire access tracking into read operations
- Keep all MCP tools active as fallback

### Phase 2 — Write-through
- Enable file writes through mount → Dolt commit pipeline
- Implement frontmatter parsing and validation
- Add debounced write batching
- Wire embedding pipeline (Candle → Qdrant) into write path

### Phase 3 — Branch-aware mounting
- Integrate with ADR-055 per-task branches
- Session-scoped branch switching (Option A) or explicit branch directories (Option B)
- Test with parallel agent sessions on different branches

### Phase 4 — Deprecate CRUD MCP tools
- Migrate agents from `memory_write`/`memory_edit`/`memory_read` to filesystem operations
- Remove deprecated MCP tools
- Update agent prompts to reference filesystem paths instead of MCP tool names


## Relations

- [[ADR-055 Proposal: Dolt Migration and Per-Task Knowledge Branching]]
- [[ADR-054 Proposal: Memory Extraction Quality Gates and Note Taxonomy]]
- [[ADR-056 Proposal: Planner-Driven Codebase Learning and Memory Hygiene]]
- [[ADR-023: Cognitive Memory Architecture — Multi-Signal Retrieval and Associative Learning]]
- [[ADR-053: Semantic Memory Search — Candle Embeddings with sqlite-vec]]