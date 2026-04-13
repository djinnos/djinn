---
title: Memory Sync and Worktree-Aware Knowledge Base
type: 
tags: ["sync","memory","cognitive","worktree"]
---


# Memory Sync and Worktree-Aware Knowledge Base

## Status
Proposed

## Context

[[decisions/adr-023-cognitive-memory-architecture-multi-signal-retrieval-and-associative-learning|ADR-023 Cognitive Memory Architecture Multi-Signal Retrieval and Associative Learning]] introduces per-note cognitive metadata: `access_count`, `last_accessed`, `confidence`, and `note_associations` (Hebbian co-access weights). Today this data lives exclusively in `~/.djinn/djinn.db` and is lost when:

- A colleague clones a project that uses Djinn
- The DB is rebuilt or re-registered
- A developer works on a different machine

Additionally, memory tools (`memory_write`, `memory_edit`, `memory_delete`) currently write `.md` files to the **main checkout's** `.djinn/` directory regardless of whether the agent is operating in a worktree. This means:

- Spike/research tasks write unvalidated notes directly to the canonical knowledge base
- Note changes aren't reviewable on the task branch PR
- Notes can't be discarded with a failed task

[[decisions/djinn-namespace-git-sync|Djinn Namespace Git Sync]] (ADR-007) already reserves the `djinn/memory` branch but has no implementation.

## Decision

### 1. Worktree-Aware Memory Tools

Memory write operations (`memory_write`, `memory_edit`, `memory_delete`) resolve the `.djinn/` target path from the MCP session context, which already carries the worktree path. When an agent is operating in a worktree, writes go to `{worktree}/.djinn/`. When no worktree is active (e.g. human via CLI), writes go to the main checkout.

This is enforced by the coordinator/MCP server — agents do not pass a worktree parameter. The MCP server resolves it internally from session context.

Reads remain canonical (main checkout + DB) unless the note exists on the current worktree branch.

### 2. Cognitive Metrics Sync via `djinn/tasks` Branch

Cognitive metadata syncs on the **same `djinn/tasks` branch** already used for task sync — no separate branch. Per-user JSON file keyed by Clerk email: `{project}/{user_email}.json` (same as tasks, extended with cognitive data).

Per-user cognitive metrics structure (alongside task data):
```json
{
  "exported_at": "2026-03-18T...",
  "note_metrics": {
    "decisions/my-adr": {
      "access_count": 42,
      "last_accessed": "2026-03-18T...",
      "confidence": 0.85
    }
  },
  "associations": [
    {
      "a": "decisions/my-adr",
      "b": "patterns/error-handling",
      "co_access_count": 7
    }
  ]
}
```

Keyed by **permalink** (not UUID) for portability across DB rebuilds. Uses the existing sync loop — same debounce, backoff, auto-import, and `from_sync` loop guard.

### 3. Import Merge Strategy

On import, merge per-user metrics into effective DB values:

| Field | Merge | Rationale |
|-------|-------|-----------|
| `access_count` | Sum across users | Collective usage signal |
| `last_accessed` | Max across users | Most recent access by anyone |
| `confidence` | Weighted average by access_count | Heavier users' signals weigh more |
| `co_access_count` | Sum across users | Collective co-access |
| `weight` | Recompute from merged co_access_count | Hebbian formula applied to merged data |

### 4. Reindex from Disk

The DB is a **derived index** — not the source of truth for notes. On git pull (detected by the KB file watcher), `reindex_from_disk()` rebuilds FTS and note metadata from the `.djinn/` files. Cognitive metrics are rebuilt from the per-user data on the `djinn/tasks` branch during sync import.

A fresh clone bootstraps meaningful signals by importing all users' metric files on first sync.

## Consequences

- Single `djinn/tasks` branch carries both task state and cognitive metadata — no extra sync channel
- Note content syncs via git pull of main branch; watcher reindexes
- No merge conflicts on metrics — each user writes only their own file
- Worktree writes make notes reviewable on task branch PRs
- Spike tasks can write speculative notes without polluting the canonical knowledge base
- DB nuke is recoverable — reindex from disk + sync import restores everything
- `touch_accessed` needs debounced flush to the sync export (not per-read disk writes)

## Relations

- [[ADR-023 Cognitive Memory Architecture Multi-Signal Retrieval and Associative Learning]]
- [[decisions/djinn-namespace-git-sync|Djinn Namespace Git Sync]]
- [[decisions/project-.djinn-directory-—-notes-only,-git-tracked|Project .djinn Directory — Notes Only, Git-Tracked]]